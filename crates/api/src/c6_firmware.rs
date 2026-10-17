//! ESP32-C6 firmware updater.
//!
//! The co-processor runs its own firmware (reported as `fw` by the supervisor
//! on 8787). A newer build can be staged on the Pi as an *available* candidate
//! that the user installs on demand — the same "offer, then install" model as
//! the Wi-Fi firmware updater, surfaced in the ESP32-C6 row in Settings.
//!
//! The candidate lives in its own directory (`available/`) the auto-flasher
//! never reads, so staging an update does NOT auto-apply it. Installing copies
//! the candidate into the flash bundle and drives `c6-flash.sh` (the version-
//! gated auto-flasher: app-only write, NVS/key preserved, snapshot/verify/
//! revert). The web API stays up through the flash — only the *supervisor*
//! drops the serial port — so the UI polls progress continuously, no outage.
//!
//! The flash bundle (`firmware.bin` + `VERSION`) is always kept equal to what
//! is actually on the chip, so a boot-time auto-flash is a no-op: on a failed
//! install the pre-update bundle is restored from `rollback/`, so a mismatch
//! can never linger and trigger an unwanted reflash.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::router::AppState;

/// The flash bundle the auto-flasher (`c6-flash.sh`) reads.
const INSTALL_DIR: &str = "/opt/sentryusb/c6";
/// Staged candidate update — the auto-flasher never looks here.
const AVAILABLE_DIR: &str = "/opt/sentryusb/c6/available";
/// Pre-update bundle, captured at install time = the user's rollback target.
const ROLLBACK_DIR: &str = "/opt/sentryusb/c6/rollback";
/// Install progress. On tmpfs so it is always writable (some boards run with a
/// read-only /mutable) and because the flow completes within one UI session —
/// it never needs to survive a reboot like the Wi-Fi updater's state does.
const STATE_FILE: &str = "/run/sentryusb-c6/fwupdate.json";
const STATE_DIR: &str = "/run/sentryusb-c6";

const C6_DEVICE: &str = "/dev/sentryusb-c6";
const C6_SUPERVISOR_API: &str = "http://127.0.0.1:8787";
const C6_SUPERVISOR_SERVICE: &str = "sentryusb-c6-supervisor";
const C6_FLASH_SERVICE: &str = "sentryusb-c6-flash.service";

/// Guards a second flash starting while one is in flight — they would fight
/// over the serial port and the bundle on disk. Held briefly by the background
/// reconcile probe, and for the whole duration of a user flash.
static INSTALL_RUNNING: AtomicBool = AtomicBool::new(false);

/// True only while the current INSTALL_RUNNING holder is a USER op. Lets a second
/// user click fail fast (409) instead of waiting out the acquire window for a
/// flash that will not release soon; reconcile never sets it.
static USER_HOLDS_LOCK: AtomicBool = AtomicBool::new(false);

/// Count of user ops currently waiting for OR holding the lock. reconcile yields
/// whenever this is > 0 so its ~7s probe never starves a user click. Kept correct
/// across request cancellation by `PendingGuard`.
static USER_OP_WAITERS: AtomicUsize = AtomicUsize::new(0);

