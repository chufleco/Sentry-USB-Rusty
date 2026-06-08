//! Charging history derived on-demand from `telemetry_samples`.
//!
//! Charge sessions are not a stored table — they are grouped at query
//! time from the per-sample charge columns the experimental sampler
//! writes (`charger_power_kw`, `charge_rate_mph`, ...). A session is a
//! contiguous run of actively-charging samples; a gap longer than
//! `SESSION_GAP_SECS` starts a new one. Energy reported by the car is
//! cumulative within a plug-in and resets to zero on unplug, so the
//! per-session total is the span between the first and last reading.
//!
//! When the experimental flag is off the charge columns are NULL for
//! every row, so the grouping yields nothing and both endpoints return
//! empty results. The flag is also checked up front so a normal install
//! does no query work and surfaces no charging UI.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::router::AppState;

/// A gap larger than this between consecutive charging samples ends the
/// session. The sampler polls charge state well inside this window while
/// a car is plugged in; 30 minutes tolerates a missed poll or two
/// without merging two genuinely separate plug-ins.
const SESSION_GAP_SECS: i64 = 30 * 60;

/// One row pulled from `telemetry_samples`, already filtered to samples
/// where the car was actively charging (see `is_actively_charging`).
struct ChargeRow {
    ts: i64,
    power_kw: Option<i64>,
    current_a: Option<i64>,
    voltage_v: Option<i64>,
    rate_mph: Option<f64>,
    energy_added_kwh: Option<f64>,
    limit_soc: Option<i64>,
    range_mi: Option<f64>,
    battery_pct: Option<f64>,
    location: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Persisted Tesla charge phase (v14+, lowercase). `None` on pre-v14
    /// rows or when the sampler couldn't decode it that tick. When
    /// present, this is the authoritative signal — see
    /// `is_actively_charging`.
    charging_state: Option<String>,
}

/// Summary of one charge session for the list view.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargeSessionSummary {
    /// Session id == start timestamp in unix seconds. Stable and
    /// directly usable as the detail-endpoint key.
    id: i64,
    start_ms: i64,
    end_ms: i64,
    duration_secs: i64,
    location: Option<String>,
    location_lat: Option<f64>,
    location_lon: Option<f64>,
    energy_added_kwh: Option<f64>,
    /// Energy drawn from the charger (wall-side), kWh. Trapezoidal
    /// integral of per-sample charger power, each sample refined from
    /// volts × amps when those agree with the car's coarse integer kW
    /// (see `sample_power_kw`). Normally >= `energy_added_kwh` (the gap is
    /// charging loss); on coarse data it can dip to/under it, which is why
    /// `efficiency_pct` is clamped.
    energy_used_kwh: Option<f64>,
    /// Charging efficiency, percent = added / used, clamped to [0, 100].
    efficiency_pct: Option<f64>,
    peak_power_kw: Option<i64>,
    start_soc: Option<f64>,
    end_soc: Option<f64>,
    start_range_mi: Option<f64>,
    end_range_mi: Option<f64>,
    charge_limit_soc: Option<i64>,
    /// User-assigned tags + the cost derived from them. Filled per-session
    /// by `apply_rates`; empty/None until then.
    tags: Vec<String>,
    cost: Option<f64>,
    /// Resolved price-per-kWh used for `cost` (for UI transparency).
    rate: Option<f64>,
    /// Currency symbol for `cost` (from prefs, default "$").
    currency: String,
}

/// One point on the detail charts. Carries every per-sample series the
/// charging view plots — all sourced from columns the sampler already
/// records, so adding them costs nothing extra on the device.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargePoint {
    ts: i64,
    power_kw: Option<i64>,
    current_a: Option<i64>,
    voltage_v: Option<i64>,
    rate_mph: Option<f64>,
    soc: Option<f64>,
    range_mi: Option<f64>,
    energy_added_kwh: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargeSessionDetail {
    #[serde(flatten)]
    summary: ChargeSessionSummary,
    avg_power_kw: Option<f64>,
    peak_current_a: Option<i64>,
    avg_current_a: Option<f64>,
    peak_voltage_v: Option<i64>,
    avg_voltage_v: Option<f64>,
    peak_rate_mph: Option<f64>,
    points: Vec<ChargePoint>,
}

/// Mean of an iterator of values, or None when it yields nothing. Used
/// for the detail view's average power / current / voltage / temp stats.
fn avg(it: impl Iterator<Item = f64>) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0u32;
    for v in it {
        sum += v;
        n += 1;
    }
    if n == 0 { None } else { Some(sum / n as f64) }
}

/// Largest gap (kW) tolerated between volts × amps and the car's own
/// integer `charger_power` before we distrust the fine-grained product.
/// Integer rounding alone is at most ±0.5 kW; 1.0 kW leaves room for
/// sensor noise while staying well below the ~2 kW-plus gap that 3-phase
/// charging always produces (see `sample_power_kw`).
const POWER_REFINE_TOLERANCE_KW: f64 = 1.0;

/// Best per-sample charger power in kW for energy integration.
///
/// Tesla reports `charger_power` only as whole kilowatts. That's fine at a
/// Supercharger (rounding 150.4 → 150 is <1%) but ruinous on a slow Level
/// 1 charge: 121 V × 12 A is 1.45 kW yet arrives as `1`, a ~30% undercount
/// that drags integrated "used" below the car's own battery-side "added"
/// and pushes efficiency past 100%. When the sample also carries voltage
/// and current we recover the fractional kW from V × I.
///
/// The guard: V × I is the true power only on a single-/split-phase charge
/// (Level 1, and North-American 240 V Level 2). On European 3-phase AC the
/// car reports *per-phase* volts/amps while `charger_power` already sums
/// the phases, so a lone V × I would be ~1/3 of reality. We therefore use
/// V × I only when it agrees with the integer power to within
/// `POWER_REFINE_TOLERANCE_KW`; otherwise we keep the integer. A
/// single-phase product lands within rounding of the integer, while a
/// 3-phase one is always off by (2/3)·total ≈ 2 kW or more — so this
/// refines exactly the low-power single/split-phase case that needs it and
/// is a no-op for 3-phase, DC fast charging, and any sample whose fields
/// disagree. It can only sharpen a value already consistent with the car's
/// own number, never invent a new one.
fn sample_power_kw(r: &ChargeRow) -> Option<f64> {
    let coarse = r.power_kw? as f64;
    match (r.voltage_v, r.current_a) {
        (Some(v), Some(a)) => {
            let fine = v as f64 * a as f64 / 1000.0;
            if (fine - coarse).abs() <= POWER_REFINE_TOLERANCE_KW {
                Some(fine)
            } else {
                Some(coarse)
            }
        }
        _ => Some(coarse),
    }
}

