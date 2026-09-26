//! C6-as-primary telemetry source with in-process-BLE failover.
//!
//! When `TELEMETRY_SOURCE=c6_primary`, the sampler prefers the ESP32-C6
//! co-processor's readings (published by the C6 supervisor as a snapshot file)
//! and only falls back to its own in-process BLE poll when the C6 is genuinely
//! failing — never on a single late poll. The fallback path is the box's
//! existing BLE adapter (onboard or external), so nothing is lost and no
//! contention is added except during a real backfill.
//!
//! This module is the pure decision + mapping layer: it reads the snapshot,
//! decides per-domain whether the C6 is trustworthy right now, and maps the
//! C6's telemetry into the same `Sample` fields the BLE path produces. It does
//! NOT touch the tick loop — the selector wiring lands in a second step.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

use crate::sample::Sample;

/// Where the C6 supervisor publishes its latest snapshot (health + per-domain
/// telemetry + freshness). Same-box IPC, atomic write on the supervisor side.
pub const SNAPSHOT_PATH: &str = "/run/sentryusb-c6/snapshot.json";

/// A domain is considered failed only once its freshest C6 value is older than
/// `SLA_MULTIPLIER × cadence`. Generous on purpose: a couple of missed/nak'd
/// polls are tolerated so the C6 gets real chances to self-recover via its own
/// fast-retry before we bail. This is the "not a quick stale give-up" knob.
pub const SLA_MULTIPLIER: u64 = 4;

/// Charge is tighter than the rest: it feeds keep-awake, so we can't let a gap
/// run long enough for the car to sleep. Still generous, but bounded.
pub const CHARGE_SLA_MULTIPLIER: u64 = 3;

/// Heartbeats beat ~every 5s. Past this with no beat, the C6 is genuinely down
/// (crash / reset / serial loss) → immediate whole-device failover.
pub const HEARTBEAT_DEAD_MS: u64 = 16_000;

/// Count of times the sampler had to fall back to in-process BLE because the
/// C6 wasn't trustworthy. This is the replacement-viability metric: during a
/// C6-primary soak it *is* the C6's real production miss rate.
pub static FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);

/// One domain's cached payload plus its freshness stamps.
#[derive(Debug, Clone)]
pub struct DomainEntry {
    pub fields: Map<String, Value>,
    /// Wall-clock unix ms the supervisor received it. Legacy freshness only:
    /// the Pi has no RTC, so this steps when NTP corrects.
    pub updated_at_ms: u64,
    /// Monotonic age at snapshot-write time (newer supervisors). Preferred.
    pub age_at_write_ms: Option<u64>,
}

/// Parsed supervisor snapshot: whole-device health plus per-domain snapshots.
#[derive(Debug, Clone, Default)]
pub struct C6Snapshot {
    pub healthy: bool,
    /// Heartbeat age at write time; add `snapshot_age_ms` for age now.
    pub last_heartbeat_age_ms: Option<u64>,
    pub serial_connected: Option<bool>,
    pub provisioned: Option<bool>,
    /// False when the supervisor's poll loop is off (C6 not talking to the car).
    pub polling_enabled: Option<bool>,
    /// Kernel boot id + CLOCK_BOOTTIME ms at write (newer supervisors).
    pub boot_id: Option<String>,
    pub written_boottime_ms: Option<u64>,
    /// The supervisor reports car contact (`car_ok_age_ms` key present).
    pub car_link_reported: bool,
    /// Age of the C6's last car contact at write time (None = never).
    pub car_ok_age_at_write_ms: Option<u64>,
    /// Last vehicle_sleep_status the car reported.
    pub car_sleep_status: Option<String>,
    /// How old the snapshot FILE is right now. Set by `read_snapshot`; None =
    /// unknown (treated as not-alive by the coordinator).
    pub snapshot_age_ms: Option<u64>,
    pub domains: HashMap<String, DomainEntry>,
}

