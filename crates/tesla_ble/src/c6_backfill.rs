//! C6_BACKFILL safety rule: the bench side-by-side mode (Pi and ESP32-C6 both
//! talking to the car) is only allowed when the C6 provably holds a DIFFERENT
//! key from the Pi. Proof = the C6 supervisor's provisioning marker naming a
//! public key that isn't the Pi's. Missing/unreadable marker or Pi key =
//! unknown = treated as shared = refused (normal one-at-a-time coordination).

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::keys::KeyPair;

/// Written by the C6 supervisor on each provision ack (pi-supervisor coord.rs).
pub const KEY_MARKER_PATH: &str = "/mutable/c6/provisioned_key.json";
/// The Pi sampler's Tesla key.
pub const PI_KEY_PATH: &str = "/root/.ble/key_private.pem";

/// sha256 hex of the Pi key's uncompressed public key.
pub fn pi_pubkey_sha256() -> Option<String> {
    let kp = KeyPair::load(Path::new(PI_KEY_PATH)).ok()?;
    Some(hex::encode(Sha256::digest(&kp.pub_uncompressed)))
}

/// Pure rule: allowed only if the marker names a key AND it isn't the Pi's.
pub fn allowed_in(marker_json: Option<&str>, pi_pubkey_sha256: Option<&str>) -> bool {
    let c6 = marker_json
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| v.get("pubkey_sha256").and_then(|x| x.as_str()).map(str::to_ascii_lowercase));
    let is_sha256 = |h: &str| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
    match (c6, pi_pubkey_sha256) {
        (Some(c6), Some(pi)) => is_sha256(&c6) && is_sha256(pi) && c6 != pi.to_ascii_lowercase(),
        _ => false,
    }
}

/// Is C6_BACKFILL permitted on this box right now?
pub fn backfill_allowed() -> bool {
    let marker = std::fs::read_to_string(KEY_MARKER_PATH).ok();
    allowed_in(marker.as_deref(), pi_pubkey_sha256().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(h: &str) -> String {
        format!(r#"{{"pubkey_sha256":"{h}"}}"#)
    }
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn shared_key_marker_refuses_backfill() {
        assert!(!allowed_in(Some(&marker(A)), Some(A)));
        assert!(!allowed_in(Some(&marker(&A.to_uppercase())), Some(A)), "case-insensitive");
    }

    #[test]
    fn separate_key_marker_allows_backfill() {
        assert!(allowed_in(Some(&marker(A)), Some(B)));
    }

    #[test]
    fn unknown_is_treated_as_shared() {
        assert!(!allowed_in(None, Some(B)), "no marker");
        assert!(!allowed_in(Some("garbage"), Some(B)), "unreadable marker");
        assert!(!allowed_in(Some(r#"{"other":1}"#), Some(B)), "marker without a key");
        assert!(!allowed_in(Some(&marker("")), Some(B)), "empty key");
        assert!(!allowed_in(Some(&marker("abc")), Some(B)), "not a sha256");
        assert!(!allowed_in(Some(&marker(A)), None), "Pi key unreadable");
    }
}
