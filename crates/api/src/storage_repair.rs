//! In-app XFS `backingfiles` storage repair.
//!
//! Recovers the external-SSD XFS partition behind `/backingfiles` when it
//! won't mount — CRC / dirty-log corruption, typically after a power loss
//! (see `docs/storage-repair-handoff.md` for the incident this mirrors).
//! It reproduces the manual `xfs_repair` recovery a human would run over SSH,
//! but guard-railed for the web UI:
//!
//!   * The real backing device is resolved at runtime (never the hard-coded
//!     `/dev/sda2`), and the repair refuses to touch the root disk.
//!   * It is only offered when `/backingfiles` lives on a *separate external*
//!     drive — `external` in the health response gates the card.
//!   * It NEVER stops the `sentryusb` service: that service is the web server
//!     running this very handler. Only the archive loop and the USB gadget are
//!     quiesced; the UI keeps serving from the (separate) root card.
//!   * The non-destructive path runs automatically (`xfs_repair -n`, plain
//!     `xfs_repair`, and a mount-to-replay-log retry). It HARD STOPS before the
//!     destructive `xfs_repair -L` — that only runs when the user explicitly
//!     re-submits with `confirm_destructive: true`.
//!   * On success it lands in a "reboot required" state. The user reboots to
//!     bring storage + gadget back through the clean boot path (the live
//!     gadget re-enable is the fragile part — a reboot is the reliable fix).

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::router::AppState;

const BACKINGFILES: &str = "/backingfiles";
/// XFS label the setup code stamps on the backingfiles partition
/// (`mkfs.xfs -L backingfiles`); used to find the device when it isn't
/// mounted (the corruption case).
const XFS_LABEL: &str = "backingfiles";
/// Bind/autofs mount points archiveloop exposes from inside `/backingfiles`.
/// All must be released before the device can be unmounted.
const SUBMOUNTS: &[&str] = &["/mnt/cam", "/mnt/music", "/mnt/lightshow", "/mnt/boombox"];
/// Writable partition where the human-readable repair transcript is kept.
const REPAIR_LOG_DIR: &str = "/mutable";
/// Per-command ceiling. `xfs_repair` on a large multi-TB SSD can take minutes;
/// 5 min matches the wizard's xfs_repair budget.
const CMD_TIMEOUT: Duration = Duration::from_secs(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

// ───────────────────────── command capture ─────────────────────────

/// Outcome of a spawned command with its combined output preserved
/// regardless of exit status.
struct CmdResult {
    ok: bool,
    code: i32,
    output: String,
}

/// Run a command capturing exit status + combined stdout/stderr, never
/// erroring on a non-zero exit.
///
/// `sentryusb_shell::run` bails on any non-zero exit — unusable here because
/// `xfs_repair -n` exits non-zero *precisely when it finds damage*, which is
/// exactly the signal we need to read.
async fn run_capture(timeout: Duration, name: &str, args: &[&str]) -> CmdResult {
    let fut = Command::new(name).args(args).kill_on_drop(true).output();
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => CmdResult {
            ok: false,
            code: -1,
            output: format!("(timed out after {timeout:?})"),
        },
        Ok(Err(e)) => CmdResult {
            ok: false,
            code: -1,
            output: format!("(failed to spawn {name}: {e})"),
        },
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&err);
            }
            CmdResult {
                ok: o.status.success(),
                code: o.status.code().unwrap_or(-1),
                output: s.trim_end().to_string(),
            }
        }
    }
}

// ───────────────────────── pure helpers (unit-tested) ─────────────────────────

/// Strip the partition suffix from a `/dev` name to get the parent disk.
/// `sda2` → `sda`, `mmcblk0p2` → `mmcblk0`, `nvme0n1p3` → `nvme0n1`.
/// Mirrors the parent-disk logic in [`crate::devices`].
fn parent_disk(dev: &str) -> String {
    let d = dev.strip_prefix("/dev/").unwrap_or(dev);
    if d.contains("mmcblk") || d.contains("nvme") || d.contains("loop") {
        // p-separated partition suffix, e.g. mmcblk0p2 / nvme0n1p3.
        if let Some(idx) = d.rfind('p') {
            let suffix = &d[idx + 1..];
            if idx > 0 && !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                return d[..idx].to_string();
            }
        }
        d.to_string()
    } else {
        // sd-style: partition suffix is trailing digits.
        let trimmed: String = d
            .chars()
            .rev()
            .skip_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if trimmed.is_empty() { d.to_string() } else { trimmed }
    }
}

