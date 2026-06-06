//! BLE radio lock — a single file under `/tmp` that says who owns
//! `hci0` right now.
//!
//! Three potential users compete for the radio: this telemetry
//! sampler, the keep-awake nudge loop ([`run/awake_start`]), and the
//! iOS-app GATT daemon (`sentryusb-ble.service`). On a Pi's single
//! controller, two centrals can't reliably run simultaneously, and a
//! central + peripheral coexistence pattern works but is fiddly with
//! BlueZ. So we serialize: whoever holds the lock has exclusive
//! access; the daemon stops `sentryusb-ble` while held, restarts it
//! on release.
//!
//! The file lives at `/tmp/ble_radio_owner` and contains
//! `<owner-name>\n<unix_seconds>\n`. Both the bash keep-awake script
//! and this daemon read/write it. A stale lock (>24h old) is treated
//! as crashed and re-claimed.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

/// Shared lock path. Coordinated with [`run/awake_start`]'s
/// `BLE_LOCK` constant — keep the two in sync.
pub const LOCK_PATH: &str = "/tmp/ble_radio_owner";

/// Stale-lock threshold. Matches `BLE_LOCK_MAX_AGE` in `awake_start`.
/// Acts as a worst-case safety net — the orphan check below catches
/// most real failure cases within a minute.
const STALE_AFTER_SECS: i64 = 86_400;

/// Minimum age before a `keep_awake`-owned lock is eligible to be
/// treated as orphaned. Gives `archiveloop` a window to start
/// writing `/tmp/archive_status.json` after `awake_start` returns,
/// so we don't race-steal a legitimately-fresh archive cycle.
const KEEP_AWAKE_ORPHAN_GRACE_SECS: i64 = 60;

/// archiveloop status file. Mtime-fresh within 120s means archive is
/// actively running. Matches the staleness logic in
/// `crates/api/src/drives_handler.rs::read_archive_status`.
const ARCHIVE_STATUS_PATH: &str = "/tmp/archive_status.json";
const ARCHIVE_STATUS_FRESH_SECS: u64 = 120;

/// PID file written by the Case-3 keep-awake nudge loop in awake_start.
const NUDGE_PID_FILE: &str = "/tmp/keep_awake_nudge_pid";

/// Acquire the radio lock for `owner`. Returns `true` if we now hold
/// it. Returns `false` if another fresh owner holds it — callers
/// should back off and retry later.
///
/// Best-effort, not strictly atomic: there's a small race between the
/// "is it stale?" check and the write. In practice the three callers
/// (keep-awake, telemetry, future) don't fight in tight loops — they
/// hold the lock for seconds-to-minutes at a time, and the cost of a
/// lost race is just one extra retry on the next 5-second tick.
pub fn try_acquire(owner: &str) -> Result<bool> {
    let now = now_secs();

    if Path::new(LOCK_PATH).exists() {
        match read_lock() {
            Ok((existing_owner, ts)) => {
                if existing_owner == owner {
                    // Re-acquire — refresh the timestamp so a long
                    // hold doesn't appear stale.
                    write_lock(owner, now)?;
                    return Ok(true);
                }
                let age = now - ts;
                if age > STALE_AFTER_SECS {
                    warn!(
                        "BLE radio lock held by '{}' for {}s — assuming crashed, taking over",
                        existing_owner, age
                    );
                    write_lock(owner, now)?;
                    return Ok(true);
                }
                // Orphan check for keep_awake: archiveloop bails on
                // set -e errors and (without the EXIT trap) leaves
                // the lock dangling. After a 60s grace, if there's
                // no fresh archive_status.json AND no live nudge
                // process, the lock is dead — take over rather than
                // wait the full 24h.
                if existing_owner == "keep_awake"
                    && age >= KEEP_AWAKE_ORPHAN_GRACE_SECS
                    && !is_archive_active()
                    && !is_nudge_alive()
                {
                    warn!(
                        "BLE radio lock owned by keep_awake but no active archive/nudge ({}s old) — orphan, taking over",
                        age
                    );
                    write_lock(owner, now)?;
                    return Ok(true);
                }
                return Ok(false);
            }
            Err(e) => {
                warn!("BLE radio lock file unreadable ({}) — overwriting", e);
                write_lock(owner, now)?;
                return Ok(true);
            }
        }
    }

    write_lock(owner, now)?;
    info!("BLE radio lock acquired by '{}'", owner);
    Ok(true)
}

