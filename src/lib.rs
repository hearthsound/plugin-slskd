//! slskd acquisition plugin: a Hearthsound acquisition provider over an slskd
//! instance.
//!
//! The plugin drives one [slskd](https://github.com/slskd/slskd) instance, which
//! owns the Soulseek connection, the peer upload queue and the download
//! directory. It implements `lifecycle` (configuration validation and a health
//! probe) and `acquisition` (search, candidate download and download
//! monitoring) at track and album granularity, and nothing else. An album query
//! answers with one candidate per peer folder, holding the folder's audio
//! files, so a whole album is downloaded from one peer directory.
//!
//! The plugin needs slskd 0.26.0 or newer, because it downloads through the
//! batch transfer API that release introduced, and it needs an API key.
//!
//! The pure logic sits in public modules ([`config`], [`random`], [`search`],
//! [`slskd`], [`tokens`], [`url`], [`version`]); the binding-coupled glue that
//! drives it - the HTTP transport and the acquisition operations - stays
//! private.
//!
//! # State between calls
//!
//! A plugin instance is recycled after every call, so the widening progress of
//! a search is kept in the instance's host-kv store under a key the search
//! token names. Candidate and download tokens carry their own state, because
//! they are small and the server may hold them across restarts.
//!
//! # Tests
//!
//! The pure logic is unit-tested on the host target:
//!
//! ```text
//! cargo test
//! ```
//!
//! Rustdoc examples in those modules are illustrative `text` blocks rather than
//! doctests: the crate is a `cdylib` for `wasm32-wasip2`, so there is no host
//! library target for a doctest to link against. Every example appears as an
//! assertion in the unit tests of the module that carries it.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use hearthsound_plugin::acquisition_provider::exports::hearthsound::plugin::acquisition::{
    Capabilities, DownloadState, Granularity, Guest as AcquisitionGuest, OutputFile, Progress,
    Query, SearchState,
};
use hearthsound_plugin::acquisition_provider::exports::hearthsound::plugin::lifecycle::Guest as LifecycleGuest;
use hearthsound_plugin::host;
use hearthsound_plugin::http;
use hearthsound_plugin::kv;
use hearthsound_plugin::types::ProviderError;

mod api;
pub mod config;
pub mod download;
pub mod random;
pub mod search;
pub mod slskd;
pub mod states;
pub mod tokens;
pub mod url;
pub mod version;

/// Prefix of the host-kv keys that hold the progress of one search.
const SEARCH_KEY_PREFIX: &str = "search:";

/// How long a search's progress stays readable, in milliseconds.
///
/// An hour is far longer than a search can take, and it bounds how long the
/// progress of a search the server stopped polling occupies the store.
const SEARCH_TTL_MS: u64 = 3_600_000;

/// API path that enqueues a batch of downloads.
const BATCH_PATH: &str = "/api/v0/transfers/downloads/batches";

/// The failure `poll-download` reports when slskd lost a download.
const LOST_DOWNLOAD: &str = "slskd lost the download and its files are not at their pinned paths";

/// The plugin implementation.
struct Slskd;

/// Validates a server-scoped configuration document.
///
/// Runs the schema-level checks of [`config::check`] and, when they pass, asks
/// the configured instance for its version, because the plugin only works with
/// slskd 0.26.0 or newer. The host calls this before it stores a configuration,
/// so a problem here is a problem an administrator has to fix.
///
/// # Parameters
///
/// - `server_config`: the JSON document the administrator entered, with the
///   schema defaults the admin UI filled in.
///
/// # Returns
///
/// `Ok(())` when the document is usable and the instance answers with a
/// supported version.
///
/// # Errors
///
/// Returns one message per problem: a missing or malformed field, an instance
/// that cannot be reached or authenticated with, or an instance whose version
/// is older than [`version::MINIMUM_SUPPORTED`].
pub fn validate_config(server_config: &str) -> Result<(), Vec<String>> {
    let problems = config::check(server_config);
    if !problems.is_empty() {
        return Err(problems);
    }

    let config = config::Config::from_json(server_config).map_err(|message| vec![message])?;

    match probe(&config) {
        Probe::Supported(version) => {
            hearthsound_plugin::info!("slskd {version} at {}", config.base_url());
            Ok(())
        }
        Probe::TooOld(version) => Err(vec![too_old(&config.base_url(), &version)]),
        Probe::Failed(error) => Err(vec![probe_problem(&config.base_url(), &error)]),
    }
}

/// Probes the configured slskd instance.
///
/// # Returns
///
/// The version string the instance reports.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidConfig`] when this instance's configuration
/// does not pass [`config::check`], the mapped error of a failed request, and
/// [`ProviderError::Unavailable`] when the instance is older than
/// [`version::MINIMUM_SUPPORTED`].
pub fn health() -> Result<String, ProviderError> {
    let config = configured()?;

    match probe(&config) {
        Probe::Supported(version) => Ok(version),
        Probe::TooOld(version) => Err(ProviderError::Unavailable(too_old(
            &config.base_url(),
            &version,
        ))),
        Probe::Failed(error) => Err(error),
    }
}

/// Implements the lifecycle interface.
impl LifecycleGuest for Slskd {
    fn validate_config(server_config: String) -> Result<(), Vec<String>> {
        validate_config(&server_config)
    }

    fn health() -> Result<String, ProviderError> {
        health()
    }
}

/// Implements the acquisition interface.
impl AcquisitionGuest for Slskd {
    fn get_capabilities() -> Capabilities {
        Capabilities {
            granularities: vec![Granularity::Track, Granularity::Album],
            progressive: false,
        }
    }

