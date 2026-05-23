//! `sentryusb-tesla-telemetry` — BLE telemetry sampler daemon.
//!
//! Runs as a systemd service alongside `sentryusb.service`. Watches
//! the USB gadget LUN for clip writes (car-awake signal), takes
//! samples via `tesla-control`, and inserts them into the
//! `telemetry_samples` table.
//!
//! Design notes:
//!   * Sampling rate adapts to car state — 15 s while awake, 15 min
//!     while asleep (using the non-waking `body-controller-state`).
//!   * Holds the `/tmp/ble_radio_owner` lock while sampling so the
//!     keep-awake nudge and iOS GATT daemon serialize cleanly.
//!   * Stops `sentryusb-ble.service` (iOS GATT) while the lock is
//!     held, restarts it on release.
//!   * Re-reads `sentryusb.conf` on every loop iteration — toggling
//!     BLE off in settings stops sampling within ~15 s without a
//!     daemon restart.

mod config;
mod db;
mod lock;
mod sample;
mod usb_watch;

use std::time::Duration;

use anyhow::Result;
use rusqlite::Connection;
use tracing::{debug, error, info, warn};

use crate::config::BleConfig;
use crate::sample::Sample;
use crate::usb_watch::CarState;

/// Lock-owner string this daemon writes into `/tmp/ble_radio_owner`.
/// Coordinated with `awake_start`'s owner string ("keep_awake").
const OWNER: &str = "telemetry";

/// Sample cadence while the car is awake. Storage cost is ~12 KB/h
/// per the user's design call.
const AWAKE_INTERVAL: Duration = Duration::from_secs(15);

/// Sample cadence while the car is asleep, after the warm-up ramp.
/// Uses `body-controller-state` which doesn't wake the car.
const ASLEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Minimum gap between two `state software-update` fetches. The OS
/// version only changes on OTAs, so every-cycle polling would waste
/// BLE air time and force the car awake more often than needed. We
/// re-check every 15 min so a drive that starts shortly after an OTA
/// still gets the new version recorded.
const SOFTWARE_VERSION_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Asleep-mode backoff for the first few attempts after the daemon
/// starts (or after the state flips Awake → Asleep). Goal: give the
/// user feedback within a minute of pairing without spamming the
/// radio long-term.
///
/// Attempt n → interval:
///   1 → 30 s,  2 → 60 s,  3 → 2 min,  4 → 5 min,  5+ → ASLEEP_INTERVAL
fn asleep_backoff(attempt: usize) -> Duration {
    match attempt {
        0 | 1 => Duration::from_secs(30),
        2 => Duration::from_secs(60),
        3 => Duration::from_secs(120),
        4 => Duration::from_secs(300),
        _ => ASLEEP_INTERVAL,
    }
}

/// How long to sleep when we can't take the BLE radio (some other
/// owner holds the lock). Short so we resume quickly when the
/// keep-awake nudge releases.
const RADIO_CONTENDED_BACKOFF: Duration = Duration::from_secs(5);

/// How long to sleep when BLE is disabled in settings. Doesn't need
/// to be aggressive — settings changes are infrequent.
const DISABLED_POLL: Duration = Duration::from_secs(60);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    info!("sentryusb-tesla-telemetry starting");

    let conn = db::open()?;
    let mut held_radio = false;
    // Counts consecutive Asleep ticks since the last Awake/Idle
    // observation. Drives `asleep_backoff` so the first few attempts
    // after pairing or after the car goes to sleep are fast (30s, 60s,
    // 2m, 5m) before settling at the 15-min steady-state cadence.
    let mut asleep_attempts: usize = 0;
    // Unix seconds of the last successful (or attempted) software
    // version fetch. Drives the SOFTWARE_VERSION_INTERVAL throttle.
    // 0 = never fetched, so the first awake sample includes it.
    let mut last_software_check: i64 = 0;

    // SIGTERM handler — release the radio on shutdown so the iOS
    // GATT daemon can come back up cleanly.
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    let mut sigint = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::interrupt(),
    )?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("SIGTERM received, releasing radio and exiting");
                if held_radio { release_radio().await; }
                return Ok(());
            }
            _ = sigint.recv() => {
                info!("SIGINT received, releasing radio and exiting");
                if held_radio { release_radio().await; }
                return Ok(());
            }
            sleep = tick(&conn, &mut held_radio, &mut asleep_attempts, &mut last_software_check) => {
                tokio::time::sleep(sleep).await;
            }
        }
    }
}

