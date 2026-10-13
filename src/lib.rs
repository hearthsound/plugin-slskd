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
//! The pure logic sits in public modules ([`url`], [`version`], [`config`]); the
//! binding-coupled glue that drives it - the HTTP transport and the acquisition
//! operations - stays private.
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

use hearthsound_plugin::host;
use hearthsound_plugin::types::ProviderError;

mod api;
pub mod config;
pub mod url;
pub mod version;

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
}
