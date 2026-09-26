//! One-shot CLI for keep-awake BLE actions.
//!
//! Replaces the `tesla-control wake|sentry-mode|charge-port-open|...`
//! shell-outs in `run/awake_start`. Each invocation tries two paths,
//! in this order:
//!
//!   1. **IPC fast-path** — connect to the running telemetry
//!      daemon's Unix socket at `/tmp/sentryusb-telemetry.sock`,
//!      send `"<verb>\n"`, read `"OK\n"` or `"ERR ...\n"`. The
//!      daemon dispatches the action through its already-warm
//!      PersistentSession. Zero new BLE connections, no slot
//!      handoff, telemetry polling never pauses. This is the
//!      preferred path whenever BLE telemetry is enabled.
//!
//!   2. **Direct-BLE fallback** — open our own PersistentSession,
//!      do the action, exit. Same path as before this binary
//!      learned about the IPC socket. Covers users who don't run
//!      the telemetry daemon (BLE telemetry disabled in settings),
//!      and cold paths where the daemon crashed and hasn't been
//!      restarted yet.
//!
//! Per-invocation overhead:
//!   * IPC path: ~50-300ms (daemon already has a connection)
//!   * Direct path: ~1-2s (scan + handshake + command)
//!
//! Usage:
//!   sentryusb-ble-action <verb>
//!
//! Verbs:
//!   wake               - VEHICLE_SECURITY RKE wake
//!   sentry-on          - turn Sentry Mode on
//!   sentry-off         - turn Sentry Mode off
//!   charge-port-open   - open the charge port
//!   charge-port-close  - close the charge port
//!   charge-start       - start charging
//!   charge-stop        - stop charging
//!   set-charging-amps:N - set charging current (1-80 A)
//!   set-charge-limit:N  - set charge limit (50-100%)
//!   keep-accessory-on  - turn Keep Accessory Power on
//!   keep-accessory-off - turn Keep Accessory Power off
//!   session-info       - pairing probe (see below)
//!   drive-state        - current gear query (see below)
//!   sentry-state       - current Sentry Mode query (Off/On; missing state fails)
//!   pair               - add-key-to-whitelist request (prompts for NFC tap)
//!   keygen             - generate the BLE P-256 keypair (replaces tesla-keygen)
//!
//! `session-info` is not an action: it prints exactly one stdout token
//! — `PAIRED`, `NOT_PAIRED`, or `UNREACHABLE` — and exits 0 for all
//! three. The API matches on that token (its shell helper only surfaces
//! stdout) and clears the paired marker only on `NOT_PAIRED`. Config
//! errors exit 2 with no token, which the API reads as "couldn't
//! verify", never "unpaired".
//!
//! `drive-state` is also a query, not an action: on success it prints
//! the gear token (`P`/`R`/`N`/`D`) to stdout and exits 0; on any
//! failure (car asleep/unreachable, no concrete gear, config/key error)
//! it exits non-zero with a stderr message. The lock-chime smart mode
//! reads this — exit 0 + `P` means "parked, safe to proceed".
//!
//! Exit codes:
//!   0 success (for session-info: a token was printed)
//!   1 invalid usage
//!   2 config error (missing VIN, missing key file)
//!   3 BLE error (scan/connect/handshake failed)
//!   4 action rejected by car (returns the fault code as stderr line)

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use sentryusb_tesla_ble::{
    actions::{self, ActionPayload},
    keys::KeyPair,
    manager::{PairingStatus, PersistentSession},
    responses::{shift_state_token, sentry_mode_token},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{error, info, warn};

const KEY_FILE: &str = "/root/.ble/key_private.pem";
const CONFIG_FILE: &str = "/root/sentryusb.conf";
/// Must match `action_socket::SOCKET_PATH` in the telemetry daemon.
const IPC_SOCKET: &str = "/tmp/sentryusb-telemetry.sock";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,btleplug=warn".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let verb = match std::env::args().nth(1) {
        Some(v) => v,
        None => {
            eprintln!(
                "usage: sentryusb-ble-action <wake|sentry-on|sentry-off|charge-port-open|charge-port-close|keep-accessory-on|keep-accessory-off|charge-start|charge-stop|set-charging-amps:N|set-charge-limit:N|session-info|drive-state|sentry-state|pair|keygen>"
            );
            return ExitCode::from(1);
        }
    };
    // `session-info` is a pairing probe, not an action — it reports a
    // stdout token rather than running a command. Handle it on its own
    // IPC-first / direct-fallback path before the action dispatch.
    if verb == "session-info" {
        return run_session_info().await;
    }
    // `drive-state` is a query too — prints the gear token and uses
    // distinct exit codes, so handle it before the action dispatch.
    if matches!(verb.as_str(), "drive-state" | "sentry-state") {
        return run_state_query(&verb).await;
    }
    // `pair` is a fire-and-forget add-key-to-whitelist request — it has
    // its own IPC-first / direct-fallback path and exit-code semantics
    // (0 = request delivered, tap your card; 3 = BLE/slot failure), so
    // handle it before the action dispatch too.
    if verb == "pair" {
        return run_pair().await;
    }
    // `keygen` generates the P-256 keypair natively (replaces
    // `tesla-keygen create`). No BLE — pure local key generation.
    if verb == "keygen" {
        return run_keygen();
    }

    // Try the IPC fast-path first. If the telemetry daemon is up,
    // it'll service the action via its already-warm session and
    // return in <300ms — without competing for the BLE slot. If the
    // socket isn't there (telemetry disabled) or the connect fails
    // (daemon crashed), fall back to direct BLE so users without
    // the telemetry daemon keep working.
    match no_answer_policy(try_via_ipc(verb.as_str()).await) {
        Ok(()) => {
            info!("action via daemon IPC: {} OK", verb);
            return ExitCode::SUCCESS;
        }
        Err(IpcFail::Unavailable(reason)) => {
            // Expected on systems where telemetry isn't running.
            // Not a warning — this is the design's intended fallback.
            info!(
                "telemetry IPC unavailable ({}), falling back to direct BLE",
                reason
            );
            match c6_direct_path() {
                DirectBle::Stock | DirectBle::Claim => {}
                DirectBle::Block => {
                    eprintln!("C6 just held the car link and the telemetry daemon is down; not opening direct BLE");
                    return ExitCode::from(3);
                }
                // The C6 holds a live grant: it's the only side allowed to sign,
                // so send the action through its supervisor (car's real answer).
                DirectBle::RouteC6 => return run_via_c6(verb.as_str()).await,
            }
        }
        Err(IpcFail::DaemonRejected(msg)) => {
            // Daemon is up but refused the action (e.g. BLE disabled
            // in settings, VIN missing, radio held by something
            // else). These would also fail on the direct path, so
            // exit with the error rather than thrashing the radio.
            error!("daemon refused action: {}", msg);
            return ExitCode::from(3);
        }
    }

    let action = match actions::parse_verb(verb.as_str()) {
        Ok(action) => action,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match run(verb.as_str(), action).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            // Map error categories to distinct exit codes so
            // awake_start can choose to retry vs log-and-skip.
            let msg = format!("{e:#}");
            if msg.contains("config") || msg.contains("TESLA_BLE_VIN") || msg.contains("key file")
            {
                ExitCode::from(2)
            } else if msg.contains("fault code") {
                ExitCode::from(4)
            } else {
                ExitCode::from(3)
            }
        }
    }
}