    fn start_search(query: Query) -> Result<String, ProviderError> {
        let config = configured()?;

        let (kind, subject, plan) = match query.granularity {
            Granularity::Track => {
                let wanted = query.tracks.first().ok_or_else(|| {
                    ProviderError::Internal("the query carries no wanted track".to_string())
                })?;
                let artist = wanted.artists.first().map_or("", String::as_str);

                (
                    search::Kind::Track,
                    wanted.title.clone(),
                    search::plan(&wanted.title, artist),
                )
            }
            Granularity::Album => {
                let album = query.album.as_deref().unwrap_or_default();
                let album_artist = query.album_artist.as_deref().unwrap_or_default();

                (
                    search::Kind::Album,
                    album.to_string(),
                    search::album_plan(album, album_artist),
                )
            }
        };

        if plan.is_empty() {
            let problem = match kind {
                search::Kind::Track => "the wanted track has no usable title",
                search::Kind::Album => "the wanted album has no usable title",
            };

            return Err(ProviderError::Internal(problem.to_string()));
        }

        let key = random::key();
        let token = tokens::encode(&tokens::SearchToken { key: key.clone() })?;
        let mut progress = search::Progress::new(plan, kind);
        let variant = progress.variant().cloned();

        hearthsound_plugin::info!(
            "searching slskd for `{}` in {} variants",
            subject,
            progress.plan.len()
        );

        if let Some(variant) = variant {
            start_variant(&client(&config), &config, &key, &mut progress, &variant)?;
        }

        Ok(token)
    }

    fn poll_search(search_token: String) -> Result<SearchState, ProviderError> {
        let config = configured()?;
        let token: tokens::SearchToken = tokens::decode(&search_token)?;
        let client = client(&config);
        let mut progress = load_progress(&token.key)?;

        let Some(variant) = progress.variant().cloned() else {
            return Ok(SearchState::Complete(Vec::new()));
        };

        let Some(search_id) = progress.search_id.clone() else {
            start_variant(&client, &config, &token.key, &mut progress, &variant)?;

            return Ok(SearchState::Running(Vec::new()));
        };

        let search: slskd::Search = client.get_json(&format!("/api/v0/searches/{search_id}"))?;
        if !search.is_complete {
            return Ok(SearchState::Running(Vec::new()));
        }

        let responses: Vec<slskd::SearchResponse> =
            client.get_json(&format!("/api/v0/searches/{search_id}/responses"))?;
        let filter = search::Filter::from_config(&config);
        let found = match progress.kind {
            search::Kind::Track => search::candidates(
                &search_id,
                &responses,
                &filter,
                variant.require_artist.as_deref(),
            )?,
            search::Kind::Album => search::album_candidates(
                &search_id,
                &responses,
                &filter,
                variant.require_artist.as_deref(),
            )?,
        };

        hearthsound_plugin::info!(
            "slskd search {search_id} (`{}`) returned {} responses and {} candidates",
            variant.text,
            responses.len(),
            found.len()
        );

        // The progress is written before the search is deleted, so a failure in
        // between leaves a search to clean up rather than a progress that
        // points at a search slskd no longer has.
        let next = progress.advance(!found.is_empty()).cloned();
        store_progress(&token.key, &progress)?;
        clear_search(&client, &search_id);

        match next {
            Some(next) => {
                start_variant(&client, &config, &token.key, &mut progress, &next)?;

                Ok(SearchState::Running(Vec::new()))
            }
            None => Ok(SearchState::Complete(found)),
        }
    }

    fn cancel_search(search_token: String) {
        let Ok(token) = tokens::decode::<tokens::SearchToken>(&search_token) else {
            hearthsound_plugin::warn!("cannot cancel a search from an unreadable token");

            return;
        };

        if let Ok(progress) = load_progress(&token.key)
            && let Ok(config) = configured()
            && let Some(search_id) = progress.search_id
        {
            cancel_request(&client(&config), &search_id);
        }

        clear_progress(&token.key);
    }

    fn start_download(candidate_token: String, files: Vec<u32>) -> Result<String, ProviderError> {
        let config = configured()?;
        let candidate: tokens::CandidateToken = tokens::decode(&candidate_token)?;
        let selected = download::select(&candidate.files, &files)?;

        if selected.is_empty() {
            return Err(ProviderError::Internal(
                "the candidate offers no file to download".to_string(),
            ));
        }

        let key = random::key();
        let client = client(&config);

        let batches = match enqueue_batches(&client, &candidate.username, &key, &selected) {
            Ok(batches) => batches,
            Err(failure) => {
                // Nothing keeps downloading behind the caller's back: the
                // batches that were already enqueued are cancelled, because the
                // server never sees their ids and so could never cancel them.
                hearthsound_plugin::warn!(
                    "cancelled {} batch(es) after a failed enqueue",
                    failure.cancelled
                );

                return Err(failure.error);
            }
        };

        hearthsound_plugin::info!(
            "downloading {} file(s) from {} into {} in {} batch(es)",
            selected.len(),
            candidate.username,
            download::destination(&key),
            batches.len()
        );

        tokens::encode(&tokens::DownloadToken {
            key,
            username: candidate.username,
            batches,
        })
    }

    fn poll_download(download_token: String) -> Result<DownloadState, ProviderError> {
        let config = configured()?;
        let token: tokens::DownloadToken = tokens::decode(&download_token)?;
        let client = client(&config);
        let size = |path: &str| size_in(&config.import_root, path);

        match poll(&client, &token, &config.import_root, size)? {
            Polled::State(state) => {
                if let DownloadState::Failed(message) = &state {
                    hearthsound_plugin::warn!("download {} failed: {message}", token.key);
                }

                Ok(state)
            }
            Polled::Lost => {
                // slskd cannot be asked about the download any more and its
                // files are not where the plugin put them: the download is
                // lost. The transfers of its batches are cancelled first, so
                // nothing keeps downloading for a candidate that has ended.
                hearthsound_plugin::warn!(
                    "slskd lost download {} and its files are not at their pinned paths",
                    token.key
                );

                Ok(DownloadState::Failed(LOST_DOWNLOAD.to_string()))
            }
            Polled::Complete(files) => {
                hearthsound_plugin::info!(
                    "download {} from {} produced {} file(s)",
                    token.key,
                    token.username,
                    files.len()
                );

                Ok(DownloadState::Complete(files))
            }
        }
    }

    fn cancel_download(download_token: String) {
        let Ok(token) = tokens::decode::<tokens::DownloadToken>(&download_token) else {
            hearthsound_plugin::warn!("cannot cancel a download from an unreadable token");

            return;
        };

        // Cancellation is best effort: the interface has no way to report a
        // failure and the acquisition is ending either way, so a plugin that
        // cannot reach its backend says so in the log and leaves the transfers
        // to the instance's own queue management.
        let Ok(config) = configured() else {
            hearthsound_plugin::warn!(
                "cannot cancel download {}: the instance configuration could not be read",
                token.key
            );

            return;
        };

        cancel(&client(&config), &token);
    }
}

