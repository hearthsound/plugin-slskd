//! The opaque tokens the plugin hands the server.
//!
//! A plugin instance is recycled between calls, so every later call has to be
//! given back whatever the earlier one learned. The plugin does that with two
//! mechanisms: the token the server holds, which names the host-kv entry
//! holding the rest of the state, and the token's own payload for the parts
//! that are small enough to travel with it.
//!
//! A token is the base64url encoding, without padding, of a JSON object, so it
//! survives URLs, log lines and database columns unchanged.
//!
//! Everything here is pure: it never touches the network, so it is unit-tested
//! on the host target.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hearthsound_plugin::types::ProviderError;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The token `start-search` returns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchToken {
    /// Key of the host-kv entry that holds the search's widening progress.
    pub key: String,
}

/// The token a candidate carries.
///
/// The token names the peer and the exact remote files of the candidate, so the
/// server can hand a candidate back for download long after the search that
/// produced it, and so `start-download` can enqueue exactly the files that were
/// checked.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateToken {
    /// Id of the slskd search the candidate came from, when it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_id: Option<String>,
    /// Name of the peer that offers the files.
    pub username: String,
    /// Remote files of the candidate, exactly as the peer reported them.
    pub files: Vec<RemoteFile>,
}

/// One remote file, as a peer reported it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteFile {
    /// Remote path, with the peer's own separators.
    pub filename: String,
    /// Size in bytes.
    pub size: u64,
}

/// Encodes a token payload.
///
/// # Parameters
///
/// - `value`: the payload to encode.
///
/// # Returns
///
/// The base64url token, without padding.
///
/// # Errors
///
/// Returns [`ProviderError::Internal`] when the payload cannot be serialized,
/// which for the payloads of this plugin means a bug rather than a bad call.
pub fn encode<T: Serialize>(value: &T) -> Result<String, ProviderError> {
    let json = serde_json::to_vec(value)
        .map_err(|error| ProviderError::Internal(format!("token: {error}")))?;

    Ok(URL_SAFE_NO_PAD.encode(json))
}

/// Decodes a token into its payload.
///
/// # Parameters
///
/// - `token`: the token a caller handed back.
///
/// # Returns
///
/// The decoded payload.
///
/// # Errors
///
/// Returns [`ProviderError::Internal`] when the token is not base64url, is not
/// JSON, or does not have the payload's shape, which means the caller handed
/// back something this plugin did not produce.
pub fn decode<T: DeserializeOwned>(token: &str) -> Result<T, ProviderError> {
    let json = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|error| ProviderError::Internal(format!("token: {error}")))?;

    serde_json::from_slice(&json)
        .map_err(|error| ProviderError::Internal(format!("token: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_search_token() {
        let token = SearchToken {
            key: "0f8b3c1d2e4a5b6c7d8e9f0a1b2c3d4e".to_string(),
        };

        let encoded = encode(&token).unwrap();
        assert_eq!(decode::<SearchToken>(&encoded).unwrap(), token);
    }

    #[test]
    fn a_token_is_base64url_without_padding() {
        let encoded = encode(&SearchToken {
            key: "a".to_string(),
        })
        .unwrap();

        assert!(!encoded.contains('='), "{encoded}");
        assert!(!encoded.contains('+'), "{encoded}");
        assert!(!encoded.contains('/'), "{encoded}");
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn round_trips_a_candidate_token() {
        let token = CandidateToken {
            search_id: Some("8f14e45f-ceea-467f-a1c8-9c5b0d1b4f3a".to_string()),
            username: "peer_free".to_string(),
            files: vec![RemoteFile {
                filename: "@@tqkz\\Music\\01 Windowlicker.flac".to_string(),
                size: 48_410_233,
            }],
        };

        let json: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encode(&token).unwrap()).unwrap())
                .unwrap();

        assert_eq!(json["username"], "peer_free");
        assert_eq!(json["searchId"], "8f14e45f-ceea-467f-a1c8-9c5b0d1b4f3a");
        assert_eq!(json["files"][0]["size"], 48_410_233);
        assert_eq!(
            decode::<CandidateToken>(&encode(&token).unwrap()).unwrap(),
            token
        );
    }

    #[test]
    fn a_candidate_token_without_a_search_id_omits_it() {
        let token = CandidateToken {
            search_id: None,
            username: "peer".to_string(),
            files: Vec::new(),
        };

        let json = URL_SAFE_NO_PAD.decode(encode(&token).unwrap()).unwrap();

        assert!(!String::from_utf8_lossy(&json).contains("searchId"));
        assert_eq!(
            decode::<CandidateToken>(&encode(&token).unwrap()).unwrap(),
            token
        );
    }

    #[test]
    fn rejects_a_token_that_is_not_base64url() {
        let error = decode::<SearchToken>("not a token!").unwrap_err();

        assert!(matches!(error, ProviderError::Internal(message) if message.contains("token")));
    }

    #[test]
    fn rejects_a_token_with_another_payload() {
        let error = decode::<CandidateToken>(
            &encode(&SearchToken {
                key: "a".to_string(),
            })
            .unwrap(),
        )
        .unwrap_err();

        assert!(matches!(error, ProviderError::Internal(_)));
    }
}