/// Two-axis result for the IPC attempt:
///   * `Unavailable` → no daemon listening; fall back to direct BLE
///   * `DaemonRejected` → daemon is up but said no; surface the
///                       error instead of retrying via direct
///                       (same failure mode would just repeat)
#[derive(Debug)]
enum IpcError {
    Unavailable(String),
    DaemonRejected(String),
    /// Connected and sent, but no answer (timeout / read error / closed). The
    /// daemon may still be signing: C6 mode refuses to go direct (see
    /// `no_answer_policy`); stock treats it as Unavailable, as before.
    NoAnswer(String),
}

/// Map `NoAnswer` per mode: flag off = stock (fall back to direct BLE);
/// c6_primary = refuse (a slow daemon's session plus ours = two Pi signers,
/// and the C6 may hold the same key).
fn no_answer_policy<T>(r: Result<T, IpcError>) -> Result<T, IpcFail> {
    no_answer_policy_with(r, c6_owns_link())
}

fn no_answer_policy_with<T>(r: Result<T, IpcError>, c6_mode: bool) -> Result<T, IpcFail> {
    match r {
        Ok(v) => Ok(v),
        Err(IpcError::NoAnswer(m)) if c6_mode => Err(IpcFail::DaemonRejected(format!(
            "telemetry daemon did not answer ({m}); C6 mode: not opening direct BLE"
        ))),
        Err(IpcError::NoAnswer(m) | IpcError::Unavailable(m)) => Err(IpcFail::Unavailable(m)),
        Err(IpcError::DaemonRejected(m)) => Err(IpcFail::DaemonRejected(m)),
    }
}

/// IPC outcome after `no_answer_policy`: fall back to direct, or surface.
#[derive(Debug, PartialEq, Eq)]
enum IpcFail {
    Unavailable(String),
    DaemonRejected(String),
}

