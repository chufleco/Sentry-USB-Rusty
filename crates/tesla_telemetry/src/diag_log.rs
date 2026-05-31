//! Periodic single-line BLE diagnostic log.
//!
//! Background task in the sampler that appends one status line per
//! minute to `/mutable/sentryusb-ble.log`. Designed to be human-
//! scannable (grep for `car=Asleep` or `queries=0` to find issues)
//! and rotated at a sensible cap so the file doesn't grow forever.
//!
//! The /api/logs/bluetooth endpoint reads the tail of this file so
//! the operator can scroll back to see "what was happening at 3pm
//! while I was at work" without needing a continuously-polling
//! browser tab.
//!
//! Per-line shape (single ASCII line, ~120 chars):
//!   [2026-05-25 16:30:12 MDT] car=Awake cam_age=45s
//!     state_age=12s bc_age=18s samples_10m=24
//!
//! Fields:
//!   car        - derived from cam_disk.bin mtime: Awake/Idle/Asleep
//!   cam_age    - seconds since last write to cam_disk.bin
//!   state_age  - seconds since last `state` sample landed
//!   bc_age     - seconds since last body-controller sample landed
//!   samples_10m - total samples in the last 10 minutes (any source)

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::Connection;
use tracing::{debug, warn};

use crate::usb_watch::{CarState, observe_path};

const LOG_PATH: &str = "/mutable/sentryusb-ble.log";
const CAM_DISK_PATH: &str = "/backingfiles/cam_disk.bin";
/// Tick interval — one log line per minute keeps the file manageable
/// (~1500 lines/day, ~150 KB at 100 bytes/line) while preserving
/// enough granularity to spot a multi-minute outage.
const TICK: Duration = Duration::from_secs(60);
/// Rotate when the file exceeds this. We don't keep an archive —
/// just truncate to the second half — because the file is purely
/// diagnostic and old data has limited value after a few days.
const ROTATE_AT_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

/// Spawn the logger task. Runs forever — the sampler process owns
/// it and it dies when the process does.
pub fn spawn(db_path: PathBuf) {
    tokio::task::spawn(async move {
        if let Err(e) = run(db_path).await {
            warn!("BLE diag logger exited: {e:#}");
        }
    });
}

async fn run(db_path: PathBuf) -> Result<()> {
    // Each tick we re-open the connection rather than holding one
    // across awaits — sqlite connections aren't Send-safe across
    // async boundaries by default and the open cost is trivial.
    let mut ticker = tokio::time::interval(TICK);
    // Skip the immediate first fire so we don't log before the
    // sampler has had a chance to insert anything.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match build_line(&db_path) {
            Ok(line) => {
                if let Err(e) = append_line(&line) {
                    debug!("diag log append failed: {e:#}");
                }
            }
            Err(e) => debug!("diag log build failed: {e:#}"),
        }
    }
}

fn build_line(db_path: &Path) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Car state from cam_disk.bin mtime — the same signal the
    // sampler's phase machine uses.
    let car = observe_path(Path::new(CAM_DISK_PATH));
    let cam_age_secs: i64 = std::fs::metadata(CAM_DISK_PATH)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| now - d.as_secs() as i64)
        .unwrap_or(-1);

    // Quick DB peek for sample freshness. New connection per tick;
    // sqlite open is microseconds on a local file.
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening telemetry DB at {}", db_path.display()))?;
    let state_ts: Option<i64> = conn
        .query_row(
            "SELECT ts FROM telemetry_samples WHERE source='state' \
             ORDER BY ts DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let bc_ts: Option<i64> = conn
        .query_row(
            "SELECT ts FROM telemetry_samples WHERE source='body_controller' \
             ORDER BY ts DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let since_10m = now - 600;
    let samples_10m: i64 = conn
        .query_row(
            "SELECT count(*) FROM telemetry_samples WHERE ts >= ?1",
            (since_10m,),
            |r| r.get(0),
        )
        .unwrap_or(0);

    let local: DateTime<Local> = Local::now();
    let ts_str = local.format("%Y-%m-%d %H:%M:%S %Z").to_string();

    let car_label = match car {
        CarState::Awake => "Awake",
        CarState::Idle => "Idle",
        CarState::Asleep => "Asleep",
    };

    Ok(format!(
        "[{ts_str}] car={car_label} cam_age={cam_age}s state_age={state} bc_age={bc} samples_10m={samples_10m}\n",
        ts_str = ts_str,
        car_label = car_label,
        cam_age = cam_age_secs,
        state = age_token(state_ts, now),
        bc = age_token(bc_ts, now),
        samples_10m = samples_10m,
    ))
}

fn age_token(ts: Option<i64>, now: i64) -> String {
    match ts {
        Some(t) => format!("{}s", (now - t).max(0)),
        None => "<never>".into(),
    }
}

fn append_line(line: &str) -> Result<()> {
    use std::io::Write;
    rotate_if_needed()?;
    // O_APPEND so concurrent writers (unlikely but safe) get atomic
    // single-line semantics on POSIX.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .with_context(|| format!("opening {} for append", LOG_PATH))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("writing to {}", LOG_PATH))?;
    Ok(())
}

/// Append a one-off, timestamped event line to the persistent BLE log
/// (the same file the Logs → Bluetooth tab shows). Unlike the systemd
/// journal — which is volatile on this Pi and wiped on every reboot —
/// this survives power cuts, so keep-accessory ON/OFF decisions can be
/// reviewed after the fact to diagnose parked-power behavior.
pub fn log_event(msg: &str) {
    let local: DateTime<Local> = Local::now();
    let ts_str = local.format("%Y-%m-%d %H:%M:%S %Z").to_string();
    let line = format!("[{ts_str}] {msg}\n");
    if let Err(e) = append_line(&line) {
        debug!("diag log_event append failed: {e:#}");
    }
}

fn rotate_if_needed() -> Result<()> {
    let meta = match std::fs::metadata(LOG_PATH) {
        Ok(m) => m,
        Err(_) => return Ok(()), // file doesn't exist yet
    };
    if meta.len() < ROTATE_AT_BYTES {
        return Ok(());
    }
    // Keep the most-recent half. Read second half, then truncate +
    // write back. No archive — this is operational diagnostic, not
    // an audit trail; older data has limited debugging value past
    // a few days.
    let raw = std::fs::read(LOG_PATH).context("reading log for rotation")?;
    let half = raw.len() / 2;
    // Trim to the next line boundary so we don't split a line.
    let start = raw[half..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| half + p + 1)
        .unwrap_or(half);
    let kept = &raw[start..];
    std::fs::write(LOG_PATH, kept).context("writing rotated log")?;
    Ok(())
}
