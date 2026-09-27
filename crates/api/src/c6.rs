//! ESP32-C6 co-processor detection and telemetry-source selection. The C6 is a
//! plug-in BLE radio that owns the car link and feeds a snapshot the sampler
//! copies into the DB. The UI uses this to detect the C6 and switch
//! `TELEMETRY_SOURCE` between `c6_primary` (use the C6) and the onboard sampler.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;

/// Character device the C6 supervisor creates while the co-processor is plugged
/// in. Same path the telemetry daemon's `c6_coord::c6_present` checks.
const C6_DEVICE: &str = "/dev/sentryusb-c6";
/// The C6 supervisor's local status/telemetry API.
const C6_SUPERVISOR_API: &str = "http://127.0.0.1:8787";
/// The C6 supervisor's config. Its `[poll] enabled` gates signed domain polling
/// (talking to the car), and defaults OFF for car safety.
const C6_SUPERVISOR_CONFIG: &str = "/etc/sentryusb/c6-supervisor.toml";
/// systemd unit for the C6 supervisor.
const C6_SUPERVISOR_SERVICE: &str = "sentryusb-c6-supervisor";
/// The Pi's Tesla key (custodian model: copied to the C6, always Pi -> C6).
const PI_KEY_PATH: &str = "/root/.ble/key_private.pem";

/// Serializes the telemetry-source and telemetry-master endpoints so concurrent
/// requests can't clobber sentryusb.conf or leave source/poll disagreeing.
pub(crate) static CONFIG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Keep supervisor polling in step with the telemetry master: telemetry off must
/// stop the C6 polling/signing too. No-op unless the C6 is the active source.
pub(crate) async fn sync_supervisor_poll_to_telemetry(telemetry_enabled: bool) -> anyhow::Result<()> {
    if !c6_source_active() {
        return Ok(());
    }
    let existed = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let text = match std::fs::read_to_string(C6_SUPERVISOR_CONFIG) {
            Ok(t) => t,
            // No supervisor config: nothing to poll-gate (C6 not installed here).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(anyhow::anyhow!("read {C6_SUPERVISOR_CONFIG}: {e}")),
        };
        let updated = set_poll_enabled_in_toml(&text, telemetry_enabled)?;
        // Write only when changed, but the restart below is UNCONDITIONAL so a
        // previous attempt that wrote the TOML then failed to restart is retried.
        if updated != text {
            let _ = std::process::Command::new("bash")
                .args(["-c", "/root/bin/remountfs_rw"])
                .status();
            write_supervisor_toml_atomic(&updated)?;
        }
        Ok(true)
    })
    .await
    .map_err(|e| anyhow::anyhow!("sync supervisor poll task panicked: {e}"))??;

    if existed {
        // Fire the restart DETACHED so the BLE master toggle returns immediately
        // instead of blocking on the supervisor stop (up to TimeoutStopSec). The
        // TOML written above is the source of truth; Restart=on-failure recovers a
        // failed restart, and the /run lease (not restart timing) governs signing.
        tokio::spawn(async {
            if let Err(e) =
                sentryusb_shell::run("systemctl", &["restart", C6_SUPERVISOR_SERVICE]).await
            {
                tracing::error!("restart {C6_SUPERVISOR_SERVICE} (poll sync) failed: {e:#}");
            }
        });
    }
    Ok(())
}

/// Set `[poll] enabled` in the supervisor's TOML, preserving all other content
/// and comments. Creates the `[poll]` table if missing. Pure for testability.
///
/// Polling must track the telemetry source: it is ON only while the C6 is the
/// source, so the C6 and the Pi sampler never sign the shared key at once.
fn set_poll_enabled_in_toml(text: &str, enabled: bool) -> anyhow::Result<String> {
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    if doc.get("poll").and_then(|p| p.as_table_like()).is_none() {
        doc["poll"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["poll"]["enabled"] = toml_edit::value(enabled);
    Ok(doc.to_string())
}

/// Is the C6 co-processor plugged in right now?
pub fn c6_present() -> bool {
    std::path::Path::new(C6_DEVICE).exists()
}

/// Stamp the shared-key provision fields (all top-level) into the supervisor
/// TOML, preserving everything else. Pure.
fn set_provision_fields_in_toml(text: &str, vin: &str, key_path: &str) -> anyhow::Result<String> {
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    doc["vin"] = toml_edit::value(vin);
    doc["private_key_pem_path"] = toml_edit::value(key_path);
    doc["onboard_ble_disabled"] = toml_edit::value(true);
    doc["auto_provision"] = toml_edit::value(true);
    Ok(doc.to_string())
}

/// True if the Pi key file exists and parses as a private-key PEM (guards
/// arming on a missing/torn file). Absent => false.
fn pi_key_looks_valid() -> bool {
    std::fs::read_to_string(PI_KEY_PATH)
        .map(|s| pem_looks_like_private_key(&s))
        .unwrap_or(false)
}

/// PEM shape check. Requires the `-----END` footer too, so a torn write
/// (header-only) is rejected.
fn pem_looks_like_private_key(s: &str) -> bool {
    s.contains("-----BEGIN") && s.contains("PRIVATE KEY-----") && s.contains("-----END")
}

/// Write the supervisor TOML atomically (temp + fsync + rename + dir fsync). The
/// supervisor parses this only at boot and exits on a parse error, so a torn
/// write would crash-loop it and risk a failback double-signer.
fn write_supervisor_toml_atomic(contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let path = std::path::Path::new(C6_SUPERVISOR_CONFIG);
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("supervisor config path has no parent dir"))?;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all(); // best-effort durability; rename already gave atomicity
    }
    Ok(())
}