/// Connect to the telemetry daemon's Unix socket, send the verb,
/// read the response. Tight timeouts on every step so a hung
/// daemon doesn't make us wait minutes before falling back.
async fn try_via_ipc(verb: &str) -> Result<(), IpcError> {
    // Connect with a 1s timeout. The daemon either accepts
    // immediately (it's already in its accept() loop) or doesn't
    // exist — any failure means "not available, fall back."
    let stream = match tokio::time::timeout(
        Duration::from_millis(1000),
        UnixStream::connect(IPC_SOCKET),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {}: {}",
                IPC_SOCKET, e
            )));
        }
        Err(_) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {} timed out",
                IPC_SOCKET
            )));
        }
    };

    let (read_half, mut write_half) = stream.into_split();

    // Send the verb as one line.
    let cmd = format!("{}\n", verb);
    if let Err(e) = write_half.write_all(cmd.as_bytes()).await {
        return Err(IpcError::Unavailable(format!(
            "writing verb: {}",
            e
        )));
    }

    // Read one line of response. 90s wall-clock — generous because
    // the daemon's own 60s per-request timeout plus connect /
    // handshake overhead can push this close to that, but bounded
    // so a stuck daemon doesn't wedge us forever.
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read_result = tokio::time::timeout(
        Duration::from_secs(90),
        reader.read_line(&mut line),
    )
    .await;
    match read_result {
        Ok(Ok(0)) => Err(IpcError::NoAnswer(
            "daemon closed connection without response".into(),
        )),
        Ok(Ok(_)) => {
            let line = line.trim();
            if line == "OK" {
                Ok(())
            } else if let Some(rest) = line.strip_prefix("ERR ") {
                Err(IpcError::DaemonRejected(rest.to_string()))
            } else {
                Err(IpcError::DaemonRejected(format!(
                    "unexpected response: {:?}",
                    line
                )))
            }
        }
        Ok(Err(e)) => Err(IpcError::NoAnswer(format!(
            "read response: {}",
            e
        ))),
        Err(_) => Err(IpcError::NoAnswer(
            "daemon response timed out after 90s".into(),
        )),
    }
}