hearthsound_plugin::export_acquisition_provider!(Slskd);

/// Starts the slskd search for one variant and records it in the progress.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `config`: the instance configuration, which supplies the search limits.
/// - `key`: the search token's key, under which the progress is stored.
/// - `progress`: the search's progress, updated with the new search id.
/// - `variant`: the variant to search for.
///
/// # Errors
///
/// Returns the mapped error of a failed request, and
/// [`ProviderError::Internal`] when the progress cannot be stored.
fn start_variant(
    client: &api::Client,
    config: &config::Config,
    key: &str,
    progress: &mut search::Progress,
    variant: &search::Variant,
) -> Result<(), ProviderError> {
    let id = random::guid();
    let request = slskd::SearchRequest {
        id: id.clone(),
        search_text: variant.text.clone(),
        search_timeout: config.search_timeout_ms,
        response_limit: config.response_limit,
        file_limit: config.file_limit,
        filter_responses: true,
        minimum_peer_upload_speed: config.min_upload_speed_bps(),
        maximum_peer_queue_length: config.max_peer_queue_length,
    };

    let _: slskd::Search = client.post_json("/api/v0/searches", &request)?;

    hearthsound_plugin::debug!("started slskd search {id} for `{}`", variant.text);

    progress.started(id);
    store_progress(key, progress)
}

/// Enqueues one batch of a download.
///
/// A partly accepted batch is cancelled before the error, so nothing downloads
/// without a token: slskd answers with a success status and a failure list, and
/// the files it did accept start transferring at once.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the files come from.
/// - `request`: the batch to enqueue.
///
/// # Errors
///
/// Returns the mapped error of a failed request, and
/// [`ProviderError::Internal`] when slskd answered with a status the plugin
/// cannot use or with failures for the files it enqueued. The error stays
/// internal even when slskd refused every file: its per-file messages name
/// neither the peer nor its queue reliably, so the plugin cannot claim the
/// backend is unavailable, and the caller treats either error as a failed
/// candidate.
fn enqueue<A: api::Transport>(
    client: &api::Client<A>,
    username: &str,
    request: &slskd::EnqueueBatchRequest,
) -> Result<(), ProviderError> {
    let response = client.post(BATCH_PATH, request)?;

    if !http::is_success(&response) {
        return Err(enqueue_problem(username, response.status, &response.body));
    }

    let enqueued: slskd::EnqueueBatchResponse = api::decode(&response)?;

    if !enqueued.failures.is_empty() {
        cancel_batch(client, username, &request.id);

        return Err(failures_problem(&enqueued.failures));
    }

    Ok(())
}

/// Why a download could not be enqueued, and what was undone for it.
struct EnqueueFailure {
    /// The error the caller returns.
    error: ProviderError,
    /// How many files of the batches that were already enqueued were
    /// cancelled.
    cancelled: usize,
}

/// Enqueues every batch of one download.
///
/// One batch per remote directory: a batch is downloaded into one destination
/// directory, so its files have to be the files of one remote directory.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the files come from.
/// - `key`: the download's random key, which names the destination directory.
/// - `selected`: the files to download, grouped by their remote directory.
///
/// # Returns
///
/// One entry per enqueued batch, holding its id and the files it is expected to
/// produce.
///
/// # Errors
///
/// Returns the error of the batch that failed, after cancelling the batches
/// that were already enqueued: the caller never receives a token for this
/// download, so it could never cancel them, and they would keep downloading.
fn enqueue_batches<A: api::Transport>(
    client: &api::Client<A>,
    username: &str,
    key: &str,
    selected: &[tokens::RemoteFile],
) -> Result<Vec<tokens::BatchRef>, EnqueueFailure> {
    let destination = download::destination(key);
    let mut batches: Vec<tokens::BatchRef> = Vec::new();

    for group in download::group_by_directory(selected) {
        let id = random::guid();
        let request = slskd::EnqueueBatchRequest {
            id: id.clone(),
            username: username.to_string(),
            files: group
                .files
                .iter()
                .map(|file| slskd::BatchFile {
                    filename: file.filename.clone(),
                    size: file.size,
                })
                .collect(),
            options: slskd::BatchOptions {
                destination: destination.clone(),
            },
        };

        if let Err(error) = enqueue(client, username, &request) {
            return Err(EnqueueFailure {
                error,
                cancelled: cancel_batches(client, username, &batches),
            });
        }

        batches.push(tokens::BatchRef {
            id,
            files: download::expected(&group.directory, &group.files),
        });
    }

    Ok(batches)
}

/// Maps a failed batch enqueue to the provider error the server sees.
///
/// # Parameters
///
/// - `username`: the peer the batch was for.
/// - `status`: the status slskd answered with.
/// - `body`: the response body, used as the message.
///
/// # Returns
///
/// Unauthorized for a rejected key, unavailable for a peer slskd reports as
/// offline (`404`) and for a throttled request (`429`), and internal for every
/// other status, which means the plugin sent something slskd would not accept.
fn enqueue_problem(username: &str, status: u16, body: &[u8]) -> ProviderError {
    let message = String::from_utf8_lossy(body);
    let message = message.trim();

    match status {
        401 | 403 => ProviderError::Unauthorized(format!("slskd rejected the API key: {message}")),
        404 => ProviderError::Unavailable(format!("slskd reports peer {username} as offline")),
        429 => ProviderError::Unavailable("slskd is throttling transfers".to_string()),
        _ => ProviderError::Internal(format!(
            "slskd refused the batch for {username}: HTTP {status}: {message}"
        )),
    }
}

/// Renders the failures slskd reported for one batch.
///
/// # Parameters
///
/// - `failures`: the failures of the enqueue response.
///
/// # Returns
///
/// The provider error, naming the first few files and how many there are.
fn failures_problem(failures: &[slskd::Failure]) -> ProviderError {
    let mut details: Vec<String> = failures
        .iter()
        .take(3)
        .map(|failure| format!("`{}`: {}", failure.filename, failure.message))
        .collect();

    if failures.len() > details.len() {
        details.push(format!("and {} more", failures.len() - details.len()));
    }

    ProviderError::Internal(format!(
        "slskd refused {} file(s): {}",
        failures.len(),
        details.join("; ")
    ))
}

