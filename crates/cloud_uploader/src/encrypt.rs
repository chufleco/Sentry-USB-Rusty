use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::rand::{SecureRandom, SystemRandom};

use sentryusb_cloud_crypto::{aad, aead, ids};
use sentryusb_drives::charging::{ChargePoint, ChargeSessionSummary};
use sentryusb_drives::types::Route;

#[derive(Debug, Clone)]
pub struct EncryptedRoute {
    pub route_id: String,
    pub route_blob_b64: String,
    pub wrapped_route_key_b64: String,
    /// Compact summary sealed under the same routeKey with
    /// `aad::route_summary`. The plaintext shape mirrors the Sentry
    /// Cloud web client's summary format (v3) exactly — the browser
    /// can't tell which side wrote a summary.
    pub summary_ciphertext_b64: String,

    pub source_file: String,
}

pub fn encrypt_route(
    route: &Route,
    pi_key: &[u8; 32],
    user_id: &str,
    pi_id: &str,
    cached_route_id: Option<&str>,
) -> Result<EncryptedRoute> {

    let route_id = match cached_route_id {
        Some(c) => c.to_string(),
        None => ids::route_id_from_path(&route.file),
    };

    let mut route_key_bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut route_key_bytes)
        .map_err(|_| anyhow::anyhow!("rng failure for route key"))?;

    let route_json = serde_json::to_vec(route).context("serialize Route to JSON")?;
    let blob_aad = aad::route_blob(user_id, pi_id, &route_id);
    let route_key = aead::Key::from_bytes(&route_key_bytes)?;
    let route_blob = aead::seal(&route_key, &blob_aad, &route_json)?;

    let summary_json =
        serde_json::to_vec(&route_summary_json(route)).context("serialize route summary")?;
    let summary_aad = aad::route_summary(user_id, pi_id, &route_id);
    let summary_ct = aead::seal(&route_key, &summary_aad, &summary_json)?;

    let wrap_aad = aad::route_key(user_id, pi_id, &route_id);
    let pi_key_obj = aead::Key::from_bytes(pi_key)?;
    let wrapped = aead::seal(&pi_key_obj, &wrap_aad, &route_key_bytes)?;

    route_key_bytes.fill(0);

    Ok(EncryptedRoute {
        route_id,
        route_blob_b64: B64.encode(&route_blob),
        wrapped_route_key_b64: B64.encode(&wrapped),
        summary_ciphertext_b64: B64.encode(&summary_ct),
        source_file: route.file.clone(),
    })
}

// ── Route summary ─────────────────────────────────────────────────────

const AUTOPILOT_FSD: u8 = 1;
const AUTOPILOT_AUTOSTEER: u8 = 2;
const AUTOPILOT_TACC: u8 = 3;

fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0_f64;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    r * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Compact per-clip summary, format v3: aggregates + `gr` gear runs +
/// optional BLE battery/location fields. The Sentry Cloud web client
/// computes the same summary after a full decrypt — field names,
/// rounding, and filters must stay identical between the two
/// implementations, since the browser consumes this plaintext without
/// knowing which side produced it.
pub fn route_summary_json(route: &Route) -> serde_json::Value {
    let len = route.points.len();
    let ap = &route.autopilot_states;

    let mut kept: u64 = 0;
    let mut dist_m = 0.0_f64;
    let mut prev: Option<(f64, f64)> = None;
    let mut first: Option<(f64, f64)> = None;
    let mut last: Option<(f64, f64)> = None;
    let mut speed_sum = 0.0_f64;
    let mut speed_count: u64 = 0;
    let mut max_speed = 0.0_f64;
    for i in 0..len {
        let p = &route.points[i];
        let (lat, lng) = (p[0], p[1]);
        if !lat.is_finite() || !lng.is_finite() {
            continue;
        }
        if lat == 0.0 && lng == 0.0 {
            continue;
        }
        let sp = route.speeds.get(i).copied().map(|s| s as f64).unwrap_or(0.0);
        let sp_val = if sp.is_finite() && sp > 0.0 { sp } else { 0.0 };
        if let Some((plat, plng)) = prev {
            let seg = haversine_m(plat, plng, lat, lng);
            if seg > 0.0 && seg < 5000.0 {
                dist_m += seg;
            }
        }
        if first.is_none() {
            first = Some((lat, lng));
        }
        last = Some((lat, lng));
        prev = Some((lat, lng));
        if sp_val > 0.0 {
            speed_sum += sp_val;
            speed_count += 1;
            if sp_val > max_speed {
                max_speed = sp_val;
            }
        }
        kept += 1;
    }

    let mut fsd_ms = 0.0_f64;
    let mut as_ms = 0.0_f64;
    let mut tacc_ms = 0.0_f64;
    let dur_ms = 60_000.0_f64;
    let mut dis: u64 = 0;
    if !ap.is_empty() {
        let per_frame = 60_000.0 / ap.len() as f64;
        let mut in_fsd = false;
        for &v in ap {
            if v == AUTOPILOT_FSD {
                fsd_ms += per_frame;
            } else if v == AUTOPILOT_AUTOSTEER {
                as_ms += per_frame;
            } else if v == AUTOPILOT_TACC {
                tacc_ms += per_frame;
            }
            let is_fsd = v == AUTOPILOT_FSD;
            if in_fsd && !is_fsd {
                dis += 1;
            }
            in_fsd = is_fsd;
        }
    }

    let mut gr: Vec<i64> = Vec::with_capacity(route.gear_runs.len() * 2);
    for run in &route.gear_runs {
        gr.push(run.gear as i64);
        gr.push(run.frames as i64);
    }

    let mut out = serde_json::json!({
        "v": 3,
        "file": route.file,
        "ptC": kept,
        "dM": dist_m.round() as i64,
        "dur": dur_ms.round() as i64,
        "fsd": fsd_ms.round() as i64,
        "asm": as_ms.round() as i64,
        "tcc": tacc_ms.round() as i64,
        "dis": dis,
        "sS": round1(speed_sum),
        "sN": speed_count,
        "sMax": round1(max_speed),
        "fLa": first.map(|p| p.0),
        "fLn": first.map(|p| p.1),
        "lLa": last.map(|p| p.0),
        "lLn": last.map(|p| p.1),
        "gr": gr,
    });
    let obj = out.as_object_mut().unwrap();
    // Accel pushes — present only when computable (frame-aligned accel
    // data), so a summary recomputed from reduced data can't overwrite
    // a real count with zero.
    if let Some(acp) = clip_accel_pushes(route) {
        obj.insert("acp".into(), serde_json::json!(acp));
    }
    if let Some(bs) = route.battery_pct_start.filter(|v| v.is_finite()) {
        obj.insert("bs".into(), serde_json::json!(round1(bs)));
    }
    if let Some(be) = route.battery_pct_end.filter(|v| v.is_finite()) {
        obj.insert("be".into(), serde_json::json!(round1(be)));
    }
    if let Some(ls) = route.location_name_start.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("ls".into(), serde_json::json!(truncate_chars(ls, 80)));
    }
    if let Some(le) = route.location_name_end.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("le".into(), serde_json::json!(truncate_chars(le, 80)));
    }
    out
}

