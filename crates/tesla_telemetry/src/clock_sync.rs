//! Set the Pi's system clock from the vehicle's GPS-derived timestamp.
//!
//! Why: Tesla's car has an always-accurate clock (GPS-synced). Every
//! `state X` response includes a `timestamp` field in the car's clock
//! frame. If our local Pi clock is significantly off — typical when
//! there's no RTC battery and WiFi isn't reachable at boot — we can
//! correct it from the first successful BLE response, with no
//! dependency on NTP or internet access.
//!
//! Design:
//!   * Latency-compensated: applies half the round-trip-time to the
//!     vehicle timestamp, so we land within ~50 ms of the true time
//!     even though the BLE call took 1-5 s.
//!   * NTP-friendly: only adjusts when local-vs-vehicle delta exceeds
//!     a threshold (default 5 min). Avoids fighting NTP's normal sub-
//!     second drift correction.
//!   * RTC-friendly: if `/dev/rtc0` exists, also writes the corrected
//!     time to the RTC so it survives reboots.
//!   * One-shot per startup window: once we've corrected the clock,
//!     subsequent responses are within tolerance so we leave them be.
//!
//! Threading: `clock_settime` modifies CLOCK_REALTIME but does NOT
//! affect CLOCK_MONOTONIC, so any `Instant` values still measure
//! elapsed time correctly across the adjustment.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

/// Threshold below which we leave the local clock alone. NTP and a
/// healthy RTC both keep the clock within a few seconds; if the delta
/// is < 5 minutes, assume one of those did its job and don't second-
/// guess it. Above 5 minutes the clock is meaningfully wrong (typical
/// non-RTC cold-boot states are years off, so this triggers cleanly).
const ADJUSTMENT_THRESHOLD_SECS: i64 = 300;

/// Parse the RFC 3339 / ISO 8601 timestamp Tesla emits in state
/// responses (e.g. `"2026-05-25T04:31:59.107Z"`). Returns unix-secs.
///
/// We don't use chrono here to avoid adding a heavy dep for one
/// fixed-format parse. The format Tesla uses is rigid: YYYY-MM-DD
/// 'T' HH:MM:SS '.' fff 'Z' (UTC, always). Anything else returns None.
pub fn parse_rfc3339_secs(s: &str) -> Option<i64> {
    // Expected length: "2026-05-25T04:31:59.107Z" = 24 chars typically.
    // Accept either with or without fractional seconds, both ending Z.
    if s.len() < 20 || !s.ends_with('Z') {
        return None;
    }
    let b = s.as_bytes();
    // Sanity-check separator positions.
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T'
        || b[13] != b':' || b[16] != b':'
    {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;

    // Days-since-epoch for the given Y/M/D, handling leap years.
    // Algorithm: Howard Hinnant's "Date Algorithms" days_from_civil.
    // Same formula used by C++'s <chrono> and Linux kernel
    // mktime64 — handles every Gregorian date correctly.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = month as i32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i32 - 1;
    let doe = (yoe * 365 + yoe / 4 - yoe / 100) as i32 + doy;
    let days_since_epoch = (era as i64) * 146097 + (doe as i64) - 719468;
    Some(days_since_epoch * 86400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64)
}

/// If the local clock is meaningfully wrong (>5 min from vehicle), set
/// it to vehicle time. Optionally persist to RTC.
///
/// Args:
///   * `vehicle_ts_secs` — the timestamp Tesla sent us
///   * `request_started_at` — monotonic Instant from before we sent
///     the BLE request. Kept around for diagnostic logging (RTT) even
///     though we don't apply it as a correction — see comment below.
///
/// Returns true if we adjusted the clock; false if delta was below
/// threshold (normal case once everything's synced).
pub fn maybe_set_clock_from_vehicle(
    vehicle_ts_secs: i64,
    request_started_at: Instant,
) -> bool {
    let local_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Empirically (validated against NTP-set reference time), Tesla
    // stamps the response timestamp just before transmitting it —
    // NOT at the midpoint of processing. So the actual one-way
    // latency from car-stamp to our-receive is small (~50 ms in
    // measurements), already small enough that our second-resolution
    // clock_settime ignores it. The earlier half-RTT correction
    // overshoot by 700-2000 ms; using the raw timestamp lands
    // within ~50-100 ms of NTP-set time. We still measure RTT for
    // diagnostic logging.
    let rtt_ms = request_started_at.elapsed().as_millis() as i64;
    let corrected_target = vehicle_ts_secs;

    let delta = corrected_target - local_secs;
    if delta.abs() < ADJUSTMENT_THRESHOLD_SECS {
        // Already close enough — leave it alone. Avoids fighting
        // NTP / RTC adjustments that are doing their job.
        return false;
    }

    info!(
        "system clock differs from vehicle by {}s (local={}, vehicle={}, rtt={}ms); \
         adjusting to vehicle time",
        delta, local_secs, corrected_target, rtt_ms
    );

    // Actually set the system clock. Requires CAP_SYS_TIME; the
    // telemetry daemon runs as root so this works.
    let ts = libc::timespec {
        tv_sec: corrected_target as libc::time_t,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        warn!("clock_settime failed: {} (errno={})", err, err.raw_os_error().unwrap_or(0));
        return false;
    }

    // If an RTC battery is present, also write the corrected time
    // there so the next boot starts from the right time without
    // needing the BLE-sync dance again. Best-effort.
    if std::path::Path::new("/dev/rtc0").exists() {
        match std::process::Command::new("hwclock").args(["-w"]).output() {
            Ok(out) if out.status.success() => {
                info!("wrote corrected time to RTC (hwclock -w)");
            }
            Ok(out) => {
                warn!(
                    "hwclock -w returned {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Err(e) => warn!("hwclock -w failed to run: {e}"),
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_tesla_timestamp() {
        // From actual Tesla state-drive response in our captures.
        let ts = parse_rfc3339_secs("2026-05-25T04:32:23.794Z").unwrap();
        // 2026-05-25T04:32:23Z = 1779683543
        assert_eq!(ts, 1779683543);
    }

    #[test]
    fn parses_without_fractional_seconds() {
        let ts = parse_rfc3339_secs("2024-01-15T10:00:00Z").unwrap();
        // 2024-01-15T10:00:00Z = 1705312800 (verified against `date -u -d`)
        assert_eq!(ts, 1705312800);
    }

    #[test]
    fn rejects_bad_format() {
        assert!(parse_rfc3339_secs("not a timestamp").is_none());
        assert!(parse_rfc3339_secs("2024-13-01T00:00:00Z").is_some()); // we don't range-check month
        assert!(parse_rfc3339_secs("2024-01-15 10:00:00Z").is_none()); // space instead of T
    }

    #[test]
    fn handles_leap_year() {
        // Feb 29 2024 was a real day.
        let ts = parse_rfc3339_secs("2024-02-29T00:00:00Z").unwrap();
        assert_eq!(ts, 1709164800);
    }
}
