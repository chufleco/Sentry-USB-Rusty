//! SessionInfoRequest → SessionInfo handshake.
//!
//! Usage:
//!   sudo ./session_info <VIN> <path-to-key.pem>
//!
//! Sends SessionInfoRequest to both VEHICLE_SECURITY and INFOTAINMENT
//! domains and prints the SessionInfo each returns.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use sentryusb_tesla_ble::{
    gatt::Connection, keys::KeyPair, proto::universal_message::Domain, scan, session,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,btleplug=warn".into()),
        )
        .with_target(false)
        .init();

    let vin = std::env::args()
        .nth(1)
        .context("usage: session_info <VIN> <path-to-key.pem>")?;
    let key_path: PathBuf = std::env::args()
        .nth(2)
        .context("usage: session_info <VIN> <path-to-key.pem>")?
        .into();
    if vin.len() != 17 {
        anyhow::bail!("VIN must be 17 characters, got {}", vin.len());
    }

    let keypair = KeyPair::load(&key_path)?;
    println!(
        "Loaded NIST P-256 key (pub: {} bytes, starts with 0x{:02x})",
        keypair.pub_uncompressed.len(),
        keypair.pub_uncompressed[0]
    );

    let adapter = scan::first_adapter().await?;
    let target = scan::scan_for_vin(&adapter, &vin, Duration::from_secs(30)).await?;
    let mut conn = Connection::open(target.peripheral).await?;

    for domain in [Domain::VehicleSecurity, Domain::Infotainment] {
        let resp = session::request_session_info(&mut conn, &keypair, domain).await?;
        println!();
        println!("=== {:?} ===", resp.domain);
        println!("  counter:    {}", resp.parsed.counter);
        println!("  clock_time: {}", resp.parsed.clock_time);
        println!("  epoch:      {} bytes", resp.parsed.epoch.len());
        println!(
            "  vehicle pubkey: {} bytes (starts with 0x{:02x})",
            resp.parsed.public_key.len(),
            resp.parsed.public_key.first().copied().unwrap_or(0)
        );
        println!("  vehicle pubkey hex: {}", hex::encode(&resp.parsed.public_key));
        println!("  raw SessionInfo bytes: {}", resp.raw.len());
    }

    conn.close().await;
    Ok(())
}
