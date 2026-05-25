// Push 6a cut the sampler over to `sample_ble.rs` (in-process Rust
// via PersistentSession). The shell-out functions in this module are
// no longer called — they stay only because Result types
// (DriveResult / ClimateResult / ChargeResult / TiresResult / Sample)
// are still re-used by the new path, and ripping the file out before
// Push 6c would force a bigger atomic change. `#[allow(dead_code)]`
// silences the "never used" warnings on the helpers that aren't
// re-exported.
#![allow(dead_code)]

//! Shells out to `tesla-control` and parses the JSON output into
//! the columns of `telemetry_samples`.
//!
//! Two sample modes:
//!   * [`sample_state`]: combines `state climate` + `state charge` for
//!     the full payload (battery %, battery temp, interior + exterior
//!     temp, HVAC). Used while the car is awake.
//!   * [`sample_body_controller`]: `body-controller-state` only —
//!     doesn't wake a sleeping car. Returns sparse data (no temps /
//!     HVAC) but confirms the car is still reachable.
//!
//! JSON field-name matching is intentionally permissive: tesla-control
//! emits protobuf-marshalled JSON whose exact casing has shifted
//! across SDK versions. We probe a handful of common names per field
//! and accept the first match. If a field is missing entirely, the
//! corresponding column lands as NULL — the schema allows it.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{info, warn};

const TESLA_CONTROL: &str = "/root/bin/tesla-control";
const KEY_FILE: &str = "/root/.ble/key_private.pem";
/// Persistent session cache. The vehicle-command library saves the
/// BLE handshake state here after a successful connect; subsequent
/// invocations skip the handshake round-trip and connect ~1-5s
/// faster. Per the upstream docs, a stale cache is auto-detected
/// and re-handshaken with zero penalty — pure upside.
///
/// Lives on `/backingfiles/` (the writable data partition) because
/// `/root` is normally mounted read-only on the Pi image. Same
/// partition as the SQLite DB, so writes are cheap.
const SESSION_CACHE: &str = "/backingfiles/tesla-session-cache.json";
/// Outer wall-clock budget for a single tesla-control invocation.
/// Sized to comfortably cover the inner `-connect-timeout 40s` +
/// `-command-timeout 10s` budget we pass to tesla-control plus a
/// small buffer for SDK retry rounds. Was 20s, which false-failed
/// slow-but-real responses (charge calls regularly take 14+s when
/// the BLE link is congested).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-call BLE connect budget passed to tesla-control. Default is
/// 20s, but the vehicle-command Go library has an internal retry
/// loop that needs room to re-handshake when the car's BLE stack
/// is busy. 40s lets it retry once or twice before giving up.
const CONNECT_TIMEOUT: &str = "40s";
/// Per-call BLE command budget once the connection is established.
/// Default is 5s, which is too tight for the bigger payloads
/// (climate, charge). 10s comfortably covers the longest payload
/// we observed during testing.
const CMD_TIMEOUT: &str = "10s";

/// Tesla shift state. Decoded from `state drive`'s `shiftState`
/// field which is either a string ("P"/"R"/"N"/"D") or a protobuf
/// int (P=1, R=2, N=3, D=4). The sampler's phase machine uses this
/// to decide whether the car is parked-and-recording (drop to
/// sleep-safe body-controller polling) vs actually being driven
/// (full state polls every 15s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftState {
    Park,
    Reverse,
    Neutral,
    Drive,
    /// Returned but value didn't match any known mapping. Treated
    /// as not-Park to avoid spuriously back-off-ing during real
    /// driving on a newer SDK.
    Unknown,
}

impl ShiftState {
    pub fn is_park(self) -> bool {
        matches!(self, ShiftState::Park)
    }
}

/// Tesla `ChargeState.charging_state` (`CarServer.ChargeState.ChargingState`
/// oneof). Mapped to a flat enum so the phase machine in `main.rs` can
/// pattern-match without dragging in proto types.
///
/// Used by the quiet-mode gate: `Starting` / `Charging` / `Calibrating`
/// mean the car is keeping itself awake for charging, so we stay in
/// Active polling regardless of shift state. Everything else (including
/// `Unknown` per design) is a candidate for the quiet path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingState {
    Unknown,
    Disconnected,
    NoPower,
    Starting,
    Charging,
    Complete,
    Stopped,
    Calibrating,
}