/// The VIN the sampler is configured with, if any. Empty string => not set.
fn configured_vin() -> String {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("TESLA_BLE_VIN") {
            return v.trim().to_string();
        }
    }
    String::new()
}

/// Background shared-key provisioning glue: when a C6 is plugged in and the Pi
/// holds a valid key + VIN, arm the supervisor TOML to auto-provision the C6 with
/// a copy (Pi is the custodian). No-op with no C6 / no key / no VIN / no
/// supervisor config. Idempotent — writes only on a real change; restart also
/// retried on a later tick if a prior one failed.
pub async fn ensure_shared_key_provision_config() -> anyhow::Result<()> {
    if !c6_present() {
        return Ok(());
    }
    // No key yet = fresh install (pairing generates it first); silent, not an error.
    if !std::path::Path::new(PI_KEY_PATH).exists() {
        return Ok(());
    }
    if !pi_key_looks_valid() {
        tracing::warn!("{PI_KEY_PATH} is not a valid-looking PEM — not arming C6 provision");
        return Ok(());
    }
    // Tesla VINs are exactly 17 chars; a short one would retry-provision forever.
    let vin = configured_vin();
    if vin.len() != 17 {
        if vin.is_empty() {
            tracing::warn!(
                "C6 present with a Pi key but no TESLA_BLE_VIN — cannot arm auto-provision"
            );
        } else {
            tracing::warn!(
                "C6 present but TESLA_BLE_VIN is {} chars, not 17 — not arming provision",
                vin.len()
            );
        }
        return Ok(());
    }

    // Share CONFIG_LOCK with the telemetry-source / master-switch writers so this
    // timer can't clobber their config edits (e.g. revert [poll] enabled).
    let _cfg_guard = CONFIG_LOCK.lock().await;

    let changed = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let text = match std::fs::read_to_string(C6_SUPERVISOR_CONFIG) {
            Ok(t) => t,
            // No supervisor config: C6 tooling isn't installed on this box.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(anyhow::anyhow!("read {C6_SUPERVISOR_CONFIG}: {e}")),
        };
        let updated = set_provision_fields_in_toml(&text, &vin, PI_KEY_PATH)?;
        if updated == text {
            return Ok(false);
        }
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        write_supervisor_toml_atomic(&updated)?;
        Ok(true)
    })
    .await
    .map_err(|e| anyhow::anyhow!("provision-config task panicked: {e}"))??;

    // Release the lock before the slow restart so UI config endpoints aren't stalled.
    drop(_cfg_guard);

    // Track the restart as pending so a failed one is retried next tick (else the
    // unchanged TOML means it never restarts and the supervisor keeps stale config).
    use std::sync::atomic::{AtomicBool, Ordering};
    static RESTART_PENDING: AtomicBool = AtomicBool::new(false);
    if changed {
        RESTART_PENDING.store(true, Ordering::SeqCst);
    }
    if RESTART_PENDING.load(Ordering::SeqCst) {
        tracing::info!("armed C6 shared-key auto-provision (Pi key -> C6); restarting supervisor");
        match sentryusb_shell::run("systemctl", &["restart", C6_SUPERVISOR_SERVICE]).await {
            Ok(_) => RESTART_PENDING.store(false, Ordering::SeqCst),
            // Leave the flag set; the next 30s tick retries the restart.
            Err(e) => tracing::error!(
                "restart {C6_SUPERVISOR_SERVICE} (provision arm) failed, will retry next tick: {e:#}"
            ),
        }
    }
    Ok(())
}

/// Fetch the supervisor's live status JSON (ack-confirmed device state).
async fn supervisor_status() -> Option<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{C6_SUPERVISOR_API}/api/esp32c6/status"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