/// Find the source device for a mountpoint in `/proc/mounts` text.
fn resolve_mount_source(mounts: &str, mountpoint: &str) -> Option<String> {
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(src), Some(mp)) = (f.next(), f.next()) else {
            continue;
        };
        if mp == mountpoint && src.starts_with("/dev/") {
            return Some(src.to_string());
        }
    }
    None
}

/// True when `xfs_repair` output says the dirty log must be replayed (or
/// destroyed with `-L`). These phrases are stable across xfs_repair versions.
fn needs_log_replay(out: &str) -> bool {
    let l = out.to_ascii_lowercase();
    l.contains("destroy the log") || l.contains("metadata changes in a log")
}

// ───────────────────────── runtime resolution ─────────────────────────

fn canonicalize_dev(src: &str) -> String {
    std::fs::canonicalize(src)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| src.to_string())
}

async fn read_proc_mounts() -> String {
    tokio::fs::read_to_string("/proc/mounts").await.unwrap_or_default()
}

/// Parent disk of the root filesystem (`/`), e.g. `mmcblk0`.
async fn root_disk() -> Option<String> {
    let mounts = read_proc_mounts().await;
    let src = resolve_mount_source(&mounts, "/")?;
    Some(parent_disk(&canonicalize_dev(&src)))
}

/// Resolve the device backing `/backingfiles`. Prefers the live mount; falls
/// back to the XFS label when the partition is corrupt and unmounted.
async fn resolve_backing_device() -> Option<String> {
    let mounts = read_proc_mounts().await;
    if let Some(src) = resolve_mount_source(&mounts, BACKINGFILES) {
        return Some(canonicalize_dev(&src));
    }
    // Not mounted (the corruption case): resolve by label from the superblock.
    let r = run_capture(PROBE_TIMEOUT, "blkid", &["-L", XFS_LABEL]).await;
    let dev = r.output.trim();
    if r.ok && dev.starts_with("/dev/") {
        return Some(canonicalize_dev(dev));
    }
    // Fallback: lsblk label scan.
    let r = run_capture(PROBE_TIMEOUT, "lsblk", &["-rno", "PATH,LABEL"]).await;
    for line in r.output.lines() {
        let mut f = line.split_whitespace();
        if let (Some(path), Some(label)) = (f.next(), f.next()) {
            if label == XFS_LABEL && path.starts_with("/dev/") {
                return Some(path.to_string());
            }
        }
    }
    None
}

