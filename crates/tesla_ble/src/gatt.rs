//! BLE GATT connection layer for Tesla cars.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use btleplug::api::{
    Characteristic, Peripheral as _, ValueNotification, WriteType,
};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use tokio::time::{sleep, timeout};
use tracing::{debug, info};

use crate::transport::{chunks_for_mtu, frame, try_unframe};
use crate::uuids;

/// Established BLE GATT connection to a Tesla car.
pub struct Connection {
    peripheral: Peripheral,
    tx_char: Characteristic,
    rx_char: Characteristic,
    rx_stream: futures::stream::BoxStream<'static, ValueNotification>,
    rx_buffer: Vec<u8>,
}

impl Connection {
    /// Connect to a peripheral previously found by `scan::scan_for_vin`,
    /// discover Tesla's service, find TX + RX characteristics, and
    /// subscribe to notifications.
    pub async fn open(peripheral: Peripheral) -> Result<Self> {
        info!("connecting to vehicle GATT");
        peripheral
            .connect()
            .await
            .context("BLE connect")?;
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

        debug!("GATT ready");
        Ok(Self {
            peripheral,
            tx_char,
            rx_char,
            rx_stream,
            rx_buffer: Vec::with_capacity(512),
        })
    }

    /// Send a framed payload (handles chunking) and wait for the next
    /// complete response frame to come back. Times out after `wait`.
    pub async fn round_trip(&mut self, payload: &[u8], wait: Duration) -> Result<Vec<u8>> {
        let framed = frame(payload);
        // Tesla supports MTU up to 247; we'd negotiate that during
        // service discovery. btleplug doesn't currently expose the
        // negotiated MTU directly, so we conservatively chunk for 247
        // — Tesla's preferred max.
        const MTU: usize = 247;
        let chunks = chunks_for_mtu(&framed, MTU);
        debug!("sending {} bytes in {} chunk(s)", framed.len(), chunks.len());
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
                    return Ok::<_, anyhow::Error>(payload);
                }
                let Some(n) = self.rx_stream.next().await else {
                    bail!("notification stream ended");
                };
                if n.uuid != uuids::FROM_VEHICLE {
                    debug!("ignoring notification on other char {}", n.uuid);
                    continue;
                }
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
