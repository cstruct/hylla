use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::{digest, hmac};

/// The header carrying a request's signature, as `<unix seconds>:<hex HMAC>`.
pub const HEADER: &str = "x-hylla-signature";

/// How far a signature's timestamp may be from now.
const MAX_AGE_SECS: u64 = 60;

// r[impl cluster.auth]
/// Signs and verifies requests between nodes with the shared cluster key.
///
/// A signature covers the method, path and query, a timestamp and a SHA-256 of the body, so
/// the key itself never crosses the network, and signatures can't be reused for other
/// requests or replayed after a minute.
pub struct Signer {
    key: hmac::Key,
}

impl Signer {
    pub fn new(key: &str) -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes()),
        }
    }

    /// The signature header value for a request.
    pub fn sign(&self, method: &str, path_and_query: &str, body: &[u8]) -> String {
        self.sign_at(now(), method, path_and_query, body)
    }

    fn sign_at(&self, timestamp: u64, method: &str, path_and_query: &str, body: &[u8]) -> String {
        let tag = hmac::sign(&self.key, &message(timestamp, method, path_and_query, body));
        format!("{timestamp}:{}", hex(tag.as_ref()))
    }

    /// Whether `signature` is valid for a request.
    pub fn verify(&self, signature: &str, method: &str, path_and_query: &str, body: &[u8]) -> bool {
        let Some((timestamp, tag)) = signature.split_once(':') else {
            return false;
        };
        let (Ok(timestamp), Some(tag)) = (timestamp.parse::<u64>(), unhex(tag)) else {
            return false;
        };
        if now().abs_diff(timestamp) > MAX_AGE_SECS {
            return false;
        }
        hmac::verify(
            &self.key,
            &message(timestamp, method, path_and_query, body),
            &tag,
        )
        .is_ok()
    }
}

fn message(timestamp: u64, method: &str, path_and_query: &str, body: &[u8]) -> Vec<u8> {
    let body_digest = digest::digest(&digest::SHA256, body);
    format!(
        "{timestamp}\n{method}\n{path_and_query}\n{}",
        hex(body_digest.as_ref())
    )
    .into_bytes()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Signer, now};

    // r[verify cluster.auth]
    #[test]
    fn signatures_cover_the_request_and_expire() {
        let signer = Signer::new("a key of at least thirty-two characters");
        let signature = signer.sign("POST", "/cluster/gossip", b"{}");
        assert!(signer.verify(&signature, "POST", "/cluster/gossip", b"{}"));
        // Any change to the request invalidates it.
        assert!(!signer.verify(&signature, "GET", "/cluster/gossip", b"{}"));
        assert!(!signer.verify(&signature, "POST", "/cluster/updates", b"{}"));
        assert!(!signer.verify(&signature, "POST", "/cluster/gossip", b"{ }"));
        // So does another key.
        let other = Signer::new("another key of at least thirty-two characters");
        assert!(!other.verify(&signature, "POST", "/cluster/gossip", b"{}"));
        // And age.
        let old = signer.sign_at(now() - 120, "POST", "/cluster/gossip", b"{}");
        assert!(!signer.verify(&old, "POST", "/cluster/gossip", b"{}"));
        assert!(!signer.verify("garbage", "POST", "/cluster/gossip", b"{}"));
        assert!(!signer.verify("1:zz", "POST", "/cluster/gossip", b"{}"));
    }
}
