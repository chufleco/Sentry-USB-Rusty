//! BLE GATT connection layer for Tesla cars.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use btleplug::api::{
    Characteristic, Peripheral as _, ValueNotification, WriteType,
};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::transport::{chunks_for_mtu, frame, try_unframe};
use crate::uuids;

/// Hard wall-clock cap on a single `peripheral.connect()` attempt.
///
/// btleplug delegates to bluez, which defaults to a ~30s connect
/// timeout. That's catastrophic during slot contention: when a phone
/// key is sitting on a Tesla BLE slot, every connect attempt blocks
/// 30s before failing, so we get ~28 retries in 14 minutes instead
/// of the ~150+ a tight timeout allows. The shorter we fail, the
/// more chances we have to win the slot in the brief window the
/// phone is silent (advertising, switching channels, etc.).
///
/// 8s is a balance: long enough that a genuinely-reachable car with
/// a normal-quality link succeeds on the first try (real connects
/// take 1-3s), short enough that a slot-blocked attempt fails fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Wire-format prefix of any RoutableMessage whose `to_destination`
/// is `Destination { sub_destination: Domain(BROADCAST=0) }`.
///
/// Bytes:
///   0x32 = (field 6 << 3) | 2 = to_destination, length-delimited
///   0x02 = length 2
///   0x08 = (field 1 << 3) | 0 = Destination.sub_destination.domain, varint
///   0x00 = varint 0 = BROADCAST
///
/// The car's VEHICLE_SECURITY subsystem sometimes broadcasts state
/// notifications (charge state changes, sentry events, etc.) to the
/// whole BLE link with this prefix. If round_trip returned one to
/// the manager as our "response," manager would try to decrypt it
/// as a signed-query reply and fail with `"response has no
/// sub_sig_data at all"` — broadcasts aren't authenticated and have
/// no nonce/tag for AES-GCM. Discarding them at the transport layer
/// keeps the manager's response-handling path simple.
///
/// Legitimate replies use `routing_address` for `to_destination`
/// (a 16-byte UUID), which encodes as `32 12 12 10 <uuid>` — the
/// byte at offset 2 is `0x12` (field 2 tag), not `0x08`, so this
/// prefix doesn't false-match real responses.
const BROADCAST_FRAME_PREFIX: &[u8] = &[0x32, 0x02, 0x08, 0x00];

/// Established BLE GATT connection to a Tesla car.
pub struct Connection {
    peripheral: Peripheral,
    tx_char: Characteristic,
    rx_stream: futures::stream::BoxStream<'static, ValueNotification>,
    rx_buffer: Vec<u8>,
    /// Running tally of "buffer started with a too-large length prefix,
    /// we cleared and continued" events since this counter was last
    /// drained. Caller (PersistentSession::run_session_task) reads via
    /// `take_framing_desync_recoveries()` and folds into a lifetime
    /// total surfaced in the live status file + periodic status log.
    /// Lives on Connection (not SessionState) so it captures
    /// every recovery — even ones that happen on a query that
    /// ultimately times out.
    framing_desync_recoveries: u32,
}

