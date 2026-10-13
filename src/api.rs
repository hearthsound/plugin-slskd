//! HTTP access to the slskd API through the `host-http` host import.
//!
//! This is the only module that touches the network. It owns the instance's
//! base URL and API key, builds the requests the slskd endpoints expect, and
//! maps a failing response to the provider error the server sees. The endpoints
//! themselves are composed by the modules that call this one.

use hearthsound_plugin::host::http::Request;
use hearthsound_plugin::http;
use hearthsound_plugin::types::ProviderError;

use crate::url;
use crate::version;

/// Wall-clock budget for one slskd request, in milliseconds.
///
/// The host clamps this to its own ceiling and to the remaining call deadline.
const REQUEST_TIMEOUT_MS: u32 = 10_000;

/// One slskd instance, as this provider instance is configured for it.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    /// Base URL of the instance, trimmed and without a trailing slash.
    base_url: String,
    /// API key sent as `X-API-Key` on every request.
    api_key: String,
}

impl Client {
    /// Builds a client for one instance.
    ///
    /// # Parameters
    ///
    /// - `base_url`: the configured base URL, with or without a trailing slash.
    /// - `api_key`: the configured API key.
    pub(crate) fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            base_url: url::normalize_base_url(base_url),
            api_key: api_key.to_string(),
        }
    }

    /// Reads the instance's semantic version.
    ///
    /// # Returns
    ///
    /// The version string the version endpoint reports, for example `0.26.1`.
    ///
    /// # Errors
    ///
    /// Returns the mapped status of a failing response (unauthorized for `401`
    /// and `403`, not-found for `404`, rate-limited for `429`, unavailable for
    /// `5xx`), an unavailable error when the host cannot complete the request,
    /// and an internal error when the body is not a version.
    pub(crate) fn version(&self) -> Result<String, ProviderError> {
        let request = self.request("GET", "/api/v0/application/version", None);
        let response = http::send(&request)?;

        if !http::is_success(&response) {
            return Err(http::status_error(&response));
        }

        version::from_body(&response.body)
    }

    /// Builds a request for one API path.
    ///
    /// # Parameters
    ///
    /// - `method`: the HTTP method.
    /// - `path`: the API path, with or without a leading slash.
    /// - `body`: the request body, already encoded.
    ///
    /// # Returns
    ///
    /// The request, carrying the authentication header and the plugin's request
    /// timeout.
    fn request(&self, method: &str, path: &str, body: Option<Vec<u8>>) -> Request {
        Request {
            method: method.to_string(),
            url: url::endpoint(&self.base_url, path),
            headers: vec![
                ("accept".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), self.api_key.clone()),
            ],
            body,
            timeout_ms: Some(REQUEST_TIMEOUT_MS),
        }
    }
}
