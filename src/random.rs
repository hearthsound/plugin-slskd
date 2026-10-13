//! The random identifiers the plugin generates.
//!
//! slskd lets a caller name a search and a transfer batch, and a batch's
//! destination is `hearthsound/<key>` under the downloads directory, so both
//! identifiers have to be drawn by the plugin: the search id is a GUID slskd
//! parses, and the download key is a random, filesystem-safe path component.
//! Both come from the component's WASI random source, which the host provides.
//!
//! Everything here is pure: it never touches the network, so it is unit-tested
//! on the host target.

/// Returns a random version 4 UUID in the canonical hyphenated form.
///
/// # Returns
///
/// A lowercase GUID, for example `8f14e45f-ceea-467f-a1c8-9c5b0d1b4f3a`, which
/// is the shape slskd's `Guid` fields accept.
///
/// # Example
///
/// ```text
/// guid().len() -> 36
/// ```
pub fn guid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Returns a random key for one download.
///
/// # Returns
///
/// Thirty-two lowercase hexadecimal characters: 128 bits of randomness with no
/// character that needs escaping in a path, a URL or a log line.
///
/// # Example
///
/// ```text
/// key().len() -> 32
/// ```
pub fn key() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_is_a_parseable_version_4_uuid() {
        let guid = guid();
        let parsed = uuid::Uuid::parse_str(&guid).unwrap();

        assert_eq!(guid.len(), 36);
        assert_eq!(guid.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(guid, guid.to_lowercase());
    }

    #[test]
    fn key_is_hexadecimal_and_filesystem_safe() {
        let key = key();

        assert_eq!(key.len(), 32);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{key}"
        );
    }

    #[test]
    fn identifiers_differ_between_calls() {
        assert_ne!(guid(), guid());
        assert_ne!(key(), key());
    }
}
