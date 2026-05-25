//! Scan + connect + run a body-controller-state query (no crypto).
//!
//! Usage:
//!   cargo run --example body_controller -- <VIN>

use std::time::Duration;

use anyhow::Context;
use sentryusb_tesla_ble::{body_controller, gatt::Connection, scan};

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
        .context("usage: cargo run --example body_controller -- <VIN>")?;
    if vin.len() != 17 {
        anyhow::bail!("VIN must be 17 characters, got {}", vin.len());
    }

    let adapter = scan::first_adapter().await?;
    let target = scan::scan_for_vin(&adapter, &vin, Duration::from_secs(30)).await?;
    let mut conn = Connection::open(target.peripheral).await?;
    let status = body_controller::query(&mut conn).await?;
    conn.close().await;

    println!();
    println!("Body Controller State:");
    println!("  vehicle_lock_state:   {}", status.vehicle_lock_state);
    println!("  vehicle_sleep_status: {}", status.vehicle_sleep_status);
    println!("  user_presence:        {}", status.user_presence);
    if let Some(c) = &status.closure_statuses {
        println!("  closures:");
        println!("    front driver door: {}", c.front_driver_door);
        println!("    front passenger:   {}", c.front_passenger_door);
        println!("    rear driver door:  {}", c.rear_driver_door);
        println!("    rear passenger:    {}", c.rear_passenger_door);
        println!("    rear trunk:        {}", c.rear_trunk);
        println!("    front trunk:       {}", c.front_trunk);
        println!("    charge port:       {}", c.charge_port);
    }
    Ok(())
}
