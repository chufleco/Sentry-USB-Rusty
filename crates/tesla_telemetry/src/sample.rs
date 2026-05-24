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
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Result of a successful `sample_state` call. The Sample goes to
/// the DB; the shift_state is in-memory state for the phase machine
/// and intentionally NOT persisted (no DB column for it).
pub struct StateSample {
    pub sample: Sample,
    pub shift_state: Option<ShiftState>,
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

/// Full sample via `state climate` + `state charge` + `state drive`
/// + `state tire-pressure`. Wakes the car briefly if it's asleep —
/// only call when we already know the car is awake (recent clip
/// writes).
///
/// Note: `state software-update` is intentionally not called. Its
/// response only contains the *pending* OTA version (often " "
/// when no update is queued), never the currently-installed
/// `car_version`. That field lives in `VehicleState` which tesla-
/// control doesn't expose as a state category, so there's no point
/// burning BLE air time on it.
pub async fn sample_state(vin: &str) -> Result<StateSample> {
    // Time each subcommand so a single grep-able summary line tells
    // us at a glance which call is failing (and why). The two
    // best-effort calls (tire-pressure / drive) used to silently
    // .ok() their errors, which made "TPMS shows but location/
    // odometer are missing" symptoms invisible in logs. Each
    // failure is now warned explicitly.
    let (climate_res, climate_o) = run_state_timed(vin, "climate").await;
    let (charge_res, charge_o) = run_state_timed(vin, "charge").await;
    let (tires_res, tires_o) = run_state_timed(vin, "tire-pressure").await;
    let (drive_res, drive_o) = run_state_timed(vin, "drive").await;

    // One-line summary, grep `state-poll` in journalctl to see the
    // outcome pattern over time. INFO so it shows at default log
    // level (no RUST_LOG bump needed).
    let total_ms = climate_o.elapsed_ms
        + charge_o.elapsed_ms
        + tires_o.elapsed_ms
        + drive_o.elapsed_ms;
    info!(
        "state-poll: climate={} charge={} tires={} drive={} [total={}ms]",
        climate_o.fmt_short(),
        charge_o.fmt_short(),
        tires_o.fmt_short(),
        drive_o.fmt_short(),
        total_ms,
    );

    // Per-failure detail with the raw error string. Lets us tell
    // apart "context deadline exceeded" (BLE saturated — too many
    // keys connected) from other failure modes like JSON parse
    // errors or process-spawn failures.
    for (label, outcome) in [
        ("climate", &climate_o),
        ("charge", &charge_o),
        ("tire-pressure", &tires_o),
        ("drive", &drive_o),
    ] {
        if let Some(err) = &outcome.error {
            warn!(
                "state-poll subcommand failed: {} ({}ms): {}",
                label, outcome.elapsed_ms, err
            );
        }
    }

    // Required calls — climate + charge populate the bulk of the
    // sample (battery, temps, HVAC). If either fails, bail.
    let climate = climate_res?;
    let charge = charge_res?;
    // Best-effort: populate whatever subset succeeded. Errors were
    // already warned above.
    let tires = tires_res.ok();
    let drive = drive_res.ok();
    let now = now_secs();
    // NOTE on battery_temp_c: Tesla's BLE state API does NOT expose
    // battery cell temperature. Both charge_state and climate_state
    // only return `battery_heater_on` / `battery_heater_no_power`
    // (booleans — is the heater running, not how hot the pack is).
    // The BMS knows the temperature internally but it isn't part of
    // the public state query surface. We leave the column nullable
    // in the schema for forward compatibility (in case Tesla adds it
    // later) but we don't waste a probe trying to find it.
    let sample = Sample {
        ts: now,
        battery_pct: pick_f64(&charge, &["batteryLevel", "battery_level", "batteryPct"]),
        battery_temp_c: None,
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
        // TPMS — Tesla emits `tpms_pressure_*` in BAR (matches the
        // raw protobuf semantics and how teslamate / other open-source
        // tools treat the value). Convert to PSI at sample time so
        // every downstream consumer (DB column, API response, UI)
        // sees the unit named on the field. A raw value of 3.0 bar
        // becomes 43.5 PSI, matching what the Tesla app shows on US
        // vehicles.
        tire_fl_psi: tires
            .as_ref()
            .and_then(|t| pick_f64(t, &["tpmsPressureFl", "tpms_pressure_fl"]))
            .map(bar_to_psi),
        tire_fr_psi: tires
            .as_ref()
            .and_then(|t| pick_f64(t, &["tpmsPressureFr", "tpms_pressure_fr"]))
            .map(bar_to_psi),
        tire_rl_psi: tires
            .as_ref()
            .and_then(|t| pick_f64(t, &["tpmsPressureRl", "tpms_pressure_rl"]))
            .map(bar_to_psi),
        tire_rr_psi: tires
            .as_ref()
            .and_then(|t| pick_f64(t, &["tpmsPressureRr", "tpms_pressure_rr"]))
            .map(bar_to_psi),
        // Odometer — Tesla emits this as `odometerInHundredthsOfAMile`
        // (an int) inside the `driveState` object. Convert to miles by
        // dividing by 100. Keep fallback candidates in case the field
        // name changes on a future SDK release.
        odometer_mi: drive.as_ref().and_then(|d| {
            pick_f64(
                d,
                &[
                    "odometerInHundredthsOfAMile",
                    "odometer_in_hundredths_of_a_mile",
                ],
            )
            .map(|hundredths| hundredths / 100.0)
            .or_else(|| {
                // Older / alternate shape — assume already in miles.
                pick_f64(
                    d,
                    &["odometer", "odometerMi", "odometer_mi", "odometerMiles"],
                )
            })
        }),
        // Reverse-geocoded address. Tesla emits it inside the
        // `locationState` object that's also returned by `state drive`
        // (separate from `driveState`); fall back to checking the
        // top-level in case the shape varies. Becomes the drive
        // start/end label after the per-drive rollup matches it (or
        // doesn't) against the known home/work coords.
        location_name: drive.as_ref().and_then(|d| {
            pick_string(d, &["locationName", "location_name"])
        }),
        source: "state".into(),
    };
    // Decode shift state for the phase machine. Accepts string
    // (P/R/N/D) or protobuf int (1/2/3/4). Returned alongside the
    // Sample but NOT stored in the DB — it's purely a transient
    // signal for "should the sampler back off to sleep-safe polling?"
    let shift_state = drive
        .as_ref()
        .and_then(|d| pick_shift_state(d));

    Ok(StateSample {
        sample,
        shift_state,
    })
}

/// Cheap sample via `body-controller-state` — works on a sleeping
/// car, doesn't wake it. The Sample row is mostly NULL (the call
/// doesn't return battery/temp/HVAC). The `user_presence` flag is
/// the real reason to call this: it lets the sampler's phase
/// machine notice when the driver gets back in to a parked car so
/// it can resume state polling without waiting for the next slow
/// asleep-mode tick.
pub async fn sample_body_controller(vin: &str) -> Result<BodyControllerSample> {
    let start = std::time::Instant::now();
    let result = run_tesla_control(vin, &["body-controller-state"]).await;
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

async fn run_state(vin: &str, category: &str) -> Result<Value> {
    let out = run_tesla_control(vin, &["state", category]).await?;
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
async fn run_state_timed(vin: &str, category: &str) -> (Result<Value>, InvocationOutcome) {
    let start = std::time::Instant::now();
    let result = run_state(vin, category).await;
    let outcome = InvocationOutcome {
        elapsed_ms: start.elapsed().as_millis() as u64,
        error: result.as_ref().err().map(|e| format!("{e:#}")),
    };
    (result, outcome)
}

async fn run_tesla_control(vin: &str, subcommand: &[&str]) -> Result<String> {
    let mut args: Vec<&str> =
        vec!["-ble", "-key-file", KEY_FILE, "-vin", vin];
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

fn now_secs() -> i64 {
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
