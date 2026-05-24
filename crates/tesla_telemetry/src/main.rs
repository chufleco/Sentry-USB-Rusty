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

/// Sample cadence for sleep-safe `body-controller-state` calls.
/// Set to 1 min so the sampler notices a drive starting within a
/// minute of the car coming out of sleep — body-controller-state
/// doesn't wake the car, so polling this often is cheap from a
/// battery-drain perspective. Replaces the old 15-min interval +
/// ramp-up backoff (which made drive starts invisible for up to
/// 15 min after sleep).
const QUIET_INTERVAL: Duration = Duration::from_secs(60);

/// How many consecutive state polls must show shift_state = Park
/// before we drop into the sleep-safe Quiet mode. 3 polls @ 15s =
/// 45 s of confirmed Park before we stop hammering the car. Keeps
/// us in Drive mode through a brief stop at a light, but bails out
/// quickly enough to let the car sleep within minutes of parking.
const PARK_CONFIRMATIONS_BEFORE_QUIET: u32 = 3;

// (Software version is intentionally not sampled. tesla-control's
// `state software-update` only returns the *pending* OTA version
// (often " "), never the currently-installed `car_version`. To
// surface the running OS version on drives, the user can enter it
// manually in settings — see fsd_versions.rs for the mapping table
// the per-drive rollup uses.)

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
    // Counts consecutive state polls showing shift_state = Park.
    // When it crosses PARK_CONFIRMATIONS_BEFORE_QUIET, the next tick
    // drops to body-controller-only polling (sleep-safe). Reset by
    // any non-Park shift observation OR by a user_presence flip
    // back to PRESENT during Quiet mode.
    let mut parked_polls: u32 = 0;
    // Last user_presence reading from body-controller-state. Used
    // to detect "driver got back in" while in Quiet mode so the
    // sampler can promote to Active on the next tick rather than
    // waiting for an external trigger.
    let mut last_user_presence: Option<bool> = None;

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
            sleep = tick(
                &conn,
                &mut held_radio,
                &mut parked_polls,
                &mut last_user_presence,
            ) => {
                tokio::time::sleep(sleep).await;
            }
        }
    }
}

