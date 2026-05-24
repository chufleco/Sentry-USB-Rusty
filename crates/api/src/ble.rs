//! BLE management endpoints: enable/disable toggle, VIN config,
//! connection status, lazy binary install.
//!
//! Companion to the pairing handshake in `system.rs` (`ble_pair` /
//! `ble_status`). These endpoints back the always-visible BLE card in
//! the Device settings tab — the user can flip the master toggle,
//! enter or update the VIN, see a live connection indicator, and
//! trigger an on-demand install of `tesla-control` / `tesla-keygen` if
//! they didn't enable BLE during initial setup.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;

/// Unix-second timestamp of the most recent successful `tesla-control`
/// invocation against the car (any subcommand). Used by `ble_connected`
/// to render a live indicator in the pair card. 0 at process start.
///
/// Writers: `system::ble_status`'s `session-info` probe and (later)
/// the telemetry sampler daemon's per-sample success path.
pub static LAST_BLE_SUCCESS_TS: AtomicI64 = AtomicI64::new(0);

/// Mark a successful tesla-control round-trip. Cheap, lock-free —
/// safe to call from any code path that just got a non-error response
/// from the car.
pub fn mark_ble_success() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    LAST_BLE_SUCCESS_TS.store(now, Ordering::Relaxed);
}

/// Whether Tesla BLE is currently enabled by the user.
///
/// Resolution order:
///   1. Explicit `BLE_ENABLED=yes|no|true|false|1|0` in the config —
///      always wins. Set by the settings toggle.
///   2. Implicit "yes" if the user previously configured BLE
///      (`TESLA_BLE_VIN` present in config), so existing installs
///      don't silently lose BLE on upgrade.
///   3. Implicit "yes" if the paired marker exists.
///   4. Default "no" for fresh installs — user opts in via settings.
pub fn is_ble_enabled() -> bool {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) =
            sentryusb_config::get_config_value(&active, &commented, "BLE_ENABLED")
        {
            return matches!(v.as_str(), "yes" | "true" | "1");
        }
        if active.contains_key("TESLA_BLE_VIN") {
            return true;
        }
    }
    Path::new("/root/.ble/paired").exists()
}

/// GET /api/system/ble-enabled
pub async fn ble_enabled_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "enabled": is_ble_enabled() })),
    )
}

/// POST /api/system/ble-enabled
///
/// Body: `{"enabled": true|false}`. Writes `BLE_ENABLED=yes|no` to
/// the config file. The change is eventually consistent: the
/// keep-awake script and (future) telemetry sampler re-read the
/// config on each loop iteration, so no in-flight processes need to
/// be force-killed here.
pub async fn ble_enabled_set(
    State(_s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let enabled = match body.get("enabled").and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "missing or non-bool `enabled` field",
            );
        }
    };

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert(
            "BLE_ENABLED".to_string(),
            if enabled { "yes" } else { "no" }.to_string(),
        );
        // The Pi's root partition is normally mounted read-only;
        // `remountfs_rw` flips it to rw for the duration of the
        // write. Match the existing pattern from
        // notifications::auto_enable_mobile_push_in_config.
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({ "enabled": enabled })),
        ),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write config: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write task panicked: {}", e),
        ),
    }
}

/// POST /api/system/ble-vin
///
/// Body: `{"vin": "5YJ3E1EA4LF..."}`. Writes `TESLA_BLE_VIN` to the
/// config file after lightweight validation. The setup wizard used to
/// be the only place to collect this; now the always-visible BLE card
/// in Device settings lets a user enter (or update) the VIN at any
/// time.
///
/// Validation is intentionally permissive — exact 17-char Tesla VINs
/// are required, but we don't enforce the country/model digit ranges
/// since Tesla can extend the VIN format on future vehicles. The
/// pairing call itself is the real validator: it will refuse a VIN
/// the car doesn't recognize.
pub async fn ble_vin_set(
    State(_s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let vin_raw = match body.get("vin").and_then(|v| v.as_str()) {
        Some(s) => s.trim().to_string(),
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "missing or non-string `vin` field",
            );
        }
    };

    let vin = vin_raw.to_uppercase();
    if vin.len() != 17 || !vin.chars().all(|c| c.is_ascii_alphanumeric()) {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "VIN must be exactly 17 alphanumeric characters",
        );
    }

    let vin_for_write = vin.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert("TESLA_BLE_VIN".to_string(), vin_for_write);
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({ "vin": vin }))),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write config: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write task panicked: {}", e),
        ),
    }
}