/// Clears the lock (and the user-holder flag) on drop so a panic — or a dropped
/// spawn task — can't leave INSTALL_RUNNING stuck true (which would 409 every
/// later request AND make reconcile refuse to bring the supervisor back).
struct RunGuard;
impl Drop for RunGuard {
    fn drop(&mut self) {
        USER_HOLDS_LOCK.store(false, Ordering::SeqCst);
        INSTALL_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Counts the caller as a waiter for the whole time it is trying to acquire the
/// lock. Drop runs even when the HTTP request future is cancelled mid-wait (tab
/// close / reload / network drop), so the waiter count — and thus reconcile's
/// yield — can never leak true.
struct PendingGuard;
impl PendingGuard {
    fn new() -> Self {
        USER_OP_WAITERS.fetch_add(1, Ordering::SeqCst);
        PendingGuard
    }
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        USER_OP_WAITERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How long a user op waits for an in-flight reconcile probe to release the lock
/// before giving up. The probe returns within ~7s on a silent chip; 15s leaves
/// margin without hanging the POST.
const USER_OP_ACQUIRE_WAIT: Duration = Duration::from_secs(15);

/// Claim the flash lock for a user-initiated op, with priority over reconcile.
/// Counts the caller as a waiter (reconcile yields), fails fast if another USER
/// op already holds the lock, then polls until an in-flight reconcile probe
/// releases it. Returns the owning `RunGuard` on success — move it into the flash
/// task so the lock is held for the whole op; None means a user flash is in
/// flight or the window elapsed (→ 409). Cancel-safe: the PendingGuard covers the
/// wait, and on success the RunGuard owns release.
async fn claim_for_user_op() -> Option<RunGuard> {
    let _waiter = PendingGuard::new();
    let deadline = Instant::now() + USER_OP_ACQUIRE_WAIT;
    loop {
        // A user flash already owns the lock: don't wait it out, 409 now.
        if USER_HOLDS_LOCK.load(Ordering::SeqCst) {
            return None;
        }
        if !INSTALL_RUNNING.swap(true, Ordering::SeqCst) {
            // No await between here and returning the guard → no cancellation
            // window can leave INSTALL_RUNNING set without an owning RunGuard.
            USER_HOLDS_LOCK.store(true, Ordering::SeqCst);
            return Some(RunGuard);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// The operation currently in flight ("install" | "rollback" | ""), so the UI can
/// frame a status it didn't itself start (e.g. opened in another tab) correctly.
static CURRENT_OP: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
fn set_op(op: &str) {
    if let Ok(mut g) = CURRENT_OP.lock() {
        *g = op.to_string();
    }
}
fn current_op() -> String {
    CURRENT_OP.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Durable, torn-write-proof file write: temp in the same dir -> fsync -> rename.
/// A half-written VERSION/state file would make the flasher fall back to the
/// baked-in app-descriptor version and reflash on the next supervisor start.
fn write_atomic(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = format!("{path}.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = Path::new(path).parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstallState {
    /// idle | running | success | failed | rolled_back
    pub state: String,
    pub step: String,
    pub progress: u8,
    pub message: String,
    pub updated_at: i64,
    /// "install" | "rollback" | "" — which operation this progress belongs to.
    #[serde(default)]
    pub op: String,
}

impl Default for InstallState {
    fn default() -> Self {
        InstallState {
            state: "idle".into(),
            step: String::new(),
            progress: 0,
            message: String::new(),
            updated_at: 0,
            op: String::new(),
        }
    }
}

fn read_state() -> InstallState {
    std::fs::read_to_string(STATE_FILE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist progress and mirror it to any WebSocket client so the bar moves live.
fn set_state(hub: &sentryusb_ws::Hub, state: &str, step: &str, progress: u8, message: &str) {
    let s = InstallState {
        state: state.into(),
        step: step.into(),
        progress,
        message: message.into(),
        updated_at: chrono::Utc::now().timestamp(),
        op: current_op(),
    };
    let _ = std::fs::create_dir_all(STATE_DIR);
    if let Ok(json) = serde_json::to_string(&s) {
        let _ = write_atomic(STATE_FILE, &json);
    }
    hub.broadcast("c6_firmware_status", &s);
    info!("[c6-fw] {step} {progress}% {message}");
}

/// Is the C6 plugged in right now?
fn c6_present() -> bool {
    Path::new(C6_DEVICE).exists()
}

/// Trim a VERSION file to its version string. Absent/blank => None.
fn read_version(dir: &str) -> Option<String> {
    std::fs::read_to_string(format!("{dir}/VERSION"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Version baked into an app image's esp_app_desc_t (magic @0x20, version[32] @0x30).
fn baked_app_version(path: &str) -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 0x60];
    std::fs::File::open(path).ok()?.read_exact(&mut buf).ok()?;
    if u32::from_le_bytes([buf[0x20], buf[0x21], buf[0x22], buf[0x23]]) != 0xABCD_5432 {
        return None;
    }
    let v = &buf[0x30..0x50];
    let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
    std::str::from_utf8(&v[..end])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// True when firmware.bin in `dir` is actually built as `ver`.
fn bundle_bin_is(dir: &str, ver: &str) -> bool {
    baked_app_version(&format!("{dir}/firmware.bin")).as_deref() == Some(ver)
}

/// The firmware the chip is actually running, from the supervisor's live status
/// (authoritative — the flash probe can only confirm "alive"). None if the
/// supervisor is unreachable or hasn't heard a version yet.
async fn running_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let v = client
        .get(format!("{C6_SUPERVISOR_API}/api/esp32c6/status"))
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()?;
    v.get("fw")
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Is the chip responding RIGHT NOW — not just "the supervisor has a cached fw"?
/// The supervisor keeps serving the last `fw` even after the chip's heartbeat goes
/// stale, so `fw != null` alone can pass for a dead chip. If we then flash a chip
/// that is actually silent, the flasher's probe sees no response and takes its
/// FULL (factory) path, which rewrites NVS and loses the Tesla key. So require a
/// live fw AND healthy AND a recent heartbeat before allowing any flash.
async fn chip_responsive() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Some(v) = client
        .get(format!("{C6_SUPERVISOR_API}/api/esp32c6/status"))
        .send()
        .await
        .ok()
        .and_then(|r| if r.status().is_success() { Some(r) } else { None })
    else {
        return false;
    };
    let Ok(v) = v.json::<serde_json::Value>().await else {
        return false;
    };
    let has_fw = v
        .get("fw")
        .and_then(|s| s.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let healthy = v.get("healthy").and_then(|b| b.as_bool()).unwrap_or(false);
    // Heartbeat within the last 20s (the chip beats ~every 5s). A missing/negative
    // age counts as not-fresh.
    let hb_fresh = v
        .get("last_heartbeat_age_ms")
        .and_then(|n| n.as_i64())
        .map(|age| (0..20_000).contains(&age))
        .unwrap_or(false);
    has_fw && healthy && hb_fresh
}

/// A candidate counts as available only when its bins are actually present, so
/// a stray VERSION file can't offer an update that can't be installed.
fn candidate_ready() -> Option<String> {
    let v = read_version(AVAILABLE_DIR)?;
    if Path::new(&format!("{AVAILABLE_DIR}/firmware.bin")).is_file() {
        Some(v)
    } else {
        None
    }
}

fn rollback_ready() -> Option<String> {
    let v = read_version(ROLLBACK_DIR)?;
    if Path::new(&format!("{ROLLBACK_DIR}/firmware.bin")).is_file() {
        Some(v)
    } else {
        None
    }
}

async fn remount_rw() {
    let _ = sentryusb_shell::run("bash", &["-c", "/root/bin/remountfs_rw"]).await;
}

/// Copy the three-file bundle (app + factory + VERSION) from `src` to `dst`.
/// Atomic: every file is staged as a temp and only renamed in once all copies
/// succeed, so a mid-copy failure (e.g. ENOSPC) leaves the existing bundle
/// intact instead of a truncated firmware.bin. VERSION is renamed LAST so it
/// never leads firmware.bin. `factory` is optional.
fn copy_bundle(src: &str, dst: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let app_tmp = format!("{dst}/firmware.bin.tmp");
    let ver_tmp = format!("{dst}/VERSION.tmp");
    let fac_src = format!("{src}/firmware.factory.bin");
    let fac_tmp = format!("{dst}/firmware.factory.bin.tmp");
    let have_fac = Path::new(&fac_src).is_file();
    // Stage temps; bail before any rename if a copy fails.
    std::fs::copy(format!("{src}/firmware.bin"), &app_tmp)?;
    std::fs::copy(format!("{src}/VERSION"), &ver_tmp)?;
    if have_fac {
        std::fs::copy(&fac_src, &fac_tmp)?;
    }
    let mut syncs = vec![app_tmp.clone(), ver_tmp.clone()];
    if have_fac {
        syncs.push(fac_tmp.clone());
    }
    for t in &syncs {
        if let Ok(f) = std::fs::File::open(t) {
            let _ = f.sync_all();
        }
    }
    // Commit. When src has no factory image, KEEP the existing dst one (deleting it
    // would strip the recovery image an app-only candidate still needs).
    std::fs::rename(&app_tmp, format!("{dst}/firmware.bin"))?;
    if have_fac {
        std::fs::rename(&fac_tmp, format!("{dst}/firmware.factory.bin"))?;
    }
    std::fs::rename(&ver_tmp, format!("{dst}/VERSION"))?;
    Ok(())
}

/// Read the chip's fw DIRECTLY over the serial port (the supervisor must be
/// stopped so the port is free). Used by reconcile to learn the chip's real
/// version while the supervisor is down — the 8787 API isn't available then.
/// Returns None if the chip is silent or not our firmware.
async fn probe_chip_fw() -> Option<String> {
    // RESET_S=0: never pulse the reset here (it can wedge the chip).
    let out = sentryusb_shell::run_with_timeout(
        Duration::from_secs(20),
        "env",
        &[
            "C6_PROBE_RESET_S=0",
            "C6_PROBE_PASSIVE_S=6",
            "python3",
            "/opt/sentryusb/c6/c6-probe.py",
            C6_DEVICE,
        ],
    )
    .await
    .ok()?;
    // The probe prints one JSON result line, e.g. {"result":"hello","fw":"0.1.30",…}.
    let line = out.lines().rev().find(|l| l.contains('{'))?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    v.get("fw")
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Is the unit NOT fully stopped? A `Type=oneshot` without RemainAfterExit is
/// `ActiveState=activating` (never `active`) WHILE its ExecStart runs, so a plain
/// `== "active"` check reports a live flash as idle — and reconcile would then
/// probe the serial port mid-write. Treat activating/deactivating as busy too.
async fn service_busy(svc: &str) -> bool {
    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(5),
        "systemctl",
        &["show", "-p", "ActiveState", "--value", svc],
    )
    .await
    {
        // Fail CLOSED: an empty/"unknown"/error state means we can't prove the port
        // is free, so treat it as busy rather than risk racing a live flash.
        Ok(o) => !matches!(o.trim(), "inactive" | "failed"),
        Err(_) => true,
    }
}

/// Recover from an interrupted flash. The install/rollback orchestration stops
/// the supervisor, flashes, then restarts it — but if the API process dies
/// between those steps (crash, restart, OTA/redeploy mid-flash) the restart
/// never runs and the C6 link is left dead. Runs on the periodic C6 timer and
/// restarts the supervisor ONLY when it is safe to: the C6 is present, no flash
/// is running in this process, the flasher service is not active (a live flash
/// legitimately holds the supervisor down), and the supervisor is actually down.
/// Make the bundle match `chip`; pin VERSION only when firmware.bin IS that version.
/// Returns true only when the bundle genuinely matches the chip.
async fn reconcile_bundle_to(chip: &str) -> bool {
    remount_rw().await;
    let src = if rollback_ready().as_deref() == Some(chip) && bundle_bin_is(ROLLBACK_DIR, chip) {
        Some(ROLLBACK_DIR)
    } else if candidate_ready().as_deref() == Some(chip) && bundle_bin_is(AVAILABLE_DIR, chip) {
        Some(AVAILABLE_DIR)
    } else {
        None
    };
    if let Some(src) = src {
        if let Err(e) = copy_bundle(src, INSTALL_DIR) {
            warn!("[c6-fw] reconcile: restore of {chip} bundle failed ({e})");
            return false;
        }
    }
    if !bundle_bin_is(INSTALL_DIR, chip) {
        warn!("[c6-fw] reconcile: firmware.bin != chip {chip}; not pinning VERSION");
        return false;
    }
    match write_atomic(&format!("{INSTALL_DIR}/VERSION"), &format!("{chip}\n")) {
        Ok(()) => true,
        Err(_) => {
            warn!("[c6-fw] reconcile: could not pin bundle VERSION to {chip}");
            false
        }
    }
}

pub async fn reconcile_supervisor() {
    // Passive checks FIRST — do not claim INSTALL_RUNNING during routine idle
    // polling, or a user's install/rollback click in that window 409s. Also yield
    // the moment a user op is waiting for the lock, so our ~7s probe never starves
    // their recover click.
    if !c6_present()
        || USER_OP_WAITERS.load(Ordering::SeqCst) > 0
        || INSTALL_RUNNING.load(Ordering::SeqCst)
    {
        return;
    }
    if service_busy(C6_FLASH_SERVICE).await {
        return;
    }

    if service_busy(C6_SUPERVISOR_SERVICE).await {
        // Supervisor up: catch a crash-before-stop mismatch (bundle VERSION != chip)
        // that the next supervisor start would otherwise reflash with no user action.
        // Trust the reported fw ONLY on a fresh heartbeat — the supervisor keeps
        // serving a cached fw after the chip goes stale, and realigning the bundle
        // to that stale version could point it away from the chip's real firmware.
        if chip_responsive().await {
            if let Some(chip) = running_version().await {
                // Realign on a VERSION mismatch OR a stale VERSION beside a swapped
                // firmware.bin (crash mid copy_bundle leaves old VERSION, new bin).
                if read_version(INSTALL_DIR).as_deref() != Some(chip.as_str())
                    || !bundle_bin_is(INSTALL_DIR, &chip)
                {
                    if USER_OP_WAITERS.load(Ordering::SeqCst) > 0
                        || INSTALL_RUNNING.swap(true, Ordering::SeqCst)
                    {
                        return;
                    }
                    let _guard = RunGuard;
                    warn!("[c6-fw] reconcile: bundle VERSION != chip fw {chip} (supervisor up) — realigning");
                    reconcile_bundle_to(&chip).await;
                }
            }
        }
        return;
    }

    // Supervisor DOWN, no flash running → interrupted-flash recovery. Claim the
    // flag now (we are about to mutate + start), then re-check flash isn't busy.
    // Yield if a user op is waiting — a silent-chip recover must win this race.
    if USER_OP_WAITERS.load(Ordering::SeqCst) > 0 || INSTALL_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _guard = RunGuard;
    if service_busy(C6_FLASH_SERVICE).await {
        return;
    }
    // Read the chip directly. If SILENT, do NOT start the supervisor — its
    // Requires= runs the flasher, and we never want an unattended factory path.
    let Some(chip) = probe_chip_fw().await else {
        warn!("[c6-fw] reconcile: C6 silent — not starting supervisor; retry next tick");
        return;
    };
    // Start the supervisor ONLY if the bundle genuinely matches the chip — else its
    // Requires=flash would reflash/mis-promote. Leave it down and retry otherwise.
    if reconcile_bundle_to(&chip).await {
        warn!("[c6-fw] interrupted-flash recovery: chip on {chip}; starting supervisor");
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(60),
            "systemctl",
            &["start", C6_SUPERVISOR_SERVICE],
        )
        .await;
    } else {
        warn!("[c6-fw] reconcile: bundle can't be matched to chip {chip}; leaving supervisor down");
    }
}

/// Terminal state + message for a non-landed flash. Keyed on what the chip
/// actually reported (not on whether the bundle got pinned): a reported version
/// => the chip answered on that fw; no version => silent, point at recovery.
fn flash_fail_terminal(outcome: &FlashOutcome, verb: &str) -> (&'static str, String) {
    match outcome {
        FlashOutcome::OtherVersion(v) => (
            "rolled_back",
            format!("The {verb} did not complete. The ESP32-C6 reports firmware {v}."),
        ),
        FlashOutcome::Silent => (
            "failed",
            format!("The {verb} did not complete and the ESP32-C6 is not reporting a firmware version. If it stays unresponsive, use \"Set up / recover this ESP32-C6\" to recover it."),
        ),
        // Landed never reaches here — callers handle success separately.
        FlashOutcome::Landed(v) => ("success", format!("ESP32-C6 firmware {v} is now running.")),
    }
}

// ── status ──────────────────────────────────────────────────────────────────

/// GET /api/system/c6-firmware
pub async fn get_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let present = c6_present();
    let running = running_version().await;
    let candidate = candidate_ready();
    let rollback = rollback_ready();
    // A flash unit still activating means a write is in flight even if THIS process
    // didn't start it (API restart mid-write) — don't declare idle / offer recover.
    let flash_busy = service_busy(C6_FLASH_SERVICE).await;
    let mut st = read_state();
    // A "running" state with no flash in flight (not in this process AND the unit is
    // idle) is orphaned. Rewrite it to idle with a FRESH timestamp and persist — a
    // default()'s updated_at:0 would be rejected by the modal poller, trapping it.
    if st.state == "running" && !INSTALL_RUNNING.load(Ordering::SeqCst) && !flash_busy {
        st = InstallState {
            updated_at: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
        let _ = std::fs::create_dir_all(STATE_DIR);
        if let Ok(json) = serde_json::to_string(&st) {
            let _ = write_atomic(STATE_FILE, &json);
        }
    }

    // Offer the update only when a candidate is staged, differs from what's
    // running, and nothing is installing. A None running (supervisor quiet)
    // suppresses the offer rather than guessing.
    let update_available = present
        && st.state != "running"
        && match (&candidate, &running) {
            (Some(c), Some(r)) => c != r,
            _ => false,
        };
    let can_rollback = present
        && st.state != "running"
        && match (&rollback, &running) {
            (Some(rb), Some(r)) => rb != r,
            _ => false,
        };
    // Offer recover when the chip is present but NOT responsive (covers supervisor
    // down AND supervisor-up-with-stale-cached-fw). Never while a flash is writing.
    let can_recover =
        present && st.state != "running" && !flash_busy && !chip_responsive().await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "present": present,
            "running_version": running,
            "candidate_version": candidate,
            "rollback_version": rollback,
            "update_available": update_available,
            "can_rollback": can_rollback,
            "can_recover": can_recover,
            "install": st,
        })),
    )
}

// ── flash orchestration ──────────────────────────────────────────────────────

/// What the chip reported after a flash — distinct from whether the on-disk
/// bundle got pinned, so the terminal message reflects the actual chip state.
enum FlashOutcome {
    Landed(String),       // chip confirmed on the requested version
    OtherVersion(String), // chip answered, but on a different version
    Silent,               // chip reported no version
}

/// Drive `c6-flash.sh` for a bundle already staged in INSTALL_DIR: stop the
/// supervisor (it holds the serial port), run the version-gated flasher
/// (oneshot, blocks), probe the chip for the truth, restore/reconcile the bundle,
/// restart the supervisor when it matches the chip, and return what the chip
/// actually reported. Err only when the flasher could not be RUN at all (e.g.
/// the supervisor would not release the port) — a nonzero flasher exit (its
/// deliberate verify-fail/rollback path) is NOT an error here; the probe is truth.
async fn flash_staged_bundle(want: &str, restore_dir: &str) -> anyhow::Result<FlashOutcome> {
    // Free the serial port for the flasher. Abort if it won't release — flashing
    // into a held port corrupts the write and can mis-trigger factory escalation.
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(60),
        "systemctl",
        &["stop", C6_SUPERVISOR_SERVICE],
    )
    .await;
    if service_busy(C6_SUPERVISOR_SERVICE).await {
        // Never flashed — put the bundle back to the caller's baseline so a later
        // supervisor start can't flash the still-staged candidate, then abort.
        if let Err(e) = restore_bundle(restore_dir).await {
            anyhow::bail!(
                "could not stop the C6 supervisor AND bundle restore failed ({e}); staged candidate may remain"
            );
        }
        anyhow::bail!("could not stop the C6 supervisor; aborting flash");
    }
    // Blocking oneshot; wait longer than its TimeoutStartSec (600s) so we never
    // give up while the flasher (incl. a factory escalation) is still writing.
    let flash = sentryusb_shell::run_with_timeout(
        Duration::from_secs(660),
        "systemctl",
        &["start", C6_FLASH_SERVICE],
    )
    .await;
    // Supervisor still down: probe the chip directly for the truth (don't trust the
    // flasher's exit code, which can be 0 on an unverified "alive" path).
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut probed = probe_chip_fw().await;
    for _ in 0..4 {
        if probed.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        probed = probe_chip_fw().await;
    }
    let landed = flash.is_ok() && probed.as_deref() == Some(want);
    // Only restart the supervisor when the bundle matches the LIVE chip — a
    // udev-retriggered flasher run would otherwise reflash a rejected image.
    let bundle_matches_chip = if landed {
        true
    } else {
        // Failed flash: restore the caller's baseline (this DROPS the rejected `want`
        // bin and leaves a CONSISTENT bundle), then align up to the chip's actual
        // version if we can source its image. Restart the supervisor only on a
        // consistent, chip-matching bundle; a silent/unsourceable chip leaves it down
        // for recovery. The on-disk bundle stays internally consistent on every path,
        // so recover() and a udev retrigger never hit the flasher's VERSION/bin death.
        // (If the filesystem is unwritable the restore can't drop the rejected bin; the
        // only consequence is an app-only, key-safe reflash of the user's own chosen
        // image — no key wipe, no brick, and no software mitigation is possible then.)
        let _ = restore_bundle(restore_dir).await;
        match probed.as_deref() {
            Some(fw) => reconcile_bundle_to(fw).await,
            None => false,
        }
    };
    if bundle_matches_chip {
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(90),
            "systemctl",
            &["start", C6_SUPERVISOR_SERVICE],
        )
        .await;
    } else {
        warn!("[c6-fw] post-flash: bundle can't be matched to chip; leaving supervisor down for user recovery");
    }
    // The flasher's exit code is NOT authoritative: c6-flash.sh deliberately exits
    // nonzero on its verify-fail/rollback path, which would make `flash` an Err even
    // though the chip is fine. The direct probe above is the truth — map from it.
    if let Err(e) = &flash {
        warn!("[c6-fw] flash unit exited nonzero ({e}); using the direct chip probe as truth");
    }
    // `Landed` (success) requires BOTH a clean flasher exit AND the probe reporting
    // `want` — a chip that reports `want` after a NONZERO flasher exit (its verify /
    // crash-loop check failed) is reported as OtherVersion, not a clean success.
    Ok(match probed {
        Some(v) if landed => FlashOutcome::Landed(v),
        Some(v) => FlashOutcome::OtherVersion(v),
        None => FlashOutcome::Silent,
    })
}

/// Restore the on-disk bundle from `src` (remount rw + copy + sync). Used on every
/// failed-flash path so firmware.bin and VERSION both match the chip.
async fn restore_bundle(src: &str) -> anyhow::Result<()> {
    remount_rw().await;
    if let Err(e) = copy_bundle(src, INSTALL_DIR) {
        warn!("[c6-fw] could not restore the bundle after a failed flash: {e}");
        return Err(e.into());
    }
    let _ = sentryusb_shell::run("sync", &[]).await;
    Ok(())
}

// ── install ──────────────────────────────────────────────────────────────────

/// POST /api/system/c6-firmware/install
pub async fn install(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if !c6_present() {
        return crate::json_error(StatusCode::CONFLICT, "No ESP32-C6 is plugged in.");
    }
    let Some(target) = candidate_ready() else {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "No ESP32-C6 firmware update is available to install.",
        );
    };
    // Take the lock FIRST, then check chip state while holding it — the acquire can
    // wait up to 15s, and a chip that was responsive at entry could go silent in
    // that window. Flashing a silent chip sends the flasher down its FULL (factory)
    // path, rewriting NVS and losing the Tesla key, so these checks must be fresh.
    let guard = match claim_for_user_op().await {
        Some(g) => g,
        None => {
            return crate::json_error(
                StatusCode::CONFLICT,
                "An ESP32-C6 firmware operation is already running.",
            );
        }
    };
    // Only flash a chip that is actually responding (healthy + fresh heartbeat,
    // not just a cached fw). (guard drops on early return, releasing the lock.)
    if !chip_responsive().await {
        return crate::json_error(
            StatusCode::CONFLICT,
            "The ESP32-C6 isn't responding right now. Try again in a moment.",
        );
    }
    // Refuse a no-op: if the candidate already matches what the chip runs, there's
    // nothing to install, and proceeding would snapshot the current firmware over
    // the rollback target for no reason.
    if running_version().await.as_deref() == Some(target.as_str()) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "The ESP32-C6 is already running that firmware.",
        );
    }
    set_op("install");
    // Publish a fresh "running" state SYNCHRONOUSLY before returning 202, so the
    // UI's first 2s poll can't read a stale terminal state from a previous run and
    // jump straight to "done".
    set_state(&s.hub, "running", "prepare", 0, "Starting…");

    let hub = s.hub.clone();
    tokio::spawn(async move {
        let _guard = guard; // owns the lock for the whole flash; clears on drop
        if let Err(e) = run_install(&hub, &target).await {
            warn!("[c6-fw] install failed: {e}");
            set_state(&hub, "failed", "failed", 100, &e.to_string());
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "started": true })),
    )
}

async fn run_install(hub: &sentryusb_ws::Hub, target: &str) -> anyhow::Result<()> {
    set_state(hub, "running", "prepare", 10, "Preparing update…");
    // Install never factory-writes. Clear any recovery marker a crashed recover may
    // have left on tmpfs, strictly fail-closed, so this flash can't inherit it.
    clear_factory_marker().map_err(|e| anyhow::anyhow!("{e}; refusing to install"))?;
    remount_rw().await;
    if !bundle_bin_is(AVAILABLE_DIR, target) {
        anyhow::bail!("staged update is not firmware {target}; refusing to install");
    }

    // Snapshot only a bundle that matches the chip: a crash-before-stop can leave
    // the bundle already set to the candidate, and snapshotting THAT would destroy
    // the real rollback target. Require a known running version (fail closed if the
    // status read dropped between the handler's check and here); realign or abort.
    let Some(chip) = running_version().await else {
        anyhow::bail!("could not read the running ESP32-C6 version; refusing to snapshot a rollback");
    };
    let aligned = read_version(INSTALL_DIR).as_deref() == Some(chip.as_str())
        && bundle_bin_is(INSTALL_DIR, &chip);
    if !aligned {
        if rollback_ready().as_deref() == Some(chip.as_str()) && bundle_bin_is(ROLLBACK_DIR, &chip) {
            copy_bundle(ROLLBACK_DIR, INSTALL_DIR).map_err(|e| {
                anyhow::anyhow!("could not realign bundle to chip before install: {e}")
            })?;
        } else {
            anyhow::bail!(
                "on-disk bundle does not match the running firmware ({chip}); refusing to snapshot a bad rollback"
            );
        }
    }

    // Capture the version being replaced as the rollback target, then stage the
    // candidate into the flash bundle.
    copy_bundle(INSTALL_DIR, ROLLBACK_DIR)
        .map_err(|e| anyhow::anyhow!("could not snapshot current firmware for rollback: {e}"))?;
    copy_bundle(AVAILABLE_DIR, INSTALL_DIR)
        .map_err(|e| anyhow::anyhow!("could not stage the update: {e}"))?;
    let _ = sentryusb_shell::run("sync", &[]).await;

    set_state(hub, "running", "stop", 25, "Pausing the C6 link…");
    set_state(hub, "running", "flash", 45, "Flashing ESP32-C6…");

    // On failure flash_staged_bundle has already restored the bundle from
    // ROLLBACK_DIR (before the supervisor restart) so the Requires=-triggered
    // flasher SKIPs rather than re-applying the rejected candidate.
    match flash_staged_bundle(target, ROLLBACK_DIR).await? {
        FlashOutcome::Landed(v) => {
            // Consume the candidate so the offer clears.
            let _ = std::fs::remove_dir_all(AVAILABLE_DIR);
            let _ = sentryusb_shell::run("sync", &[]).await;
            set_state(
                hub,
                "success",
                "done",
                100,
                &format!("ESP32-C6 firmware {v} is now running."),
            );
        }
        // Flash ran but did not land. The bundle was already restored/reconciled
        // inside flash_staged_bundle; the candidate stays in AVAILABLE_DIR for retry.
        outcome => {
            set_state(
                hub,
                "running",
                "restore",
                90,
                "Update did not take — restoring previous firmware…",
            );
            let (state, msg) = flash_fail_terminal(&outcome, "update");
            set_state(hub, state, "done", 100, &msg);
        }
    }
    Ok(())
}

// ── rollback ──────────────────────────────────────────────────────────────────

/// POST /api/system/c6-firmware/rollback
pub async fn rollback(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if !c6_present() {
        return crate::json_error(StatusCode::CONFLICT, "No ESP32-C6 is plugged in.");
    }
    let Some(target) = rollback_ready() else {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "No previous ESP32-C6 firmware is saved to roll back to.",
        );
    };
    // Take the lock FIRST, then check chip state while holding it — the acquire can
    // wait up to 15s and a responsive chip could go silent in that window; flashing
    // a silent chip full-flashes and wipes NVS, so these checks must be fresh.
    let guard = match claim_for_user_op().await {
        Some(g) => g,
        None => {
            return crate::json_error(
                StatusCode::CONFLICT,
                "An ESP32-C6 firmware operation is already running.",
            );
        }
    };
    // Same guard as install: never flash a silent chip (full-flash would wipe NVS).
    // (guard drops on early return, releasing the lock.)
    if !chip_responsive().await {
        return crate::json_error(
            StatusCode::CONFLICT,
            "The ESP32-C6 isn't responding right now. Try again in a moment.",
        );
    }
    // No-op guard: if the chip already runs the rollback target, don't proceed —
    // it would snapshot over AVAILABLE_DIR and wipe ROLLBACK_DIR for nothing.
    if running_version().await.as_deref() == Some(target.as_str()) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "The ESP32-C6 is already running that firmware.",
        );
    }
    set_op("rollback");
    set_state(&s.hub, "running", "prepare", 0, "Starting…");

    let hub = s.hub.clone();
    tokio::spawn(async move {
        let _guard = guard; // owns the lock for the whole flash; clears on drop
        if let Err(e) = run_rollback(&hub, &target).await {
            warn!("[c6-fw] rollback failed: {e}");
            set_state(&hub, "failed", "failed", 100, &e.to_string());
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "started": true })),
    )
}

