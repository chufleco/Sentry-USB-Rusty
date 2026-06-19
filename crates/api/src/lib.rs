pub mod auth;
pub mod ble;
pub mod ble_debug;
pub mod router;
pub mod drives_handler;
pub mod status;
pub mod system;
pub mod files;
pub mod lock_chime;
pub mod terminal;
pub mod keep_awake;
pub mod away_mode;
pub mod travel_mode;
pub mod notifications;
pub mod notification_center;
pub mod setup;
pub mod backup;
pub mod update;
pub mod support;
pub mod community;
pub mod healthcheck;
pub mod clips;
pub mod preferences;
pub mod memory;
pub mod logs;
pub mod devices;
pub mod cloud;
pub mod snapshots;
pub mod keep_accessory;
pub mod charging;
pub mod storage_repair;

pub use auth::{AuthState, init_auth};
pub use router::build_router;

use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;

/// Standard JSON response helper.
pub fn json_response<T: Serialize>(status: StatusCode, data: T) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::to_value(data).unwrap_or_default()))
}

/// Standard error response.
pub fn json_error(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

/// Standard success response.
pub fn json_ok() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(serde_json::json!({"success": true})))
}

/// Process-wide shared `reqwest` client for the outbound community /
/// notification proxies.
///
/// Previously every proxied request built its own `reqwest::Client`,
/// spinning up a fresh rustls/TLS stack and connection pool with no
/// keep-alive reuse. One shared client pools connections to the two
/// upstreams across requests. It carries **no** request-level timeout —
/// the per-endpoint values (10s / 15s / 30s …) are preserved by each
/// call site via `.timeout(..)` on the request builder, which overrides
/// the client default. The 120s builder timeout is only a backstop so a
/// site that forgets one can't hang a connection forever.
pub fn http_client() -> &'static reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}
