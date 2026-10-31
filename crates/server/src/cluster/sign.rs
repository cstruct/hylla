use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::{digest, hmac};

/// The header carrying a request's signature, as `<unix seconds>:<hex HMAC>`.
pub const HEADER: &str = "x-hylla-signature";

/// The header carrying a response's signature, as a hex HMAC.
pub const RESPONSE_HEADER: &str = "x-hylla-response-signature";

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

// r[impl cluster.clock-skew]
/// How far the timestamp of `signature` is from now, in seconds, when that's beyond what's
/// accepted: the clocks of the nodes differ, rather than the request being forged.
pub fn skew(signature: &str) -> Option<u64> {
    let timestamp = signature.split_once(':')?.0.parse::<u64>().ok()?;
    let skew = now().abs_diff(timestamp);
    (skew > MAX_AGE_SECS).then_some(skew)
}

impl Signer {
    // r[impl cluster.auth.responses]
    /// The signature header value for a response to the request signed `request`.
    ///
    /// It covers the request's signature, so it can't answer another request, and the
    /// response's status and body.
    pub fn sign_response(&self, request: &str, status: u16, body: &[u8]) -> String {
        hex(hmac::sign(&self.key, &response_message(request, status, body)).as_ref())
    }

    /// Whether `signature` is valid for a response to the request signed `request`.
    pub fn verify_response(
        &self,
        signature: &str,
        request: &str,
        status: u16,
        body: &[u8],
    ) -> bool {
        let Some(tag) = unhex(signature) else {
            return false;
        };
        hmac::verify(&self.key, &response_message(request, status, body), &tag).is_ok()
    }
}

fn response_message(request: &str, status: u16, body: &[u8]) -> Vec<u8> {
    let body_digest = digest::digest(&digest::SHA256, body);
    format!(
        "response\n{request}\n{status}\n{}",
        hex(body_digest.as_ref())
    )
    .into_bytes()
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
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Signer, now, skew};

    // r[verify cluster.clock-skew]
    #[test]
    fn signatures_from_skewed_clocks_are_told_apart() {
        let signer = Signer::new("a key of at least thirty-two characters");
        let current = signer.sign("POST", "/cluster/gossip", b"{}");
        assert_eq!(skew(&current), None);
        let behind = signer.sign_at(now() - 300, "POST", "/cluster/gossip", b"{}");
        assert!(skew(&behind).is_some_and(|skew| (299..=301).contains(&skew)));
        let ahead = signer.sign_at(now() + 120, "POST", "/cluster/gossip", b"{}");
        assert!(skew(&ahead).is_some());
        assert_eq!(skew("garbage"), None);
    }

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

    // r[verify cluster.auth.responses]
    #[test]
    fn response_signatures_cover_the_request_status_and_body() {
        let signer = Signer::new("a key of at least thirty-two characters");
        let request = signer.sign("POST", "/cluster/gossip", b"{}");
        let signature = signer.sign_response(&request, 200, b"{\"members\":[]}");
        assert!(signer.verify_response(&signature, &request, 200, b"{\"members\":[]}"));
        assert!(!signer.verify_response(&signature, &request, 200, b"{}"));
        assert!(!signer.verify_response(&signature, &request, 404, b"{\"members\":[]}"));
        let other_request = signer.sign("POST", "/cluster/updates", b"{}");
        assert!(!signer.verify_response(&signature, &other_request, 200, b"{\"members\":[]}"));
        let other = Signer::new("another key of at least thirty-two characters");
        assert!(!other.verify_response(&signature, &request, 200, b"{\"members\":[]}"));
        assert!(!signer.verify_response("zz", &request, 200, b"{\"members\":[]}"));
    }
}
