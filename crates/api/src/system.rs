//! System actions: reboot, toggle drives, BLE pair, speedtest, SSH, diagnostics, RTC.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use crate::router::AppState;

/// POST /api/system/reboot
pub async fn reboot(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async { let _ = sentryusb_shell::run("reboot", &[]).await; });
    crate::json_ok()
}

/// POST /api/system/shutdown
///
/// Power off the device. Spawned so the HTTP response can flush before
/// the kernel starts tearing things down. Falls back through `poweroff`
/// → `shutdown -h now` → `systemctl poweroff` since some minimal images
/// only ship one of the three.
pub async fn shutdown(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async {
        if sentryusb_shell::run("poweroff", &[]).await.is_ok() {
            return;
        }
        if sentryusb_shell::run("shutdown", &["-h", "now"]).await.is_ok() {
            return;
        }
        let _ = sentryusb_shell::run("systemctl", &["poweroff"]).await;
    });
    crate::json_ok()
}

/// POST /api/system/toggle-drives
pub async fn toggle_drives(State(_s): State<AppState>, _body: String) -> (StatusCode, Json<serde_json::Value>) {
    let was_active = sentryusb_gadget::is_active();
    let result = if was_active {
        tokio::task::spawn_blocking(sentryusb_gadget::disable).await
    } else {
        tokio::task::spawn_blocking(sentryusb_gadget::enable).await
    };
    match result {
        Ok(Ok(())) => crate::json_ok(),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget {} failed: {}", if was_active { "disable" } else { "enable" }, e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget task panicked: {}", e),
        ),
    }
}

/// POST /api/system/gadget-enable — idempotent set-to-active.
///
/// Called from the `/root/bin/enable_gadget.sh` shim so archiveloop coordinates
/// with this server instead of driving configfs directly in parallel.
pub async fn gadget_enable(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if sentryusb_gadget::is_active() {
        return crate::json_ok();
    }
    match tokio::task::spawn_blocking(sentryusb_gadget::enable).await {
        Ok(Ok(())) => crate::json_ok(),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget enable failed: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget task panicked: {}", e),
        ),
    }
}

/// POST /api/system/gadget-disable — idempotent set-to-inactive.
pub async fn gadget_disable(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if !sentryusb_gadget::is_active() {
        return crate::json_ok();
    }
    match tokio::task::spawn_blocking(sentryusb_gadget::disable).await {
        Ok(Ok(())) => crate::json_ok(),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget disable failed: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget task panicked: {}", e),
        ),
    }
}

/// POST /api/system/trigger-sync
///
/// Force archiveloop to start a sync cycle now, regardless of the
/// connectivity check's current opinion. archiveloop has two distinct
/// wait states the loop can be sitting in when the user clicks "Start
/// Archive":
///
///   1. `wait_for_archive_to_be_reachable` — usual case after a fresh
///      boot or after the car drove away from the home WiFi. Loop
///      polls archive-is-reachable.sh until it succeeds. Consumes
///      `/tmp/archive_is_reachable` to fake a positive result and
///      proceed to the archive step.
///
///   2. `wait_for_archive_to_be_unreachable` — idle steady state after
///      archive completed; loop is waiting for the car to drive away
///      so the next archive cycle can start fresh. Consumes
///      `/tmp/archive_is_unreachable` to fake "user drove away" and
///      proceed back to step 1.
///
/// The Go-era `force_sync.sh` only created the unreachable canary,
/// which is correct for state (2) but a no-op for state (1) — the
/// exact case a user hits when their NAS is briefly down or the
/// reachability check is misconfigured. Create the unreachable canary
/// first (covering state 2), wait a moment for archiveloop to
/// consume it, then create the reachable canary (covering both: state
/// 1 directly, or state 2 after archiveloop transitions out via the
/// first canary). Either way the loop kicks off an archive cycle.
pub async fn trigger_sync(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async {
        let unreachable = std::path::Path::new("/tmp/archive_is_unreachable");
        let reachable = std::path::Path::new("/tmp/archive_is_reachable");
        // Step 1: kick a loop sitting in wait_for_unreachable.
        let _ = std::fs::File::create(unreachable);
        // Wait up to ~5s for archiveloop to consume it. If it doesn't,
        // the loop is already past that state (in wait_for_reachable),
        // and a stale canary left lying around would otherwise fire on
        // the next idle cycle and cause a phantom force-sync the user
        // didn't ask for. Clean up either way.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !unreachable.exists() {
                break;
            }
        }
        let _ = std::fs::remove_file(unreachable);
        // Step 2: kick a loop sitting in wait_for_reachable. archiveloop
        // consumes this and starts an archive cycle even if the real
        // network probe is currently failing — exactly what a user
        // means when they click "Start Archive Now".
        let _ = std::fs::File::create(reachable);
    });
    crate::json_ok()
}