async fn run_rollback(hub: &sentryusb_ws::Hub, target: &str) -> anyhow::Result<()> {
    set_state(hub, "running", "prepare", 15, "Preparing rollback…");
    // Rollback never factory-writes. Clear any leftover recovery marker, strictly
    // fail-closed, so this flash can't inherit a crashed recover's authorization.
    clear_factory_marker().map_err(|e| anyhow::anyhow!("{e}; refusing to roll back"))?;
    remount_rw().await;
    if !bundle_bin_is(ROLLBACK_DIR, target) {
        anyhow::bail!("saved rollback is not firmware {target}; refusing to roll back");
    }

    // Don't re-stage INSTALL_DIR as the candidate unless it IS the chip's current
    // version — else a crash-before-stop would publish a wrong binary as the re-offer.
    let Some(chip) = running_version().await else {
        anyhow::bail!("could not read the running ESP32-C6 version; refusing to roll back");
    };
    if read_version(INSTALL_DIR).as_deref() != Some(chip.as_str()) || !bundle_bin_is(INSTALL_DIR, &chip)
    {
        anyhow::bail!(
            "on-disk bundle does not match the running firmware ({chip}); refusing to roll back"
        );
    }

    // Re-offer the version we're leaving (the current bundle) as the candidate,
    // so the user can re-apply it after rolling back. Then stage the rollback
    // target into the flash bundle.
    copy_bundle(INSTALL_DIR, AVAILABLE_DIR)
        .map_err(|e| anyhow::anyhow!("could not re-stage the current firmware as available: {e}"))?;
    copy_bundle(ROLLBACK_DIR, INSTALL_DIR)
        .map_err(|e| anyhow::anyhow!("could not stage the rollback firmware: {e}"))?;
    let _ = sentryusb_shell::run("sync", &[]).await;

    set_state(hub, "running", "flash", 50, "Flashing previous firmware…");
    // On failure flash_staged_bundle restores the bundle from AVAILABLE_DIR (the
    // version we were on) before the supervisor restart, so the Requires=-flasher
    // SKIPs instead of re-applying the rollback target.
    match flash_staged_bundle(target, AVAILABLE_DIR).await? {
        FlashOutcome::Landed(v) => {
            // Rolled back cleanly. No previous-of-previous to offer, and the
            // version we left is now the available candidate.
            let _ = std::fs::remove_dir_all(ROLLBACK_DIR);
            let _ = sentryusb_shell::run("sync", &[]).await;
            set_state(
                hub,
                "success",
                "done",
                100,
                &format!("Rolled back — ESP32-C6 firmware {v} is now running."),
            );
        }
        // Rollback did not land; bundle already restored inside flash_staged_bundle.
        outcome => {
            set_state(
                hub,
                "running",
                "restore",
                90,
                "Rollback did not take — keeping current firmware…",
            );
            let _ = std::fs::remove_dir_all(AVAILABLE_DIR);
            let _ = sentryusb_shell::run("sync", &[]).await;
            let (state, msg) = flash_fail_terminal(&outcome, "rollback");
            set_state(hub, state, "done", 100, &msg);
        }
    }
    Ok(())
}

