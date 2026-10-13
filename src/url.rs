//! URL validation and composition for the slskd API.
//!
//! Everything here is pure: it never touches the network and never depends on
//! the plugin bindings, so it is unit-tested on the host target.

/// Returns whether `value` is an absolute `http` or `https` URL.
///
/// The check is deliberately structural rather than a full URL parse: it
/// accepts exactly the shapes an slskd base URL can have (a service name on the
/// server's network, a host with a port, or a reverse proxy in front of one)
/// and rejects everything a request could not be built from.
///
/// # Parameters
///
/// - `value`: the candidate URL.
///
/// # Returns
///
/// `true` when `value` starts with `http://` or `https://`, carries a non-empty
/// host, and contains no whitespace.
///
/// # Example
///
/// ```text
/// is_http_url("http://slskd:5030")   -> true
/// is_http_url("http://localhost:5030") -> true
/// is_http_url("slskd:5030")          -> false
/// is_http_url("ftp://slskd:5030")    -> false
/// ```
pub fn is_http_url(value: &str) -> bool {
    let value = value.trim();
    let after_scheme = match value.strip_prefix("https://") {
        Some(rest) => rest,
        None => match value.strip_prefix("http://") {
            Some(rest) => rest,
            None => return false,
        },
    };
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    !host.is_empty() && !host.contains(char::is_whitespace)
}

/// Normalizes a configured base URL.
///
/// Surrounding whitespace and trailing slashes are removed so that the
/// configured value and the URLs built from it do not double up on `/`.
///
/// # Parameters
///
/// - `value`: the configured base URL.
///
/// # Returns
///
/// The trimmed URL, without a trailing slash.
///
/// # Example
///
/// ```text
/// normalize_base_url(" http://slskd:5030/ ") -> "http://slskd:5030"
/// normalize_base_url("http://slskd:5030")   -> "http://slskd:5030"
/// ```
pub fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

/// Joins a base URL with an API path.
///
/// # Parameters
///
/// - `base_url`: the configured base URL, with or without a trailing slash.
/// - `path`: the API path, with or without a leading slash.
///
/// # Returns
///
/// The absolute URL of the endpoint.
///
/// # Example
///
/// ```text
/// endpoint("http://slskd:5030/", "/api/v0/searches") -> "http://slskd:5030/api/v0/searches"
/// ```
pub fn endpoint(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        normalize_base_url(base_url),
        path.trim_start_matches('/')
    )
}

/// Percent-encodes one path segment.
///
/// slskd's transfer routes carry a peer name in a path segment (`{username}`),
/// and peer names contain spaces, brackets and other characters a URL cannot
/// carry raw. Everything outside the RFC 3986 unreserved set is escaped,
/// including `/`, `?` and `#`, so a name cannot change the route it is pasted
/// into.
///
/// # Parameters
///
/// - `value`: the segment to encode.
///
/// # Returns
///
/// The encoded segment.
///
/// # Example
///
/// ```text
/// encode_segment("Some [Peer]") -> "Some%20%5BPeer%5D"
/// ```
pub fn encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(*byte));
            }
            _ => {
                encoded.push('%');
                encoded.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
            }
        }
    }
    encoded
}

/// Uppercase hexadecimal digits used by [`encode_segment`].
const HEX_DIGITS: [u8; 16] = *b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_and_https_urls() {
        assert!(is_http_url("http://slskd:5030"));
        assert!(is_http_url("https://slskd.internal"));
        assert!(is_http_url("https://proxy.example.org/slskd/"));
    }

    #[test]
    fn rejects_urls_without_a_host_or_scheme() {
        assert!(!is_http_url("slskd:5030"));
        assert!(!is_http_url("ftp://slskd:5030"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("https:///api/v0"));
        assert!(!is_http_url("http://slskd 5030"));
    }

    #[test]
    fn normalization_removes_surrounding_space_and_trailing_slashes() {
        assert_eq!(
            normalize_base_url("  http://slskd:5030/  "),
            "http://slskd:5030"
        );
        assert_eq!(normalize_base_url("http://slskd:5030"), "http://slskd:5030");
    }

    #[test]
    fn endpoint_joins_exactly_one_slash() {
        assert_eq!(
            endpoint("http://slskd:5030", "api/v0/searches"),
            "http://slskd:5030/api/v0/searches"
        );
        assert_eq!(
            endpoint("http://slskd:5030/", "/api/v0/searches"),
            "http://slskd:5030/api/v0/searches"
        );
    }

    #[test]
    fn encoding_escapes_reserved_characters() {
        assert_eq!(encode_segment("Some [Peer]"), "Some%20%5BPeer%5D");
        assert_eq!(encode_segment("a/b?c#d"), "a%2Fb%3Fc%23d");
        assert_eq!(encode_segment("peer_1.2-3~4"), "peer_1.2-3~4");
    }

    #[test]
    fn encoding_escapes_every_byte_of_a_name() {
        assert_eq!(encode_segment("_@user & friend"), "_%40user%20%26%20friend");
    }
}
