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

use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;
use tracing::{debug, error, info, warn};

use crate::config::BleConfig;
use crate::sample::Sample;
use crate::usb_watch::CarState;

/// Lock-owner string this daemon writes into `/tmp/ble_radio_owner`.
/// Coordinated with `awake_start`'s owner string ("keep_awake").
const OWNER: &str = "telemetry";

/// Tick cadence in Active mode. `state drive` always runs on each
/// tick (highest priority — carries shiftState + location + odometer).
/// The slower sub-samplers (climate, charge, tires) only run when
/// their per-command interval has elapsed; see `Schedule` below.
const DRIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How often to refresh climate (cabin/exterior temp, HVAC) in
/// Active mode. 60s is fine — these are slow-changing.
const CLIMATE_INTERVAL: Duration = Duration::from_secs(60);

/// How often to refresh charge (battery %) in Active mode. 60s,
/// staggered 30s after climate so the two big-payload calls don't
/// both fire on the same tick.
const CHARGE_INTERVAL: Duration = Duration::from_secs(60);
const CHARGE_INITIAL_OFFSET: Duration = Duration::from_secs(30);

/// How often to refresh tire pressure in Active mode. 5 min — TPMS
/// almost never changes mid-drive, and the call has the smallest
/// payload of the four, so even at 5-min cadence it costs almost
/// nothing.
const TIRES_INTERVAL: Duration = Duration::from_secs(300);

/// Sample cadence for sleep-safe `body-controller-state` calls in
/// Quiet mode. 30s (down from 60s) halves the worst-case wakeup
/// latency — important because the user_presence flip is what
/// promotes us back to Active when someone gets in a parked car.
/// body-controller-state doesn't wake the car, so polling this
/// often is cheap from a battery-drain perspective.
const QUIET_INTERVAL: Duration = Duration::from_secs(30);

/// Retry interval after a sub-sampler fails. Implements the
/// "constantly pummel the endpoint with recons" pattern the Pi's
/// bluez stack doesn't do natively — when the car's BLE side drops
/// a connection (which it does aggressively to save battery), we
/// want to hit it again within seconds, not wait the full normal
/// interval. Catches the brief acceptance window before the car's
/// connection table refills with other clients.
const FAST_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// How many consecutive fast retries before backing off to the
/// normal cadence. 3 × FAST_RETRY_INTERVAL = ~9s of aggressive
/// retry per sub-sampler before giving up — enough to catch a real
/// reconnection window without burning power on a truly dead link.
const MAX_FAST_RETRIES: u32 = 3;

/// How often to do a `state climate` + `state charge` refresh while
/// in Quiet mode but the car is provably awake (recent clip writes
/// → Sentry recording or charging). The default Quiet flow only
/// runs body-controller-state, which doesn't carry battery/temps/
/// HVAC — so without this refresh, parked-with-Sentry would show
/// frozen values for as long as the session lasts. 3 min is the
/// sweet spot: fresh enough that the dashboard cards feel alive,
/// rare enough that we add minimal BLE load (~2 calls every 3 min,
/// vs Active mode's 4 calls every 15 s). Safe because the car is
/// already awake — we're not adding any wake-up drain.
const PARKED_AWAKE_REFRESH_INTERVAL: Duration = Duration::from_secs(180);

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

/// Per-command "next due" timestamps for the Active-mode scheduler.
///
/// Each tick, the scheduler walks the four poll types in priority
/// order and runs any that are due. `state drive` always runs first
/// when due — it carries shiftState + locationName + odometer and
/// must stay fresh. The slower polls (climate, charge, tires) only
/// run when their per-command interval has elapsed and only after
/// drive has gotten its turn this tick.
///
/// Stagger: charge starts 30s offset from climate so the two
/// big-payload calls don't stack on the same tick. The offset is
/// preserved automatically as long as both don't go overdue
/// simultaneously (which only happens after a long Quiet period —
/// acceptable, the next cycle restores the stagger naturally).
struct Schedule {
    next_drive: Instant,
    next_climate: Instant,
    next_charge: Instant,
    next_tires: Instant,
    /// Consecutive failure counters — used by the fast-retry
    /// pattern. On a successful sub-sample they reset to 0; on
    /// failure they increment and drive a 3s retry until
    /// MAX_FAST_RETRIES is hit, at which point we back off to the
    /// normal cadence to avoid hammering a permanently-dead link.
    drive_failures: u32,
    climate_failures: u32,
    charge_failures: u32,
    tires_failures: u32,
}

