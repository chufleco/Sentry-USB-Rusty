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
mod keep_accessory;
mod lock;
mod c6_coord;
mod c6_source;
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

/// Active-mode tick cadence. `state drive` runs every tick (highest
/// priority — carries shiftState + location + odometer); slower
/// sub-samplers run on their own intervals (see `Schedule`).
const DRIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How often to refresh climate (cabin/exterior temp, HVAC) in
/// Active mode. 60s is fine — these are slow-changing.
const CLIMATE_INTERVAL: Duration = Duration::from_secs(60);

/// How often to refresh charge (battery %) in Active mode. 60s,
/// staggered 30s after climate so the two big-payload calls don't
/// both fire on the same tick.
const CHARGE_INTERVAL: Duration = Duration::from_secs(60);
const CHARGE_INITIAL_OFFSET: Duration = Duration::from_secs(30);

/// Charge refresh cadence while **fast charging** (DC: Supercharger /
/// CCS). The power curve tapers fast on a DC charge, so 60s smears the
/// shape and misses the early peak; 15s captures it. Only used while the
/// last charge poll showed active charging above `FAST_CHARGE_THRESHOLD_KW`
/// — it reverts to `CHARGE_INTERVAL` the moment power drops or the charge
/// ends.
const CHARGE_FAST_INTERVAL: Duration = Duration::from_secs(15);

/// Charger power (kW) above which we treat a charge as DC fast charging
/// and switch to `CHARGE_FAST_INTERVAL`. Mirror of
/// `FAST_CHARGE_THRESHOLD_KW` in `crates/api/src/charging.rs` (the api
/// crate can't be a dependency here) — keep the two in sync. Strict `>`,
/// so a 22 kW European AC wallbox stays on the normal cadence. `i32` to
/// match the decoded `charger_power_kw`.
const FAST_CHARGE_THRESHOLD_KW: i32 = 22;

/// Tire-pressure refresh in Active mode. 5 min — TPMS barely changes
/// mid-drive.
const TIRES_INTERVAL: Duration = Duration::from_secs(300);

/// `state closures` refresh in Active mode. 60s. Sole source of
/// `sentry_mode_state` for the quiet-mode gate, which only cares about
/// transitions — a remote sentry toggle reaches us within ~1 tick.
const CLOSURES_INTERVAL: Duration = Duration::from_secs(60);

// No separate location sampler: Tesla returns location_name in `state
// drive` responses but not in `state location`, so `sample_drive_ble`
// pulls the address from the drive response at the 15s drive cadence.

/// Quiet-mode cadence for sleep-safe `body-controller-state` calls.
/// 30s keeps wakeup latency low (user_presence promotes us back to
/// Active); these calls don't wake the car, so polling often is cheap.
const QUIET_INTERVAL: Duration = Duration::from_secs(30);

/// Retry interval after a sub-sampler fails. The car drops BLE
/// connections aggressively to save battery, so retry within seconds
/// to catch its brief acceptance window before other clients refill
/// its connection table.
const FAST_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// Consecutive fast retries before backing off to normal cadence.
/// ~9s of aggressive retry — enough to catch a reconnection window
/// without burning power on a dead link.
const MAX_FAST_RETRIES: u32 = 3;

/// `state climate` + `state charge` refresh while in Quiet mode but
/// the car is provably awake (recent clip writes). body-controller-state
/// alone doesn't carry battery/temps/HVAC, so without this the
/// parked-with-Sentry dashboard would show frozen values. 3 min keeps
/// the cards alive at minimal BLE load; safe since the car is awake.
const PARKED_AWAKE_REFRESH_INTERVAL: Duration = Duration::from_secs(180);

/// `state tire-pressure` poll while parked-awake. 30 min — TPMS doesn't
/// change while parked, but this keeps the card fresh for users who
/// rarely drive (otherwise tires only update in Active mode).
const PARKED_AWAKE_TPMS_INTERVAL: Duration = Duration::from_secs(1800);

/// Consecutive Park polls before dropping to Quiet mode. 3 @ 15s = 45s
/// — rides through a stop at a light but lets the car sleep soon after
/// parking.
const PARK_CONFIRMATIONS_BEFORE_QUIET: u32 = 3;

/// How long a charge/sentry reading stays authoritative for the Quiet
/// gate. ~3 parked-awake refresh opportunities (see
/// PARKED_AWAKE_REFRESH_INTERVAL) or ~10 Active charge polls — past
/// that, the reading predates a sustained poll outage and pinning
/// Active on it would recreate the accidental-keep-awake loop the
/// unread defaults used to cause, just with a stale value instead of
/// no value. Policy constant, deliberately not derived from the poll
/// cadences.
const GATE_READING_MAX_AGE: Duration = Duration::from_secs(600);

/// A gate input paired with when it was read, so consumers that make
/// live decisions (the Quiet gate, poll-cadence hints) can ignore
/// readings that predate a long poll outage. Keep-accessory
/// deliberately consumes the raw value instead — see its call site.
#[derive(Clone, Copy)]
struct TimedReading<T: Copy> {
    value: T,
    at: Instant,
}

impl<T: Copy> TimedReading<T> {
    fn now(value: T) -> Self {
        Self { value, at: Instant::now() }
    }

    /// Stamp with a value already `age` old (e.g. a C6 snapshot value delivered
    /// some time ago). Backdates `at` so the gate's freshness check ages it from
    /// when the reading was actually valid — NOT from now — so a stale C6 charge
    /// state can't masquerade as fresh and defeat the keep-awake gate.
    /// None when `age` predates the monotonic clock's origin: an age we can't
    /// represent must not collapse to "now" and read as fresh.
    fn at_age(value: T, age: Duration) -> Option<Self> {
        Instant::now().checked_sub(age).map(|at| Self { value, at })
    }

    /// The value if read within `max_age`, else None (treat as unread).
    fn fresh(&self, max_age: Duration) -> Option<T> {
        (self.at.elapsed() < max_age).then_some(self.value)
    }
}

/// Cadence for the keep-accessory geofence `state location` poll. Raw
/// GPS isn't bundled in `state drive`, so this is its own round-trip —
/// kept coarse (home/away changes slowly) and only run when the
/// keep-accessory feature is enabled, to spare BLE air time.
const LOCATION_POLL_INTERVAL: Duration = Duration::from_secs(30);

// Software version isn't sampled: `state software-update` returns only
// the pending OTA version, never the installed `car_version`. Users
// enter the running version manually (see fsd_versions.rs).

/// Backoff when another owner holds the BLE radio lock. Short so we
/// resume quickly when the keep-awake nudge releases.
const RADIO_CONTENDED_BACKOFF: Duration = Duration::from_secs(5);

/// How long to sleep when BLE is disabled in settings. Doesn't need
/// to be aggressive — settings changes are infrequent.
const DISABLED_POLL: Duration = Duration::from_secs(60);

/// Per-command "next due" timestamps for the Active-mode scheduler.
/// Each tick walks the poll types in priority order and runs any due;
/// `state drive` goes first (shiftState + locationName + odometer).
/// charge is staggered 30s off climate so the two big-payload calls
/// don't stack on one tick.
struct Schedule {
    next_drive: Instant,
    next_climate: Instant,
    next_charge: Instant,
    next_tires: Instant,
    /// `state closures` — read only for sentry_mode_state, which the
    /// quiet-mode gate needs each cycle.
    next_closures: Instant,
    /// Consecutive-failure counters for the fast-retry pattern: reset on
    /// success, increment on failure to drive 3s retries until
    /// MAX_FAST_RETRIES, then back off to normal cadence.
    drive_failures: u32,
    climate_failures: u32,
    charge_failures: u32,
    tires_failures: u32,
    closures_failures: u32,
}