/// `keygen` verb. Generates the P-256 BLE keypair under /root/.ble
/// (PKCS#8 private + SPKI public PEM) via the native generator — the
/// drop-in replacement for `tesla-keygen … create`. Idempotent and
/// safe: if a private key already exists it is left untouched (never
/// clobber a key that may already be paired with the car) and we exit 0.
/// Marks the keys freshly generated so the UI knows pairing is pending.
///
/// Exit codes: 0 keys present/created, 2 filesystem/keygen error.
fn run_keygen() -> ExitCode {
    let dir = Path::new("/root/.ble");
    if dir.join("key_private.pem").exists() {
        info!("keygen: keypair already present at {} — leaving it untouched", dir.display());
        println!("OK");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        error!("keygen: creating {}: {e:#}", dir.display());
        return ExitCode::from(2);
    }
    match sentryusb_tesla_ble::keys::generate_keypair(dir) {
        Ok(_) => {
            // Mark not-yet-paired so the web UI doesn't falsely show
            // "BLE Paired" on a fresh keypair (matches the wizard).
            let _ = std::fs::write(dir.join("key_pending_pairing"), "");
            info!("keygen: generated BLE keypair under {}", dir.display());
            println!("OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("keygen: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// `session-info` verb. Prints one pairing token to stdout and exits 0
/// (config errors exit 2 with no token). IPC-first so the probe reuses
/// the telemetry daemon's warm connection; direct fallback covers a
/// disabled/crashed daemon.
async fn run_session_info() -> ExitCode {
    match no_answer_policy(session_info_via_ipc().await) {
        Ok(token) => {
            println!("{token}");
            return ExitCode::SUCCESS;
        }
        Err(IpcFail::Unavailable(reason)) => {
            info!(
                "telemetry IPC unavailable ({}), checking pairing via direct BLE",
                reason
            );
            // Unknown, never NOT_PAIRED: the API leaves the paired marker alone.
            if !matches!(c6_direct_path(), DirectBle::Stock | DirectBle::Claim) {
                println!("UNREACHABLE");
                return ExitCode::SUCCESS;
            }
        }
        Err(IpcFail::DaemonRejected(msg)) => {
            // Daemon answered with something we don't recognise. Don't
            // fall through to a direct attempt (it would just wake the
            // car again and likely repeat) — report unknown as
            // unreachable so the API leaves the marker alone.
            warn!("daemon returned unexpected session-info reply: {}", msg);
            println!("UNREACHABLE");
            return ExitCode::SUCCESS;
        }
    }

    // Direct fallback: open our own one-shot session.
    let (vin, adapter) = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::from(2);
        }
    };
    let keypair = match KeyPair::load(Path::new(KEY_FILE)) {
        Ok(k) => k,
        Err(e) => {
            error!("loading BLE key file {KEY_FILE}: {e:#}");
            return ExitCode::from(2);
        }
    };
    let session = PersistentSession::start(keypair, vin, adapter);
    let status = session.check_pairing().await;
    session.shutdown().await;
    let token = match status {
        PairingStatus::Paired => "PAIRED",
        PairingStatus::NotPaired => "NOT_PAIRED",
        PairingStatus::Unreachable(reason) => {
            info!("session-info direct probe unreachable: {reason}");
            "UNREACHABLE"
        }
    };
    println!("{token}");
    ExitCode::SUCCESS
}

/// `pair` verb. Sends an unauthenticated add-key-to-whitelist request for
/// our own public key, so the car prompts for an NFC-card tap on the
/// center console. Fire-and-forget: success (exit 0, prints `OK`) means
/// the request was delivered — the caller then waits for the tap and
/// confirms enrolment with `session-info`. Replaces the old
/// `tesla-control add-key-request` shell-out.
///
/// IPC-first: routes through the telemetry daemon's already-open
/// connection, reusing the BLE slot the daemon holds. This is the key
/// fix for the "maximum number of BLE devices" failure — the old flow
/// stopped the daemon and opened a *new* connection before the car freed
/// the slot. Direct fallback (own one-shot session) covers a
/// disabled/crashed daemon.
///
/// Exit codes: 0 request delivered, 2 config/key error, 3 BLE/slot error.
async fn run_pair() -> ExitCode {
    match no_answer_policy(try_via_ipc("pair").await) {
        Ok(()) => {
            println!("OK");
            info!("add-key-request delivered via daemon IPC — tap your card on the console");
            return ExitCode::SUCCESS;
        }
        Err(IpcFail::Unavailable(reason)) => {
            info!(
                "telemetry IPC unavailable ({}), sending add-key-request via direct BLE",
                reason
            );
            if !matches!(c6_direct_path(), DirectBle::Stock | DirectBle::Claim) {
                eprintln!("C6 owns the car link; pair the C6 instead");
                return ExitCode::from(3);
            }
        }
        Err(IpcFail::DaemonRejected(msg)) => {
            // Daemon is up but the add-key write failed (slot full, car
            // out of range/asleep). A direct attempt would open a *new*
            // connection — exactly what hits the car's slot limit — so
            // surface the daemon's error rather than thrashing the radio.
            eprintln!("{msg}");
            return ExitCode::from(3);
        }
    }

    // Direct fallback: open our own one-shot session and send the request.
    let (vin, adapter) = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::from(2);
        }
    };
    let keypair = match KeyPair::load(Path::new(KEY_FILE)) {
        Ok(k) => k,
        Err(e) => {
            error!("loading BLE key file {KEY_FILE}: {e:#}");
            return ExitCode::from(2);
        }
    };
    let session = PersistentSession::start(keypair, vin, adapter);
    let result = tokio::time::timeout(Duration::from_secs(60), session.add_key_request()).await;
    session.shutdown().await;
    match result {
        Ok(Ok(())) => {
            println!("OK");
            info!("add-key-request delivered via direct BLE — tap your card on the console");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            error!("{e:#}");
            ExitCode::from(3)
        }
        Err(_) => {
            error!("pair (add-key) timed out after 60s");
            ExitCode::from(3)
        }
    }
}

/// Send `session-info` over the daemon IPC socket and map the reply to
/// a stdout token. `Unavailable` means no daemon is listening (caller
/// falls back to direct); `DaemonRejected` means an unrecognised reply.
async fn session_info_via_ipc() -> Result<&'static str, IpcError> {
    let stream = match tokio::time::timeout(
        Duration::from_millis(1000),
        UnixStream::connect(IPC_SOCKET),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {}: {}",
                IPC_SOCKET, e
            )));
        }
        Err(_) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {} timed out",
                IPC_SOCKET
            )));
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    if let Err(e) = write_half.write_all(b"session-info\n").await {
        return Err(IpcError::Unavailable(format!("writing verb: {}", e)));
    }

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    match tokio::time::timeout(Duration::from_secs(90), reader.read_line(&mut line)).await {
        Ok(Ok(0)) => Err(IpcError::NoAnswer(
            "daemon closed connection without response".into(),
        )),
        Ok(Ok(_)) => {
            let line = line.trim();
            let rest = line.strip_prefix("ERR ").map(str::trim);
            if line == "OK" {
                Ok("PAIRED")
            } else if rest.is_some_and(|r| r.starts_with("NOT_PAIRED")) {
                Ok("NOT_PAIRED")
            } else if rest.is_some_and(|r| r.starts_with("UNREACHABLE")) {
                Ok("UNREACHABLE")
            } else {
                Err(IpcError::DaemonRejected(line.to_string()))
            }
        }
        Ok(Err(e)) => Err(IpcError::NoAnswer(format!("read response: {}", e))),
        Err(_) => Err(IpcError::NoAnswer(
            "daemon response timed out after 90s".into(),
        )),
    }
}