/// GET /api/system/ble-connected
///
/// Reports the unix-second timestamp of the most recent successful
/// BLE round-trip, plus a derived `seconds_ago`. The timestamp is
/// taken as the maximum of two sources:
///   * `LAST_BLE_SUCCESS_TS` — webui process's own probes (clicking
///     pair, settings-page session-info polls).
///   * `MAX(ts) FROM telemetry_samples` — the out-of-process sampler
///     daemon's autonomous activity.
/// Without the second source the indicator would say "Disconnected"
/// while the sampler is happily writing rows every 15 s.
///
/// Callers (the BlePairButton card) interpret the freshness:
///   * `seconds_ago < 60`  → "Connected"
///   * `< 600`            → "Last seen Ns ago"
///   * `>= 600` or null   → "Disconnected"
///
/// `sample_count_10min` lets the UI render a "5 samples / 10m" hint
/// so the user can tell that data is flowing, not just that the radio
/// pinged once a long time ago.
pub async fn ble_connected(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let probe_ts = LAST_BLE_SUCCESS_TS.load(Ordering::Relaxed);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let since = now - 600;

    let store = s.drives.store.clone();
    let (sampler_ts, sample_count_10min) = tokio::task::spawn_blocking(move || {
        store.with_locked_conn(|conn| {
            let max_ts: Option<i64> = conn
                .query_row(
                    "SELECT MAX(ts) FROM telemetry_samples",
                    [],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM telemetry_samples WHERE ts >= ?1",
                    (since,),
                    |r| r.get(0),
                )
                .unwrap_or(0);
            (max_ts.unwrap_or(0), count)
        })
    })
    .await
    .unwrap_or((0, 0));

    let last = probe_ts.max(sampler_ts);
    let seconds_ago = if last == 0 {
        None
    } else {
        Some((now - last).max(0))
    };

    // Surface "why the gap" context so the UI can explain a stale
    // connection instead of just saying "Disconnected". The keep-awake
    // nudge claims the BLE radio (writes "keep_awake" into
    // /tmp/ble_radio_owner) — typically because archiveloop is in the
    // middle of an archive cycle and is poking the car to prevent USB
    // power-off. While that owner is set, the sampler can't take new
    // samples, so the freshness pill should say "paused" not "broken".
    let radio_owner = read_radio_owner();
    let archiving = crate::drives_handler::is_archiving();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "last_success_ts": last,
            "seconds_ago": seconds_ago,
            "sample_count_10min": sample_count_10min,
            "radio_owner": radio_owner,
            "archiving": archiving,
        })),
    )
}

/// Read the first line of `/tmp/ble_radio_owner` — the owner-name
/// string written by `awake_start` ("keep_awake") or the telemetry
/// sampler ("telemetry"). Returns None when the file is missing
/// (no one holds the radio).
fn read_radio_owner() -> Option<String> {
    let contents = std::fs::read_to_string("/tmp/ble_radio_owner").ok()?;
    let first = contents.lines().next()?.trim();
    if first.is_empty() { None } else { Some(first.to_string()) }
}