impl Schedule {
    fn new(now: Instant) -> Self {
        Self {
            // Drive + climate + tires fire immediately on first
            // tick — get a baseline snapshot.
            next_drive: now,
            next_climate: now,
            next_charge: now + CHARGE_INITIAL_OFFSET,
            next_tires: now,
            drive_failures: 0,
            climate_failures: 0,
            charge_failures: 0,
            tires_failures: 0,
        }
    }
    fn drive_due(&self, now: Instant) -> bool { now >= self.next_drive }
    fn climate_due(&self, now: Instant) -> bool { now >= self.next_climate }
    fn charge_due(&self, now: Instant) -> bool { now >= self.next_charge }
    fn tires_due(&self, now: Instant) -> bool { now >= self.next_tires }

    /// Compute the next-due instant for a sub-sampler that just ran.
    /// On success: normal interval. On failure within MAX_FAST_RETRIES:
    /// short retry interval (~3s) to catch the car's brief
    /// post-disconnect acceptance window. After too many fast retries
    /// in a row: fall back to the normal interval so we don't burn
    /// battery on a permanently-failing link.
    fn next_after(now: Instant, success: bool, failures: u32, normal: Duration) -> Instant {
        if success {
            now + normal
        } else if failures <= MAX_FAST_RETRIES {
            now + FAST_RETRY_INTERVAL
        } else {
            now + normal
        }
    }

    fn mark_drive(&mut self, now: Instant, success: bool) {
        self.drive_failures = if success { 0 } else { self.drive_failures.saturating_add(1) };
        self.next_drive = Self::next_after(now, success, self.drive_failures, DRIVE_INTERVAL);
    }
    fn mark_climate(&mut self, now: Instant, success: bool) {
        self.climate_failures = if success { 0 } else { self.climate_failures.saturating_add(1) };
        self.next_climate = Self::next_after(now, success, self.climate_failures, CLIMATE_INTERVAL);
    }
    fn mark_charge(&mut self, now: Instant, success: bool) {
        self.charge_failures = if success { 0 } else { self.charge_failures.saturating_add(1) };
        self.next_charge = Self::next_after(now, success, self.charge_failures, CHARGE_INTERVAL);
    }
    fn mark_tires(&mut self, now: Instant, success: bool) {
        self.tires_failures = if success { 0 } else { self.tires_failures.saturating_add(1) };
        self.next_tires = Self::next_after(now, success, self.tires_failures, TIRES_INTERVAL);
    }