/// Release the radio lock if we own it. No-op if the file is missing
/// or owned by someone else (some other component may have taken it
/// over due to staleness).
pub fn release(owner: &str) -> Result<()> {
    if !Path::new(LOCK_PATH).exists() {
        return Ok(());
    }
    match read_lock() {
        Ok((existing_owner, _)) if existing_owner == owner => {
            fs::remove_file(LOCK_PATH)
                .with_context(|| format!("failed to remove {}", LOCK_PATH))?;
            info!("BLE radio lock released by '{}'", owner);
        }
        Ok((other, _)) => {
            warn!(
                "BLE radio lock owned by '{}', not us ('{}') — not releasing",
                other, owner
            );
        }
        Err(e) => {
            warn!("BLE radio lock unreadable on release ({}) — leaving alone", e);
        }
    }
    Ok(())
}

/// Returns the current owner string if the lock exists, else `None`.
/// Diagnostic helper — never use this to decide whether to acquire
/// (use [`try_acquire`] for that, which handles staleness).
pub fn current_owner() -> Option<String> {
    read_lock().ok().map(|(owner, _)| owner)
}

fn read_lock() -> Result<(String, i64)> {
    let contents = fs::read_to_string(LOCK_PATH)
        .with_context(|| format!("failed to read {}", LOCK_PATH))?;
    let mut lines = contents.lines();
    let owner = lines
        .next()
        .ok_or_else(|| anyhow!("lock file empty"))?
        .trim()
        .to_string();
    let ts = lines
        .next()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    Ok((owner, ts))
}

fn write_lock(owner: &str, ts: i64) -> Result<()> {
    // Atomic-ish: write to .tmp then rename. Cheap because /tmp is
    // tmpfs.
    let tmp = format!("{}.tmp", LOCK_PATH);
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp))?;
        writeln!(f, "{}", owner)?;
        writeln!(f, "{}", ts)?;
    }
    fs::rename(&tmp, LOCK_PATH)
        .with_context(|| format!("failed to rename {} -> {}", tmp, LOCK_PATH))?;
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// True when archiveloop is currently running (status file fresh
/// within 120s). Used by the orphan-lock check to distinguish a
/// stuck "keep_awake" lock from a real archive cycle.
pub fn is_archive_active() -> bool {
    let Ok(meta) = std::fs::metadata(ARCHIVE_STATUS_PATH) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs() < ARCHIVE_STATUS_FRESH_SECS)
        .unwrap_or(false)
}

/// True when `/tmp/keep_awake_nudge_pid` exists and the PID is
/// still alive. Detected via `/proc/<pid>` existence — no libc
/// dependency. False positives only possible on PID reuse, which
/// is rare on a Pi with a sparse process table; in that case we
/// just don't steal the lock and fall through to the 24h safety net.
fn is_nudge_alive() -> bool {
    let Ok(pid_str) = std::fs::read_to_string(NUDGE_PID_FILE) else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Cede-sentinel path. When this file exists, the radio actor (the
/// experimental broker) has claimed exclusive responsibility for
/// sampling, and any standalone legacy sampler loop must CEDE — stop
/// driving its own polls — so the two can never double-sample the car
/// over the one radio.
///
/// Lives on tmpfs alongside the radio-owner lock. The broker creates it
/// when it takes over and removes it on shutdown; a normal (flag-off)
/// install never creates it, so the legacy loop runs exactly as before.
pub const CEDE_SENTINEL_PATH: &str = "/tmp/ble_sampler_ceded";

/// Mark that the actor/broker owns sampling: create the cede-sentinel so
/// a standalone sampler defers. Best-effort on tmpfs.
pub fn set_sampler_ceded() -> Result<()> {
    fs::write(CEDE_SENTINEL_PATH, b"ceded\n")
        .with_context(|| format!("failed to create cede sentinel {}", CEDE_SENTINEL_PATH))
}

/// Clear the cede-sentinel (broker shutting down / handing sampling
/// back). No-op if absent.
pub fn clear_sampler_ceded() -> Result<()> {
    match fs::remove_file(CEDE_SENTINEL_PATH) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("failed to remove {}", CEDE_SENTINEL_PATH))
        }
    }
}

/// Whether a standalone sampler should cede to the broker. True iff the
/// cede-sentinel exists. This is the READ side of the sentinel: a
/// standalone sampler calls it to defer so that, even if a broker is
/// running in the same install, the two never sample concurrently over
/// the single radio. The broker only writes the sentinel (it never reads
/// it), so this is unused in the broker binary itself — it is the API the
/// deferring sampler consumes, and the unit tests exercise it.
#[allow(dead_code)]
pub fn sampler_should_cede() -> bool {
    Path::new(CEDE_SENTINEL_PATH).exists()
}