impl ChargingState {
    /// "Actively pulling power (or about to)" — the three states that
    /// keep the car awake on its own. Sampler stays in Active mode
    /// while this is true, even when shift_state has been Park for a
    /// while; quiet mode would just leave battery % stale during a
    /// charge session.
    pub fn is_active_charging(self) -> bool {
        matches!(
            self,
            ChargingState::Starting | ChargingState::Charging | ChargingState::Calibrating
        )
    }
}

/// Tesla `ClosuresState.SentryModeState` oneof, flattened. Mirrors the
/// proto's six states; `Off` is the only one where the car will sleep
/// on its own (the other five all hold the car awake monitoring).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentryMode {
    Off,
    Idle,
    Armed,
    Aware,
    Panic,
    /// "Quiet Sentry" — alarm is suppressed but the system is still
    /// armed and recording. Not to be confused with the sampler's
    /// own quiet-poll mode (unfortunate naming collision in the
    /// Tesla proto).
    Quiet,
}

impl SentryMode {
    /// Anything that isn't `Off` keeps the car awake. Used by the
    /// quiet-mode gate alongside `ChargingState::is_active_charging`.
    pub fn is_on(self) -> bool {
        !matches!(self, SentryMode::Off)
    }
}

/// Common fields every state response carries that downstream code
/// might want regardless of which specific sub-sampler it was. The
/// `vehicle_ts_secs` field is what the clock-sync feature uses to
/// keep the Pi's wall clock honest — Tesla derives this from GPS
/// time so it's always accurate.
#[derive(Default, Clone, Copy)]
pub struct ResponseMeta {
    /// Tesla's wall-clock timestamp from the response body, parsed
    /// from the RFC 3339 `timestamp` field to ms-since-epoch
    /// precision (Tesla includes fractional seconds like `.794Z` —
    /// we preserve them so clock-sync doesn't introduce ~500ms of
    /// rounding error). None when not present or unparseable.
    /// Used by `clock_sync::maybe_set_clock_from_vehicle`.
    pub vehicle_ts_ms: Option<i64>,
    /// Monotonic Instant from just before we sent the BLE request.
    /// Used for diagnostic RTT logging when clock-sync fires.
    pub request_started_at: Option<std::time::Instant>,
}

/// Result of a successful `sample_drive` call. Drive is the
/// highest-priority poll because it carries the three signals that
/// must stay fresh during a drive:
///   * `shift_state` — phase-machine input (drive detection)
///   * `location_name` — Tesla's reverse-geocoded address
///   * `odometer_mi` — mile counter, ticks continuously while driving
pub struct DriveResult {
    pub location_name: Option<String>,
    pub odometer_mi: Option<f64>,
    pub shift_state: Option<ShiftState>,
    pub meta: ResponseMeta,
}

/// Result of a successful `sample_climate` call. Slow-changing —
/// polled at a coarser cadence than `sample_drive`.
pub struct ClimateResult {
    pub interior_temp_c: Option<f64>,
    pub exterior_temp_c: Option<f64>,
    pub hvac_on: Option<bool>,
    pub meta: ResponseMeta,
}

/// Result of a successful `sample_charge` call. Slow-changing.
///
/// `charging_state` is in-memory only (not persisted to the DB) — the
/// phase machine in `main.rs` reads it on each tick to decide whether
/// it's safe to drop into quiet polling. `None` means we couldn't
/// extract it from the response; the gate treats unknown as
/// "stay Active" to avoid wrongly quieting down during a real charge
/// session.
pub struct ChargeResult {
    pub battery_pct: Option<f64>,
    pub charging_state: Option<ChargingState>,
    pub meta: ResponseMeta,
}

/// Result of a successful `sample_closures` call. In-memory only —
/// the only field we read from closures right now is sentry mode (for
/// the quiet-mode gate), no DB persistence. If we later want to
/// surface door / window / charge-port state in the UI, this is the
/// right place to add fields.
pub struct ClosuresResult {
    pub sentry_mode: Option<SentryMode>,
    pub meta: ResponseMeta,
}