impl C6Snapshot {
    /// Parse the supervisor snapshot JSON. Shape combines the supervisor's
    /// `status_json` (health) with `telemetry_json` (domains), which nests the
    /// per-domain payload under a `fields` key:
    /// `{ healthy, last_heartbeat_age_ms, domains: { <d>: { updated_at_ms,
    /// fields: { ... } } } }`. Returns None on any structural miss so the
    /// caller treats an unreadable/garbage snapshot as "C6 unavailable" and
    /// falls back — never trusts a half-parsed snapshot.
    pub fn parse(json: &str) -> Option<C6Snapshot> {
        let v: Value = serde_json::from_str(json).ok()?;
        let healthy = v.get("healthy")?.as_bool()?;
        let last_heartbeat_age_ms = v.get("last_heartbeat_age_ms").and_then(|x| x.as_u64());
        let mut domains = HashMap::new();
        if let Some(map) = v.get("domains").and_then(|d| d.as_object()) {
            for (name, dv) in map {
                let obj = match dv.as_object() {
                    Some(o) => o,
                    None => continue,
                };
                let updated_at_ms = match obj.get("updated_at_ms").and_then(|x| x.as_u64()) {
                    Some(t) => t,
                    None => continue, // no freshness stamp => unusable
                };
                let fields = match obj.get("fields").and_then(|f| f.as_object()) {
                    Some(f) => f.clone(),
                    None => continue, // no payload => unusable
                };
                let age_at_write_ms = obj.get("age_ms").and_then(|x| x.as_u64());
                domains.insert(name.clone(), DomainEntry { fields, updated_at_ms, age_at_write_ms });
            }
        }
        Some(C6Snapshot {
            healthy,
            last_heartbeat_age_ms,
            serial_connected: v.get("serial_connected").and_then(|x| x.as_bool()),
            provisioned: v.get("provisioned").and_then(|x| x.as_bool()),
            polling_enabled: v.get("polling_enabled").and_then(|x| x.as_bool()),
            boot_id: v.get("boot_id").and_then(|x| x.as_str()).map(str::to_string),
            written_boottime_ms: v.get("written_boottime_ms").and_then(|x| x.as_u64()),
            car_link_reported: v.get("car_ok_age_ms").is_some(),
            car_ok_age_at_write_ms: v.get("car_ok_age_ms").and_then(|x| x.as_u64()),
            car_sleep_status: v.get("car_sleep_status").and_then(|x| x.as_str()).map(str::to_string),
            snapshot_age_ms: None,
            domains,
        })
    }

    /// Heartbeat age now: age at write plus how old the file is. A snapshot
    /// that stopped being rewritten (supervisor dead) keeps ageing here
    /// instead of freezing at its last small value.
    pub fn heartbeat_age_ms(&self) -> Option<u64> {
        let at_write = self.last_heartbeat_age_ms?;
        Some(at_write.saturating_add(self.snapshot_age_ms.unwrap_or(0)))
    }

    /// Age of the C6's last car contact now (at-write + file age).
    pub fn car_ok_age_ms(&self) -> Option<u64> {
        Some(self.car_ok_age_at_write_ms?.saturating_add(self.snapshot_age_ms.unwrap_or(u64::MAX / 4)))
    }

    /// A domain's age now. Monotonic when the supervisor stamps `age_ms`,
    /// else the legacy wall-clock difference against `now_wall_ms`.
    pub fn domain_age_ms(&self, domain: &str, now_wall_ms: u64) -> Option<u64> {
        let d = self.domains.get(domain)?;
        match (d.age_at_write_ms, self.snapshot_age_ms, self.written_boottime_ms) {
            (Some(a), Some(s), Some(_)) => Some(a.saturating_add(s)),
            _ => Some(now_wall_ms.saturating_sub(d.updated_at_ms)),
        }
    }
}

/// Read + parse the current supervisor snapshot from tmpfs, stamping how old
/// the file is. Returns None if missing or unparseable (= C6 unavailable).
/// Cheap (~4 KB tmpfs read); the tick reads it once per cycle.
pub fn read_snapshot() -> Option<C6Snapshot> {
    let json = std::fs::read_to_string(SNAPSHOT_PATH).ok()?;
    let mut snap = C6Snapshot::parse(&json)?;
    snap.snapshot_age_ms = match (&snap.boot_id, snap.written_boottime_ms) {
        // Same boot: CLOCK_BOOTTIME difference, immune to wall-clock steps.
        (Some(b), Some(w)) => {
            if crate::c6_coord::boot_id().as_deref() == Some(b.as_str()) {
                crate::c6_coord::boottime_ms().and_then(|now| now.checked_sub(w))
            } else {
                None
            }
        }
        // Legacy supervisor: file mtime. A backward clock step reads as age 0
        // (C6 presumed alive), the direction that never double-drives the key.
        _ => std::fs::metadata(SNAPSHOT_PATH)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| std::time::SystemTime::now().duration_since(t).map(|d| d.as_millis() as u64).unwrap_or(0)),
    };
    Some(snap)
}