/// Provisioned per the supervisor's live status (ack-confirmed), not a marker
/// file (written before the ack). Unreachable/missing => false (fail closed).
async fn c6_provisioned() -> bool {
    supervisor_status()
        .await
        .and_then(|v| v.get("provisioned").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// Is `TELEMETRY_SOURCE` currently `c6_primary`? Reads ACTIVE config lines only
/// (never the commented fallback): the daemon and ble-action treat a commented
/// `TELEMETRY_SOURCE` as sampler mode, so the status must match, otherwise the
/// UI toggle desyncs from the running daemon and can get stuck on.
fn c6_source_active() -> bool {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("TELEMETRY_SOURCE") {
            return v.trim().eq_ignore_ascii_case("c6_primary");
        }
    }
    false
}

/// GET /api/system/c6-status
pub async fn c6_status_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "present": c6_present(),
            "provisioned": c6_provisioned().await,
            "source_active": c6_source_active(),
        })),
    )
}

/// GET /api/system/telemetry-source
pub async fn telemetry_source_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let source = if c6_source_active() { "c6_primary" } else { "sampler" };
    (StatusCode::OK, Json(serde_json::json!({ "source": source })))
}

/// POST /api/system/telemetry-source writes `TELEMETRY_SOURCE=c6_primary` (use
/// the C6) or removes it (fall back to the onboard sampler), then restarts the
/// telemetry daemon so the switch takes effect immediately. The daemon treats a
/// present C6 as BLE-enabled on its own, so this endpoint does not touch
/// `BLE_ENABLED`.
pub async fn telemetry_source_set(
    State(_s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let source = match body.get("source").and_then(|v| v.as_str()) {
        Some(s) => s.trim().to_ascii_lowercase(),
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "missing or non-string `source` field",
            );
        }
    };
    let use_c6 = match source.as_str() {
        "c6_primary" => true,
        "sampler" => false,
        _ => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "`source` must be \"c6_primary\" or \"sampler\"",
            );
        }
    };

    // Refuse to select the C6 unless it is actually plugged in and provisioned,
    // so the UI can't strand telemetry on a co-processor that can't drive.
    if use_c6 && !c6_present() {
        return crate::json_error(
            StatusCode::CONFLICT,
            "no ESP32-C6 detected; plug it in first",
        );
    }
    if use_c6 && !c6_provisioned().await {
        return crate::json_error(
            StatusCode::CONFLICT,
            "the ESP32-C6 is not provisioned with a Tesla key yet",
        );
    }

    // Serialize with the telemetry master-switch endpoint (see CONFIG_LOCK).
    let _cfg_guard = CONFIG_LOCK.lock().await;

    // Supervisor polling is ON only when the C6 is the source AND telemetry is
    // enabled by the master switch. Selecting the C6 must not override a
    // telemetry-off master, otherwise the C6 would poll/sign against master-off.
    let poll_on = use_c6 && crate::ble::is_ble_enabled();

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // Prepare and validate BOTH edits in memory before writing either file,
        // so a bad/missing supervisor TOML can't leave sentryusb.conf switched
        // with polling out of sync.
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        if use_c6 {
            active.insert("TELEMETRY_SOURCE".to_string(), "c6_primary".to_string());
        } else {
            active.remove("TELEMETRY_SOURCE");
        }

        // Poll matches poll_on (C6 source + master on). Absent config is fine
        // when disabling; enabling reached here only with a present+provisioned C6.
        let supervisor_toml: Option<String> = match std::fs::read_to_string(C6_SUPERVISOR_CONFIG) {
            Ok(text) => {
                let updated = set_poll_enabled_in_toml(&text, poll_on)?; // validates TOML
                (updated != text).then_some(updated)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !use_c6 => None,
            Err(e) => return Err(anyhow::anyhow!("read {C6_SUPERVISOR_CONFIG}: {e}")),
        };

        // Commit order avoids both signers armed if the 2nd write fails: enabling
        // writes source then poll; disabling writes poll off then source.
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        let write_conf = || sentryusb_config::write_file(config_path, &active);
        let write_sup = || -> anyhow::Result<()> {
            if let Some(ref t) = supervisor_toml {
                write_supervisor_toml_atomic(t)?;
            }
            Ok(())
        };
        if use_c6 {
            write_conf()?;
            write_sup()?;
        } else {
            write_sup()?;
            write_conf()?;
        }
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to write config: {e}"),
            );
        }
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("config write task panicked: {e}"),
            );
        }
    }

    // Restart ONLY the supervisor (its [poll] is boot-read) to make it re-read the
    // change. Never restart sentryusb-telemetry: it re-reads the source each tick.
    // Fire the restart DETACHED and return immediately: a synchronous restart made
    // the toggle hang for up to TimeoutStopSec while a busy supervisor stopped,
    // which read as an unresponsive UI. The config written above is the source of
    // truth; the unit's Restart=on-failure recovers a failed restart, and the UI's
    // c6-status poll reflects the applied state. Single-signer is unaffected: who
    // may sign is governed by the /run lease (owner:c6 vs owner:sampler), not by
    // when this restart lands.
    let source = if use_c6 { "c6_primary" } else { "sampler" };
    tokio::spawn(async {
        if let Err(e) =
            sentryusb_shell::run("systemctl", &["restart", C6_SUPERVISOR_SERVICE]).await
        {
            tracing::error!(
                "restart {C6_SUPERVISOR_SERVICE} after telemetry-source change failed: {e:#}"
            );
        }
    });

    (StatusCode::OK, Json(serde_json::json!({ "source": source })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_poll_enabled_true_preserving_other_keys() {
        let src = "[poll]\ndefault_secs = 30\nenabled = false\n";
        let out = set_poll_enabled_in_toml(src, true).unwrap();
        assert!(out.contains("enabled = true"));
        assert!(out.contains("default_secs = 30"), "other keys preserved");
    }

    #[test]
    fn sets_poll_enabled_false() {
        let src = "[poll]\nenabled = true\n";
        let out = set_poll_enabled_in_toml(src, false).unwrap();
        assert!(out.contains("enabled = false"));
    }

    #[test]
    fn creates_poll_table_when_missing() {
        let src = "vin = \"5YJ\"\nbaud = 921600\n";
        let out = set_poll_enabled_in_toml(src, true).unwrap();
        assert!(out.contains("[poll]"));
        assert!(out.contains("enabled = true"));
        assert!(out.contains("vin = \"5YJ\""), "existing top-level keys preserved");
    }

    #[test]
    fn provision_fields_stamped_into_empty_config() {
        let out = set_provision_fields_in_toml("", "5YJ3TESTVIN000001", PI_KEY_PATH).unwrap();
        assert!(out.contains("vin = \"5YJ3TESTVIN000001\""));
        assert!(out.contains("private_key_pem_path = \"/root/.ble/key_private.pem\""));
        assert!(out.contains("onboard_ble_disabled = true"));
        assert!(out.contains("auto_provision = true"));
    }

    #[test]
    fn provision_fields_preserve_existing_content_and_poll_table() {
        let src = "baud = 921600\n\n[poll]\nenabled = true\ndefault_secs = 15\n";
        let out = set_provision_fields_in_toml(src, "5YJABC", "/root/.ble/key_private.pem").unwrap();
        assert!(out.contains("baud = 921600"), "existing keys preserved");
        assert!(out.contains("[poll]"), "poll table preserved");
        assert!(out.contains("enabled = true"), "poll contents untouched");
        assert!(out.contains("vin = \"5YJABC\""));
        assert!(out.contains("auto_provision = true"));
    }

    #[test]
    fn provision_fields_are_idempotent() {
        // Stamping a config that already carries the same values is a no-op, so
        // the ensure() caller never rewrites/restarts on an unchanged config.
        let once = set_provision_fields_in_toml("baud = 921600\n", "5YJABC", PI_KEY_PATH).unwrap();
        let twice = set_provision_fields_in_toml(&once, "5YJABC", PI_KEY_PATH).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn pem_shape_accepts_pkcs8_and_sec1_rejects_junk() {
        assert!(pem_looks_like_private_key(
            "-----BEGIN PRIVATE KEY-----\nMIIB...\n-----END PRIVATE KEY-----\n"
        ));
        assert!(pem_looks_like_private_key(
            "-----BEGIN EC PRIVATE KEY-----\nMHc...\n-----END EC PRIVATE KEY-----\n"
        ));
        assert!(!pem_looks_like_private_key(""), "empty rejected");
        assert!(!pem_looks_like_private_key("-----BEGIN"), "truncated header rejected");
        assert!(!pem_looks_like_private_key("random bytes"), "junk rejected");
        assert!(
            !pem_looks_like_private_key("-----BEGIN PUBLIC KEY-----\n-----END PUBLIC KEY-----\n"),
            "public key rejected"
        );
        assert!(
            !pem_looks_like_private_key("-----BEGIN PRIVATE KEY-----\nMIIB"),
            "header without footer (torn generate_keypair write) rejected"
        );
    }

    #[test]
    fn provision_fields_update_stale_key_path() {
        // A config pointing at the old placeholder key path gets corrected.
        let src = "vin = \"5YJABC\"\nprivate_key_pem_path = \"/mutable/tesla_key.pem\"\n";
        let out = set_provision_fields_in_toml(src, "5YJABC", PI_KEY_PATH).unwrap();
        assert!(out.contains("private_key_pem_path = \"/root/.ble/key_private.pem\""));
        assert!(!out.contains("/mutable/tesla_key.pem"), "old path replaced");
        assert_ne!(out, src, "a stale key path is a real change");
    }
}
