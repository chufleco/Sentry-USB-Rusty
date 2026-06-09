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

/// Wall-clock cap on a single `peripheral.connect()`. bluez defaults to
/// ~30s, which wastes retries under slot contention — failing fast lets
/// us re-attempt many more times in the brief windows the phone key is
/// silent. 8s clears real connects (1-3s) but bails quickly on a held
/// slot.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Wire prefix of a RoutableMessage with `to_destination =
/// Domain(BROADCAST=0)`:
///   0x32 = to_destination, length-delimited (field 6)
///   0x02 = length 2
///   0x08 = sub_destination.domain, varint (field 1)
///   0x00 = BROADCAST
///
/// VCSEC broadcasts unsolicited state notifications with this prefix.
/// They're unauthenticated (no AES-GCM nonce/tag), so returning one as
/// a "response" would fail decryption with "no sub_sig_data"; discard
/// them at the transport layer. Real replies use routing_address
/// (`32 12 12 10 <uuid>`), whose offset-2 byte is 0x12 not 0x08, so
/// this prefix doesn't false-match them.
const BROADCAST_FRAME_PREFIX: &[u8] = &[0x32, 0x02, 0x08, 0x00];

/// Established BLE GATT connection to a Tesla car.
pub struct Connection {
    peripheral: Peripheral,
    tx_char: Characteristic,
    rx_stream: futures::stream::BoxStream<'static, ValueNotification>,
    rx_buffer: Vec<u8>,
    /// Framing-desync recoveries (oversized length prefix cleared and
    /// retried) since the last drain. Read via
    /// `take_framing_desync_recoveries()`. Lives on Connection so it
    /// captures recoveries even on a query that later times out.
    framing_desync_recoveries: u32,
}

impl Connection {
    /// Connect to a peripheral previously found by `scan::scan_for_vin`,
    /// discover Tesla's service, find TX + RX characteristics, and
    /// subscribe to notifications.
    pub async fn open(peripheral: Peripheral) -> Result<Self> {
        info!("connecting to vehicle GATT");
        // Wrap connect() in our own timeout — see CONNECT_TIMEOUT.
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
                // Clean up so bluez doesn't leak a half-open slot,
                // which would fail the next attempt with "already
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

        // Post-subscribe settle: bluez can emit a subscribe-complete
        // notification 50-200ms after subscribe() returns. Drain it, or
        // the first round_trip mis-parses it as frame prefix bytes.
        conn.drain_until_quiet(Duration::from_millis(300)).await;

        debug!("GATT ready");
        Ok(conn)
    }