impl Schedule {
    fn new(now: Instant) -> Self {
        Self {
            // Fire immediately on the first tick for a baseline snapshot
            // (incl. the sentry_mode + charging_state the quiet-mode gate
            // needs). charge waits 30s to stagger off climate.
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

    /// Next-due instant for a sub-sampler that just ran: normal interval
    /// on success, ~3s retry within MAX_FAST_RETRIES, else normal.
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
    /// `fast` = the last charge poll saw active DC fast charging; use the
    /// 15s cadence so the steep power taper is captured, else the 60s one.
    /// The fast-retry-on-failure path is unchanged.
    fn mark_charge(&mut self, now: Instant, success: bool, fast: bool) {
        self.charge_failures = if success { 0 } else { self.charge_failures.saturating_add(1) };
        let interval = if fast { CHARGE_FAST_INTERVAL } else { CHARGE_INTERVAL };
        self.next_charge = Self::next_after(now, success, self.charge_failures, interval);
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

    // Migration recovery: previous versions stopped sentryusb-ble.service
    // during Active mode to claim exclusive hci0. Existing installs that
    // are upgrading FROM that behavior may have sentryusb-ble in a
    // stopped state right now (we stopped it on the last cycle before
    // the upgrade, and never started it again because we got killed).
    //
    // One-shot start ensures the iOS GATT daemon is running by the time
    // we hand control to the main loop. From here on out we don't touch
    // sentryusb-ble at all — they coexist via BLE multi-role.
    //
    // Best-effort with a short timeout so a hung systemctl doesn't
    // block startup; sentryusb-ble.service has Restart=always anyway
    // so systemd will recover on the next start attempt regardless.
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(5),
        "systemctl",
        &["start", "sentryusb-ble"],
    )
    .await;

    // Brief startup wait for the clock (RTC or NTP) to dodge the first
    // cold-boot tick. 30s is enough because BLE clock sync (clock_sync.rs)
    // takes over once the first state response lands.
    wait_for_clock_sync(Duration::from_secs(30)).await;

    let conn = db::open()?;

    // Background diagnostic logger — one line/min to
    // /mutable/sentryusb-ble.log for the Logs → Bluetooth UI tab.
    // Independent of the main loop; own read-only DB handle.
    diag_log::spawn(sentryusb_drives::DEFAULT_DB_PATH.into());

    // IPC bridge for external BLE actions (sentryusb-ble-action from
    // run/awake_start), letting keep-awake nudges reuse our warm
    // PersistentSession instead of stopping us to grab the radio.
    let (action_tx, mut action_rx) = mpsc::channel::<action_socket::ActionRequest>(8);
    action_socket::spawn(action_tx);

    let mut held_radio = false;
    // Consecutive Park polls; crossing PARK_CONFIRMATIONS_BEFORE_QUIET
    // drops to sleep-safe body-controller polling. Reset by any non-Park
    // shift or a user_presence flip back to PRESENT.
    let mut parked_polls: u32 = 0;
    // Last user_presence from body-controller-state; detects "driver got
    // back in" to promote Quiet → Active on the next tick.
    let mut last_user_presence: Option<bool> = None;
    // Per-command Active-mode scheduler; persists across ticks. First
    // tick fires drive + climate + tires immediately, charge +30s.
    let mut schedule = Schedule::new(Instant::now());
    // Last parked-awake refresh (climate + charge while Quiet but
    // recording), keeping battery/temps fresh during Sentry/charging.
    let mut last_parked_awake_refresh: Option<Instant> = None;
    // Separate, rarer TPMS timer (30 min) — TPMS doesn't change while
    // parked. Bundled into the same Sample row when both fire together.
    let mut last_parked_awake_tpms_refresh: Option<Instant> = None;
    // Long-lived BLE session, lazy-spawned in tick() and reused to avoid
    // re-scan + re-handshake each cycle. Recreated if the VIN changes.
    let mut ble_session: Option<sample_ble::SessionHandle> = None;
    // Last charging_state from a successful `state charge` poll,
    // timestamped so gate consumers can expire it (GATE_READING_MAX_AGE).
    // Drives the quiet-mode gate: active charging keeps the car awake,
    // so quieting would leave battery_pct stale. `None` → assume
    // charging (stay Active until proven otherwise).
    let mut last_charging_state: Option<TimedReading<sample::ChargingState>> = None;
    // Same gate for sentry mode (from `state closures`): any non-Off
    // value keeps the car awake. `None` → assume on.
    let mut last_sentry_mode: Option<TimedReading<sample::SentryMode>> = None;
    // Throttle for the staying-Active gate log (~1/min).
    let mut last_gate_log: Option<Instant> = None;
    // Last known GPS from the drive poll (held across ticks; parked
    // polls legitimately omit coords). Feeds the keep-accessory geofence.
    let mut last_lat: Option<f64> = None;
    let mut last_lon: Option<f64> = None;
    // Last known reverse-geocoded address from the drive poll, held across
    // ticks. Tesla returns locationName only on `state drive`, which doesn't
    // run every tick — so a short charge (no drive poll) would otherwise get
    // a sample with no address. Held + stamped like lat/lon so every sample
    // carries the last-known address.
    let mut last_location_name: Option<String> = None;
    // Throttle for the geofence `state location` poll (keep-accessory only).
    let mut last_location_poll: Option<Instant> = None;
    // Keep-Accessory-Power automation policy state (see keep_accessory.rs).
    let mut keep_accessory_state = keep_accessory::KeepAccessoryState::default();
    // Sampler-emitted keep-awake nudge state (see #329 / nudge_keep_awake).
    // `next_nudge_due_at`: when the next `wake` action should fire; `None`
    // = fire on the first eligible tick. `nudge_retry_count`: consecutive
    // failures in the current 3-attempt cycle. `last_nudge_notification_at`:
    // dedup window for the "Keep awake failed (attempt 3/3)" push so a
    // prolonged outage emits at most one notification per 10 min.
    let mut next_nudge_due_at: Option<Instant> = None;
    let mut nudge_retry_count: u32 = 0;
    let mut last_nudge_notification_at: Option<Instant> = None;
    // Shared-key handoff with the ESP32-C6 (TELEMETRY_SOURCE=c6_primary only).
    let mut c6_link = C6Link::default();

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

    // Deadline for the next tick. The inter-tick wait is a select branch
    // (not a sleep inside a branch body), so IPC action requests and
    // signals are serviced immediately during the wait instead of
    // queueing behind it for up to DISABLED_POLL. This also means an
    // action no longer cancels an in-flight tick (the old racing-future
    // pattern dropped the tick mid-await, silently discarding whatever
    // sample data that tick had already collected).
    let mut next_tick_at = tokio::time::Instant::now();
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
                // Force every sub-sampler due now — no charge stagger, no
                // fast-retry gating. "Poll now" should return a full fresh
                // read in one cycle, not a battery field stale by 30s.
                schedule = Schedule::new(now);
                schedule.next_drive = now;
                schedule.next_climate = now;
                schedule.next_charge = now;
                schedule.next_closures = now;
                schedule.next_tires = now;
                last_parked_awake_refresh = None;
                last_parked_awake_tpms_refresh = None;
                // Reset parked_polls so the phase flips to Active even
                // from parked-confirmed Quiet (else we'd only fire a
                // body_controller call). Next Park observation re-ticks it.
                parked_polls = 0;
                // Fire the tick now rather than waiting out the current
                // inter-tick sleep (up to 30s in Quiet).
                next_tick_at = tokio::time::Instant::now();
            }
            Some(req) = action_rx.recv() => {
                // IPC: an external process (sentryusb-ble-action) wants a
                // one-shot action through our PersistentSession. Serializes
                // naturally with the select. Doesn't touch next_tick_at —
                // the schedule decides what's due, not the action.
                handle_action_request(
                    req,
                    &mut held_radio,
                    &mut ble_session,
                    &mut c6_link,
                ).await;
            }
            _ = tokio::time::sleep_until(next_tick_at) => {
                let (sleep, cfg) = tick(
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
                    &mut last_gate_log,
                    &mut last_lat,
                    &mut last_lon,
                    &mut last_location_name,
                    &mut last_location_poll,
                    &mut next_nudge_due_at,
                    &mut nudge_retry_count,
                    &mut last_nudge_notification_at,
                    &mut c6_link,
                ).await;
                // Keep-Accessory-Power automation runs after each tick
                // with the freshly-updated signals. Best-effort; gated
                // on the 12V flag + home geofence inside evaluate(), and
                // a no-op until both are configured. Reuses the config
                // snapshot the tick already parsed (None = load failed).
                if let Some(cfg) = cfg {
                    // Whoever owns the car sends it: the C6 (via its
                    // supervisor) while c6_primary owns, else our session.
                    let c6_owns = c6_link.c6_owns(&cfg);
                    let route = keep_accessory_route(
                        c6_owns,
                        c6_link.grant_live,
                        ble_session.as_ref().map(|h| &h.session),
                        &cfg.c6_supervisor_api,
                    );
                    if let Some(route) = route {
                        keep_accessory::evaluate(
                            &cfg.keep_accessory,
                            route,
                            &mut keep_accessory_state,
                            last_lat,
                            last_lon,
                            // "parked" must be HW3-robust: HW3 reports
                            // shift_state=Unknown when parked, so the
                            // shift-based counter never confirms. OR in
                            // car_truly_asleep (the daemon's own quiet
                            // signal) so home→OFF works on HW3 too.
                            parked_polls >= PARK_CONFIRMATIONS_BEFORE_QUIET
                                || (c6_owns && c6_link.parked_obs >= PARK_CONFIRMATIONS_BEFORE_QUIET)
                                || usb_watch::observe() == CarState::Asleep,
                            lock::is_archive_active(),
                            // The C6 holds its own link; "radio held" gates the Pi's.
                            held_radio || c6_owns,
                            // Deliberately RAW (not freshness-filtered):
                            // evaluate() reads this as "cable state", and a
                            // 10-min expiry to None mid-BLE-outage is
                            // indistinguishable from cable-unplugged — it
                            // would tear down an in-progress charge hold and
                            // re-fire its notifications on recovery. Stale
                            // exposure is bounded by its own CHARGE_HOLD_MAX.
                            last_charging_state.as_ref().map(|r| r.value),
                        )
                        .await;
                    }
                }
                next_tick_at = tokio::time::Instant::now() + sleep;
            }
        }
    }
}

/// Whether the gate may drop to sleep-permitting (Quiet) polling.
/// Quiet means "let the car sleep": permitted only when the car is
/// parked-or-asleep AND nothing has a reason to hold it awake — not
/// charging, sentry off, and no keep-awake (archive cycle or web-UI /
/// manual nudge) in effect.
///
/// The keep-awake term is the fix for letting a parked car sleep
/// mid-archive: an archive disconnects the USB gadget, so the car both
/// confirms Park (drive computer powers down → Unknown shift) and later
/// trips `car_truly_asleep` (cam_disk stops updating). The override has to
/// sit on the whole decision, not just one path, or a >5min archive slips
/// through the asleep door.
fn should_enter_quiet(
    car_truly_asleep: bool,
    parked_confirmed: bool,
    actively_charging: bool,
    sentry_on: bool,
    keep_awake_active: bool,
) -> bool {
    (car_truly_asleep || parked_confirmed)
        && !actively_charging
        && !sentry_on
        && !keep_awake_active
}

/// Resolve optional charge/sentry readings into the booleans that feed the
/// Quiet gate.
///
/// Unknown values stay conservative while the car is not yet parked-confirmed
/// and during explicit keep-awake work. Once the sampler has independently
/// confirmed the car is parked/asleep and keep-awake is not active, unread
/// charge/sentry values must not pin Active forever: doing so turns telemetry
/// itself into an accidental keep-awake loop when Tesla omits those optional
/// fields or encrypted responses fail to decode.
fn resolve_gate_inputs(
    car_truly_asleep: bool,
    parked_confirmed: bool,
    last_charging_state: Option<sample::ChargingState>,
    last_sentry_mode: Option<sample::SentryMode>,
    keep_awake_active: bool,
) -> (bool, bool) {
    let sleep_candidate = car_truly_asleep || parked_confirmed;
    let unknown_should_pin_active = keep_awake_active || !sleep_candidate;

    let actively_charging = last_charging_state
        .map(|s| s.is_active_charging())
        .unwrap_or(unknown_should_pin_active);
    let sentry_on = last_sentry_mode
        .map(|s| s.is_on())
        .unwrap_or(unknown_should_pin_active);

    (actively_charging, sentry_on)
}

/// Whether Quiet mode may run the parked-awake climate/charge/closures
/// refresh: the car must be provably awake, either recording clips
/// (cam-disk mtime fresh) or per the body controller's own sleep
/// status. BC AWAKE matters when the mtime lies — a charging car with
/// Sentry off writes no clips, so usb_watch reads Asleep/Idle while
/// the car is wide awake. Polling an awake car adds no wake-up drain.
fn should_refresh_parked_awake(observation: CarState, bc_awake: Option<bool>) -> bool {
    observation == CarState::Awake || bc_awake == Some(true)
}

