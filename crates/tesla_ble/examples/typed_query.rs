//! Persistent session + typed response decoding.
//!
//! Runs one round of climate / charge / drive / tire-pressure queries
//! through the in-process Rust path and prints decoded field values —
//! exercises the full chain: scan → connect → handshake → signed query
//! → encrypted response → decrypt → proto decode → typed fields.

use std::path::PathBuf;

use anyhow::Context;
use sentryusb_tesla_ble::{keys::KeyPair, manager::PersistentSession};

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
        .context("usage: typed_query <VIN> <key.pem>")?;
    let key_path: PathBuf = std::env::args()
        .nth(2)
        .context("usage: typed_query <VIN> <key.pem>")?
        .into();
    if vin.len() != 17 {
        anyhow::bail!("VIN must be 17 chars, got {}", vin.len());
    }

    let keypair = KeyPair::load(&key_path)?;
    // None = let btleplug pick the first adapter (host default).
    // Tests that need a specific adapter would pass Some("hci1").
    let session = PersistentSession::start(keypair, vin, None);

    println!("\n--- climate ---");
    match session.get_climate().await {
        Ok(c) => {
            println!("  inside_temp_c:        {:?}", c.optional_inside_temp_celsius);
            println!("  outside_temp_c:       {:?}", c.optional_outside_temp_celsius);
            println!("  driver_temp_setting:  {:?}", c.optional_driver_temp_setting);
            println!("  is_climate_on:        {:?}", c.optional_is_climate_on);
            println!("  fan_status:           {:?}", c.optional_fan_status);
        }
        Err(e) => println!("  FAIL: {e:#}"),
    }

    println!("\n--- charge ---");
    match session.get_charge().await {
        Ok(c) => {
            println!("  battery_level:        {:?}", c.optional_battery_level);
            println!("  usable_battery_level: {:?}", c.optional_usable_battery_level);
            println!("  charge_limit_soc:     {:?}", c.optional_charge_limit_soc);
            println!("  charger_voltage:      {:?}", c.optional_charger_voltage);
            println!("  battery_range:        {:?}", c.optional_battery_range);
        }
        Err(e) => println!("  FAIL: {e:#}"),
    }

    println!("\n--- drive ---");
    match session.get_drive().await {
        Ok(d) => {
            println!("  shift_state:          {:?}", d.shift_state);
            println!("  speed:                {:?}", d.optional_speed);
            println!("  timestamp:            {:?}", d.timestamp);
        }
        Err(e) => println!("  FAIL: {e:#}"),
    }

    println!("\n--- tire pressure ---");
    match session.get_tire_pressure().await {
        Ok(t) => {
            println!("  tpms_pressure_fl:     {:?}", t.optional_tpms_pressure_fl);
            println!("  tpms_pressure_fr:     {:?}", t.optional_tpms_pressure_fr);
            println!("  tpms_pressure_rl:     {:?}", t.optional_tpms_pressure_rl);
            println!("  tpms_pressure_rr:     {:?}", t.optional_tpms_pressure_rr);
        }
        Err(e) => println!("  FAIL: {e:#}"),
    }

    session.shutdown().await;
    Ok(())
}
