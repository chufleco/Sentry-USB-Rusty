//! Page-load aggregate endpoint.
//!
//! On a cold dashboard paint the web client fires a fistful of small,
//! independent reads (status, storage breakdown, drive stats, drive
//! processing status, setup config, update status). Each is cheap, but
//! the round-trip count dominates first-paint latency on a high-latency
//! link. `GET /api/overview` collapses that burst into one request: it
//! invokes the SAME handlers the singleton endpoints use, concurrently,
//! and nests each one's response body VERBATIM under a named key.
//!
//! Two properties are load-bearing:
//!
//!  * **No drift.** This endpoint never hand-rolls a payload shape. Five
//!    parts nest the byte-for-byte JSON the real singleton handler returns;
//!    the sixth (config) calls the SAME `setup::merged_config_map` the
//!    `/api/setup/config` handler uses. So the aggregate can never disagree
//!    with the per-tile endpoint a poller later refreshes from.
//!
//!  * **Per-part error isolation.** Each sub-handler runs on its own task;
//!    a non-2xx status OR a panic nulls only that key and records the
//!    failure under `"errors"`, while the envelope itself stays `200`. A
//!    single flaky (or panicking) tile can never blank the whole dashboard.
//!
//! The whole surface is gated by the master experimental flag. With the
//! flag off [`get_overview`] returns 404 and touches nothing, so a normal
//! install is byte-for-byte unchanged. The flag is read fresh per request
//! (via `crate::flags::experimental_enabled`), so toggling it needs no
//! daemon restart.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::flags::experimental_enabled;
use crate::router::AppState;

/// One sub-handler's outcome, reduced to what the envelope needs: either
/// the body to nest, or an error to record. We deliberately key the
/// decision off the HTTP status the real handler chose, so "what counts
/// as failure" is defined in exactly one place (the singleton handler)
/// and never duplicated here.
struct Part {
    body: Value,
    error: Option<Value>,
}

/// Fold a `(StatusCode, Json<Value>)` — the shape every aggregated
/// handler returns except setup-config — into a [`Part`]. A 2xx status
/// yields the body for nesting; anything else yields `null` plus an error
/// record carrying the numeric status and the handler's own error body
/// (which is already `{"error": ...}` by convention), so the client can
/// surface *why* a tile is missing without us inventing a message.
fn part_from_tuple(out: (StatusCode, Json<Value>)) -> Part {
    let (status, Json(body)) = out;
    if status.is_success() {
        Part { body, error: None }
    } else {
        Part {
            body: Value::Null,
            error: Some(json!({
                "status": status.as_u16(),
                "body": body,
            })),
        }
    }
}

/// Await one spawned sub-handler task and reduce it to a [`Part`]. A
/// `JoinError` means the handler panicked (or was cancelled); that nulls
/// only this slot and records a 500-shaped error, so a single panicking
/// tile can never take down the whole envelope.
async fn join_part(handle: tokio::task::JoinHandle<(StatusCode, Json<Value>)>) -> Part {
    match handle.await {
        Ok(out) => part_from_tuple(out),
        Err(e) => Part {
            body: Value::Null,
            error: Some(json!({
                "status": StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                "body": { "error": format!("handler did not complete: {e}") },
            })),
        },
    }
}