/// Result of a successful `sample_tires` call. Very slow-changing —
/// polled at the coarsest cadence (every few minutes).
pub struct TiresResult {
    pub tire_fl_psi: Option<f64>,
    pub tire_fr_psi: Option<f64>,
    pub tire_rl_psi: Option<f64>,
    pub tire_rr_psi: Option<f64>,
    pub meta: ResponseMeta,
}

/// Result of a body-controller-state probe. Both fields are
/// in-memory signals for the phase machine — they ride the sample
/// row so it gets persisted with a body_controller source marker,
/// but the user_presence flag itself isn't stored.
pub struct BodyControllerSample {
    pub sample: Sample,
    /// Driver-seat occupancy. Used to detect "user got back in"
    /// while in body-controller-only mode so the sampler can
    /// promote to full state polling without waiting for the
    /// 15-min asleep cycle.
    pub user_presence: Option<bool>,
}

/// A single point-in-time observation, in the shape the DB writer
/// wants. All fields except `ts` and `source` are nullable because
/// different sample paths populate different subsets.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub ts: i64,
    pub battery_pct: Option<f64>,
    pub battery_temp_c: Option<f64>,
    pub interior_temp_c: Option<f64>,
    pub exterior_temp_c: Option<f64>,
    pub hvac_on: Option<bool>,
    // TPMS pressures in PSI. All four optional — cars without TPMS
    // (or runs where the `state tire-pressure` call fails / times
    // out) just leave these as None and the UI hides the row.
    pub tire_fl_psi: Option<f64>,
    pub tire_fr_psi: Option<f64>,
    pub tire_rl_psi: Option<f64>,
    pub tire_rr_psi: Option<f64>,
    /// Odometer in miles (Tesla native unit). Sampled every awake
    /// cycle — ticks continuously while driving.
    pub odometer_mi: Option<f64>,
    /// Tesla's reverse-geocoded address string for the car's
    /// current location. Pulled from `state drive`. Used as
    /// drive start/end labels in the UI.
    pub location_name: Option<String>,
    pub source: String,
}

/// Highest-priority sample: `state drive`. Cheap, fast, and carries
/// the three signals that matter most for drive tracking:
///   * `shift_state` — input to the phase machine (drive detection)
///   * `location_name` — Tesla's reverse-geocoded address (start/end)
///   * `odometer_mi` — mile counter, ticks continuously while driving
/// Polled every ~15s in Active mode because location and shift state
/// must stay fresh; the other three sub-samplers below run on slower
/// cadences (60s / 5 min).
///
/// Note: `state software-update` is intentionally not called. Its
/// response only contains the *pending* OTA version (often " "),
/// never the currently-installed `car_version`. That field lives in
/// `VehicleState` which tesla-control doesn't expose as a state
/// category, so there's no point burning BLE air time on it.
pub async fn sample_drive(vin: &str, adapter: &str) -> Result<DriveResult> {
    let started = std::time::Instant::now();
    let (result, outcome) = run_state_timed(vin, adapter, "drive").await;
    info!("state-poll: drive={}", outcome.fmt_short());
    if let Some(err) = &outcome.error {
        warn!(
            "state-poll subcommand failed: drive ({}ms): {}",
            outcome.elapsed_ms, err
        );
    }
    let drive = result?;
    let meta = build_meta(&drive, started);
    Ok(DriveResult {
        // Reverse-geocoded address. Tesla emits it inside the
        // `locationState` object that's also returned by `state drive`
        // (separate from `driveState`); fall back to checking the
        // top-level in case the shape varies.
        location_name: pick_string(&drive, &["locationName", "location_name"]),
        // Odometer — Tesla emits this as `odometerInHundredthsOfAMile`
        // inside the `driveState` object. Divide by 100 for miles.
        odometer_mi: pick_f64(
            &drive,
            &[
                "odometerInHundredthsOfAMile",
                "odometer_in_hundredths_of_a_mile",
            ],
        )
        .map(|hundredths| hundredths / 100.0)
        .or_else(|| {
            // Older / alternate shape — already in miles.
            pick_f64(
                &drive,
                &["odometer", "odometerMi", "odometer_mi", "odometerMiles"],
            )
        }),
        // Shift state for the phase machine. Not persisted — purely
        // a transient signal for "should the sampler back off?"
        shift_state: pick_shift_state(&drive),
        meta,
    })
}

