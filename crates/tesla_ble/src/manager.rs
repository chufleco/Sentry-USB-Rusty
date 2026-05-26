//! Push 4: persistent BLE session manager.
//!
//! The current per-call pattern (scan → connect → handshake → command
//! → disconnect) opens a fresh GATT connection for every query. That
//! works but means we never hold a Tesla BLE connection slot —
//! every cycle we re-compete for one of the car's ~3 slots against
//! phone keys + the iOS app. Connection failures during a busy
//! moment ("paired phone walked up while we were sampling") look
//! to the sampler like generic BLE flakiness.
//!
//! `PersistentSession` flips that: one long-lived background tokio
//! task owns the `Connection` + per-domain session keys across many
//! commands. Once we have the slot, we keep it until the link
//! genuinely dies. Phone keys can connect and disconnect freely
//! against the remaining slots without disrupting us, and our
//! per-query overhead drops from ~1.5-2s (scan + handshake + cmd)
//! to ~200-500ms (just the cmd).
//!
//! ## Usage
//!
//! ```ignore
//! let session = PersistentSession::start(keypair, vin).await;
//! loop {
//!     let climate = session.query(
//!         Domain::Infotainment,
//!         VehicleDataState::Climate,
//!     ).await?;
//!     // ... parse, persist
//!     tokio::time::sleep(Duration::from_secs(15)).await;
//! }
//! ```
//!
//! ## Recovery behavior
//!
//! * Transport error (BLE link drop / GATT timeout) → drop connection,
//!   next query triggers a fresh scan + reconnect. Reconnect backs
//!   off on repeated failures but each new `query()` call resets the
//!   schedule so a long idle followed by a sudden burst connects
//!   immediately.
//! * Counter/epoch fault from car (the car has seen this counter
//!   before, or the epoch rolled over) → drop the affected domain's
//!   session state, next query re-handshakes just that domain. The
//!   underlying GATT connection stays up.
//! * Other faults → returned to caller, no state changes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use prost::Message;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::auth;
use crate::crypto::{SessionKey, derive_session_key};
use crate::gatt::Connection;
use crate::keys::KeyPair;
use crate::proto::signatures::{SessionInfo, signature_data};
use crate::proto::universal_message::{
    Domain, RoutableMessage, destination, routable_message,
};
use crate::scan;
use crate::session;
use crate::state_query::{self, VehicleDataState};

/// Max time a single query's BLE round-trip can take before we treat
/// it as a transport failure and force a reconnect on the next call.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// First reconnect attempt after a failure waits this long. Each
/// successive failure doubles up to `RECONNECT_BACKOFF_MAX`. Any
/// successful connection resets back to this value.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(1_500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Seconds added to the *estimated* car clock to produce the
/// `expires_at` field. Tesla caps this at a few minutes (commands
/// stamped too far in the future are rejected as a replay-prevention
/// precaution), but the value just needs to comfortably cover the
/// BLE round-trip and any local drift between sampler clock and car
/// clock. 60 s is a safe margin without coming close to Tesla's cap.
const EXPIRES_WINDOW: u32 = 60;

/// Flags value to send on signed state queries. Bit 1 (value 2) is
/// FLAG_ENCRYPT_RESPONSE — required so the car encrypts its reply
/// instead of sending it plaintext, matches tesla-control's wire
/// format, and is part of the metadata the AES-GCM tag is computed
/// over so the value must match between our sign + the car's verify.
const QUERY_FLAGS: u32 = 2;

/// Handle to a long-lived BLE session with one Tesla vehicle.
/// Cheap to clone — internally an `mpsc::Sender` to the background
/// task. Dropping all clones doesn't stop the task; call `shutdown()`
/// for that.
#[derive(Clone)]
pub struct PersistentSession {
    cmd_tx: mpsc::Sender<Command>,
}