/// GET /api/system/ble-latest-sample
///
/// Returns the most recent row from `telemetry_samples` so the UI can
/// show the user exactly what the Pi is pulling from the car right
/// now. Null fields just stay null in the response — e.g. a
/// `body_controller` sample only carries `ts` and `source`.
///
/// Used by the "Show output" panel on the BLE pair card. Polled
/// every 5 s while the panel is open.
pub async fn ble_latest_sample(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = s.drives.store.clone();
    let row = tokio::task::spawn_blocking(move || {
        store.with_locked_conn(|conn| {
            // battery_temp_c intentionally not selected — Tesla doesn't
            // expose battery cell temp via the state API, so the column
            // is always NULL. Including it in the response would just
            // advertise a field that's never populated.
            // Pull the most recent sample row plus the latest non-null
            // TPMS reading per tire (which may live in an older sample
            // than the most-recent one — tires update slowly).
            let latest = conn
                .query_row(
                    "SELECT ts, battery_pct, interior_temp_c, \
                            exterior_temp_c, hvac_on, source, odometer_mi, location_name \
                     FROM telemetry_samples \
                     ORDER BY ts DESC LIMIT 1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<f64>>(1)?,
                            r.get::<_, Option<f64>>(2)?,
                            r.get::<_, Option<f64>>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, Option<f64>>(6)?,
                            r.get::<_, Option<String>>(7)?,
                        ))
                    },
                )
                .ok();
            // Slow-changing fields (TPMS) sample less often than every
            // cycle. Pull the most-recent non-null per field separately
            // so the live panel can show what we know even when the
            // latest sample didn't include them.
            let latest_col_f64 = |col: &str| -> Option<f64> {
                conn.query_row(
                    &format!(
                        "SELECT {col} FROM telemetry_samples \
                         WHERE {col} IS NOT NULL \
                         ORDER BY ts DESC LIMIT 1"
                    ),
                    [],
                    |r| r.get(0),
                )
                .ok()
            };
            latest.map(|row| {
                (
                    row,
                    (
                        latest_col_f64("tire_fl_psi"),
                        latest_col_f64("tire_fr_psi"),
                        latest_col_f64("tire_rl_psi"),
                        latest_col_f64("tire_rr_psi"),
                    ),
                )
            })
        })
    })
    .await
    .ok()
    .flatten();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    match row {
        Some((
            (ts, battery_pct, interior_temp_c, exterior_temp_c, hvac_on, source, odometer_mi, location_name),
            (tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi),
        )) => {
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ts": ts,
                    "seconds_ago": (now - ts).max(0),
                    "battery_pct": battery_pct,
                    "interior_temp_c": interior_temp_c,
                    "exterior_temp_c": exterior_temp_c,
                    "hvac_on": hvac_on.map(|v| v != 0),
                    "tire_fl_psi": tire_fl_psi,
                    "tire_fr_psi": tire_fr_psi,
                    "tire_rl_psi": tire_rl_psi,
                    "tire_rr_psi": tire_rr_psi,
                    "odometer_mi": odometer_mi,
                    "location_name": location_name,
                    "source": source,
                })),
            )
        }
        None => (
            StatusCode::OK,
            Json(serde_json::json!({ "ts": null })),
        ),
    }
}