impl Connection {
    /// Connect to a peripheral previously found by `scan::scan_for_vin`,
    /// discover Tesla's service, find TX + RX characteristics, and
    /// subscribe to notifications.
    pub async fn open(peripheral: Peripheral) -> Result<Self> {
        info!("connecting to vehicle GATT");
        // Wrap btleplug's connect() in our own timeout. btleplug uses
        // bluez's default (~30s) which is far too long when racing a
        // phone key for a slot. On the success path real connects
        // land in 1-3s, so 8s is a generous cap that fails fast on
        // slot contention without false-failing healthy connects.
        let started = std::time::Instant::now();
        match timeout(CONNECT_TIMEOUT, peripheral.connect()).await {
            Ok(Ok(())) => {
                debug!(
                    "BLE connect succeeded in {}ms",
                    started.elapsed().as_millis()
                );
            }
            Ok(Err(e)) => return Err(e).context("BLE connect"),
            Err(_) => {
                // Best-effort cleanup so bluez doesn't leak a
                // half-open connection slot on its side, which would
                // make the *next* attempt fail with "already
                // connecting".
                let _ = peripheral.disconnect().await;
                warn!(
                    "BLE connect timed out after {}ms (slot likely held by phone key)",
                    started.elapsed().as_millis()
                );
                bail!(
                    "BLE connect timed out after {}s — slot likely held by another client",
                    CONNECT_TIMEOUT.as_secs()
                );
            }
        }
        peripheral
            .discover_services()
            .await
            .context("GATT service discovery")?;

        // Find Tesla's TX (we → car) + RX (car → us) characteristics.
        let chars = peripheral.characteristics();
        let tx_char = chars
            .iter()
            .find(|c| c.uuid == uuids::TO_VEHICLE)
            .cloned()
            .context("TO_VEHICLE characteristic not found — wrong device?")?;
        let rx_char = chars
            .iter()
            .find(|c| c.uuid == uuids::FROM_VEHICLE)
            .cloned()
            .context("FROM_VEHICLE characteristic not found — wrong device?")?;

        // Subscribe to FROM_VEHICLE notifications.
        peripheral
            .subscribe(&rx_char)
            .await
            .context("subscribe to FROM_VEHICLE notifications")?;
        let rx_stream = peripheral
            .notifications()
            .await
            .context("create notification stream")?;

        let mut conn = Self {
            peripheral,
            tx_char,
            rx_stream,
            rx_buffer: Vec::with_capacity(512),
            framing_desync_recoveries: 0,
        };

        // One-time post-subscribe settle: bluez can emit a subscribe-
        // complete notification (or an initial GATT indication burst)
        // 50-200ms after the subscribe() returns. If we don't drain
        // them, the first round_trip's receive loop picks them up as
        // garbage prefix bytes and mis-parses the framing — producing
        // an "empty RoutableMessage with all fields None" error.
        // 300ms quiet window is enough on every Pi bluez version
        // we've tested.
        conn.drain_until_quiet(Duration::from_millis(300)).await;

        debug!("GATT ready");
        Ok(conn)
    }

    /// Drain pending notifications and clear the unframe buffer.
    /// Used before TX in `round_trip` (short quiet window — just clearing
    /// in-flight stragglers between commands) and after subscribe in
    /// `open` (longer quiet window — catches bluez's post-subscribe
    /// notification burst). See `quiet_window` discussion in caller
    /// sites for which to use.
    async fn drain_until_quiet(&mut self, quiet_window: Duration) {
        let mut drained = 0;
        loop {
            match timeout(quiet_window, self.rx_stream.next()).await {
                Ok(Some(n)) => {
                    drained += 1;
                    debug!(
                        "drained stale notification #{} on {} ({} bytes)",
                        drained,
                        n.uuid,
                        n.value.len()
                    );
                }
                // Timed out (queue quiet for `quiet_window`) or stream
                // closed — done.
                _ => break,
            }
        }
        // Reset the unframe buffer too in case a partial frame is
        // sitting there from a stale notification.
        self.rx_buffer.clear();
        if drained > 0 {
            debug!("drained {} stale notification(s)", drained);
        }
    }

