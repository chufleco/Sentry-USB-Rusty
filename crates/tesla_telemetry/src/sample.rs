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

const TESLA_CONTROL: &str = "/root/bin/tesla-control";
const KEY_FILE: &str = "/root/.ble/key_private.pem";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

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
    /// Tesla OS version ("2026.2.9.10"). Only populated on cycles
    /// that include the throttled software-update fetch (see
    /// `sample_state`'s `include_software` flag).
    pub software_version: Option<String>,
    pub source: String,
}

/// Full sample via `state climate` + `state charge`. Wakes the car
/// briefly if it's asleep — only call when we already know the car
/// is awake (recent clip writes).
///
/// `include_software` controls whether the (relatively expensive)
/// software-update fetch runs this cycle. The OS version only
/// changes on OTAs, so the main loop throttles this to ~15 min via
/// `SOFTWARE_VERSION_INTERVAL`.
pub async fn sample_state(vin: &str, include_software: bool) -> Result<Sample> {
    let climate = run_state(vin, "climate").await?;
    let charge = run_state(vin, "charge").await?;
    // tire-pressure / drive / software-update are best-effort:
    // cars or SDK versions that don't expose them return an error
    // instead of populating fields. Don't let any of them fail the
    // whole sample — just leave their fields as None.
    let tires = run_state(vin, "tire-pressure").await.ok();
    let drive = run_state(vin, "drive").await.ok();
    let software = if include_software {
        run_state(vin, "software-update").await.ok()
    } else {
        None
    };
    let now = now_secs();
    // NOTE on battery_temp_c: Tesla's BLE state API does NOT expose
    // battery cell temperature. Both charge_state and climate_state
    // only return `battery_heater_on` / `battery_heater_no_power`
    // (booleans — is the heater running, not how hot the pack is).
    // The BMS knows the temperature internally but it isn't part of
    // the public state query surface. We leave the column nullable
    // in the schema for forward compatibility (in case Tesla adds it
    // later) but we don't waste a probe trying to find it.
    Ok(Sample {
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
        // Odometer lives in drive_state.
        odometer_mi: drive
            .as_ref()
            .and_then(|d| pick_f64(d, &["odometer", "odometerMi"])),
        // Tesla OS version. Tesla exposes the *currently installed*
        // version on different fields depending on SDK build —
        // `carVersion` / `car_version` (top-level vehicle state) or
        // `version` inside the software-update payload. Try the most
        // likely names; degrade to None on unknown shapes.
        software_version: software.as_ref().and_then(|s| {
            pick_string(s, &["carVersion", "car_version", "version"])
        }),
        source: "state".into(),
    })
}

/// Cheap sample via `body-controller-state` — works on a sleeping
/// car, doesn't wake it. Populates only the timestamp and source; the
/// reachability of the car is what's interesting, the absence of
/// other fields is fine and the schema lets them stay NULL.
pub async fn sample_body_controller(vin: &str) -> Result<Sample> {
    let _ = run_tesla_control(vin, &["body-controller-state"]).await?;
    Ok(Sample {
        ts: now_secs(),
        source: "body_controller".into(),
        ..Sample::default()
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
        Value::String(s) if !s.is_empty() => Some(s.clone()),
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
