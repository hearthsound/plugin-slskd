//! HTTP access to the slskd API through the `host-http` host import.
//!
//! This is the only module that touches the network. It owns the instance's
//! base URL and API key, builds the requests the slskd endpoints expect, and
//! maps a failing response to the provider error the server sees. The endpoints
//! themselves are composed by the modules that call this one.
//!
//! [`Transport`] is the seam the download logic is tested through: the default
//! transport hands each request to the host, and the unit tests hand it a fake
//! that records the request and answers from a fixture.

use hearthsound_plugin::host::http::{Request, Response};
use hearthsound_plugin::http;
use hearthsound_plugin::types::ProviderError;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::url;
use crate::version;

/// Wall-clock budget for one slskd request, in milliseconds.
///
/// The host clamps this to its own ceiling and to the remaining call deadline.
const REQUEST_TIMEOUT_MS: u32 = 10_000;

/// Performs one HTTP request.
///
/// The client builds requests and reads responses; where the bytes travel is
/// the transport's business. That is what lets the download logic be unit-tested
/// on the host target, where the `host-http` import of the shipped wasm build
/// does not exist.
pub(crate) trait Transport {
    /// Sends one request and returns its response, whatever its status.
    ///
    /// # Parameters
    ///
    /// - `request`: the request to perform.
    ///
    /// # Returns
    ///
    /// The response.
    ///
    /// # Errors
    ///
    /// Returns the transport's own failure: [`ProviderError::Unauthorized`] for
    /// a request the host denied and [`ProviderError::Unavailable`] for one it
    /// could not complete.
    fn send(&self, request: &Request) -> Result<Response, ProviderError>;
}

/// The transport that sends requests through the `host-http` host import.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HttpTransport;

impl Transport for HttpTransport {
    fn send(&self, request: &Request) -> Result<Response, ProviderError> {
        http::send(request)
    }
}

/// One slskd instance, as this provider instance is configured for it.
#[derive(Debug, Clone)]
pub(crate) struct Client<T: Transport = HttpTransport> {
    /// Base URL of the instance, trimmed and without a trailing slash.
    base_url: String,
    /// API key sent as `X-API-Key` on every request.
    api_key: String,
    /// Carries the requests to the instance.
    transport: T,
}

impl Client<HttpTransport> {
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
            transport: HttpTransport,
        }
    }
}

