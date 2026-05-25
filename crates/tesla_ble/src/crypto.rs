//! Tesla vehicle-command crypto primitives.
//!
//! This is Push 1 of Phase 2: the ECDH-derived session-key half.
//! AES-GCM signing comes in Push 2, once we've verified key derivation
//! matches what tesla-control produces.
//!
//! **Key derivation per Tesla's vehicle-command spec**
//! (`pkg/protocol/protocol/session.go` in their open-source repo):
//!
//! 1. ECDH: own private scalar × vehicle ephemeral public key →
//!    shared point. Take the 32-byte X coordinate.
//! 2. Hash: SHA-1(X). Take the first 16 bytes.
//! 3. That 16-byte value is the AES-128 session key used for
//!    `AES_GCM_PERSONALIZED` signatures on every signed command.
//!
//! The use of SHA-1 is intentional and matches Tesla's spec exactly —
//! it's not collision-bearing here, it's just being used as a key-
//! derivation hash. Don't "upgrade" this to SHA-256 without first
//! confirming Tesla changed the wire format, or the car will reject
//! every command with a decryption failure.

use anyhow::{Context, Result};
use p256::PublicKey;
use p256::SecretKey;
use p256::ecdh::diffie_hellman;
use sha1::{Digest, Sha1};

/// Length of the derived AES-GCM session key in bytes (AES-128).
pub const SESSION_KEY_LEN: usize = 16;

/// The 16-byte symmetric session key derived from an ECDH exchange
/// with one Tesla vehicle domain.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKey(pub [u8; SESSION_KEY_LEN]);

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't log the full key to journals; show length only.
        write!(f, "SessionKey({} bytes)", self.0.len())
    }
}

impl SessionKey {
    pub fn as_bytes(&self) -> &[u8; SESSION_KEY_LEN] {
        &self.0
    }
}

/// Derive the AES-128-GCM session key for one Tesla domain.
///
/// `our_secret` is the user's BLE private key (loaded from
/// `/root/.ble/key_private.pem`). `vehicle_pubkey_sec1` is the 65-byte
/// uncompressed SEC1 public key the car returned in its `SessionInfo`
/// response (`0x04 || X(32) || Y(32)`).
///
/// Errors:
/// * vehicle pubkey doesn't decode as a valid P-256 point (car sent
///   garbage or we got the wrong domain's session-info back)
/// * ECDH multiplication produces the point at infinity (shouldn't
///   happen with valid inputs but the p256 crate flags it)
pub fn derive_session_key(
    our_secret: &SecretKey,
    vehicle_pubkey_sec1: &[u8],
) -> Result<SessionKey> {
    let vehicle_pub = PublicKey::from_sec1_bytes(vehicle_pubkey_sec1)
        .context("decode vehicle SEC1 public key")?;
    // diffie_hellman gives us the shared secret as a SharedSecret
    // newtype around the X coordinate. raw_secret_bytes() exposes the
    // 32 raw bytes we need to hash.
    let shared = diffie_hellman(our_secret.to_nonzero_scalar(), vehicle_pub.as_affine());
    let x_bytes = shared.raw_secret_bytes();

    // SHA-1 of the X coordinate, truncated to 16 bytes = AES-128 key.
    let mut hasher = Sha1::new();
    hasher.update(x_bytes);
    let digest = hasher.finalize();

    let mut key = [0u8; SESSION_KEY_LEN];
    key.copy_from_slice(&digest[..SESSION_KEY_LEN]);
    Ok(SessionKey(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip test: two keypairs, each derives the same shared
    /// session key from the other's public side. Proves the ECDH +
    /// SHA-1 chain is internally consistent without needing a captured
    /// real-world vector.
    #[test]
    fn ecdh_session_key_is_symmetric() {
        use p256::elliptic_curve::rand_core::OsRng;

        let alice = SecretKey::random(&mut OsRng);
        let bob = SecretKey::random(&mut OsRng);

        let alice_pub = alice.public_key().to_sec1_bytes().to_vec();
        let bob_pub = bob.public_key().to_sec1_bytes().to_vec();

        let k_alice = derive_session_key(&alice, &bob_pub).unwrap();
        let k_bob = derive_session_key(&bob, &alice_pub).unwrap();

        assert_eq!(
            k_alice.as_bytes(),
            k_bob.as_bytes(),
            "ECDH session key must be symmetric"
        );
        assert_eq!(k_alice.as_bytes().len(), SESSION_KEY_LEN);
    }

    /// Determinism: same inputs always produce the same key. Catches
    /// the "accidentally read uninitialized memory" or "RNG snuck in"
    /// regression.
    #[test]
    fn derive_session_key_is_deterministic() {
        use p256::elliptic_curve::rand_core::OsRng;

        let our = SecretKey::random(&mut OsRng);
        let their = SecretKey::random(&mut OsRng);
        let their_pub = their.public_key().to_sec1_bytes().to_vec();

        let k1 = derive_session_key(&our, &their_pub).unwrap();
        let k2 = derive_session_key(&our, &their_pub).unwrap();
        assert_eq!(k1.as_bytes(), k2.as_bytes());
    }

    /// Garbage vehicle pubkey is rejected, not silently returning
    /// a key derived from an attacker-controlled point at infinity.
    #[test]
    fn rejects_invalid_vehicle_pubkey() {
        use p256::elliptic_curve::rand_core::OsRng;

        let our = SecretKey::random(&mut OsRng);
        let bad = vec![0u8; 65]; // all-zeros isn't a valid SEC1 point
        assert!(derive_session_key(&our, &bad).is_err());
    }
}
