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
        let destination = download::destination(&key);
        let client = client(&config);
        let mut batches = Vec::new();

        // One batch per remote directory: slskd downloads a batch into one
        // destination directory and flattens its files, so the files of a batch
        // have to be the files of one remote directory.
        for group in download::group_by_directory(&selected) {
            let id = random::guid();
            let request = slskd::EnqueueBatchRequest {
                id: id.clone(),
                username: candidate.username.clone(),
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

            enqueue(&client, &candidate.username, &request)?;

            batches.push(tokens::BatchRef {
                id,
                files: download::expected(&group.files),
            });
        }

        hearthsound_plugin::info!(
            "downloading {} file(s) from {} into {destination} in {} batch(es)",
            selected.len(),
            candidate.username,
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
                // A batch slskd no longer knows leaves its files unaccounted
                // for, which the assessment reports as unverifiable.
                Err(ProviderError::NotFound) => {
                    hearthsound_plugin::warn!("slskd no longer knows batch {}", batch.id);
                }
                Err(error) => return Err(error),
            }
        }

        match states::assess(&expected, &observed) {
            states::Assessment::Failed(message) => {
                hearthsound_plugin::warn!("download {} failed: {message}", token.key);

                Ok(DownloadState::Failed(message))
            }
            states::Assessment::Queued(place) => Ok(DownloadState::Queued(place)),
            states::Assessment::Running(progress) => {
                Ok(DownloadState::Running(wit_progress(progress)))
            }
            states::Assessment::AwaitingFiles(progress) => {
                if !files_present(&config, &token.key, &expected) {
                    // slskd signals a completed transfer before it moves the
                    // file out of its incomplete directory, so the file is
                    // only believed once it is at its pinned path.
                    return Ok(DownloadState::Running(wit_progress(progress)));
                }

                complete(&client, &token, &transfers);

                Ok(DownloadState::Complete(output_files(
                    &config, &token.key, &expected,
                )))
            }
            states::Assessment::Unverifiable => {
                if !files_present(&config, &token.key, &expected) {
                    // The download cannot be judged and its files are not
                    // there: the server treats this as a lost download and
                    // restarts the search.
                    return Err(ProviderError::NotFound);
                }

                complete(&client, &token, &transfers);

                Ok(DownloadState::Complete(output_files(
                    &config, &token.key, &expected,
                )))
            }
        }
    }

    fn cancel_download(download_token: String) {
        let Ok(token) = tokens::decode::<tokens::DownloadToken>(&download_token) else {
            hearthsound_plugin::warn!("cannot cancel a download from an unreadable token");

            return;
        };

        let Ok(config) = configured() else {
            return;
        };

        let client = client(&config);

        for batch in &token.batches {
            let ids: Vec<String> = transfers(&client, &batch.id)
                .into_iter()
                .map(|transfer| transfer.id)
                .collect();

            clear_transfers(&client, &token.username, &ids);
        }
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
/// cannot use or with failures for the files it enqueued.
fn enqueue(
    client: &api::Client,
    username: &str,
    request: &slskd::EnqueueBatchRequest,
) -> Result<(), ProviderError> {
    let response = client.post(BATCH_PATH, request)?;

    if !http::is_success(&response) {
        return Err(enqueue_problem(username, response.status, &response.body));
    }

    let enqueued: slskd::EnqueueBatchResponse = api::decode(&response)?;

    if !enqueued.failures.is_empty() {
        return Err(failures_problem(&enqueued.failures));
    }

    Ok(())
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
fn transfers(client: &api::Client, id: &str) -> Vec<slskd::Transfer> {
    match client.get_json::<slskd::Batch>(&batch_path(id)) {
        Ok(batch) => batch.transfers,
        Err(error) => {
            hearthsound_plugin::warn!("could not read slskd batch {id}: {error:?}");

            Vec::new()
        }
    }
}

/// Clears one transfer from slskd's records.
///
/// `remove=true` is a record-only delete: slskd marks the transfer removed and
/// leaves the file where it is, so a file the server has not imported yet is
/// never touched.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the transfer belongs to, which its route needs.
/// - `id`: id of the transfer.
fn clear_transfer(client: &api::Client, username: &str, id: &str) {
    let path = format!(
        "/api/v0/transfers/downloads/{}/{}?remove=true",
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

/// Reports a finished download and clears its transfers from slskd.
///
/// slskd keeps every transfer record it is not told to forget, so the plugin
/// clears the ones it reported. `remove=true` is a record-only delete: the file
/// itself is left where slskd put it for the server to import.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `token`: the download's token.
/// - `transfers`: ids of the transfers to clear.
fn complete(client: &api::Client, token: &tokens::DownloadToken, transfers: &[String]) {
    hearthsound_plugin::info!(
        "download {} from {} produced {} file(s)",
        token.key,
        token.username,
        token
            .batches
            .iter()
            .map(|batch| batch.files.len())
            .sum::<usize>()
    );

    clear_transfers(client, &token.username, transfers);
}

/// Clears the given transfers from slskd's records, ignoring failures.
///
/// # Parameters
///
/// - `client`: the instance's client.
/// - `username`: the peer the transfers belong to.
/// - `ids`: ids of the transfers.
fn clear_transfers(client: &api::Client, username: &str, ids: &[String]) {
    for id in ids {
        clear_transfer(client, username, id);
    }
}

/// Returns whether every expected file is present at its pinned path.
///
/// # Parameters
///
/// - `config`: the instance configuration, which names the import root.
/// - `key`: the download's key, which names the destination directory.
/// - `expected`: the files the download is expected to produce.
///
/// # Returns
///
/// `true` when every file is a regular file of exactly the expected size. A
/// file that is still being written, renamed or missing makes this `false`.
fn files_present(config: &config::Config, key: &str, expected: &[tokens::ExpectedFile]) -> bool {
    expected.iter().all(|file| {
        let path = download::pinned_path(key, &file.name);

        match host::files::stat(&config.import_root, &path) {
            Some(stat) => !stat.is_dir && stat.size == file.size,
            None => false,
        }
    })
}

/// Builds the output files of a completed download.
///
/// # Parameters
///
/// - `config`: the instance configuration, which names the import root.
/// - `key`: the download's key.
/// - `expected`: the files the download produced.
///
/// # Returns
///
/// One output file per expected file, naming the import root and the pinned
/// path.
fn output_files(
    config: &config::Config,
    key: &str,
    expected: &[tokens::ExpectedFile],
) -> Vec<OutputFile> {
    expected
        .iter()
        .map(|file| OutputFile {
            root: config.import_root.clone(),
            path: download::pinned_path(key, &file.name),
            track_index: None,
        })
        .collect()
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
        let config = config::Config::from_json(r#"{"importRoot":"slskd-downloads"}"#).unwrap();
        let expected = vec![tokens::ExpectedFile {
            filename: "@@tqkz\\Music\\01 Song.flac".to_string(),
            name: "01 Song.flac".to_string(),
            size: 4_841,
        }];

        let files = output_files(&config, "0f8b", &expected);

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
}