// ── recover / set up a silent or blank chip ──────────────────────────────────

// On tmpfs so a crash/power-loss can't leave a factory authorization armed across
// a reboot. Must match FACTORY_FLASH_MARKER in c6-flash.sh.
const FACTORY_MARKER: &str = "/run/sentryusb-c6/ALLOW_FACTORY_FLASH";

/// Clear any stale factory authorization, STRICTLY fail-closed. Ok when the marker
/// is absent (NotFound); Err on any other fs error — don't trust `Path::exists()`,
/// which also returns false on a metadata error and could let a readable marker slip.
fn clear_factory_marker() -> anyhow::Result<()> {
    match std::fs::remove_file(FACTORY_MARKER) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "could not clear a stale C6 recovery authorization: {e}"
        )),
    }
}

/// Removes the one-shot factory authorization on drop so a panic/early return can't
/// leave it armed.
struct MarkerGuard;
impl Drop for MarkerGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(FACTORY_MARKER);
    }
}

/// POST /api/system/c6-firmware/recover
/// User-authorized recovery of a silent/blank chip: arms the one-shot marker the
/// flasher needs to write a chip that isn't responding. The flasher tries an
/// app-only write first (key preserved) and only factory-writes if that won't boot.
pub async fn recover(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if !c6_present() {
        return crate::json_error(StatusCode::CONFLICT, "No ESP32-C6 is plugged in.");
    }
    // Take the lock FIRST, then check port/supervisor state while holding it. The
    // acquire can wait up to 15s for a reconcile probe; checking chip_responsive /
    // service_busy beforehand would let that result go stale (e.g. reconcile brings
    // the chip + supervisor back healthy during the wait and we would still arm a
    // factory flash and stop a healthy supervisor).
    let guard = match claim_for_user_op().await {
        Some(g) => g,
        None => {
            return crate::json_error(
                StatusCode::CONFLICT,
                "An ESP32-C6 firmware operation is already running.",
            );
        }
    };
    // If the chip is actually responsive, it isn't silent — use update/rollback.
    // (guard drops here on early return, releasing the lock.)
    if chip_responsive().await {
        return crate::json_error(
            StatusCode::CONFLICT,
            "The ESP32-C6 is responding — use update or rollback instead.",
        );
    }
    if service_busy(C6_FLASH_SERVICE).await {
        return crate::json_error(
            StatusCode::CONFLICT,
            "A firmware flash is already running.",
        );
    }
    set_op("recover");
    set_state(&s.hub, "running", "prepare", 0, "Starting…");

    let hub = s.hub.clone();
    tokio::spawn(async move {
        let _guard = guard; // owns the lock for the whole flash; clears on drop
        if let Err(e) = run_recover(&hub).await {
            warn!("[c6-fw] recover failed: {e}");
            set_state(&hub, "failed", "failed", 100, &e.to_string());
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "started": true })),
    )
}

