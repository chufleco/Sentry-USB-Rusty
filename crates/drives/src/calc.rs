//! Single source of truth for drive-stat geometry and unit conversions.
//!
//! Every distance constant, unit factor, and geo function the drives
//! pipeline uses lives here. Before this module these were duplicated:
//! `haversine_m` was defined in both `aggregate.rs` and `grouper.rs`,
//! `MAX_FROM_MEDIAN_M` / `MAX_JUMP_M` were declared locally in two places,
//! and the `1609.344` / `2.23694` literals were scattered across ~8 sites.
//! Drift between any of those copies produced per-drive mileage that
//! disagreed between code paths (the list cache vs `single_drive`).
//!
//! It also mirrors the companion `drive-calc.cjs` module in the Sentry
//! Drive (Electron) app, so the two apps compute identical numbers for the
//! same dataset. Drive moved to WGS-84 geodesic distance; this module
//! matches it (see [`geodesic_m`]).

/// WGS-84 semi-major axis (equatorial radius), meters.
pub const WGS84_A: f64 = 6_378_137.0;

/// WGS-84 flattening.
pub const WGS84_F: f64 = 1.0 / 298.257_223_563;

/// Mean Earth radius used by the legacy spherical [`haversine_m`], meters.
pub const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Meters per statute mile (exact).
pub const M_PER_MILE: f64 = 1609.344;

/// Meters-per-second → miles-per-hour.
pub const MPS_TO_MPH: f64 = 2.236_94;

/// A point farther than this from the drive's median is treated as a GPS
/// outlier and dropped before distance accumulation.
pub const MAX_FROM_MEDIAN_M: f64 = 1_000_000.0;

/// A single inter-point hop longer than this is treated as a GPS glitch
/// (teleport) and excluded from the accumulated distance.
pub const MAX_JUMP_M: f64 = 5000.0;

/// Park-majority threshold for the precise per-segment classifier: a clip
/// segment counts as "parked" when at least this fraction of its frames
/// are in Park.
pub const PARK_MAJORITY_FRACTION: f64 = 0.5;

/// Park-majority threshold for the fast whole-clip heuristic. Deliberately
/// stricter than [`PARK_MAJORITY_FRACTION`] — the fast path approximates
/// from raw frame counts without per-segment splitting, so it needs a
/// higher bar to avoid misclassifying a brief stop as a parked clip. These
/// two thresholds are intentionally different; do NOT unify them.
pub const PARK_MAJORITY_FRACTION_FAST: f64 = 0.6;

/// Geodesic distance between two WGS-84 coordinates in meters, via the
/// Andoyer–Lambert approximation to Vincenty (accurate to <0.5 m for the
/// spans dashcam drives cover). Replaces the spherical haversine, which
/// carried a systematic 0.1–0.3% error that made this app's mileage lag
/// the Sentry Drive app's. Both apps now use this formula.
pub fn geodesic_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    if lat1 == lat2 && lon1 == lon2 {
        return 0.0;
    }
    let to_rad = std::f64::consts::PI / 180.0;
    // Reduced (parametric) latitudes on the ellipsoid.
    let b1 = ((1.0 - WGS84_F) * (lat1 * to_rad).tan()).atan();
    let b2 = ((1.0 - WGS84_F) * (lat2 * to_rad).tan()).atan();
    let p = (b1 + b2) / 2.0;
    let q = (b2 - b1) / 2.0;

    // Central angle σ via the HAVERSINE form: h = sin²(σ/2), then
    // σ = 2·atan2(√h, √(1−h)). This is numerically stable for the short
    // (~1–6 m) hops that 1 Hz dashcam GPS produces — the previous
    // acos(law-of-cosines) form lost ~half the mantissa there (cos σ ≈ 1)
    // and rounded sub-meter hops to 0, undercounting a real drive by
    // ~0.7%. Mirrors Sentry Drive's geodesicM exactly so the two apps
    // agree to floating-point precision.
    let sin_half_dlon = ((lon2 - lon1) * to_rad / 2.0).sin();
    let sin_q = q.sin();
    let h = sin_q * sin_q + b1.cos() * b2.cos() * sin_half_dlon * sin_half_dlon;
    let sigma = 2.0 * h.sqrt().atan2((1.0 - h).max(0.0).sqrt());
    if sigma == 0.0 {
        return 0.0;
    }
    let sin_sigma = sigma.sin();

    // Andoyer first-order flattening correction. Denominators are the
    // half-angle squares from the haversine `h`, clamped away from zero
    // (near σ=0 the numerators vanish faster, so the correction → 0).
    let cos2_half = (1.0 - h).max(1e-12); // cos²(σ/2)
    let sin2_half = h.max(1e-12); // sin²(σ/2)
    let x = (sigma - sin_sigma) * p.sin().powi(2) * q.cos().powi(2) / cos2_half;
    let y = (sigma + sin_sigma) * p.cos().powi(2) * sin_q * sin_q / sin2_half;
    WGS84_A * (sigma - (WGS84_F / 2.0) * (x + y))
}

/// Legacy spherical great-circle distance in meters. Retained only for
/// code that must reproduce the pre-WGS-84 numbers exactly (none in the
/// live path now). New code uses [`geodesic_m`].
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let to_rad = std::f64::consts::PI / 180.0;
    let dlat = (lat2 - lat1) * to_rad;
    let dlon = (lon2 - lon1) * to_rad;
    let a = (dlat / 2.0).sin().powi(2)
        + (lat1 * to_rad).cos() * (lat2 * to_rad).cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    EARTH_RADIUS_M * c
}

