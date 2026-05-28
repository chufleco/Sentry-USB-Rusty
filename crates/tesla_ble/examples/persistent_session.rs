//! Run a persistent BLE session that holds the GATT connection across
//! many state queries.
//!
//! What this proves:
//!   * First query incurs the scan + handshake cost (~1-3s).
//!   * Subsequent queries reuse the connection + session keys, so
//!     they should land in the ~200-500ms range — close to the
//!     pure BLE round-trip time.
//!   * The connection stays held while we sleep between queries.
//!     Phones can connect/disconnect in the remaining slots without
//!     bumping us off.
//!
//! Usage:
//!   sudo ./persistent_session <VIN> <key.pem> [interval_secs] [iterations]
//!
//! Default interval = 15 s (matches the sampler's Active-mode cadence).
//! Default iterations = forever (Ctrl-C to stop).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use sentryusb_tesla_ble::{
    keys::KeyPair,
    manager::PersistentSession,
    proto::universal_message::Domain,
    state_query::VehicleDataState,
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
        .context("usage: persistent_session <VIN> <key.pem> [interval_secs] [iterations]")?;
    let key_path: PathBuf = std::env::args()
        .nth(2)
        .context("usage: persistent_session <VIN> <key.pem> [interval_secs] [iterations]")?
        .into();
    let interval_secs: u64 = std::env::args()
        .nth(3)
        .as_deref()
        .map(str::parse)
        .transpose()?
        .unwrap_or(15);
    let iterations: Option<u32> = std::env::args()
        .nth(4)
        .as_deref()
        .map(str::parse)
        .transpose()?;

    if vin.len() != 17 {
        anyhow::bail!("VIN must be 17 chars, got {}", vin.len());
    }

    let keypair = KeyPair::load(&key_path)?;
    println!("Loaded key, starting persistent session for VIN {}…{}",
             &vin[..3], &vin[vin.len()-4..]);

    let session = PersistentSession::start(keypair, vin, None);

    // Iterate climate → charge → drive → tire-pressure on each tick.
    // Same workload shape the telemetry sampler runs.
    let states = [
        VehicleDataState::Climate,
        VehicleDataState::Charge,
        VehicleDataState::Drive,
        VehicleDataState::TirePressure,
    ];

    let mut iter = 0u32;
    loop {
        iter += 1;
        println!("\n--- iteration {} ---", iter);
        for state in states {
            let started = Instant::now();
            match session.query(Domain::Infotainment, state).await {
                Ok(plain) => {
                    let elapsed = started.elapsed();
                    println!(
                        "  {:?}: OK in {:>5} ms  ({} bytes)",
                        state,
                        elapsed.as_millis(),
                        plain.len()
                    );
                }
                Err(e) => {
                    let elapsed = started.elapsed();
                    println!(
                        "  {:?}: FAIL in {:>5} ms — {:#}",
                        state,
                        elapsed.as_millis(),
                        e
                    );
                }
            }
            // Small inter-query gap so we don't pound the bluez queue.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if let Some(n) = iterations {
            if iter >= n {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }

    session.shutdown().await;
    Ok(())
}