async fn run_recover(hub: &sentryusb_ws::Hub) -> anyhow::Result<()> {
    set_state(hub, "running", "prepare", 10, "Preparing recovery…");
    remount_rw().await;
    // Self-heal a bundle left inconsistent by a crash mid copy_bundle (firmware.bin
    // renamed, VERSION not): pin VERSION to the on-disk bin's baked app version so the
    // flasher's VERSION/bin consistency check passes and recovery can't get stuck.
    if let Some(baked) = baked_app_version(&format!("{INSTALL_DIR}/firmware.bin")) {
        if read_version(INSTALL_DIR).as_deref() != Some(baked.as_str()) {
            let _ = write_atomic(&format!("{INSTALL_DIR}/VERSION"), &format!("{baked}\n"));
        }
    }
    let want = read_version(INSTALL_DIR)
        .ok_or_else(|| anyhow::anyhow!("no bundled firmware to write"))?;
    let _ = std::fs::create_dir_all(STATE_DIR);
    // Timestamped so the flasher can ignore a stale authorization.
    write_atomic(FACTORY_MARKER, &format!("{}\n", chrono::Utc::now().timestamp()))
        .map_err(|e| anyhow::anyhow!("could not authorize recovery: {e}"))?;
    let _marker = MarkerGuard; // disarm on return/panic
    let _ = sentryusb_shell::run("sync", &[]).await;

    set_state(hub, "running", "flash", 45, "Recovering the ESP32-C6…");
    match flash_staged_bundle(&want, INSTALL_DIR).await? {
        FlashOutcome::Landed(v) => {
            set_state(
                hub,
                "success",
                "done",
                100,
                &format!("ESP32-C6 firmware {v} is now running."),
            );
        }
        // Recovery ran but the chip did not come back on the recovered version.
        FlashOutcome::OtherVersion(v) => {
            set_state(
                hub,
                "failed",
                "failed",
                100,
                &format!("Recovery did not complete. The ESP32-C6 reports firmware {v}. Unplug/replug or press RESET, then try again."),
            );
        }
        FlashOutcome::Silent => {
            set_state(
                hub,
                "failed",
                "failed",
                100,
                "Recovery did not complete and the ESP32-C6 is not responding. Unplug/replug or press RESET, then try again.",
            );
        }
    }
    Ok(())
}