/// Read-only vehicle query. Prints gear (`P`/`R`/`N`/`D`) or Sentry (`Off`/`On`)
/// and exits 0 on success; exits non-zero on any failure (so the
/// lock-chime caller's `run_with_timeout` sees an Err). IPC-first so the
/// query reuses the telemetry daemon's warm connection; direct fallback
/// covers a disabled/crashed daemon.
async fn run_state_query(verb: &str) -> ExitCode {
    match no_answer_policy(state_query_via_ipc(verb).await) {
        Ok(token) => {
            println!("{token}");
            return ExitCode::SUCCESS;
        }
        Err(IpcFail::Unavailable(reason)) => {
            info!(
                "telemetry IPC unavailable ({}), reading vehicle state via direct BLE",
                reason
            );
            if !matches!(c6_direct_path(), DirectBle::Stock | DirectBle::Claim) {
                eprintln!("C6 owns the car link and the telemetry daemon is down");
                return ExitCode::from(3);
            }
        }
        Err(IpcFail::DaemonRejected(msg)) => {
            // Daemon is up but couldn't read the state (car asleep /
            // unreachable / no concrete reading). A direct attempt would
            // just repeat against the same car, so surface the failure
            // instead of thrashing the radio.
            eprintln!("{msg}");
            return ExitCode::from(3);
        }
    }

    // Direct fallback: open our own one-shot session.
    let (vin, adapter) = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::from(2);
        }
    };
    let keypair = match KeyPair::load(Path::new(KEY_FILE)) {
        Ok(k) => k,
        Err(e) => {
            error!("loading BLE key file {KEY_FILE}: {e:#}");
            return ExitCode::from(2);
        }
    };
    let session = PersistentSession::start(keypair, vin, adapter);
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        if verb == "sentry-state" {
            let state = session.get_closures().await?;
            sentry_mode_token(&state).map(str::to_owned).context("car reported no Sentry state")
        } else {
            let state = session.get_drive().await?;
            shift_state_token(&state).map(str::to_owned).context("car reported no gear (may be asleep)")
        }
    }).await;
    session.shutdown().await;
    match result {
        Ok(Ok(token)) => {
            println!("{token}");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            error!("{e:#}");
            ExitCode::from(3)
        }
        Err(_) => {
            error!("{verb} timed out after 60s");
            ExitCode::from(3)
        }
    }
}

/// Send a read-only state query over the daemon IPC socket and read its
/// state token. `Unavailable` means no daemon is listening (caller falls
/// back to direct); `DaemonRejected` carries the daemon's error line.
async fn state_query_via_ipc(verb: &str) -> Result<String, IpcError> {
    let stream = match tokio::time::timeout(
        Duration::from_millis(1000),
        UnixStream::connect(IPC_SOCKET),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {}: {}",
                IPC_SOCKET, e
            )));
        }
        Err(_) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {} timed out",
                IPC_SOCKET
            )));
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    if let Err(e) = write_half.write_all(format!("{verb}\n").as_bytes()).await {
        return Err(IpcError::Unavailable(format!("writing verb: {}", e)));
    }

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    match tokio::time::timeout(Duration::from_secs(90), reader.read_line(&mut line)).await {
        Ok(Ok(0)) => Err(IpcError::NoAnswer(
            "daemon closed connection without response".into(),
        )),
        Ok(Ok(_)) => {
            let line = line.trim();
            if let Some(tok) = line.strip_prefix("OK ") {
                Ok(tok.trim().to_string())
            } else if let Some(rest) = line.strip_prefix("ERR ") {
                Err(IpcError::DaemonRejected(rest.trim().to_string()))
            } else {
                // Bare "OK" (no token) or anything unexpected — the
                // daemon should always include a gear token here.
                Err(IpcError::DaemonRejected(format!(
                    "unexpected state-query reply: {:?}",
                    line
                )))
            }
        }
        Ok(Err(e)) => Err(IpcError::NoAnswer(format!("read response: {}", e))),
        Err(_) => Err(IpcError::NoAnswer(
            "daemon response timed out after 90s".into(),
        )),
    }
}