impl<T: Transport> Client<T> {
    /// Builds a client over a transport other than the host's.
    ///
    /// # Parameters
    ///
    /// - `base_url`: the configured base URL, with or without a trailing slash.
    /// - `api_key`: the configured API key.
    /// - `transport`: where the requests travel.
    ///
    /// # Returns
    ///
    /// A client that hands every request to `transport`.
    #[cfg(test)]
    pub(crate) fn with_transport(base_url: &str, api_key: &str, transport: T) -> Self {
        Self {
            base_url: url::normalize_base_url(base_url),
            api_key: api_key.to_string(),
            transport,
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
        let response = self.send(&self.request("GET", "/api/v0/application/version"))?;
        require_success(&response)?;

        version::from_body(&response.body)
    }

    /// Sends a GET request and decodes its JSON body.
    ///
    /// # Parameters
    ///
    /// - `path`: the API path, including any query string.
    ///
    /// # Returns
    ///
    /// The decoded response body.
    ///
    /// # Errors
    ///
    /// Returns the mapped status of a failing response, an unavailable error
    /// when the host cannot complete the request, and an internal error when
    /// the body is not JSON for `R`.
    pub(crate) fn get_json<R: DeserializeOwned>(&self, path: &str) -> Result<R, ProviderError> {
        self.send_json(&self.request("GET", path))
    }

    /// Sends a POST request with a JSON body and decodes its JSON response.
    ///
    /// # Parameters
    ///
    /// - `path`: the API path.
    /// - `body`: the value to send as the request body.
    ///
    /// # Returns
    ///
    /// The decoded response body.
    ///
    /// # Errors
    ///
    /// Returns the mapped status of a failing response, an unavailable error
    /// when the host cannot complete the request, [`ProviderError::Internal`]
    /// when the body cannot be serialized, and an internal error when the
    /// response is not JSON for `R`.
    pub(crate) fn post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, ProviderError> {
        let response = self.post(path, body)?;
        require_success(&response)?;

        decode(&response)
    }

    /// Sends a POST request with a JSON body and returns the raw response.
    ///
    /// The batch enqueue needs the status and the body itself, because slskd
    /// reports a partially enqueued batch with a success status and a list of
    /// failures in the body.
    ///
    /// # Parameters
    ///
    /// - `path`: the API path.
    /// - `body`: the value to send as the request body.
    ///
    /// # Returns
    ///
    /// The response, whatever its status.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Internal`] when the body cannot be serialized
    /// and [`ProviderError::Unavailable`] when the host cannot complete the
    /// request.
    pub(crate) fn post<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<Response, ProviderError> {
        let mut request = self.request("POST", path);
        request.body = Some(
            serde_json::to_vec(body)
                .map_err(|error| ProviderError::Internal(format!("request body: {error}")))?,
        );
        request
            .headers
            .push(("content-type".to_string(), "application/json".to_string()));

        self.send(&request)
    }

    /// Sends a request that carries no body and returns its status.
    ///
    /// Used for the requests whose answer is only a status code, such as the
    /// cancellation and deletion calls, where the caller decides whether a
    /// failure matters.
    ///
    /// # Parameters
    ///
    /// - `method`: the HTTP method.
    /// - `path`: the API path, including any query string.
    ///
    /// # Returns
    ///
    /// The response status.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unavailable`] when the host cannot complete the
    /// request.
    pub(crate) fn status(&self, method: &str, path: &str) -> Result<u16, ProviderError> {
        Ok(self.send(&self.request(method, path))?.status)
    }

    /// Sends a request and returns its response, whatever its status.
    ///
    /// # Parameters
    ///
    /// - `request`: the request to perform.
    ///
    /// # Returns
    ///
    /// The response.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unauthorized`] when the host denied the request
    /// and [`ProviderError::Unavailable`] when it timed out or failed.
    fn send(&self, request: &Request) -> Result<Response, ProviderError> {
        self.transport.send(request)
    }

    /// Sends a request, requires a successful status and decodes the body.
    ///
    /// # Parameters
    ///
    /// - `request`: the request to perform.
    ///
    /// # Returns
    ///
    /// The decoded response body.
    ///
    /// # Errors
    ///
    /// Returns the mapped status of a failing response and
    /// [`ProviderError::Internal`] when the body is not JSON for `R`.
    fn send_json<R: DeserializeOwned>(&self, request: &Request) -> Result<R, ProviderError> {
        let response = self.send(request)?;
        require_success(&response)?;

        decode(&response)
    }

    /// Builds a request for one API path.
    ///
    /// # Parameters
    ///
    /// - `method`: the HTTP method.
    /// - `path`: the API path, with or without a leading slash.
    ///
    /// # Returns
    ///
    /// The request, carrying the authentication header and the plugin's request
    /// timeout.
    fn request(&self, method: &str, path: &str) -> Request {
        Request {
            method: method.to_string(),
            url: url::endpoint(&self.base_url, path),
            headers: vec![
                ("accept".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), self.api_key.clone()),
            ],
            body: None,
            timeout_ms: Some(REQUEST_TIMEOUT_MS),
        }
    }
}

/// Decodes a response body as JSON.
///
/// # Parameters
///
/// - `response`: the response whose body to decode.
///
/// # Returns
///
/// The decoded body.
///
/// # Errors
///
/// Returns [`ProviderError::Internal`] when the body is not JSON for `T`.
pub(crate) fn decode<T: DeserializeOwned>(response: &Response) -> Result<T, ProviderError> {
    serde_json::from_slice(&response.body)
        .map_err(|error| ProviderError::Internal(format!("response body: {error}")))
}

/// Requires a successful status, mapping the failure to a provider error.
///
/// # Parameters
///
/// - `response`: the response to check.
///
/// # Returns
///
/// `Ok(())` for a `2xx` status.
///
/// # Errors
///
/// Returns [`http::status_error`] of a non-`2xx` response: unauthorized for
/// `401` and `403`, not-found for `404`, rate-limited for `429`, unavailable
/// for `5xx`, and internal for the remaining statuses.
fn require_success(response: &Response) -> Result<(), ProviderError> {
    if http::is_success(response) {
        Ok(())
    } else {
        Err(http::status_error(response))
    }
}
