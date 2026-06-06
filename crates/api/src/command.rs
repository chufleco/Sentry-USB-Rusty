//! Unified command surface.
//!
//! `Command` is the single typed description of an action the device
//! can perform on behalf of a user. Today exactly one HTTP endpoint
//! (`POST /api/command`) dispatches from it; the planned BLE broker will
//! deserialize the same enum from its own transport and call the same
//! [`execute`] function. Because both transports share one enum and one
//! dispatcher, web and BLE can never offer a different set of actions or
//! interpret a parameter differently — parity is structural, not a thing
//! anyone has to remember to keep in sync.
//!
//! The dispatcher is deliberately thin: it never reimplements business
//! logic. Each arm either calls the existing handler/manager that already
//! owns that behavior, or — for the BLE-domain vehicle actions that have
//! no standalone handler — spawns `sentryusb-ble-action` exactly the way
//! [`crate::keep_accessory`] does (route through the telemetry daemon's
//! warm BLE session over IPC rather than grabbing the radio directly).
//!
//! The whole surface is gated by the master experimental flag. With the
//! flag off, [`command_endpoint`] returns 404 and does nothing, so a
//! normal install is byte-for-byte unchanged.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::flags::experimental_enabled;
use crate::router::AppState;

/// `sentryusb-ble-action` routes a verb through the telemetry daemon's
/// warm BLE session (IPC), avoiding a competing radio grab. 30s covers a
/// cold direct-BLE fallback if the daemon happens to be down — same
/// budget the keep-accessory handler uses.
const BLE_ACTION_BIN: &str = "/root/bin/sentryusb-ble-action";
const BLE_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Every action the device exposes to a transport. Internally tagged on
/// `action` with snake_case names, so a request body looks like
/// `{"action":"keep_accessory","on":true}` and a parameterless one like
/// `{"action":"reboot"}`. Typed fields throughout — no stringly-typed
/// params — so an invalid shape fails at deserialization rather than
/// deep inside a handler.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Command {
    /// Reboot the device.
    Reboot,
    /// Power the device off.
    Shutdown,
    /// Toggle the USB mass-storage gadget on/off (whichever it isn't).
    ToggleDrives,
    /// Force an archive/sync cycle to start now.
    TriggerSync,
    /// Keep the car's 12V accessory rail powered (`on`) or release it.
    KeepAccessory { on: bool },
    /// Ask the telemetry sampler to do a full state poll immediately.
    BleForcePoll,
    /// Wake the vehicle over BLE.
    VehicleWake,
    /// Turn the vehicle's Sentry mode on/off over BLE.
    VehicleSentry { on: bool },
    /// Open (`open: true`) or close the charge port over BLE.
    VehicleChargePort { open: bool },
    /// Start a keep-awake session. `mode` is the caller's label and
    /// defaults to "manual" when omitted (matching the HTTP route);
    /// `duration_min` defaults to 10 when omitted.
    KeepAwakeStart {
        mode: Option<String>,
        duration_min: Option<u32>,
    },
    /// Stop / cancel any active keep-awake session.
    KeepAwakeStop,
    /// Enable Away Mode (WiFi AP) for `duration_min` minutes. Required —
    /// Away Mode has no safe default duration and the handler rejects 0,
    /// so the type makes the always-failing "omitted" shape impossible.
    AwayModeEnable { duration_min: u32 },
    /// Disable Away Mode.
    AwayModeDisable,
}

/// Uniform result of dispatching a [`Command`]. `ok` is the
/// success/failure bit a transport keys off; `message` is a short
/// human-readable note for logs and UI toasts.
#[derive(Debug, Serialize)]
pub struct CommandResponse {
    pub ok: bool,
    pub message: String,
}

impl CommandResponse {
    fn ok(message: impl Into<String>) -> Self {
        CommandResponse { ok: true, message: message.into() }
    }

    fn err(message: impl Into<String>) -> Self {
        CommandResponse { ok: false, message: message.into() }
    }
}

/// Spawn `sentryusb-ble-action <verb>` and map the result to a
/// [`CommandResponse`]. Shared by keep-accessory and every vehicle
/// action — they differ only in the verb and the log/response label.
async fn run_ble_action(verb: &str, label: &str) -> CommandResponse {
    info!("[command] ble-action: {}", verb);
    match sentryusb_shell::run_with_timeout(BLE_ACTION_TIMEOUT, BLE_ACTION_BIN, &[verb]).await {
        Ok(_) => {
            info!("[command] {} ok", verb);
            CommandResponse::ok(format!("{label} ok"))
        }
        Err(e) => {
            warn!("[command] {} failed: {}", verb, e);
            CommandResponse::err(format!("{label} failed: {e}"))
        }
    }
}

