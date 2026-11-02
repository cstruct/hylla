//! Names of builds, aliases and wildcard hosts, shared by validation and serving.

/// Whether `label` is the SHA of a build: 40 (SHA-1) or 64 (SHA-256) lowercase hexadecimal
/// characters.
pub fn is_sha(label: &str) -> bool {
    matches!(label.len(), 40 | 64)
        && label
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// r[impl site.alias-names]
/// Whether `name` can name an alias: 1 to 63 lowercase letters, digits and `-`, not starting
/// or ending with `-`, so it can be a host label, and not a SHA, so it can't be taken for one.
pub fn is_alias_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !is_sha_shaped(name)
}

/// Whether `name` is 40 or 64 hexadecimal characters, of either case.
fn is_sha_shaped(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The host name a wildcard server name such as `*.preview.example.com` matches one label in
/// front of, or `None` for a name without a wildcard.
pub fn wildcard_suffix(server_name: &str) -> Option<&str> {
    server_name.strip_prefix("*.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shas_are_full_lowercase_hex() {
        assert!(is_sha(&"a".repeat(40)));
        assert!(is_sha(&"0f".repeat(32)));
        assert!(!is_sha(&"a".repeat(39)));
        assert!(!is_sha(&"A".repeat(40)));
        assert!(!is_sha(&"g".repeat(40)));
    }

    // r[verify site.alias-names]
    #[test]
    fn alias_names_are_host_labels_that_arent_shas() {
        for name in ["live", "pr1873", "a", "feature-x", &"a".repeat(63)] {
            assert!(is_alias_name(name), "{name}");
        }
        for name in [
            "",
            "-live",
            "live-",
            "Live",
            "pr_1",
            "a.b",
            &"a".repeat(64),
            &"a".repeat(40),
            &"A".repeat(40),
        ] {
            assert!(!is_alias_name(name), "{name}");
        }
    }
}