async fn device_fstype(dev: &str) -> Option<String> {
    let r = run_capture(PROBE_TIMEOUT, "lsblk", &["-rno", "FSTYPE", dev]).await;
    let t = r.output.lines().next().unwrap_or("").trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// `/backingfiles` lives on a different physical disk than root and isn't the
/// onboard SD slot — the precondition for offering repair.
async fn is_external(dev: &str) -> bool {
    let bp = parent_disk(dev);
    if bp == "mmcblk0" {
        return false;
    }
    match root_disk().await {
        Some(rp) => bp != rp,
        None => true,
    }
}

/// True for a genuine XFS *error* line for this device, excluding the benign
/// informational lines XFS prints on every mount/unmount ("Mounting V5
/// Filesystem", "Ending clean mount"). The first cut flagged any line that
/// merely mentioned the device, so a healthy remount after a repair looked
/// like two fresh "errors". An error line must carry a real error keyword.
fn is_xfs_error_line(line: &str, devbase: &str) -> bool {
    let l = line.to_ascii_lowercase();
    if !l.contains("xfs") || !l.contains(devbase) {
        return false;
    }
    // Normal lifecycle chatter — never an error.
    if l.contains("mounting")
        || l.contains("unmounting")
        || l.contains("ending clean mount")
        || l.contains("ending clean unmount")
    {
        return false;
    }
    l.contains("error")
        || l.contains("corrupt")
        || l.contains("shut down")
        || l.contains("shutdown")
        || l.contains("i/o error")
        || l.contains("log recovery")
        || l.contains("inconsistent")
        || l.contains("needs repair")
        || l.contains("metadata corruption")
}

/// Recent genuine XFS error lines for the device, newest last.
async fn recent_xfs_errors(dev: &str) -> Vec<String> {
    let r = run_capture(PROBE_TIMEOUT, "dmesg", &["--ctime"]).await;
    let devbase = dev.strip_prefix("/dev/").unwrap_or(dev).to_ascii_lowercase();
    let mut out: Vec<String> = r
        .output
        .lines()
        .filter(|l| is_xfs_error_line(l, &devbase))
        .map(|s| s.trim().to_string())
        .collect();
    let len = out.len();
    if len > 12 {
        out = out.split_off(len - 12);
    }
    out
}

fn cam_disk_present() -> bool {
    std::path::Path::new(&format!("{BACKINGFILES}/cam_disk.bin")).exists()
}

// ───────────────────────── persisted transcript ─────────────────────────

fn persist_log(buf: &str) -> Option<String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("storage_repair_{ts}.log");
    let path = format!("{REPAIR_LOG_DIR}/{name}");
    match std::fs::write(&path, buf) {
        Ok(_) => Some(name),
        Err(e) => {
            tracing::warn!("failed to write repair log {path}: {e}");
            None
        }
    }
}

fn latest_repair_log() -> Option<String> {
    let rd = std::fs::read_dir(REPAIR_LOG_DIR).ok()?;
    let mut best: Option<(u64, String)> = None;
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Some(ts) = n
            .strip_prefix("storage_repair_")
            .and_then(|x| x.strip_suffix(".log"))
            .and_then(|x| x.parse::<u64>().ok())
        {
            if best.as_ref().map_or(true, |(b, _)| ts > *b) {
                best = Some((ts, n));
            }
        }
    }
    best.map(|(_, n)| n)
}

// ───────────────────────── GET /api/storage/health ─────────────────────────

#[derive(Serialize)]
struct StorageHealth {
    /// `healthy` | `unmounted` | `corrupt` | `missing_images` | `no_external`
    state: String,
    /// Whether `/backingfiles` is on a separate external drive (gates the UI).
    external: bool,
    device: Option<String>,
    fstype: Option<String>,
    mounted: bool,
    mountpoint: String,
    cam_disk_present: bool,
    /// Recent XFS kernel errors mentioning the device, newest last.
    dmesg_errors: Vec<String>,
    /// Filename of the most recent persisted repair transcript, if any.
    last_repair_log: Option<String>,
}

/// GET /api/storage/health
pub async fn storage_health(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let device = resolve_backing_device().await;
    let mounts = read_proc_mounts().await;
    let mounted = resolve_mount_source(&mounts, BACKINGFILES).is_some();
    let external = match &device {
        Some(d) => is_external(d).await,
        None => false,
    };
    let fstype = match &device {
        Some(d) => device_fstype(d).await,
        None => None,
    };
    let cam = cam_disk_present();
    let dmesg_errors = if external {
        match &device {
            Some(d) => recent_xfs_errors(d).await,
            None => vec![],
        }
    } else {
        vec![]
    };

    let state = if !external {
        "no_external"
    } else if mounted {
        if cam { "healthy" } else { "missing_images" }
    } else if !dmesg_errors.is_empty() {
        "corrupt"
    } else {
        "unmounted"
    };

    let health = StorageHealth {
        state: state.to_string(),
        external,
        device,
        fstype,
        mounted,
        mountpoint: BACKINGFILES.to_string(),
        cam_disk_present: cam,
        dmesg_errors,
        last_repair_log: latest_repair_log(),
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(health).unwrap_or_else(|_| serde_json::json!({}))),
    )
}