/// Why the sampler is (or isn't) using the C6 for a domain this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Trust the C6's cached value for this domain.
    C6,
    /// Fall back to in-process BLE, with the reason (for logging/metrics).
    FallbackBle(FailReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// No snapshot file, or it didn't parse.
    NoSnapshot,
    /// Heartbeats stopped — C6 is down device-wide.
    HeartbeatDead,
    /// Supervisor reports unhealthy (serial down / beats stale).
    Unhealthy,
    /// This domain has no cached value at all.
    DomainMissing,
    /// This domain's freshest value is older than its SLA.
    Stale,
}

impl FailReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FailReason::NoSnapshot => "no_snapshot",
            FailReason::HeartbeatDead => "heartbeat_dead",
            FailReason::Unhealthy => "unhealthy",
            FailReason::DomainMissing => "domain_missing",
            FailReason::Stale => "stale",
        }
    }
}

/// The failover decision for one domain. `cadence_secs` is that domain's normal
/// poll cadence; `now_ms` is wall-clock; the SLA is `multiplier × cadence`.
/// Charge callers pass `CHARGE_SLA_MULTIPLIER`, others `SLA_MULTIPLIER`.
///
/// Order matters: whole-device liveness first (a dead C6 fails every domain),
/// then per-domain freshness. Nothing here is a "one late poll" trigger — the
/// SLA window is multiple cadences wide.
pub fn decide(
    snapshot: Option<&C6Snapshot>,
    domain: &str,
    cadence_secs: u64,
    sla_multiplier: u64,
    now_ms: u64,
) -> Source {
    let snap = match snapshot {
        Some(s) => s,
        None => return Source::FallbackBle(FailReason::NoSnapshot),
    };
    // Whole-device: dead heartbeats or an explicit unhealthy flag fails all.
    if let Some(age) = snap.heartbeat_age_ms() {
        if age > HEARTBEAT_DEAD_MS {
            return Source::FallbackBle(FailReason::HeartbeatDead);
        }
    }
    if !snap.healthy {
        return Source::FallbackBle(FailReason::Unhealthy);
    }
    // Per-domain freshness against a generous SLA.
    let age_ms = match snap.domain_age_ms(domain, now_ms) {
        Some(a) => a,
        None => return Source::FallbackBle(FailReason::DomainMissing),
    };
    let sla_ms = cadence_secs.saturating_mul(sla_multiplier).saturating_mul(1000);
    if age_ms > sla_ms {
        return Source::FallbackBle(FailReason::Stale);
    }
    Source::C6
}

/// Per-domain freshness policy: (expected cadence secs, SLA multiplier),
/// matching the C6 supervisor's poll cadences. Charge is tighter because it
/// gates keep-awake; the rest get the generous default.
fn domain_policy(domain: &str) -> (u64, u64) {
    match domain {
        "drive" => (15, SLA_MULTIPLIER),
        "climate" => (60, SLA_MULTIPLIER),
        "charge" => (60, CHARGE_SLA_MULTIPLIER),
        "closures" => (60, SLA_MULTIPLIER),
        "tires" => (300, SLA_MULTIPLIER),
        "location" => (30, SLA_MULTIPLIER),
        _ => (60, SLA_MULTIPLIER),
    }
}

/// `decide` with the per-domain policy baked in — the tick-loop entry point.
pub fn decide_domain(snapshot: Option<&C6Snapshot>, domain: &str, now_ms: u64) -> Source {
    let (cadence, mult) = domain_policy(domain);
    decide(snapshot, domain, cadence, mult, now_ms)
}

/// Record a fallback (call once each time the selector chooses in-process BLE
/// because the C6 wasn't trustworthy). Returns the running total.
pub fn record_fallback() -> u64 {
    FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1
}