/// Dispatch a single [`Command`]. The match is exhaustive: adding a
/// variant forces a new arm here, which is exactly the property that
/// keeps the transports honest. Each arm delegates to the code that
/// already owns the behavior and never reimplements it.
///
/// Callers are responsible for the experimental-flag gate
/// ([`command_endpoint`] does it); `execute` assumes it has been
/// cleared so the future BLE broker can apply its own policy.
pub async fn execute(state: &AppState, cmd: Command) -> CommandResponse {
    match cmd {
        // ── System: delegate to the existing handlers verbatim. These
        // spawn the same one-line shell op (or gadget toggle) the web
        // routes already use, so behavior is identical by construction. ──
        Command::Reboot => {
            // The handler returns a 200 envelope we don't forward; the
            // reboot itself is spawned, so this only acknowledges intent.
            let _ = crate::system::reboot(State(state.clone())).await;
            CommandResponse::ok("reboot requested")
        }
        Command::Shutdown => {
            let _ = crate::system::shutdown(State(state.clone())).await;
            CommandResponse::ok("shutdown requested")
        }
        Command::ToggleDrives => {
            let (status, _) =
                crate::system::toggle_drives(State(state.clone()), String::new()).await;
            if status.is_success() {
                CommandResponse::ok("drives toggled")
            } else {
                CommandResponse::err("drive toggle failed")
            }
        }
        Command::TriggerSync => {
            let _ = crate::system::trigger_sync(State(state.clone())).await;
            CommandResponse::ok("sync triggered")
        }

        // ── BLE-domain actions: spawn sentryusb-ble-action, exactly as
        // keep_accessory.rs does. keep-accessory is the canonical
        // "both transports" action; the vehicle verbs follow the same
        // shape and so are reachable identically over web and BLE. ──
        Command::KeepAccessory { on } => {
            let verb = if on { "keep-accessory-on" } else { "keep-accessory-off" };
            run_ble_action(verb, "keep-accessory").await
        }
        Command::VehicleWake => run_ble_action("wake", "vehicle wake").await,
        Command::VehicleSentry { on } => {
            let verb = if on { "sentry-on" } else { "sentry-off" };
            run_ble_action(verb, "vehicle sentry").await
        }
        Command::VehicleChargePort { open } => {
            let verb = if open { "charge-port-open" } else { "charge-port-close" };
            run_ble_action(verb, "vehicle charge-port").await
        }

        // ── BLE force-poll: nudge the sampler with SIGUSR1. Best-effort,
        // same one-liner as ble::ble_force_poll — we don't fork
        // tesla-control here (it would fight the sampler for the radio).
        // Async (not blocking std::process) so the dispatcher stays
        // cancellation-safe for the future BLE broker. ──
        Command::BleForcePoll => {
            match sentryusb_shell::run_with_timeout(
                Duration::from_secs(5),
                "systemctl",
                &["kill", "-s", "SIGUSR1", "sentryusb-telemetry"],
            )
            .await
            {
                Ok(_) => CommandResponse::ok("state poll queued"),
                Err(e) => CommandResponse::err(format!("force-poll signal failed: {e}")),
            }
        }

        // ── Keep-awake: the manager exposes clean async start/stop on
        // shared state, so delegation is direct. Surface the manager's
        // real resulting state (a start can land Pending vs Active) so a
        // transport reports what actually happened, not just "started". ──
        Command::KeepAwakeStart { mode, duration_min } => {
            let duration = Duration::from_secs(u64::from(duration_min.unwrap_or(10)) * 60)
                .max(Duration::from_secs(1));
            state
                .keep_awake
                .start(mode.unwrap_or_else(|| "manual".to_string()), duration)
                .await;
            let st = state.keep_awake.status().await;
            let label = st.get("state").and_then(|s| s.as_str()).unwrap_or("started");
            CommandResponse::ok(format!("keep-awake {label}"))
        }
        Command::KeepAwakeStop => {
            state.keep_awake.stop().await;
            CommandResponse::ok("keep-awake stopped")
        }

        // ── Away mode: the enable/disable handlers own the AP-profile
        // precondition check, watcher spawning, and flag-file persistence.
        // Reuse them verbatim by handing each the JSON body it expects,
        // rather than duplicating that stateful logic here. ──
        Command::AwayModeEnable { duration_min } => {
            let body = serde_json::json!({ "duration_min": duration_min }).to_string();
            let (status, Json(v)) = crate::away_mode::enable(State(state.clone()), body).await;
            if status.is_success() {
                CommandResponse::ok("away mode enabled")
            } else {
                let msg = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("away mode enable failed");
                CommandResponse::err(msg.to_string())
            }
        }
        Command::AwayModeDisable => {
            let _ = crate::away_mode::disable(State(state.clone())).await;
            CommandResponse::ok("away mode disabled")
        }
    }
}

/// POST /api/command — body is a tagged [`Command`].
///
/// Gated behind the master experimental flag, checked fresh per request:
/// when it's off the endpoint returns 404 and does nothing. When on, it
/// deserializes the command and dispatches it, returning a
/// [`CommandResponse`] as JSON — 200 on success, 502 when the underlying
/// action failed, 400 when the body isn't a valid command. The body is
/// taken untyped and converted here (rather than via a `Json<Command>`
/// extractor) so a bad command yields the same `{ok,message}` envelope
/// every other response uses, instead of axum's raw 422.
pub async fn command_endpoint(
    State(state): State<AppState>,
    Json(raw): Json<serde_json::Value>,
) -> axum::response::Response {
    if !experimental_enabled() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "experimental command surface disabled" })),
        )
            .into_response();
    }

    let cmd: Command = match serde_json::from_value(raw) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(CommandResponse::err(format!("invalid command: {e}"))),
            )
                .into_response();
        }
    };

    let resp = execute(&state, cmd).await;
    let status = if resp.ok { StatusCode::OK } else { StatusCode::BAD_GATEWAY };
    (status, Json(resp)).into_response()
}