enum Command {
    Query {
        domain: Domain,
        state: VehicleDataState,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    /// Generic signed request — caller supplies the inner payload
    /// bytes already encoded (e.g. a VCSEC RKEAction or a car_server
    /// VehicleControl action). Used by keep-awake actions
    /// (wake-vehicle, sentry-mode, charge-port) so they reuse the
    /// same sign + send + decrypt + refresh-and-retry pipeline as
    /// state queries.
    SignedRequest {
        domain: Domain,
        inner: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    /// Unauthenticated body-controller-state query. Runs through the
    /// held GATT connection (not a new one) so it doesn't fight the
    /// authenticated queries for bluez or kick the persistent slot.
    BodyController {
        reply: oneshot::Sender<Result<crate::proto::vcsec::VehicleStatus>>,
    },
    Shutdown,
}

/// Per-domain authenticated session state cached across commands.
struct DomainSession {
    key: SessionKey,
    epoch: Vec<u8>,
    /// Most recent counter the car has seen from us. The next
    /// outgoing command uses `counter + 1`.
    counter: u32,
    /// Car's `clock_time` from the last SessionInfo, paired with the
    /// local `Instant` at which we received it. Estimated current
    /// car clock = `clock_time_at_handshake + (Instant::now() -
    /// handshake_local_time)`. Without the local-elapsed term we
    /// keep stamping commands with `expires_at` derived from a
    /// frozen clock that stops advancing, and the car eventually
    /// rejects them as TIME_EXPIRED (fault 17) the moment the real
    /// clock passes our stale `expires_at`.
    clock_time_at_handshake: u32,
    handshake_local_time: Instant,
}

impl DomainSession {
    /// Best-effort estimate of the car's current clock_time, derived
    /// from our last cached value + local elapsed seconds. Local + car
    /// clocks drift slowly enough that this is fine for the
    /// `expires_at` calculation across a session lifetime.
    fn estimated_car_clock(&self) -> u32 {
        let elapsed = self.handshake_local_time.elapsed().as_secs() as u32;
        self.clock_time_at_handshake.saturating_add(elapsed)
    }
}

/// Owned by the background task only.
struct SessionState {
    keypair: KeyPair,
    vin: String,
    /// Configured `BLE_ADAPTER` from sentryusb.conf — None means
    /// "let btleplug pick the first one." Mirrors the config field
    /// the api crate reads.
    adapter_name: Option<String>,
    conn: Option<Connection>,
    domains: HashMap<Domain, DomainSession>,
    /// Current reconnect backoff. Doubles on each failed connect.
    backoff: Duration,
    /// When the manager started or last reconnected — for logging.
    connected_at: Option<Instant>,
    /// Successful queries served since the current connection was
    /// established. Reset to 0 on every reconnect. Used by the
    /// periodic status log so operators can see at a glance that
    /// the slot is being held (counter climbs steadily) vs being
    /// re-grabbed (counter resets often).
    queries_since_connect: u32,
    /// Monotonic timestamp of the most recent query (signed or
    /// body-controller) that fully succeeded. Read by the disconnect
    /// diagnostic so a tester's log shows whether the link was
    /// healthy right up to the drop ("last_ok=1s ago") or had been
    /// silently degrading ("last_ok=45s ago"). Reset on each
    /// successful connect.
    last_successful_query_at: Option<Instant>,
    /// Total connection drops detected by `handle_transport_error_if_any`
    /// since the daemon started. Helps testers see at a glance how
    /// flappy their BLE link is over a drive — every drop logs the
    /// running total so a journalctl tail tells the whole story.
    lifetime_drops: u32,
}

/// Log a connection-status summary every this many successful
/// queries. At Active-mode 15s cycles that's roughly every 6 minutes
/// — enough to confirm in journalctl that the slot is held without
/// flooding the log.
const STATUS_LOG_EVERY_N_QUERIES: u32 = 25;

impl PersistentSession {
    /// Spawn the background session task and return a handle.
    /// Doesn't itself trigger a connection — the first `query()`
    /// call kicks that off.
    ///
    /// `adapter_name` accepts a string like `"hci1"` to force a
    /// specific BLE adapter (matches the `BLE_ADAPTER` config in
    /// sentryusb.conf). `None` or an empty string lets btleplug
    /// pick the first one it finds.
    pub fn start(
        keypair: KeyPair,
        vin: String,
        adapter_name: Option<String>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let state = SessionState {
            keypair,
            vin,
            adapter_name,
            conn: None,
            domains: HashMap::new(),
            backoff: RECONNECT_BACKOFF_MIN,
            connected_at: None,
            queries_since_connect: 0,
            last_successful_query_at: None,
            lifetime_drops: 0,
        };
        tokio::spawn(run_session_task(state, cmd_rx));
        Self { cmd_tx }
    }

    /// Issue an authenticated state query. Blocks until the response
    /// is decrypted or an error occurs. Errors include:
    ///   * background task is gone (shouldn't happen unless `shutdown` was called)
    ///   * scan/connect failure (car asleep, out of range, slots full)
    ///   * car returned a non-zero `signed_message_fault`
    ///   * decryption failure
    pub async fn query(
        &self,
        domain: Domain,
        state: VehicleDataState,
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Query {
                domain,
                state,
                reply: tx,
            })
            .await
            .context("PersistentSession background task has stopped")?;
        rx.await.context("session task dropped the reply channel")?
    }

    /// Best-effort stop. Closes the connection and ends the
    /// background task. After calling this, `query()` returns an
    /// error.
    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown).await;
    }

    /// Issue a generic signed request with caller-supplied inner
    /// payload bytes. Used by keep-awake actions
    /// (`actions::wake_vehicle`, `set_sentry_mode`, etc.) that need
    /// the AES-GCM signing pipeline but produce different inner
    /// protobufs than the state queries.
    pub async fn send_signed(
        &self,
        domain: Domain,
        inner: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SignedRequest {
                domain,
                inner,
                reply: tx,
            })
            .await
            .context("PersistentSession background task has stopped")?;
        rx.await.context("session task dropped the reply channel")?
    }

    /// Convenience wrapper around `send_signed` for the typed action
    /// helpers in `crate::actions`.
    pub async fn send_action(
        &self,
        action: crate::actions::ActionPayload,
    ) -> Result<Vec<u8>> {
        self.send_signed(action.domain, action.inner).await
    }

    /// Unauthenticated body-controller-state query. Runs through
    /// the held GATT connection — no new scan + connect, no
    /// competition with the authenticated state queries that share
    /// the same persistent session. Used by the telemetry sampler's
    /// Quiet-mode poll (sleep-safe; doesn't wake the car).
    pub async fn body_controller_state(
        &self,
    ) -> Result<crate::proto::vcsec::VehicleStatus> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::BodyController { reply: tx })
            .await
            .context("PersistentSession background task has stopped")?;
        rx.await.context("session task dropped the reply channel")?
    }

    // -------------------------------------------------------------
    // Typed convenience wrappers. Each does a raw `query()` to the
    // Infotainment domain + decodes the response into the relevant
    // car_server sub-message. Sampler code can use these directly
    // without learning about proto bytes.
    // -------------------------------------------------------------

    /// `state climate`. Interior/exterior temps, HVAC, defroster, etc.
    pub async fn get_climate(&self) -> Result<crate::proto::car_server::ClimateState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Climate)
            .await?;
        crate::responses::parse_climate(&bytes)
    }

    /// `state charge`. Battery %, charger info, range estimate.
    pub async fn get_charge(&self) -> Result<crate::proto::car_server::ChargeState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Charge)
            .await?;
        crate::responses::parse_charge(&bytes)
    }

    /// `state drive`. Shift state, speed, heading.
    pub async fn get_drive(&self) -> Result<crate::proto::car_server::DriveState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Drive)
            .await?;
        crate::responses::parse_drive(&bytes)
    }

    /// `state location`. GPS coords (when authorized).
    pub async fn get_location(&self) -> Result<crate::proto::car_server::LocationState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Location)
            .await?;
        crate::responses::parse_location(&bytes)
    }

    /// `state tire-pressure`. PSI per tire.
    pub async fn get_tire_pressure(&self) -> Result<crate::proto::car_server::TirePressureState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::TirePressure)
            .await?;
        crate::responses::parse_tire_pressure(&bytes)
    }

    /// `state closures`. Door/window/trunk/charge-port states.
    pub async fn get_closures(&self) -> Result<crate::proto::car_server::ClosuresState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Closures)
            .await?;
        crate::responses::parse_closures(&bytes)
    }
}