/// POST /api/system/ble-pair
pub async fn ble_pair(State(s): State<AppState>, _body: String) -> (StatusCode, Json<serde_json::Value>) {
    // Master kill-switch: when the user has flipped Tesla BLE off in
    // settings, refuse pairing even if a VIN is configured. The
    // setting is the security boundary that protects the car from a
    // Pi-as-proximity-key scenario.
    if !crate::ble::is_ble_enabled() {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "BLE is disabled in settings — enable it before pairing",
        );
    }

    let config_path = sentryusb_config::find_config_path();
    let vin = match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => active.get("TESLA_BLE_VIN").cloned().unwrap_or_default(),
        Err(_) => String::new(),
    };

    if vin.is_empty() {
        return crate::json_error(StatusCode::BAD_REQUEST, "TESLA_BLE_VIN not configured");
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        hub.broadcast("ble_status", &serde_json::json!({"status": "pairing"}));
        let vin_upper = vin.to_uppercase();

        // Stop BLE daemon and bluetoothd for exclusive hci0 access
        let _ = sentryusb_shell::run("systemctl", &["stop", "sentryusb-ble"]).await;
        let _ = sentryusb_shell::run("systemctl", &["stop", "bluetooth"]).await;

        let result = sentryusb_shell::run_with_timeout(
            Duration::from_secs(120),
            "/root/bin/tesla-control",
            &["-ble", "-vin", &vin_upper, "add-key-request", "/root/.ble/key_public.pem", "owner", "cloud_key"],
        ).await;

        // Restart services
        let _ = sentryusb_shell::run("systemctl", &["start", "bluetooth"]).await;
        let _ = sentryusb_shell::run("systemctl", &["start", "sentryusb-ble"]).await;

        match result {
            Ok(output) => {
                hub.broadcast("ble_status", &serde_json::json!({"status": "waiting", "output": output}));
            }
            Err(e) => {
                let mut msg = e.to_string();
                if let Some(idx) = msg.find("stderr: ") {
                    msg = msg[idx + 8..].to_string();
                }
                hub.broadcast("ble_status", &serde_json::json!({"status": "error", "error": msg}));
            }
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "pairing_started"})))
}

/// GET /api/system/ble-status
pub async fn ble_status(
    State(_s): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let pub_exists = std::path::Path::new("/root/.ble/key_public.pem").exists();
    let priv_exists = std::path::Path::new("/root/.ble/key_private.pem").exists();

    // VIN is read up front so every response path can include it —
    // the BLE pair card uses this to pre-populate the VIN input
    // regardless of whether pairing is complete.
    let config_path = sentryusb_config::find_config_path();
    let vin = match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => active.get("TESLA_BLE_VIN").cloned().unwrap_or_default(),
        Err(_) => String::new(),
    };
    let binaries_installed = std::path::Path::new("/root/bin/tesla-control").exists()
        && std::path::Path::new("/root/bin/tesla-keygen").exists();

    if !pub_exists || !priv_exists {
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "not_paired",
            "vin": vin,
            "binaries_installed": binaries_installed,
        })));
    }

    if vin.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "keys_generated",
            "vin": "",
            "binaries_installed": binaries_installed,
        })));
    }

    // Quick check (no BLE probe)
    if params.get("quick").map(|v| v.as_str()) == Some("true") {
        if std::path::Path::new("/root/.ble/paired").exists() {
            return (StatusCode::OK, Json(serde_json::json!({
                "status": "paired",
                "vin": vin,
                "binaries_installed": binaries_installed,
            })));
        }
        if std::path::Path::new("/root/.ble/key_pending_pairing").exists() {
            return (StatusCode::OK, Json(serde_json::json!({
                "status": "keys_generated",
                "vin": vin,
                "binaries_installed": binaries_installed,
            })));
        }
        let _ = std::fs::write("/root/.ble/paired", "1");
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "paired",
            "vin": vin,
            "binaries_installed": binaries_installed,
        })));
    }

    // Full BLE session-info probe
    let result = sentryusb_shell::run_with_timeout(
        Duration::from_secs(15),
        "/root/bin/tesla-control",
        &["-ble", "-vin", &vin.to_uppercase(), "session-info", "/root/.ble/key_private.pem", "infotainment"],
    ).await;

    if result.is_err() {
        let _ = std::fs::remove_file("/root/.ble/paired");
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "keys_generated",
            "vin": vin,
            "binaries_installed": binaries_installed,
            "note": "Car not reachable or key not paired",
        })));
    }

    // session-info round-trip succeeded — feed the live "connected"
    // indicator on the BLE settings card.
    crate::ble::mark_ble_success();
    let _ = std::fs::write("/root/.ble/paired", "1");
    let _ = std::fs::remove_file("/root/.ble/key_pending_pairing");
    (StatusCode::OK, Json(serde_json::json!({
        "status": "paired",
        "vin": vin,
        "binaries_installed": binaries_installed,
    })))
}