/// One main-loop iteration; returns how long to sleep before the next,
/// plus the config snapshot it parsed (so the caller's keep-accessory
/// pass doesn't re-read the file; `None` = config load failed).
///
/// Two phases, decided each tick:
///   * **Active** — clip writes happening and shift_state not
///     confirmed-Park. Full `state` polls, radio held continuously.
///   * **Quiet** — car asleep OR Park for PARK_CONFIRMATIONS_BEFORE_QUIET
///     polls. Sleep-safe body-controller-state polls; radio released
///     between deep-asleep polls (for iOS GATT) but held during
///     parked-with-Sentry (cadence too fast to cycle GATT).
///
/// Active → Quiet when parked_polls hits the count; Quiet → Active when
/// user_presence flips NOT_PRESENT → PRESENT.
async fn tick(
    conn: &Connection,
    held_radio: &mut bool,
    parked_polls: &mut u32,
    last_user_presence: &mut Option<bool>,
    schedule: &mut Schedule,
    last_parked_awake_refresh: &mut Option<Instant>,
    last_parked_awake_tpms_refresh: &mut Option<Instant>,
    ble_session: &mut Option<sample_ble::SessionHandle>,
    last_charging_state: &mut Option<TimedReading<sample::ChargingState>>,
    last_sentry_mode: &mut Option<TimedReading<sample::SentryMode>>,
    last_gate_log: &mut Option<Instant>,
    last_lat: &mut Option<f64>,
    last_lon: &mut Option<f64>,
    last_location_name: &mut Option<String>,
    last_location_poll: &mut Option<Instant>,
    next_nudge_due_at: &mut Option<Instant>,
    nudge_retry_count: &mut u32,
    last_nudge_notification_at: &mut Option<Instant>,
    c6_link: &mut C6Link,
) -> (Duration, Option<BleConfig>) {
    let mut cfg = match BleConfig::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to load BLE config: {e}");
            return (DISABLED_POLL, None);
        }
    };
    enforce_backfill_rule(&mut cfg);

    // Disabled / unconfigured checks BEFORE the session spawn. The old
    // order ensured the session first, which (a) kept a warm GATT
    // connection to the car alive forever after the user disabled BLE
    // in settings, and (b) on a Pi with no BLE key file turned the
    // intended 60s idle poll into a 5s retry spin with a warning logged
    // every cycle.
    // C6-primary with the sampler idle (BLE off / no VIN): the C6 may own the car.
    if (!cfg.enabled || cfg.vin.is_empty()) && cfg.c6_primary && !cfg.c6_backfill {
        c6_release(ble_session, held_radio, c6_link).await;
    }
    if !cfg.enabled {
        if *held_radio {
            info!("BLE disabled in settings — releasing radio");
            release_radio().await;
            *held_radio = false;
        }
        // Drop the persistent session so disabling BLE actually closes
        // the BLE connection to the car.
        *ble_session = None;
        *parked_polls = 0;
        *last_user_presence = None;
        return (DISABLED_POLL, Some(cfg));
    }
    if cfg.vin.is_empty() {
        debug!("no TESLA_BLE_VIN configured, idling");
        if *held_radio {
            release_radio().await;
            *held_radio = false;
        }
        *ble_session = None;
        *parked_polls = 0;
        *last_user_presence = None;
        return (DISABLED_POLL, Some(cfg));
    }

    // C6 plugged in and provisioned, source unset: opt this box in (next tick).
    if !cfg.c6_primary {
        c6_onboarding(&cfg, c6_link);
    }

    // Shared-key coordination: decide who may talk to the car BEFORE any
    // session exists. C6-owned => no Tesla BLE from this process at all.
    if cfg.c6_primary && !cfg.c6_backfill && !c6_coord::c6_present() {
        // No C6 plugged in: stock behaviour, no failback wait.
        c6_link.coord.mark_absent();
    } else if cfg.c6_primary && !cfg.c6_backfill {
        let snap = c6_source::read_snapshot();
        // No boottime => time never advances => stays C6-owned (safe side).
        let now = c6_coord::boottime_ms().unwrap_or(0);
        let prev = c6_link.coord.owner();
        let health = c6_coord::Health {
            device_alive: c6_coord::c6_alive(snap.as_ref()),
            // Known awake: recording clips, or a keep-awake/archive holds it up
            // (an archive unplugs the gadget, so clips alone can't show that).
            car_link_down: c6_coord::car_link_down(
                snap.as_ref(),
                usb_watch::observe() == CarState::Awake || lock::keep_awake_requested(),
            ),
        };
        let owner = c6_link.coord.step_health(health, now);
        if owner != prev {
            info!(
                "C6 coordination: car link owner {} -> {} ({:?})",
                prev.as_str(),
                owner.as_str(),
                c6_link.coord.reason()
            );
        }
        match owner {
            c6_coord::Owner::C6 => {
                c6_release(ble_session, held_radio, c6_link).await;
                *parked_polls = 0;
                *last_user_presence = None;
                let sleep = c6_owned_tick(
                    conn,
                    &cfg,
                    snap.as_ref(),
                    c6_link,
                    last_charging_state,
                    last_sentry_mode,
                    last_lat,
                    last_lon,
                    last_location_name,
                    next_nudge_due_at,
                    nudge_retry_count,
                    last_nudge_notification_at,
                )
                .await;
                return (sleep, Some(cfg));
            }
            c6_coord::Owner::Sampler => c6_link.parked_obs = 0, // C6 evidence is stale now
        }
    }
    // Lease first, so the supervisor holds before we connect; no lease, no car.
    if !c6_claim_for_sampler(&cfg, c6_link) {
        return (Duration::from_secs(5), Some(cfg));
    }

    // Lazy-spawn / recreate-on-VIN-change the persistent BLE session.
    // Cheap when it exists (a VIN compare); the first call does the
    // key-load + scan + connect + handshake.
    if let Err(e) = sample_ble::ensure_session_for(ble_session, &cfg.vin, Some(&cfg.adapter)) {
        warn!("could not start PersistentSession (will retry next tick): {e:#}");
        return (Duration::from_secs(5), Some(cfg));
    }
    let session = &ble_session
        .as_ref()
        .expect("ensure_session_for set it on success")
        .session;

    let observation = usb_watch::observe();
    let car_truly_asleep = observation == CarState::Asleep;
    let parked_confirmed = *parked_polls >= PARK_CONFIRMATIONS_BEFORE_QUIET;

    // A keep-awake in effect — an archive cycle or any nudge loop (web-UI,
    // drive processing) — must pin us Active. Dropping to sleep-safe
    // polling mid-archive lets the car sleep, which cuts USB power and
    // aborts the copy. Self-clears when the work finishes, so it can't
    // wedge the car permanently awake (see lock::keep_awake_requested).
    let keep_awake_active = lock::keep_awake_requested();

    // Freshness-filtered gate inputs: a reading older than
    // GATE_READING_MAX_AGE predates a sustained poll outage, so the
    // gate treats it as unread rather than letting a stale Charging /
    // SentryOn pin the car Active forever.
    let charging_fresh = last_charging_state
        .as_ref()
        .and_then(|r| r.fresh(GATE_READING_MAX_AGE));
    let sentry_fresh = last_sentry_mode
        .as_ref()
        .and_then(|r| r.fresh(GATE_READING_MAX_AGE));

    let (actively_charging, sentry_on) = resolve_gate_inputs(
        car_truly_asleep,
        parked_confirmed,
        charging_fresh,
        sentry_fresh,
        keep_awake_active,
    );

    // Keep-awake state cleanup happens here; the nudge itself fires
    // AFTER the polls below (see "Sampler keep-awake CPC dispatch"
    // comment further down). The two halves are separated because the
    // pre-poll location used to fire the nudge against a session that
    // might still be in mid-reconnect, which on AIC8800 / UART-BT boards
    // returned `le-connection-abort-by-local` and burned the 3-retry
    // budget every cycle. Firing after a successful poll guarantees the
    // session is alive at the moment we send the action — the verb call
    // reuses the same warm GATT link instead of triggering a fresh
    // scan/connect that races the chip-firmware HCI gaps.
    if !keep_awake_active {
        // Reset cycle state once the keep-awake reason clears so the next
        // archive starts at a clean 0/3, immediate-fire baseline.
        *next_nudge_due_at = None;
        *nudge_retry_count = 0;
    }

    // Two paths to quiet mode: car_truly_asleep (no recent clip writes)
    // or parked_confirmed (Park 3+ polls). Both also require the car isn't
    // charging, running sentry, or being held awake for an archive — those
    // keep it awake, and quieting would leave battery_pct/sentry stale or
    // break the copy.
    let in_quiet_mode = should_enter_quiet(
        car_truly_asleep,
        parked_confirmed,
        actively_charging,
        sentry_on,
        keep_awake_active,
    );

    // Diagnostic: the car is parked/asleep but we're staying Active
    // because charging or sentry says it has a reason to be awake. This
    // is the #1 "why won't my car sleep?" question — surface which
    // signal pinned us Active and, crucially, whether it's a real
    // reading or the conservative unknown-default (`None` → assume on).
    // A persistent `[DEFAULTED]` here means Tesla isn't reporting that
    // field over BLE (it drops optional fields), so the car can never
    // qualify for Quiet. Throttled to ~once/min.
    if !in_quiet_mode {
        let now = Instant::now();
        let due = last_gate_log
            .map(|t| now.duration_since(t) >= Duration::from_secs(60))
            .unwrap_or(true);
        if due {
            *last_gate_log = Some(now);
            let sentry_src = if sentry_fresh.is_some() {
                "read"
            } else if last_sentry_mode.is_some() {
                "STALE: treated unread"
            } else {
                "DEFAULTED: unread"
            };
            let charge_src = if charging_fresh.is_some() {
                "read"
            } else if last_charging_state.is_some() {
                "STALE: treated unread"
            } else {
                "DEFAULTED: unread"
            };
            // Quiet needs (asleep || parked_confirmed) && !charge &&
            // !sentry && !keep_awake. Log all five so a stuck-Active gate
            // is diagnosable; keep_awake_active=true is an archive/nudge
            // hold (the deliberate stay-awake during a copy).
            info!(
                "gate: staying Active — car_truly_asleep={}, parked_polls={}/{}, \
                 sentry_on={} [{}], actively_charging={} [{}], keep_awake_active={}",
                car_truly_asleep,
                *parked_polls,
                PARK_CONFIRMATIONS_BEFORE_QUIET,
                sentry_on,
                sentry_src,
                actively_charging,
                charge_src,
                keep_awake_active,
            );
        }
    }

    if in_quiet_mode {
        // Sleep-safe path: acquire the radio for the brief BC call, then
        // release if truly asleep (so iOS GATT returns). Keep it held in
        // parked-confirmed — cycling GATT at the poll cadence is wasteful.
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
                    // info-level: a held radio (e.g. archiveloop's
                    // keep_awake during archive cycles) is a common reason
                    // quiet samples go missing — surface it as "waiting",
                    // not "broken".
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
            // Set when the BC poll failed at the connect layer (scan /
            // adapter / GATT connect). Every further BLE call this tick
            // would redo the same 30s scan + backoff against an
            // unreachable car, so the follow-up probes are skipped.
            let mut connect_failed = false;
            // Always probe body-controller first — it's the
            // canonical source of user_presence and is sleep-safe.
            // `bc_awake` is the car's own sleep status (VCSEC), which
            // stays truthful when the cam-disk mtime doesn't: a
            // charging car with Sentry off writes no clips, so
            // usb_watch reads Asleep while the car is wide awake.
            let (presence_now, bc_awake) =
                match sample_ble::sample_body_controller_ble(session).await {
                    Ok(bc) => {
                        let p = bc.user_presence;
                        let awake = bc.awake;
                        persist(conn, bc.sample);
                        (p, awake)
                    }
                    Err(e) => {
                        connect_failed =
                            sentryusb_tesla_ble::manager::is_connect_failure(&e);
                        warn!("sample_body_controller failed: {e:#}");
                        (*last_user_presence, None)
                    }
                };

            // Driver-got-back-in: user_presence NOT_PRESENT → PRESENT.
            // Promote to Active immediately (short Duration → state poll
            // next tick instead of a full QUIET_INTERVAL).
            if *last_user_presence == Some(false) && presence_now == Some(true) {
                info!("user_presence flipped PRESENT — resuming full state polls");
                *parked_polls = 0;
                *last_user_presence = presence_now;
                if car_truly_asleep && bc_awake != Some(true) {
                    // Same guard as the release at the bottom of the
                    // quiet path: a fresh BC AWAKE overrides a stale
                    // cam mtime, and releasing here just hands another
                    // process a window to grab the lock before the
                    // Active tick re-acquires it.
                    release_radio().await;
                    *held_radio = false;
                }
                // 1s so the OS scheduler gets a moment; effectively
                // immediate next tick → state poll.
                return (Duration::from_secs(1), Some(cfg));
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
            if !connect_failed && presence_now == Some(true) {
                match sample_ble::sample_drive_ble(session).await {
                    Ok(d) => {
                        if cfg.experimental {
                            sample_ble::log_drive_detail(&d);
                        }
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
                            return (Duration::from_secs(1), Some(cfg));
                        }
                    }
                    Err(e) => {
                        warn!("state drive probe in quiet+present failed: {e:#}");
                    }
                }
            }

            // Parked-awake state refresh: when the car is parked
            // (Quiet mode) but provably awake — recording dashcam
            // clips (observation == Awake) OR the body controller
            // reports AWAKE (covers charging with Sentry off, where
            // no clips are written and the cam mtime lies) — do a
            // periodic climate + charge poll so battery/temps don't
            // go indefinitely stale during Sentry sessions or AC
            // charging. Safe because the car is already awake — we
            // add no wake-up drain. Only fires every 3 min to keep
            // BLE load minimal.
            //
            // Runs even with the user in the car: the drive probe
            // above already early-returned to Active on an actual
            // shift out of Park, so reaching here means still-parked
            // — and someone sitting in a charging car shouldn't
            // freeze the charge telemetry.
            if !connect_failed && should_refresh_parked_awake(observation, bc_awake) {
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
                                if cfg.experimental {
                                    sample_ble::log_climate_detail(&c);
                                }
                                try_sync_clock(c.meta);
                                refresh.interior_temp_c = c.interior_temp_c;
                                refresh.exterior_temp_c = c.exterior_temp_c;
                                refresh.hvac_on = c.hvac_on;
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake climate refresh failed: {e:#}"),
                        }
                        match sample_ble::sample_charge_ble(session).await {
                            Ok(c) => {
                                if cfg.experimental {
                                    sample_ble::log_charge_detail(&c);
                                }
                                try_sync_clock(c.meta);
                                refresh.battery_pct = c.battery_pct;
                                // Persist the phase here too (v14): if a charge
                                // ends while the car stays parked-awake (e.g.
                                // Sentry), this quiet refresh is what writes the
                                // stopped/complete row that drops the banner.
                                refresh.charging_state =
                                    c.charging_state.map(|cs| cs.as_db_str().to_string());
                                // Also refresh the gate input so a
                                // charge that starts mid-quiet bumps
                                // us back to Active on the next tick.
                                if let Some(cs) = c.charging_state {
                                    *last_charging_state = Some(TimedReading::now(cs));
                                }
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake charge refresh failed: {e:#}"),
                        }
                        // Closures refresh — gives us a sentry_mode
                        // update so a remotely-enabled sentry session
                        // also bumps us back to Active. No persisted
                        // fields, so this doesn't affect `any_ok`.
                        match sample_ble::sample_closures_ble(session).await {
                            Ok(c) => {
                                if cfg.experimental {
                                    sample_ble::log_closures_detail(&c);
                                }
                                try_sync_clock(c.meta);
                                if let Some(sm) = c.sentry_mode {
                                    *last_sentry_mode = Some(TimedReading::now(sm));
                                }
                            }
                            Err(e) => warn!("parked-awake closures refresh failed: {e:#}"),
                        }
                        // Location not refreshed: Tesla only returns
                        // location_name in `state drive`, which Quiet
                        // doesn't call. Fine — parked means not moving.
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
                            Err(e) => warn!("parked-awake tires refresh failed: {e:#}"),
                        }
                        *last_parked_awake_tpms_refresh = Some(Instant::now());
                    }

                    if any_ok {
                        persist(conn, refresh);
                    }
                }
            }

            *last_user_presence = presence_now;
            if car_truly_asleep && bc_awake != Some(true) {
                // Deep sleep + no user → release the inter-process
                // radio lock between polls. Skipped when the body
                // controller says the car is actually awake (stale
                // cam mtime, e.g. charging with Sentry off) — the
                // 3-min refresh wants the lock every tick, and
                // dropping it just invites lock churn with the other
                // BLE processes.
                release_radio().await;
                *held_radio = false;
            }
        }

        // Keep the gate snapshot live in Quiet too — this is where a
        // reading crosses GATE_READING_MAX_AGE, and the (stale) marker
        // is the diagnostic for "why did the gate stop honoring it".
        // `shift` is None truthfully: no Active drive poll ran.
        write_gate_status_file(
            last_sentry_mode.as_ref(),
            last_charging_state.as_ref(),
            None,
        );
        (QUIET_INTERVAL, Some(cfg))
    } else {
        // Active mode — scheduler-driven multi-poll. Each tick runs the
        // overdue sub-samplers in priority order, `state drive` first
        // (shiftState + location + odometer); the rest on slower cadences.
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
                    return (RADIO_CONTENDED_BACKOFF, Some(cfg));
                }
                Err(e) => {
                    warn!("failed to acquire radio lock: {e}");
                    return (RADIO_CONTENDED_BACKOFF, Some(cfg));
                }
            }
        }

        let tick_now = Instant::now();
        // First tick after a long Quiet period: next_drive is very stale
        // (Quiet never calls mark_drive). Reset so the stagger returns
        // and all sub-samplers fire now for a fresh snapshot.
        if tick_now.duration_since(schedule.next_drive)
            > Duration::from_secs(2 * DRIVE_INTERVAL.as_secs())
        {
            *schedule = Schedule::new(tick_now);
        }
        // One Sample built across the sub-samplers that ran this tick;
        // unran/failed fields stay None (schema + aggregator handle NULLs).
        let mut sample = Sample {
            ts: sample::now_secs(),
            source: "state".into(),
            ..Sample::default()
        };
        let mut shift_state_observed: Option<sample::ShiftState> = None;
        let mut any_call_ran = false;
        // Bench/soak backfill only (C6_BACKFILL=1): read the co-processor
        // snapshot once and let fresh C6 domains skip their BLE poll. Never in
        // shared-key production, where this tick only runs once the sampler
        // owns the car (see c6_coord) and polls everything itself.
        let c6_backfill = cfg.c6_primary && cfg.c6_backfill;
        let c6_snap = if c6_backfill { c6_source::read_snapshot() } else { None };
        // Only stamped in backfill mode; every other path never reads it.
        let now_ms = if c6_backfill {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        } else {
            0
        };
        // Set when a sub-sampler failed at the connect layer (scan /
        // adapter / GATT connect) — the car is unreachable, so each
        // remaining sub-sampler this tick would redo the same 30s scan
        // + backoff. Skip them; their next_due stays in the past, so
        // they re-run on the next tick once the car is reachable.
        let mut connect_failed = false;

        // ── 1. DRIVE (priority) ── shiftState, locationName, odometer.
        if schedule.drive_due(tick_now) {
            let success = if c6_backfill
                && c6_took_drive(&c6_snap, now_ms, &mut sample, &mut shift_state_observed)
            {
                true
            } else { match sample_ble::sample_drive_ble(session).await {
                Ok(d) => {
                    if cfg.experimental {
                        sample_ble::log_drive_detail(&d);
                    }
                    try_sync_clock(d.meta);
                    sample.odometer_mi = d.odometer_mi;
                    // Hold the last-known address across ticks (the stamp
                    // below applies it to every sample, so a charge-only
                    // tick still carries the address).
                    if d.location_name.is_some() {
                        *last_location_name = d.location_name.clone();
                    }
                    shift_state_observed = d.shift_state;
                    // Hold last-known GPS across ticks (parked polls omit
                    // coords); feeds the keep-accessory geofence.
                    if d.lat.is_some() {
                        *last_lat = d.lat;
                    }
                    if d.lon.is_some() {
                        *last_lon = d.lon;
                    }
                    true
                }
                Err(e) => {
                    if sentryusb_tesla_ble::manager::is_connect_failure(&e) {
                        connect_failed = true;
                    }
                    warn!("sample_drive failed: {e:#}");
                    false
                }
            }};
            // Fast-retry on failure (~3s), normal interval on
            // success. See Schedule::next_after for the pattern.
            schedule.mark_drive(tick_now, success);
            any_call_ran = true;
        }

        // ── 1b. LOCATION (raw GPS) ── `state drive` returns the address
        // name but NOT raw coords when parked, so lat/lon need their own
        // `state location` round-trip. Three consumers: the keep-accessory
        // home geofence, the automatic Away Mode geofence (the API server
        // reads the GPS file we write below), and the charging-map pin
        // (which needs coords, not just the address). Run it when either
        // geofence feature is on OR the car is actively charging — so a
        // charge gets a map pin without polling GPS on every idle parked
        // car. Coarse cadence; pure input (not a DB sample) — doesn't set
        // any_call_ran.
        let charging_now = last_charging_state
            .as_ref()
            .and_then(|r| r.fresh(GATE_READING_MAX_AGE))
            .map(|s| s.is_active_charging())
            .unwrap_or(false);
        if !connect_failed
            && (cfg.keep_accessory.enabled || cfg.away_auto_enabled || charging_now)
        {
            let due = last_location_poll
                .map(|t| tick_now.duration_since(t) >= LOCATION_POLL_INTERVAL)
                .unwrap_or(true);
            if due {
                match sample_ble::sample_location_ble(session).await {
                    Ok((la, lo)) => {
                        if la.is_some() {
                            *last_lat = la;
                        }
                        if lo.is_some() {
                            *last_lon = lo;
                        }
                        *last_location_poll = Some(tick_now);
                        info!(
                            "state-poll: location gps=({}, {})",
                            la.map(|v| format!("{v:.6}")).unwrap_or_else(|| "?".into()),
                            lo.map(|v| format!("{v:.6}")).unwrap_or_else(|| "?".into()),
                        );
                        // Expose the current fix for the web UI's "Use
                        // current location" button (sets the home geofence).
                        // Best-effort; tiny JSON on the writable /mutable.
                        if let (Some(la), Some(lo)) = (la, lo) {
                            let json = format!(
                                "{{\"lat\":{la},\"lon\":{lo},\"ts\":{}}}\n",
                                sample::now_secs()
                            );
                            let _ = std::fs::write("/mutable/keep_accessory_gps.json", json);
                        }
                    }
                    Err(e) => warn!("state-poll: location failed: {e:#}"),
                }
            }
        }

        // ── 2. CLIMATE (every 60s) ──
        if (c6_backfill || !connect_failed) && schedule.climate_due(tick_now) {
            let success = if c6_backfill && c6_took_climate(&c6_snap, now_ms, &mut sample) {
                true
            } else if connect_failed {
                false // BLE unreachable this tick (flag-off only; C6 tried above)
            } else { match sample_ble::sample_climate_ble(session).await {
                Ok(c) => {
                    if cfg.experimental {
                        sample_ble::log_climate_detail(&c);
                    }
                    try_sync_clock(c.meta);
                    sample.interior_temp_c = c.interior_temp_c;
                    sample.exterior_temp_c = c.exterior_temp_c;
                    sample.hvac_on = c.hvac_on;
                    true
                }
                Err(e) => {
                    if sentryusb_tesla_ble::manager::is_connect_failure(&e) {
                        connect_failed = true;
                    }
                    warn!("sample_climate failed: {e:#}");
                    false
                }
            }};
            schedule.mark_climate(tick_now, success);
            any_call_ran = true;
        }

        // ── 3. CHARGE (every 60s, or 15s while DC fast charging) ──
        if (c6_backfill || !connect_failed) && schedule.charge_due(tick_now) {
            // Set in the Ok arm when this poll sees DC fast charging; picks
            // the 15s vs 60s next-charge cadence in `mark_charge` below.
            let mut fast_charging = false;
            let success = if c6_backfill
                && c6_took_charge(&c6_snap, now_ms, &mut sample, last_charging_state, &mut fast_charging)
            {
                true
            } else if connect_failed {
                false // BLE unreachable this tick (flag-off only; C6 tried above)
            } else { match sample_ble::sample_charge_ble(session).await {
                Ok(c) => {
                    if cfg.experimental {
                        sample_ble::log_charge_detail(&c);
                    }
                    // Persist the charging detail (v11 columns). Charging is a
                    // standard feature now (graduated off the experimental
                    // flag), so these are always written — NULL only when the
                    // car isn't reporting charge data.
                    let d = &c.detail;
                    sample.charger_power_kw = d.charger_power_kw;
                    sample.charger_actual_current_a = d.charger_actual_current_a;
                    sample.charger_voltage_v = d.charger_voltage_v;
                    sample.charge_rate_mph = d.charge_rate_mph;
                    sample.charge_energy_added_kwh = d.charge_energy_added_kwh;
                    sample.charge_limit_soc = d.charge_limit_soc;
                    sample.battery_range_mi = d.battery_range_mi;
                    sample.charging_amps_set = d.charging_amps_set;
                    sample.charge_current_request_max = d.charge_current_request_max;
                    sample.charge_port_door_open = d.charge_port_door_open;
                    sample.charge_minutes_to_full = d.minutes_to_full_charge;
                    // DC fast charging = actively charging AND power above
                    // the AC Level 2 ceiling. Drives the 15s poll cadence so
                    // the steep Supercharger taper is captured (60s smears
                    // it and misses the early peak). Reverts automatically
                    // once power drops or the charge ends.
                    fast_charging = c
                        .charging_state
                        .map(|s| s.is_active_charging())
                        .unwrap_or(false)
                        && d.charger_power_kw.is_some_and(|p| p > FAST_CHARGE_THRESHOLD_KW);
                    // Persist the charge phase (v14) so /api/charging/current
                    // can keep the dashboard banner up the whole charge across
                    // BLE sampler dropouts — and only drop it when a poll
                    // actually reports a stopped/complete phase.
                    sample.charging_state = c.charging_state.map(|cs| cs.as_db_str().to_string());
                    try_sync_clock(c.meta);
                    sample.battery_pct = c.battery_pct;
                    // Refresh the gate input on success; keep the previous
                    // value on failure (don't force an Active burst on one
                    // transient miss).
                    if let Some(cs) = c.charging_state {
                        *last_charging_state = Some(TimedReading::now(cs));
                    }
                    true
                }
                Err(e) => {
                    if sentryusb_tesla_ble::manager::is_connect_failure(&e) {
                        connect_failed = true;
                    }
                    warn!("sample_charge failed: {e:#}");
                    false
                }
            }};
            schedule.mark_charge(tick_now, success, fast_charging);
            any_call_ran = true;
        }

        // ── 4. CLOSURES (every 60s) ── consumed only for sentry_mode
        // (the quiet-mode gate); door/window/port state is in the same
        // response if the UI ever needs it.
        if !connect_failed && schedule.closures_due(tick_now) {
            let success = match sample_ble::sample_closures_ble(session).await {
                Ok(c) => {
                    if cfg.experimental {
                        sample_ble::log_closures_detail(&c);
                    }
                    try_sync_clock(c.meta);
                    if let Some(sm) = c.sentry_mode {
                        *last_sentry_mode = Some(TimedReading::now(sm));
                    }
                    true
                }
                Err(e) => {
                    if sentryusb_tesla_ble::manager::is_connect_failure(&e) {
                        connect_failed = true;
                    }
                    warn!("sample_closures failed: {e:#}");
                    false
                }
            };
            schedule.mark_closures(tick_now, success);
            any_call_ran = true;
        }

        // ── 5. TIRES (every 5 min) ──
        if (c6_backfill || !connect_failed) && schedule.tires_due(tick_now) {
            let success = if c6_backfill && c6_took_tires(&c6_snap, now_ms, &mut sample) {
                true
            } else if connect_failed {
                false // BLE unreachable this tick (flag-off only; C6 tried above)
            } else { match sample_ble::sample_tires_ble(session).await {
                Ok(t) => {
                    try_sync_clock(t.meta);
                    sample.tire_fl_psi = t.tire_fl_psi;
                    sample.tire_fr_psi = t.tire_fr_psi;
                    sample.tire_rl_psi = t.tire_rl_psi;
                    sample.tire_rr_psi = t.tire_rr_psi;
                    true
                }
                Err(e) => {
                    warn!("sample_tires failed: {e:#}");
                    false
                }
            }};
            schedule.mark_tires(tick_now, success);
            any_call_ran = true;
        }

        // Parked = not in a driving gear. Park or Unknown both count:
        // Intel-MCU Teslas report the gear as Invalid/SNA (→ Unknown) once
        // the drive computer powers down on parking, while a moving car
        // always reports a real gear. Drive/Reverse/Neutral reset; None
        // (drive poll didn't run / failed) leaves the counter alone.
        match shift_state_observed {
            Some(
                sample::ShiftState::Drive
                | sample::ShiftState::Reverse
                | sample::ShiftState::Neutral,
            ) => {
                *parked_polls = 0;
            }
            Some(_) => {
                // Park, or Unknown (drive computer asleep).
                *parked_polls = parked_polls.saturating_add(1);
                if *parked_polls == PARK_CONFIRMATIONS_BEFORE_QUIET {
                    // One-shot on first confirm; charge/sentry/keep-awake
                    // decide the next tick.
                    if actively_charging || sentry_on || keep_awake_active {
                        info!(
                            "{} consecutive parked observations — but staying Active \
                             (actively_charging={}, sentry_on={}, keep_awake_active={}); \
                             car is awake for a reason, quiet polling would freeze \
                             battery/sentry signals or break an in-flight archive",
                            PARK_CONFIRMATIONS_BEFORE_QUIET,
                            actively_charging,
                            sentry_on,
                            keep_awake_active,
                        );
                    } else {
                        info!(
                            "{} consecutive parked observations — dropping to body-controller polling so the car can sleep",
                            PARK_CONFIRMATIONS_BEFORE_QUIET
                        );
                    }
                }
            }
            None => {
                // Drive poll didn't run / failed — leave the counter alone.
            }
        }

        // Clear user_presence so the next Quiet entry starts from a
        // fresh baseline before the "got back in" check.
        *last_user_presence = None;

        // Persist whatever this tick collected; sparse rows (e.g.
        // drive-only) are handled downstream.
        if any_call_ran {
            // Stamp the held last-known GPS onto the row so a parked-and-
            // charging sample carries the charger's location for the
            // charging-view map pin. Parked polls omit fresh coords, so
            // `*last_lat`/`*last_lon` (held across ticks) is the right
            // source. Always written now (charging graduated off the flag);
            // NULL until a location poll has supplied coords.
            sample.latitude = *last_lat;
            sample.longitude = *last_lon;
            // Stamp the held address too (same rationale as lat/lon): a
            // charge-only tick where the drive poll didn't run still gets
            // the last-known address, so a short charge shows it. Only
            // overwrite from the held value when this tick didn't already
            // set one.
            if sample.location_name.is_none() {
                sample.location_name = last_location_name.clone();
            }
            persist(conn, sample);
        }

        // Sampler keep-awake CPC dispatch (task #336 / supervisor pattern).
        //
        // We're at the end of an active tick — telemetry polls have run on
        // the held session, which means the BLE link to the car is verified
        // warm right now. This is the safe moment to piggy-back a
        // `charge-port-close` action onto the same session: it reuses the
        // already-handshook GATT path, no fresh scan-and-connect, no race
        // with PersistentSession's reconnect logic.
        //
        // Single-verb design (CPC only, no `wake`, no `combo`):
        //   * On-vehicle measurement 2026-06-23 (Phases 1–2 of #336 in-car
        //     test): CPC every 60s held a parked Tesla 2026.20.3 `online`
        //     for 1h+ continuous — 5× the post-2026.14.3 median online
        //     window of 12 min. 25 CPCs in Phase 1, 34 more in Phase 2,
        //     all returned "action OK; decrypted response = 22 bytes."
        //   * `wake` alone is unreliable on parked cars (timed out in
        //     Stage 1 isolation test); `combo` triggers a cross-domain
        //     race that produces `le-connection-abort-by-local` 13 s
        //     after the wake step. Neither outperforms CPC alone.
        //   * 60 s cadence: well inside Tesla's post-2026.14.3 ~18 min
        //     "falling asleep" window, picked aggressive enough to absorb
        //     a single failed nudge without the cycle expiring.
        //
        // Failure budget: 3 attempts at 30 s spacing, then a 10-min-dedup
        // push notification + 60 s back-off. Matches the legacy bash
        // path's user-visible behavior so SC's notification regex still
        // matches.
        if keep_awake_active {
            let now = Instant::now();
            let due = next_nudge_due_at.map(|t| now >= t).unwrap_or(true);
            if due {
                let interval = Duration::from_secs(cfg.keep_awake_interval_secs);
                let result = session
                    .send_action(sentryusb_tesla_ble::actions::charge_port_close())
                    .await;
                match result {
                    Ok(_) => {
                        info!(
                            "keep-awake: charge-port-close nudge sent (next in {}s)",
                            cfg.keep_awake_interval_secs
                        );
                        *next_nudge_due_at = Some(now + interval);
                        *nudge_retry_count = 0;
                    }
                    Err(e) => {
                        *nudge_retry_count += 1;
                        warn!(
                            "keep-awake: charge-port-close nudge failed \
                             (attempt {}/3): {:#}",
                            *nudge_retry_count, e
                        );
                        // v3.11.17 tactical recovery: after two consecutive
                        // in-band nudge failures, ask PersistentSession to
                        // force-close its current connection so the next
                        // command reopens via the normal ensure_connected
                        // path. Targets the "sampler thinks it has a
                        // connection but writes fail / next nudge stuck
                        // behind bad state" case observed on BCM Pi 5 +
                        // Rock 4C+ even after the v3.11.16 query-timeout
                        // absorption shipped. NOT expected to fix slot
                        // contention — if the car is genuinely refusing
                        // fresh connects, the subsequent reconnect will
                        // fail too. Cooldown-gated inside the handler
                        // (FORCE_RECONNECT_COOLDOWN = 90s) so a stuck
                        // window doesn't thrash the radio.
                        if *nudge_retry_count == 2 {
                            match session
                                .force_reconnect(format!(
                                    "keep-awake nudge fail 2/3: {:#}",
                                    e
                                ))
                                .await
                            {
                                Ok(outcome) => info!(
                                    "keep-awake: ForceReconnect outcome={:?}",
                                    outcome
                                ),
                                Err(fe) => warn!(
                                    "keep-awake: ForceReconnect send failed: {:#}",
                                    fe
                                ),
                            }
                        }
                        if *nudge_retry_count >= 3 {
                            let notify_due = last_nudge_notification_at
                                .map(|t| now.duration_since(t) >= Duration::from_secs(600))
                                .unwrap_or(true);
                            if notify_due {
                                emit_keep_awake_failure_notification(&format!("{:#}", e));
                                *last_nudge_notification_at = Some(now);
                            }
                            // Reset retry count and back off to normal
                            // cadence — don't spam the link with 30 s
                            // retries once the budget is exhausted.
                            *nudge_retry_count = 0;
                            *next_nudge_due_at = Some(now + interval);
                        } else {
                            *next_nudge_due_at = Some(now + Duration::from_secs(30));
                        }
                    }
                }
            }
        }

        // Live gate snapshot for the BLE card (not the DB).
        write_gate_status_file(
            last_sentry_mode.as_ref(),
            last_charging_state.as_ref(),
            shift_state_observed,
        );

        // Sleep until the next sub-sampler is due (usually drive, 15s).
        let next = schedule.next_due();
        let after = Instant::now();
        let sleep = if next > after {
            next.duration_since(after)
        } else {
            // Already overdue — tick again immediately.
            Duration::from_millis(100)
        };
        (sleep, Some(cfg))
    }
}

