//! The plugin's server-scoped configuration.
//!
//! The manifest carries the JSON Schema the admin UI renders; this module is
//! the plugin's own view of the same document. It has two halves:
//!
//! - [`Config`], the typed configuration the call path reads, with the defaults
//!   the manifest declares;
//! - [`check`], the schema-level validation `validate-config` runs before it
//!   asks the configured instance anything.
//!
//! Everything here is pure: it never touches the network and never depends on
//! the plugin bindings beyond the shared error type, so it is unit-tested on
//! the host target.

use crate::url;

/// slskd base URL used when the instance configuration omits one.
pub const DEFAULT_URL: &str = "http://slskd:5030";

/// The server-scoped configuration of one plugin instance.
///
/// Every field has a default, because the plugin must stay callable while an
/// administrator is still filling the configuration in; [`check`] is what
/// refuses an unusable document before it is stored.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Base URL of the slskd instance.
    #[serde(default = "default_url")]
    pub url: String,
    /// slskd API key, sent as `X-API-Key`.
    #[serde(default)]
    pub api_key: String,
    /// Id of the import root whose path is slskd's downloads directory.
    #[serde(default)]
    pub import_root: String,
    /// How long one slskd search may run, in milliseconds.
    #[serde(default = "default_search_timeout_ms")]
    pub search_timeout_ms: u32,
    /// Maximum number of peer responses slskd keeps for a search.
    #[serde(default = "default_response_limit")]
    pub response_limit: u32,
    /// Maximum number of files slskd keeps for a search.
    #[serde(default = "default_file_limit")]
    pub file_limit: u32,
    /// Peers slower than this are dropped by slskd, in kilobits per second.
    #[serde(default)]
    pub min_upload_speed_kbps: u32,
    /// Peers with a longer upload queue are skipped by the plugin.
    #[serde(default = "default_max_peer_queue_length")]
    pub max_peer_queue_length: u32,
    /// Audio extensions that count as candidates.
    #[serde(default = "default_preferred_extensions")]
    pub preferred_extensions: Vec<String>,
}

impl Config {
    /// Deserializes an instance configuration document.
    ///
    /// # Parameters
    ///
    /// - `json`: the JSON document `host-config` returned, or the one
    ///   `validate-config` was handed.
    ///
    /// # Returns
    ///
    /// The typed configuration, with the manifest defaults for every field the
    /// document omits.
    ///
    /// # Errors
    ///
    /// Returns the message describing the problem when the document is not JSON
    /// or a field does not have the type the manifest declares.
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| format!("server config: {error}"))
    }

    /// Returns the instance's base URL without a trailing slash.
    pub fn base_url(&self) -> String {
        url::normalize_base_url(&self.url)
    }
}

/// Returns the default base URL of the slskd instance.
fn default_url() -> String {
    DEFAULT_URL.to_string()
}

/// Returns the default search timeout.
fn default_search_timeout_ms() -> u32 {
    15_000
}

/// Returns the default response limit of one search.
fn default_response_limit() -> u32 {
    100
}

/// Returns the default file limit of one search.
fn default_file_limit() -> u32 {
    10_000
}

/// Returns the default maximum peer queue length.
fn default_max_peer_queue_length() -> u32 {
    50
}

/// Returns the extensions that count as candidates when none are configured.
fn default_preferred_extensions() -> Vec<String> {
    vec!["flac".to_string(), "mp3".to_string()]
}

/// Returns the schema-level problems of a configuration document.
///
/// These are the checks `validate-config` can make without asking the instance
/// anything: the document parses, the URL is one a request can be built from,
/// and the key and import root are present. A problem names the field it is
/// about, so an administrator can act on it.
///
/// # Parameters
///
/// - `json`: the JSON document to check.
///
/// # Returns
///
/// One message per problem, in field order; empty when the document passes. A
/// document that is not JSON at all yields exactly one message.
///
/// # Example
///
/// ```text
/// check(r#"{"url":"slskd:5030","apiKey":"k","importRoot":"r"}"#)
///     -> ["url must be an http or https URL, got `slskd:5030`"]
/// ```
pub fn check(json: &str) -> Vec<String> {
    let Ok(document) = serde_json::from_str::<serde_json::Value>(json) else {
        return vec!["server config is not valid JSON".to_string()];
    };

    let mut problems = Vec::new();
    check_url(&document, &mut problems);
    check_text(&document, "apiKey", "apiKey", &mut problems);
    check_text(&document, "importRoot", "importRoot", &mut problems);

    problems
}