async fn run(verb: &str, action: ActionPayload) -> Result<()> {
    let (vin, adapter) = load_config()?;
    info!(
        "sentryusb-ble-action: verb={} domain={:?} inner={} bytes vin={}…{}",
        verb,
        action.domain,
        action.inner.len(),
        &vin[..3],
        &vin[vin.len() - 4..]
    );

    let keypair = KeyPair::load(Path::new(KEY_FILE))
        .with_context(|| format!("loading BLE key file {KEY_FILE}"))?;
    let session = PersistentSession::start(keypair, vin, adapter);

    // One-shot — wrap in an outer timeout so the script doesn't hang
    // indefinitely if the car never advertises.
    let resp = tokio::time::timeout(
        Duration::from_secs(60),
        session.send_action(action),
    )
    .await
    .context("BLE action timed out after 60s")?;
    session.shutdown().await;

    match resp {
        Ok(bytes) => {
            info!("action OK; decrypted response = {} bytes", bytes.len());
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Read TESLA_BLE_VIN and (optionally) BLE_ADAPTER from sentryusb.conf.
/// Returns (vin, Some(adapter)) or fails if VIN missing.
fn load_config() -> Result<(String, Option<String>)> {
    let raw = std::fs::read_to_string(CONFIG_FILE)
        .with_context(|| format!("reading {CONFIG_FILE}"))?;
    let mut vin: Option<String> = None;
    let mut adapter: Option<String> = None;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("export TESLA_BLE_VIN=") {
            vin = Some(unquote(rest).to_uppercase());
        } else if let Some(rest) = trimmed.strip_prefix("export BLE_ADAPTER=") {
            adapter = Some(unquote(rest));
        }
    }
    let vin = vin.context("TESLA_BLE_VIN not set in /root/sentryusb.conf")?;
    if vin.len() != 17 {
        anyhow::bail!("TESLA_BLE_VIN must be 17 chars, got {}", vin.len());
    }
    Ok((vin, adapter))
}

/// True when `TELEMETRY_SOURCE=c6_primary` (and not the C6_BACKFILL bench
/// mode). Parsed with the same config crate + lookup the telemetry daemon uses.
fn c6_owns_link() -> bool {
    match sentryusb_config::parse_file(sentryusb_config::find_config_path()) {
        Ok((active, commented)) => {
            c6_owns_link_in(&active, &commented, sentryusb_tesla_ble::c6_backfill::backfill_allowed())
        }
        Err(_) => false,
    }
}

// Must match the telemetry daemon's c6_coord / the supervisor's coord.rs.
const C6_DEVICE: &str = "/dev/sentryusb-c6";
const C6_LEASE_PATH: &str = "/run/sentryusb-c6/sampler_lease.json";
const C6_LEASE_LOCK_PATH: &str = "/run/sentryusb-c6/lease.lock";
const C6_CLAIM_SETTLE_MS: u64 = 35_000;
/// Covers one direct action (each path times out within ~90s). The daemon
/// won't grant the C6 over this lease until it lapses.
const C6_SAMPLER_LEASE_MS: u64 = 120_000;

#[derive(Debug, PartialEq, Eq)]
enum DirectBle {
    /// No C6 in the picture: stock behaviour.
    Stock,
    /// C6 present but not driving: claim the car (lease), then go direct.
    Claim,
    /// The C6 only just held the car (grant expired < settle ago) or the lease
    /// is unreadable: refuse.
    Block,
    /// The C6 holds a live grant: route the action through its supervisor.
    RouteC6,
}

/// Pure: may this process open its own session? `lease` = current lease file.
fn direct_ble_decision(c6_primary: bool, c6_present: bool, lease: Option<&str>, boot_id: &str, now_ms: u64) -> DirectBle {
    if !c6_present {
        return DirectBle::Stock;
    }
    // A lease we can't parse at all: assume a live grant (refuse).
    let parsed = match lease.map(serde_json::from_str::<serde_json::Value>) {
        None => None,
        Some(Ok(v)) => Some(v),
        Some(Err(_)) => return DirectBle::Block,
    };
    let grant_now = parsed.as_ref().is_some_and(|v| {
        v.get("owner").and_then(|x| x.as_str()) == Some("c6")
            && v.get("boot_id").and_then(|x| x.as_str()) == Some(boot_id)
            && match (
                v.get("written_boottime_ms").and_then(|x| x.as_u64()),
                v.get("valid_for_ms").and_then(|x| x.as_u64()),
            ) {
                (Some(w), Some(valid)) => now_ms < w.saturating_add(valid),
                _ => false,
            }
    });
    if grant_now {
        return DirectBle::RouteC6;
    }
    let grant_recent = parsed.is_some_and(|v| {
            v.get("owner").and_then(|x| x.as_str()) == Some("c6")
                && v.get("boot_id").and_then(|x| x.as_str()) == Some(boot_id)
                && match (
                    v.get("written_boottime_ms").and_then(|x| x.as_u64()),
                    v.get("valid_for_ms").and_then(|x| x.as_u64()),
                ) {
                    // Live, or expired too recently for an in-flight C6 command to have ended.
                    (Some(w), Some(valid)) => now_ms < w.saturating_add(valid).saturating_add(C6_CLAIM_SETTLE_MS),
                    _ => true, // unreadable grant: assume live
                }
        });
    // A live/just-expired grant blocks whatever the flag says now (it may have
    // been flipped while the daemon was down); otherwise flag off = stock.
    if grant_recent {
        DirectBle::Block
    } else if c6_primary {
        DirectBle::Claim
    } else {
        DirectBle::Stock
    }
}

fn boottime_ms() -> Option<u64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer to a stack timespec.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    (rc == 0).then(|| ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000)
}

/// Called when the telemetry daemon is unreachable. C6 absent (unplugged) or
/// flag off = stock direct BLE. C6 present: route through it while it holds
/// a live grant, refuse right after one; otherwise publish our own sampler
/// lease (supervisor holds + parks) and go direct.
fn c6_direct_path() -> DirectBle {
    let present = std::path::Path::new(C6_DEVICE).exists();
    if !present {
        return DirectBle::Stock;
    }
    let (Some(boot), Some(now)) = (
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok().map(|s| s.trim().to_string()),
        boottime_ms(),
    ) else {
        return DirectBle::Block; // can't evaluate a lease with a C6 present: refuse
    };
    let owns = c6_owns_link();
    // Read-decide-write under the lease lock the daemon also takes, so its
    // grant and our claim can never interleave. Can't lock = refuse.
    let _lock = {
        use std::os::fd::AsRawFd;
        let path = std::path::Path::new(C6_LEASE_LOCK_PATH);
        let _ = path.parent().map(std::fs::create_dir_all);
        let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path) else {
            return DirectBle::Block;
        };
        // SAFETY: valid fd owned by `f`; the lock is released when `f` drops.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return DirectBle::Block;
        }
        f
    };
    let lease = std::fs::read_to_string(C6_LEASE_PATH).ok();
    match direct_ble_decision(owns, present, lease.as_deref(), &boot, now) {
        DirectBle::Claim => {
            let body = serde_json::json!({
                "owner": "sampler", "holder": "ble-action", "boot_id": boot,
                "written_boottime_ms": now, "valid_for_ms": C6_SAMPLER_LEASE_MS,
            })
            .to_string();
            let path = std::path::Path::new(C6_LEASE_PATH);
            let tmp = path.with_extension("json.tmp");
            let ok = path.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok())
                && std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path)).is_ok();
            // Can't publish our claim: don't risk two signers.
            if ok { DirectBle::Claim } else { DirectBle::Block }
        }
        other => other,
    }
}

