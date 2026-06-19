//! Travel Mode: keep the USB gadget presented to the car at all times.
//!
//! On a road trip the Pi reaches the archive server through an always-on
//! travel-router VPN, which breaks the normal "archive when home, keep
//! recording when driving" assumption. Travel Mode lets the background
//! archive loop keep snapshotting and uploading footage, but skips every
//! step that would disconnect the USB gadget from the car so Sentry/Dashcam
//! recording stays continuous.
//!
//! The flag is persisted as `TRAVEL_MODE_ENABLED` (`yes`/`no`) in
//! sentryusb.conf — the same pattern as `AWAY_MODE_AUTO_ENABLED`. The
//! archiveloop bash script reads it fresh each cycle (`travel_mode_active`),
//! so toggling here takes effect without restarting the daemon. This module
//! is the secret-menu toggle's only job: read + persist that one boolean.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;

/// Parse `TRAVEL_MODE_ENABLED` from a specific config file. Split out from
/// [`read_travel_enabled`] so it can be unit-tested against a temp conf.
fn read_travel_enabled_at(config_path: &str) -> bool {
    if let Ok((active, commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = sentryusb_config::get_config_value(&active, &commented, "TRAVEL_MODE_ENABLED") {
            return matches!(v.trim(), "yes" | "true" | "1");
        }
    }
    false
}

/// Whether Travel Mode is enabled in the active config file.
fn read_travel_enabled() -> bool {
    read_travel_enabled_at(sentryusb_config::find_config_path())
}

/// GET /api/travel-mode/status → `{"enabled": bool}`.
pub async fn status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "enabled": read_travel_enabled() })),
    )
}

#[derive(Deserialize)]
pub struct TravelBody {
    enabled: bool,
}

/// POST /api/travel-mode — body `{"enabled": bool}`.
///
/// Persists `TRAVEL_MODE_ENABLED` (`yes`/`no`) so the setting survives reboot
/// and the archiveloop picks it up on its next cycle. RO root → flip rw for
/// the write, same pattern as `away_mode::set_mode`.
pub async fn set(
    State(_s): State<AppState>,
    Json(body): Json<TravelBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let want = body.enabled;

    let persist = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert(
            "TRAVEL_MODE_ENABLED".to_string(),
            if want { "yes" } else { "no" }.to_string(),
        );
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match persist {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "enabled": want })),
        ),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write failed: {e}"),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config task failed: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_conf(contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sentryusb-travel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sentryusb.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn reads_yes_as_enabled() {
        let p = temp_conf("export TRAVEL_MODE_ENABLED=yes\n");
        assert!(read_travel_enabled_at(p.to_str().unwrap()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_no_as_disabled() {
        let p = temp_conf("export TRAVEL_MODE_ENABLED=no\n");
        assert!(!read_travel_enabled_at(p.to_str().unwrap()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_unset_as_disabled() {
        let p = temp_conf("export SOMETHING_ELSE=1\n");
        assert!(!read_travel_enabled_at(p.to_str().unwrap()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn quoted_value_is_parsed() {
        // sentryusb_config::unquote handles surrounding quotes.
        let p = temp_conf("export TRAVEL_MODE_ENABLED='yes'\n");
        assert!(read_travel_enabled_at(p.to_str().unwrap()));
        let _ = std::fs::remove_file(&p);
    }
}