    /// Send a framed payload (handles chunking) and wait for the next
    /// complete response frame to come back. Times out after `wait`.
    ///
    /// `accept` is a caller-supplied check: every unframed candidate
    /// is run through it; if it returns false, the frame is discarded
    /// and we keep listening for the next one. This is how callers
    /// implement "drop frames that don't look like my expected
    /// response shape" (e.g. RoutableMessage::decode succeeds). Pass
    /// `|_| true` to accept everything.
    ///
    /// Real failure modes the validator catches:
    ///   * Late BLE notifications that snuck in after our previous
    ///     query completed and now read as a "frame" between our
    ///     TX and the actual response.
    ///   * Framing desync from a dropped/reordered BLE notification —
    ///     the unframed payload is garbage bytes that happen to satisfy
    ///     the 2-byte length prefix but won't decode as our protocol.
    ///   * Unsolicited messages from the car we don't know how to
    ///     interpret (anything other than the broadcast pattern that
    ///     the BROADCAST_FRAME_PREFIX special-case already handles).
    pub async fn round_trip<F>(
        &mut self,
        payload: &[u8],
        wait: Duration,
        accept: F,
    ) -> Result<Vec<u8>>
    where
        F: Fn(&[u8]) -> bool,
    {
        // Drain anything queued before we TX, otherwise the first
        // `next()` after our send could return a stale frame from
        // a prior unrelated request and we'd parse that as our
        // response. 100ms quiet window (was 50ms) — slightly more
        // headroom for stragglers that arrive just after the previous
        // round_trip returned. Cheap latency cost (≈50ms per query)
        // for fewer desyncs.
        self.drain_until_quiet(Duration::from_millis(100)).await;

        let framed = frame(payload);
        // Chunk at the BLE default ATT_MTU (23) — 20 byte payload per
        // ATT write after the 3-byte ATT header. This matches what
        // tesla-control's go-ble library does when its explicit MTU
        // exchange fails: see vehicle-command/pkg/connector/ble/ble.go
        // tryToConnect():
        //
        //     txMtu, err := client.ExchangeMTU(maxBLEMTUSize)
        //     if err != nil {
        //         conn.blockLength = ble.DefaultMTU - 3 // = 20
        //     } else {
        //         conn.blockLength = min(txMtu, ...) - 3
        //     }
        //
        // Why we use the conservative default unconditionally:
        // btleplug 0.11.x does NOT expose an MTU exchange API at all
        // (no `peripheral.request_mtu()`, no way to query the
        // negotiated MTU after connect). On bluez setups where the
        // stack didn't auto-negotiate a larger MTU during service
        // discovery — which depends on bluez version, kernel, chip
        // firmware, and post-pair adapter state — our previous
        // hardcoded 247 produced WriteWithoutResponse calls of
        // 80-200+ bytes, which bluez rejects with "Failed to
        // initiate write" because the data can't fit in a single
        // ATT_Write_Cmd at the default MTU.
        //
        // This was THE root cause of the tester's 323-drop streak:
        // all "BLE write: Failed to initiate write" within 100ms of
        // connect. tesla-control on the same hardware/pair worked
        // because go-ble's WriteCharacteristic path also chunks to
        // blockLength regardless of MTU exchange outcome.
        //
        // 20-byte chunks cost a few extra ATT_Write_Cmds per query
        // (a ~100 byte session-info request goes from 1 write to 5)
        // but is universally accepted. Once btleplug exposes MTU
        // exchange (or we drop to D-Bus to call bluez's RequestMTU
        // ourselves) we can bump this back up.
        const ATT_DEFAULT_PAYLOAD: usize = 23;
        let chunks = chunks_for_mtu(&framed, ATT_DEFAULT_PAYLOAD);
        debug!(
            "TX framed ({} bytes in {} chunk(s)): {}",
            framed.len(),
            chunks.len(),
            hex::encode(&framed)
        );
        // Pre-write liveness check. Diagnostic: when a tester sees
        // a wall of "BLE write: Failed to initiate write" errors that
        // fire within ~100ms of "connected", we want to know whether
        // bluez had already marked the link dead before we even tried
        // — that's a totally different bug class from "the write
        // itself failed on a healthy link." If is_connected returns
        // false here, the LL link died between our subscribe and the
        // first TX, which usually means either Tesla actively dropped
        // us (bad pair, phone-key slot contention) or the RF link
        // failed (range, interference, supervision timeout).
        match self.peripheral.is_connected().await {
            Ok(true) => {
                debug!("pre-TX is_connected=true; proceeding with {} chunk(s)", chunks.len());
            }
            Ok(false) => {
                bail!(
                    "BLE write: peripheral.is_connected()=false before TX — \
                     link died between subscribe and first write (typically \
                     pair-auth failure, phone-key slot race, or RF drop)"
                );
            }
            Err(e) => {
                // Probe failed — log but proceed; the write below
                // will surface the real error if there is one.
                debug!("pre-TX is_connected() probe errored ({}), proceeding anyway", e);
            }
        }
        for chunk in chunks {
            self.peripheral
                .write(&self.tx_char, chunk, WriteType::WithoutResponse)
                .await
                .context("BLE write")?;
        }

        // Receive until we have a complete framed payload.
        //
        // `desyncs` counts how many times we've hit a too-large length
        // prefix in this single round_trip. We recover inline (clear
        // the polluting bytes, keep RX'ing for the real response)
        // rather than bailing — the wall-clock `timeout` still bounds
        // total wait time, and recovering in-flight lets a Tesla query
        // succeed even when bluez handed us a partial-frame straggler
        // right after our drain exited.
        //
        // Why this matters in practice: tester bundles showed
        // sample_charge / sample_climate failing on EVERY tick (373
        // and 377 failures vs 0 successes for charge in a 10h window)
        // while sample_drive / sample_closures / sample_tires worked
        // fine. The pattern was a single 100-150 byte stale
        // notification landing in rx_buffer between drain end and
        // Tesla's actual response — try_unframe read its first 2
        // bytes as a length (always > 1024 since the bytes were
        // mid-payload garbage), the old code bailed, and the whole
        // query failed. With inline recovery, the buffer gets cleared
        // and we wait for the next notification — which IS Tesla's
        // real response — and the query succeeds.
        //
        // We do cap recovery at MAX_DESYNCS to avoid pathological
        // spinning if the link is genuinely flooded with garbage; at
        // that point the wall-clock timeout should fire anyway, but
        // an explicit cap makes the failure mode "loud" instead of
        // "silently maxed out at the timeout boundary."
        //
        // 256 (was 16): the Rock Pi 4C+ on kernel 6.18 with its
        // BCM4345C0 chip can emit 50+ garbage notifications per real
        // response — apparently the chip duplicates frames OR splits
        // them at weird boundaries, so bluez delivers the same
        // partial-frame bytes to us many times in a few milliseconds.
        // At MAX_DESYNCS=16 we'd bail before the real response could
        // accumulate. A tester bundle showed desync_recoveries=5956
        // in 49 minutes (122/min average, peaks much higher) with 0
        // successful queries because every round_trip hit the old
        // cap. 256 gives the recovery loop enough headroom for the
        // chip's noise without sacrificing the wall-clock timeout
        // safety net. On clean stacks (Pi 5 USB dongle, etc.) we
        // never get close to 16 desyncs in one round_trip, so this
        // doesn't change behavior there.
        const MAX_DESYNCS: u32 = 256;
        let mut desyncs: u32 = 0;
        timeout(wait, async {
            loop {
                let unframed = match try_unframe(&mut self.rx_buffer) {
                    Ok(v) => v,
                    Err(e) => {
                        // try_unframe fails when the length prefix says
                        // something insane (> 1024). Bytes from one
                        // frame are being interpreted as a length
                        // prefix of another — most often a stale
                        // notification that snuck in after drain
                        // exited. Clear the polluting bytes and KEEP
                        // RX'ing within this same round_trip; Tesla's
                        // real response is usually the next chunk to
                        // arrive.
                        let head_hex = hex::encode(
                            &self.rx_buffer[..self.rx_buffer.len().min(64)],
                        );
                        warn!(
                            "framing desync: try_unframe rejected {} buffer bytes \
                             ({}); head: {}… — clearing buffer, continuing to RX \
                             within the same round_trip",
                            self.rx_buffer.len(),
                            e,
                            head_hex,
                        );
                        self.rx_buffer.clear();
                        desyncs += 1;
                        self.framing_desync_recoveries =
                            self.framing_desync_recoveries.saturating_add(1);
                        if desyncs > MAX_DESYNCS {
                            return Err(e).context(format!(
                                "exceeded {MAX_DESYNCS} framing desyncs in one round_trip — \
                                 giving up so caller can re-handshake"
                            ));
                        }
                        continue;
                    }
                };
                if let Some(payload) = unframed {
                    // Tesla never sends RoutableMessages this small —
                    // the minimum useful response has at least a
                    // to_destination + uuid + status, which is well
                    // over 8 bytes. A < 8-byte "frame" is almost
                    // always bluez's subscribe-complete leakage or a
                    // similar internal notification we mis-interpreted
                    // as the length prefix of a real frame. Discard
                    // and keep listening.
                    if payload.len() < 8 {
                        debug!(
                            "ignoring suspiciously short frame ({} bytes): {} — \
                             treating as framing desync, continuing to RX",
                            payload.len(),
                            hex::encode(&payload)
                        );
                        continue;
                    }
                    // Drop VCSEC's unsolicited broadcast notifications
                    // before returning to the caller. These arrive
                    // mid-query (especially right after connect, when
                    // VCSEC bursts state-change events) and would
                    // poison the response decoder with "no sub_sig_data"
                    // errors. See BROADCAST_FRAME_PREFIX comment for
                    // the byte-level reasoning.
                    if payload.starts_with(BROADCAST_FRAME_PREFIX) {
                        debug!(
                            "discarding VCSEC BROADCAST notification ({} bytes): {} — \
                             not a response to our request, continuing to RX",
                            payload.len(),
                            hex::encode(&payload[..payload.len().min(48)])
                        );
                        continue;
                    }
                    // Caller-supplied shape check. If the bytes don't
                    // look like the protocol message the caller is
                    // expecting (e.g. RoutableMessage::decode fails),
                    // discard the frame and keep RX'ing. This catches
                    // framing desyncs that happen to produce a
                    // valid-LENGTH-but-garbage-CONTENT payload — the
                    // common pattern we saw on bluez 5.82 post-reconnect.
                    if !accept(&payload) {
                        debug!(
                            "validator rejected frame ({} bytes), continuing to RX: head={}",
                            payload.len(),
                            hex::encode(&payload[..payload.len().min(48)])
                        );
                        continue;
                    }
                    debug!("unframed payload ({} bytes): {}", payload.len(), hex::encode(&payload));
                    return Ok::<_, anyhow::Error>(payload);
                }
                let Some(n) = self.rx_stream.next().await else {
                    bail!("notification stream ended");
                };
                if n.uuid != uuids::FROM_VEHICLE {
                    debug!("ignoring notification on other char {}", n.uuid);
                    continue;
                }
                debug!("RX chunk ({} bytes): {}", n.value.len(), hex::encode(&n.value));
                self.rx_buffer.extend_from_slice(&n.value);
            }
        })
        .await
        .context("waiting for response")?
    }

