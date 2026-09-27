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
            std::fs::write(C6_SUPERVISOR_CONFIG, updated)?;
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

/// Fetch the supervisor's live status JSON (ack-confirmed device state).
async fn supervisor_status() -> Option<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{C6_SUPERVISOR_API}/api/coprocessor/status"))
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
                std::fs::write(C6_SUPERVISOR_CONFIG, t)?;
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
}
