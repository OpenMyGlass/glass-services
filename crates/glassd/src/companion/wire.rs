//! Framing of the companion link, identical to `omg-transport`'s codec so glassd and the
//! companion interoperate byte-for-byte.
//!
//! Wire layout per frame: a `u32` big-endian payload length, then the postcard payload
//! ([`omg_protocol::wire`]). The cap is [`omg_protocol::MAX_FRAME_LEN`] (16 MiB), which bounds
//! memory on the Glass and rejects garbage lengths. This is a synchronous, incremental
//! decoder (feed bytes as they arrive, take whole frames out) so it drops straight into the
//! daemon's single-threaded `poll` loop, the same way [`crate::companion`] reuses
//! `glass-api`'s `FrameDecoder` for the launcher socket.

use omg_protocol::{Frame, MAX_FRAME_LEN, wire};
use std::fmt;

/// Number of bytes in the length prefix.
const LEN_PREFIX: usize = 4;

/// A framing or decoding error on the companion link. Every variant is fatal: the link is
/// closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The length prefix announces more than [`MAX_FRAME_LEN`] bytes.
    TooLong(usize),
    /// The payload is not a valid postcard [`Frame`].
    Decode(String),
    /// A frame could not be serialized (payload too large).
    Encode(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::TooLong(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME_LEN}"),
            WireError::Decode(e) => write!(f, "cannot decode frame: {e}"),
            WireError::Encode(e) => write!(f, "cannot encode frame: {e}"),
        }
    }
}

impl std::error::Error for WireError {}

/// Encodes one [`Frame`] as `len:u32be || postcard(Frame)` in a new buffer.
pub fn frame_bytes(frame: &Frame) -> Result<Vec<u8>, WireError> {
    let payload = wire::encode_payload(frame).map_err(|e| WireError::Encode(e.to_string()))?;
    let mut out = Vec::with_capacity(LEN_PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Incremental decoder: feed it bytes as they arrive, take whole frames out. Partial frames
/// stay buffered until the rest arrives.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    start: usize,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes buffered and not yet returned as a frame.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Takes the next complete frame, if one is buffered. An error is final: the stream
    /// cannot be resynchronised.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, WireError> {
        let pending = &self.buf[self.start..];
        if pending.len() < LEN_PREFIX {
            return Ok(None);
        }
        let len = u32::from_be_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(WireError::TooLong(len));
        }
        if pending.len() < LEN_PREFIX + len {
            return Ok(None);
        }
        let payload = &pending[LEN_PREFIX..LEN_PREFIX + len];
        let frame = wire::decode_payload(payload).map_err(|e| WireError::Decode(e.to_string()))?;
        self.start += LEN_PREFIX + len;
        if self.start > 4096 && self.start * 2 > self.buf.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omg_protocol::{BatteryStatus, Ping};

    #[test]
    fn frames_roundtrip() {
        let frames = [
            Frame::Ping(Ping {
                token: 9,
                unix_ms: 1,
            }),
            Frame::BatteryStatus(BatteryStatus {
                percent: 55,
                charging: true,
            }),
        ];
        let mut bytes = Vec::new();
        for f in &frames {
            bytes.extend(frame_bytes(f).unwrap());
        }
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        for f in &frames {
            assert_eq!(d.next_frame().unwrap().as_ref(), Some(f));
        }
        assert_eq!(d.next_frame().unwrap(), None);
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn partial_frames_are_buffered_byte_by_byte() {
        let f = Frame::Ping(Ping {
            token: 42,
            unix_ms: 7,
        });
        let bytes = frame_bytes(&f).unwrap();
        let mut d = FrameDecoder::new();
        for b in &bytes[..bytes.len() - 1] {
            d.push(&[*b]);
            assert_eq!(d.next_frame().unwrap(), None);
        }
        d.push(&[bytes[bytes.len() - 1]]);
        assert_eq!(d.next_frame().unwrap(), Some(f));
    }

    #[test]
    fn oversize_length_is_rejected_before_the_body() {
        let mut d = FrameDecoder::new();
        d.push(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes());
        assert_eq!(d.next_frame(), Err(WireError::TooLong(MAX_FRAME_LEN + 1)));
    }

    #[test]
    fn garbage_payload_is_a_decode_error() {
        let mut d = FrameDecoder::new();
        // length 2, then two bytes that are not a valid Frame discriminant/body.
        d.push(&[0, 0, 0, 2, 0xff, 0xff]);
        assert!(matches!(d.next_frame(), Err(WireError::Decode(_))));
    }
}