/// Trapezoidal integral of charger power (kW) over a session's samples,
/// in kWh. `None` with fewer than two power readings. In-session samples
/// are <= `SESSION_GAP_SECS` apart by construction, so no gap guard is
/// needed beyond skipping non-positive dt. Per-sample power is the
/// V × I-refined estimate from `sample_power_kw`, not the raw integer kW.
fn integrate_power_kwh(rows: &[ChargeRow]) -> Option<f64> {
    let pts: Vec<(i64, f64)> = rows
        .iter()
        .filter_map(|r| sample_power_kw(r).map(|p| (r.ts, p)))
        .collect();
    if pts.len() < 2 {
        return None;
    }
    let mut kwh = 0.0;
    for w in pts.windows(2) {
        let dt_h = (w[1].0 - w[0].0) as f64 / 3600.0;
        if dt_h > 0.0 {
            kwh += (w[0].1 + w[1].1) / 2.0 * dt_h;
        }
    }
    if kwh > 0.0 { Some(kwh) } else { None }
}

/// One time-of-use price window for a tag, scoped by time-of-day, days of
/// the week, and a month range — the device equivalent of a Tessie "rate
/// schedule". All bounds are in local time.
struct RateSchedule {
    rate: f64,
    /// Local minutes-of-day. `start_min > end_min` wraps past midnight
    /// (e.g. 22:00–06:00 off-peak).
    start_min: i32,
    end_min: i32,
    /// Days the window applies on, 0=Sun..6=Sat. Empty = every day.
    days: Vec<i32>,
    /// Inclusive month range, 1=Jan..12=Dec. `start_month > end_month`
    /// wraps the year (e.g. Nov–Feb).
    start_month: i32,
    end_month: i32,
}

impl RateSchedule {
    /// Whether a local `minute`-of-day / `weekday` (0=Sun) / `month`
    /// (1=Jan) falls in this window. Time is half-open `[start, end)`,
    /// the month range is inclusive, and an empty `days` matches any day.
    fn covers(&self, minute: i32, weekday: i32, month: i32) -> bool {
        self.covers_time(minute) && self.covers_day(weekday) && self.covers_month(month)
    }

    /// Time-of-day in `[start, end)`, wrapping when the window crosses
    /// midnight.
    fn covers_time(&self, min: i32) -> bool {
        if self.start_min <= self.end_min {
            min >= self.start_min && min < self.end_min
        } else {
            min >= self.start_min || min < self.end_min
        }
    }

    fn covers_day(&self, weekday: i32) -> bool {
        self.days.is_empty() || self.days.contains(&weekday)
    }

    /// Inclusive month range, wrapping the year when `start_month >
    /// end_month` (e.g. Nov–Feb covers Nov, Dec, Jan, Feb).
    fn covers_month(&self, month: i32) -> bool {
        if self.start_month <= self.end_month {
            month >= self.start_month && month <= self.end_month
        } else {
            month >= self.start_month || month <= self.end_month
        }
    }
}

/// Pricing for one tag: an optional flat fallback rate plus any number of
/// time-of-use schedules. A charging interval is priced at the first
/// schedule that covers it, else `flat`, else the global default rate.
struct TagRate {
    flat: Option<f64>,
    schedules: Vec<RateSchedule>,
}

impl TagRate {
    /// A plan with neither a flat rate nor a schedule carries no pricing,
    /// so it never wins selection and never costs a session.
    fn is_configured(&self) -> bool {
        self.flat.is_some() || !self.schedules.is_empty()
    }
}

/// Parse a time-of-day pref ("HH:MM", or a bare minute count) into
/// minutes-of-day.
fn parse_minute_of_day(v: &serde_json::Value) -> Option<i32> {
    if let Some(s) = v.as_str() {
        let s = s.trim();
        if let Some((h, m)) = s.split_once(':') {
            let h: i32 = h.trim().parse().ok()?;
            let m: i32 = m.trim().parse().ok()?;
            return ((0..=24).contains(&h) && (0..60).contains(&m))
                .then_some((h * 60 + m).min(1440));
        }
        let m: i32 = s.parse().ok()?;
        return (0..=1440).contains(&m).then_some(m);
    }
    let m = v.as_i64()? as i32;
    (0..=1440).contains(&m).then_some(m)
}

/// Electricity-rate config for charge cost, read from user preferences:
/// `charging_currency` (symbol, default "$"), `charging_default_rate` (the
/// flat price per kWh for untagged sessions / fallback), and
/// `charging_tag_rates` — a `{ tag: plan }` map where each plan is either a
/// bare number (a flat per-tag rate) or `{ flat, schedules }` (a flat rate
/// plus time-of-use schedules). Numeric prefs may arrive as a JSON number
/// or a numeric string (the web inputs send strings).
struct RateConfig {
    currency: String,
    default_rate: Option<f64>,
    tags: std::collections::HashMap<String, TagRate>,
}

impl RateConfig {
    fn load() -> Self {
        let prefs = crate::preferences::load_prefs();
        let currency = prefs
            .get("charging_currency")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("$")
            .to_string();
        let default_rate = prefs.get("charging_default_rate").and_then(num_from_json);
        let tags = prefs
            .get("charging_tag_rates")
            .and_then(|v| v.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), parse_tag_rate(v))).collect())
            .unwrap_or_default();
        Self {
            currency,
            default_rate,
            tags,
        }
    }
}

/// Parse one `charging_tag_rates` entry. Accepts the legacy shape (a bare
/// number / numeric string = a flat rate, no schedules) and the new shape
/// (`{ flat, schedules: [...] }`), so per-tag flat rates set before this
/// feature survive the upgrade.
fn parse_tag_rate(v: &serde_json::Value) -> TagRate {
    if let Some(obj) = v.as_object() {
        let flat = obj.get("flat").and_then(num_from_json);
        let schedules = obj
            .get("schedules")
            .and_then(|s| s.as_array())
            .map(|arr| arr.iter().filter_map(parse_schedule).collect())
            .unwrap_or_default();
        TagRate { flat, schedules }
    } else {
        TagRate {
            flat: num_from_json(v),
            schedules: Vec::new(),
        }
    }
}

/// Parse one schedule object; `None` if it lacks a valid rate or time
/// bounds (mirrors the web editor, which drops such rows on save). A
/// missing/empty `days` means every day; missing months default to the
/// full year.
fn parse_schedule(v: &serde_json::Value) -> Option<RateSchedule> {
    let obj = v.as_object()?;
    Some(RateSchedule {
        rate: num_from_json(obj.get("rate")?)?,
        start_min: parse_minute_of_day(obj.get("start")?)?,
        end_min: parse_minute_of_day(obj.get("end")?)?,
        days: parse_days(obj.get("days")),
        start_month: parse_month(obj.get("startMonth")).unwrap_or(1),
        end_month: parse_month(obj.get("endMonth")).unwrap_or(12),
    })
}

