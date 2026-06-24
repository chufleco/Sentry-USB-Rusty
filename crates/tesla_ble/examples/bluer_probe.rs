//! bluer probe — task #338 spike.
//!
//! WHY THIS EXISTS
//! ---------------
//! btleplug's BLE connect path on the AIC8800 chip (Radxa Zero 3W,
//! OPi Zero 3W) hits HCI -38 ENOSYS on the connection-cancel opcode
//! (0x2039) when a connect attempt times out — so btleplug aborts
//! locally and returns `le-connection-abort-by-local`. Once that
//! happens, every retry fails the same way until the Pi reboots.
//!
//! This probe asks BlueZ for the connect operation via D-Bus (bluer
//! crate) instead of going through btleplug's HCI dance. BlueZ's
//! `Connect` method doesn't issue the connection-cancel opcode on
//! the failure path, so AIC8800 boards should be able to complete
//! the connect even when btleplug couldn't.
//!
//! WHAT THIS DOES
//! --------------
//! 1. Discovers the default BlueZ adapter.
//! 2. Starts a 30-second LE scan for a Tesla beacon (local name
//!    `S<16-hex>C`, computed from the VIN via SHA1).
//! 3. When the beacon is observed, asks BlueZ to connect to it.
//! 4. Reports success + how long each phase took, or the exact
//!    error if any phase failed.
//!
//! Three loops of scan→connect→disconnect, so we can see whether
//! a fresh connect attempt after a clean disconnect still works on
//! the chip (the reconnect path btleplug fails on).
//!
//! USAGE
//! -----
//!   sudo ./bluer_probe <VIN>
//!
//! Runs on the Pi directly. Cross-compile with:
//!   RUSTFLAGS="-C target-cpu=cortex-a53 -C target-feature=-aes,-sha2" \
//!     cross build --release --target aarch64-unknown-linux-gnu \
//!     -p sentryusb-tesla-ble --example bluer_probe
//!
//! Then scp the binary to /tmp/bluer_probe on the Pi and run.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bluer::{AdapterEvent, Address, Session};
use futures::StreamExt;
use sentryusb_tesla_ble::local_name::vehicle_local_name;

const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ITERATIONS: usize = 3;
const REST_BETWEEN_ITERATIONS: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,bluer=info".into()),
        )
        .with_target(false)
        .init();

    let vin = std::env::args()
        .nth(1)
        .context("usage: bluer_probe <VIN>")?;
    if vin.len() != 17 {
        anyhow::bail!("VIN must be 17 chars, got {}", vin.len());
    }
    let target_name = vehicle_local_name(&vin);
    tracing::info!(target_name = %target_name, vin_tail = %&vin[vin.len()-4..],
        "bluer_probe starting — looking for Tesla beacon");

    // Open the BlueZ session.
    let session = Session::new().await.context("Session::new (D-Bus connect)")?;
    let adapter = session.default_adapter().await.context("no default BlueZ adapter")?;
    adapter.set_powered(true).await.ok();
    tracing::info!(adapter = %adapter.name(), "adapter ready");

    let mut iteration_results: Vec<IterationResult> = Vec::new();
    for n in 1..=ITERATIONS {
        tracing::info!(iteration = n, "=== iteration {}/{} ===", n, ITERATIONS);
        let result = run_one_iteration(&adapter, &target_name).await;
        match &result {
            Ok(r) => tracing::info!(scan_ms = r.scan_ms, connect_ms = r.connect_ms,
                "iteration {} OK", n),
            Err(e) => tracing::warn!(error = ?e, "iteration {} FAILED", n),
        }
        iteration_results.push(IterationResult { iteration: n, result });

        if n < ITERATIONS {
            tracing::info!("resting {}s before next iteration", REST_BETWEEN_ITERATIONS.as_secs());
            tokio::time::sleep(REST_BETWEEN_ITERATIONS).await;
        }
    }

    println!("\n=== SUMMARY ===");
    let mut ok = 0;
    let mut fail = 0;
    for ir in &iteration_results {
        match &ir.result {
            Ok(r) => {
                ok += 1;
                println!("  iter {}: OK scan={}ms connect={}ms",
                    ir.iteration, r.scan_ms, r.connect_ms);
            }
            Err(e) => {
                fail += 1;
                println!("  iter {}: FAIL — {:#}", ir.iteration, e);
            }
        }
    }
    println!("\n  ok={ok} fail={fail} (of {ITERATIONS})");
    println!("\nVerdict: {}",
        if ok == ITERATIONS { "bluer connects cleanly on this chip — green light for full PersistentSession migration" }
        else if ok > 0 { "bluer mostly works but has gaps — log the errors above and investigate before committing" }
        else { "bluer no better than btleplug on this chip — pivot to hybrid fallback (#337)" }
    );
    Ok(())
}

struct OkResult {
    scan_ms: u128,
    connect_ms: u128,
}

struct IterationResult {
    iteration: usize,
    result: Result<OkResult>,
}

async fn run_one_iteration(adapter: &bluer::Adapter, target_name: &str) -> Result<OkResult> {
    // Phase 1 — scan for the named beacon.
    let scan_start = Instant::now();
    let address = scan_for_tesla(adapter, target_name).await?;
    let scan_ms = scan_start.elapsed().as_millis();
    tracing::info!(addr = ?address, scan_ms = scan_ms, "beacon located");

    // Phase 2 — connect via BlueZ Connect method.
    let connect_start = Instant::now();
    let device = adapter.device(address).context("device lookup")?;
    let connect_fut = device.connect();
    tokio::time::timeout(CONNECT_TIMEOUT, connect_fut)
        .await
        .context("connect timed out")?
        .context("BlueZ Connect failed")?;
    let connect_ms = connect_start.elapsed().as_millis();
    tracing::info!(connect_ms = connect_ms, "connected via BlueZ D-Bus");

    // Phase 3 — clean disconnect so the next iteration starts from
    // the same baseline as the sampler's reconnect cycle.
    let _ = device.disconnect().await;

    Ok(OkResult { scan_ms, connect_ms })
}

async fn scan_for_tesla(adapter: &bluer::Adapter, target_name: &str) -> Result<Address> {
    let mut events = adapter
        .discover_devices()
        .await
        .context("discover_devices")?;
    let deadline = tokio::time::sleep(SCAN_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                anyhow::bail!("scan timed out after {}s without seeing {}",
                    SCAN_TIMEOUT.as_secs(), target_name);
            }
            ev = events.next() => {
                let Some(ev) = ev else { anyhow::bail!("discovery stream ended unexpectedly"); };
                if let AdapterEvent::DeviceAdded(addr) = ev {
                    let device = adapter.device(addr).context("device lookup")?;
                    if let Ok(Some(name)) = device.name().await {
                        if name == target_name {
                            return Ok(addr);
                        }
                    }
                }
            }
        }
    }
}