/// GET /api/system/speedtest — stream 64MB of random data for bandwidth testing.
///
/// The 64 KB chunk is filled once at first request and reused for the
/// lifetime of the process. Bandwidth tests don't need cryptographic
/// uniqueness per byte — they just need network throughput pressure —
/// so pre-filling eliminates ~8.2M `rand::random::<u64>()` calls per
/// invocation (1000 chunks × 8192 random u64s) which were the actual
/// bottleneck, not the allocation.
static SPEEDTEST_CHUNK: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

fn speedtest_chunk() -> &'static Vec<u8> {
    SPEEDTEST_CHUNK.get_or_init(|| {
        let mut buf = vec![0u8; 65536];
        for chunk in buf.chunks_mut(8) {
            let val = rand::random::<u64>();
            let bytes = val.to_le_bytes();
            let len = chunk.len().min(8);
            chunk[..len].copy_from_slice(&bytes[..len]);
        }
        buf
    })
}

pub async fn speedtest(State(_s): State<AppState>) -> impl IntoResponse {
    use axum::body::Body;

    let chunk = speedtest_chunk();
    let stream = tokio_stream::iter(
        (0..1000).map(move |_| Ok::<_, std::convert::Infallible>(chunk.clone()))
    );

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
}

/// GET /api/system/rtc-status
pub async fn get_rtc_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let rtc_exists = std::path::Path::new("/dev/rtc0").exists();
    let mut rtc_time = String::new();
    if rtc_exists {
        if let Ok(out) = sentryusb_shell::run("hwclock", &["-r"]).await {
            rtc_time = out.trim().to_string();
        }
    }
    (StatusCode::OK, Json(serde_json::json!({
        "available": rtc_exists,
        "time": rtc_time,
    })))
}

/// GET /api/system/clock-status
///
/// Reports whether the Pi's system clock can be trusted for
/// timestamping samples + matching them to drives later. Used by the
/// BLE pair card to show a "clock not synced — sampling paused"
/// warning ONLY when both:
///   * The system clock looks bogus (year < 2025 = unset / Jan-1-2000
///     fallback / etc.)
///   * No RTC battery is installed (with RTC, clock survives reboots
///     and is fine the moment the kernel comes up)
///
/// Users with an RTC battery never see the warning. Users without
/// one only see it during the brief window between boot and NTP
/// sync — which on home WiFi is typically seconds.
///
/// Response shape:
/// ```json
/// {
///   "synced": true,            // year >= 2025 OR systemd-timesyncd marker
///   "has_rtc": true,           // /dev/rtc0 exists
///   "ntp_synced": true,        // /run/systemd/timesync/synchronized exists
///   "show_warning": false      // !synced && !has_rtc — what the UI should gate on
/// }
/// ```
pub async fn get_clock_status(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let ntp_synced =
        std::path::Path::new("/run/systemd/timesync/synchronized").exists();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 2025-01-01 00:00:00 UTC = 1735689600.
    let year_looks_recent = secs > 1_735_689_600;
    let synced = ntp_synced || year_looks_recent;
    let has_rtc = std::path::Path::new("/dev/rtc0").exists();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "synced": synced,
            "has_rtc": has_rtc,
            "ntp_synced": ntp_synced,
            // The single boolean the UI cares about — don't pester
            // RTC users, only warn when clock is bad AND there's no
            // hardware fallback.
            "show_warning": !synced && !has_rtc,
        })),
    )
}

/// GET /api/system/ssh-pubkey
pub async fn get_ssh_pubkey(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let pub_key = std::fs::read_to_string("/root/.ssh/id_ed25519.pub")
        .or_else(|_| std::fs::read_to_string("/root/.ssh/id_rsa.pub"))
        .unwrap_or_default();
    (StatusCode::OK, Json(serde_json::json!({"public_key": pub_key.trim()})))
}

/// POST /api/system/ssh-keygen
pub async fn generate_ssh_key(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // Production images run with a read-only root, so writing to
    // /root/.ssh fails (EROFS) without remounting first. remountfs_rw is
    // the canonical helper; the mount fallback covers dev images where
    // it isn't installed.
    let _ = sentryusb_shell::run(
        "bash",
        &["-c", "/root/bin/remountfs_rw 2>/dev/null || mount -o remount,rw / 2>/dev/null || true"],
    )
    .await;

    let key_path = "/root/.ssh/id_ed25519";
    let _ = std::fs::remove_file(key_path);
    let _ = std::fs::remove_file(format!("{}.pub", key_path));
    let _ = std::fs::create_dir_all("/root/.ssh");

    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(15),
        "ssh-keygen",
        &["-t", "ed25519", "-f", key_path, "-N", "", "-C", "sentryusb"],
    ).await {
        Ok(_) => {
            let pub_key = std::fs::read_to_string(format!("{}.pub", key_path)).unwrap_or_default();
            (StatusCode::OK, Json(serde_json::json!({"public_key": pub_key.trim()})))
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to generate SSH key: {}", e)),
    }
}
