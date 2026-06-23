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

/// Wanted-flag written by `drives_handler::register_keep_awake_want` —
/// presence means at least one in-process owner (web UI session,
/// drive-data processor) currently wants the car kept awake. Path
/// must match `KEEP_AWAKE_WANTED_FLAG` in `crates/api/src/drives_handler.rs`.
const WEBUI_WANTED_FLAG: &str = "/tmp/keep_awake_webui_wanted";

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
    try_acquire_at(
        Path::new(LOCK_PATH),
        Path::new(ARCHIVE_STATUS_PATH),
        Path::new(NUDGE_PID_FILE),
        owner,
    )
}

/// Path-parameterized core of [`try_acquire`]. Production goes through
/// the wrapper above; tests pass temp paths so `cargo test` never
/// touches the real `/tmp/ble_radio_owner` — on a live Pi that file is
/// owned by the running daemon, so tests against the real path either
/// fail with EPERM (unprivileged) or, worse, steal the production lock
/// (root).
fn try_acquire_at(
    lock_path: &Path,
    archive_status: &Path,
    nudge_pid: &Path,
    owner: &str,
) -> Result<bool> {
    let now = now_secs();

    if lock_path.exists() {
        match read_lock(lock_path) {
            Ok((existing_owner, ts)) => {
                if existing_owner == owner {
                    // Re-acquire — refresh the timestamp so a long
                    // hold doesn't appear stale.
                    write_lock(lock_path, owner, now)?;
                    return Ok(true);
                }
                let age = now - ts;
                if age > STALE_AFTER_SECS {
                    warn!(
                        "BLE radio lock held by '{}' for {}s — assuming crashed, taking over",
                        existing_owner, age
                    );
                    write_lock(lock_path, owner, now)?;
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
                    && !is_archive_active_at(archive_status)
                    && !is_nudge_alive_at(nudge_pid)
                {
                    warn!(
                        "BLE radio lock owned by keep_awake but no active archive/nudge ({}s old) — orphan, taking over",
                        age
                    );
                    write_lock(lock_path, owner, now)?;
                    return Ok(true);
                }
                return Ok(false);
            }
            Err(e) => {
                warn!("BLE radio lock file unreadable ({}) — overwriting", e);
                write_lock(lock_path, owner, now)?;
                return Ok(true);
            }
        }
    }

    write_lock(lock_path, owner, now)?;
    info!("BLE radio lock acquired by '{}'", owner);
    Ok(true)
}

/// Release the radio lock if we own it. No-op if the file is missing
/// or owned by someone else (some other component may have taken it
/// over due to staleness).
pub fn release(owner: &str) -> Result<()> {
    release_at(Path::new(LOCK_PATH), owner)
}

fn release_at(lock_path: &Path, owner: &str) -> Result<()> {
    if !lock_path.exists() {
        return Ok(());
    }
    match read_lock(lock_path) {
        Ok((existing_owner, _)) if existing_owner == owner => {
            fs::remove_file(lock_path)
                .with_context(|| format!("failed to remove {}", lock_path.display()))?;
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
    current_owner_at(Path::new(LOCK_PATH))
}

fn current_owner_at(lock_path: &Path) -> Option<String> {
    read_lock(lock_path).ok().map(|(owner, _)| owner)
}

fn read_lock(lock_path: &Path) -> Result<(String, i64)> {
    let contents = fs::read_to_string(lock_path)
        .with_context(|| format!("failed to read {}", lock_path.display()))?;
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

fn write_lock(lock_path: &Path, owner: &str, ts: i64) -> Result<()> {
    // Atomic-ish: write to .tmp then rename. Cheap because /tmp is
    // tmpfs.
    let tmp = lock_path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        writeln!(f, "{}", owner)?;
        writeln!(f, "{}", ts)?;
    }
    fs::rename(&tmp, lock_path).with_context(|| {
        format!("failed to rename {} -> {}", tmp.display(), lock_path.display())
    })?;
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
    is_archive_active_at(Path::new(ARCHIVE_STATUS_PATH))
}

fn is_archive_active_at(status_path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(status_path) else {
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
    is_nudge_alive_at(Path::new(NUDGE_PID_FILE))
}

fn is_nudge_alive_at(pid_file: &Path) -> bool {
    let Ok(pid_str) = std::fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };
    Path::new(&format!("/proc/{pid}")).exists()
}

/// True when an in-process owner has registered a keep-awake want via
/// `drives_handler::register_keep_awake_want`. Presence-only — the flag
/// is removed on the 1→0 transition. Bridges the gap when `awake_start`
/// hasn't been spawned (and therefore no nudge PID file exists) but the
/// web UI or drive processor still wants the car awake.
fn is_webui_keep_awake_wanted_at(flag_path: &Path) -> bool {
    flag_path.exists()
}

/// True when something currently wants the car kept awake: an archive
/// cycle (fresh `archive_status.json`), a keep-awake nudge loop
/// (`awake_start`'s Case-3 PID file — archiveloop, web-UI, drive
/// processing), or an in-process keep-awake want (web UI session,
/// drive-data processor, future sampler-delegated keep-awake). The
/// quiet-mode gate consults this so it never green-lights sleep out
/// from under an in-flight archive. All inputs self-clear — the
/// status file goes stale (120s), the PID dies, the wanted-flag is
/// removed on release — so this can't wedge the car permanently
/// awake once the work finishes.
pub fn keep_awake_requested() -> bool {
    is_archive_active()
        || is_nudge_alive()
        || is_webui_keep_awake_wanted_at(Path::new(WEBUI_WANTED_FLAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hermetic per-test path set: everything under a fresh temp dir,
    /// so `cargo test` never touches the real /tmp/ble_radio_owner —
    /// on a live Pi that file belongs to the running daemon (tests
    /// against it fail with EPERM unprivileged, or steal the
    /// production lock as root).
    struct Env {
        _dir: tempfile::TempDir,
        lock: std::path::PathBuf,
        archive: std::path::PathBuf,
        nudge: std::path::PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Env {
                lock: dir.path().join("ble_radio_owner"),
                archive: dir.path().join("archive_status.json"),
                nudge: dir.path().join("keep_awake_nudge_pid"),
                _dir: dir,
            }
        }
        fn acquire(&self, owner: &str) -> bool {
            try_acquire_at(&self.lock, &self.archive, &self.nudge, owner).unwrap()
        }
        fn owner(&self) -> Option<String> {
            current_owner_at(&self.lock)
        }
    }

    #[test]
    fn acquire_when_unheld_succeeds() {
        let env = Env::new();
        assert!(env.acquire("telemetry"));
        assert_eq!(env.owner().as_deref(), Some("telemetry"));
    }

    #[test]
    fn acquire_when_we_already_own_succeeds() {
        let env = Env::new();
        env.acquire("telemetry");
        assert!(env.acquire("telemetry"), "self-reacquire should succeed");
    }

    #[test]
    fn acquire_when_other_owner_fresh_fails() {
        let env = Env::new();
        env.acquire("keep_awake");
        assert!(!env.acquire("telemetry"));
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn acquire_steals_stale_lock() {
        let env = Env::new();
        // Write a stale lock from another owner.
        write_lock(&env.lock, "keep_awake", now_secs() - STALE_AFTER_SECS - 1).unwrap();
        assert!(env.acquire("telemetry"), "stale lock should be stealable");
        assert_eq!(env.owner().as_deref(), Some("telemetry"));
    }

    #[test]
    fn acquire_steals_orphaned_keep_awake_lock() {
        let env = Env::new();
        // Simulate: archive crashed mid-cycle (or set -e killed it
        // pre-trap). Lock is keep_awake, well past the grace window,
        // but no fresh archive status and no nudge PID (both paths
        // absent in the hermetic temp dir).
        write_lock(
            &env.lock,
            "keep_awake",
            now_secs() - KEEP_AWAKE_ORPHAN_GRACE_SECS - 5,
        )
        .unwrap();
        assert!(
            env.acquire("telemetry"),
            "orphaned keep_awake lock should be stealable after grace",
        );
        assert_eq!(env.owner().as_deref(), Some("telemetry"));
    }

    #[test]
    fn acquire_waits_for_fresh_archive_even_past_grace() {
        let env = Env::new();
        // Past the grace window but an archive IS running (fresh
        // status file) — the lock must NOT be stolen.
        write_lock(
            &env.lock,
            "keep_awake",
            now_secs() - KEEP_AWAKE_ORPHAN_GRACE_SECS - 5,
        )
        .unwrap();
        std::fs::write(&env.archive, b"{}").unwrap();
        assert!(
            !env.acquire("telemetry"),
            "keep_awake lock with a live archive must not be stolen",
        );
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn acquire_respects_grace_window_on_keep_awake_lock() {
        let env = Env::new();
        // Within the grace window — even with no archive/nudge,
        // don't steal. Avoids a race where archiveloop just ran
        // awake_start and hasn't written archive_status.json yet.
        write_lock(&env.lock, "keep_awake", now_secs()).unwrap();
        assert!(
            !env.acquire("telemetry"),
            "fresh keep_awake lock should NOT be stolen during grace",
        );
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn release_removes_when_we_own() {
        let env = Env::new();
        env.acquire("telemetry");
        release_at(&env.lock, "telemetry").unwrap();
        assert!(env.owner().is_none());
    }

    #[test]
    fn release_noop_when_other_owns() {
        let env = Env::new();
        env.acquire("keep_awake");
        release_at(&env.lock, "telemetry").unwrap();
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn webui_wanted_flag_presence_detected() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("keep_awake_webui_wanted");
        assert!(!is_webui_keep_awake_wanted_at(&flag));
        std::fs::write(&flag, b"").unwrap();
        assert!(is_webui_keep_awake_wanted_at(&flag));
        std::fs::remove_file(&flag).unwrap();
        assert!(!is_webui_keep_awake_wanted_at(&flag));
    }
}