// ───────────────────────── POST /api/storage/repair ─────────────────────────

#[derive(Deserialize, Default)]
struct RepairRequest {
    /// Authorizes the destructive `xfs_repair -L` last resort. Without it the
    /// flow hard-stops at the escalation gate and broadcasts `needs_force`.
    #[serde(default)]
    confirm_destructive: bool,
}

/// Broadcasts each repair step over the `storage_repair` WS channel and
/// accumulates the full transcript for persistence.
struct RepairLog {
    hub: sentryusb_ws::Hub,
    buf: String,
}

impl RepairLog {
    fn line(&mut self, phase: &str, line: impl Into<String>) {
        let line = line.into();
        self.buf.push_str(&line);
        self.buf.push('\n');
        self.hub.broadcast(
            "storage_repair",
            &serde_json::json!({ "status": "running", "phase": phase, "line": line }),
        );
    }

    fn cmd(&mut self, phase: &str, label: &str, r: &CmdResult) {
        self.line(phase, format!("$ {label}"));
        for l in r.output.lines() {
            self.line(phase, format!("  {l}"));
        }
        self.line(phase, format!("  → exit {}", r.code));
    }
}

/// POST /api/storage/repair
///
/// Validates synchronously (so the caller gets an immediate 4xx on a bad
/// precondition) then spawns the repair, streaming progress over WS. Returns
/// `{ "status": "started" }` on a valid request.
pub async fn storage_repair(State(s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    let req: RepairRequest = serde_json::from_str(&body).unwrap_or_default();

    let device = match resolve_backing_device().await {
        Some(d) => d,
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "Could not find the camera storage device (no /backingfiles mount and no 'backingfiles'-labelled partition).",
            )
        }
    };
    if !is_external(&device).await {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "Storage repair is only available when camera storage is on a separate external drive.",
        );
    }
    // Belt-and-suspenders: never aim the repair at the root disk.
    if let Some(rp) = root_disk().await {
        if parent_disk(&device) == rp {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "Refusing to repair: the resolved device is the system/root disk.",
            );
        }
    }
    if let Some(fs) = device_fstype(&device).await {
        if fs != "xfs" {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                &format!("Storage repair currently supports XFS only (found '{fs}')."),
            );
        }
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        run_repair(hub, device, req.confirm_destructive).await;
    });

    (StatusCode::OK, Json(serde_json::json!({ "status": "started" })))
}