/// Send `verb` through the C6 supervisor, waiting for the car's own answer
/// where the firmware tracks one. Exit codes as the direct path.
async fn run_via_c6(verb: &str) -> ExitCode {
    let Some(cmd) = sentryusb_tesla_ble::c6_route::verb_to_c6_command(verb) else {
        eprintln!("C6 owns the car link; '{verb}' has no C6 equivalent");
        return ExitCode::from(3);
    };
    let api = supervisor_api();
    match sentryusb_tesla_ble::c6_route::supervisor_command(&api, &cmd).await {
        Ok(()) => {
            info!("action via C6 supervisor: {} OK", verb);
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("action via C6 supervisor failed: {e:#}");
            ExitCode::from(3)
        }
    }
}

/// C6 supervisor address from `C6_SUPERVISOR_API` (loopback only, as in the
/// daemon's BleConfig), else the default.
fn supervisor_api() -> String {
    sentryusb_config::parse_file(sentryusb_config::find_config_path())
        .ok()
        .and_then(|(a, c)| sentryusb_config::get_config_value(&a, &c, "C6_SUPERVISOR_API"))
        .map(|v| v.trim().to_string())
        .filter(|v| v.starts_with("127.0.0.1:") || v.starts_with("localhost:"))
        .unwrap_or_else(|| sentryusb_tesla_ble::c6_route::DEFAULT_SUPERVISOR_API.to_string())
}