    /// Best-effort disconnect. Safe to call multiple times.
    pub async fn close(self) {
        let _ = self.peripheral.disconnect().await;
        // Tiny grace period to let bluez clean up its connection state.
        sleep(Duration::from_millis(100)).await;
    }

    /// Read-and-reset the per-connection framing-desync recovery
    /// counter. Returns how many in-round_trip recoveries fired since
    /// the last call (zero if the link has been clean). Used by
    /// PersistentSession to roll the per-connection count into a
    /// session-lifetime total that's surfaced in the live status
    /// file and periodic status log — gives us visibility into how
    /// often the recovery path is actually being exercised in the
    /// wild, vs being a dead-code safety net.
    pub fn take_framing_desync_recoveries(&mut self) -> u32 {
        std::mem::take(&mut self.framing_desync_recoveries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_prefix_matches_real_capture() {
        // Exact bytes captured from a failing tester's bundle:
        // a VCSEC unsolicited broadcast that previously poisoned
        // our response decoder with "no sub_sig_data at all".
        let raw = hex::decode(
            "320208003a020802521f1a1d12160a14d261fa622f06da46cf0cf4751ab79e3d8e7a46801802220101",
        )
        .unwrap();
        assert!(
            raw.starts_with(BROADCAST_FRAME_PREFIX),
            "filter must catch the exact bytes the bug fired on"
        );
    }

    #[test]
    fn broadcast_prefix_does_not_match_routing_address_to_destination() {
        // A normal reply to us has to_destination = routing_address
        // (16-byte UUID). Encoding: 32 12 12 10 <uuid bytes>.
        // The byte at offset 2 is 0x12 (field 2 tag, routing_address)
        // — not 0x08 — so the broadcast filter must NOT match.
        let normal_reply_prefix: [u8; 4] = [0x32, 0x12, 0x12, 0x10];
        assert!(!normal_reply_prefix.starts_with(BROADCAST_FRAME_PREFIX));
    }

    #[test]
    fn broadcast_prefix_does_not_match_other_domain() {
        // A frame addressed to (some other) Domain, e.g. INFOTAINMENT(3),
        // would encode as 32 02 08 03. Even though the first three
        // bytes match, the fourth doesn't — only the literal
        // BROADCAST (=0) variant should be filtered.
        let to_infotainment: [u8; 4] = [0x32, 0x02, 0x08, 0x03];
        assert!(!to_infotainment.starts_with(BROADCAST_FRAME_PREFIX));
    }
}