    /// When should the next tick fire? Min of all four next-due
    /// timestamps. The main loop sleeps until this instant.
    fn next_due(&self) -> Instant {
        self.next_drive
            .min(self.next_climate)
            .min(self.next_charge)
            .min(self.next_tires)
    }
}

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
    // Per-command scheduler for Active mode. Persists across ticks
    // so per-poll cadences stay stable. Initialized so the first
    // Active tick fires drive + climate + tires immediately (for a
    // fresh start-of-drive snapshot), with charge deferred 30s so
    // it doesn't stack with climate.
    let mut schedule = Schedule::new(Instant::now());
    // Last time we did a parked-awake state refresh (climate +
    // charge while in Quiet mode but the car is recording dashcam
    // clips). Lets battery/temps stay reasonably fresh during
    // Sentry sessions and charging without dropping the radio-lock
    // dance the deep-sleep Quiet path relies on.
    let mut last_parked_awake_refresh: Option<Instant> = None;

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
                &mut schedule,
                &mut last_parked_awake_refresh,
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
    schedule: &mut Schedule,
    last_parked_awake_refresh: &mut Option<Instant>,
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
                    // Bumped to info — this is one of the main
                    // reasons quiet-mode samples go missing (archiveloop's
                    // keep_awake holds the radio during archive cycles).
                    // Surfacing it in the diagnostics panel lets the
                    // user tell "sampler is broken" from "sampler is
                    // politely waiting its turn".
                    info!(
                        "radio held by {:?} during quiet poll, skipping",
                        lock::current_owner()
                    );
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
            let presence_now = match sample::sample_body_controller(&cfg.vin, &cfg.adapter).await {
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
            // Drive-only (not the full telemetry batch) because
            // we just need shiftState here — the full Active mode
            // scheduler kicks in on the next tick if we detect a
            // shift change.
            if presence_now == Some(true) {
                match sample::sample_drive(&cfg.vin, &cfg.adapter).await {
                    Ok(d) => {
                        let shift_changed_to_drive = d
                            .shift_state
                            .map_or(false, |s| !s.is_park() && s != sample::ShiftState::Unknown);
                        // Persist whatever the drive probe got
                        // (location + odometer freshness even
                        // while parked-with-Sentry).
                        let probe_sample = Sample {
                            ts: sample::now_secs(),
                            location_name: d.location_name,
                            odometer_mi: d.odometer_mi,
                            source: "state".into(),
                            ..Sample::default()
                        };
                        persist(conn, probe_sample);
                        if shift_changed_to_drive {
                            info!(
                                "shift_state non-Park while user in car — resuming full state polls"
                            );
                            *parked_polls = 0;
                            *last_user_presence = presence_now;
                            // Reset schedule so Active starts fresh
                            // with a full snapshot.
                            *schedule = Schedule::new(Instant::now());
                            return Duration::from_secs(1);
                        }
                    }
                    Err(e) => {
                        warn!("state drive probe in quiet+present failed: {e}");
                    }
                }
            }

            // Parked-awake state refresh: when the car is parked
            // (Quiet mode) but actively recording dashcam clips
            // (observation == Awake), do a periodic climate +
            // charge poll so battery/temps don't go indefinitely
            // stale during Sentry sessions or AC charging. Safe
            // because the car is already awake — we add no
            // wake-up drain. Only fires every 3 min to keep BLE
            // load minimal.
            //
            // Skipped when car_truly_asleep (let it sleep) or
            // when the user is in the car (the drive probe above
            // already covers that path and a state transition is
            // imminent).
            if observation == CarState::Awake && presence_now != Some(true) {
                let due = last_parked_awake_refresh
                    .map(|t| t.elapsed() >= PARKED_AWAKE_REFRESH_INTERVAL)
                    .unwrap_or(true);
                if due {
                    let mut refresh = Sample {
                        ts: sample::now_secs(),
                        source: "state".into(),
                        ..Sample::default()
                    };
                    let mut any_ok = false;
                    match sample::sample_climate(&cfg.vin, &cfg.adapter).await {
                        Ok(c) => {
                            refresh.interior_temp_c = c.interior_temp_c;
                            refresh.exterior_temp_c = c.exterior_temp_c;
                            refresh.hvac_on = c.hvac_on;
                            any_ok = true;
                        }
                        Err(e) => warn!("parked-awake climate refresh failed: {e}"),
                    }
                    match sample::sample_charge(&cfg.vin, &cfg.adapter).await {
                        Ok(c) => {
                            refresh.battery_pct = c.battery_pct;
                            any_ok = true;
                        }
                        Err(e) => warn!("parked-awake charge refresh failed: {e}"),
                    }
                    if any_ok {
                        persist(conn, refresh);
                    }
                    *last_parked_awake_refresh = Some(Instant::now());
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
        // Active mode — scheduler-driven multi-poll. Each tick
        // composes one or more sub-samplers to run sequentially
        // based on what's overdue. `state drive` always runs first
        // when due (it carries shiftState + location + odometer —
        // the freshest-required signals); climate/charge/tires
        // run on slower per-command cadences and only inserted
        // after drive has had its turn.
        if !*held_radio {
            match lock::try_acquire(OWNER) {
                Ok(true) => {
                    *held_radio = true;
                    stop_ios_gatt().await;
                }
                Ok(false) => {
                    info!(
                        "radio held by {:?}, backing off {}s",
                        lock::current_owner(),
                        RADIO_CONTENDED_BACKOFF.as_secs()
                    );
                    return RADIO_CONTENDED_BACKOFF;
                }
                Err(e) => {
                    warn!("failed to acquire radio lock: {e}");
                    return RADIO_CONTENDED_BACKOFF;
                }
            }
        }

        let tick_now = Instant::now();
        // Detect "first tick after a long Quiet period" — the
        // schedule's next_drive will be very stale (Quiet doesn't
        // call mark_drive). Reset the schedule so climate/charge
        // get their 30s stagger back, and so all four sub-samplers
        // fire on this first tick for a fresh snapshot.
        if tick_now.duration_since(schedule.next_drive)
            > Duration::from_secs(2 * DRIVE_INTERVAL.as_secs())
        {
            *schedule = Schedule::new(tick_now);
        }
        // Single Sample built up across whatever sub-samplers ran
        // this tick. Fields stay None for any sub-sampler that
        // didn't run or that failed — the schema and the
        // aggregator both handle per-field NULLs gracefully.
        let mut sample = Sample {
            ts: sample::now_secs(),
            source: "state".into(),
            ..Sample::default()
        };
        let mut shift_state_observed: Option<sample::ShiftState> = None;
        let mut any_call_ran = false;

        // ── 1. DRIVE (priority — runs first when due) ──
        // Carries: shiftState (drive detection), locationName,
        // odometer. The "must stay fresh" signals.
        if schedule.drive_due(tick_now) {
            let success = match sample::sample_drive(&cfg.vin, &cfg.adapter).await {
                Ok(d) => {
                    sample.odometer_mi = d.odometer_mi;
                    sample.location_name = d.location_name;
                    shift_state_observed = d.shift_state;
                    true
                }
                Err(e) => {
                    warn!("sample_drive failed: {e}");
                    false
                }
            };
            // Fast-retry on failure (~3s), normal interval on
            // success. See Schedule::next_after for the pattern.
            schedule.mark_drive(tick_now, success);
            any_call_ran = true;
        }

        // ── 2. CLIMATE (every 60s) ──
        if schedule.climate_due(tick_now) {
            let success = match sample::sample_climate(&cfg.vin, &cfg.adapter).await {
                Ok(c) => {
                    sample.interior_temp_c = c.interior_temp_c;
                    sample.exterior_temp_c = c.exterior_temp_c;
                    sample.hvac_on = c.hvac_on;
                    true
                }
                Err(e) => {
                    warn!("sample_climate failed: {e}");
                    false
                }
            };
            schedule.mark_climate(tick_now, success);
            any_call_ran = true;
        }

        // ── 3. CHARGE (every 60s, offset 30s from climate) ──
        if schedule.charge_due(tick_now) {
            let success = match sample::sample_charge(&cfg.vin, &cfg.adapter).await {
                Ok(c) => {
                    sample.battery_pct = c.battery_pct;
                    true
                }
                Err(e) => {
                    warn!("sample_charge failed: {e}");
                    false
                }
            };
            schedule.mark_charge(tick_now, success);
            any_call_ran = true;
        }

        // ── 4. TIRES (every 5 min) ──
        if schedule.tires_due(tick_now) {
            let success = match sample::sample_tires(&cfg.vin, &cfg.adapter).await {
                Ok(t) => {
                    sample.tire_fl_psi = t.tire_fl_psi;
                    sample.tire_fr_psi = t.tire_fr_psi;
                    sample.tire_rl_psi = t.tire_rl_psi;
                    sample.tire_rr_psi = t.tire_rr_psi;
                    true
                }
                Err(e) => {
                    warn!("sample_tires failed: {e}");
                    false
                }
            };
            schedule.mark_tires(tick_now, success);
            any_call_ran = true;
        }

        // Update park-confirmation counter from drive's shift
        // observation (if drive ran this tick).
        match shift_state_observed {
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
                // SDK returned an unrecognized shift code — leave
                // counter alone (better to stay Active than drop to
                // Quiet on a parsing miss).
            }
            Some(_) => {
                // Drive / Reverse / Neutral — actively moving,
                // reset the Park counter.
                *parked_polls = 0;
            }
            None => {
                // Drive didn't run this tick OR drive failed —
                // leave the counter alone.
            }
        }

        // Clear stale user_presence — next time we drop to Quiet,
        // we want a fresh baseline before triggering the "got back
        // in" transition.
        *last_user_presence = None;

        // Persist whatever this tick collected. Even a single
        // drive-only poll lands a row with location/odometer
        // populated — the live-output panel and aggregator both
        // handle the sparse-row case.
        if any_call_ran {
            persist(conn, sample);
        }

        // Sleep until the next scheduled sub-sampler is due. Drive
        // is normally the soonest (15s), but if a slow call this
        // tick blew through the budget, we'll wake up sooner.
        let next = schedule.next_due();
        let after = Instant::now();
        if next > after {
            next.duration_since(after)
        } else {
            // Already overdue — next tick immediately (cheap, the
            // tick itself enforces the actual cadence).
            Duration::from_millis(100)
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
