//! Phase 2 Push 1 verification: derive the session key for one Tesla
//! domain and compare to what tesla-control has cached for the same
//! pairing.
//!
//! What this proves: our ECDH + SHA-1 truncation produces a key that
//! the car will accept. If our derived key matches the bytes in
//! tesla-control's session-cache JSON, the next step (AES-GCM signing
//! in Push 2) is wiring up the rest of the protocol around a known-
//! good key. If they DON'T match, fixing it now is much cheaper than
//! debugging "decrypt failed on the car" later.
//!
//! Usage:
//!   cargo run --example derive_key -- \
//!     /root/.ble/key_private.pem \
//!     <vehicle-pubkey-hex>
//!
//! Where <vehicle-pubkey-hex> is the 130-character hex string
//! representing the 65-byte SEC1 uncompressed pubkey the car returned
//! in its SessionInfo response. The easiest way to get one is to run
//! the existing `session_info` example first — its output prints the
//! pubkey for both INFOTAINMENT and VEHICLE_SECURITY domains.
//!
//! You can then cross-check against tesla-control's session cache:
//!
//!   cat /backingfiles/tesla-session-cache.json | jq .
//!
//! The cache stores SessionInfo bytes per (VIN, domain). Decoding
//! the SessionInfo proto inside one of those entries gives you the
//! same `public_key` field — same vehicle pubkey we should be using
//! here. If our derived session key matches the car's expectation
//! (which we'll know for sure when Push 2 lands and `state climate`
//! either works or returns a decrypt error), Push 1 is correct.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sentryusb_tesla_ble::crypto::derive_session_key;
use sentryusb_tesla_ble::keys::KeyPair;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let key_path: PathBuf = args
        .next()
        .context("usage: derive_key <key_private.pem> <vehicle_pubkey_hex>")?
        .into();
    let vehicle_pub_hex = args
        .next()
        .context("missing vehicle pubkey hex (run `session_info` first to obtain)")?;

    let pub_bytes = hex::decode(vehicle_pub_hex.trim())
        .context("vehicle pubkey arg must be hex")?;
    if pub_bytes.len() != 65 {
        bail!(
            "expected 65-byte SEC1 pubkey (got {} bytes). \
             The first byte should be 0x04.",
            pub_bytes.len()
        );
    }
    if pub_bytes[0] != 0x04 {
        bail!(
            "vehicle pubkey doesn't start with 0x04 — got 0x{:02x}. \
             Tesla always returns the uncompressed SEC1 form.",
            pub_bytes[0]
        );
    }

    println!("Loading key:        {}", key_path.display());
    let keypair = KeyPair::load(&key_path)?;
    println!("Our pubkey:         {}", hex::encode(&keypair.pub_uncompressed));
    println!("Vehicle pubkey:     {}", hex::encode(&pub_bytes));
    println!();

    let session_key = derive_session_key(&keypair.secret, &pub_bytes)?;
    println!("Derived session key (AES-128, 16 bytes):");
    println!("  hex:  {}", hex::encode(session_key.as_bytes()));
    println!();
    println!("Done. If Push 2's `state climate` example fails with a");
    println!("decrypt error, the key derivation here is the first thing");
    println!("to suspect — re-run this command and verify the hex doesn't");
    println!("change between runs (it shouldn't — ECDH is deterministic).");

    Ok(())
}