/// One iteration of the main loop. Returns the duration to sleep
/// before the next iteration.
///
/// Two phases, decided each tick:
///
///   * **Active** — clip writes are happening AND shift_state isn't
///     confirmed-Park. Full `state` polls every AWAKE_INTERVAL, radio
///     held continuously. Each successful poll updates `parked_polls`
///     based on the observed shift.
///   * **Quiet** — either no clip writes (car asleep) OR shift_state
///     has been Park for `PARK_CONFIRMATIONS_BEFORE_QUIET` polls
///     (car parked-with-Sentry-recording). Body-controller-state
///     polls every QUIET_INTERVAL — sleep-safe, doesn't pin the car
///     awake. Radio is released between deep-asleep polls (so iOS
///     GATT can run) but held while in parked-with-Sentry (poll
///     cadence is too fast to cycle the GATT daemon cleanly).
///
/// Transitions:
///   * Active → Quiet: parked_polls reaches the confirmation count.
///   * Quiet → Active: body-controller user_presence flips
///     NOT_PRESENT → PRESENT (driver got back in). The next tick
///     immediately does a state poll.
async fn tick(
    conn: &Connection,
    held_radio: &mut bool,
    parked_polls: &mut u32,
    last_user_presence: &mut Option<bool>,
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
        *parked_polls = 0;
        *last_user_presence = None;
        return DISABLED_POLL;
    }
    if cfg.vin.is_empty() {
        debug!("no TESLA_BLE_VIN configured, idling");
        if *held_radio {
            release_radio().await;
            *held_radio = false;
        }
        *parked_polls = 0;
        *last_user_presence = None;
        return DISABLED_POLL;
    }

    let observation = usb_watch::observe();
    let car_truly_asleep = observation == CarState::Asleep;
    let parked_confirmed = *parked_polls >= PARK_CONFIRMATIONS_BEFORE_QUIET;
    let in_quiet_mode = car_truly_asleep || parked_confirmed;

    if in_quiet_mode {
        // Sleep-safe path. Acquire the radio for the brief BC call,
        // then release if the car is truly asleep (so iOS GATT comes
        // back). When in parked-confirmed (Sentry recording), keep
        // the radio held — 1-min poll cadence means cycling GATT
        // would burn ~10% of the time in stop/start churn.
        let acquired = if *held_radio {
            true
        } else {
            match lock::try_acquire(OWNER) {
                Ok(true) => {
                    *held_radio = true;
                    stop_ios_gatt().await;
                    true
                }
                Ok(false) => {
                    debug!("radio contended during quiet poll, skipping");
                    false
                }
                Err(e) => {
                    warn!("failed to acquire radio lock for quiet poll: {e}");
                    false
                }
            }
        };

        if acquired {
            // Always probe body-controller first — it's the
            // canonical source of user_presence and is sleep-safe.
            let presence_now = match sample::sample_body_controller(&cfg.vin).await {
                Ok(bc) => {
                    let p = bc.user_presence;
                    persist(conn, bc.sample);
                    p
                }
                Err(e) => {
                    warn!("sample_body_controller failed: {e}");
                    *last_user_presence
                }
            };

            // Driver-got-back-in detection: user_presence flipped
            // from NOT_PRESENT to PRESENT (was outside the car,
            // now inside). Promote to Active immediately — the
            // short returned Duration triggers a state poll on the
            // next tick instead of waiting another full QUIET_INTERVAL.
            if *last_user_presence == Some(false) && presence_now == Some(true) {
                info!("user_presence flipped PRESENT — resuming full state polls");
                *parked_polls = 0;
                *last_user_presence = presence_now;
                if car_truly_asleep {
                    release_radio().await;
                    *held_radio = false;
                }
                // 1s so the OS scheduler gets a moment; effectively
                // immediate next tick → state poll.
                return Duration::from_secs(1);
            }

            // When the user is in the car AND we're in Quiet
            // (because shift_state was Park last we checked), also
            // poll `state drive` to catch a shift change. This
            // covers the "user sat in parked car for a while then
            // drove away" case where user_presence never flips.
            // The car was already awake (user is in it), so we're
            // not making things worse from a sleep-timer
            // perspective.
            if presence_now == Some(true) {
                match sample::sample_state(&cfg.vin).await {
                    Ok(ss) => {
                        let shift_changed_to_drive = ss
                            .shift_state
                            .map_or(false, |s| !s.is_park() && s != sample::ShiftState::Unknown);
                        persist(conn, ss.sample);
                        if shift_changed_to_drive {
                            info!(
                                "shift_state non-Park while user in car — resuming full state polls"
                            );
                            *parked_polls = 0;
                            *last_user_presence = presence_now;
                            return Duration::from_secs(1);
                        }
                    }
                    Err(e) => {
                        warn!("state drive probe in quiet+present failed: {e}");
                    }
                }
            }

            *last_user_presence = presence_now;
            if car_truly_asleep {
                // Deep sleep + no user → hand the radio back to
                // iOS GATT between polls.
                release_radio().await;
                *held_radio = false;
            }
        }
        QUIET_INTERVAL
    } else {
        // Active state polling — full telemetry, car stays awake.
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

        match sample::sample_state(&cfg.vin).await {
            Ok(ss) => {
                // Update park-confirmation counter. Any non-Park
                // reading resets it; an Unknown reading neither
                // increments nor resets (better to stay in Active
                // when the SDK returns a value we can't decode).
                match ss.shift_state {
                    Some(s) if s.is_park() => {
                        *parked_polls = parked_polls.saturating_add(1);
                        if *parked_polls == PARK_CONFIRMATIONS_BEFORE_QUIET {
                            info!(
                                "{} consecutive Park observations — dropping to body-controller polling so the car can sleep",
                                PARK_CONFIRMATIONS_BEFORE_QUIET
                            );
                        }
                    }
                    Some(sample::ShiftState::Unknown) => {
                        // leave counter alone
                    }
                    Some(_) => {
                        // Drive / Reverse / Neutral — actively
                        // moving, reset.
                        *parked_polls = 0;
                    }
                    None => {
                        // shift_state not present in response; treat
                        // like Unknown.
                    }
                }
                // Clear stale user_presence — next time we drop to
                // Quiet, we want a fresh baseline before triggering
                // the "got back in" transition.
                *last_user_presence = None;
                persist(conn, ss.sample);
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
