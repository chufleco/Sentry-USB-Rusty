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
    let adapter = current_ble_adapter_for_pair();
    tokio::spawn(async move {
        hub.broadcast("ble_status", &serde_json::json!({"status": "pairing"}));
        let vin_upper = vin.to_uppercase();

        // ── Step 1: quiesce everything that touches the radio. ──
        // sentryusb-telemetry holds a persistent BLE session and
        // would compete with tesla-control for the slot. The iOS
        // GATT daemon (sentryusb-ble) registers as a peripheral
        // server. bluetoothd itself gets stopped briefly so we can
        // power-cycle the controller before tesla-control needs it.
        let _ = sentryusb_shell::run("systemctl", &["stop", "sentryusb-telemetry"]).await;
        let _ = sentryusb_shell::run("systemctl", &["stop", "sentryusb-ble"]).await;
        let _ = sentryusb_shell::run("systemctl", &["stop", "bluetooth"]).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        // ── Step 2: restart bluetoothd. ──
        // An earlier revision of this handler left bluetoothd
        // stopped through the tesla-control call, assuming
        // tesla-control wanted exclusive raw-HCI access. In
        // practice tesla-control talks to bluez via D-Bus, so it
        // needs bluetoothd UP. Leaving bluetoothd down "worked" on
        // most chips (the underlying mgmt-socket calls succeeded
        // anyway) but silently broke pair on the Rock 4C+'s
        // BCM4345C0 build 0342 firmware where every subsequent
        // BLE write was a no-op until something reset the
        // controller.
        let _ = sentryusb_shell::run("systemctl", &["start", "bluetooth"]).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        // ── Step 3: full controller power-cycle. ──
        // The BCM4345C0 (Rock Pi 4C+, possibly other Broadcom
        // boards on recent kernels) enters a corrupt state after
        // enough connection cycles where every BLE write returns
        // OK at the bluez API level but never reaches the wire.
        // dmesg shows `unexpected event for opcode 0x2016`
        // warnings when this happens. The only thing that clears
        // it is `hci_dev_close()` which `bluetoothctl power off`
        // triggers via the management interface. ~3s extra during
        // a one-time pair operation is a small price for working
        // on broken chips; no functional cost on healthy ones.
        let _ = sentryusb_shell::run("bluetoothctl", &["power", "off"]).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = sentryusb_shell::run("bluetoothctl", &["power", "on"]).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        // ── Step 4: the actual add-key-request. ──
        // All flags passed explicitly so default-resolution
        // differences across tesla-control versions can't bite us:
        //   -bt-adapter — uses the user's configured choice (USB
        //                 dongle vs onboard); pre-fix, tesla-control
        //                 defaulted to hci0 even when the user had
        //                 selected hci1.
        //   -connect-timeout / -command-timeout — generous (30s
        //                 each) because the post-power-cycle window
        //                 is when the chip is at its most reliable;
        //                 we'd rather wait longer than fail fast.
        let add_result = sentryusb_shell::run_with_timeout(
            Duration::from_secs(120),
            "/root/bin/tesla-control",
            &[
                "-ble",
                "-vin", &vin_upper,
                "-bt-adapter", &adapter,
                "-connect-timeout", "30s",
                "-command-timeout", "30s",
                "add-key-request",
                "/root/.ble/key_public.pem",
                "owner", "cloud_key",
            ],
        ).await;

        // ── Step 5: post-pair BLE verification. ──
        // tesla-control returning exit 0 from add-key-request only
        // proves that the bluez WriteValue D-Bus call returned —
        // not that the bytes actually reached the car. On the
        // BCM4345C0 + kernel 6.18 combo, writes succeed at the
        // bluez layer while being silently dropped at the controller.
        // tesla-control then exits success, the UI shows "tap your
        // card", and the user wonders why the car never prompted.
        //
        // Fix: immediately probe `session-info`. This needs a fresh
        // BLE round-trip and will succeed (or fail with a known
        // protocol error like "key not whitelisted") if the link
        // works. It will time out / "connect failed" / "Failed to
        // initiate write" if the chip is in the bad state. We use
        // that distinction to surface a meaningful error instead
        // of the misleading "tap your card" message.
        let verify_ok = match &add_result {
            Ok(_) => {
                // Verify via sentryusb-ble-action (same binary the
                // ble-status probe below uses), which prints one token:
                // PAIRED / NOT_PAIRED / UNREACHABLE. Both PAIRED and
                // NOT_PAIRED mean the round-trip reached the car —
                // NOT_PAIRED is just "key not whitelisted yet" because
                // the user hasn't tapped their card, which still proves
                // the radio link works. Only UNREACHABLE (or a missing/
                // garbled reply) is the chip-broken transport state we
                // flag. The pair flow has stopped the telemetry daemon,
                // so this takes the binary's direct-BLE fallback — the
                // fresh, independent round-trip we want here. 20s matches
                // the ble-status probe's cold direct-scan budget.
                let probe = sentryusb_shell::run_with_timeout(
                    Duration::from_secs(20),
                    "/root/bin/sentryusb-ble-action",
                    &["session-info"],
                ).await;
                let token = probe.as_deref().map(str::trim).unwrap_or("");
                matches!(token, "PAIRED" | "NOT_PAIRED")
            }
            Err(_) => false,
        };

        // ── Step 6: bring services back up. ──
        let _ = sentryusb_shell::run("systemctl", &["start", "sentryusb-ble"]).await;
        let _ = sentryusb_shell::run("systemctl", &["start", "sentryusb-telemetry"]).await;

        match add_result {
            Ok(output) if verify_ok => {
                hub.broadcast(
                    "ble_status",
                    &serde_json::json!({"status": "waiting", "output": output}),
                );
            }
            Ok(_) => {
                // The "lies about success" case. add-key-request
                // returned 0 but the post-pair probe couldn't reach
                // the car. Surface what we know so the user doesn't
                // sit waiting for a card prompt that will never
                // come. Includes `verify_failed: true` so the
                // frontend can render this case more prominently if
                // it wants to (separate code path from the generic
                // "tesla-control errored out" case).
                hub.broadcast(
                    "ble_status",
                    &serde_json::json!({
                        "status": "error",
                        "verify_failed": true,
                        "error": "Pair request returned success but the BLE adapter isn't reliably reaching the car \
                                  (verification probe failed). This usually means the onboard Bluetooth chip is in a \
                                  bad state (a known firmware/kernel quirk on some Broadcom chips like the Rock 4C+'s \
                                  BCM4345C0). Plug in a USB Bluetooth dongle and select it under BLE adapter settings, \
                                  then retry pairing.",
                    }),
                );
            }
            Err(e) => {
                let mut msg = e.to_string();
                if let Some(idx) = msg.find("stderr: ") {
                    msg = msg[idx + 8..].to_string();
                }
                hub.broadcast(
                    "ble_status",
                    &serde_json::json!({"status": "error", "error": msg}),
                );
            }
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "pairing_started"})))
}