/// `backfill_ok`: C6_BACKFILL only counts on a proven separate key; on a
/// shared or unknown key it's ignored (one-at-a-time coordination).
fn c6_owns_link_in(
    active: &sentryusb_config::SetupConfig,
    commented: &sentryusb_config::SetupConfig,
    backfill_ok: bool,
) -> bool {
    let get = |k| sentryusb_config::get_config_value(active, commented, k);
    let primary = get("TELEMETRY_SOURCE").is_some_and(|v| v.trim().eq_ignore_ascii_case("c6_primary"));
    let backfill = get("C6_BACKFILL")
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "yes" | "true"));
    primary && !(backfill && backfill_ok)
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    // len >= 2 so a value that is a single quote character can't
    // underflow the slice below.
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"'))
            || (t.starts_with('\'') && t.ends_with('\'')))
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{c6_owns_link_in, direct_ble_decision, DirectBle};

    fn grant(w: u64, valid: u64) -> String {
        format!(r#"{{"owner":"c6","boot_id":"b","written_boottime_ms":{w},"valid_for_ms":{valid}}}"#)
    }

    #[test]
    fn no_answer_is_stock_fallback_only_with_the_flag_off() {
        use super::{no_answer_policy_with, IpcError, IpcFail};
        let slow = || Err::<(), _>(IpcError::NoAnswer("daemon response timed out after 90s".into()));
        // Stock (flag off): unchanged, falls back to direct BLE.
        assert_eq!(no_answer_policy_with(slow(), false), Err(IpcFail::Unavailable("daemon response timed out after 90s".into())));
        // C6 mode: a slow daemon may still be signing; never go direct.
        assert!(matches!(no_answer_policy_with(slow(), true), Err(IpcFail::DaemonRejected(_))));
        // A daemon that isn't there at all still falls back in both modes.
        let down = || Err::<(), _>(IpcError::Unavailable("connect: refused".into()));
        assert!(matches!(no_answer_policy_with(down(), true), Err(IpcFail::Unavailable(_))));
    }

    #[test]
    fn c6_absent_or_flag_off_is_stock_behaviour() {
        assert_eq!(direct_ble_decision(true, false, Some(&grant(0, 120_000)), "b", 1), DirectBle::Stock);
        assert_eq!(direct_ble_decision(false, true, None, "b", 1), DirectBle::Stock);
        // Flag flipped off while a grant is still live: never direct.
        assert_eq!(direct_ble_decision(false, true, Some(&grant(0, 120_000)), "b", 1), DirectBle::RouteC6);
    }

    #[test]
    fn c6_present_blocks_only_while_it_holds_the_car() {
        // Live grant: route through the C6 (even with the flag flipped off).
        assert_eq!(direct_ble_decision(true, true, Some(&grant(0, 120_000)), "b", 60_000), DirectBle::RouteC6);
        assert_eq!(direct_ble_decision(false, true, Some(&grant(0, 120_000)), "b", 60_000), DirectBle::RouteC6);
        // Expired < settle ago: refuse (a C6 command may still be in flight).
        assert_eq!(direct_ble_decision(true, true, Some(&grant(0, 120_000)), "b", 150_000), DirectBle::Block);
        // Grant long expired (daemon down), no lease, sampler lease, other boot: claim + go.
        assert_eq!(direct_ble_decision(true, true, Some(&grant(0, 120_000)), "b", 156_000), DirectBle::Claim);
        assert_eq!(direct_ble_decision(true, true, None, "b", 1), DirectBle::Claim);
        let sampler = r#"{"owner":"sampler","boot_id":"b","written_boottime_ms":0,"valid_for_ms":600000}"#;
        assert_eq!(direct_ble_decision(true, true, Some(sampler), "b", 1), DirectBle::Claim);
        assert_eq!(direct_ble_decision(true, true, Some(&grant(0, 120_000)), "old", 1), DirectBle::Claim);
        // Unparseable lease: assume a live grant.
        assert_eq!(direct_ble_decision(true, true, Some("garbage"), "b", 1), DirectBle::Block);
    }

    fn owns_with(conf: &str, backfill_ok: bool) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.conf");
        std::fs::write(&p, conf).unwrap();
        let (a, c) = sentryusb_config::parse_file(p.to_str().unwrap()).unwrap();
        c6_owns_link_in(&a, &c, backfill_ok)
    }

    fn owns(conf: &str) -> bool {
        owns_with(conf, true)
    }

    #[test]
    fn backfill_is_ignored_unless_the_key_is_proven_separate() {
        let conf = "export TELEMETRY_SOURCE=c6_primary\nexport C6_BACKFILL=1\n";
        assert!(owns_with(conf, false), "shared/unknown key: C6 still owns, no direct fallback");
        assert!(!owns_with(conf, true), "separate key: bench side-by-side");
    }

    #[test]
    fn c6_guard_follows_the_daemon_parser() {
        assert!(!owns("export TESLA_BLE_VIN=X\n"));
        assert!(owns("export TELEMETRY_SOURCE='c6_primary'\n"));
        assert!(owns("#export TELEMETRY_SOURCE=c6_primary\n"));
        assert!(!owns("export TELEMETRY_SOURCE=sampler\n#export TELEMETRY_SOURCE=c6_primary\n"));
        assert!(!owns("export TELEMETRY_SOURCE=c6_primary\nexport C6_BACKFILL=1\n"));
    }
}