/// Map the C6 `charge` domain fields onto a `Sample`. Field names are the C6's
/// telemetry keys (`{"state":"charge", ...}`); targets are the exact DB columns
/// the BLE path writes, so a C6-sourced row matches a BLE one except `source`.
/// Returns true if it populated at least the SoC or charging_state — a fresh
/// but empty/sparse payload returns false so the caller falls back to BLE
/// rather than write a hollow row and suppress the real result.
pub fn apply_charge(fields: &Map<String, Value>, s: &mut Sample) -> bool {
    let f_i32 = |k: &str| fields.get(k).and_then(|v| v.as_i64()).map(|n| n as i32);
    let f_f32 = |k: &str| fields.get(k).and_then(|v| v.as_f64()).map(|n| n as f32);
    let f_f64 = |k: &str| fields.get(k).and_then(|v| v.as_f64());
    let mut populated = false;

    // Prefer usable_battery_level (Tesla app's headline %), fall back to raw
    // battery_level — mirrors sample_charge_ble so C6 rows match BLE rows.
    if let Some(b) = f_f64("usable_battery_level").or_else(|| f_f64("battery_level")) {
        s.battery_pct = Some(b);
        populated = true;
    }
    s.charger_power_kw = f_i32("charger_power").or(s.charger_power_kw);
    s.charger_actual_current_a = f_i32("charger_actual_current").or(s.charger_actual_current_a);
    s.charger_voltage_v = f_i32("charger_voltage").or(s.charger_voltage_v);
    s.charge_rate_mph = f_f32("charge_rate_mph").or(s.charge_rate_mph);
    s.charge_energy_added_kwh = f_f32("charge_energy_added").or(s.charge_energy_added_kwh);
    s.charge_limit_soc = f_i32("charge_limit_soc").or(s.charge_limit_soc);
    s.battery_range_mi = f_f32("battery_range").or(s.battery_range_mi);
    s.charge_minutes_to_full = f_i32("minutes_to_full_charge").or(s.charge_minutes_to_full);
    if let Some(cs) = fields.get("charging_state").and_then(|v| v.as_str()) {
        // DB stores charging_state lowercase (matches ChargingState::as_db_str).
        s.charging_state = Some(cs.to_lowercase());
        populated = true;
    }
    populated
}

/// Tesla reports TPMS in bar; the DB column is PSI. VERIFIED against a live
/// car: C6 `tpms_pressure_fl` 2.9 bar == sampler `tire_fl_psi` 42.1. Rounds to
/// 0.1 psi to match `sample_ble::bar_to_psi` exactly.
const BAR_TO_PSI: f64 = 14.5038;
fn bar_to_psi(bar: f64) -> f64 {
    ((bar * BAR_TO_PSI) * 10.0).round() / 10.0
}

/// Map the C6 `climate` domain. C6 temps are already Celsius, matching the DB.
/// Returns true if any field was populated (else caller falls back to BLE).
pub fn apply_climate(fields: &Map<String, Value>, s: &mut Sample) -> bool {
    let mut populated = false;
    if let Some(t) = fields.get("inside_temp_celsius").and_then(|v| v.as_f64()) {
        s.interior_temp_c = Some(t);
        populated = true;
    }
    if let Some(t) = fields.get("outside_temp_celsius").and_then(|v| v.as_f64()) {
        s.exterior_temp_c = Some(t);
        populated = true;
    }
    if let Some(on) = fields.get("is_climate_on").and_then(|v| v.as_bool()) {
        s.hvac_on = Some(on);
        populated = true;
    }
    populated
}

/// Map the C6 `drive` domain odometer. The C6 `drive` telemetry carries NO
/// location_name/GPS (Tesla doesn't bundle it in `state drive`, and the C6
/// doesn't poll `state location`), so those stay None and are sourced from SEI
/// / the sampler's own location poll. Returns true if the odometer was present.
pub fn apply_drive(fields: &Map<String, Value>, s: &mut Sample) -> bool {
    if let Some(od) = fields.get("odometer_miles").and_then(|v| v.as_f64()) {
        s.odometer_mi = Some(od);
        return true;
    }
    false
}