/// Reads the transfers of one batch.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `id`: id of the slskd batch.
///
/// # Returns
///
/// The transfers of the batch, and none when slskd does not know the batch or
/// the request failed, which the callers of this function treat as "nothing to
/// do".
fn transfers<A: api::Transport>(client: &api::Client<A>, id: &str) -> Vec<slskd::Transfer> {
    match client.get_json::<slskd::Batch>(&batch_path(id)) {
        Ok(batch) => batch.transfers,
        Err(error) => {
            hearthsound_plugin::warn!("could not read slskd batch {id}: {error:?}");

            Vec::new()
        }
    }
}

/// Cancels one batch: clears its transfers and deletes their files.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the batch's files come from.
/// - `id`: id of the slskd batch.
///
/// # Returns
///
/// How many transfers were cleared.
fn cancel_batch<A: api::Transport>(client: &api::Client<A>, username: &str, id: &str) -> usize {
    let ids: Vec<String> = transfers(client, id)
        .into_iter()
        .map(|transfer| transfer.id)
        .collect();

    clear_transfers(client, username, &ids, true);

    ids.len()
}

/// Cancels every batch of a download.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the batches' files come from.
/// - `batches`: the batches to cancel.
///
/// # Returns
///
/// How many transfers were cleared.
fn cancel_batches<A: api::Transport>(
    client: &api::Client<A>,
    username: &str,
    batches: &[tokens::BatchRef],
) -> usize {
    batches
        .iter()
        .map(|batch| cancel_batch(client, username, &batch.id))
        .sum()
}

/// Cancels a download: clears every transfer of every batch and deletes their
/// files.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `token`: the download's token.
fn cancel<A: api::Transport>(client: &api::Client<A>, token: &tokens::DownloadToken) {
    cancel_batches(client, &token.username, &token.batches);
}

/// Clears one transfer from slskd's records.
///
/// `remove_files` decides what slskd does with the file behind the record:
///
/// - `false` after a successful import, which is doc 03's post-import step: the
///   host either moved the file out of the download directory or left it there
///   for the import root to keep, so deleting it would destroy a file the host
///   is about to use (a `copy` or `link` import may still be reading it).
/// - `true` when a download is cancelled, which is what doc 06 requires ("a
///   cancelled acquisition leaves no partial file"): the only files this
///   deletes are the ones under the plugin's own `hearthsound/<key>/`
///   destination, which no other part of the system writes.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the transfer belongs to, which its route needs.
/// - `id`: id of the transfer.
/// - `remove_files`: whether slskd also deletes the file the transfer wrote.
fn clear_transfer<A: api::Transport>(
    client: &api::Client<A>,
    username: &str,
    id: &str,
    remove_files: bool,
) {
    let path = format!(
        "/api/v0/transfers/downloads/{}/{}?remove={remove_files}",
        url::encode_segment(username),
        id
    );

    match client.status("DELETE", &path) {
        Ok(200 | 204 | 404) => {}
        Ok(status) => {
            hearthsound_plugin::warn!("could not clear slskd transfer {id}: HTTP {status}")
        }
        Err(error) => hearthsound_plugin::warn!("could not clear slskd transfer {id}: {error:?}"),
    }
}

/// Clears a finished download's transfers from slskd.
///
/// slskd keeps every transfer record it is not told to forget, so the plugin
/// clears the ones it reported. The flag is `remove_files = false`: the files
/// themselves stay where slskd put them for the host to import.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `token`: the download's token.
/// - `transfers`: ids of the transfers to clear.
fn complete<A: api::Transport>(
    client: &api::Client<A>,
    token: &tokens::DownloadToken,
    transfers: &[String],
) {
    clear_transfers(client, &token.username, transfers, false);
}

/// Clears the given transfers from slskd's records, ignoring failures.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the transfers belong to.
/// - `ids`: ids of the transfers.
/// - `remove_files`: whether slskd also deletes the files the transfers wrote.
fn clear_transfers<A: api::Transport>(
    client: &api::Client<A>,
    username: &str,
    ids: &[String],
    remove_files: bool,
) {
    for id in ids {
        clear_transfer(client, username, id, remove_files);
    }
}

/// Returns whether every expected file is present at one of its paths.
///
/// # Parameters
///
/// - `key`: the download's key, which names the destination directory.
/// - `expected`: the files the download is expected to produce.
/// - `size`: the size of the regular file at one path under the import root.
///
/// # Returns
///
/// `true` when every file is a regular file of exactly the expected size. A
/// file that is still being written, renamed or missing makes this `false`.
fn files_present<S>(key: &str, expected: &[tokens::ExpectedFile], size: S) -> bool
where
    S: Fn(&str) -> Option<u64>,
{
    expected
        .iter()
        .all(|file| located(key, file, &size).is_some())
}

/// Finds one downloaded file under its download's destination directory.
///
/// slskd downloads a batch into one destination directory and either flattens
/// its files to their names or keeps their remote subfolders. The plugin
/// accepts either layout, so both paths are tried and the one whose size
/// matches is reported.
///
/// # Parameters
///
/// - `key`: the download's key, which names the destination directory.
/// - `file`: the file to find.
/// - `size`: the size of the regular file at one path under the import root.
///
/// # Returns
///
/// The path of the file, relative to the import root, or `None` when it is at
/// neither of its paths or has the wrong size.
fn located<S>(key: &str, file: &tokens::ExpectedFile, size: S) -> Option<String>
where
    S: Fn(&str) -> Option<u64>,
{
    let flattened = download::pinned_path(key, &file.name);

    if size(&flattened) == Some(file.size) {
        return Some(flattened);
    }

    if file.relative.is_empty() || file.relative == file.name {
        return None;
    }

    let nested = download::pinned_path(key, &file.relative);
    (size(&nested) == Some(file.size)).then_some(nested)
}

/// Returns the size of the regular file at one path under an import root.
///
/// # Parameters
///
/// - `root`: the import root id.
/// - `path`: the path, relative to the root.
///
/// # Returns
///
/// The size in bytes, and `None` for a directory or a path the host does not
/// have.
fn size_in(root: &str, path: &str) -> Option<u64> {
    match host::files::stat(root, path) {
        Some(stat) if !stat.is_dir => Some(stat.size),
        _ => None,
    }
}