/// Meters → miles.
#[inline]
pub fn m_to_mi(m: f64) -> f64 {
    m / M_PER_MILE
}

/// Meters/second → miles/hour.
#[inline]
pub fn mps_to_mph(mps: f64) -> f64 {
    mps * MPS_TO_MPH
}

/// Round to 2 decimal places (distances, speeds).
#[inline]
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Round to 1 decimal place (percentages).
#[inline]
pub fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// True when a point sits at Null Island (0,0) — Tesla's sentinel for "no
/// fix". Both components within 1° of zero is well clear of any real road.
#[inline]
pub fn is_null_island(lat: f64, lon: f64) -> bool {
    lat.abs() < 1.0 && lon.abs() < 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pinned constants — a change here must be deliberate, and must be
    //    mirrored in Sentry Drive's drive-calc.cjs ──
    #[test]
    fn constants_are_pinned() {
        assert_eq!(WGS84_A, 6_378_137.0);
        assert_eq!(WGS84_F, 1.0 / 298.257_223_563);
        assert_eq!(EARTH_RADIUS_M, 6_371_000.0);
        assert_eq!(M_PER_MILE, 1609.344);
        assert_eq!(MPS_TO_MPH, 2.236_94);
        assert_eq!(MAX_FROM_MEDIAN_M, 1_000_000.0);
        assert_eq!(MAX_JUMP_M, 5000.0);
        assert_eq!(PARK_MAJORITY_FRACTION, 0.5);
        assert_eq!(PARK_MAJORITY_FRACTION_FAST, 0.6);
    }

    // ── golden geodesic vectors, cross-validated against Vincenty to
    //    ≤0.4 m. Identical to Sentry Drive's lock test. ──
    #[test]
    fn geodesic_golden_vectors() {
        // NYC → LA
        let d = geodesic_m(40.7128, -74.0060, 34.0522, -118.2437);
        assert!((d - 3_944_422.179).abs() < 1.0, "NYC->LA = {d}");
        // SF → NYC
        let d = geodesic_m(37.7749, -122.4194, 40.7128, -74.0060);
        assert!((d - 4_139_145.867).abs() < 1.0, "SF->NYC = {d}");
        // Short east-west segment (0.0003° lon at 37°N) — pins
        // small-distance precision, the scale most drive points hop.
        let d = geodesic_m(37.0, -122.0, 37.0, -121.9997);
        assert!((d - 26.703_659).abs() < 0.001, "short E-W = {d}");
        // Short north-south segment (0.001° lat).
        let d = geodesic_m(37.0, -122.0, 37.001, -122.0);
        assert!((d - 110.977_539).abs() < 0.001, "short N-S = {d}");
    }

    #[test]
    fn geodesic_same_point_is_zero() {
        assert_eq!(geodesic_m(37.5, -122.3, 37.5, -122.3), 0.0);
    }

    #[test]
    fn geodesic_short_hops_sum_to_direct_line() {
        // A dashcam drive is thousands of ~1-6 m hops between 1 Hz GPS
        // samples. Summing them must equal the straight-line distance.
        // Computing the central angle as acos(law-of-cosines) loses ~half
        // the float mantissa for such short segments (cos σ ≈ 1 − 1e-14)
        // and even rounds sub-meter hops to exactly 0 — undercounting a
        // real drive by ~0.7%. The stable atan2(haversine) central angle
        // (matching Sentry Drive's geodesicM) does not. Walk ~2 km due
        // east in 2000 ~1 m hops and require the sum to match the direct
        // line to floating-point precision.
        let (lat0, lon0) = (37.4_f64, -122.1_f64);
        let lon1 = lon0 + 0.0234; // ~2 km E-W at 37.4°N
        let n = 2000;
        let direct = geodesic_m(lat0, lon0, lat0, lon1);
        let mut summed = 0.0;
        let mut prev_lon = lon0;
        for i in 1..=n {
            let lon = lon0 + (lon1 - lon0) * (i as f64) / (n as f64);
            summed += geodesic_m(lat0, prev_lon, lat0, lon);
            prev_lon = lon;
        }
        let rel = (summed - direct).abs() / direct;
        assert!(
            rel < 1e-6,
            "sum of short hops ({summed:.3} m) must match the direct line \
             ({direct:.3} m); rel error {rel:.2e} — unstable central-angle \
             formula undercounts short segments"
        );
    }

    #[test]
    fn geodesic_is_symmetric() {
        let a = geodesic_m(40.7128, -74.0060, 34.0522, -118.2437);
        let b = geodesic_m(34.0522, -118.2437, 40.7128, -74.0060);
        assert!((a - b).abs() < 1e-6, "asymmetry {a} vs {b}");
    }

    #[test]
    fn geodesic_leads_haversine_by_sub_percent() {
        // The whole point of the migration: geodesic and the old spherical
        // value differ, but only by a fraction of a percent.
        let g = geodesic_m(37.7749, -122.4194, 40.7128, -74.0060);
        let h = haversine_m(37.7749, -122.4194, 40.7128, -74.0060);
        let rel = (g - h).abs() / g;
        assert!(rel < 0.005, "geodesic vs haversine rel diff {rel}");
        assert!(rel > 0.0, "should not be identical");
    }

    #[test]
    fn unit_conversions() {
        assert!((m_to_mi(1609.344) - 1.0).abs() < 1e-12);
        assert!((mps_to_mph(10.0) - 22.3694).abs() < 1e-9);
    }

    #[test]
    fn null_island_detection() {
        assert!(is_null_island(0.0, 0.0));
        assert!(is_null_island(0.5, -0.5));
        assert!(!is_null_island(37.0, -122.0));
    }
}