/// Block until the clock looks correct (year >= 2025 or timesyncd's
/// synced marker), or `timeout` elapses. Without an RTC battery the Pi
/// boots with a years-off clock until NTP catches up, and samples with
/// bad timestamps fall outside any drive window and are unrecoverable —
/// so don't sample until the clock is sane.
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

/// Live gate inputs for the BLE card, overwritten each Active tick (not
/// persisted). `unknown` = no value read from the car yet.
const GATE_STATUS_PATH: &str = "/mutable/sentryusb-ble-gate.txt";

fn write_gate_status_file(
    sentry: Option<&TimedReading<sample::SentryMode>>,
    charging: Option<&TimedReading<sample::ChargingState>>,
    shift: Option<sample::ShiftState>,
) {
    let sentry_s = sentry
        .map(|r| format!("{:?}", r.value))
        .unwrap_or_else(|| "unknown".into());
    let charging_s = charging
        .map(|r| format!("{:?}", r.value))
        .unwrap_or_else(|| "unknown".into());
    // `absent` = drive poll didn't report a shift_state (omitted from
    // the response, or no drive poll ran this tick — Quiet mode).
    let shift_s = shift
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "absent".into());
    // Reading age as extra keys (parsers strip known prefixes only, so
    // these are additive). `_stale=true` = past GATE_READING_MAX_AGE —
    // the gate is treating the value above as unread.
    let age_line = |name: &str, at: Option<Instant>| -> String {
        match at {
            Some(at) => {
                let age = at.elapsed();
                let stale = if age >= GATE_READING_MAX_AGE {
                    format!("{name}_stale=true\n")
                } else {
                    String::new()
                };
                format!("{name}_age_s={}\n{stale}", age.as_secs())
            }
            None => String::new(),
        }
    };
    let sentry_age = age_line("sentry_mode", sentry.map(|r| r.at));
    let charging_age = age_line("charging_state", charging.map(|r| r.at));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = format!(
        "sentry_mode={sentry_s}\n{sentry_age}charging_state={charging_s}\n{charging_age}shift_state={shift_s}\nupdated={now}\n"
    );
    let _ = std::fs::write(GATE_STATUS_PATH, body);
}