/// Builds the output files of a completed download.
///
/// # Parameters
///
/// - `root`: the import root id.
/// - `key`: the download's key.
/// - `expected`: the files the download produced.
/// - `size`: the size of the regular file at one path under the import root.
///
/// # Returns
///
/// One output file per expected file, naming the import root and the path the
/// file was found at. The completion check has just passed, so every file is at
/// one of its paths; a file that vanished in between is reported at its
/// flattened path and the host fails to import it.
fn output_files<S>(
    root: &str,
    key: &str,
    expected: &[tokens::ExpectedFile],
    size: S,
) -> Vec<OutputFile>
where
    S: Fn(&str) -> Option<u64>,
{
    expected
        .iter()
        .map(|file| OutputFile {
            root: root.to_string(),
            path: located(key, file, &size)
                .unwrap_or_else(|| download::pinned_path(key, &file.name)),
            track_index: None,
        })
        .collect()
}

/// What one poll of a download found.
enum Polled {
    /// The download has not finished; the state to report as it is.
    State(DownloadState),
    /// slskd lost the download and its files are not at their paths: its
    /// batches have been cancelled and the download failed.
    Lost,
    /// Every expected file is at its path; these are the files it produced.
    Complete(Vec<OutputFile>),
}

/// Polls one download and decides what it is.
///
/// A batch slskd no longer knows contributes no transfers, which leaves its
/// files unaccounted for and makes the assessment unverifiable.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `token`: the download's token.
/// - `root`: the import root id, which the output files name.
/// - `size`: the size of the regular file at one path under the import root.
///
/// # Returns
///
/// The [`Polled`] outcome.
///
/// # Errors
///
/// Returns the mapped error of a batch request that failed for a reason other
/// than slskd not knowing the batch.
fn poll<A: api::Transport, S>(
    client: &api::Client<A>,
    token: &tokens::DownloadToken,
    root: &str,
    size: S,
) -> Result<Polled, ProviderError>
where
    S: Fn(&str) -> Option<u64>,
{
    let expected: Vec<tokens::ExpectedFile> = token
        .batches
        .iter()
        .flat_map(|batch| batch.files.iter().cloned())
        .collect();

    let mut observed = Vec::new();
    let mut transfers = Vec::new();

    for batch in &token.batches {
        match client.get_json::<slskd::Batch>(&batch_path(&batch.id)) {
            Ok(batch) => {
                transfers.extend(batch.transfers.iter().map(|transfer| transfer.id.clone()));
                observed.extend(batch.transfers.iter().map(states::Observed::from));
            }
            Err(ProviderError::NotFound) => {}
            Err(error) => return Err(error),
        }
    }

    let outcome = match states::assess(&expected, &observed) {
        states::Assessment::Failed(message) => Polled::State(DownloadState::Failed(message)),
        states::Assessment::Queued(place) => Polled::State(DownloadState::Queued(place)),
        states::Assessment::Running(progress) => {
            Polled::State(DownloadState::Running(wit_progress(progress)))
        }
        states::Assessment::AwaitingFiles(progress) => {
            if !files_present(&token.key, &expected, &size) {
                // slskd signals a completed transfer before it moves the file
                // out of its incomplete directory, so the file is only believed
                // once it is at one of its paths.
                return Ok(Polled::State(DownloadState::Running(wit_progress(
                    progress,
                ))));
            }

            complete(client, token, &transfers);

            Polled::Complete(output_files(root, &token.key, &expected, &size))
        }
        states::Assessment::Unverifiable => {
            if !files_present(&token.key, &expected, &size) {
                cancel_batches(client, &token.username, &token.batches);

                return Ok(Polled::Lost);
            }

            complete(client, token, &transfers);

            Polled::Complete(output_files(root, &token.key, &expected, &size))
        }
    };

    Ok(outcome)
}

/// Converts the plugin's progress into the interface's record.
///
/// # Parameters
///
/// - `progress`: the summed progress of a download.
///
/// # Returns
///
/// The progress record `poll-download` reports.
fn wit_progress(progress: states::Progress) -> Progress {
    Progress {
        bytes: progress.bytes,
        total_bytes: Some(progress.total_bytes),
        speed_bps: Some(progress.speed_bps),
    }
}

/// Returns the API path of one download batch.
///
/// # Parameters
///
/// - `id`: id of the slskd batch.
///
/// # Returns
///
/// The path, which carries no trailing slash because slskd's route does not.
fn batch_path(id: &str) -> String {
    format!("/api/v0/transfers/downloads/batches/{id}")
}

/// Cancels a running slskd search and lets go of its record.
///
/// slskd keeps every search it is not told to forget, so the plugin both
/// cancels the search (which stops it) and deletes it (which removes the row).
/// A failure is logged and ignored: the search is over from the plugin's point
/// of view either way.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `id`: id of the slskd search.
fn cancel_request(client: &api::Client, id: &str) {
    match client.status("PUT", &format!("/api/v0/searches/{id}")) {
        Ok(200 | 204 | 404) => {}
        Ok(status) => {
            hearthsound_plugin::warn!("could not cancel slskd search {id}: HTTP {status}")
        }
        Err(error) => hearthsound_plugin::warn!("could not cancel slskd search {id}: {error:?}"),
    }

    clear_search(client, id);
}

/// Deletes a finished slskd search, logging and ignoring a failure.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `id`: id of the slskd search.
fn clear_search(client: &api::Client, id: &str) {
    match client.status("DELETE", &format!("/api/v0/searches/{id}")) {
        Ok(200 | 204 | 404) => {}
        Ok(status) => {
            hearthsound_plugin::warn!("could not delete slskd search {id}: HTTP {status}")
        }
        Err(error) => hearthsound_plugin::warn!("could not delete slskd search {id}: {error:?}"),
    }
}

/// Reads the progress of one search from the instance's store.
///
/// # Parameters
///
/// - `key`: the search token's key.
///
/// # Returns
///
/// The progress of that search.
///
/// # Errors
///
/// Returns [`ProviderError::NotFound`] when the store has no live entry for the
/// key, which means the search token is unknown to this instance or has
/// expired.
fn load_progress(key: &str) -> Result<search::Progress, ProviderError> {
    kv::get_json(&progress_key(key))?.ok_or(ProviderError::NotFound)
}

