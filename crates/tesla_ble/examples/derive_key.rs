//! Derive the session key for one Tesla domain and compare it to what
//! tesla-control has cached for the same pairing — proves our ECDH +
//! SHA-1 truncation produces a key the car accepts.
//!
//! Usage:
//!   cargo run --example derive_key -- \
//!     /root/.ble/key_private.pem \
//!     <vehicle-pubkey-hex>
//!
//! <vehicle-pubkey-hex> is the 130-char hex of the 65-byte SEC1 pubkey
//! the car returned in its SessionInfo (run the `session_info` example
//! to get it). Cross-check against tesla-control's cache:
//!
//!   cat /backingfiles/tesla-session-cache.json | jq .
//!
//! Decoding the SessionInfo proto in an entry gives the same
//! `public_key`; a matching derived key means we're correct.

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
