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

/// Established BLE GATT connection to a Tesla car.
pub struct Connection {
    peripheral: Peripheral,
    tx_char: Characteristic,
    rx_stream: futures::stream::BoxStream<'static, ValueNotification>,
    rx_buffer: Vec<u8>,
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
    pub async fn round_trip(&mut self, payload: &[u8], wait: Duration) -> Result<Vec<u8>> {
        // Drain anything queued before we TX, otherwise the first
        // `next()` after our send could return a stale frame from
        // a prior unrelated request and we'd parse that as our
        // response. 50ms quiet window — enough to consume any
        // stragglers from the previous round_trip without adding
        // meaningful latency.
        self.drain_until_quiet(Duration::from_millis(50)).await;

        let framed = frame(payload);
        // Tesla supports MTU up to 247; we'd negotiate that during
        // service discovery. btleplug doesn't currently expose the
        // negotiated MTU directly, so we conservatively chunk for 247
        // — Tesla's preferred max.
        const MTU: usize = 247;
        let chunks = chunks_for_mtu(&framed, MTU);
        debug!(
            "TX framed ({} bytes in {} chunk(s)): {}",
            framed.len(),
            chunks.len(),
            hex::encode(&framed)
        );
        for chunk in chunks {
            self.peripheral
                .write(&self.tx_char, chunk, WriteType::WithoutResponse)
                .await
                .context("BLE write")?;
        }

        // Receive until we have a complete framed payload.
        timeout(wait, async {
            loop {
                if let Some(payload) = try_unframe(&mut self.rx_buffer)? {
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
}