/// Accelerator pushes while FSD is engaged, per clip — same rules as
/// the Pi-local aggregator: a press starts when the pedal exceeds 1%
/// at least 3s after FSD engaged, and counts once the pedal returns to
/// 0%. Frame-indexed over the autopilot-state bytes; requires the
/// accel array to be frame-aligned. The web client implements the same
/// rule for locally-derived summaries — keep them identical.
fn clip_accel_pushes(route: &Route) -> Option<u64> {
    let ap = &route.autopilot_states;
    let n = ap.len();
    if n == 0 || route.accel_positions.len() != n {
        return None;
    }
    let dt_ms = 60_000.0 / n as f64;
    let mut pushes: u64 = 0;
    let mut in_press = false;
    let mut engage_idx: isize = -1;
    for i in 0..n {
        let is_fsd = ap[i] == AUTOPILOT_FSD;
        if is_fsd && (i == 0 || ap[i - 1] != AUTOPILOT_FSD) {
            engage_idx = i as isize;
            in_press = false;
        }
        if !is_fsd {
            in_press = false;
            continue;
        }
        let mut accel_pct = route.accel_positions[i] as f64;
        if accel_pct <= 1.0 {
            accel_pct *= 100.0;
        }
        let since_engage_ms = if engage_idx >= 0 {
            (i as isize - engage_idx) as f64 * dt_ms
        } else {
            0.0
        };
        if !in_press && accel_pct > 1.0 && since_engage_ms >= 3000.0 {
            in_press = true;
        } else if in_press && accel_pct <= 0.0 {
            pushes += 1;
            in_press = false;
        }
    }
    Some(pushes)
}

/// `String.prototype.slice(0, n)` equivalent — char-boundary-safe.
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ── Charge sessions ───────────────────────────────────────────────────

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargeBlobWire<'a> {
    #[serde(flatten)]
    summary: &'a ChargeSessionSummary,
    points: &'a [ChargePoint],
}

#[derive(Debug, Clone)]
pub struct EncryptedCharge {
    pub charge_id: String,
    pub charge_blob_b64: String,
    pub wrapped_charge_key_b64: String,
    pub summary_ciphertext_b64: String,
    pub mutable_ciphertext_b64: Option<String>,
}

/// The rewritable `{ tags, costOverride }` envelope plaintext.
/// camelCase to match the web client.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChargeMutable {
    pub tags: Vec<String>,
    pub cost_override: Option<CostOverride>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CostOverride {
    pub amount: f64,
    pub currency: String,
}

pub fn encrypt_charge(
    summary: &ChargeSessionSummary,
    points: &[ChargePoint],
    mutable: Option<&ChargeMutable>,
    pi_key: &[u8; 32],
    user_id: &str,
    pi_id: &str,
) -> Result<EncryptedCharge> {
    let charge_id = ids::charge_id_from_start_ts(summary.id);

    let mut charge_key_bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut charge_key_bytes)
        .map_err(|_| anyhow::anyhow!("rng failure for charge key"))?;
    let charge_key = aead::Key::from_bytes(&charge_key_bytes)?;

    let blob_json = serde_json::to_vec(&ChargeBlobWire { summary, points })
        .context("serialize charge blob")?;
    let blob_ct = aead::seal(
        &charge_key,
        &aad::charge_blob(user_id, pi_id, &charge_id),
        &blob_json,
    )?;

    let summary_json = serde_json::to_vec(summary).context("serialize charge summary")?;
    let summary_ct = aead::seal(
        &charge_key,
        &aad::charge_summary(user_id, pi_id, &charge_id),
        &summary_json,
    )?;

    let mutable_ct = match mutable {
        Some(m) => {
            let json = serde_json::to_vec(m).context("serialize charge mutable")?;
            Some(aead::seal(
                &charge_key,
                &aad::charge_mutable(user_id, pi_id, &charge_id),
                &json,
            )?)
        }
        None => None,
    };

    let pi_key_obj = aead::Key::from_bytes(pi_key)?;
    let wrapped = aead::seal(
        &pi_key_obj,
        &aad::charge_key(user_id, pi_id, &charge_id),
        &charge_key_bytes,
    )?;
    charge_key_bytes.fill(0);

    Ok(EncryptedCharge {
        charge_id,
        charge_blob_b64: B64.encode(&blob_ct),
        wrapped_charge_key_b64: B64.encode(&wrapped),
        summary_ciphertext_b64: B64.encode(&summary_ct),
        mutable_ciphertext_b64: mutable_ct.map(|c| B64.encode(&c)),
    })
}

// ── Mutable-sync seal/open helpers ────────────────────────────────────