/// Slow-cadence sample: `state climate`. Polled every ~60s in
/// Active mode. Returns the cabin/exterior temps + HVAC on/off.
pub async fn sample_climate(vin: &str, adapter: &str) -> Result<ClimateResult> {
    let started = std::time::Instant::now();
    let (result, outcome) = run_state_timed(vin, adapter, "climate").await;
    info!("state-poll: climate={}", outcome.fmt_short());
    if let Some(err) = &outcome.error {
        warn!(
            "state-poll subcommand failed: climate ({}ms): {}",
            outcome.elapsed_ms, err
        );
    }
    let climate = result?;
    let meta = build_meta(&climate, started);
    Ok(ClimateResult {
        interior_temp_c: pick_f64(
            &climate,
            &["insideTempCelsius", "insideTemp", "inside_temp", "insideTempC"],
        ),
        exterior_temp_c: pick_f64(
            &climate,
            &["outsideTempCelsius", "outsideTemp", "outside_temp", "outsideTempC"],
        ),
        hvac_on: pick_bool(
            &climate,
            &["isClimateOn", "is_climate_on", "hvacAuto", "climateKeeperMode"],
        ),
        meta,
    })
}

/// Slow-cadence sample: `state charge`. Polled every ~60s in
/// Active mode, offset 30s from climate so the two big-payload
/// calls don't stack in the same cycle. Returns battery percent.
///
/// NOTE on battery_temp_c: Tesla's BLE state API does NOT expose
/// battery cell temperature. Both charge_state and climate_state
/// only return `battery_heater_on` (boolean — is the heater
/// running, not how hot the pack is). We leave the column nullable
/// in the schema for forward compatibility but don't waste a probe.
pub async fn sample_charge(vin: &str, adapter: &str) -> Result<ChargeResult> {
    let started = std::time::Instant::now();
    let (result, outcome) = run_state_timed(vin, adapter, "charge").await;
    info!("state-poll: charge={}", outcome.fmt_short());
    if let Some(err) = &outcome.error {
        warn!(
            "state-poll subcommand failed: charge ({}ms): {}",
            outcome.elapsed_ms, err
        );
    }
    let charge = result?;
    let meta = build_meta(&charge, started);
    Ok(ChargeResult {
        battery_pct: pick_f64(&charge, &["batteryLevel", "battery_level", "batteryPct"]),
        // Legacy shell-out path no longer wired up (see header), so
        // we don't bother parsing charging_state out of the JSON.
        charging_state: None,
        meta,
    })
}

/// Very-slow-cadence sample: `state tire-pressure`. Polled every
/// ~5 min in Active mode — TPMS values almost never change during
/// a single drive, so the slow cadence saves BLE air time without
/// noticeable freshness cost. Values converted from Tesla's native
/// BAR to PSI to match what US vehicles display.
pub async fn sample_tires(vin: &str, adapter: &str) -> Result<TiresResult> {
    let started = std::time::Instant::now();
    let (result, outcome) = run_state_timed(vin, adapter, "tire-pressure").await;
    info!("state-poll: tires={}", outcome.fmt_short());
    if let Some(err) = &outcome.error {
        warn!(
            "state-poll subcommand failed: tire-pressure ({}ms): {}",
            outcome.elapsed_ms, err
        );
    }
    let tires = result?;
    let meta = build_meta(&tires, started);
    Ok(TiresResult {
        tire_fl_psi: pick_f64(&tires, &["tpmsPressureFl", "tpms_pressure_fl"])
            .map(bar_to_psi),
        tire_fr_psi: pick_f64(&tires, &["tpmsPressureFr", "tpms_pressure_fr"])
            .map(bar_to_psi),
        tire_rl_psi: pick_f64(&tires, &["tpmsPressureRl", "tpms_pressure_rl"])
            .map(bar_to_psi),
        tire_rr_psi: pick_f64(&tires, &["tpmsPressureRr", "tpms_pressure_rr"])
            .map(bar_to_psi),
        meta,
    })
}