/// Read the user's configured BLE adapter for the pair flow.
/// Duplicates the resolver in `ble.rs::current_ble_adapter` to avoid
/// cross-module visibility changes for a 10-line helper. If they
/// drift, both should be brought back together — neither is
/// performance-sensitive (called once per pair attempt).
fn current_ble_adapter_for_pair() -> String {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("BLE_ADAPTER") {
            let trimmed = v.trim();
            if trimmed.starts_with("hci") {
                return trimmed.to_string();
            }
        }
    }
    "hci0".to_string()
}

/// Remount the root filesystem read-write. These images keep `/`
/// read-only to protect the SD card; a plain write to `/root` silently
/// no-ops until this runs. Mirrors the remount the keygen / config /
/// VIN-set paths in `ble.rs` already do before their writes.
fn remount_root_rw() {
    if let Err(e) = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status()
    {
        tracing::warn!("remountfs_rw failed to run: {e}");
    }
}

/// Persist the BLE pairing marker so the pair card survives a reload.
///
/// `paired=true` writes `/root/.ble/paired` and clears the
/// `key_pending_pairing` flag; `paired=false` removes the marker.
/// Always remounts rw first — without it the write no-ops on the
/// read-only-root images and the card reverts to "Pair" on every
/// navigation (the bug this fixes). Logs on failure instead of
/// swallowing it, so a future regression is visible in the journal.
fn set_ble_paired_marker(paired: bool) {
    remount_root_rw();
    // ENOENT on a remove just means the file wasn't there — expected,
    // not worth a warning.
    let ignore_missing = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    if paired {
        if let Err(e) = std::fs::write("/root/.ble/paired", "1") {
            tracing::warn!("failed to write /root/.ble/paired: {e}");
        }
        if let Err(e) = std::fs::remove_file("/root/.ble/key_pending_pairing") {
            if !ignore_missing(&e) {
                tracing::warn!("failed to clear /root/.ble/key_pending_pairing: {e}");
            }
        }
    } else if let Err(e) = std::fs::remove_file("/root/.ble/paired") {
        if !ignore_missing(&e) {
            tracing::warn!("failed to remove /root/.ble/paired: {e}");
        }
    }
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
        set_ble_paired_marker(true);
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "paired",
            "vin": vin,
            "binaries_installed": binaries_installed,
        })));
    }

    // Full pairing probe via sentryusb-ble-action, which reuses the
    // telemetry daemon's warm connection (IPC) instead of spawning a
    // competing tesla-control process that fought the radio and — on
    // any timeout — wrongly deleted the paired marker. The binary
    // prints one stdout token; run_with_timeout only surfaces stdout,
    // so we match on that. 20s covers a cold direct-fallback scan when
    // the daemon is down.
    let probe = sentryusb_shell::run_with_timeout(
        Duration::from_secs(20),
        "/root/bin/sentryusb-ble-action",
        &["session-info"],
    )
    .await;
    let token = probe.as_deref().map(str::trim).unwrap_or("");

    match token {
        "PAIRED" => {
            // Feed the live "connected" indicator on the BLE card.
            crate::ble::mark_ble_success();
            set_ble_paired_marker(true);
            (StatusCode::OK, Json(serde_json::json!({
                "status": "paired",
                "vin": vin,
                "binaries_installed": binaries_installed,
            })))
        }
        "NOT_PAIRED" => {
            // The ONLY case that clears the marker: the car explicitly
            // rejected our key (KEY_NOT_ON_WHITELIST). Re-pair needed.
            set_ble_paired_marker(false);
            (StatusCode::OK, Json(serde_json::json!({
                "status": "keys_generated",
                "vin": vin,
                "binaries_installed": binaries_installed,
                "note": "Car reports this key is not paired -- re-pair from the BLE card and tap your card on the console",
            })))
        }
        _ => {
            // UNREACHABLE, a non-zero exit (config error / binary
            // missing), or an empty/garbled reply: pairing is unknown,
            // not disproven. Leave the marker as-is so a busy radio or a
            // sleeping car can't flip the card to "not paired".
            if let Err(e) = &probe {
                tracing::warn!(
                    "ble session-info probe could not run ({e:#}); leaving paired marker unchanged"
                );
            }
            let paired = std::path::Path::new("/root/.ble/paired").exists();
            (StatusCode::OK, Json(serde_json::json!({
                "status": if paired { "paired" } else { "keys_generated" },
                "vin": vin,
                "binaries_installed": binaries_installed,
                "note": "Could not verify pairing right now (car asleep or BLE radio busy); status unchanged",
            })))
        }
    }
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
pub async fn get_rtc_status(State(_s): State<AppState>) -> impl IntoResponse {
    let rtc_exists = std::path::Path::new("/dev/rtc0").exists();
    let mut rtc_time = String::new();
    if rtc_exists {
        if let Ok(out) = sentryusb_shell::run("hwclock", &["-r"]).await {
            rtc_time = out.trim().to_string();
        }
    }
    // RTC presence is a hardware fact that doesn't change at runtime.
    // The Dashboard hits this on every load — let the browser short-
    // circuit subsequent requests for 5 min and save a round trip.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=300")],
        Json(serde_json::json!({
            "available": rtc_exists,
            "time": rtc_time,
        })),
    )
}

