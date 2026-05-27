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

mod action_socket;
mod clock_sync;
mod config;
mod db;
mod diag_log;
mod lock;
mod sample;
mod sample_ble;
mod usb_watch;

use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;
use tokio::sync::mpsc;
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

/// How often to refresh `state closures` in Active mode. 60s — same
/// cadence as climate/charge. This is the sole source of
/// `sentry_mode_state` for the quiet-mode gate, so it has to be at
/// least as fresh as the cadence at which the phase machine reacts
/// (every drive-poll, i.e. 15s). 60s is a fine compromise: the gate
/// only cares about transitions, not millisecond accuracy, and a
/// remote sentry toggle from the Tesla app reaches us within ~1 tick.
const CLOSURES_INTERVAL: Duration = Duration::from_secs(60);

// (LOCATION_INTERVAL was removed: a brief stint adding `state
// location` as its own sub-sampler ended once a tester's
// tesla-control wire capture showed Tesla returns location_name in
// `state drive` responses but NOT in `state location` responses.
// `sample_drive_ble` now extracts the address from the bundled
// LocationState in the drive response, so location refreshes at
// the drive cadence (15s) for free.)

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

/// How often to poll `state tire-pressure` while in Quiet mode but
/// the car is awake. Much rarer than the climate/charge refresh
/// because TPMS readings genuinely don't change while parked —
/// 30 min is enough to feed the TPMS dashboard card with periodic
/// fresh data and to confirm the sensors are still reporting.
/// Without this, TPMS would only ever update during/right after a
/// drive (Active mode polls tires every 5 min), and users who
/// rarely drive would see indefinitely stale numbers.
const PARKED_AWAKE_TPMS_INTERVAL: Duration = Duration::from_secs(1800);

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
    /// `state closures` — only field we read from it is
    /// sentry_mode_state, which the quiet-mode gate needs every
    /// poll cycle. Same cadence as climate/charge so the gate's
    /// inputs are roughly co-fresh.
    next_closures: Instant,
    /// Consecutive failure counters — used by the fast-retry
    /// pattern. On a successful sub-sample they reset to 0; on
    /// failure they increment and drive a 3s retry until
    /// MAX_FAST_RETRIES is hit, at which point we back off to the
    /// normal cadence to avoid hammering a permanently-dead link.
    drive_failures: u32,
    climate_failures: u32,
    charge_failures: u32,
    tires_failures: u32,
    closures_failures: u32,
}

impl Schedule {
    fn new(now: Instant) -> Self {
        Self {
            // Drive + climate + tires + closures fire immediately on
            // first tick — get a baseline snapshot, including the
            // sentry_mode + (after the 30s charge offset) charging_state
            // signals the quiet-mode gate depends on. Location is
            // bundled into drive responses so it refreshes for free
            // on the drive cadence.
            next_drive: now,
            next_climate: now,
            next_charge: now + CHARGE_INITIAL_OFFSET,
            next_tires: now,
            next_closures: now,
            drive_failures: 0,
            climate_failures: 0,
            charge_failures: 0,
            tires_failures: 0,
            closures_failures: 0,
        }
    }
    fn drive_due(&self, now: Instant) -> bool { now >= self.next_drive }
    fn climate_due(&self, now: Instant) -> bool { now >= self.next_climate }
    fn charge_due(&self, now: Instant) -> bool { now >= self.next_charge }
    fn tires_due(&self, now: Instant) -> bool { now >= self.next_tires }
    fn closures_due(&self, now: Instant) -> bool { now >= self.next_closures }

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
    fn mark_closures(&mut self, now: Instant, success: bool) {
        self.closures_failures = if success { 0 } else { self.closures_failures.saturating_add(1) };
        self.next_closures = Self::next_after(now, success, self.closures_failures, CLOSURES_INTERVAL);
    }