async fn run_repair(hub: sentryusb_ws::Hub, device: String, confirm_destructive: bool) {
    let mut log = RepairLog { hub: hub.clone(), buf: String::new() };
    let t = CMD_TIMEOUT;

    log.line(
        "preflight",
        format!("Repairing {device} (XFS backingfiles); confirm_destructive={confirm_destructive}"),
    );

    // ── 1. Quiesce — NEVER stop the `sentryusb` service (it's us). ──
    log.line("quiesce", "Stopping the archive loop and USB gadget (the web UI stays up)…");
    let _ = run_capture(t, "systemctl", &["stop", "sentryusb-archive"]).await;
    let _ = run_capture(t, "bash", &["-c", "killall archiveloop 2>/dev/null || true"]).await;
    match tokio::task::spawn_blocking(sentryusb_gadget::disable).await {
        Ok(Ok(())) => log.line("quiesce", "USB gadget disabled."),
        Ok(Err(e)) => log.line("quiesce", format!("Gadget disable warning (continuing): {e}")),
        Err(e) => log.line("quiesce", format!("Gadget disable task error (continuing): {e}")),
    }

    // ── 2. Release mounts so xfs_repair won't refuse on a busy device. ──
    log.line("unmount", "Releasing mounts…");
    let mut mps: Vec<&str> = SUBMOUNTS.to_vec();
    mps.push(BACKINGFILES);
    for mp in mps {
        let r = run_capture(t, "umount", &[mp]).await;
        if !r.ok && !r.output.contains("not mounted") && !r.output.contains("not found") {
            log.cmd("unmount", &format!("umount {mp}"), &r);
        }
    }
    let r = run_capture(t, "umount", &[device.as_str()]).await;
    if !r.ok && !r.output.contains("not mounted") {
        log.cmd("unmount", &format!("umount {device}"), &r);
    }

    // ── 3. Read-only diagnosis. ──
    log.line("dryrun", "Running read-only check (xfs_repair -n)…");
    let dry = run_capture(t, "xfs_repair", &["-n", &device]).await;
    log.cmd("dryrun", &format!("xfs_repair -n {device}"), &dry);

    // ── 4. Non-destructive repair, with a mount-to-replay-log retry. ──
    log.line("repair", "Attempting repair (xfs_repair)…");
    let mut rep = run_capture(t, "xfs_repair", &[&device]).await;
    log.cmd("repair", &format!("xfs_repair {device}"), &rep);

    if !rep.ok && needs_log_replay(&rep.output) {
        log.line("repair", "Dirty log detected — mounting to replay it, then retrying…");
        let m = run_capture(t, "mount", &[&device, BACKINGFILES]).await;
        log.cmd("repair", &format!("mount {device} {BACKINGFILES}"), &m);
        if m.ok {
            let u = run_capture(t, "umount", &[BACKINGFILES]).await;
            log.cmd("repair", &format!("umount {BACKINGFILES}"), &u);
            rep = run_capture(t, "xfs_repair", &[&device]).await;
            log.cmd("repair", &format!("xfs_repair {device} (after log replay)"), &rep);
        } else {
            log.line("repair", "Mount failed — the log cannot be replayed this way.");
        }
    }

    // ── 5. Escalation gate: destructive -L only with explicit confirmation. ──
    if !rep.ok && needs_log_replay(&rep.output) {
        if !confirm_destructive {
            let log_file = persist_log(&log.buf);
            hub.broadcast(
                "storage_repair",
                &serde_json::json!({
                    "status": "needs_force",
                    "device": device,
                    "log_file": log_file,
                    "message": "The filesystem log is damaged and can't be replayed. The only repair left destroys the pending XFS log (xfs_repair -L), which may lose the most recently written metadata — typically a few of the newest clips. Confirm to proceed.",
                }),
            );
            return;
        }
        log.line("repair", "Confirmed — clearing the XFS log (xfs_repair -L)…");
        rep = run_capture(t, "xfs_repair", &["-L", &device]).await;
        log.cmd("repair", &format!("xfs_repair -L {device}"), &rep);
    }

    if !rep.ok {
        let log_file = persist_log(&log.buf);
        hub.broadcast(
            "storage_repair",
            &serde_json::json!({
                "status": "error",
                "device": device,
                "log_file": log_file,
                "error": "xfs_repair could not repair the filesystem. Review the log — the drive itself may be failing (check the SSD's power, cable and enclosure).",
            }),
        );
        return;
    }

    // ── 6. Read-only verify, then unmount so the reboot mounts cleanly. ──
    log.line("verify", "Repair succeeded — verifying contents…");
    let mut cam_present = false;
    let mut lost_found = 0usize;
    let m = run_capture(t, "mount", &[&device, BACKINGFILES]).await;
    if m.ok {
        cam_present = cam_disk_present();
        if let Ok(rd) = std::fs::read_dir(format!("{BACKINGFILES}/lost+found")) {
            lost_found = rd.flatten().count();
        }
        log.line(
            "verify",
            format!("cam_disk.bin present: {cam_present}; lost+found entries: {lost_found}"),
        );
        let _ = run_capture(t, "umount", &[BACKINGFILES]).await;
    } else {
        log.cmd("verify", &format!("mount {device} {BACKINGFILES}"), &m);
        log.line("verify", "Could not mount after repair to verify — the reboot will retry the mount.");
    }

    // ── 7. Reboot-required terminal state (user initiates the reboot). ──
    let log_file = persist_log(&log.buf);
    let message = if cam_present {
        "Repair complete. A reboot is required to bring camera storage back online.".to_string()
    } else {
        "Repair complete, but cam_disk.bin is missing. After rebooting you'll need to recreate the backing files by re-running the Setup Wizard. A reboot is required first.".to_string()
    };
    hub.broadcast(
        "storage_repair",
        &serde_json::json!({
            "status": "reboot_required",
            "device": device,
            "cam_disk_present": cam_present,
            "lost_found_count": lost_found,
            "log_file": log_file,
            "message": message,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_disk_strips_partition_suffix() {
        assert_eq!(parent_disk("/dev/sda2"), "sda");
        assert_eq!(parent_disk("sda2"), "sda");
        assert_eq!(parent_disk("/dev/sda"), "sda");
        assert_eq!(parent_disk("mmcblk0p2"), "mmcblk0");
        assert_eq!(parent_disk("/dev/mmcblk0p2"), "mmcblk0");
        assert_eq!(parent_disk("mmcblk0"), "mmcblk0");
        assert_eq!(parent_disk("nvme0n1p3"), "nvme0n1");
        assert_eq!(parent_disk("nvme0n1"), "nvme0n1");
    }

    #[test]
    fn resolve_mount_source_finds_backingfiles() {
        let mounts = "\
sysfs /sys sysfs rw 0 0
/dev/mmcblk0p2 / ext4 rw,relatime 0 0
/dev/sda1 /mutable ext4 rw 0 0
/dev/sda2 /backingfiles xfs rw,noatime 0 0
tmpfs /run tmpfs rw 0 0";
        assert_eq!(resolve_mount_source(mounts, "/backingfiles").as_deref(), Some("/dev/sda2"));
        assert_eq!(resolve_mount_source(mounts, "/").as_deref(), Some("/dev/mmcblk0p2"));
        assert_eq!(resolve_mount_source(mounts, "/mutable").as_deref(), Some("/dev/sda1"));
        assert_eq!(resolve_mount_source(mounts, "/nope"), None);
    }

    #[test]
    fn resolve_mount_source_ignores_short_lines() {
        // A malformed/short line must not panic or mis-resolve.
        assert_eq!(resolve_mount_source("garbage\n/dev/sda2 /backingfiles xfs rw 0 0", "/backingfiles").as_deref(), Some("/dev/sda2"));
    }

    #[test]
    fn xfs_error_filter_ignores_benign_mount_lines() {
        let dev = "sda2";
        // Benign lifecycle chatter — the lines a healthy remount prints.
        // These were wrongly flagged as "2 recent filesystem errors".
        assert!(!is_xfs_error_line(
            "[Sun Jun 14 05:34:14 2026] XFS (sda2): Mounting V5 Filesystem b1a5fe90",
            dev
        ));
        assert!(!is_xfs_error_line(
            "[Sun Jun 14 05:34:14 2026] XFS (sda2): Ending clean mount",
            dev
        ));
        // Real errors from the actual incident MUST still be flagged.
        assert!(is_xfs_error_line("XFS (sda2): Metadata CRC error detected", dev));
        assert!(is_xfs_error_line(
            "XFS (sda2): Filesystem has been shut down due to log error (0x2).",
            dev
        ));
        assert!(is_xfs_error_line("XFS (sda2): log mount/recovery failed: error -74", dev));
        // A different volume's noise is ignored.
        assert!(!is_xfs_error_line("XFS (sdb1): Metadata CRC error detected", dev));
        // Non-XFS lines are ignored.
        assert!(!is_xfs_error_line("EXT4-fs (sda1): error count", dev));
    }

    #[test]
    fn needs_log_replay_matches_xfs_repair_phrases() {
        // The exact ERROR xfs_repair prints when the log must be replayed.
        let replay = "ERROR: The filesystem has valuable metadata changes in a log which needs to\nbe replayed. Mount the filesystem to replay the log, and unmount it before\nre-running xfs_repair. If you are unable to mount the filesystem, then use\nthe -L option to destroy the log and attempt a repair.";
        assert!(needs_log_replay(replay));
        // A clean repair run must not trip the gate.
        let clean = "Phase 1 - find and verify superblock...\nPhase 7 - verify and correct link counts...\ndone";
        assert!(!needs_log_replay(clean));
    }
}