/// Parse a `days` array (0=Sun..6=Sat) into a sorted, deduped, in-range
/// Vec. Missing / empty / all-out-of-range yields an empty Vec, which
/// `RateSchedule::covers_day` treats as "every day".
fn parse_days(v: Option<&serde_json::Value>) -> Vec<i32> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut days: Vec<i32> = arr
        .iter()
        .filter_map(|d| d.as_i64())
        .map(|d| d as i32)
        .filter(|d| (0..=6).contains(d))
        .collect();
    days.sort_unstable();
    days.dedup();
    days
}

/// Parse a month pref (1=Jan..12=Dec) from a JSON number or numeric
/// string; `None` if absent or out of range.
fn parse_month(v: Option<&serde_json::Value>) -> Option<i32> {
    let v = v?;
    let m = v
        .as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))? as i32;
    (1..=12).contains(&m).then_some(m)
}

/// Parse a preference value (JSON number or numeric string) into a
/// non-negative rate. Negative / non-finite / unparseable → `None`.
fn num_from_json(v: &serde_json::Value) -> Option<f64> {
    let n = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// Cost of a session under one rate plan: integrate charger power over
/// each sample interval (trapezoidal, V × I-refined per `sample_power_kw`,
/// so this matches `energy_used_kwh`) and price each interval at the first
/// schedule covering its local time, else the plan's `flat` rate, else the
/// global `default_rate`. Returns `None` when no interval ever resolved a
/// configured rate (so an empty plan with no default leaves cost null) or
/// with too little power data to integrate.
fn plan_cost(
    rows: &[ChargeRow],
    flat: Option<f64>,
    schedules: &[RateSchedule],
    default_rate: Option<f64>,
) -> Option<f64> {
    use chrono::{Datelike, Local, Timelike};
    let pts: Vec<(i64, f64)> = rows
        .iter()
        .filter_map(|r| sample_power_kw(r).map(|p| (r.ts, p)))
        .collect();
    if pts.len() < 2 {
        return None;
    }
    let mut cost = 0.0;
    let mut priced = false;
    for w in pts.windows(2) {
        let dt_h = (w[1].0 - w[0].0) as f64 / 3600.0;
        if dt_h <= 0.0 {
            continue;
        }
        let energy = (w[0].1 + w[1].1) / 2.0 * dt_h;
        let mid_ts = (w[0].0 + w[1].0) / 2;
        // The local clock at the interval midpoint selects the schedule.
        let rate = chrono::DateTime::from_timestamp(mid_ts, 0).and_then(|dt| {
            let local = dt.with_timezone(&Local);
            let minute = local.hour() as i32 * 60 + local.minute() as i32;
            let weekday = local.weekday().num_days_from_sunday() as i32;
            let month = local.month() as i32;
            schedules
                .iter()
                .find(|s| s.covers(minute, weekday, month))
                .map(|s| s.rate)
                .or(flat)
                .or(default_rate)
        });
        if let Some(rate) = rate {
            cost += energy * rate;
            priced = true;
        }
    }
    priced.then_some(cost)
}

/// Fill a summary's tag + cost fields. A session priced by a configured
/// tag plan wins over the untagged default; among multiple configured tag
/// plans the most expensive one wins (order-independent — never
/// under-bill). Cost is charged on energy used (wall-side), so it includes
/// charging loss. `rate` is the effective $/kWh — a blended average when
/// schedules span the session.
fn apply_rates(
    s: &mut ChargeSessionSummary,
    rows: &[ChargeRow],
    tags: Vec<String>,
    rates: &RateConfig,
) {
    // Most expensive configured tag plan the session carries, if any.
    let best_tag_cost = tags
        .iter()
        .filter_map(|t| rates.tags.get(t))
        .filter(|p| p.is_configured())
        .filter_map(|p| plan_cost(rows, p.flat, &p.schedules, rates.default_rate))
        .fold(None, |acc: Option<f64>, c| Some(acc.map_or(c, |a: f64| a.max(c))));

    let (cost, rate) = if let Some(c) = best_tag_cost {
        let blended = match s.energy_used_kwh {
            Some(u) if u > 0.0 => Some(c / u),
            _ => None,
        };
        (Some(c), blended)
    } else if let Some(dr) = rates.default_rate {
        // Untagged, or no configured tag plan: flat default on used energy.
        (s.energy_used_kwh.map(|u| dr * u), Some(dr))
    } else {
        (None, None)
    };
    s.cost = cost;
    s.rate = rate;
    s.currency = rates.currency.clone();
    s.tags = tags;
}

/// Heuristic "is this row charging?" for pre-v14 rows that don't carry a
/// persisted Tesla phase. `rate_mph` is the authoritative truth when the
/// car reports it: nonzero rate = energy is going into the battery; an
/// explicit `Some(0.0)` rate means "not charging" even when `power_kw`
/// is positive (cabin pre-conditioning, 12V top-up, and BMS thermal
/// management all draw power without charging). Only when rate is
/// missing entirely do we fall back to power — that covers the rare
/// decode failure where the car is genuinely charging but the rate field
/// didn't come through.
///
/// For v14+ rows callers should use `is_actively_charging`, which uses
/// the persisted phase directly and falls back to this heuristic only
/// when phase is `None`.
fn is_charging(power_kw: Option<i64>, rate_mph: Option<f64>) -> bool {
    match rate_mph {
        Some(r) => r > 0.0,
        None => power_kw.is_some_and(|p| p > 0),
    }
}

/// Phase-first "is this row charging?" — the predicate `load_charge_rows`
/// uses to decide whether a sample belongs in a charge session. When the
/// row carries a persisted Tesla phase (v14+) it's authoritative:
/// `charging`/`starting`/`calibrating` → yes, everything else → no.
/// Pre-v14 rows fall back to `is_charging` over power and rate.
///
/// Why both layers: phase alone misses the entire pre-v14 fleet; the
/// heuristic alone produces phantom sessions when the car wakes from
/// sleep plugged in but full (it draws a couple of kW for cabin
/// pre-conditioning, the old heuristic saw `power_kw > 0` and counted
/// it as a charge session that added zero kWh).
fn is_actively_charging(
    phase: Option<&str>,
    power_kw: Option<i64>,
    rate_mph: Option<f64>,
) -> bool {
    match phase_is_active(phase) {
        Some(active) => active,
        None => is_charging(power_kw, rate_mph),
    }
}

/// How stale the latest charge row may be before the banner gives up
/// entirely. Generous (24h) because the only case that can leave a
/// "charging" phase on the newest row is a charge that ended while BLE
/// was fully down (so no stopped/complete poll ever landed) — this is
/// the self-healing backstop for that.
const CHARGE_STALE_SECS: i64 = 86_400;

/// True/false if the persisted Tesla charge phase is an actively-charging
/// one. `None` when there's no phase string (pre-v14 rows) so the caller
/// can fall back to the old power/rate heuristic. The spellings mirror
/// `ChargingState::as_db_str` in the telemetry crate (the api crate can't
/// depend on that binary crate, so this is a deliberate string contract).
fn phase_is_active(phase: Option<&str>) -> Option<bool> {
    phase.map(|p| matches!(p, "charging" | "starting" | "calibrating"))
}

/// Pull charging samples in `[from, to]` ordered by time. `to` of
/// `None` means "no upper bound".
fn load_charge_rows(
    conn: &rusqlite::Connection,
    from: i64,
    to: Option<i64>,
) -> anyhow::Result<Vec<ChargeRow>> {
    let upper = to.unwrap_or(i64::MAX);
    // SQL pulls every row with any charge-related signal (phase OR power
    // OR rate non-NULL); the Rust filter below decides whether each one
    // is actually charging via `is_actively_charging`. This split keeps
    // the SQL simple and the predicate unit-testable.
    let mut stmt = conn.prepare(
        "SELECT ts, charger_power_kw, charger_actual_current_a, charger_voltage_v, \
                charge_rate_mph, charge_energy_added_kwh, charge_limit_soc, \
                battery_range_mi, battery_pct, location_name, \
                latitude, longitude, charging_state \
         FROM telemetry_samples \
         WHERE ts BETWEEN ?1 AND ?2 \
           AND (charging_state IS NOT NULL \
                OR charger_power_kw IS NOT NULL \
                OR charge_rate_mph IS NOT NULL) \
         ORDER BY ts ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![from, upper], |r| {
        Ok(ChargeRow {
            ts: r.get(0)?,
            power_kw: r.get(1)?,
            current_a: r.get(2)?,
            voltage_v: r.get(3)?,
            rate_mph: r.get(4)?,
            energy_added_kwh: r.get(5)?,
            limit_soc: r.get(6)?,
            range_mi: r.get(7)?,
            battery_pct: r.get(8)?,
            location: r.get(9)?,
            lat: r.get(10)?,
            lon: r.get(11)?,
            charging_state: r.get(12)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        if is_actively_charging(row.charging_state.as_deref(), row.power_kw, row.rate_mph) {
            out.push(row);
        }
    }
    Ok(out)
}

/// Split time-ordered, already-filtered charging rows into sessions on
/// the gap threshold. Each inner Vec is one session, in time order.
fn group_sessions(rows: Vec<ChargeRow>) -> Vec<Vec<ChargeRow>> {
    let mut sessions: Vec<Vec<ChargeRow>> = Vec::new();
    for row in rows {
        match sessions.last_mut() {
            Some(cur) if row.ts - cur.last().unwrap().ts <= SESSION_GAP_SECS => cur.push(row),
            _ => sessions.push(vec![row]),
        }
    }
    sessions
}

/// Reduce one session's rows to a summary. `rows` is non-empty and
/// time-ordered.
fn summarize(rows: &[ChargeRow]) -> ChargeSessionSummary {
    let first = &rows[0];
    let last = &rows[rows.len() - 1];

    // Energy is cumulative within a plug-in; the span between the first
    // and last reading is what this session added. Clamp at zero so a
    // mid-session counter reset can't produce a negative.
    let energy_added_kwh = match (first.energy_added_kwh, last.energy_added_kwh) {
        (Some(a), Some(b)) => Some((b - a).max(0.0)),
        (None, Some(b)) => Some(b),
        _ => None,
    };

    // Energy drawn from the charger ("used", wall-side): trapezoidal
    // integral of per-sample power over the session, each sample refined
    // from volts × amps where possible (`sample_power_kw`) so a slow Level
    // 1 charge isn't undercounted by integer-kW rounding. Normally >=
    // energy added (battery-side); the gap is charging loss.
    let energy_used_kwh = integrate_power_kwh(rows);

    // Charging efficiency = added / used. Clamp to [0, 100] as a residual
    // safety net: V × I refinement removes the big low-power undercount,
    // but coarse data can still nudge "used" a hair under "added" and
    // yield a >100% artifact that reads as broken.
    let efficiency_pct = match (energy_added_kwh, energy_used_kwh) {
        (Some(added), Some(used)) if used > 0.0 => {
            Some((added / used * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };

    ChargeSessionSummary {
        id: first.ts,
        start_ms: first.ts * 1000,
        end_ms: last.ts * 1000,
        duration_secs: last.ts - first.ts,
        location: rows.iter().find_map(|r| r.location.clone()),
        location_lat: rows.iter().find_map(|r| r.lat),
        location_lon: rows.iter().find_map(|r| r.lon),
        energy_added_kwh,
        energy_used_kwh,
        efficiency_pct,
        peak_power_kw: rows.iter().filter_map(|r| r.power_kw).max(),
        start_soc: rows.iter().find_map(|r| r.battery_pct),
        end_soc: rows.iter().rev().find_map(|r| r.battery_pct),
        start_range_mi: rows.iter().find_map(|r| r.range_mi),
        end_range_mi: rows.iter().rev().find_map(|r| r.range_mi),
        charge_limit_soc: rows.iter().rev().find_map(|r| r.limit_soc),
        // Filled by `apply_rates` once tags + the rate config are known.
        tags: Vec::new(),
        cost: None,
        rate: None,
        currency: String::new(),
    }
}

/// GET /api/charging
///
/// Charge sessions newest-first. Empty when no charging has been sampled.
pub async fn list_charging(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = state
        .drives
        .store
        .with_locked_conn(|conn| load_charge_rows(conn, 0, None));

    match result {
        Ok(rows) => {
            let rates = RateConfig::load();
            let tag_map = state.drives.store.get_all_charge_tags().unwrap_or_default();
            let mut sessions: Vec<ChargeSessionSummary> = group_sessions(rows)
                .iter()
                .map(|s| {
                    let mut summary = summarize(s);
                    let tags = tag_map.get(&summary.id).cloned().unwrap_or_default();
                    apply_rates(&mut summary, s, tags, &rates);
                    summary
                })
                .collect();
            sessions.sort_by(|a, b| b.id.cmp(&a.id));
            (
                StatusCode::OK,
                Json(serde_json::json!({ "sessions": sessions })),
            )
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/charging/{id}
///
/// Detail for the session whose start timestamp is `id`, including the
/// per-sample series for the power / SoC charts. Rows are re-grouped
/// from `id` forward and the first session returned, so the endpoint is
/// stateless and needs no stored session table.
pub async fn single_charging(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Bound the scan so a session that never closes can't read the whole
    // table. One plug-in can't plausibly exceed this; the gap split ends
    // the session well before the bound in practice.
    let window_end = id + 7 * 24 * 60 * 60;
    let result = state
        .drives
        .store
        .with_locked_conn(|conn| load_charge_rows(conn, id, Some(window_end)));

    let rows = match result {
        Ok(rows) => rows,
        Err(e) => return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let session = match group_sessions(rows).into_iter().next() {
        Some(s) => s,
        None => return crate::json_error(StatusCode::NOT_FOUND, "charge session not found"),
    };

    let mut summary = summarize(&session);
    let tags = state
        .drives
        .store
        .get_charge_tags(summary.id)
        .unwrap_or_default();
    apply_rates(&mut summary, &session, tags, &RateConfig::load());

    let points: Vec<ChargePoint> = session
        .iter()
        .map(|r| ChargePoint {
            ts: r.ts * 1000,
            power_kw: r.power_kw,
            current_a: r.current_a,
            voltage_v: r.voltage_v,
            rate_mph: r.rate_mph,
            soc: r.battery_pct,
            range_mi: r.range_mi,
            energy_added_kwh: r.energy_added_kwh,
        })
        .collect();

    let detail = ChargeSessionDetail {
        avg_power_kw: avg(session.iter().filter_map(|r| r.power_kw.map(|v| v as f64))),
        peak_current_a: session.iter().filter_map(|r| r.current_a).max(),
        avg_current_a: avg(session.iter().filter_map(|r| r.current_a.map(|v| v as f64))),
        peak_voltage_v: session.iter().filter_map(|r| r.voltage_v).max(),
        avg_voltage_v: avg(session.iter().filter_map(|r| r.voltage_v.map(|v| v as f64))),
        peak_rate_mph: session
            .iter()
            .filter_map(|r| r.rate_mph)
            .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v)))),
        summary,
        points,
    };

    (StatusCode::OK, Json(serde_json::to_value(detail).unwrap()))
}

/// Live charge status for the dashboard banner. `charging` is false when
/// the latest sample isn't an active charge or is stale (the car stopped
/// being sampled); the other fields are present only while charging.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentCharge {
    charging: bool,
    soc: Option<f64>,
    limit_soc: Option<i64>,
    power_kw: Option<i64>,
    minutes_to_full: Option<i64>,
    range_mi: Option<f64>,
}

impl CurrentCharge {
    fn idle() -> Self {
        Self {
            charging: false,
            soc: None,
            limit_soc: None,
            power_kw: None,
            minutes_to_full: None,
            range_mi: None,
        }
    }
}

/// The single most-recent telemetry row, charge-relevant columns only.
struct LatestCharge {
    ts: i64,
    soc: Option<f64>,
    limit_soc: Option<i64>,
    power_kw: Option<i64>,
    rate_mph: Option<f64>,
    minutes_to_full: Option<i64>,
    range_mi: Option<f64>,
    charging_state: Option<String>,
}

/// GET /api/charging/current — is the car charging right now, with the
/// fields the dashboard banner shows.
///
/// Reads the most-recent *charge-bearing* row (one that carries a charge
/// phase or charger power/rate — also the only rows that carry battery %).
/// The charging decision is phase-first: while the persisted Tesla phase
/// is charging/starting/calibrating the banner stays up for the whole
/// charge regardless of how stale the sample is (the BLE sampler can go
/// minutes between polls mid-charge), and only drops when a poll actually
/// reports a stopped/complete phase. Pre-v14 rows (no phase) fall back to
/// the old "fresh within 10 min AND nonzero power/rate" heuristic.
pub async fn current_charging(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    use rusqlite::OptionalExtension;
    let latest = state.drives.store.with_locked_conn(|conn| {
        conn.query_row(
            "SELECT ts, battery_pct, charge_limit_soc, charger_power_kw, \
                    charge_rate_mph, charge_minutes_to_full, battery_range_mi, \
                    charging_state \
             FROM telemetry_samples \
             WHERE charging_state IS NOT NULL \
                OR charger_power_kw IS NOT NULL \
                OR charge_rate_mph IS NOT NULL \
             ORDER BY ts DESC LIMIT 1",
            [],
            |r| {
                Ok(LatestCharge {
                    ts: r.get(0)?,
                    soc: r.get(1)?,
                    limit_soc: r.get(2)?,
                    power_kw: r.get(3)?,
                    rate_mph: r.get(4)?,
                    minutes_to_full: r.get(5)?,
                    range_mi: r.get(6)?,
                    charging_state: r.get(7)?,
                })
            },
        )
        .optional()
    });

    let cur = match latest {
        Ok(Some(l)) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(l.ts);
            let age = now - l.ts;
            let charging = match phase_is_active(l.charging_state.as_deref()) {
                // Phase says actively charging — hold the banner the whole
                // charge; only the 24h backstop can drop it.
                Some(true) => age <= CHARGE_STALE_SECS,
                // Phase says stopped/complete/disconnected — done, no banner.
                Some(false) => false,
                // Pre-v14 row with no phase — old heuristic.
                None => age <= 600 && is_charging(l.power_kw, l.rate_mph),
            };
            // Battery % is shown for the persistent car-status banner as long
            // as the data is reasonably fresh (<= 24h), so the banner doesn't
            // vanish the moment a charge ends. The charging-only fields are
            // present only while actively charging.
            let soc = if age <= CHARGE_STALE_SECS { l.soc } else { None };
            CurrentCharge {
                charging,
                soc,
                limit_soc: if charging { l.limit_soc } else { None },
                power_kw: if charging { l.power_kw } else { None },
                minutes_to_full: if charging { l.minutes_to_full } else { None },
                range_mi: if charging { l.range_mi } else { l.range_mi.filter(|_| soc.is_some()) },
            }
        }
        _ => CurrentCharge::idle(),
    };
    (StatusCode::OK, Json(serde_json::to_value(cur).unwrap()))
}

#[derive(Deserialize)]
pub struct SetChargeTagsRequest {
    pub tags: Vec<String>,
}

/// GET /api/charging/tags — every charge tag in use, sorted.
pub async fn list_charge_tags(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.get_all_charge_tag_names() {
        Ok(tags) => (
            StatusCode::OK,
            Json(serde_json::to_value(tags).unwrap_or_default()),
        ),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// PUT /api/charging/{id}/tags — set tags for a charge session. `id` is
/// the session's start timestamp (its stable id), so unlike drives it
/// needs no resolution to a canonical key.
pub async fn set_charge_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SetChargeTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.set_charge_tags(id, &body.tags) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct BulkDeleteChargesRequest {
    pub ids: Vec<String>,
}

/// POST /api/charging/bulk-delete — delete charge sessions by id (their
/// start timestamps). A session isn't a stored row; deleting it means
/// removing the charge-bearing telemetry samples in its window (and its
/// tags). The session is derived from those samples, so it disappears
/// once they're gone; non-charge samples in the window are preserved.
pub async fn bulk_delete_charges(
    State(state): State<AppState>,
    Json(body): Json<BulkDeleteChargesRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.ids.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "deleted": 0, "sessions": 0 })),
        );
    }
    let ids: Vec<i64> = body.ids.iter().filter_map(|s| s.parse::<i64>().ok()).collect();

    let result = state
        .drives
        .store
        .with_locked_conn(|conn| -> anyhow::Result<(usize, usize)> {
            let mut deleted = 0usize;
            let mut sessions = 0usize;
            for id in &ids {
                // Re-derive the session window from its start id (bounded
                // scan, same as single_charging), then drop its samples.
                let window_end = *id + 7 * 24 * 60 * 60;
                let rows = load_charge_rows(conn, *id, Some(window_end))?;
                let session = match group_sessions(rows).into_iter().next() {
                    Some(s) if !s.is_empty() => s,
                    _ => continue,
                };
                let start = session.first().unwrap().ts;
                let end = session.last().unwrap().ts;
                deleted += conn.execute(
                    "DELETE FROM telemetry_samples WHERE ts BETWEEN ?1 AND ?2 \
                     AND (charging_state IS NOT NULL \
                          OR charger_power_kw IS NOT NULL \
                          OR charge_rate_mph IS NOT NULL)",
                    rusqlite::params![start, end],
                )?;
                conn.execute(
                    "DELETE FROM charge_tags WHERE session_ts = ?1",
                    rusqlite::params![start],
                )?;
                sessions += 1;
            }
            Ok((deleted, sessions))
        });

    match result {
        Ok((deleted, sessions)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "deleted": deleted, "sessions": sessions })),
        ),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: i64, power: Option<i64>, rate: Option<f64>, energy: Option<f64>) -> ChargeRow {
        ChargeRow {
            ts,
            power_kw: power,
            current_a: None,
            voltage_v: None,
            rate_mph: rate,
            energy_added_kwh: energy,
            limit_soc: None,
            range_mi: None,
            battery_pct: None,
            location: None,
            lat: None,
            lon: None,
            charging_state: None,
        }
    }

    #[test]
    fn gap_splits_into_two_sessions() {
        let rows = vec![
            row(1_000, Some(7), Some(25.0), Some(0.0)),
            row(1_300, Some(7), Some(25.0), Some(1.0)),
            // > 30 min later — new session
            row(1_300 + SESSION_GAP_SECS + 1, Some(11), Some(40.0), Some(0.0)),
        ];
        let sessions = group_sessions(rows);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].len(), 2);
        assert_eq!(sessions[1].len(), 1);
    }

    #[test]
    fn energy_total_is_first_to_last_span() {
        let rows = vec![
            row(1_000, Some(7), Some(25.0), Some(2.0)),
            row(1_300, Some(7), Some(25.0), Some(9.4)),
        ];
        let s = summarize(&rows);
        assert_eq!(s.energy_added_kwh, Some(7.4));
        assert_eq!(s.peak_power_kw, Some(7));
        assert_eq!(s.duration_secs, 300);
        assert_eq!(s.id, 1_000);
    }

    #[test]
    fn non_charging_rows_excluded_by_is_charging() {
        assert!(!is_charging(Some(0), Some(0.0)));
        assert!(!is_charging(None, None));
        assert!(
            is_charging(Some(7), None),
            "no rate signal at all — trust nonzero power as a real-charge proxy",
        );
        assert!(is_charging(None, Some(12.0)));
    }

    #[test]
    fn rate_zero_overrides_nonzero_power() {
        // Regression for the phantom-session bug. On-vehicle, the user
        // woke the car with the cabin AC remote-start on. Car was at
        // 79% with an 80% limit so it wasn't charging, but BMS routed
        // 2 kW to climate. Tesla reported power_kw=2, rate_mph=0.0,
        // energy_added_kwh=17.48 (carried over from the prior charge).
        // The old `power > 0 || rate > 0` predicate said "charging" on
        // the strength of the 2 kW alone, the row entered a "session",
        // a phantom session appeared in the UI with 0 kWh added.
        //
        // The fix: when rate is reported, trust it. An explicit zero
        // rate means no energy is going to the battery, regardless of
        // power draw elsewhere in the car.
        assert!(
            !is_charging(Some(2), Some(0.0)),
            "rate=0 explicitly reported → not charging, even with nonzero power \
             (cabin AC / BMS thermal / 12V top-up all draw power without charging)",
        );
        assert!(
            !is_charging(Some(4), Some(0.0)),
            "and the larger AC-startup draw at wake-from-sleep is not charging either",
        );
    }

    // ── Phase-first session predicate ──────────────────────────────────
    //
    // `is_actively_charging` is what `load_charge_rows` actually uses to
    // decide whether a sample belongs in a charge session. When the row
    // has a persisted Tesla phase (v14+, written by the sampler) the
    // phase is authoritative; pre-v14 rows fall back to `is_charging`.
    // These tests pin both layers.

    #[test]
    fn phase_charging_is_included_even_with_weak_signals() {
        // Tesla says "charging"; trust the phase even if power_kw is
        // reported as 0 (mid-handshake) or rate_mph as None (decode glitch).
        assert!(is_actively_charging(Some("charging"), Some(0), Some(0.0)));
        assert!(is_actively_charging(Some("charging"), None, None));
        assert!(is_actively_charging(Some("starting"), Some(1), None));
        assert!(is_actively_charging(Some("calibrating"), None, Some(0.0)));
    }

    #[test]
    fn phase_complete_excludes_phantom_power_draw() {
        // The on-vehicle scenario again, but now with the v14 phase
        // present. The phase says "complete" (charge limit reached);
        // any power draw at this point is climate / 12V / BMS, NOT
        // charging. Trust the phase, ignore the nonzero power.
        assert!(!is_actively_charging(Some("complete"), Some(2), Some(0.0)));
        assert!(!is_actively_charging(Some("stopped"), Some(4), Some(0.0)));
        assert!(!is_actively_charging(Some("disconnected"), None, None));
        assert!(!is_actively_charging(Some("nopower"), Some(0), Some(0.0)));
        assert!(
            !is_actively_charging(Some("unknown"), Some(7), Some(20.0)),
            "Tesla explicitly said 'unknown'; be conservative — would rather \
             miss a row than create a phantom session",
        );
    }

    #[test]
    fn no_phase_falls_back_to_heuristic() {
        // Pre-v14 row (or v14 row where the sampler couldn't decode the
        // phase that tick): no `charging_state` value persisted. Defer
        // to `is_charging`, which itself prefers rate over power.
        assert!(is_actively_charging(None, Some(4), Some(20.0)));
        assert!(is_actively_charging(None, Some(7), None));
        assert!(!is_actively_charging(None, Some(2), Some(0.0))); // phantom
        assert!(!is_actively_charging(None, None, None));
        assert!(!is_actively_charging(None, Some(0), Some(0.0)));
    }

    #[test]
    fn structs_serialize_camelcase_for_the_web_client() {
        // Regression for the on-vehicle bug: the web UI reads camelCase
        // keys (startMs, energyAddedKwh, powerKw, ...). Without
        // #[serde(rename_all = "camelCase")] the structs emit snake_case,
        // so EVERY field arrives `undefined` → "Invalid Date", NaN
        // duration, "—" stats, 0.0 energy. Pin the wire names here.
        let s = summarize(&[
            row(1_000, Some(7), Some(25.0), Some(2.0)),
            row(1_300, Some(11), Some(40.0), Some(9.4)),
        ]);
        let j = serde_json::to_string(&s).unwrap();
        for key in ["startMs", "endMs", "durationSecs", "energyAddedKwh", "peakPowerKw"] {
            assert!(j.contains(&format!("\"{key}\"")), "summary must emit {key}: {j}");
        }
        assert!(!j.contains("\"start_ms\""), "summary must NOT emit snake_case: {j}");

        // Obviously-synthetic placeholder values — the test asserts only the
        // serialized KEY NAMES (camelCase), never these numbers.
        let p = ChargePoint {
            ts: 1,
            power_kw: Some(1),
            current_a: Some(1),
            voltage_v: Some(1),
            rate_mph: Some(1.0),
            soc: Some(1.0),
            range_mi: Some(1.0),
            energy_added_kwh: Some(1.0),
        };
        let jp = serde_json::to_string(&p).unwrap();
        for key in ["powerKw", "currentA", "voltageV", "rateMph", "rangeMi", "energyAddedKwh"] {
            assert!(jp.contains(&format!("\"{key}\"")), "point must emit {key}: {jp}");
        }
        assert!(!jp.contains("\"power_kw\""), "point must NOT emit snake_case: {jp}");
    }

    // ── Cost + efficiency ──────────────────────────────────────────────

    /// Build a config with a flat default and flat-only tag plans — the
    /// pre-schedule shape, so the cost/efficiency tests read cleanly.
    fn rates(default: Option<f64>, tags: &[(&str, f64)]) -> RateConfig {
        RateConfig {
            currency: "$".into(),
            default_rate: default,
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), TagRate { flat: Some(*v), schedules: Vec::new() }))
                .collect(),
        }
    }

    /// Steady 10 kW for an hour = 10 kWh used; a convenient fixture for
    /// cost math (cost = rate × 10, blended rate = cost / 10).
    fn hour_session() -> [ChargeRow; 2] {
        [
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ]
    }

    /// `Some(x)` within float tolerance of `b` — blended rates and costs go
    /// through a multiply-then-divide, so avoid brittle exact equality.
    fn approx(a: Option<f64>, b: f64) -> bool {
        matches!(a, Some(x) if (x - b).abs() < 1e-9)
    }

    #[test]
    fn highest_cost_tag_plan_wins_then_default() {
        let session = hour_session();
        let r = rates(Some(0.10), &[("Home", 0.12), ("Public", 0.40)]);
        let cost_for = |tags: Vec<String>| {
            let mut s = summarize(&session);
            apply_rates(&mut s, &session, tags, &r);
            s.cost
        };
        // No tags, or a tag with no configured plan → default (0.10 × 10).
        assert!(approx(cost_for(vec![]), 1.0));
        assert!(approx(cost_for(vec!["Work".into()]), 1.0));
        // One configured tag → its rate (0.12 × 10).
        assert!(approx(cost_for(vec!["Home".into()]), 1.2));
        // Multiple → most expensive, independent of order (0.40 × 10).
        assert!(approx(cost_for(vec!["Home".into(), "Public".into()]), 4.0));
        assert!(approx(cost_for(vec!["Public".into(), "Home".into()]), 4.0));
    }

    #[test]
    fn cost_is_none_without_default_or_tag_plan() {
        let session = hour_session();
        let mut s = summarize(&session);
        // A tag with no configured plan and no default → no cost.
        apply_rates(&mut s, &session, vec!["Home".into()], &rates(None, &[]));
        assert_eq!(s.cost, None);
        assert_eq!(s.rate, None);
    }

    #[test]
    fn num_from_json_accepts_number_or_string_rejects_negative() {
        assert_eq!(num_from_json(&serde_json::json!(0.30)), Some(0.30));
        assert_eq!(num_from_json(&serde_json::json!("0.30")), Some(0.30));
        assert_eq!(num_from_json(&serde_json::json!(0)), Some(0.0));
        assert_eq!(num_from_json(&serde_json::json!(-1.0)), None);
        assert_eq!(num_from_json(&serde_json::json!("abc")), None);
    }

    #[test]
    fn energy_used_is_trapezoidal_integral_of_power() {
        // Steady 10 kW across one hour (two samples 3600s apart) = 10 kWh.
        let used = integrate_power_kwh(&[
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ])
        .unwrap();
        assert!((used - 10.0).abs() < 1e-9, "expected 10 kWh, got {used}");
        // Fewer than two power samples → None.
        assert_eq!(integrate_power_kwh(&[row(0, Some(10), None, None)]), None);
    }

    #[test]
    fn low_power_used_refined_from_volts_amps() {
        // Regression for the on-vehicle ">100% efficiency" report. Level 1:
        // 121 V × 12 A = 1.452 kW of real draw, but Tesla reports integer
        // `charger_power` = 1. Integrating the integer undercounts "used"
        // below the car's battery-side "added" and clamps efficiency to
        // 100%. V × I recovers the fractional kW.
        let mut a = row(0, Some(1), Some(4.0), Some(0.0));
        a.voltage_v = Some(121);
        a.current_a = Some(12);
        let mut b = row(3600, Some(1), Some(4.0), Some(1.4));
        b.voltage_v = Some(121);
        b.current_a = Some(12);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 1.452).abs() < 1e-6, "expected ~1.452 kWh from V×I, got {used}");
    }

    #[test]
    fn level2_power_refined_within_tolerance() {
        // North-American 240 V Level 2: 240 V × 48 A = 11.52 kW, integer
        // reported 11. Within tolerance → refine to the accurate 11.52.
        let mut a = row(0, Some(11), Some(40.0), Some(0.0));
        a.voltage_v = Some(240);
        a.current_a = Some(48);
        let mut b = row(3600, Some(11), Some(40.0), Some(11.0));
        b.voltage_v = Some(240);
        b.current_a = Some(48);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 11.52).abs() < 1e-6, "expected V×I refinement 11.52, got {used}");
    }

    #[test]
    fn three_phase_power_falls_back_to_integer() {
        // European 3-phase AC: the car reports PER-PHASE 230 V × 16 A while
        // `charger_power` already sums the phases to integer 11 kW. A lone
        // V × I = 3.68 kW would be ~1/3 of reality, so the guard must reject
        // it and keep 11 — otherwise this "fix" would break 3-phase users
        // worse than the integer-rounding bug it cures.
        let mut a = row(0, Some(11), Some(30.0), Some(0.0));
        a.voltage_v = Some(230);
        a.current_a = Some(16);
        let mut b = row(3600, Some(11), Some(30.0), Some(10.0));
        b.voltage_v = Some(230);
        b.current_a = Some(16);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 11.0).abs() < 1e-9, "expected integer fallback 11 kWh, got {used}");
    }

    #[test]
    fn missing_volts_or_amps_uses_integer_power() {
        // No voltage on the sample (older rows, decode gap) → keep the
        // integer power exactly as before. Pins the unchanged path.
        let mut a = row(0, Some(7), Some(25.0), Some(0.0));
        a.current_a = Some(30); // voltage still None
        let mut b = row(3600, Some(7), Some(25.0), Some(7.0));
        b.current_a = Some(30);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 7.0).abs() < 1e-9, "expected integer 7 kWh when volts missing, got {used}");
    }

    #[test]
    fn summarize_computes_used_and_efficiency_then_apply_rates_costs_on_used() {
        let session = [
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ];
        let mut s = summarize(&session);
        assert_eq!(s.energy_added_kwh, Some(9.0)); // battery-side
        assert_eq!(s.energy_used_kwh, Some(10.0)); // wall-side
        assert_eq!(s.efficiency_pct.map(|p| p.round()), Some(90.0));

        // Cost is rate × used (not added): 0.30 × 10.0 = 3.00.
        apply_rates(&mut s, &session, vec!["Home".into()], &rates(None, &[("Home", 0.30)]));
        assert_eq!(s.tags, vec!["Home".to_string()]);
        assert_eq!(s.rate, Some(0.30));
        assert_eq!(s.cost, Some(3.0));
        assert_eq!(s.currency, "$");
    }

    #[test]
    fn apply_rates_leaves_cost_none_when_no_rate() {
        let session = [
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ];
        let mut s = summarize(&session);
        apply_rates(&mut s, &session, vec![], &rates(None, &[]));
        assert_eq!(s.cost, None);
        assert_eq!(s.rate, None);
    }

    #[test]
    fn schedule_covers_time_day_month() {
        // Off-peak overnight, weekdays (Mon–Fri), summer (Jun–Sep).
        let s = RateSchedule {
            rate: 0.08,
            start_min: 22 * 60,
            end_min: 6 * 60,
            days: vec![1, 2, 3, 4, 5],
            start_month: 6,
            end_month: 9,
        };
        assert!(s.covers(23 * 60, 3, 7)); // 11pm Wed in July — in window
        assert!(s.covers(2 * 60, 3, 7)); // 2am — still off-peak (wraps midnight)
        assert!(!s.covers(12 * 60, 3, 7)); // noon — outside time window
        assert!(!s.covers(23 * 60, 0, 7)); // Sunday — outside day set
        assert!(!s.covers(23 * 60, 3, 12)); // December — outside month range

        // Empty days = every day; month range wrapping the year (Nov–Feb).
        let winter = RateSchedule {
            rate: 0.05,
            start_min: 0,
            end_min: 1440,
            days: vec![],
            start_month: 11,
            end_month: 2,
        };
        assert!(winter.covers(9 * 60, 0, 1)); // January, Sunday — inside wrap
        assert!(winter.covers(9 * 60, 0, 12)); // December — inside wrap
        assert!(!winter.covers(9 * 60, 0, 6)); // June — outside wrapped range
    }

    #[test]
    fn tag_schedule_prices_session() {
        // A tag whose only schedule is all-day/all-year at 0.20 prices the
        // hour session at 0.20 × 10 = 2.00 (timezone-independent).
        let session = hour_session();
        let mut r = rates(None, &[]);
        r.tags.insert(
            "Home".into(),
            TagRate {
                flat: None,
                schedules: vec![RateSchedule {
                    rate: 0.20,
                    start_min: 0,
                    end_min: 1440,
                    days: vec![],
                    start_month: 1,
                    end_month: 12,
                }],
            },
        );
        let mut s = summarize(&session);
        apply_rates(&mut s, &session, vec!["Home".into()], &r);
        assert!(approx(s.cost, 2.0));
        assert!(approx(s.rate, 0.20));
    }

    #[test]
    fn tag_plan_beats_default() {
        // A flat tag plan (Supercharger 0.40) wins over the default 0.10.
        let session = hour_session();
        let mut s = summarize(&session);
        apply_rates(
            &mut s,
            &session,
            vec!["Supercharger".into()],
            &rates(Some(0.10), &[("Supercharger", 0.40)]),
        );
        assert!(approx(s.cost, 4.0)); // 0.40 × 10 used; tag beats default
        assert!(approx(s.rate, 0.40));
    }

    #[test]
    fn parse_tag_rate_accepts_number_and_object() {
        // Legacy shape: a bare number is read as a flat rate, no schedules.
        let legacy = parse_tag_rate(&serde_json::json!(0.04));
        assert_eq!(legacy.flat, Some(0.04));
        assert!(legacy.schedules.is_empty());

        // New shape: flat + a schedule with days + month range. The rate
        // arrives as a string (the web inputs send strings).
        let plan = parse_tag_rate(&serde_json::json!({
            "flat": 0.30,
            "schedules": [{
                "label": "Off-peak",
                "start": "22:00",
                "end": "06:00",
                "days": [1, 2, 3, 4, 5],
                "startMonth": 6,
                "endMonth": 9,
                "rate": "0.08"
            }]
        }));
        assert_eq!(plan.flat, Some(0.30));
        assert_eq!(plan.schedules.len(), 1);
        let sch = &plan.schedules[0];
        assert_eq!(sch.rate, 0.08);
        assert_eq!(sch.start_min, 22 * 60);
        assert_eq!(sch.end_min, 6 * 60);
        assert_eq!(sch.days, vec![1, 2, 3, 4, 5]);
        assert_eq!(sch.start_month, 6);
        assert_eq!(sch.end_month, 9);

        // Object with no flat and no schedules → an unconfigured plan.
        let empty = parse_tag_rate(&serde_json::json!({}));
        assert_eq!(empty.flat, None);
        assert!(!empty.is_configured());
    }

    #[test]
    fn parse_minute_of_day_handles_hhmm_and_numbers() {
        assert_eq!(parse_minute_of_day(&serde_json::json!("06:30")), Some(390));
        assert_eq!(parse_minute_of_day(&serde_json::json!("22:00")), Some(1320));
        assert_eq!(parse_minute_of_day(&serde_json::json!(390)), Some(390));
        assert_eq!(parse_minute_of_day(&serde_json::json!("nope")), None);
        assert_eq!(parse_minute_of_day(&serde_json::json!("25:00")), None);
    }
}