    /// When should the next tick fire? Min of all next-due timestamps
    /// across every sub-sampler. The main loop sleeps until this
    /// instant.
    fn next_due(&self) -> Instant {
        self.next_drive
            .min(self.next_climate)
            .min(self.next_charge)
            .min(self.next_tires)
            .min(self.next_closures)
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

    // Brief startup wait for the system clock to come up — either via
    // RTC (immediate) or NTP (seconds, if WiFi is reachable). Just
    // long enough to dodge the very first cold-boot tick; the
    // BLE-based clock sync (see clock_sync.rs) handles everything
    // else once the first state response lands. Was 5 min when we
    // depended entirely on NTP; now 30s is plenty because the car
    // itself becomes our backup time source via BLE.
    wait_for_clock_sync(Duration::from_secs(30)).await;

    let conn = db::open()?;

    // Background diagnostic logger — writes one line per minute to
    // /mutable/sentryusb-ble.log so the operator can scroll back
    // through "what was the car state at 3pm while I was away"
    // from the Logs → Bluetooth UI tab. Fully independent of the
    // sampler's main loop; opens its own read-only DB handle each
    // tick. Lives as long as the process does.
    diag_log::spawn(sentryusb_drives::DEFAULT_DB_PATH.into());

    // IPC bridge for external BLE actions (currently just
    // sentryusb-ble-action invoked by run/awake_start). Lets
    // keep-awake nudges piggyback on our already-warm
    // PersistentSession instead of having to stop us first to grab
    // the radio. Channel is bounded but small — keep-awake actions
    // arrive ~once per archive cycle, never concurrently.
    let (action_tx, mut action_rx) = mpsc::channel::<action_socket::ActionRequest>(8);
    action_socket::spawn(action_tx);

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
    // Separate (much rarer) timer for TPMS — TPMS readings don't
    // change while parked, so we poll them every 30 min in Quiet
    // rather than every 3 min like climate/charge. Bundled into
    // the same tick's Sample row when both timers happen to fire
    // together.
    let mut last_parked_awake_tpms_refresh: Option<Instant> = None;
    // Long-lived BLE session for state queries. Lazy-spawned in
    // tick() on the first state-poll cycle after BLE telemetry is
    // enabled; reused for the rest of the process's lifetime so we
    // avoid re-scanning + re-handshaking on every cycle. Dropped
    // and recreated if VIN changes mid-run.
    let mut ble_session: Option<sample_ble::SessionHandle> = None;
    // Last-known charging_state from a successful `state charge` poll.
    // Drives the quiet-mode gate: while the car is actively charging
    // (Starting / Charging / Calibrating), the car is keeping itself
    // awake, so quiet-mode body-controller polling would just leave
    // battery_pct stale. `None` = we've never had a successful charge
    // poll → conservative default: treat as actively-charging so we
    // stay Active until we have proof otherwise.
    let mut last_charging_state: Option<sample::ChargingState> = None;
    // Same idea for sentry mode — read from `state closures`. Any
    // non-Off value means the car is awake for sentry purposes;
    // dropping to quiet would freeze the live-data panel during an
    // active session. `None` → treat as on for the conservative
    // default.
    let mut last_sentry_mode: Option<sample::SentryMode> = None;

    // SIGTERM handler — release the radio on shutdown so the iOS
    // GATT daemon can come back up cleanly.
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    let mut sigint = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::interrupt(),
    )?;
    // SIGUSR1 = "do a full state poll now" — fired by the
    // /api/system/ble-force-poll endpoint when the user clicks
    // the "Poll now" button. Forces the next-due time of every
    // sub-sampler to "now" and resets the parked-awake refresh
    // timer, so the next tick runs everything regardless of the
    // current phase.
    let mut sigusr1 = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::user_defined1(),
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
            _ = sigusr1.recv() => {
                info!("SIGUSR1 received — forcing a full state poll on next tick (all sub-samplers due immediately)");
                let now = Instant::now();
                // Build a fresh schedule, then override every next_*
                // field so ALL sub-samplers fire on the very next
                // tick — no CHARGE_INITIAL_OFFSET, no FAST_RETRY
                // gating, no per-type pacing. The user explicitly
                // asked for a fresh full read; honor that by polling
                // drive + climate + charge + closures + tires all
                // in one cycle.
                //
                // (Schedule::new normally staggers `next_charge` by
                // CHARGE_INITIAL_OFFSET = 30s to avoid hammering the
                // car with everything at once on a fresh daemon
                // startup. That's the right call for startup but the
                // wrong call here — when the user clicks "Poll now"
                // and waits 8s to see results, a 30s charge delay
                // would leave the battery field stale and look
                // broken even though everything else updated.)
                schedule = Schedule::new(now);
                schedule.next_drive = now;
                schedule.next_climate = now;
                schedule.next_charge = now;
                schedule.next_closures = now;
                schedule.next_tires = now;
                last_parked_awake_refresh = None;
                last_parked_awake_tpms_refresh = None;
                // Reset parked_polls so the phase machine flips to
                // Active even if we'd been in parked-confirmed
                // Quiet — otherwise the force-poll would only fire
                // a body_controller call. The next Park observation
                // will tick the counter back up.
                parked_polls = 0;
                // Loop continues immediately — next tick runs at
                // the top of the loop without sleeping.
            }
            Some(req) = action_rx.recv() => {
                // IPC: an external process (e.g. sentryusb-ble-action
                // from run/awake_start) wants us to do a one-shot BLE
                // action through our existing PersistentSession.
                // Serializes naturally with the rest of the select —
                // any in-flight tick completes first, then we handle
                // this, then the next tick fires.
                handle_action_request(
                    req,
                    &mut held_radio,
                    &mut ble_session,
                ).await;
            }
            sleep = tick(
                &conn,
                &mut held_radio,
                &mut parked_polls,
                &mut last_user_presence,
                &mut schedule,
                &mut last_parked_awake_refresh,
                &mut last_parked_awake_tpms_refresh,
                &mut ble_session,
                &mut last_charging_state,
                &mut last_sentry_mode,
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
    last_parked_awake_tpms_refresh: &mut Option<Instant>,
    ble_session: &mut Option<sample_ble::SessionHandle>,
    last_charging_state: &mut Option<sample::ChargingState>,
    last_sentry_mode: &mut Option<sample::SentryMode>,
) -> Duration {
    let cfg = match BleConfig::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to load BLE config: {e}");
            return DISABLED_POLL;
        }
    };

    // Lazy-spawn (or recreate-on-VIN-change) the persistent BLE
    // session. Cheap when the session already exists — just a VIN
    // string compare. The first call does the heavy lifting (key
    // load + scan + GATT connect + handshake) inside the first
    // sample_*_ble call below.
    if let Err(e) = sample_ble::ensure_session_for(ble_session, &cfg.vin, Some(&cfg.adapter)) {
        warn!("could not start PersistentSession (will retry next tick): {e:#}");
        return Duration::from_secs(5);
    }
    let session = &ble_session
        .as_ref()
        .expect("ensure_session_for set it on success")
        .session;

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

    // Conservative defaults: if we've never had a successful charge or
    // closures poll, treat the car as actively-charging / sentry-on so
    // the gate keeps us in Active mode. Pro: never wrongly quiets down
    // during a charge session we haven't observed yet. Con: cold-start
    // does a brief Active burst (~30-60s) before the first charge +
    // closures polls land and confirm the car can actually sleep,
    // which may wake the car once per daemon restart.
    let actively_charging = last_charging_state
        .map(|s| s.is_active_charging())
        .unwrap_or(true);
    let sentry_on = last_sentry_mode.map(|s| s.is_on()).unwrap_or(true);

    // Two paths to quiet-mode polling:
    //   * car_truly_asleep — no clip writes for the AWAKE_WITHIN window.
    //     Body-controller is the only sleep-safe option here.
    //   * parked_confirmed — shift_state=Park for 3+ consecutive polls.
    //     The car COULD sleep, but only if nothing else is keeping
    //     it awake.
    // Either path requires that the car isn't actively pulling power
    // for a charge or running sentry monitoring — both keep the car
    // awake on their own, and quiet-mode polling would just leave
    // battery_pct / sentry_mode_state stale on the dashboard.
    let want_quiet = car_truly_asleep || parked_confirmed;
    let in_quiet_mode = want_quiet && !actively_charging && !sentry_on;

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
            let presence_now = match sample_ble::sample_body_controller_ble(session).await {
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
                match sample_ble::sample_drive_ble(session).await {
                    Ok(d) => {
                        // Self-correct the Pi's clock if it's
                        // significantly off — uses Tesla's
                        // GPS-derived timestamp from the response.
                        // No-op when local clock is already close.
                        try_sync_clock(d.meta);
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
                // Two independent timers in this branch:
                //   * `refresh_due`   — climate + charge every 3 min
                //   * `tpms_due`      — tire pressure every 30 min
                // Either firing opens this poll cycle; both can fire
                // in the same tick and get bundled into one Sample.
                let refresh_due = last_parked_awake_refresh
                    .map(|t| t.elapsed() >= PARKED_AWAKE_REFRESH_INTERVAL)
                    .unwrap_or(true);
                let tpms_due = last_parked_awake_tpms_refresh
                    .map(|t| t.elapsed() >= PARKED_AWAKE_TPMS_INTERVAL)
                    .unwrap_or(true);

                if refresh_due || tpms_due {
                    let mut refresh = Sample {
                        ts: sample::now_secs(),
                        source: "state".into(),
                        ..Sample::default()
                    };
                    let mut any_ok = false;

                    if refresh_due {
                        match sample_ble::sample_climate_ble(session).await {
                            Ok(c) => {
                                try_sync_clock(c.meta);
                                refresh.interior_temp_c = c.interior_temp_c;
                                refresh.exterior_temp_c = c.exterior_temp_c;
                                refresh.hvac_on = c.hvac_on;
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake climate refresh failed: {e}"),
                        }
                        match sample_ble::sample_charge_ble(session).await {
                            Ok(c) => {
                                try_sync_clock(c.meta);
                                refresh.battery_pct = c.battery_pct;
                                // Also refresh the gate input so a
                                // charge that starts mid-quiet bumps
                                // us back to Active on the next tick.
                                if let Some(cs) = c.charging_state {
                                    *last_charging_state = Some(cs);
                                }
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake charge refresh failed: {e}"),
                        }
                        // Closures refresh — gives us a sentry_mode
                        // update so a remotely-enabled sentry session
                        // also bumps us back to Active. No persisted
                        // fields, so this doesn't affect `any_ok`.
                        match sample_ble::sample_closures_ble(session).await {
                            Ok(c) => {
                                try_sync_clock(c.meta);
                                if let Some(sm) = c.sentry_mode {
                                    *last_sentry_mode = Some(sm);
                                }
                            }
                            Err(e) => warn!("parked-awake closures refresh failed: {e}"),
                        }
                        // (Location not refreshed here: the dedicated
                        // location poll was a dead end — Tesla only
                        // returns location_name in `state drive`
                        // responses, not standalone `state location`.
                        // Parked-awake mode doesn't call drive
                        // either, so the address stays at whatever
                        // the last drive poll captured. That's
                        // fine for the parked case — by definition
                        // we're not moving.)
                        *last_parked_awake_refresh = Some(Instant::now());
                    }

                    if tpms_due {
                        // TPMS rarely changes while parked, but
                        // periodic checks confirm sensors still
                        // report and feed the dashboard's TPMS card.
                        match sample_ble::sample_tires_ble(session).await {
                            Ok(t) => {
                                try_sync_clock(t.meta);
                                refresh.tire_fl_psi = t.tire_fl_psi;
                                refresh.tire_fr_psi = t.tire_fr_psi;
                                refresh.tire_rl_psi = t.tire_rl_psi;
                                refresh.tire_rr_psi = t.tire_rr_psi;
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake tires refresh failed: {e}"),
                        }
                        *last_parked_awake_tpms_refresh = Some(Instant::now());
                    }

                    if any_ok {
                        persist(conn, refresh);
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
            let success = match sample_ble::sample_drive_ble(session).await {
                Ok(d) => {
                    try_sync_clock(d.meta);
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
            let success = match sample_ble::sample_climate_ble(session).await {
                Ok(c) => {
                    try_sync_clock(c.meta);
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
            let success = match sample_ble::sample_charge_ble(session).await {
                Ok(c) => {
                    try_sync_clock(c.meta);
                    sample.battery_pct = c.battery_pct;
                    // Refresh the gate input. Successful poll → last
                    // known value advances; failure → keep previous
                    // (don't downgrade to None and force a conservative
                    // Active burst on a single transient failure).
                    if let Some(cs) = c.charging_state {
                        *last_charging_state = Some(cs);
                    }
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

        // ── 4. CLOSURES (every 60s) ──
        // Only field we consume is sentry_mode — fed into the
        // quiet-mode gate. Door / window / charge-port state is
        // available from the same response and could be surfaced
        // in the UI later without an additional BLE call.
        if schedule.closures_due(tick_now) {
            let success = match sample_ble::sample_closures_ble(session).await {
                Ok(c) => {
                    try_sync_clock(c.meta);
                    if let Some(sm) = c.sentry_mode {
                        *last_sentry_mode = Some(sm);
                    }
                    true
                }
                Err(e) => {
                    warn!("sample_closures failed: {e}");
                    false
                }
            };
            schedule.mark_closures(tick_now, success);
            any_call_ran = true;
        }

        // ── 5. TIRES (every 5 min) ──
        if schedule.tires_due(tick_now) {
            let success = match sample_ble::sample_tires_ble(session).await {
                Ok(t) => {
                    try_sync_clock(t.meta);
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
                    // Whether we actually drop to quiet on the next
                    // tick depends on the new charging/sentry gate.
                    // Log both the threshold-hit AND the gate outcome
                    // so journalctl tells the operator why polling
                    // did or didn't slow down.
                    if actively_charging || sentry_on {
                        info!(
                            "{} consecutive Park observations — but staying Active \
                             (actively_charging={}, sentry_on={}); car is awake for \
                             a reason, quiet polling would freeze battery/sentry signals",
                            PARK_CONFIRMATIONS_BEFORE_QUIET,
                            actively_charging,
                            sentry_on,
                        );
                    } else {
                        info!(
                            "{} consecutive Park observations — dropping to body-controller polling so the car can sleep",
                            PARK_CONFIRMATIONS_BEFORE_QUIET
                        );
                    }
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

/// Block until the system clock looks plausibly correct, or `timeout`
/// elapses. "Plausible" = year >= 2025 (anything later than the time
/// this code was written) OR systemd-timesyncd has set its
/// "synchronized" marker file. Either condition is sufficient — RTC
/// users will satisfy the first check immediately on boot.
///
/// Why this matters: without an RTC battery, the Pi's clock can be
/// years off after a cold boot until WiFi reaches an NTP server.
/// Samples written with bad timestamps are unrecoverable — they fall
/// outside any real drive window when the aggregator runs later.
/// So we just don't sample until the clock is sane. Best-effort:
/// times out after 5 min so we don't block forever in pathological
/// no-WiFi setups.
async fn wait_for_clock_sync(timeout: Duration) {
    if clock_is_sane() {
        debug!("clock looks sane on startup; no wait needed");
        return;
    }
    info!(
        "system clock is not synced yet — pausing sampler until \
         NTP catches up (max {}s). Install an RTC battery on the \
         Pi's BAT pin to avoid this on subsequent boots.",
        timeout.as_secs()
    );
    let deadline = std::time::Instant::now() + timeout;
    let mut last_log = std::time::Instant::now();
    let log_every = Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if clock_is_sane() {
            info!("system clock is now synced; resuming sampler");
            return;
        }
        if last_log.elapsed() >= log_every {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            info!(
                "still waiting for clock sync ({}s remaining)",
                remaining.as_secs()
            );
            last_log = std::time::Instant::now();
        }
    }
    warn!(
        "clock sync timeout reached — starting sampler anyway. \
         Telemetry written before NTP eventually syncs may not \
         match drives correctly."
    );
}

/// "Is the system clock plausibly correct?" — two signals, either
/// one is enough:
///   1. systemd-timesyncd has set its synchronized marker
///   2. The year is >= 2025 (anything in or after the year this
///      code was written; rules out the typical 1970 / 2000 / 2014
///      fallback values that show up on a Pi without RTC)
fn clock_is_sane() -> bool {
    // systemd-timesyncd marker — touched the moment a successful NTP
    // exchange happens, persists across reboots if the rootfs is
    // writable.
    if std::path::Path::new("/run/systemd/timesync/synchronized").exists() {
        return true;
    }
    // Year sanity check — a Pi with an RTC battery will pass this
    // immediately on boot even before NTP runs.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 2025-01-01 00:00:00 UTC = 1735689600.
    secs > 1_735_689_600
}

/// Helper: feed a successful response's metadata into the clock-sync
/// machinery. No-op if the response didn't include a vehicle
/// timestamp (e.g. body-controller-state) or if our clock is already
/// within tolerance. Called from every success branch in tick() so
/// any working sub-sample can fix the clock.
fn try_sync_clock(meta: sample::ResponseMeta) {
    if let (Some(vehicle_ts_ms), Some(started)) =
        (meta.vehicle_ts_ms, meta.request_started_at)
    {
        clock_sync::maybe_set_clock_from_vehicle(vehicle_ts_ms, started);
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

/// Service one IPC action request from `sentryusb-ble-action`. Does
/// the same radio + iOS-daemon prep as a normal Active-mode tick:
/// acquires the BLE radio lock if we don't already hold it, stops
/// the iOS GATT daemon for exclusive hci0, then dispatches the
/// action through the long-lived PersistentSession.
///
/// We deliberately do NOT release the radio after a successful
/// action — the very next tick will probably want it anyway, and
/// thrashing the iOS daemon down/up on every keep-awake nudge
/// would defeat the whole point of routing this through us.
async fn handle_action_request(
    req: action_socket::ActionRequest,
    held_radio: &mut bool,
    ble_session: &mut Option<sample_ble::SessionHandle>,
) {
    let verb = req.verb.clone();
    info!("action_socket: IPC request received — verb={}", verb);

    // Load config: respects the same enable/VIN gate as the rest of
    // the daemon. If BLE is off in settings, refuse the action so
    // ble-action can fall back (or just propagate the error to
    // awake_start, which probably shouldn't have called us anyway).
    let cfg = match crate::config::BleConfig::load() {
        Ok(c) => c,
        Err(e) => {
            let _ = req.reply.send(Err(anyhow::anyhow!(
                "load BLE config: {e}"
            )));
            return;
        }
    };
    if !cfg.enabled {
        let _ = req.reply.send(Err(anyhow::anyhow!(
            "BLE is disabled in settings"
        )));
        return;
    }
    if cfg.vin.is_empty() {
        let _ = req.reply.send(Err(anyhow::anyhow!(
            "TESLA_BLE_VIN not configured"
        )));
        return;
    }

    // Resolve the verb to a typed ActionPayload before doing any
    // BLE work — saves the cost of the radio handoff for a typo.
    let action = match action_socket::parse_verb(&verb) {
        Ok(a) => a,
        Err(e) => {
            let _ = req.reply.send(Err(e));
            return;
        }
    };

    // Lazy-spawn or reuse the PersistentSession on the configured
    // VIN/adapter — exactly the same call the tick loop uses.
    if let Err(e) = sample_ble::ensure_session_for(
        ble_session,
        &cfg.vin,
        Some(&cfg.adapter),
    ) {
        let _ = req.reply.send(Err(anyhow::anyhow!(
            "PersistentSession start failed: {e:#}"
        )));
        return;
    }

    // Acquire the radio + stop iOS GATT if not already held. Same
    // logic the Active-mode tick uses; copy-pasted here rather
    // than refactored out because the surrounding context (which
    // failure cases are recoverable, when to return early) is
    // different enough that a shared helper would obscure more
    // than it would deduplicate.
    if !*held_radio {
        match lock::try_acquire(OWNER) {
            Ok(true) => {
                *held_radio = true;
                stop_ios_gatt().await;
            }
            Ok(false) => {
                let _ = req.reply.send(Err(anyhow::anyhow!(
                    "radio held by {:?} — cannot service action right now",
                    lock::current_owner()
                )));
                return;
            }
            Err(e) => {
                let _ = req.reply.send(Err(anyhow::anyhow!(
                    "could not acquire radio lock: {e}"
                )));
                return;
            }
        }
    }

    let session = &ble_session
        .as_ref()
        .expect("ensure_session_for left session populated")
        .session;

    let started = Instant::now();
    let result = session.send_action(action).await;
    let elapsed_ms = started.elapsed().as_millis();
    match &result {
        Ok(bytes) => info!(
            "action_socket: verb={} ok ({}ms, {} bytes decrypted response)",
            verb,
            elapsed_ms,
            bytes.len()
        ),
        Err(e) => warn!(
            "action_socket: verb={} failed after {}ms: {:#}",
            verb, elapsed_ms, e
        ),
    }
    let _ = req.reply.send(result.map(|_| ()));
}

/// Restart the iOS GATT daemon and clear our radio-lock entry.
/// Called on radio release transitions and SIGTERM.
///
/// Ordering matters here: the lock file release happens FIRST and
/// synchronously, because that's the bit other processes
/// (keep_awake, archiveloop) need before they can take the radio.
/// The systemctl-start shell-out happens second under a tight
/// 5-second cap — systemctl can occasionally block much longer
/// (dependency resolution, bluez settling, etc.), and on the
/// SIGTERM path we'd rather skip the iOS-daemon restart than risk
/// systemd SIGKILL'ing us mid-shutdown. The next sampler instance's
/// own teardown logic (or systemd's restart policy) will eventually
/// bring sentryusb-ble back up either way.
async fn release_radio() {
    // Lock file first — fast, deterministic, the actual "release"
    // semantic other processes depend on.
    if let Err(e) = lock::release(OWNER) {
        warn!("failed to release radio lock: {e}");
    }
    // Then iOS daemon restart, bounded so we never sit here for
    // 30+ seconds during shutdown.
    let result = sentryusb_shell::run_with_timeout(
        Duration::from_secs(5),
        "systemctl",
        &["start", "sentryusb-ble"],
    )
    .await;
    if let Err(e) = result {
        warn!(
            "systemctl start sentryusb-ble didn't complete within 5s — \
             skipping (next sampler instance or systemd will bring it back): {e}"
        );
    }
}
