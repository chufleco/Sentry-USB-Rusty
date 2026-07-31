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

#![allow(dead_code)] // wired into the tick loop in the follow-up increment.

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

/// Parsed supervisor snapshot: whole-device health plus per-domain snapshots.
#[derive(Debug, Clone)]
pub struct C6Snapshot {
    pub healthy: bool,
    pub last_heartbeat_age_ms: Option<u64>,
    /// domain → (fields, unix-ms the C6 last delivered it).
    pub domains: HashMap<String, (Map<String, Value>, u64)>,
}

impl C6Snapshot {
    /// Parse the supervisor snapshot JSON. Shape mirrors the supervisor's
    /// `status_json`: `{ healthy, last_heartbeat_age_ms, domains: { <d>: {
    /// updated_at_ms, ...fields } } }`. Returns None on any structural miss so
    /// the caller treats an unreadable/garbage snapshot as "C6 unavailable"
    /// and falls back — never trusts a half-parsed snapshot.
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
                let mut fields = obj.clone();
                fields.remove("updated_at_ms");
                domains.insert(name.clone(), (fields, updated_at_ms));
            }
        }
        Some(C6Snapshot { healthy, last_heartbeat_age_ms, domains })
    }
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
    if let Some(age) = snap.last_heartbeat_age_ms {
        if age > HEARTBEAT_DEAD_MS {
            return Source::FallbackBle(FailReason::HeartbeatDead);
        }
    }
    if !snap.healthy {
        return Source::FallbackBle(FailReason::Unhealthy);
    }
    // Per-domain freshness against a generous SLA.
    let (_, updated_at_ms) = match snap.domains.get(domain) {
        Some(d) => d,
        None => return Source::FallbackBle(FailReason::DomainMissing),
    };
    let sla_ms = cadence_secs.saturating_mul(sla_multiplier).saturating_mul(1000);
    let age_ms = now_ms.saturating_sub(*updated_at_ms);
    if age_ms > sla_ms {
        return Source::FallbackBle(FailReason::Stale);
    }
    Source::C6
}

/// Record a fallback (call once each time the selector chooses in-process BLE
/// because the C6 wasn't trustworthy). Returns the running total.
pub fn record_fallback() -> u64 {
    FALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1
}

/// Map the C6 `charge` domain fields onto a `Sample`. Field names are the C6's
/// telemetry keys (`{"state":"charge", ...}`); targets are the exact DB columns
/// the BLE path writes, so a C6-sourced row is byte-identical to a BLE one
/// except `source`. Only charge fields are touched — climate/drive/tires get
/// their own `apply_*` in the follow-up; unknown/missing keys stay None.
pub fn apply_charge(fields: &Map<String, Value>, s: &mut Sample) {
    let f_i32 = |k: &str| fields.get(k).and_then(|v| v.as_i64()).map(|n| n as i32);
    let f_f32 = |k: &str| fields.get(k).and_then(|v| v.as_f64()).map(|n| n as f32);
    let f_f64 = |k: &str| fields.get(k).and_then(|v| v.as_f64());

    if let Some(b) = f_f64("battery_level") {
        s.battery_pct = Some(b);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(healthy: bool, hb_age: Option<u64>, domain: &str, updated_at_ms: u64) -> C6Snapshot {
        let mut domains = HashMap::new();
        domains.insert(domain.to_string(), (Map::new(), updated_at_ms));
        C6Snapshot { healthy, last_heartbeat_age_ms: hb_age, domains }
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
            "domains":{"charge":{"updated_at_ms":170000,"battery_level":59,"charging_state":"Charging"}}}"#;
        let s = C6Snapshot::parse(json).unwrap();
        assert!(s.healthy);
        assert_eq!(s.last_heartbeat_age_ms, Some(1200));
        let (fields, ts) = s.domains.get("charge").unwrap();
        assert_eq!(*ts, 170000);
        assert!(fields.get("updated_at_ms").is_none()); // stripped
        assert_eq!(fields.get("battery_level").unwrap().as_i64(), Some(59));
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
}