/// Pull Tesla's wall-clock timestamp out of a state response (every
/// `state X` response has a `timestamp` field in the nested state
/// object, e.g. `driveState.timestamp`, `climateState.timestamp`).
/// Used by the clock-sync feature: a successful response gives us a
/// GPS-accurate reference for setting the Pi's clock without needing
/// NTP. Bundled with the request-started Instant so the caller can
/// compute round-trip-time for latency compensation.
fn build_meta(parsed: &Value, started: std::time::Instant) -> ResponseMeta {
    let ts = pick_string(parsed, &["timestamp"])
        .and_then(|s| crate::clock_sync::parse_rfc3339_ms(&s));
    ResponseMeta {
        vehicle_ts_ms: ts,
        request_started_at: Some(started),
    }
}

/// Cheap sample via `body-controller-state` — works on a sleeping
/// car, doesn't wake it. The Sample row is mostly NULL (the call
/// doesn't return battery/temp/HVAC). The `user_presence` flag is
/// the real reason to call this: it lets the sampler's phase
/// machine notice when the driver gets back in to a parked car so
/// it can resume state polling without waiting for the next slow
/// asleep-mode tick.
pub async fn sample_body_controller(vin: &str, adapter: &str) -> Result<BodyControllerSample> {
    let start = std::time::Instant::now();
    let result = run_tesla_control(vin, adapter, &["body-controller-state"]).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    match &result {
        Ok(_) => info!("body-controller poll: ok({}ms)", elapsed_ms),
        Err(e) => warn!("body-controller poll: err({}ms): {:#}", elapsed_ms, e),
    }
    let out = result?;
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    // user_presence enum names vary across SDK versions: try the
    // protobuf-mangled name first, then the snake-cased one. Values
    // we treat as "present": the literal protobuf enum
    // VEHICLE_USER_PRESENCE_PRESENT or a friendly "PRESENT" / "true".
    let user_presence = pick_string(
        &parsed,
        &["userPresence", "user_presence", "vehicleUserPresence"],
    )
    .map(|s| {
        let upper = s.to_ascii_uppercase();
        upper.contains("PRESENT") && !upper.contains("NOT_PRESENT")
    });
    Ok(BodyControllerSample {
        sample: Sample {
            ts: now_secs(),
            source: "body_controller".into(),
            ..Sample::default()
        },
        user_presence,
    })
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

async fn run_state(vin: &str, adapter: &str, category: &str) -> Result<Value> {
    let out = run_tesla_control(vin, adapter, &["state", category]).await?;
    serde_json::from_str::<Value>(&out)
        .with_context(|| format!("failed to parse tesla-control state {} output", category))
}

/// Outcome of a single `run_state` call. Captures success/fail,
/// wall-clock duration, and the raw error message so the
/// `sample_state` caller can emit a one-line summary + per-failure
/// detail line. Diagnostic-only — not persisted anywhere.
struct InvocationOutcome {
    elapsed_ms: u64,
    error: Option<String>,
}

impl InvocationOutcome {
    /// Short formatter used inside the summary line. Returns either
    /// `ok(420ms)` or `err(15000ms)` — keeps the per-poll summary
    /// readable when scanned in a journalctl pager.
    fn fmt_short(&self) -> String {
        if self.error.is_some() {
            format!("err({}ms)", self.elapsed_ms)
        } else {
            format!("ok({}ms)", self.elapsed_ms)
        }
    }
}

/// Wraps `run_state` to capture timing + error text for the
/// diagnostic summary. The Result is returned unchanged so existing
/// success/failure handling paths are unaffected.
async fn run_state_timed(
    vin: &str,
    adapter: &str,
    category: &str,
) -> (Result<Value>, InvocationOutcome) {
    let start = std::time::Instant::now();
    let result = run_state(vin, adapter, category).await;
    let outcome = InvocationOutcome {
        elapsed_ms: start.elapsed().as_millis() as u64,
        error: result.as_ref().err().map(|e| format!("{e:#}")),
    };
    (result, outcome)
}

async fn run_tesla_control(
    vin: &str,
    adapter: &str,
    subcommand: &[&str],
) -> Result<String> {
    // Pass tesla-control's own connect/command timeouts explicitly.
    // Defaults are 20s connect / 5s command — too tight for our use
    // case, which routinely sees 10-15s latencies during BLE
    // contention. The vehicle-command library has an internal retry
    // loop inside the connect window, so giving it more headroom
    // converts what would be a hard failure into a successful retry.
    //
    // `-bt-adapter` selects which HCI device to use. Defaults to
    // hci0 (onboard) but the user can switch to an external dongle
    // (hci1+) via settings for substantially better reliability.
    let mut args: Vec<&str> = vec![
        "-ble",
        "-key-file",
        KEY_FILE,
        "-vin",
        vin,
        "-bt-adapter",
        adapter,
        "-connect-timeout",
        CONNECT_TIMEOUT,
        "-command-timeout",
        CMD_TIMEOUT,
        // Skip the handshake round-trip on subsequent calls — pure
        // upside per upstream docs (stale cache → auto re-handshake
        // with no penalty).
        "-session-cache",
        SESSION_CACHE,
    ];
    args.extend_from_slice(subcommand);
    sentryusb_shell::run_with_timeout(COMMAND_TIMEOUT, TESLA_CONTROL, &args)
        .await
        .with_context(|| format!("tesla-control {} failed", subcommand.join(" ")))
}

/// Probe a list of candidate field names (top-level OR one level
/// nested under any object value) and return the first f64-coercible
/// match.
fn pick_f64(v: &Value, names: &[&str]) -> Option<f64> {
    for name in names {
        if let Some(n) = direct_f64(v, name) {
            return Some(n);
        }
    }
    if let Value::Object(map) = v {
        for child in map.values() {
            for name in names {
                if let Some(n) = direct_f64(child, name) {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn pick_bool(v: &Value, names: &[&str]) -> Option<bool> {
    for name in names {
        if let Some(b) = direct_bool(v, name) {
            return Some(b);
        }
    }
    if let Value::Object(map) = v {
        for child in map.values() {
            for name in names {
                if let Some(b) = direct_bool(child, name) {
                    return Some(b);
                }
            }
        }
    }
    None
}

/// String-flavored version of `pick_f64` — same top-level + one-level
/// nested probe pattern.
fn pick_string(v: &Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(s) = direct_string(v, name) {
            return Some(s);
        }
    }
    if let Value::Object(map) = v {
        for child in map.values() {
            for name in names {
                if let Some(s) = direct_string(child, name) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn direct_string(v: &Value, name: &str) -> Option<String> {
    match v.get(name)? {
        // Trim whitespace — Tesla's protojson output sometimes
        // includes leading/trailing newlines on string fields, and
        // an all-whitespace value should be treated as missing so
        // the UI doesn't render a labelled-but-empty row.
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

fn direct_f64(v: &Value, name: &str) -> Option<f64> {
    match v.get(name)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn direct_bool(v: &Value, name: &str) -> Option<bool> {
    match v.get(name)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Decode shift_state from `state drive` JSON. Tesla's protobuf
/// emits this as a **oneof** — the JSON form is
/// `"shiftState": {"P": {}}` (variant name as a key, empty object as
/// value). Older SDK builds may also have emitted it as a string or
/// protobuf int; we handle all three shapes for robustness.
fn pick_shift_state(v: &Value) -> Option<ShiftState> {
    // Locate the `shiftState` field — top level or one level
    // nested (under `driveState`).
    let shift = v
        .get("shiftState")
        .or_else(|| v.get("shift_state"))
        .or_else(|| {
            v.as_object()?
                .values()
                .find_map(|c| c.get("shiftState").or_else(|| c.get("shift_state")))
        })?;

    // Protobuf oneof shape: `{"P": {}}` — single key, empty value.
    if let Value::Object(map) = shift {
        if let Some(key) = map.keys().next() {
            return Some(decode_shift_token(key));
        }
        return None;
    }
    // String shape: `"P"`.
    if let Value::String(s) = shift {
        return Some(decode_shift_token(s));
    }
    // Int shape: Protobuf SHIFT_STATE_P=1, R=2, N=3, D=4.
    if let Some(n) = shift.as_i64() {
        return Some(match n {
            1 => ShiftState::Park,
            2 => ShiftState::Reverse,
            3 => ShiftState::Neutral,
            4 => ShiftState::Drive,
            _ => ShiftState::Unknown,
        });
    }
    None
}

fn decode_shift_token(s: &str) -> ShiftState {
    match s.to_ascii_uppercase().as_str() {
        "P" | "PARK" | "SHIFT_STATE_P" => ShiftState::Park,
        "R" | "REVERSE" | "SHIFT_STATE_R" => ShiftState::Reverse,
        "N" | "NEUTRAL" | "SHIFT_STATE_N" => ShiftState::Neutral,
        "D" | "DRIVE" | "SHIFT_STATE_D" => ShiftState::Drive,
        _ => ShiftState::Unknown,
    }
}

/// Bar → PSI. 1 bar = 14.5038 psi (NIST). Rounded to 1 decimal so
/// the DB doesn't carry FP noise we can't observe at display time.
fn bar_to_psi(bar: f64) -> f64 {
    ((bar * 14.5038) * 10.0).round() / 10.0
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_top_level_number() {
        let v = json!({"batteryLevel": 73.0});
        assert_eq!(pick_f64(&v, &["batteryLevel"]), Some(73.0));
    }

    #[test]
    fn picks_nested_number() {
        let v = json!({"chargeState": {"batteryLevel": 81.5}});
        assert_eq!(pick_f64(&v, &["batteryLevel"]), Some(81.5));
    }

    #[test]
    fn picks_first_matching_alias() {
        let v = json!({"climateState": {"inside_temp": 22.5}});
        assert_eq!(
            pick_f64(&v, &["insideTempCelsius", "inside_temp", "insideTemp"]),
            Some(22.5),
        );
    }

    #[test]
    fn returns_none_when_missing() {
        let v = json!({"unrelated": 1});
        assert_eq!(pick_f64(&v, &["batteryLevel"]), None);
    }

    #[test]
    fn parses_string_number() {
        let v = json!({"climateState": {"outsideTemp": "13.2"}});
        assert_eq!(pick_f64(&v, &["outsideTemp"]), Some(13.2));
    }

    #[test]
    fn shift_state_string_decodes_park_drive() {
        let v = json!({"driveState": {"shiftState": "P"}});
        assert_eq!(pick_shift_state(&v), Some(ShiftState::Park));
        assert!(pick_shift_state(&v).unwrap().is_park());

        let v = json!({"driveState": {"shiftState": "D"}});
        assert_eq!(pick_shift_state(&v), Some(ShiftState::Drive));
        assert!(!pick_shift_state(&v).unwrap().is_park());
    }

    #[test]
    fn shift_state_int_decodes_proto_form() {
        // Protobuf SHIFT_STATE_P = 1
        let v = json!({"shiftState": 1});
        assert_eq!(pick_shift_state(&v), Some(ShiftState::Park));
        // SHIFT_STATE_D = 4
        let v = json!({"shiftState": 4});
        assert_eq!(pick_shift_state(&v), Some(ShiftState::Drive));
    }

    #[test]
    fn shift_state_decodes_protobuf_oneof_shape() {
        // Real tesla-control output shape:
        // {"driveState": {"shiftState": {"P": {}}}}
        let v = json!({"driveState": {"shiftState": {"P": {}}}});
        assert_eq!(pick_shift_state(&v), Some(ShiftState::Park));
        let v = json!({"driveState": {"shiftState": {"D": {}}}});
        assert_eq!(pick_shift_state(&v), Some(ShiftState::Drive));
    }

    #[test]
    fn shift_state_unknown_for_garbage() {
        let v = json!({"shiftState": "what"});
        assert_eq!(pick_shift_state(&v), Some(ShiftState::Unknown));
    }

    #[test]
    fn shift_state_none_when_missing() {
        let v = json!({"unrelated": 1});
        assert_eq!(pick_shift_state(&v), None);
    }

    #[test]
    fn picks_bool_with_aliases() {
        assert_eq!(
            pick_bool(&json!({"climateState": {"isClimateOn": true}}), &["isClimateOn"]),
            Some(true),
        );
        assert_eq!(
            pick_bool(&json!({"climateState": {"is_climate_on": 0}}), &["is_climate_on"]),
            Some(false),
        );
    }
}