async fn run_session_task(
    mut state: SessionState,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Query {
                domain,
                state: vds,
                reply,
            } => {
                let inner = state_query::build_get_state_request(vds);
                let result = signed_request_with_refresh_retry(
                    &mut state, domain, inner,
                )
                .await;
                handle_transport_error_if_any(&mut state, &result).await;
                let _ = reply.send(result);
            }
            Command::SignedRequest {
                domain,
                inner,
                reply,
            } => {
                let result = signed_request_with_refresh_retry(
                    &mut state, domain, inner,
                )
                .await;
                handle_transport_error_if_any(&mut state, &result).await;
                let _ = reply.send(result);
            }
            Command::BodyController { reply } => {
                let result = handle_body_controller(&mut state).await;
                handle_transport_error_if_any(&mut state, &result).await;
                let _ = reply.send(result);
            }
            Command::Shutdown => break,
        }
    }
    if let Some(conn) = state.conn.take() {
        conn.close().await;
    }
}

/// Outer wrapper that handles SessionInfo-refresh responses. The car
/// sometimes replies to a signed command with a fresh SessionInfo
/// payload instead of an encrypted response, signaling "your session
/// state is stale, here's the new state, please retry." Tesla's
/// reference client does the same refresh-and-retry dance.
///
/// We do at most one retry per query — if even with refreshed state
/// the retry still hits the same "needs refresh" outcome, something
/// deeper is wrong and we surface the error instead of looping
/// forever.
async fn signed_request_with_refresh_retry(
    state: &mut SessionState,
    domain: Domain,
    inner: Vec<u8>,
) -> Result<Vec<u8>> {
    let result = match try_signed_request_once(state, domain, &inner).await {
        Ok(QueryOutcome::Plaintext(bytes)) => Ok(bytes),
        Ok(QueryOutcome::SessionRefresh(info)) => {
            apply_session_refresh(state, domain, info)?;
            info!(
                "PersistentSession: retrying signed request to {:?} after SessionInfo refresh",
                domain
            );
            match try_signed_request_once(state, domain, &inner).await {
                Ok(QueryOutcome::Plaintext(bytes)) => Ok(bytes),
                Ok(QueryOutcome::SessionRefresh(_)) => {
                    bail!("car requested SessionInfo refresh twice in a row — giving up")
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    };
    if result.is_ok() {
        note_successful_query(state);
    }
    result
}

/// One of two normal outcomes from a signed query.
enum QueryOutcome {
    /// Decrypted response payload — pass it through to the caller.
    Plaintext(Vec<u8>),
    /// Car returned a fresh SessionInfo asking us to update our
    /// cached state and retry. Caller must call `apply_session_refresh`
    /// and re-issue the query.
    SessionRefresh(SessionInfo),
}

/// Apply a car-provided SessionInfo refresh: derive a new session
/// key, replace the cached domain state, reset the local handshake
/// clock so `estimated_car_clock` tracks the new baseline. Cheap —
/// no GATT traffic, just ECDH + a HashMap insert.
fn apply_session_refresh(
    state: &mut SessionState,
    domain: Domain,
    info: SessionInfo,
) -> Result<()> {
    let key = derive_session_key(&state.keypair.secret, &info.public_key)
        .context("deriving session key from refreshed SessionInfo")?;
    info!(
        "PersistentSession: refreshed {:?} session — counter={} clock_time={}",
        domain, info.counter, info.clock_time
    );
    state.domains.insert(
        domain,
        DomainSession {
            key,
            epoch: info.epoch,
            counter: info.counter,
            clock_time_at_handshake: info.clock_time,
            handshake_local_time: Instant::now(),
        },
    );
    Ok(())
}

async fn try_signed_request_once(
    state: &mut SessionState,
    domain: Domain,
    inner: &[u8],
) -> Result<QueryOutcome> {
    ensure_connected(state).await?;
    ensure_domain_session(state, domain).await?;

    let conn = state
        .conn
        .as_mut()
        .context("not connected after ensure_connected (bug)")?;
    let ds = state
        .domains
        .get_mut(&domain)
        .context("domain session not present after ensure_domain_session (bug)")?;

    let counter = ds.counter + 1;
    let expires_at = ds.estimated_car_clock().saturating_add(EXPIRES_WINDOW);

    let parts = auth::sign(
        &ds.key,
        &state.keypair.pub_uncompressed,
        inner,
        domain,
        state.vin.as_bytes(),
        &ds.epoch,
        expires_at,
        counter,
        QUERY_FLAGS,
    )?;

    let envelope = auth::build_signed_routable_message(&parts, domain, QUERY_FLAGS);

    debug!(
        "PersistentSession: TX domain={:?} inner_len={} counter={}",
        domain,
        inner.len(),
        counter
    );
    let resp_bytes = conn.round_trip(&envelope, QUERY_TIMEOUT).await?;

    // Counter advances on the wire whether the car accepts or rejects
    // the message — by the time the car responds, our `counter` value
    // is what it's seen. Update before checking fault so a retry uses
    // counter+1.
    ds.counter = counter;

    let rm = RoutableMessage::decode(resp_bytes.as_slice())
        .context("decoding response RoutableMessage")?;

    let fault = rm
        .signed_message_status
        .as_ref()
        .map(|s| s.signed_message_fault as u32)
        .unwrap_or(0);

    // Check for a SessionInfo refresh first — the car uses this as
    // the standard "your session is stale, here's fresh info" reply.
    // It's not an error; it's an instruction to refresh and retry.
    if let Some(routable_message::Payload::SessionInfo(info_bytes)) = &rm.payload {
        let parsed = SessionInfo::decode(info_bytes.as_slice())
            .context("decoding refreshed SessionInfo from car")?;
        return Ok(QueryOutcome::SessionRefresh(parsed));
    }

    if fault != 0 {
        // Counter/epoch faults are recoverable by re-handshaking the
        // domain. Drop our cached session state so the next query
        // re-runs the SessionInfoRequest exchange.
        const FAULT_INVALID_SIGNATURE: u32 = 5;
        const FAULT_INVALID_TOKEN_OR_COUNTER: u32 = 6;
        const FAULT_INCORRECT_EPOCH: u32 = 15;
        const FAULT_TIME_EXPIRED: u32 = 17;
        if matches!(
            fault,
            FAULT_INVALID_SIGNATURE
                | FAULT_INVALID_TOKEN_OR_COUNTER
                | FAULT_INCORRECT_EPOCH
                | FAULT_TIME_EXPIRED
        ) {
            warn!(
                "PersistentSession: domain {:?} returned recoverable fault {} — \
                 dropping session state, will re-handshake on next query",
                domain, fault
            );
            state.domains.remove(&domain);
        }
        bail!("car responded with fault code {}", fault);
    }

    // Pull out the encrypted payload + AES_GCM_Response sig data.
    let resp_sig = match rm.sub_sig_data.as_ref() {
        Some(routable_message::SubSigData::SignatureData(sd)) => {
            match sd.sig_type.as_ref() {
                Some(signature_data::SigType::AesGcmResponseData(r)) => r,
                Some(other) => bail!(
                    "response signature_data was not AES_GCM_Response — got {}. \
                     Full response hex: {}",
                    sig_type_name(other),
                    hex::encode(&resp_bytes),
                ),
                None => bail!(
                    "response signature_data has no sig_type. Full response hex: {}",
                    hex::encode(&resp_bytes),
                ),
            }
        }
        None => bail!(
            "response has no sub_sig_data at all. payload variant: {}. Full hex: {}",
            payload_variant_name(rm.payload.as_ref()),
            hex::encode(&resp_bytes),
        ),
    };

    let ciphertext = rm
        .payload
        .as_ref()
        .and_then(|p| match p {
            routable_message::Payload::ProtobufMessageAsBytes(b) => Some(b.as_slice()),
            _ => None,
        })
        .context("response missing encrypted payload")?;

    let from_domain = rm
        .from_destination
        .as_ref()
        .and_then(|d| d.sub_destination.as_ref())
        .and_then(|sd| match sd {
            destination::SubDestination::Domain(d) => Domain::try_from(*d).ok(),
            _ => None,
        })
        .unwrap_or(domain);

    let request_tag = match &parts.signature_data.sig_type {
        Some(signature_data::SigType::AesGcmPersonalizedData(p)) => p.tag.clone(),
        _ => unreachable!("we just signed with AES_GCM_PERSONALIZED"),
    };

    let plaintext = match auth::decrypt_response(
        &ds.key,
        &request_tag,
        from_domain,
        state.vin.as_bytes(),
        rm.flags,
        resp_sig.counter,
        fault,
        &resp_sig.nonce,
        &resp_sig.tag,
        ciphertext,
    ) {
        Ok(p) => p,
        Err(e) => {
            // Decrypt failure with valid-looking sig_data almost
            // always means our cached session state diverged from
            // the car's view (e.g. an interleaving client bumped
            // the car's counter or rolled the epoch). Drop the
            // domain state so the wrapper retries with a fresh
            // handshake and surface the original error so the
            // caller knows what happened.
            warn!(
                "PersistentSession: decrypt failed for {:?} — \
                 dropping domain state for re-handshake on retry",
                domain
            );
            state.domains.remove(&domain);
            return Err(e);
        }
    };

    debug!("PersistentSession: decrypted {} bytes", plaintext.len());
    Ok(QueryOutcome::Plaintext(plaintext))
}

/// Drops the held connection if `result` looks like a transport
/// failure (link dropped, BLE write to a closed handle, etc.). Next
/// command triggers a fresh scan + connect. Protocol-level faults
/// (INVALID_SIGNATURE, etc.) are handled separately inside the
/// query/body_controller handlers and don't drop the connection.
///
/// On every drop, emits a single structured log line summarizing the
/// connection's lifetime + freshness of last successful query +
/// running drop count. Testers paste their journalctl tail and we
/// can immediately distinguish slot contention (held=20m, last_ok=1s,
/// many drops) from a degraded link (held=20m, last_ok=45s, occasional
/// drops) from a flapping radio (held=10s repeatedly).
async fn handle_transport_error_if_any<T>(
    state: &mut SessionState,
    result: &Result<T>,
) {
    if let Err(e) = result {
        if state.conn.is_some() && is_transport_error(e) {
            state.lifetime_drops = state.lifetime_drops.saturating_add(1);
            let held_secs = state
                .connected_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let last_ok = state
                .last_successful_query_at
                .map(|t| format!("{}s", t.elapsed().as_secs()))
                .unwrap_or_else(|| "<never>".into());
            warn!(
                "PersistentSession: connection lost — \
                 held={}m{}s queries={} last_ok={}_ago drops_total={} reason={:#}",
                held_secs / 60,
                held_secs % 60,
                state.queries_since_connect,
                last_ok,
                state.lifetime_drops,
                e,
            );
            if let Some(conn) = state.conn.take() {
                conn.close().await;
            }
            state.domains.clear();
            state.connected_at = None;
            state.queries_since_connect = 0;
            // Intentionally NOT resetting last_successful_query_at —
            // the value across the drop is useful for the next
            // diagnostic line ("reconnected after Xs gap since last
            // working query").
        }
    }
}

/// Run a body-controller-state query through the held connection.
/// Mirrors `crate::body_controller::query` but uses the persistent
/// session's connection so we don't open a competing one and
/// trigger the bluez races that the per-call body-controller path
/// kept hitting.
async fn handle_body_controller(
    state: &mut SessionState,
) -> Result<crate::proto::vcsec::VehicleStatus> {
    ensure_connected(state).await?;
    let conn = state
        .conn
        .as_mut()
        .context("ensure_connected returned without a connection")?;
    let result = crate::body_controller::query(conn).await;
    if result.is_ok() {
        note_successful_query(state);
    }
    result
}

async fn ensure_connected(state: &mut SessionState) -> Result<()> {
    if state.conn.is_some() {
        return Ok(());
    }

    let adapter = scan::adapter_by_name(state.adapter_name.as_deref())
        .await
        .context("locating BLE adapter")?;
    // 30s scan window matches what the one-shot examples use; covers
    // a car waking from sleep + advertising stabilizing.
    let scan_result = match scan::scan_for_vin(&adapter, &state.vin, Duration::from_secs(30)).await
    {
        Ok(r) => r,
        Err(e) => {
            // Connect failure — back off before letting the caller
            // retry. Subsequent failures double the wait; success
            // resets it.
            sleep(state.backoff).await;
            state.backoff = (state.backoff * 2).min(RECONNECT_BACKOFF_MAX);
            return Err(e).context("scan failed");
        }
    };

    let conn = match Connection::open(scan_result.peripheral).await {
        Ok(c) => c,
        Err(e) => {
            sleep(state.backoff).await;
            state.backoff = (state.backoff * 2).min(RECONNECT_BACKOFF_MAX);
            return Err(e).context("connect failed");
        }
    };

    state.conn = Some(conn);
    state.backoff = RECONNECT_BACKOFF_MIN;
    state.connected_at = Some(Instant::now());
    state.queries_since_connect = 0;
    info!("PersistentSession: connected (held until link drops)");
    Ok(())
}

/// Increment the per-connection query counter and, every
/// `STATUS_LOG_EVERY_N_QUERIES`, emit a status line summarizing how
/// long the current connection has been held + how many queries it
/// has served. Operators can grep this to confirm the persistent
/// slot is being held vs being re-grabbed each cycle.
fn note_successful_query(state: &mut SessionState) {
    state.queries_since_connect = state.queries_since_connect.saturating_add(1);
    // Record the success time so the disconnect diagnostic can show
    // "last_ok=Xs ago" — distinguishes a clean drop (link was fine
    // until it suddenly wasn't) from a degraded link (queries were
    // already missing before the drop).
    state.last_successful_query_at = Some(Instant::now());
    let n = state.queries_since_connect;
    if n == 1 || n % STATUS_LOG_EVERY_N_QUERIES == 0 {
        let uptime = state
            .connected_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        info!(
            "PersistentSession: held for {}m{}s, {} queries served on this connection",
            uptime / 60,
            uptime % 60,
            n
        );
    }
}

async fn ensure_domain_session(state: &mut SessionState, domain: Domain) -> Result<()> {
    if state.domains.contains_key(&domain) {
        return Ok(());
    }
    let conn = state
        .conn
        .as_mut()
        .context("ensure_domain_session called without connection")?;

    info!("PersistentSession: handshake for {:?}", domain);
    let info = session::request_session_info(conn, &state.keypair, domain)
        .await
        .with_context(|| format!("session-info handshake for {:?}", domain))?;
    let key = derive_session_key(&state.keypair.secret, &info.parsed.public_key)
        .context("deriving session key")?;
    state.domains.insert(
        domain,
        DomainSession {
            key,
            epoch: info.parsed.epoch.clone(),
            counter: info.parsed.counter,
            clock_time_at_handshake: info.parsed.clock_time,
            handshake_local_time: Instant::now(),
        },
    );
    Ok(())
}

/// Human-readable name for a SignatureData::sig_type variant. Used
/// in error messages so an unexpected response shape tells us
/// exactly what shape it had instead of "missing X" guesswork.
fn sig_type_name(t: &signature_data::SigType) -> &'static str {
    match t {
        signature_data::SigType::AesGcmPersonalizedData(_) => "AES_GCM_PERSONALIZED",
        signature_data::SigType::AesGcmResponseData(_) => "AES_GCM_RESPONSE",
        signature_data::SigType::HmacPersonalizedData(_) => "HMAC_PERSONALIZED",
        signature_data::SigType::SessionInfoTag(_) => "SESSION_INFO_TAG (HMAC)",
    }
}

/// Human-readable name for a RoutableMessage::payload variant.
fn payload_variant_name(p: Option<&routable_message::Payload>) -> &'static str {
    match p {
        Some(routable_message::Payload::ProtobufMessageAsBytes(_)) => "ProtobufMessageAsBytes (encrypted)",
        Some(routable_message::Payload::SessionInfo(_)) => "SessionInfo (refresh)",
        Some(routable_message::Payload::SessionInfoRequest(_)) => "SessionInfoRequest",
        None => "<none>",
    }
}

/// Heuristic: does this error look like the BLE link dropped (vs a
/// fault returned by the car at the protocol level)? Used to decide
/// whether to drop the connection for the next query to reopen.
fn is_transport_error(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}");
    msg.contains("notification stream ended")
        || msg.contains("BLE write")
        || msg.contains("waiting for response")
        || msg.contains("not connected")
        || msg.contains("Peripheral")
}