/// Map the C6 `tires` domain, bar → PSI (rounded to 0.1) to match the DB
/// column. Returns true if at least one pressure was populated.
pub fn apply_tires(fields: &Map<String, Value>, s: &mut Sample) -> bool {
    let psi = |k: &str| fields.get(k).and_then(|v| v.as_f64()).map(bar_to_psi);
    let mut populated = false;
    if let Some(p) = psi("tpms_pressure_fl") {
        s.tire_fl_psi = Some(p);
        populated = true;
    }
    if let Some(p) = psi("tpms_pressure_fr") {
        s.tire_fr_psi = Some(p);
        populated = true;
    }
    if let Some(p) = psi("tpms_pressure_rl") {
        s.tire_rl_psi = Some(p);
        populated = true;
    }
    if let Some(p) = psi("tpms_pressure_rr") {
        s.tire_rr_psi = Some(p);
        populated = true;
    }
    populated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(healthy: bool, hb_age: Option<u64>, domain: &str, updated_at_ms: u64) -> C6Snapshot {
        let mut domains = HashMap::new();
        domains.insert(
            domain.to_string(),
            DomainEntry { fields: Map::new(), updated_at_ms, age_at_write_ms: None },
        );
        C6Snapshot { healthy, last_heartbeat_age_ms: hb_age, domains, ..Default::default() }
    }

    #[test]
    fn fresh_and_healthy_uses_c6() {
        let s = snap(true, Some(3_000), "charge", 100_000);
        // 60s cadence, ×3 SLA = 180s window; 30s old → fresh.
        assert_eq!(decide(Some(&s), "charge", 60, 3, 130_000), Source::C6);
    }

    #[test]
    fn dead_heartbeat_fails_every_domain() {
        let s = snap(true, Some(20_000), "charge", 130_000); // 20s > 16s dead
        assert_eq!(
            decide(Some(&s), "charge", 60, 3, 131_000),
            Source::FallbackBle(FailReason::HeartbeatDead)
        );
    }

    #[test]
    fn unhealthy_flag_fails() {
        let s = snap(false, Some(3_000), "charge", 130_000);
        assert_eq!(
            decide(Some(&s), "charge", 60, 3, 131_000),
            Source::FallbackBle(FailReason::Unhealthy)
        );
    }

    #[test]
    fn generous_sla_tolerates_a_couple_missed_polls_but_not_a_sustained_gap() {
        // charge 60s ×3 = 180s SLA. 150s old (2+ missed polls) → still C6.
        let s = snap(true, Some(2_000), "charge", 100_000);
        assert_eq!(decide(Some(&s), "charge", 60, 3, 250_000), Source::C6);
        // 200s old (> 180s) → sustained gap → fall back.
        assert_eq!(
            decide(Some(&s), "charge", 60, 3, 300_000),
            Source::FallbackBle(FailReason::Stale)
        );
    }

    #[test]
    fn missing_domain_and_no_snapshot_fall_back() {
        let s = snap(true, Some(2_000), "charge", 100_000);
        assert_eq!(
            decide(Some(&s), "tires", 300, 4, 200_000),
            Source::FallbackBle(FailReason::DomainMissing)
        );
        assert_eq!(
            decide(None, "charge", 60, 3, 200_000),
            Source::FallbackBle(FailReason::NoSnapshot)
        );
    }

    #[test]
    fn parse_reads_health_and_domain_freshness() {
        let json = r#"{"healthy":true,"last_heartbeat_age_ms":1200,
            "domains":{"charge":{"updated_at_ms":170000,"fields":{"battery_level":59,"charging_state":"Charging"}}}}"#;
        let s = C6Snapshot::parse(json).unwrap();
        assert!(s.healthy);
        assert_eq!(s.last_heartbeat_age_ms, Some(1200));
        let d = s.domains.get("charge").unwrap();
        assert_eq!(d.updated_at_ms, 170000);
        assert!(d.fields.get("updated_at_ms").is_none()); // not part of fields payload
        assert_eq!(d.fields.get("battery_level").unwrap().as_i64(), Some(59));
    }

    #[test]
    fn stale_file_ages_heartbeat_and_domains_monotonically() {
        // Supervisor died 60s ago: the file still says hb 1s old, but the file
        // age must push the heartbeat past dead, and domain age uses age_ms.
        let json = r#"{"healthy":true,"last_heartbeat_age_ms":1000,"boot_id":"b","written_boottime_ms":5,
            "domains":{"charge":{"updated_at_ms":999999999,"age_ms":2000,"fields":{"charging_state":"Charging"}}}}"#;
        let mut s = C6Snapshot::parse(json).unwrap();
        s.snapshot_age_ms = Some(60_000);
        assert_eq!(s.heartbeat_age_ms(), Some(61_000));
        // wall clock far BEHIND updated_at (backward NTP step) is ignored.
        assert_eq!(s.domain_age_ms("charge", 0), Some(62_000));
        assert_eq!(
            decide(Some(&s), "charge", 60, 3, 0),
            Source::FallbackBle(FailReason::HeartbeatDead)
        );
    }

    #[test]
    fn parse_rejects_garbage_so_caller_falls_back() {
        assert!(C6Snapshot::parse("not json").is_none());
        assert!(C6Snapshot::parse(r#"{"no_healthy_field":1}"#).is_none());
    }

    #[test]
    fn apply_charge_maps_c6_fields_to_sample_columns() {
        let json = r#"{"battery_level":59,"charger_power":4,"charger_actual_current":20,
            "charger_voltage":235,"charge_rate_mph":21.4,"charge_energy_added":0.18,
            "charge_limit_soc":85,"battery_range":126.9,"minutes_to_full_charge":145,
            "charging_state":"Charging"}"#;
        let fields: Map<String, Value> = serde_json::from_str(json).unwrap();
        let mut s = Sample { ts: 1, source: "c6".into(), ..Default::default() };
        apply_charge(&fields, &mut s);
        assert_eq!(s.battery_pct, Some(59.0));
        assert_eq!(s.charger_power_kw, Some(4));
        assert_eq!(s.charger_actual_current_a, Some(20));
        assert_eq!(s.charge_limit_soc, Some(85));
        assert_eq!(s.charge_minutes_to_full, Some(145));
        assert_eq!(s.charging_state.as_deref(), Some("charging")); // lowercased
    }

    #[test]
    fn apply_climate_maps_celsius_direct() {
        let fields: Map<String, Value> = serde_json::from_str(
            r#"{"inside_temp_celsius":26.6,"outside_temp_celsius":21.0,"is_climate_on":false}"#,
        )
        .unwrap();
        let mut s = Sample::default();
        apply_climate(&fields, &mut s);
        assert_eq!(s.interior_temp_c, Some(26.6));
        assert_eq!(s.exterior_temp_c, Some(21.0));
        assert_eq!(s.hvac_on, Some(false));
    }

    #[test]
    fn apply_drive_maps_odometer_and_leaves_location_none() {
        let fields: Map<String, Value> = serde_json::from_str(
            r#"{"shift_state":"Invalid","odometer_miles":45459.04}"#,
        )
        .unwrap();
        let mut s = Sample::default();
        apply_drive(&fields, &mut s);
        assert_eq!(s.odometer_mi, Some(45459.04));
        // C6 drive carries no location — must NOT invent one.
        assert!(s.location_name.is_none());
        assert!(s.latitude.is_none() && s.longitude.is_none());
    }

    #[test]
    fn apply_tires_converts_bar_to_psi() {
        // Live-verified: 2.9 bar == 42.1 psi.
        let fields: Map<String, Value> = serde_json::from_str(
            r#"{"tpms_pressure_fl":2.9,"tpms_pressure_fr":2.825,"tpms_pressure_rl":2.85,"tpms_pressure_rr":2.875}"#,
        )
        .unwrap();
        let mut s = Sample::default();
        assert!(apply_tires(&fields, &mut s));
        // 2.9 bar * 14.5038 = 42.06, rounded to 0.1 = 42.1 (matches BLE bar_to_psi).
        assert_eq!(s.tire_fl_psi, Some(42.1));
        assert_eq!(s.tire_fr_psi, Some(41.0));
    }

    #[test]
    fn apply_returns_false_on_empty_payload_so_caller_falls_back() {
        let empty: Map<String, Value> = Map::new();
        let mut s = Sample::default();
        assert!(!apply_charge(&empty, &mut s));
        assert!(!apply_climate(&empty, &mut s));
        assert!(!apply_drive(&empty, &mut s));
        assert!(!apply_tires(&empty, &mut s));
    }

    #[test]
    fn apply_charge_prefers_usable_battery_level() {
        let fields: Map<String, Value> =
            serde_json::from_str(r#"{"battery_level":60,"usable_battery_level":57,"charging_state":"Charging"}"#)
                .unwrap();
        let mut s = Sample::default();
        assert!(apply_charge(&fields, &mut s));
        assert_eq!(s.battery_pct, Some(57.0)); // usable, not raw 60
    }
}