/// `GET /api/overview` — one-shot page-load aggregate.
///
/// Flag off → 404 (same shape as the command surface). Flag on → invokes
/// the existing status / storage / drive-stats / drive-status / config /
/// update handlers concurrently and returns a single envelope nesting
/// each body verbatim under `status`, `storageBreakdown`, `driveStats`,
/// `driveStatus`, `config`, `updateStatus`, with any per-part failures
/// collected under `errors`.
pub async fn get_overview(State(state): State<AppState>) -> impl IntoResponse {
    // Read the flag fresh, exactly like the command surface, so a normal
    // install never exposes this endpoint and toggling needs no restart.
    if !experimental_enabled() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "experimental overview disabled" })),
        )
            .into_response();
    }

    // Fan out the five tuple-returning handlers concurrently. Each is
    // spawned onto its own task so a PANIC in one sub-handler (e.g. a
    // poisoned mutex) nulls only that slot via `join_part` rather than
    // unwinding the whole aggregate — the per-part isolation guarantee
    // has to hold for panics, not just non-2xx returns. Each takes
    // `State<AppState>` by value; AppState is `Clone` and cheap (Arcs and
    // handles). We call the real handlers, never reimplement them, so the
    // aggregate can't drift from the singletons.
    let (status, storage, drive_stats, drive_status, update_status) = tokio::join!(
        join_part(tokio::spawn(crate::status::get_status(State(state.clone())))),
        join_part(tokio::spawn(crate::status::get_storage_breakdown(State(state.clone())))),
        join_part(tokio::spawn(crate::drives_handler::drive_stats(State(state.clone())))),
        join_part(tokio::spawn(crate::drives_handler::processing_status(State(state.clone())))),
        join_part(tokio::spawn(crate::update::get_update_status(State(state.clone())))),
    );

    // Setup config: the real `get_setup_config` handler returns an opaque
    // `axum::response::Response` (it attaches its own Cache-Control), so
    // pulling a clean JSON body back out of it is awkward. Per the design
    // note, re-derive the identical merged shape directly from the same
    // source (`find_config_path` + `parse_file`) — same data, same
    // structure, no Response disassembly.
    let config = config_part();

    // Assemble. Any part that failed is null in its slot and recorded in
    // `errors`; the envelope status stays 200 regardless.
    let mut errors = serde_json::Map::new();
    for (key, part) in [
        ("status", &status),
        ("storageBreakdown", &storage),
        ("driveStats", &drive_stats),
        ("driveStatus", &drive_status),
        ("config", &config),
        ("updateStatus", &update_status),
    ] {
        if let Some(err) = &part.error {
            errors.insert(key.to_string(), err.clone());
        }
    }

    let envelope = json!({
        "status":           status.body,
        "storageBreakdown": storage.body,
        "driveStats":       drive_stats.body,
        "driveStatus":      drive_status.body,
        "config":           config.body,
        "updateStatus":     update_status.body,
        "errors":           Value::Object(errors),
    });

    // Short private cache with stale-while-revalidate: a second navigation
    // within 2s reuses the payload outright; for the next 8s a stale copy
    // can be shown while a fresh fetch happens in the background. Matches
    // the cadence of the tiles this seeds (status polls ~2s) without ever
    // outliving an edit.
    (
        StatusCode::OK,
        [(
            axum::http::header::CACHE_CONTROL,
            "private, max-age=2, stale-while-revalidate=8",
        )],
        Json(envelope),
    )
        .into_response()
}

/// The setup-config part. Calls the SAME `setup::merged_config_map` the
/// `/api/setup/config` handler uses, so the aggregate's config is the
/// identical shape by construction — not a re-derivation that could drift.
/// (The handler returns an opaque `Response` with its own Cache-Control,
/// which is why we share the map builder rather than the handler itself.)
/// A read/parse failure nulls the slot and records a 500-shaped error so
/// config isolates like every other part.
fn config_part() -> Part {
    match crate::setup::merged_config_map() {
        Ok(merged) => Part {
            body: Value::Object(merged),
            error: None,
        },
        Err(e) => Part {
            body: Value::Null,
            error: Some(json!({
                "status": StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                "body": { "error": format!("Failed to read config: {}", e) },
            })),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-part isolation contract, tested directly on the reducer
    /// that enforces it. A 2xx tuple nests its body with no error; a
    /// non-2xx tuple nulls the body and records the status + body under an
    /// error — which is what keeps one failed tile from blanking the
    /// envelope. (The on-path drift invariant — overview's parts ==
    /// the singleton handlers — is enforced structurally: every part calls
    /// the real handler / shared `setup::merged_config_map` and nests the
    /// result verbatim, so no second code path exists to diverge; an
    /// integration test would need a fully-wired AppState + live
    /// /backingfiles + sentryusb.conf, which the unit harness can't stand up.)
    #[test]
    fn part_from_tuple_isolates_failures() {
        let ok = part_from_tuple((StatusCode::OK, Json(json!({ "a": 1 }))));
        assert_eq!(ok.body, json!({ "a": 1 }));
        assert!(ok.error.is_none());

        let bad = part_from_tuple((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "boom" })),
        ));
        assert_eq!(bad.body, Value::Null);
        let err = bad.error.expect("failed part records an error");
        assert_eq!(err["status"], 500);
        assert_eq!(err["body"], json!({ "error": "boom" }));
    }
}