/// No-op, kept for call-site stability.
///
/// This used to `systemctl stop sentryusb-ble` before each Active poll
/// to claim exclusive hci0, cycling the iOS GATT server every 30-60s.
/// Unnecessary: BLE LE multi-role lets one controller act as central
/// (us → Tesla) and peripheral (sentryusb-ble → iOS app) at once, and
/// all shipped chips support it. The inter-process radio lock still
/// serializes our Rust processes; sentryusb-ble now runs continuously.
/// (The pair flow in api/system.rs still stops it briefly — tesla-control
/// wants exclusive bluez access for the add-key handshake.)
async fn stop_ios_gatt() {
    debug!("stop_ios_gatt: no-op (sentryusb-ble + telemetry coexist via BLE multi-role)");
}

/// Service one IPC action request from `sentryusb-ble-action`: acquire
/// the radio lock if needed, then dispatch through the PersistentSession.
/// Doesn't release the radio afterward — the next tick likely wants it,
/// and thrashing would defeat the point of routing actions through us.
async fn handle_action_request(
    req: action_socket::ActionRequest,
    held_radio: &mut bool,
    ble_session: &mut Option<sample_ble::SessionHandle>,
    c6_link: &mut C6Link,
) {
    let verb = req.verb.clone();
    info!("action_socket: IPC request received — verb={}", verb);

    // Same enable/VIN gate as the rest of the daemon — refuse the action
    // if BLE is off so ble-action can fall back.
    let mut cfg = match crate::config::BleConfig::load() {
        Ok(c) => c,
        Err(e) => {
            let _ = req.reply.send(Err(anyhow::anyhow!(
                "load BLE config: {e}"
            )));
            return;
        }
    };
    enforce_backfill_rule(&mut cfg);
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

    // C6 owns the car: route through it, never open our own session.
    if c6_link.c6_owns(&cfg) || c6_link.pending_close.is_some() {
        let result = c6_action(&cfg, &verb).await;
        match &result {
            Ok(_) => info!("action_socket: verb={} routed via C6", verb),
            Err(e) => warn!("action_socket: verb={} via C6 failed: {:#}", verb, e),
        }
        let _ = req.reply.send(result);
        return;
    }

    // Resolve the verb before any BLE work (saves the radio handoff on
    // a typo). State queries (session-info, drive-state, sentry-state) reuse the
    // held connection but return data rather than a fire-and-forget
    // action; every other verb must resolve to a typed ActionPayload.
    enum Dispatch {
        SessionInfo,
        DriveState,
        SentryState,
        Pair,
        Action(sentryusb_tesla_ble::actions::ActionPayload),
    }
    let dispatch = match verb.as_str() {
        "session-info" => Dispatch::SessionInfo,
        "drive-state" => Dispatch::DriveState,
        "sentry-state" => Dispatch::SentryState,
        "pair" => Dispatch::Pair,
        _ => match action_socket::parse_verb(&verb) {
            Ok(a) => Dispatch::Action(a),
            Err(e) => {
                let _ = req.reply.send(Err(e));
                return;
            }
        },
    };

    // Same lease claim as the tick (a flag flip since the last tick must not
    // let this path connect while the supervisor still holds a grant).
    if !c6_claim_for_sampler(&cfg, c6_link) {
        let _ = req.reply.send(Err(anyhow::anyhow!(
            "UNREACHABLE: could not claim the car link from the C6"
        )));
        return;
    }

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

    // Acquire the radio if not already held (same as the Active tick;
    // not shared because the early-return handling differs).
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
    let result: Result<String> = match dispatch {
        // Pairing probe: reuse this session's held connection. Map the
        // tri-state onto the line protocol's OK/ERR contract —
        // Paired => "OK", NotPaired => "ERR NOT_PAIRED", Unreachable =>
        // "ERR UNREACHABLE: …". sentryusb-ble-action parses these tokens
        // and the API clears the paired marker only on NOT_PAIRED.
        Dispatch::SessionInfo => {
            use sentryusb_tesla_ble::manager::PairingStatus;
            let status = session.check_pairing().await;
            info!(
                "action_socket: verb=session-info -> {:?} ({}ms)",
                status,
                started.elapsed().as_millis()
            );
            match status {
                PairingStatus::Paired => Ok(String::new()),
                PairingStatus::NotPaired => Err(anyhow::anyhow!("NOT_PAIRED")),
                PairingStatus::Unreachable(reason) => {
                    Err(anyhow::anyhow!("UNREACHABLE: {reason}"))
                }
            }
        }
        // Gear probe: read DriveState over the held connection and reply
        // with the single-letter token (`OK P`/`OK R`/…). A reachable
        // car that reports no concrete gear (Invalid/SNA — typically a
        // parked-and-dozing car) maps to UNREACHABLE so the caller
        // retries rather than treating it as a definite gear.
        Dispatch::DriveState => {
            let drive = session.get_drive().await;
            let elapsed_ms = started.elapsed().as_millis();
            match drive {
                Ok(drive) => {
                    match sentryusb_tesla_ble::responses::shift_state_token(&drive) {
                        Some(tok) => {
                            info!("action_socket: verb=drive-state -> {} ({}ms)", tok, elapsed_ms);
                            Ok(tok.to_string())
                        }
                        None => {
                            info!(
                                "action_socket: verb=drive-state -> no gear reported ({}ms)",
                                elapsed_ms
                            );
                            Err(anyhow::anyhow!(
                                "UNREACHABLE: car reported no gear (may be asleep)"
                            ))
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "action_socket: verb=drive-state failed after {}ms: {:#}",
                        elapsed_ms, e
                    );
                    Err(anyhow::anyhow!("UNREACHABLE: {e:#}"))
                }
            }
        }
        Dispatch::SentryState => {
            match session.get_closures().await {
                Ok(closures) => sentryusb_tesla_ble::responses::sentry_mode_token(&closures)
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("UNREACHABLE: car reported no Sentry state")),
                Err(e) => Err(anyhow::anyhow!("UNREACHABLE: {e:#}")),
            }
        }
        // Add-key-to-whitelist (pairing) request over the held
        // connection. Fire-and-forget: "OK" means the car received the
        // request and is prompting for the NFC-card tap. Routing it
        // through this warm session reuses the BLE slot the daemon
        // already holds — sidestepping the car's "maximum number of BLE
        // devices" limit that the old stop-daemon-then-tesla-control flow
        // tripped. Enrolment is confirmed afterwards via session-info.
        Dispatch::Pair => {
            let result = session.add_key_request().await;
            let elapsed_ms = started.elapsed().as_millis();
            match &result {
                Ok(()) => info!(
                    "action_socket: verb=pair add-key delivered ({}ms) — awaiting NFC tap",
                    elapsed_ms
                ),
                Err(e) => warn!(
                    "action_socket: verb=pair failed after {}ms: {:#}",
                    elapsed_ms, e
                ),
            }
            result.map(|()| String::new())
        }
        Dispatch::Action(action) => {
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
            result.map(|_| String::new())
        }
    };
    let _ = req.reply.send(result);
}

