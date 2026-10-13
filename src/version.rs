//! The slskd release the plugin needs, and how that release is read.
//!
//! slskd answers `GET /api/v0/application/version` with its semantic version.
//! The plugin drives the batch download API that slskd 0.26.0 introduced, so
//! everything older is refused in `validate-config` and in `health`.
//!
//! Everything here is pure: it never touches the network and never depends on
//! the plugin bindings beyond the shared error type, so it is unit-tested on
//! the host target.

use hearthsound_plugin::types::ProviderError;

/// The `(major, minor, patch)` of the oldest slskd release the plugin works
/// with, the release that introduced the batch download API.
pub const MINIMUM: (u64, u64, u64) = (0, 26, 0);

/// [`MINIMUM`] as it appears in a configuration problem or a health error.
pub const MINIMUM_SUPPORTED: &str = "0.26.0";

/// Parses the `major.minor.patch` prefix of a version string.
///
/// # Parameters
///
/// - `version`: a version string as slskd reports it, for example `0.26.1` or
///   `0.27.0-canary`.
///
/// # Returns
///
/// The `(major, minor, patch)` triple when all three components are present and
/// numeric, and `None` otherwise. A pre-release or build suffix is ignored, so
/// `0.26.0-alpha` parses as `0.26.0`.
///
/// # Example
///
/// ```text
/// parse("0.26.1")        -> Some((0, 26, 1))
/// parse("0.27.0-canary") -> Some((0, 27, 0))
/// parse("0.26")          -> None
/// ```
pub fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut components = version.trim().split(['.', '-', '+']);
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;

    Some((major, minor, patch))
}

/// Returns whether `version` is at least [`MINIMUM`].
///
/// # Parameters
///
/// - `version`: a version string as slskd reports it.
///
/// # Returns
///
/// `true` when the version parses and is not older than [`MINIMUM`], and
/// `false` when it parses as older or does not parse at all.
///
/// # Example
///
/// ```text
/// is_supported("0.26.0") -> true
/// is_supported("0.25.9") -> false
/// is_supported("nightly") -> false
/// ```
pub fn is_supported(version: &str) -> bool {
    parse(version).is_some_and(|parsed| parsed >= MINIMUM)
}

/// Reads the version out of a body from the version endpoint.
///
/// slskd answers with a JSON string (`"0.26.1"`); a bare text body is accepted
/// too, because an instance behind a reverse proxy may drop the JSON encoding.
///
/// # Parameters
///
/// - `body`: the raw response body.
///
/// # Returns
///
/// The version string, without its JSON quotes or surrounding whitespace.
///
/// # Errors
///
/// Returns [`ProviderError::Internal`] when the body carries no version-looking
/// text, which means the endpoint answered something other than its version.
pub fn from_body(body: &[u8]) -> Result<String, ProviderError> {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let version = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::String(value)) => value.trim().to_string(),
        _ => text.to_string(),
    };

    if parse(&version).is_some() {
        Ok(version)
    } else {
        Err(ProviderError::Internal(format!(
            "the slskd version endpoint answered `{version}`"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_numeric_components() {
        assert_eq!(parse("0.26.1"), Some((0, 26, 1)));
        assert_eq!(parse("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse(" 0.26.10 "), Some((0, 26, 10)));
    }

    #[test]
    fn ignores_a_pre_release_or_build_suffix() {
        assert_eq!(parse("0.27.0-canary"), Some((0, 27, 0)));
        assert_eq!(parse("0.26.0-alpha.1+build.7"), Some((0, 26, 0)));
    }

    #[test]
    fn rejects_versions_without_three_numeric_components() {
        assert_eq!(parse("0.26"), None);
        assert_eq!(parse("nightly"), None);
        assert_eq!(parse("0.26.x"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn supports_everything_from_the_minimum_up() {
        assert!(is_supported("0.26.0"));
        assert!(is_supported("0.26.1"));
        assert!(is_supported("1.0.0"));
        assert!(!is_supported("0.25.9"));
        assert!(!is_supported("unknown"));
    }

    #[test]
    fn treats_a_canary_build_as_its_release_version() {
        assert!(is_supported("0.26.0-canary"));
        assert!(is_supported("0.27.0-canary.4"));
        assert!(!is_supported("0.25.0-canary"));
    }

    #[test]
    fn reads_a_json_encoded_version() {
        assert_eq!(from_body(br#""0.26.1""#).unwrap(), "0.26.1");
        assert_eq!(from_body(b"\"  0.27.0 \"").unwrap(), "0.27.0");
    }

    #[test]
    fn reads_a_bare_version() {
        assert_eq!(from_body(b"0.26.1\n").unwrap(), "0.26.1");
    }

    #[test]
    fn rejects_a_body_that_is_not_a_version() {
        assert!(matches!(
            from_body(b"").unwrap_err(),
            ProviderError::Internal(message) if message.contains("version endpoint")
        ));
        assert!(matches!(
            from_body(b"<!doctype html>").unwrap_err(),
            ProviderError::Internal(_)
        ));
    }
}