/// Writes the progress of one search to the instance's store.
///
/// # Parameters
///
/// - `key`: the search token's key.
/// - `progress`: the progress to store.
///
/// # Errors
///
/// Returns [`ProviderError::Internal`] when the value cannot be serialized or
/// the host rejects the write.
fn store_progress(key: &str, progress: &search::Progress) -> Result<(), ProviderError> {
    kv::set_json(&progress_key(key), progress, Some(SEARCH_TTL_MS))
}

/// Removes the progress of one search from the instance's store.
///
/// # Parameters
///
/// - `key`: the search token's key.
fn clear_progress(key: &str) {
    host::kv::delete(&progress_key(key));
}

/// Returns the host-kv key that holds one search's progress.
///
/// # Parameters
///
/// - `key`: the search token's key.
///
/// # Returns
///
/// The store key, prefixed so a search's entry cannot collide with an entry
/// another part of the plugin writes.
fn progress_key(key: &str) -> String {
    format!("{SEARCH_KEY_PREFIX}{key}")
}

/// Builds the slskd client of one instance.
///
/// # Parameters
///
/// - `config`: the instance configuration.
///
/// # Returns
///
/// A client for the configured URL and key.
fn client(config: &config::Config) -> api::Client {
    api::Client::new(&config.base_url(), &config.api_key)
}

/// The outcome of a version probe against one configured instance.
enum Probe {
    /// The instance answered with a version the plugin works with.
    Supported(String),
    /// The instance answered with a version that is too old.
    TooOld(String),
    /// The instance could not be asked, or answered with a failure status.
    Failed(ProviderError),
}

/// Asks an instance for its version and compares it with the minimum.
///
/// # Parameters
///
/// - `config`: the instance configuration.
///
/// # Returns
///
/// The [`Probe`] outcome, which distinguishes a reachable instance from one the
/// plugin cannot talk to.
fn probe(config: &config::Config) -> Probe {
    match api::Client::new(&config.base_url(), &config.api_key).version() {
        Ok(version) if version::is_supported(&version) => Probe::Supported(version),
        Ok(version) => Probe::TooOld(version),
        Err(error) => Probe::Failed(error),
    }
}

/// Reads this instance's configuration through the host.
///
/// # Returns
///
/// The typed configuration of the instance the call belongs to.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidConfig`] when the document the host stored
/// does not pass [`config::check`] or does not deserialize.
fn configured() -> Result<config::Config, ProviderError> {
    let raw = host::config::server_config();
    let problems = config::check(&raw);

    if !problems.is_empty() {
        return Err(ProviderError::InvalidConfig(problems.join("; ")));
    }

    config::Config::from_json(&raw).map_err(ProviderError::InvalidConfig)
}

/// Renders an instance that reports a version the plugin cannot use.
///
/// # Parameters
///
/// - `url`: the instance's base URL.
/// - `version`: the version the instance reported.
///
/// # Returns
///
/// The message used in a validation problem and in a health error.
fn too_old(url: &str, version: &str) -> String {
    format!(
        "{url} reports slskd {version}; version {} or newer is required",
        version::MINIMUM_SUPPORTED
    )
}