/// Release our radio-lock entry. Called on radio-release transitions
/// and SIGTERM.
async fn release_radio() {
    // Just release the lock — the sync semantic between our Rust
    // processes (telemetry, ble-action, pair). sentryusb-ble doesn't
    // check it, and there's nothing to restart now that stop_ios_gatt
    // is a no-op and sentryusb-ble runs continuously.
    if let Err(e) = lock::release(OWNER) {
        warn!("failed to release radio lock: {e}");
    }
}

/// Send a short keep-awake alert with the full BLE reason retained in history.
/// The explicit category supports mobile routing. Any failure to spawn is
/// swallowed (the nudge cycle has already done its retries; a missing
/// FCM relay can't recover the underlying BLE failure).
fn emit_keep_awake_failure_notification(reason: &str) {
    let title = std::env::var("NOTIFICATION_TITLE").unwrap_or_else(|_| "SentryUSB".into());
    let body = format!(
        "Tesla BLE: Keep awake failed (attempt 3/3). BLE command failed. Response: {reason}"
    );
    let _ = std::process::Command::new("/root/bin/send-push-message")
        .args([&format!("{title}:"), &body, "", "keep_awake_failure",
            "Tesla BLE: Could not keep the car awake. Archiving may be interrupted."])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    // `should_enter_quiet` is the gate's sleep decision. Quiet = "let the
    // car sleep"; permitted only when the car is parked or asleep AND
    // nothing has a reason to hold it awake. Args, in order:
    // (car_truly_asleep, parked_confirmed, actively_charging, sentry_on,
    //  keep_awake_active).

    #[test]
    fn parked_idle_car_is_allowed_to_sleep() {
        // Parked, sentry off, not charging, nothing running → may sleep.
        // The behavior we preserve for a genuinely idle parked car.
        assert!(should_enter_quiet(false, true, false, false, false));
    }

    #[test]
    fn keep_awake_pins_active_while_parked() {
        // The regression (commit fba51ce): parked + sentry off + not
        // charging would quiet, but an archive / keep-awake nudge is
        // running, so the car must NOT be allowed to sleep mid-archive.
        assert!(!should_enter_quiet(false, true, false, false, true));
    }

    #[test]
    fn keep_awake_pins_active_even_when_car_looks_asleep() {
        // A long archive disconnects the USB gadget, so cam_disk.bin stops
        // updating and the car trips "truly asleep" after 5 min even though
        // it's awake. The override must cover this second path too — hence
        // it sits on the whole decision, not just the parked-polls counter.
        assert!(should_enter_quiet(true, false, false, false, false));
        assert!(!should_enter_quiet(true, false, false, false, true));
    }

    #[test]
    fn charging_or_sentry_still_pins_active() {
        assert!(!should_enter_quiet(false, true, true, false, false)); // charging
        assert!(!should_enter_quiet(false, true, false, true, false)); // sentry on
    }

    #[test]
    fn unread_charge_and_sentry_do_not_pin_parked_car_when_keep_awake_is_off() {
        let (actively_charging, sentry_on) = resolve_gate_inputs(
            false, // car_truly_asleep
            true,  // parked_confirmed
            None,  // charging_state unread
            None,  // sentry_mode unread
            false, // keep_awake_active
        );

        assert!(!actively_charging);
        assert!(!sentry_on);
        assert!(should_enter_quiet(false, true, actively_charging, sentry_on, false));
    }

    #[test]
    fn unread_charge_and_sentry_still_pin_active_during_keep_awake() {
        let (actively_charging, sentry_on) = resolve_gate_inputs(
            false, // car_truly_asleep
            true,  // parked_confirmed
            None,  // charging_state unread
            None,  // sentry_mode unread
            true,  // keep_awake_active
        );

        assert!(actively_charging);
        assert!(sentry_on);
        assert!(!should_enter_quiet(false, true, actively_charging, sentry_on, true));
    }

    #[test]
    fn moving_car_never_quiets() {
        // Not parked-confirmed and not asleep (e.g. driving).
        assert!(!should_enter_quiet(false, false, false, false, false));
    }

    /// A TimedReading backdated by `age` (for expiry tests).
    fn reading_aged<T: Copy>(value: T, age: Duration) -> TimedReading<T> {
        TimedReading {
            value,
            at: Instant::now().checked_sub(age).expect("test age fits in Instant"),
        }
    }

    #[test]
    fn fresh_reading_within_max_age_returns_value() {
        let r = reading_aged(sample::ChargingState::Charging, Duration::from_secs(30));
        assert_eq!(
            r.fresh(GATE_READING_MAX_AGE),
            Some(sample::ChargingState::Charging)
        );
    }

    #[test]
    fn expired_reading_returns_none() {
        let r = reading_aged(
            sample::ChargingState::Charging,
            GATE_READING_MAX_AGE + Duration::from_secs(1),
        );
        assert_eq!(r.fresh(GATE_READING_MAX_AGE), None);
    }

    #[test]
    fn expired_charging_reading_no_longer_pins_parked_car() {
        // A pre-outage Some(Charging) must expire into the unread path:
        // parked-confirmed + keep-awake off → gate treats it sleep-safe
        // instead of pinning Active forever on a stale value.
        let stale = reading_aged(
            sample::ChargingState::Charging,
            GATE_READING_MAX_AGE + Duration::from_secs(1),
        );
        let charging_fresh = stale.fresh(GATE_READING_MAX_AGE);
        assert_eq!(charging_fresh, None);

        let (actively_charging, sentry_on) =
            resolve_gate_inputs(false, true, charging_fresh, None, false);
        assert!(!actively_charging);
        assert!(!sentry_on);
        assert!(should_enter_quiet(false, true, actively_charging, sentry_on, false));
    }

    #[test]
    fn expired_readings_still_pin_active_during_keep_awake() {
        let stale = reading_aged(
            sample::ChargingState::Charging,
            GATE_READING_MAX_AGE + Duration::from_secs(1),
        );
        let (actively_charging, sentry_on) =
            resolve_gate_inputs(false, true, stale.fresh(GATE_READING_MAX_AGE), None, true);
        assert!(actively_charging);
        assert!(sentry_on);
        assert!(!should_enter_quiet(false, true, actively_charging, sentry_on, true));
    }

    #[test]
    fn bc_awake_enables_refresh_when_cam_mtime_is_stale() {
        // Charging with Sentry off: no clip writes, so usb_watch reads
        // Asleep — but the body controller knows the car is awake.
        assert!(should_refresh_parked_awake(CarState::Asleep, Some(true)));
        // Same for the Idle band (writes within 5 min but not 90 s),
        // which previously got neither refresh path.
        assert!(should_refresh_parked_awake(CarState::Idle, Some(true)));
    }

    #[test]
    fn refresh_skipped_unless_provably_awake() {
        // BC says asleep / unknown and no clip writes → let it sleep.
        assert!(!should_refresh_parked_awake(CarState::Asleep, Some(false)));
        assert!(!should_refresh_parked_awake(CarState::Asleep, None));
        assert!(!should_refresh_parked_awake(CarState::Idle, None));
        // Clip writes alone are still sufficient (BC unread).
        assert!(should_refresh_parked_awake(CarState::Awake, None));
    }

    #[test]
    fn expired_reading_raw_value_remains_for_keep_accessory() {
        // Keep-accessory intentionally consumes the raw value (see its
        // call site): expiry must hide the reading from the gate without
        // destroying it for the charge-hold policy.
        let stale = reading_aged(
            sample::ChargingState::Charging,
            GATE_READING_MAX_AGE + Duration::from_secs(1),
        );
        assert_eq!(stale.fresh(GATE_READING_MAX_AGE), None);
        assert_eq!(stale.value, sample::ChargingState::Charging);
    }
}

// ── C6-primary domain helpers ────────────────────────────────────────────
// Each tries to satisfy a domain from the C6 snapshot instead of an in-process
// BLE poll. Returns true if the C6 gave a fresh, trustworthy value (sample +
// side-effects populated); false → the caller falls back to BLE. A false return
// records the fallback (the replacement-viability metric) with its reason.
// Only wired when TELEMETRY_SOURCE=c6_primary; dead on a normal box.

/// Log a fallback with its reason and the running count. Centralizes the metric
/// so every "did not use C6" path (incl. NoSnapshot and empty payloads) counts.
fn c6_fallback(domain: &str, reason: &str) -> bool {
    let n = c6_source::record_fallback();
    debug!(domain, reason, fallbacks = n, "C6 not used -> in-process BLE");
    false
}

fn c6_took_drive(
    snap: &Option<c6_source::C6Snapshot>,
    now_ms: u64,
    sample: &mut Sample,
    shift_state_observed: &mut Option<sample::ShiftState>,
) -> bool {
    match c6_source::decide_domain(snap.as_ref(), "drive", now_ms) {
        c6_source::Source::C6 => {
            let fields = &snap.as_ref().unwrap().domains.get("drive").unwrap().fields;
            let shift = fields
                .get("shift_state")
                .and_then(|v| v.as_str())
                .map(sample::ShiftState::from_c6_str)
                .unwrap_or(sample::ShiftState::Unknown);
            // Only satisfy drive from C6 when the car is genuinely PARKED. An
            // active drive needs the BLE drive poll for the reverse-geocoded
            // address (the C6 carries none) and fresh GPS. Tesla emits an
            // Invalid/SNA shift (-> Unknown) transiently WHILE DRIVING too, so
            // Unknown alone isn't proof of parked — require ~zero drive power
            // before trusting it, else fall back to BLE. A parked car (incl.
            // parked-and-charging) shows ~0 drive power; motion shows non-zero.
            let drive_power = fields.get("power").and_then(|v| v.as_i64()).unwrap_or(0);
            let parked = matches!(shift, sample::ShiftState::Park)
                || (matches!(shift, sample::ShiftState::Unknown) && drive_power.abs() < 1);
            if !parked {
                return c6_fallback("drive", "moving_needs_ble_address");
            }
            if !c6_source::apply_drive(fields, sample) {
                return c6_fallback("drive", "empty_payload");
            }
            *shift_state_observed = Some(shift); // mode drive-detection input
            true
        }
        c6_source::Source::FallbackBle(r) => c6_fallback("drive", r.as_str()),
    }
}

fn c6_took_climate(snap: &Option<c6_source::C6Snapshot>, now_ms: u64, sample: &mut Sample) -> bool {
    match c6_source::decide_domain(snap.as_ref(), "climate", now_ms) {
        c6_source::Source::C6 => {
            let fields = &snap.as_ref().unwrap().domains.get("climate").unwrap().fields;
            if c6_source::apply_climate(fields, sample) {
                true
            } else {
                c6_fallback("climate", "empty_payload")
            }
        }
        c6_source::Source::FallbackBle(r) => c6_fallback("climate", r.as_str()),
    }
}