/// One iteration of the main loop. Returns the duration to sleep
/// before the next iteration. `asleep_attempts` tracks consecutive
/// Asleep ticks for the warm-up backoff and gets reset whenever the
/// car is observed Awake or Idle.
async fn tick(
    conn: &Connection,
    held_radio: &mut bool,
    asleep_attempts: &mut usize,
    last_software_check: &mut i64,
) -> Duration {
    let cfg = match BleConfig::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to load BLE config: {e}");
            return DISABLED_POLL;
        }
    };

    if !cfg.enabled {
        if *held_radio {
            info!("BLE disabled in settings — releasing radio");
            release_radio().await;
            *held_radio = false;
        }
        *asleep_attempts = 0;
        return DISABLED_POLL;
    }
    if cfg.vin.is_empty() {
        debug!("no TESLA_BLE_VIN configured, idling");
        if *held_radio {
            release_radio().await;
            *held_radio = false;
        }
        *asleep_attempts = 0;
        return DISABLED_POLL;
    }

    let state = usb_watch::observe();

    match state {
        CarState::Awake | CarState::Idle => {
            // Car is producing clip writes → reset the warm-up
            // counter so the next Asleep transition starts fast
            // again rather than jumping straight to 15-min.
            *asleep_attempts = 0;
            // Need the radio. Try to acquire.
            if !*held_radio {
                match lock::try_acquire(OWNER) {
                    Ok(true) => {
                        *held_radio = true;
                        stop_ios_gatt().await;
                    }
                    Ok(false) => {
                        debug!(
                            "radio held by {:?}, backing off",
                            lock::current_owner()
                        );
                        return RADIO_CONTENDED_BACKOFF;
                    }
                    Err(e) => {
                        warn!("failed to acquire radio lock: {e}");
                        return RADIO_CONTENDED_BACKOFF;
                    }
                }
            }

            // Throttle the software-update fetch: every cycle is
            // wasteful (OS version only changes on OTAs), so only
            // include it once per SOFTWARE_VERSION_INTERVAL.
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let include_software = now_ts - *last_software_check
                >= SOFTWARE_VERSION_INTERVAL.as_secs() as i64;
            match sample::sample_state(&cfg.vin, include_software).await {
                Ok(s) => {
                    if include_software {
                        // Bump the gate even on a None result so we
                        // don't retry the (possibly slow) call every
                        // cycle when the car doesn't expose it.
                        *last_software_check = now_ts;
                    }
                    persist(conn, s);
                }
                Err(e) => {
                    warn!("sample_state failed: {e}");
                    // Keep the radio — transient failure (car
                    // briefly out of range, BLE jitter). If
                    // failures persist the next clip-write probe
                    // will eventually flip us to Asleep.
                }
            }
            AWAKE_INTERVAL
        }
        CarState::Asleep => {
            // Briefly take the radio for one sample, then release
            // so iOS GATT can come back.
            let acquired = if *held_radio {
                true
            } else {
                match lock::try_acquire(OWNER) {
                    Ok(true) => {
                        stop_ios_gatt().await;
                        true
                    }
                    Ok(false) => {
                        debug!("radio contended during asleep sample, skipping");
                        false
                    }
                    Err(e) => {
                        warn!("failed to acquire radio lock for asleep sample: {e}");
                        false
                    }
                }
            };

            if acquired {
                match sample::sample_body_controller(&cfg.vin).await {
                    Ok(s) => persist(conn, s),
                    Err(e) => warn!("sample_body_controller failed: {e}"),
                }
                release_radio().await;
                *held_radio = false;
            }
            *asleep_attempts = asleep_attempts.saturating_add(1);
            asleep_backoff(*asleep_attempts)
        }
    }
}

fn persist(conn: &Connection, sample: Sample) {
    let ts = sample.ts;
    let source = sample.source.clone();
    if let Err(e) = db::insert(conn, &sample) {
        error!("failed to insert telemetry sample (ts={ts}): {e}");
    } else {
        debug!("inserted telemetry sample (ts={ts}, source={source})");
    }
}

/// Stop the iOS GATT daemon (`sentryusb-ble.service`) so this
/// daemon has exclusive `hci0` access. Best-effort — if systemctl
/// fails, log and continue; the tesla-control call will surface a
/// real BLE error if there's actual contention.
async fn stop_ios_gatt() {
    debug!("stopping sentryusb-ble for telemetry session");
    let _ = sentryusb_shell::run("systemctl", &["stop", "sentryusb-ble"]).await;
}

/// Restart the iOS GATT daemon and clear our radio-lock entry.
/// Called on radio release transitions and SIGTERM.
async fn release_radio() {
    let _ = sentryusb_shell::run("systemctl", &["start", "sentryusb-ble"]).await;
    if let Err(e) = lock::release(OWNER) {
        warn!("failed to release radio lock: {e}");
    }
}