/// GET /api/system/ble-diagnostics
///
/// Returns the last N lines from the telemetry sampler's systemd
/// journal, filtered to the diagnostic lines emitted by `sample.rs`
/// (`state-poll`, `body-controller poll`, `subcommand failed`).
/// Used by the "Diagnostics" panel on the BLE pair card so the user
/// can see which tesla-control subcommand is timing out without
/// SSH-ing to the Pi.
///
/// Why journalctl rather than a custom log file: systemd is already
/// capturing every line the daemon emits at INFO+ level, with
/// rotation handled by journald's storage settings. Adding a second
/// log destination would just duplicate that, and journald handles
/// the rotation/size-cap concerns for us.
pub async fn ble_diagnostics(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let output = tokio::task::spawn_blocking(|| {
        std::process::Command::new("journalctl")
            .args([
                "-u",
                "sentryusb-tesla-telemetry",
                "-n",
                "200",
                "--no-pager",
                "--output=short-iso",
            ])
            .output()
    })
    .await;

    let raw = match output {
        Ok(Ok(out)) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        Ok(Ok(out)) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!(
                    "journalctl failed (exit {}): {}",
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr)
                ),
            );
        }
        Ok(Err(e)) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to spawn journalctl: {e}"),
            );
        }
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("diagnostics task panicked: {e}"),
            );
        }
    };

    // Filter to the diagnostic lines we care about so the UI panel
    // stays readable. Everything else (startup messages, periodic
    // info chatter) is still in the journal if needed via SSH.
    //
    // Patterns chosen to cover the four classes of "why isn't the
    // sampler updating" that come up in practice:
    //   1. Subcommand-level: `state-poll`, `body-controller poll`,
    //      `subcommand failed` — per-tesla-control invocation
    //      timing + error text.
    //   2. Sample-level: `sample_state failed`,
    //      `sample_body_controller failed` — both subcommands
    //      succeeded but the whole sample didn't persist.
    //   3. Radio coordination: `radio held by` (keep_awake won the
    //      lock), `failed to acquire radio lock`,
    //      `failed to release radio lock` — these explain "values
    //      are Xs old" gaps that aren't tesla-control's fault.
    //   4. Lifecycle: phase transitions, presence flips, config
    //      reloads, DB insert failures.
    let lines: Vec<&str> = raw
        .lines()
        .filter(|l| {
            l.contains("state-poll")
                || l.contains("body-controller poll")
                || l.contains("subcommand failed")
                || l.contains("sample_state failed")
                || l.contains("sample_body_controller failed")
                || l.contains("state drive probe")
                || l.contains("dropping to body-controller polling")
                || l.contains("resuming full state polls")
                || l.contains("radio held by")
                || l.contains("failed to acquire radio lock")
                || l.contains("failed to release radio lock")
                || l.contains("failed to insert telemetry sample")
                || l.contains("failed to load BLE config")
                || l.contains("BLE disabled in settings")
                || l.contains("user_presence flipped")
        })
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "lines": lines,
            "total_journal_lines": raw.lines().count(),
        })),
    )
}

/// POST /api/system/ble-install
///
/// Idempotent lazy install of `tesla-control` and `tesla-keygen`,
/// plus first-time keypair generation. Used when the user opts into
/// BLE from the settings page on a Pi that didn't have BLE selected
/// during initial setup.
///
/// Runs in a background task so the HTTP response can return
/// immediately. Progress lands on the `ble_install_status` WebSocket
/// topic so the BlePairButton card can show a spinner with the
/// current step.
pub async fn ble_install(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let already_installed = std::path::Path::new("/root/bin/tesla-control").exists()
        && std::path::Path::new("/root/bin/tesla-keygen").exists()
        && std::path::Path::new("/root/.ble/key_private.pem").exists();

    let hub = s.hub.clone();
    tokio::spawn(async move {
        if already_installed {
            hub.broadcast(
                "ble_install_status",
                &serde_json::json!({ "status": "done", "already_installed": true }),
            );
            return;
        }
        hub.broadcast(
            "ble_install_status",
            &serde_json::json!({ "status": "installing" }),
        );
        let hub_progress = hub.clone();
        let result =
            sentryusb_setup::archive::install_tesla_ble_binaries(move |msg| {
                hub_progress.broadcast(
                    "ble_install_status",
                    &serde_json::json!({ "status": "progress", "message": msg }),
                );
            })
            .await;

        match result {
            Ok(()) => {
                hub.broadcast(
                    "ble_install_status",
                    &serde_json::json!({ "status": "done", "already_installed": false }),
                );
            }
            Err(e) => {
                hub.broadcast(
                    "ble_install_status",
                    &serde_json::json!({ "status": "error", "error": e.to_string() }),
                );
            }
        }
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "started",
            "already_installed": already_installed,
        })),
    )
}