fn c6_took_charge(
    snap: &Option<c6_source::C6Snapshot>,
    now_ms: u64,
    sample: &mut Sample,
    last_charging_state: &mut Option<TimedReading<sample::ChargingState>>,
    fast_charging: &mut bool,
) -> bool {
    match c6_source::decide_domain(snap.as_ref(), "charge", now_ms) {
        c6_source::Source::C6 => {
            let snap = snap.as_ref().unwrap();
            let fields = &snap.domains.get("charge").unwrap().fields;
            // charging_state gates keep-awake — if the C6 payload lacks it, fall
            // back to BLE so the gate always gets a fresh authoritative value.
            let cs = match fields.get("charging_state").and_then(|v| v.as_str()) {
                Some(cs) => cs,
                None => return c6_fallback("charge", "no_charging_state"),
            };
            if !c6_source::apply_charge(fields, sample) {
                return c6_fallback("charge", "empty_payload");
            }
            let parsed = sample::ChargingState::from_c6_str(cs);
            // Stamp the keep-awake gate input with the snapshot's REAL age, not
            // now — a C6 value delivered N ms ago must age from then, so a stale
            // "Stopped" can't hold the gate open (or let the car sleep) falsely.
            let age = std::time::Duration::from_millis(
                snap.domain_age_ms("charge", now_ms).unwrap_or(u64::MAX / 2),
            );
            if let Some(tr) = TimedReading::at_age(parsed, age) {
                *last_charging_state = Some(tr);
            }
            let power = fields.get("charger_power").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            *fast_charging = parsed.is_active_charging() && power > FAST_CHARGE_THRESHOLD_KW;
            true
        }
        c6_source::Source::FallbackBle(r) => c6_fallback("charge", r.as_str()),
    }
}

fn c6_took_tires(snap: &Option<c6_source::C6Snapshot>, now_ms: u64, sample: &mut Sample) -> bool {
    match c6_source::decide_domain(snap.as_ref(), "tires", now_ms) {
        c6_source::Source::C6 => {
            let fields = &snap.as_ref().unwrap().domains.get("tires").unwrap().fields;
            if c6_source::apply_tires(fields, sample) {
                true
            } else {
                c6_fallback("tires", "empty_payload")
            }
        }
        c6_source::Source::FallbackBle(r) => c6_fallback("tires", r.as_str()),
    }
}

#[cfg(test)]
mod timed_reading_tests {
    use super::*;

    #[test]
    fn at_age_ages_from_delivery_not_now() {
        // A value delivered 5s ago, checked against a 3s window, must read stale
        // (the C6-stale-charge keep-awake bug: now() would wrongly read fresh).
        let tr = TimedReading::at_age(42u8, Duration::from_secs(5)).unwrap();
        assert_eq!(tr.fresh(Duration::from_secs(3)), None, "5s-old value must be stale at 3s window");
        assert_eq!(tr.fresh(Duration::from_secs(10)), Some(42), "fresh within a 10s window");
        // now() by contrast is fresh at 3s.
        assert_eq!(TimedReading::now(42u8).fresh(Duration::from_secs(3)), Some(42));
        // A very old age must never read fresh (None or a stale stamp).
        let old = TimedReading::at_age(1u8, Duration::from_secs(10_000_000));
        assert!(old.map_or(true, |t| t.fresh(Duration::from_secs(600)).is_none()));
    }
}

// ── Shared-key C6 ownership (TELEMETRY_SOURCE=c6_primary) ─────────────────

/// Handoff state plus which C6 values were already written as rows.
#[derive(Default)]
struct C6Link {
    coord: c6_coord::Coordinator,
    /// domain -> updated_at_ms of the value last persisted (dedupe key only).
    written: std::collections::HashMap<&'static str, u64>,
    /// Our session after shutdown, until its task (and link) has really ended.
    /// The C6 gets no grant while this is still open.
    pending_close: Option<sentryusb_tesla_ble::manager::PersistentSession>,
    /// A C6 grant is (or may still be) live on disk.
    grant_live: bool,
    /// When we last withdrew a C6 grant (boottime ms): no signing until settled.
    grant_revoked_at: Option<u64>,
    /// The one-shot TELEMETRY_SOURCE onboarding write was attempted.
    onboarding_tried: bool,
    /// First c6_release of this run checked for a leftover sampler lease.
    startup_checked: bool,
    /// Don't grant the C6 before this (boottime ms): leftover lease settle.
    first_grant_after: Option<u64>,
    /// Consecutive fresh C6 drive readings showing parked (keep-accessory
    /// input while the C6 owns the car; the sampler's own counter is idle).
    parked_obs: u32,
}

/// C6_BACKFILL (bench side-by-side) only on a proven separate key; on a shared
/// or unknown key it's refused loudly and the box runs normal one-at-a-time
/// coordination. Never user-facing: nothing sets it automatically.
fn enforce_backfill_rule(cfg: &mut BleConfig) {
    if cfg.c6_backfill && !sentryusb_tesla_ble::c6_backfill::backfill_allowed() {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            error!(
                "C6_BACKFILL=1 REFUSED: the C6 is not proven to hold a different key from this Pi \
                 (shared or unknown). Two signers on one key desync the car. Running normal \
                 one-at-a-time C6 coordination instead."
            );
        }
        cfg.c6_backfill = false;
    }
}

/// Keep-accessory sender for the current owner. None = nobody can send now
/// (Pi owns but has no session yet); evaluate() then retries next tick.
fn keep_accessory_route<'a>(
    c6_owns: bool,
    c6_granted: bool,
    session: Option<&'a sentryusb_tesla_ble::manager::PersistentSession>,
    c6_api: &'a str,
) -> Option<keep_accessory::Route<'a>> {
    match keep_accessory_via(c6_owns, c6_granted, session.is_some())? {
        KaVia::C6 => Some(keep_accessory::Route::C6 { api: c6_api }),
        KaVia::Ble => session.map(keep_accessory::Route::Ble),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum KaVia {
    C6,
    Ble,
}

/// Pure routing decision: the owner of the car sends; never both. C6-owned but
/// not yet granted on disk = nobody sends (evaluate() retries next tick).
fn keep_accessory_via(c6_owns: bool, c6_granted: bool, have_session: bool) -> Option<KaVia> {
    if c6_owns {
        c6_granted.then_some(KaVia::C6)
    } else if have_session {
        Some(KaVia::Ble)
    } else {
        None
    }
}

/// Stand the sampler down and, once our car link is really closed, grant the
/// car to the C6. Non-blocking: an unfinished close just defers the grant to
/// a later tick (the old sampler lease keeps the supervisor holding).
async fn c6_release(
    ble_session: &mut Option<sample_ble::SessionHandle>,
    held_radio: &mut bool,
    link: &mut C6Link,
) {
    if let Some(h) = ble_session.take() {
        info!("C6 owns the car link: closing the sampler's Tesla BLE session");
        h.session.shutdown().await;
        link.pending_close = Some(h.session);
    }
    if *held_radio {
        release_radio().await;
        *held_radio = false;
    }
    if link.pending_close.as_ref().is_some_and(|s| !s.is_closed()) {
        info!("C6 coordination: waiting for the sampler session to close before granting");
        return;
    }
    link.pending_close = None;
    // Check-then-grant under the lease lock, so a ble-action claim can't land
    // between the check and our grant (and then be overwritten by it).
    let Some(_lock) = c6_coord::lease_lock() else {
        link.grant_live = false;
        return;
    };
    if c6_coord::ble_action_holds_car() {
        info!("C6 coordination: sentryusb-ble-action holds the car; granting once it lapses");
        link.grant_live = false;
        return;
    }
    // A sampler lease we didn't write this run (daemon restart) may cover a
    // Pi command still in flight: wait CLAIM_SETTLE_MS before the first grant.
    let now = c6_coord::boottime_ms().unwrap_or(0);
    if !link.grant_live && !link.startup_checked {
        link.startup_checked = true;
        if c6_coord::disk_sampler_lease_live() {
            link.first_grant_after = Some(now.saturating_add(c6_coord::CLAIM_SETTLE_MS));
        }
    }
    if link.first_grant_after.is_some_and(|t| now < t) {
        link.grant_live = false;
        return;
    }
    link.first_grant_after = None;
    link.grant_revoked_at = None;
    link.grant_live = c6_coord::write_lease(c6_coord::Owner::C6);
}

/// Publish the sampler lease before any Pi session touches the car, whenever
/// a C6 could be involved (flag on, or a supervisor snapshot exists). False =
/// not yet safe to connect: the lease write failed, or a C6 grant was only
/// just withdrawn and a C6 command may still be in flight.
fn c6_claim_for_sampler(cfg: &BleConfig, link: &mut C6Link) -> bool {
    if cfg.c6_primary && cfg.c6_backfill {
        // Bench backfill: both radios by design, so the C6 is granted too.
        link.grant_live = c6_coord::write_lease(c6_coord::Owner::C6);
        return true;
    }
    let c6_possible = cfg.c6_primary || std::path::Path::new(c6_source::SNAPSHOT_PATH).exists();
    if !c6_possible {
        return true; // stock box
    }
    let Some(_lock) = c6_coord::lease_lock() else { return false };
    // A grant still live on disk (e.g. from before a daemon restart) counts as
    // just-withdrawn too: the C6 may be mid-command.
    if !link.grant_live && c6_coord::disk_grant_live() {
        link.grant_live = true;
    }
    // C6 unplugged: nothing left on the other side to finish a command.
    if !c6_coord::c6_present() {
        link.grant_live = false;
        link.grant_revoked_at = None;
    }
    if !c6_coord::write_lease(c6_coord::Owner::Sampler) {
        return false;
    }
    let now = c6_coord::boottime_ms().unwrap_or(0);
    if link.grant_live {
        link.grant_live = false;
        link.grant_revoked_at = Some(now);
    }
    match link.grant_revoked_at {
        Some(t) if now.saturating_sub(t) < c6_coord::CLAIM_SETTLE_MS => false,
        _ => {
            link.grant_revoked_at = None;
            true
        }
    }
}

/// C6 onboarding: once a provisioned C6 is live and TELEMETRY_SOURCE is unset,
/// write `TELEMETRY_SOURCE=c6_primary` so the sampler stands down. Never over an
/// explicit choice, never with a C6_BACKFILL bench config; tried once per run.
fn c6_onboarding(cfg: &BleConfig, link: &mut C6Link) {
    if cfg.telemetry_source_set || cfg.c6_backfill || link.onboarding_tried {
        return;
    }
    let snap = c6_source::read_snapshot();
    if !(c6_coord::c6_alive(snap.as_ref()) && snap.as_ref().and_then(|s| s.provisioned) == Some(true)) {
        return;
    }
    link.onboarding_tried = true;
    let path = sentryusb_config::find_config_path();
    let result = (|| -> anyhow::Result<()> {
        let (mut active, commented) = sentryusb_config::parse_file(path)?;
        if sentryusb_config::get_config_value(&active, &commented, "TELEMETRY_SOURCE").is_some() {
            return Ok(()); // raced with a user edit
        }
        active.insert("TELEMETRY_SOURCE".into(), "c6_primary".into());
        let _ = std::process::Command::new("bash").args(["-c", "/root/bin/remountfs_rw"]).status();
        sentryusb_config::write_file(path, &active)
    })();
    match result {
        Ok(()) => info!("C6 onboarding: provisioned C6 found, set TELEMETRY_SOURCE=c6_primary"),
        Err(e) => warn!("C6 onboarding: could not set TELEMETRY_SOURCE: {e:#}"),
    }
}

impl C6Link {
    /// True when this process must not talk to the car itself.
    fn c6_owns(&self, cfg: &BleConfig) -> bool {
        self.c6_owns_with(cfg, c6_coord::c6_present())
    }

    fn c6_owns_with(&self, cfg: &BleConfig, c6_present: bool) -> bool {
        cfg.c6_primary && !cfg.c6_backfill && c6_present && self.coord.owner() == c6_coord::Owner::C6
    }
}

/// Service an action-socket verb while the C6 owns the car. Queries answer
/// from the snapshot or report UNREACHABLE (never NOT_PAIRED, which would
/// clear the paired marker); commands go to the supervisor.
async fn c6_action(cfg: &BleConfig, verb: &str) -> Result<String> {
    match verb {
        "session-info" => Err(anyhow::anyhow!("UNREACHABLE: C6 owns the car link")),
        "drive-state" => {
            let snap = c6_source::read_snapshot();
            let now = sample::now_secs().max(0) as u64 * 1000;
            let fresh = matches!(
                c6_source::decide_domain(snap.as_ref(), "drive", now),
                c6_source::Source::C6
            );
            let tok = snap
                .as_ref()
                .filter(|_| fresh)
                .and_then(|s| s.domains.get("drive"))
                .and_then(|d| d.fields.get("shift_state"))
                .and_then(|v| v.as_str())
                .map(sample::ShiftState::from_c6_str)
                .and_then(|s| match s {
                    sample::ShiftState::Park => Some("P"),
                    sample::ShiftState::Reverse => Some("R"),
                    sample::ShiftState::Neutral => Some("N"),
                    sample::ShiftState::Drive => Some("D"),
                    sample::ShiftState::Unknown => None,
                });
            tok.map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("UNREACHABLE: no fresh gear from C6"))
        }
        "pair" => Err(anyhow::anyhow!("C6 owns the car link; pair the C6 instead")),
        _ => {
            // Same validation (ranges, typos) as the BLE path, then map.
            action_socket::parse_verb(verb)?;
            let cmd = c6_coord::verb_to_c6_command(verb)
                .ok_or_else(|| anyhow::anyhow!("'{verb}' has no C6 equivalent"))?;
            c6_coord::supervisor_command(&cfg.c6_supervisor_api, &cmd).await?;
            Ok(String::new())
        }
    }
}