/// True when something currently wants the car kept awake: an archive
/// cycle (fresh `archive_status.json`) or any keep-awake nudge loop
/// (`awake_start`'s Case-3 PID file — archiveloop, web-UI, drive
/// processing). The quiet-mode gate consults this so it never green-lights
/// sleep out from under an in-flight archive. Both inputs self-clear — the
/// status file goes stale (120s), the PID dies — so this can't wedge the
/// car permanently awake once the work finishes.
pub fn keep_awake_requested() -> bool {
    is_archive_active() || is_nudge_alive()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The lock path is global — tests that mutate it serialize via
    // this mutex to avoid cross-test interference.
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());

    fn with_tmp_lock<F: FnOnce()>(f: F) {
        let _g = LOCK.lock().unwrap();
        let _ = fs::remove_file(LOCK_PATH);
        f();
        let _ = fs::remove_file(LOCK_PATH);
    }

    #[test]
    fn acquire_when_unheld_succeeds() {
        with_tmp_lock(|| {
            assert!(try_acquire("telemetry").unwrap());
            assert_eq!(current_owner().as_deref(), Some("telemetry"));
        });
    }

    #[test]
    fn acquire_when_we_already_own_succeeds() {
        with_tmp_lock(|| {
            try_acquire("telemetry").unwrap();
            assert!(try_acquire("telemetry").unwrap(), "self-reacquire should succeed");
        });
    }

    #[test]
    fn acquire_when_other_owner_fresh_fails() {
        with_tmp_lock(|| {
            try_acquire("keep_awake").unwrap();
            assert!(!try_acquire("telemetry").unwrap());
            assert_eq!(current_owner().as_deref(), Some("keep_awake"));
        });
    }

    #[test]
    fn acquire_steals_stale_lock() {
        with_tmp_lock(|| {
            // Write a stale lock from another owner.
            write_lock("keep_awake", now_secs() - STALE_AFTER_SECS - 1).unwrap();
            assert!(try_acquire("telemetry").unwrap(), "stale lock should be stealable");
            assert_eq!(current_owner().as_deref(), Some("telemetry"));
        });
    }

    #[test]
    fn acquire_steals_orphaned_keep_awake_lock() {
        with_tmp_lock(|| {
            // Simulate: archive crashed mid-cycle (or set -e killed it
            // pre-trap). Lock is keep_awake, well past the grace
            // window, but no fresh archive status and no nudge PID
            // (default for an in-memory test harness).
            write_lock(
                "keep_awake",
                now_secs() - KEEP_AWAKE_ORPHAN_GRACE_SECS - 5,
            )
            .unwrap();
            assert!(
                try_acquire("telemetry").unwrap(),
                "orphaned keep_awake lock should be stealable after grace",
            );
            assert_eq!(current_owner().as_deref(), Some("telemetry"));
        });
    }

    #[test]
    fn acquire_respects_grace_window_on_keep_awake_lock() {
        with_tmp_lock(|| {
            // Within the grace window — even with no archive/nudge,
            // don't steal. Avoids a race where archiveloop just ran
            // awake_start and hasn't written archive_status.json yet.
            write_lock("keep_awake", now_secs()).unwrap();
            assert!(
                !try_acquire("telemetry").unwrap(),
                "fresh keep_awake lock should NOT be stolen during grace",
            );
            assert_eq!(current_owner().as_deref(), Some("keep_awake"));
        });
    }

    #[test]
    fn release_removes_when_we_own() {
        with_tmp_lock(|| {
            try_acquire("telemetry").unwrap();
            release("telemetry").unwrap();
            assert!(current_owner().is_none());
        });
    }

    #[test]
    fn release_noop_when_other_owns() {
        with_tmp_lock(|| {
            try_acquire("keep_awake").unwrap();
            release("telemetry").unwrap();
            assert_eq!(current_owner().as_deref(), Some("keep_awake"));
        });
    }

    // Cede-sentinel — serialized on its own mutex since it touches a
    // fixed tmpfs path. A normal (flag-off) install never sets it, so
    // the default must be "do not cede".
    static CEDE_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn cede_sentinel_absent_means_no_cede() {
        let _g = CEDE_LOCK.lock().unwrap();
        let _ = clear_sampler_ceded();
        assert!(
            !sampler_should_cede(),
            "default (no broker) install must not cede sampling",
        );
    }

    #[test]
    fn set_and_clear_cede_sentinel() {
        let _g = CEDE_LOCK.lock().unwrap();
        let _ = clear_sampler_ceded();
        set_sampler_ceded().unwrap();
        assert!(sampler_should_cede(), "after set, standalone sampler cedes");
        clear_sampler_ceded().unwrap();
        assert!(!sampler_should_cede(), "after clear, sampler resumes");
        // Idempotent clear.
        clear_sampler_ceded().unwrap();
    }
}