/// Renders a failed probe as a validation problem.
///
/// # Parameters
///
/// - `url`: the instance's base URL.
/// - `error`: the error the probe failed with.
///
/// # Returns
///
/// The message used in a validation problem.
fn probe_problem(url: &str, error: &ProviderError) -> String {
    match error {
        ProviderError::Unauthorized(message) => format!("{url} rejected the API key: {message}"),
        ProviderError::NotFound => {
            format!("{url} does not answer the plugin API; is it an slskd instance?")
        }
        ProviderError::RateLimited(ms) => format!("{url} is rate limiting; retry in {ms} ms"),
        ProviderError::Unavailable(message) => format!("{url} is unreachable: {message}"),
        ProviderError::InvalidConfig(message) | ProviderError::Internal(message) => {
            format!("{url}: {message}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_a_document_with_field_problems_without_a_request() {
        let problems = validate_config("{}").unwrap_err();

        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn names_the_field_a_problem_is_about() {
        let problems = validate_config(r#"{"url":"slskd:5030"}"#).unwrap_err();

        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("url"), "{}", problems[0]);
        assert!(problems[1].contains("apiKey"), "{}", problems[1]);
        assert!(problems[2].contains("importRoot"), "{}", problems[2]);
    }

    #[test]
    fn reports_a_document_that_is_not_json() {
        assert_eq!(
            validate_config("not json").unwrap_err(),
            vec!["server config is not valid JSON".to_string()]
        );
    }

    #[test]
    fn reports_an_error_of_each_kind() {
        let url = "http://slskd:5030";

        assert!(probe_problem(url, &ProviderError::NotFound).contains("is it an slskd instance?"));
        assert!(
            probe_problem(url, &ProviderError::Unauthorized("no key".to_string()))
                .contains("API key")
        );
        assert!(probe_problem(url, &ProviderError::RateLimited(2_000)).contains("2000 ms"));
        assert!(
            probe_problem(
                url,
                &ProviderError::Unavailable("connect refused".to_string())
            )
            .contains("is unreachable")
        );
        assert!(probe_problem(url, &ProviderError::Internal("odd".to_string())).contains("odd"));
    }

    #[test]
    fn names_the_required_version() {
        let message = too_old("http://slskd:5030", "0.25.1");

        assert!(message.contains("0.25.1"), "{message}");
        assert!(message.contains(version::MINIMUM_SUPPORTED), "{message}");
    }

    #[test]
    fn advertises_track_and_album_granularity() {
        let capabilities = Slskd::get_capabilities();

        assert_eq!(
            capabilities.granularities,
            vec![Granularity::Track, Granularity::Album]
        );
        assert!(!capabilities.progressive);
    }

    #[test]
    fn keys_search_progress_apart_from_other_entries() {
        assert_eq!(progress_key("abc"), "search:abc");
    }

    #[test]
    fn names_the_batch_route_without_a_trailing_slash() {
        assert_eq!(BATCH_PATH, "/api/v0/transfers/downloads/batches");
        assert_eq!(
            batch_path("4c1f0b7a"),
            "/api/v0/transfers/downloads/batches/4c1f0b7a"
        );
    }

    #[test]
    fn maps_a_failed_enqueue_to_the_error_it_means() {
        assert!(matches!(
            enqueue_problem("peer", 401, b"no key"),
            ProviderError::Unauthorized(_)
        ));
        assert!(matches!(
            enqueue_problem("peer", 404, b""),
            ProviderError::Unavailable(message) if message.contains("offline")
        ));
        assert!(matches!(
            enqueue_problem("peer", 429, b""),
            ProviderError::Unavailable(message) if message.contains("throttling")
        ));
        assert!(matches!(
            enqueue_problem("peer", 500, b"boom"),
            ProviderError::Internal(message) if message.contains("HTTP 500") && message.contains("boom")
        ));
    }

    #[test]
    fn reports_the_progress_record_the_interface_carries() {
        let progress = wit_progress(states::Progress {
            bytes: 10,
            total_bytes: 100,
            speed_bps: 5,
        });

        assert_eq!(progress.bytes, 10);
        assert_eq!(progress.total_bytes, Some(100));
        assert_eq!(progress.speed_bps, Some(5));
    }

    #[test]
    fn pins_output_files_under_the_import_root() {
        let expected = vec![expected_file(
            "@@tqkz\\Music\\01 Song.flac",
            "01 Song.flac",
            4_841,
        )];
        let files = output_files(
            "slskd-downloads",
            "0f8b",
            &expected,
            present(&[("hearthsound/0f8b/01 Song.flac", 4_841)]),
        );

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].root, "slskd-downloads");
        assert_eq!(files[0].path, "hearthsound/0f8b/01 Song.flac");
        assert_eq!(files[0].track_index, None);
    }

    #[test]
    fn renders_the_failures_of_an_enqueue() {
        let failures = vec![
            slskd::Failure {
                filename: "a.mp3".to_string(),
                message: "queue full".to_string(),
            },
            slskd::Failure {
                filename: "b.mp3".to_string(),
                message: "locked".to_string(),
            },
        ];

        let error = failures_problem(&failures);

        assert!(matches!(
            error,
            ProviderError::Internal(message)
                if message.contains("2 file(s)")
                    && message.contains("a.mp3")
                    && message.contains("queue full")
        ));
    }

    /// Base URL of the fake instance.
    const BASE: &str = "http://slskd:5030";

    /// Builds an expected file.
    fn expected_file(filename: &str, relative: &str, size: u64) -> tokens::ExpectedFile {
        tokens::ExpectedFile {
            filename: filename.to_string(),
            name: slskd::basename(filename).to_string(),
            relative: relative.to_string(),
            size,
        }
    }

    /// Builds the stat replacement the poll uses, over the given paths.
    fn present(files: &[(&str, u64)]) -> impl Fn(&str) -> Option<u64> {
        let files: Vec<(String, u64)> = files
            .iter()
            .map(|(path, size)| (path.to_string(), *size))
            .collect();

        move |path: &str| {
            files
                .iter()
                .find(|(known, _)| known == path)
                .map(|(_, size)| *size)
        }
    }

    /// The requests and the remaining routes of a [`Fake`].
    #[derive(Default)]
    struct FakeState {
        /// Answers left, each with the call fragment it answers.
        routes: Vec<(&'static str, u16, &'static str)>,
        /// Every request the client sent.
        requests: Vec<host::http::Request>,
    }

    /// A transport that answers recorded requests and remembers what it was
    /// asked, so the download logic can be tested without an slskd instance.
    ///
    /// Each route is used once, so two enqueues can answer differently. A
    /// clone shares the state, which is how a test reads the calls back after
    /// handing the transport to a client.
    #[derive(Clone, Default)]
    struct Fake {
        state: std::rc::Rc<std::cell::RefCell<FakeState>>,
    }

    impl Fake {
        /// Builds a transport that answers the given routes, in order.
        fn new(routes: &[(&'static str, u16, &'static str)]) -> Self {
            Self {
                state: std::rc::Rc::new(std::cell::RefCell::new(FakeState {
                    routes: routes.to_vec(),
                    requests: Vec::new(),
                })),
            }
        }

        /// Returns the calls the client made, as method and path.
        fn calls(&self) -> Vec<String> {
            self.state
                .borrow()
                .requests
                .iter()
                .map(|request| {
                    let url = request.url.strip_prefix(BASE).unwrap_or(&request.url);

                    format!("{} {url}", request.method)
                })
                .collect()
        }
    }

    impl api::Transport for Fake {
        fn send(
            &self,
            request: &host::http::Request,
        ) -> Result<host::http::Response, ProviderError> {
            let url = request.url.strip_prefix(BASE).unwrap_or(&request.url);
            let call = format!("{} {url}", request.method);
            let mut state = self.state.borrow_mut();
            state.requests.push(request.clone());

            let index = state
                .routes
                .iter()
                .position(|(fragment, _, _)| call.contains(fragment));

            let Some(index) = index else {
                return Err(ProviderError::Unavailable(format!("no route for {call}")));
            };

            let (_, status, body) = state.routes.remove(index);

            Ok(host::http::Response {
                status,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
            })
        }
    }

    /// One batch of a download, answered by [`Fake`] with two transfers.
    const A_BATCH: &str = r#"{"id":"b1","username":"peer_free","transfers":[{"id":"t1","filename":"peer\\A\\one.flac"},{"id":"t2","filename":"peer\\B\\two.flac"}]}"#;

    #[test]
    fn cancels_the_batches_it_already_enqueued() {
        let fake = Fake::new(&[
            (
                "POST /api/v0/transfers/downloads/batches",
                201,
                r#"{"failures":[]}"#,
            ),
            ("POST /api/v0/transfers/downloads/batches", 429, ""),
            ("GET /api/v0/transfers/downloads/batches/", 200, A_BATCH),
            ("DELETE /api/v0/transfers/downloads/", 204, ""),
            ("DELETE /api/v0/transfers/downloads/", 204, ""),
        ]);
        let client = api::Client::with_transport(BASE, "key", fake.clone());
        let selected = vec![
            tokens::RemoteFile {
                filename: "peer\\A\\one.flac".to_string(),
                size: 1,
            },
            tokens::RemoteFile {
                filename: "peer\\B\\two.flac".to_string(),
                size: 2,
            },
        ];

        let failure = enqueue_batches(&client, "peer_free", "0f8b", &selected).unwrap_err();

        assert!(matches!(
            failure.error,
            ProviderError::Unavailable(message) if message.contains("throttling")
        ));
        assert_eq!(failure.cancelled, 2);
        assert_eq!(
            fake.calls()
                .into_iter()
                .filter(|call| call.starts_with("DELETE"))
                .collect::<Vec<String>>(),
            vec![
                "DELETE /api/v0/transfers/downloads/peer_free/t1?remove=true".to_string(),
                "DELETE /api/v0/transfers/downloads/peer_free/t2?remove=true".to_string(),
            ]
        );
    }

    #[test]
    fn cancels_a_partly_accepted_batch() {
        let fake = Fake::new(&[
            (
                "POST /api/v0/transfers/downloads/batches",
                201,
                r#"{"failures":[{"filename":"b.mp3","message":"queue full"}]}"#,
            ),
            (
                "GET /api/v0/transfers/downloads/batches/",
                200,
                r#"{"transfers":[{"id":"t1","filename":"a.mp3"}]}"#,
            ),
            ("DELETE /api/v0/transfers/downloads/", 204, ""),
        ]);
        let client = api::Client::with_transport(BASE, "key", fake.clone());
        let request = slskd::EnqueueBatchRequest {
            id: "b1".to_string(),
            username: "peer_free".to_string(),
            files: vec![
                slskd::BatchFile {
                    filename: "a.mp3".to_string(),
                    size: 1,
                },
                slskd::BatchFile {
                    filename: "b.mp3".to_string(),
                    size: 2,
                },
            ],
            options: slskd::BatchOptions {
                destination: "hearthsound/0f8b".to_string(),
            },
        };

        let error = enqueue(&client, "peer_free", &request).unwrap_err();

        assert!(matches!(
            error,
            ProviderError::Internal(message) if message.contains("queue full")
        ));
        assert_eq!(
            fake.calls()
                .into_iter()
                .filter(|call| call.starts_with("DELETE"))
                .collect::<Vec<String>>(),
            vec!["DELETE /api/v0/transfers/downloads/peer_free/t1?remove=true".to_string()]
        );
    }

    #[test]
    fn clears_imported_transfers_without_removing_the_files() {
        let fake = Fake::new(&[("DELETE /api/v0/transfers/downloads/", 204, "")]);
        let client = api::Client::with_transport(BASE, "key", fake.clone());
        let token = tokens::DownloadToken {
            key: "0f8b".to_string(),
            username: "peer_free".to_string(),
            batches: Vec::new(),
        };

        complete(&client, &token, &["t1".to_string()]);

        assert_eq!(
            fake.calls(),
            vec!["DELETE /api/v0/transfers/downloads/peer_free/t1?remove=false".to_string()]
        );
    }

    #[test]
    fn fails_a_lost_download_and_cancels_it() {
        let fake = Fake::new(&[
            (
                "GET /api/v0/transfers/downloads/batches/",
                200,
                r#"{"transfers":[{"id":"t9","filename":"peer\\A\\other.flac","state":"Completed, Succeeded"}]}"#,
            ),
            (
                "GET /api/v0/transfers/downloads/batches/",
                200,
                r#"{"transfers":[{"id":"t9","filename":"peer\\A\\other.flac"}]}"#,
            ),
            ("DELETE /api/v0/transfers/downloads/", 204, ""),
        ]);
        let client = api::Client::with_transport(BASE, "key", fake.clone());
        let token = tokens::DownloadToken {
            key: "0f8b".to_string(),
            username: "peer_free".to_string(),
            batches: vec![tokens::BatchRef {
                id: "b1".to_string(),
                files: vec![expected_file("peer\\A\\one.flac", "one.flac", 10)],
            }],
        };

        let outcome = poll(&client, &token, "slskd-downloads", present(&[])).ok();

        assert!(matches!(outcome, Some(Polled::Lost)));
        assert_eq!(
            fake.calls()
                .into_iter()
                .filter(|call| call.starts_with("DELETE"))
                .collect::<Vec<String>>(),
            vec!["DELETE /api/v0/transfers/downloads/peer_free/t9?remove=true".to_string()]
        );
    }

    /// Builds a client whose one batch reports a completed transfer.
    fn completed_batch() -> api::Client<Fake> {
        let fake = Fake::new(&[
            (
                "GET /api/v0/transfers/downloads/batches/",
                200,
                r#"{"transfers":[{"id":"t1","filename":"peer\\A\\CD1\\01 - x.flac","state":"Completed, Succeeded"}]}"#,
            ),
            ("DELETE /api/v0/transfers/downloads/", 204, ""),
        ]);

        api::Client::with_transport(BASE, "key", fake)
    }

    /// Builds the download of [`completed_batch`].
    fn completed_download() -> tokens::DownloadToken {
        tokens::DownloadToken {
            key: "0f8b".to_string(),
            username: "peer_free".to_string(),
            batches: vec![tokens::BatchRef {
                id: "b1".to_string(),
                files: vec![expected_file(
                    "peer\\A\\CD1\\01 - x.flac",
                    "CD1/01 - x.flac",
                    10,
                )],
            }],
        }
    }

    #[test]
    fn accepts_a_completed_file_of_a_flattened_batch() {
        let client = completed_batch();

        let outcome = poll(
            &client,
            &completed_download(),
            "slskd-downloads",
            present(&[("hearthsound/0f8b/01 - x.flac", 10)]),
        )
        .ok();

        let Some(Polled::Complete(files)) = outcome else {
            panic!("expected a completed download");
        };

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "hearthsound/0f8b/01 - x.flac");
    }

    #[test]
    fn accepts_a_completed_file_of_a_nested_batch() {
        let client = completed_batch();

        let outcome = poll(
            &client,
            &completed_download(),
            "slskd-downloads",
            present(&[("hearthsound/0f8b/CD1/01 - x.flac", 10)]),
        )
        .ok();

        let Some(Polled::Complete(files)) = outcome else {
            panic!("expected a completed download");
        };

        assert_eq!(files[0].path, "hearthsound/0f8b/CD1/01 - x.flac");
    }

    #[test]
    fn keeps_waiting_for_a_file_that_is_not_at_a_path_yet() {
        let client = completed_batch();

        let outcome = poll(
            &client,
            &completed_download(),
            "slskd-downloads",
            present(&[("hearthsound/0f8b/CD1/01 - x.flac", 9)]),
        )
        .ok();

        assert!(matches!(
            outcome,
            Some(Polled::State(DownloadState::Running(_)))
        ));
    }
}
