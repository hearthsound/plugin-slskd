//! slskd acquisition plugin: a Hearthsound acquisition provider over an slskd
//! instance.
//!
//! The plugin drives one [slskd](https://github.com/slskd/slskd) instance, which
//! owns the Soulseek connection, the peer upload queue and the download
//! directory. It implements `lifecycle` (configuration validation and a health
//! probe) and `acquisition` (search, candidate download and download
//! monitoring), and nothing else.
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
    Capabilities, DownloadState, Granularity, Guest as AcquisitionGuest, Query, SearchState,
};
use hearthsound_plugin::acquisition_provider::exports::hearthsound::plugin::lifecycle::Guest as LifecycleGuest;
use hearthsound_plugin::host;
use hearthsound_plugin::kv;
use hearthsound_plugin::types::ProviderError;

mod api;
pub mod config;
pub mod random;
pub mod search;
pub mod slskd;
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
            granularities: vec![Granularity::Track],
            progressive: false,
        }
    }

    fn start_search(query: Query) -> Result<String, ProviderError> {
        let config = configured()?;

        if query.granularity != Granularity::Track {
            return Err(ProviderError::Internal(
                "this plugin acquires tracks only".to_string(),
            ));
        }

        let wanted = query.tracks.first().ok_or_else(|| {
            ProviderError::Internal("the query carries no wanted track".to_string())
        })?;
        let artist = wanted.artists.first().map_or("", String::as_str);
        let plan = search::plan(&wanted.title, artist);

        if plan.is_empty() {
            return Err(ProviderError::Internal(
                "the wanted track has no usable title".to_string(),
            ));
        }

        let key = random::key();
        let token = tokens::encode(&tokens::SearchToken { key: key.clone() })?;
        let mut progress = search::Progress::new(plan);
        let variant = progress.variant().cloned();

        hearthsound_plugin::info!(
            "searching slskd for `{}` in {} variants",
            wanted.title,
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
        let found = search::candidates(
            &search_id,
            &responses,
            &filter,
            variant.require_artist.as_deref(),
        )?;

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

    fn start_download(_candidate_token: String, _files: Vec<u32>) -> Result<String, ProviderError> {
        Err(ProviderError::Internal(
            "downloading candidates is not implemented yet".to_string(),
        ))
    }

    fn poll_download(_download_token: String) -> Result<DownloadState, ProviderError> {
        Err(ProviderError::Internal(
            "downloading candidates is not implemented yet".to_string(),
        ))
    }

    fn cancel_download(_download_token: String) {}
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
    fn advertises_track_granularity_only() {
        let capabilities = Slskd::get_capabilities();

        assert_eq!(capabilities.granularities, vec![Granularity::Track]);
        assert!(!capabilities.progressive);
    }

    #[test]
    fn keys_search_progress_apart_from_other_entries() {
        assert_eq!(progress_key("abc"), "search:abc");
    }
}