    /// Drain pending notifications and clear the unframe buffer. Called
    /// before TX in `round_trip` (short window, clears stragglers) and
    /// after subscribe in `open` (longer window, catches bluez's
    /// post-subscribe burst).
    ///
    /// Bounded by `MAX_DRAIN_TOTAL` wall-clock: the loop only exits when
    /// the stream goes quiet for a full `quiet_window`, so a car emitting
    /// unsolicited VCSEC broadcasts faster than the window would
    /// otherwise pin us here forever — and this runs *outside* the
    /// caller's response timeout, so nothing above us would fire.
    async fn drain_until_quiet(&mut self, quiet_window: Duration) {
        const MAX_DRAIN_TOTAL: Duration = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let mut drained = 0;
        loop {
            if started.elapsed() >= MAX_DRAIN_TOTAL {
                warn!(
                    "drain_until_quiet: still receiving after {}ms ({} notifications \
                     drained) — proceeding anyway; the response validator will \
                     discard remaining stragglers",
                    started.elapsed().as_millis(),
                    drained,
                );
                break;
            }
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
    /// The validator catches late notifications from a prior query,
    /// framing desyncs (garbage that satisfies the length prefix but
    /// won't decode), and unsolicited car messages.
    pub async fn round_trip<F>(
        &mut self,
        payload: &[u8],
        wait: Duration,
        accept: F,
    ) -> Result<Vec<u8>>
    where
        F: Fn(&[u8]) -> bool,
    {
        // Drain queued notifications before TX so a stale frame from a
        // prior request isn't parsed as our response. 100ms window
        // trades ~50ms latency for fewer desyncs.
        self.drain_until_quiet(Duration::from_millis(100)).await;

        let framed = frame(payload);
        // Chunk at the BLE default ATT_MTU (23) = 20-byte payload after
        // the 3-byte ATT header. btleplug 0.11.x exposes no MTU-exchange
        // API and no way to read the negotiated MTU, so we can't assume
        // a larger one: on stacks that didn't auto-negotiate up, writes
        // above ~20 bytes are rejected with "Failed to initiate write".
        // 20-byte chunks cost a few extra ATT writes per query but are
        // universally accepted. (tesla-control's go-ble falls back to
        // the same 20 when its MTU exchange fails.)
        const ATT_DEFAULT_PAYLOAD: usize = 23;
        let chunks = chunks_for_mtu(&framed, ATT_DEFAULT_PAYLOAD);
        debug!(
            "TX framed ({} bytes in {} chunk(s)): {}",
            framed.len(),
            chunks.len(),
            hex::encode(&framed)
        );
        // Pre-write liveness check: if is_connected is false here, the
        // link died between subscribe and first TX (pair-auth failure,
        // phone-key slot race, or RF drop) — distinct from a write that
        // fails on a healthy link.
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
            // WithResponse (ATT_Write_Req per chunk), not fire-and-forget
            // ATT_Write_Cmd: forces Tesla to ACK each chunk before the
            // next, so chunks can't be silently lost. Lost chunks made
            // Tesla wait ~14s for a request it thought incomplete, past
            // our round_trip timeout. Costs ~50ms/chunk. Matches
            // tesla-control (noRsp=false).
            self.peripheral
                .write(&self.tx_char, chunk, WriteType::WithResponse)
                .await
                .context("BLE write")?;
        }

        // Receive until we have a complete framed payload.
        //
        // `desyncs` counts too-large length prefixes hit in this
        // round_trip. We recover inline (clear the polluting bytes, keep
        // RX'ing) rather than bail — a stale straggler landing in
        // rx_buffer between drain and Tesla's real response would
        // otherwise fail the whole query. Capped at MAX_DESYNCS so a
        // genuinely flooded link fails loudly; the wall-clock timeout is
        // the real safety net. 64 gives headroom for chip/bluez stacks
        // that are noisy on the RX side.
        const MAX_DESYNCS: u32 = 64;
        let mut desyncs: u32 = 0;
        timeout(wait, async {
            loop {
                let unframed = match try_unframe(&mut self.rx_buffer) {
                    Ok(v) => v,
                    Err(e) => {
                        // try_unframe fails on an insane length prefix
                        // (> 1024) — usually a stale notification's bytes
                        // read as a length. Clear and keep RX'ing; the
                        // real response is usually next.
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
                    // Tesla responses are well over 8 bytes; a shorter
                    // "frame" is almost always bluez subscribe-complete
                    // leakage misread as a length prefix. Discard.
                    if payload.len() < 8 {
                        debug!(
                            "ignoring suspiciously short frame ({} bytes): {} — \
                             treating as framing desync, continuing to RX",
                            payload.len(),
                            hex::encode(&payload)
                        );
                        continue;
                    }
                    // Drop VCSEC broadcast notifications — they arrive
                    // mid-query and would poison the decoder with "no
                    // sub_sig_data". See BROADCAST_FRAME_PREFIX.
                    if payload.starts_with(BROADCAST_FRAME_PREFIX) {
                        debug!(
                            "discarding VCSEC BROADCAST notification ({} bytes): {} — \
                             not a response to our request, continuing to RX",
                            payload.len(),
                            hex::encode(&payload[..payload.len().min(48)])
                        );
                        continue;
                    }
                    // Caller shape check: discard frames that don't
                    // decode as the expected message (valid-length but
                    // garbage-content desyncs, seen on bluez 5.82).
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

    /// Read and reset the framing-desync recovery counter.
    /// PersistentSession folds this into a session-lifetime total for
    /// the status log.
    pub fn take_framing_desync_recoveries(&mut self) -> u32 {
        std::mem::take(&mut self.framing_desync_recoveries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_prefix_matches_real_capture() {
        // Real VCSEC broadcast bytes that previously poisoned the
        // decoder with "no sub_sig_data at all".
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