/// Checks the `url` member of a configuration document.
///
/// # Parameters
///
/// - `document`: the configuration document.
/// - `problems`: collects the messages.
fn check_url(document: &serde_json::Value, problems: &mut Vec<String>) {
    match text(document, "url") {
        Some(url) if url::is_http_url(url) => {}
        Some(url) => problems.push(format!("url must be an http or https URL, got `{url}`")),
        None => problems.push("url must be an http or https URL".to_string()),
    }
}

/// Checks one required string field, appending a problem when it is missing or
/// empty.
///
/// # Parameters
///
/// - `document`: the configuration document.
/// - `field`: the JSON member to read.
/// - `label`: the name the problem message uses, which is the member name.
/// - `problems`: collects the messages.
fn check_text(document: &serde_json::Value, field: &str, label: &str, problems: &mut Vec<String>) {
    match text(document, field) {
        Some(value) if !value.trim().is_empty() => {}
        Some(_) => problems.push(format!("{label} must not be empty")),
        None => problems.push(format!("{label} is required")),
    }
}

/// Reads a string member of a configuration document.
///
/// # Parameters
///
/// - `document`: the configuration document.
/// - `field`: the JSON member to read.
///
/// # Returns
///
/// The member's string value, and `None` when the member is absent, `null`, or
/// not a string.
fn text<'a>(document: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    document.get(field).and_then(serde_json::Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_complete_configuration() {
        assert_eq!(
            check(
                r#"{"url":"http://slskd:5030","apiKey":"secret","importRoot":"slskd-downloads"}"#
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn reports_a_document_that_is_not_json() {
        assert_eq!(
            check("not json"),
            vec!["server config is not valid JSON".to_string()]
        );
    }

    #[test]
    fn reports_every_missing_field_at_once() {
        let problems = check("{}");

        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems[0].contains("url must be an http or https URL"),
            "{}",
            problems[0]
        );
        assert!(
            problems[1].contains("apiKey is required"),
            "{}",
            problems[1]
        );
        assert!(
            problems[2].contains("importRoot is required"),
            "{}",
            problems[2]
        );
    }

    #[test]
    fn reports_empty_field_values() {
        let problems = check(r#"{"url":"http://slskd:5030","apiKey":"  ","importRoot":""}"#);

        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("apiKey must not be empty"));
        assert!(problems[1].contains("importRoot must not be empty"));
    }

    #[test]
    fn reports_a_url_that_is_not_http() {
        let problems = check(r#"{"url":"slskd:5030","apiKey":"secret","importRoot":"root"}"#);

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("url must be an http or https URL"));
        assert!(problems[0].contains("slskd:5030"));
    }

    #[test]
    fn reports_fields_of_the_wrong_type() {
        let problems = check(r#"{"url":7,"apiKey":[],"importRoot":null}"#);

        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("url must be an http or https URL"));
        assert!(problems[1].contains("apiKey is required"));
        assert!(problems[2].contains("importRoot is required"));
    }

    #[test]
    fn uses_the_manifest_defaults() {
        let config = Config::from_json("{}").unwrap();

        assert_eq!(config.url, DEFAULT_URL);
        assert_eq!(config.base_url(), "http://slskd:5030");
        assert_eq!(config.search_timeout_ms, 15_000);
        assert_eq!(config.response_limit, 100);
        assert_eq!(config.file_limit, 10_000);
        assert_eq!(config.min_upload_speed_kbps, 0);
        assert_eq!(config.max_peer_queue_length, 50);
        assert_eq!(config.preferred_extensions, vec!["flac", "mp3"]);
        assert!(config.api_key.is_empty());
        assert!(config.import_root.is_empty());
    }

    #[test]
    fn reads_the_configured_values() {
        let config = Config::from_json(
            r#"{
                "url": "http://slskd:5030/",
                "apiKey": "secret",
                "importRoot": "slskd-downloads",
                "searchTimeoutMs": 8000,
                "responseLimit": 10,
                "fileLimit": 250,
                "minUploadSpeedKbps": 80,
                "maxPeerQueueLength": 5,
                "preferredExtensions": ["flac"]
            }"#,
        )
        .unwrap();

        assert_eq!(config.base_url(), "http://slskd:5030");
        assert_eq!(config.api_key, "secret");
        assert_eq!(config.import_root, "slskd-downloads");
        assert_eq!(config.search_timeout_ms, 8_000);
        assert_eq!(config.response_limit, 10);
        assert_eq!(config.file_limit, 250);
        assert_eq!(config.min_upload_speed_kbps, 80);
        assert_eq!(config.max_peer_queue_length, 5);
        assert_eq!(config.preferred_extensions, vec!["flac"]);
    }

    #[test]
    fn rejects_a_field_of_the_wrong_type() {
        let message = Config::from_json(r#"{"searchTimeoutMs":"soon"}"#).unwrap_err();

        assert!(message.contains("server config"), "{message}");
    }
}