fn sentry_from_c6_str(s: &str) -> Option<sample::SentryMode> {
    Some(match s.trim() {
        "Off" => sample::SentryMode::Off,
        "Idle" => sample::SentryMode::Idle,
        "Armed" => sample::SentryMode::Armed,
        "Aware" => sample::SentryMode::Aware,
        "Panic" => sample::SentryMode::Panic,
        "Quiet" => sample::SentryMode::Quiet,
        _ => return None,
    })
}

/// Parked evidence from one C6 drive reading: P, or an explicit Unknown
/// (HW3 parked) WITH a reported ~zero drive power. A missing shift or missing
/// power is no evidence (keep-accessory OFF must not fire on a sparse frame).
fn c6_drive_parked(shift: Option<sample::ShiftState>, power: Option<f64>) -> bool {
    match shift {
        Some(sample::ShiftState::Park) => true,
        Some(sample::ShiftState::Unknown) => power.is_some_and(|p| p.abs() < 1.0),
        _ => false,
    }
}

/// Fresh C6 value for `domain` not yet written as a row, else None.
fn c6_new_value<'a>(
    snap: Option<&'a c6_source::C6Snapshot>,
    link: &C6Link,
    domain: &'static str,
    now_wall_ms: u64,
) -> Option<&'a c6_source::DomainEntry> {
    if c6_source::decide_domain(snap, domain, now_wall_ms) != c6_source::Source::C6 {
        return None;
    }
    let d = snap?.domains.get(domain)?;
    (link.written.get(domain) != Some(&d.updated_at_ms)).then_some(d)
}

/// One tick while the C6 owns the car: persist its fresh values as normal
/// `state` rows (downstream readers key on that source), refresh the gate
/// inputs, and route the keep-awake nudge through the C6. No BLE here.
#[allow(clippy::too_many_arguments)]
async fn c6_owned_tick(
    conn: &Connection,
    cfg: &BleConfig,
    snap: Option<&c6_source::C6Snapshot>,
    link: &mut C6Link,
    last_charging_state: &mut Option<TimedReading<sample::ChargingState>>,
    last_sentry_mode: &mut Option<TimedReading<sample::SentryMode>>,
    last_lat: &mut Option<f64>,
    last_lon: &mut Option<f64>,
    last_location_name: &mut Option<String>,
    next_nudge_due_at: &mut Option<Instant>,
    nudge_retry_count: &mut u32,
    last_nudge_notification_at: &mut Option<Instant>,
) -> Duration {
    let now_wall = sample::now_secs().max(0) as u64 * 1000;
    let mut row = Sample { ts: sample::now_secs(), source: "state".into(), ..Sample::default() };
    let mut any = false;
    let mut shift = None;
    let mut mark: Vec<(&'static str, u64)> = Vec::new();

    if let Some(d) = c6_new_value(snap, link, "drive", now_wall) {
        if c6_source::apply_drive(&d.fields, &mut row) {
            any = true;
        }
        shift = d.fields.get("shift_state").and_then(|v| v.as_str()).map(sample::ShiftState::from_c6_str);
        let parked = c6_drive_parked(shift, d.fields.get("power").and_then(|v| v.as_f64()));
        link.parked_obs = if parked { link.parked_obs.saturating_add(1) } else { 0 };
        mark.push(("drive", d.updated_at_ms));
    } else if c6_source::decide_domain(snap, "drive", now_wall) != c6_source::Source::C6 {
        link.parked_obs = 0; // no fresh drive data: no parked evidence
    }
    if let Some(d) = c6_new_value(snap, link, "location", now_wall) {
        let lat = d.fields.get("latitude").and_then(|v| v.as_f64());
        let lon = d.fields.get("longitude").and_then(|v| v.as_f64());
        if let (Some(la), Some(lo)) = (lat, lon) {
            *last_lat = Some(la);
            *last_lon = Some(lo);
            any = true;
        }
        if let Some(n) = d.fields.get("location_name").and_then(|v| v.as_str()) {
            *last_location_name = Some(n.to_string());
            row.location_name = Some(n.to_string());
        }
        mark.push(("location", d.updated_at_ms));
    }
    if let Some(d) = c6_new_value(snap, link, "climate", now_wall) {
        any |= c6_source::apply_climate(&d.fields, &mut row);
        mark.push(("climate", d.updated_at_ms));
    }
    if let Some(d) = c6_new_value(snap, link, "charge", now_wall) {
        any |= c6_source::apply_charge(&d.fields, &mut row);
        if let Some(cs) = d.fields.get("charging_state").and_then(|v| v.as_str()) {
            let age = snap.and_then(|s| s.domain_age_ms("charge", now_wall)).unwrap_or(u64::MAX / 2);
            if let Some(tr) = TimedReading::at_age(sample::ChargingState::from_c6_str(cs), Duration::from_millis(age)) {
                *last_charging_state = Some(tr);
            }
        }
        mark.push(("charge", d.updated_at_ms));
    }
    if let Some(d) = c6_new_value(snap, link, "tires", now_wall) {
        any |= c6_source::apply_tires(&d.fields, &mut row);
        mark.push(("tires", d.updated_at_ms));
    }
    if let Some(d) = c6_new_value(snap, link, "closures", now_wall) {
        if let Some(sm) = d.fields.get("sentry_mode_state").and_then(|v| v.as_str()).and_then(sentry_from_c6_str) {
            let age = snap.and_then(|s| s.domain_age_ms("closures", now_wall)).unwrap_or(u64::MAX / 2);
            if let Some(tr) = TimedReading::at_age(sm, Duration::from_millis(age)) {
                *last_sentry_mode = Some(tr);
            }
        }
        mark.push(("closures", d.updated_at_ms));
    }
    for (d, t) in mark {
        link.written.insert(d, t);
    }
    if any {
        row.latitude = *last_lat;
        row.longitude = *last_lon;
        if row.location_name.is_none() {
            row.location_name = last_location_name.clone();
        }
        persist(conn, row);
    }
    write_gate_status_file(last_sentry_mode.as_ref(), last_charging_state.as_ref(), shift);

    // Keep-awake: same cadence/retry/notify budget as the BLE path, sent by
    // the C6 (the only device allowed to sign on this key right now).
    // No grant on disk yet (our session still closing, or ble-action holds the
    // car): the supervisor would refuse, so don't burn the retry budget.
    if lock::keep_awake_requested() && link.grant_live {
        let now = Instant::now();
        if next_nudge_due_at.map(|t| now >= t).unwrap_or(true) {
            let interval = Duration::from_secs(cfg.keep_awake_interval_secs);
            let cmd = serde_json::json!({"cmd": "nudge_charge_port_close", "await_result": true});
            match c6_coord::supervisor_command(&cfg.c6_supervisor_api, &cmd).await {
                Ok(()) => {
                    info!("keep-awake: charge-port-close nudge sent via C6 (next in {}s)", interval.as_secs());
                    *next_nudge_due_at = Some(now + interval);
                    *nudge_retry_count = 0;
                }
                Err(e) => {
                    *nudge_retry_count += 1;
                    warn!("keep-awake: C6 nudge failed (attempt {}/3): {:#}", *nudge_retry_count, e);
                    if *nudge_retry_count >= 3 {
                        let notify_due = last_nudge_notification_at
                            .map(|t| now.duration_since(t) >= Duration::from_secs(600))
                            .unwrap_or(true);
                        if notify_due {
                            emit_keep_awake_failure_notification(&format!("{:#}", e));
                            *last_nudge_notification_at = Some(now);
                        }
                        *nudge_retry_count = 0;
                        *next_nudge_due_at = Some(now + interval);
                    } else {
                        *next_nudge_due_at = Some(now + Duration::from_secs(30));
                    }
                }
            }
        }
    } else if !lock::keep_awake_requested() {
        *next_nudge_due_at = None;
        *nudge_retry_count = 0;
    }

    // Snapshot refreshes every second; 15s matches the Active drive cadence.
    C6_OWNED_POLL
}

/// Tick cadence while the C6 owns the car.
const C6_OWNED_POLL: Duration = Duration::from_secs(15);

#[cfg(test)]
mod c6_link_tests {
    use super::*;

    #[test]
    fn flag_unset_never_hands_the_car_to_the_c6() {
        // Stock config: the sampler always owns, whatever the coordinator says.
        let link = C6Link::default();
        assert_eq!(link.coord.owner(), c6_coord::Owner::C6);
        assert!(!link.c6_owns_with(&BleConfig::default(), true));
    }

    #[test]
    fn c6_primary_owns_but_backfill_does_not() {
        let link = C6Link::default();
        let mut cfg = BleConfig { c6_primary: true, ..BleConfig::default() };
        assert!(link.c6_owns_with(&cfg, true));
        assert!(!link.c6_owns_with(&cfg, false), "C6 unplugged = stock");
        cfg.c6_backfill = true; // bench soak: both radios by design
        assert!(!link.c6_owns_with(&cfg, true));
    }

    #[test]
    fn failback_releases_ownership_to_the_sampler() {
        let mut link = C6Link::default();
        let cfg = BleConfig { c6_primary: true, ..BleConfig::default() };
        link.coord.step(false, 0);
        link.coord.step(false, c6_coord::FAILBACK_AFTER_MS);
        assert!(!link.c6_owns_with(&cfg, true));
    }
}

#[cfg(test)]
mod keep_accessory_route_tests {
    use super::*;

    #[test]
    fn c6_parked_evidence_needs_a_real_reading() {
        use sample::ShiftState::*;
        assert!(c6_drive_parked(Some(Park), None));
        assert!(c6_drive_parked(Some(Unknown), Some(0.0)));
        assert!(!c6_drive_parked(Some(Unknown), None), "no power field");
        assert!(!c6_drive_parked(Some(Unknown), Some(12.0)), "moving");
        assert!(!c6_drive_parked(None, Some(0.0)), "no shift field");
        assert!(!c6_drive_parked(Some(Drive), Some(0.0)));
    }

    #[test]
    fn owner_decides_who_sends_keep_accessory() {
        // C6 owns: the C6 sends, even if a stale session handle lingers.
        assert_eq!(keep_accessory_via(true, true, false), Some(KaVia::C6));
        assert_eq!(keep_accessory_via(true, true, true), Some(KaVia::C6));
        // C6 owns but the grant isn't on disk yet: nobody sends this tick.
        assert_eq!(keep_accessory_via(true, false, true), None);
        // Pi holds the grant: its own session sends; no session yet = retry later.
        assert_eq!(keep_accessory_via(false, false, true), Some(KaVia::Ble));
        assert_eq!(keep_accessory_via(false, false, false), None);
        assert!(matches!(
            keep_accessory_route(true, true, None, "127.0.0.1:1"),
            Some(keep_accessory::Route::C6 { .. })
        ));
    }

    #[tokio::test]
    async fn c6_route_sends_keep_accessory_and_waits_for_the_car() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for status in ["200 OK", "502 Bad Gateway"] {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap();
                bodies.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let body = r#"{"result":"x"}"#;
                let resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                s.write_all(resp.as_bytes()).await.unwrap();
            }
            bodies
        });
        let route = keep_accessory::Route::C6 { api: &addr };
        assert!(route.set_power(true).await.is_ok(), "car said ok");
        assert!(route.set_power(false).await.is_err(), "car rejected => failure, not success");
        let bodies = server.await.unwrap();
        assert!(bodies[0].contains(r#""cmd":"keep_accessory""#) && bodies[0].contains(r#""on":true"#));
        assert!(bodies[0].contains(r#""await_result":true"#));
        assert!(bodies[1].contains(r#""on":false"#));
    }
}