/// Unwrap a content key (routeKey or chargeKey) from its base64 wrapped
/// form using this Pi's piKey + the matching key-wrap AAD.
pub fn unwrap_content_key(
    pi_key: &[u8; 32],
    wrapped_b64: &str,
    wrap_aad: &[u8],
) -> Result<[u8; 32]> {
    let wrapped = B64.decode(wrapped_b64).context("decode wrapped key b64")?;
    let pi_key_obj = aead::Key::from_bytes(pi_key)?;
    let raw = aead::open(&pi_key_obj, wrap_aad, &wrapped)?;
    let key: [u8; 32] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("unwrapped key is not 32 bytes"))?;
    Ok(key)
}

/// Seal a JSON-serializable plaintext under a content key, returning b64.
pub fn seal_json_b64<T: serde::Serialize>(
    key_bytes: &[u8; 32],
    aad_bytes: &[u8],
    value: &T,
) -> Result<String> {
    let key = aead::Key::from_bytes(key_bytes)?;
    let json = serde_json::to_vec(value)?;
    Ok(B64.encode(&aead::seal(&key, aad_bytes, &json)?))
}

/// Open a b64 ciphertext under a content key and deserialize the JSON.
pub fn open_json_b64<T: serde::de::DeserializeOwned>(
    key_bytes: &[u8; 32],
    aad_bytes: &[u8],
    ct_b64: &str,
) -> Result<T> {
    let key = aead::Key::from_bytes(key_bytes)?;
    let ct = B64.decode(ct_b64).context("decode ciphertext b64")?;
    let plain = aead::open(&key, aad_bytes, &ct)?;
    Ok(serde_json::from_slice(&plain)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentryusb_drives::types::Route;

    fn sample_route() -> Route {
        Route {
            file: "2026-04-27/clip-front.mp4".to_string(),
            date: "2026-04-27_12-00-00".to_string(),
            points: vec![[40.7, -74.0]],
            gear_states: vec![0, 1, 0],
            autopilot_states: vec![0, 0, 1],
            speeds: vec![10.0, 12.0],
            accel_positions: vec![0.1, 0.2],
            raw_park_count: 1,
            raw_frame_count: 100,
            gear_runs: vec![],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
            ..Default::default()
        }
    }

    #[test]
    fn encrypt_then_local_decrypt_roundtrip() {
        let pi_key = [7u8; 32];
        let user_id = "user-cuid-abc";
        let pi_id = "pi-cuid-xyz";
        let route = sample_route();
        let encrypted = encrypt_route(&route, &pi_key, user_id, pi_id, None).unwrap();

        assert_eq!(encrypted.route_id, ids::route_id_from_path(&route.file));
        assert_eq!(encrypted.route_id.len(), 64);

        let wrapped = B64.decode(&encrypted.wrapped_route_key_b64).unwrap();
        let blob = B64.decode(&encrypted.route_blob_b64).unwrap();

        let pi_key_obj = aead::Key::from_bytes(&pi_key).unwrap();
        let wrap_aad = aad::route_key(user_id, pi_id, &encrypted.route_id);
        let recovered_key_bytes = aead::open(&pi_key_obj, &wrap_aad, &wrapped).unwrap();
        let recovered_key: [u8; 32] = recovered_key_bytes.as_slice().try_into().unwrap();

        let blob_aad = aad::route_blob(user_id, pi_id, &encrypted.route_id);
        let route_key = aead::Key::from_bytes(&recovered_key).unwrap();
        let plaintext = aead::open(&route_key, &blob_aad, &blob).unwrap();
        let recovered_route: Route = serde_json::from_slice(&plaintext).unwrap();

        assert_eq!(recovered_route.file, route.file);
        assert_eq!(recovered_route.points, route.points);
        assert_eq!(recovered_route.speeds, route.speeds);
    }

    #[test]
    fn encrypt_different_routes_produces_distinct_blobs() {
        let pi_key = [9u8; 32];
        let mut a = sample_route();
        let mut b = sample_route();
        a.file = "a.mp4".to_string();
        b.file = "b.mp4".to_string();
        let ea = encrypt_route(&a, &pi_key, "u", "p", None).unwrap();
        let eb = encrypt_route(&b, &pi_key, "u", "p", None).unwrap();
        assert_ne!(ea.route_id, eb.route_id);
        assert_ne!(ea.route_blob_b64, eb.route_blob_b64);
        assert_ne!(ea.wrapped_route_key_b64, eb.wrapped_route_key_b64);
    }

    #[test]
    fn cached_route_id_is_used_verbatim() {
        let pi_key = [1u8; 32];
        let route = sample_route();
        let cached = "deadbeef".repeat(8);
        let e = encrypt_route(&route, &pi_key, "u", "p", Some(&cached)).unwrap();
        assert_eq!(e.route_id, cached);
    }

    /// BLE rollup fields ride inside the encrypted route blob — defend
    /// the wire shape across future refactors. Cloud renders these on
    /// the per-clip + per-drive summaries; losing them silently here
    /// would be invisible until a user opened a drive on the dashboard.
    #[test]
    fn ble_telemetry_roundtrips_through_blob() {
        let pi_key = [3u8; 32];
        let user_id = "user-cuid-xyz";
        let pi_id = "pi-cuid-123";
        let mut route = sample_route();
        route.battery_pct_start = Some(82.0);
        route.battery_pct_end = Some(79.5);
        route.interior_temp_min = Some(19.0);
        route.interior_temp_max = Some(24.5);
        route.exterior_temp_avg = Some(11.0);
        route.hvac_runtime_s = Some(45);
        route.tire_fl_psi = Some(40.5);
        route.tire_fr_psi = Some(40.0);
        route.tire_rl_psi = Some(38.5);
        route.tire_rr_psi = Some(39.0);
        route.odometer_mi_start = Some(12_345.5);
        route.odometer_mi_end = Some(12_346.2);
        route.location_name_start = Some("Home".to_string());
        route.location_name_end = Some("123 Main St".to_string());

        let encrypted = encrypt_route(&route, &pi_key, user_id, pi_id, None).unwrap();
        let wrapped = B64.decode(&encrypted.wrapped_route_key_b64).unwrap();
        let blob = B64.decode(&encrypted.route_blob_b64).unwrap();
        let pi_key_obj = aead::Key::from_bytes(&pi_key).unwrap();
        let wrap_aad = aad::route_key(user_id, pi_id, &encrypted.route_id);
        let recovered_key_bytes = aead::open(&pi_key_obj, &wrap_aad, &wrapped).unwrap();
        let recovered_key: [u8; 32] = recovered_key_bytes.as_slice().try_into().unwrap();
        let blob_aad = aad::route_blob(user_id, pi_id, &encrypted.route_id);
        let route_key = aead::Key::from_bytes(&recovered_key).unwrap();
        let plaintext = aead::open(&route_key, &blob_aad, &blob).unwrap();
        let recovered: Route = serde_json::from_slice(&plaintext).unwrap();

        assert_eq!(recovered.battery_pct_start, Some(82.0));
        assert_eq!(recovered.battery_pct_end, Some(79.5));
        assert_eq!(recovered.interior_temp_min, Some(19.0));
        assert_eq!(recovered.interior_temp_max, Some(24.5));
        assert_eq!(recovered.exterior_temp_avg, Some(11.0));
        assert_eq!(recovered.hvac_runtime_s, Some(45));
        assert_eq!(recovered.tire_fl_psi, Some(40.5));
        assert_eq!(recovered.tire_fr_psi, Some(40.0));
        assert_eq!(recovered.tire_rl_psi, Some(38.5));
        assert_eq!(recovered.tire_rr_psi, Some(39.0));
        assert_eq!(recovered.odometer_mi_start, Some(12_345.5));
        assert_eq!(recovered.odometer_mi_end, Some(12_346.2));
        assert_eq!(recovered.location_name_start.as_deref(), Some("Home"));
        assert_eq!(recovered.location_name_end.as_deref(), Some("123 Main St"));
    }

    fn sample_summary() -> ChargeSessionSummary {
        let rows = vec![
            sentryusb_drives::charging::ChargeRow {
                ts: 1_750_000_000,
                power_kw: Some(11),
                current_a: Some(48),
                voltage_v: Some(240),
                rate_mph: Some(44.0),
                energy_added_kwh: Some(1.0),
                limit_soc: Some(80),
                range_mi: Some(200.0),
                battery_pct: Some(60.0),
                location: Some("Home".to_string()),
                lat: Some(53.5),
                lon: Some(-113.5),
                charging_state: Some("charging".to_string()),
            },
            sentryusb_drives::charging::ChargeRow {
                ts: 1_750_003_600,
                power_kw: Some(11),
                current_a: Some(48),
                voltage_v: Some(240),
                rate_mph: Some(44.0),
                energy_added_kwh: Some(11.5),
                limit_soc: Some(80),
                range_mi: Some(240.0),
                battery_pct: Some(75.0),
                location: Some("Home".to_string()),
                lat: Some(53.5),
                lon: Some(-113.5),
                charging_state: Some("charging".to_string()),
            },
        ];
        sentryusb_drives::charging::summarize(&rows)
    }

    #[test]
    fn encrypt_charge_roundtrips_all_three_slots() {
        let pi_key = [5u8; 32];
        let user_id = "user-cuid-abc";
        let pi_id = "pi-cuid-xyz";
        let summary = sample_summary();
        let points = vec![ChargePoint {
            ts: summary.start_ms,
            power_kw: Some(11),
            current_a: Some(48),
            voltage_v: Some(240),
            rate_mph: Some(44.0),
            soc: Some(60.0),
            range_mi: Some(200.0),
            energy_added_kwh: Some(1.0),
        }];
        let mutable = ChargeMutable {
            tags: vec!["home".to_string()],
            cost_override: Some(CostOverride { amount: 4.20, currency: "$".to_string() }),
        };
        let enc = encrypt_charge(&summary, &points, Some(&mutable), &pi_key, user_id, pi_id)
            .unwrap();
        assert_eq!(enc.charge_id, ids::charge_id_from_start_ts(summary.id));
        assert_eq!(enc.charge_id.len(), 64);

        // Unwrap the chargeKey like the browser/sync would.
        let charge_key = unwrap_content_key(
            &pi_key,
            &enc.wrapped_charge_key_b64,
            &aad::charge_key(user_id, pi_id, &enc.charge_id),
        )
        .unwrap();

        // Summary slot: parses back to the same camelCase shape.
        let summary_back: serde_json::Value = open_json_b64(
            &charge_key,
            &aad::charge_summary(user_id, pi_id, &enc.charge_id),
            &enc.summary_ciphertext_b64,
        )
        .unwrap();
        assert_eq!(summary_back["id"], serde_json::json!(summary.id));
        assert_eq!(summary_back["energyAddedKwh"], serde_json::json!(10.5));
        assert_eq!(summary_back["fastCharging"], serde_json::json!(false));

        // Blob slot: summary fields flattened + points array.
        let blob_back: serde_json::Value = open_json_b64(
            &charge_key,
            &aad::charge_blob(user_id, pi_id, &enc.charge_id),
            &enc.charge_blob_b64,
        )
        .unwrap();
        assert_eq!(blob_back["startMs"], serde_json::json!(summary.start_ms));
        assert_eq!(blob_back["points"].as_array().unwrap().len(), 1);

        // Mutable slot.
        let mutable_back: ChargeMutable = open_json_b64(
            &charge_key,
            &aad::charge_mutable(user_id, pi_id, &enc.charge_id),
            enc.mutable_ciphertext_b64.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(mutable_back, mutable);

        // Cross-slot replay must fail the AEAD check (distinct AADs).
        let swapped: Result<serde_json::Value> = open_json_b64(
            &charge_key,
            &aad::charge_summary(user_id, pi_id, &enc.charge_id),
            &enc.charge_blob_b64,
        );
        assert!(swapped.is_err());
    }

    /// Route summaries must match the v3 shape the Sentry Cloud web
    /// client writes — pin the keys + rounding contract.
    #[test]
    fn route_summary_matches_web_v3_shape() {
        let mut route = sample_route();
        route.points = vec![[53.5, -113.5], [53.501, -113.5]];
        route.speeds = vec![10.0, 12.5];
        route.autopilot_states = vec![1, 1, 0, 3];
        route.gear_runs = vec![];
        route.battery_pct_start = Some(82.46);
        route.location_name_start = Some("Home".to_string());

        let v = route_summary_json(&route);
        assert_eq!(v["v"], serde_json::json!(3));
        assert_eq!(v["file"], serde_json::json!(route.file));
        assert_eq!(v["ptC"], serde_json::json!(2));
        // ~111m between the two points at this latitude.
        let dm = v["dM"].as_i64().unwrap();
        assert!((100..125).contains(&dm), "dM was {}", dm);
        assert_eq!(v["dur"], serde_json::json!(60000));
        // 2 of 4 frames FSD → 30000ms; 1 frame TACC → 15000ms.
        assert_eq!(v["fsd"], serde_json::json!(30000));
        assert_eq!(v["tcc"], serde_json::json!(15000));
        assert_eq!(v["dis"], serde_json::json!(1));
        assert_eq!(v["sS"], serde_json::json!(22.5));
        assert_eq!(v["sN"], serde_json::json!(2));
        assert_eq!(v["sMax"], serde_json::json!(12.5));
        assert_eq!(v["fLa"], serde_json::json!(53.5));
        assert_eq!(v["lLa"], serde_json::json!(53.501));
        assert_eq!(v["bs"], serde_json::json!(82.5));
        assert_eq!(v["ls"], serde_json::json!("Home"));
        assert!(v.get("be").is_none());
        assert!(v.get("le").is_none());
    }

    /// Pins the acp rule (and its omission) against the web's
    /// clipAccelPushes — both sides must produce identical summaries.
    #[test]
    fn route_summary_acp_counts_fsd_accel_pushes() {
        let mut route = sample_route();
        route.points = vec![[53.5, -113.5]; 20];
        route.speeds = vec![10.0; 20];
        // All 20 frames FSD; dt = 3000ms/frame, engage at frame 0.
        route.autopilot_states = vec![1u8; 20];
        // Two presses: frames 2-3 and 10-11, each returning to 0 after.
        let mut accel = vec![0.0f32; 20];
        accel[2] = 0.5;
        accel[3] = 0.5;
        accel[10] = 0.6;
        accel[11] = 0.6;
        route.accel_positions = accel;
        let v = route_summary_json(&route);
        assert_eq!(v["acp"], serde_json::json!(2));

        // Misaligned accel array -> acp omitted, never zero.
        route.accel_positions = vec![0.0; 5];
        let v2 = route_summary_json(&route);
        assert!(v2.get("acp").is_none());
    }

    /// Routes without BLE telemetry should still serialize compactly —
    /// `skip_serializing_if = "Option::is_none"` keeps the wire small
    /// for Pis without the BLE feature enabled, and the cloud's
    /// `#[serde(default)]` deserialization fills None for every field.
    #[test]
    fn route_without_ble_omits_fields_from_blob() {
        let route = sample_route();
        let json = serde_json::to_string(&route).unwrap();
        // None of the BLE field names appear in the camelCase JSON.
        for name in [
            "batteryPctStart", "batteryPctEnd",
            "interiorTempMin", "interiorTempMax", "exteriorTempAvg",
            "hvacRuntimeS",
            "tireFlPsi", "tireFrPsi", "tireRlPsi", "tireRrPsi",
            "odometerMiStart", "odometerMiEnd",
            "locationNameStart", "locationNameEnd",
        ] {
            assert!(!json.contains(name), "BLE field {} leaked into no-telemetry blob: {}", name, json);
        }
    }
}
