//! Tesla BLE message framing: 2-byte big-endian length prefix + payload.
//!
//! Messages above the BLE MTU are chunked across multiple GATT writes
//! (TX) or notifications (RX), reassembled into the full payload using
//! the length prefix.

use anyhow::{Result, bail};

/// Wrap `payload` in the 2-byte length prefix Tesla expects.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u16;
    let mut out = Vec::with_capacity(2 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Try to extract one complete payload from `buf`. Returns:
/// - `Ok(Some(payload))` and removes those bytes from the front of `buf`
/// - `Ok(None)` if we haven't received enough bytes yet
/// - `Err(_)` if the buffer contains garbage
pub fn try_unframe(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if len > 1024 {
        bail!("framed payload claims length {len}, refusing (cap is 1024)");
    }
    if buf.len() < 2 + len {
        return Ok(None);
    }
    let payload = buf[2..2 + len].to_vec();
    buf.drain(..2 + len);
    Ok(Some(payload))
}

/// Split `frame` into MTU-sized GATT writes. ATT overhead is 3 bytes,
/// so the chunk payload is `mtu - 3` bytes max.
pub fn chunks_for_mtu(frame: &[u8], mtu: usize) -> Vec<&[u8]> {
    let chunk_size = mtu.saturating_sub(3).max(20);
    frame.chunks(chunk_size).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let payload = b"hello world";
        let framed = frame(payload);
        assert_eq!(&framed[..2], &(payload.len() as u16).to_be_bytes());
        let mut buf = framed;
        let out = try_unframe(&mut buf).unwrap();
        assert_eq!(out.as_deref(), Some(payload.as_ref()));
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_buffer() {
        let mut buf = vec![0x00, 0x05, 0x68, 0x65]; // length=5, only 2 of 5 bytes
        assert!(try_unframe(&mut buf).unwrap().is_none());
    }

    #[test]
    fn rejects_oversized_length() {
        let mut buf = vec![0xff, 0xff];
        assert!(try_unframe(&mut buf).is_err());
    }

    #[test]
    fn chunks_respect_mtu() {
        let frame = vec![0u8; 500];
        let chunks = chunks_for_mtu(&frame, 247);
        let chunk_size = 247 - 3;
        assert!(chunks.iter().all(|c| c.len() <= chunk_size));
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, 500);
    }
}
