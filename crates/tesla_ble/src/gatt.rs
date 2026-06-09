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

/// The BLE 4.0 default ATT_MTU every stack accepts. Used as the TX
/// chunking basis whenever the negotiated MTU can't be read from bluez
/// (old bluez without the property, non-Linux dev host, busctl missing).
const ATT_DEFAULT_MTU: usize = 23;

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
    /// ATT MTU bluez negotiated for this connection (read from the
    /// GattCharacteristic1 `MTU` D-Bus property at open), or
    /// `ATT_DEFAULT_MTU` when it couldn't be read. Drives the TX chunk
    /// size: MTU 247 sends a typical signed envelope in one write
    /// instead of nine — measured ~920ms → ~400ms per query.
    att_mtu: usize,
    /// Whether the previous `round_trip` on this connection completed
    /// cleanly (a validated response was returned). Gates the pre-TX
    /// hygiene steps: after a clean exchange the notification stream
    /// holds no leftovers of ours, so the 100ms quiet-window drain and
    /// the is_connected D-Bus probe are pure latency — and any stray
    /// VCSEC broadcast that does arrive is discarded in-band by the
    /// RX filters anyway. After a timeout/error, a late response may
    /// still be in flight, so the next round_trip does the full drain.
    last_round_trip_clean: bool,
}

impl Connection {
    /// Connect to a peripheral previously found by `scan::scan_for_vin`,
    /// discover Tesla's service, find TX + RX characteristics, and
    /// subscribe to notifications.
    pub async fn open(peripheral: Peripheral) -> Result<Self> {
        info!("connecting to vehicle GATT");
        // Peer MAC for the bluez D-Bus object path — needed after
        // connect to look up the negotiated ATT MTU.
        let peer_mac = peripheral.address().to_string();
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
            att_mtu: ATT_DEFAULT_MTU,
            // The post-subscribe settle below leaves the line clean.
            last_round_trip_clean: true,
        };

        // Use the ATT MTU bluez actually negotiated, when it can tell
        // us. Only values above the floor are trusted; everything else
        // keeps the universally-safe 20-byte chunks.
        match query_negotiated_att_mtu(&peer_mac).await {
            Some(mtu) if mtu > ATT_DEFAULT_MTU => {
                info!(
                    "negotiated ATT MTU {} — TX in {}-byte chunks",
                    mtu,
                    mtu.saturating_sub(3).clamp(20, 512)
                );
                conn.att_mtu = mtu;
            }
            _ => {
                info!(
                    "negotiated ATT MTU unavailable from bluez — \
                     falling back to 20-byte TX chunks"
                );
            }
        }

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
        // Pre-TX hygiene, only when the line might be dirty: after an
        // unclean previous exchange (timeout/error — a late response
        // may still be in flight) or with bytes already sitting in the
        // reassembly buffer. After a clean exchange both steps are
        // skipped — that saves the full 100ms quiet window on every
        // query (~37% of a warm query's latency), and the RX-side
        // filters below still discard any unsolicited frame that
        // slips in between queries.
        let dirty = !self.last_round_trip_clean || !self.rx_buffer.is_empty();
        if dirty {
            self.drain_until_quiet(Duration::from_millis(100)).await;
        }
        // Pessimistically mark dirty until this exchange proves clean —
        // every early return below (write error, timeout, desync cap)
        // then forces the full drain on the next call.
        self.last_round_trip_clean = false;

        let framed = frame(payload);
        // Chunk at the ATT MTU bluez negotiated for this connection
        // (read from D-Bus at open — btleplug exposes no MTU API). The
        // car negotiates 247, so a typical signed envelope goes out in
        // one ATT write instead of nine 20-byte ones; with WithResponse
        // ACKs at ~50ms each that halves measured query latency. When
        // the negotiated value couldn't be read, `att_mtu` stays at the
        // BLE 4.0 default 23 → 20-byte chunks, which every stack
        // accepts (tesla-control's go-ble falls back to the same 20
        // when its MTU exchange fails). We never exceed what bluez
        // itself negotiated, so "write too large" rejections can't
        // happen — bluez enforces the negotiated MTU locally.
        let chunks = chunks_for_mtu(&framed, self.att_mtu);
        debug!(
            "TX framed ({} bytes in {} chunk(s)): {}",
            framed.len(),
            chunks.len(),
            hex::encode(&framed)
        );
        // Pre-write liveness check: if is_connected is false here, the
        // link died between subscribe and first TX (pair-auth failure,
        // phone-key slot race, or RF drop) — distinct from a write that
        // fails on a healthy link. Only worth a D-Bus round-trip when
        // the line is suspect; on a clean line the write itself surfaces
        // any failure just as clearly.
        if dirty {
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
        let result = timeout(wait, async {
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
        .context("waiting for response")?;

        // A validated response made it back and the reassembly buffer is
        // drained to a frame boundary — the next round_trip may skip the
        // pre-TX hygiene. (Leftover buffered bytes re-trigger it via the
        // rx_buffer check even with the flag set.)
        if result.is_ok() {
            self.last_round_trip_clean = true;
        }
        result
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

/// Run one `busctl` invocation, bounded to 3s so a hung D-Bus can't
/// stall the connect path. Returns stdout on success, None otherwise.
async fn busctl(args: &[&str]) -> Option<String> {
    use tokio::process::Command;
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new("busctl").args(args).output(),
    )
    .await;
    match result {
        Ok(Ok(o)) if o.status.success() => {
            Some(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        _ => None,
    }
}

/// Read the ATT MTU bluez negotiated for the connection to `peer_mac`,
/// via the `MTU` property bluez ≥5.62 exposes on GattCharacteristic1
/// D-Bus objects. btleplug has no MTU API, so this is the only window
/// into the negotiated value.
///
/// `busctl tree` lists the device's characteristic object paths; the
/// MTU is an ATT connection property, so bluez reports the same value
/// on every characteristic — the first one that answers wins. (Not
/// `GetManagedObjects` — busctl's JSON serializer rejects bluez's
/// reply with "Failed to create new json object".)
///
/// Returns `None` on any failure — busctl missing (non-Linux dev
/// host), old bluez without the property, parse mismatch — and the
/// caller stays on the universally-safe default.
async fn query_negotiated_att_mtu(peer_mac: &str) -> Option<usize> {
    // /org/bluez/hciX/dev_58_D1_5A_44_90_FC/serviceNNNN/charNNNN
    let dev_fragment = format!("/dev_{}/", peer_mac.replace(':', "_").to_uppercase());

    let tree = busctl(&["tree", "--list", "org.bluez"]).await?;
    for path in tree.lines().map(str::trim).filter(|p| {
        p.contains(&dev_fragment)
            && p.rsplit('/')
                .next()
                .is_some_and(|seg| seg.starts_with("char"))
    }) {
        let Some(prop) = busctl(&[
            "get-property",
            "org.bluez",
            path,
            "org.bluez.GattCharacteristic1",
            "MTU",
        ])
        .await
        else {
            continue;
        };
        // Output shape: `q 247`
        if let Some(v) = prop
            .trim()
            .strip_prefix("q ")
            .and_then(|n| n.parse::<usize>().ok())
        {
            return Some(v);
        }
    }
    None
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