/// GET /api/system/clock-status
///
/// Reports whether the Pi's system clock can be trusted for
/// timestamping samples + matching them to drives later. Used by the
/// BLE pair card to show a short "clock not synced" hint ONLY when
/// all of:
///   * The system clock looks bogus (year < 2025 = unset / Jan-1-2000
///     fallback / etc.)
///   * No RTC battery is installed (with RTC, clock survives reboots)
///   * No NTP sync has happened yet
///
/// Note: the telemetry sampler can now self-correct the system clock
/// from any successful BLE state-poll response (Tesla embeds a
/// GPS-derived timestamp in every state reply). So even without RTC
/// or WiFi, the clock comes good as soon as the car responds once.
/// The warning is now informational ("we're waiting on the first
/// reading") rather than blocking.
///
/// Response shape:
/// ```json
/// {
///   "synced": true,            // year >= 2025 OR systemd-timesyncd marker
///   "has_rtc": true,           // /dev/rtc0 exists
///   "ntp_synced": true,        // /run/systemd/timesync/synchronized exists
///   "show_warning": false      // !synced && !has_rtc && !ntp_synced
/// }
/// ```
pub async fn get_clock_status(
    State(_s): State<AppState>,
) -> impl IntoResponse {
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

    // NTP sync state flips at most a handful of times per boot. A 10s
    // cache cuts repeat polling without hiding state changes that
    // matter to the BLE warning UI.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=10")],
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
pub async fn get_ssh_pubkey(State(_s): State<AppState>) -> impl IntoResponse {
    let pub_key = std::fs::read_to_string("/root/.ssh/id_ed25519.pub")
        .or_else(|_| std::fs::read_to_string("/root/.ssh/id_rsa.pub"))
        .unwrap_or_default();
    // The pubkey only changes when generate_ssh_key runs; cache an
    // hour and let users explicitly reload when they regenerate.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=3600")],
        Json(serde_json::json!({"public_key": pub_key.trim()})),
    )
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
