//! Framing: a varint32 length, then one protobuf message (docs/api.md, "Transport").
//!
//! The limit applies to the message body (the bytes after the length prefix): at most
//! [`MAX_FRAME_LEN`] bytes, in both directions. A longer length, or a length varint longer
//! than 5 bytes, is a protocol error.

use prost::Message;
use std::fmt;

/// Largest message body accepted or sent: 64 KiB.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Longest valid length prefix (a varint32 takes at most 5 bytes).
pub const MAX_VARINT_LEN: usize = 5;

/// A framing or decoding error. Every variant is a protocol error: the connection is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The length prefix announces more than [`MAX_FRAME_LEN`] bytes.
    TooLong(u64),
    /// The length prefix is longer than 5 bytes, or does not fit 32 bits.
    BadVarint,
    /// The body is not a valid protobuf message of the expected type.
    Decode(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLong(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME_LEN}"),
            FrameError::BadVarint => write!(f, "malformed length varint"),
            FrameError::Decode(e) => write!(f, "cannot decode frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Appends the varint32 encoding of `n` to `out`.
pub fn put_varint32(n: u32, out: &mut Vec<u8>) {
    let mut n = n;
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Encodes `msg` as one frame (length prefix and body) appended to `out`.
/// Fails with [`FrameError::TooLong`] if the body exceeds [`MAX_FRAME_LEN`]; `out` is then unchanged.
pub fn encode_frame<M: Message>(msg: &M, out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = msg.encoded_len();
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLong(len as u64));
    }
    out.reserve(len + MAX_VARINT_LEN);
    put_varint32(len as u32, out);
    msg.encode(out).expect("Vec<u8> grows as needed");
    Ok(())
}

/// Encodes `msg` as one frame in a new buffer.
pub fn frame_bytes<M: Message>(msg: &M) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::new();
    encode_frame(msg, &mut out)?;
    Ok(out)
}

/// Parses a length prefix at the start of `buf`.
/// Returns `Ok(None)` if more bytes are needed, else `(length, prefix size)`.
fn parse_len(buf: &[u8]) -> Result<Option<(usize, usize)>, FrameError> {
    let mut value: u64 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if i >= MAX_VARINT_LEN {
            return Err(FrameError::BadVarint);
        }
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            if value > u64::from(u32::MAX) {
                return Err(FrameError::BadVarint);
            }
            if value > MAX_FRAME_LEN as u64 {
                return Err(FrameError::TooLong(value));
            }
            return Ok(Some((value as usize, i + 1)));
        }
    }
    if buf.len() >= MAX_VARINT_LEN {
        return Err(FrameError::BadVarint);
    }
    Ok(None)
}

/// Incremental decoder: feed it bytes as they arrive, take whole frames out.
/// Partial frames stay buffered until the rest arrives.
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

    /// Takes the next complete frame body, if one is buffered.
    /// An error is final: the stream cannot be resynchronised.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let pending = &self.buf[self.start..];
        let Some((len, prefix)) = parse_len(pending)? else {
            return Ok(None);
        };
        if pending.len() < prefix + len {
            return Ok(None);
        }
        let body = pending[prefix..prefix + len].to_vec();
        self.start += prefix + len;
        if self.start > 4096 && self.start * 2 > self.buf.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        Ok(Some(body))
    }

    /// Takes and decodes the next complete frame, if one is buffered.
    pub fn next_message<M: Message + Default>(&mut self) -> Result<Option<M>, FrameError> {
        match self.next_body()? {
            None => Ok(None),
            Some(body) => M::decode(body.as_slice())
                .map(Some)
                .map_err(|e| FrameError::Decode(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ClientFrame, Hello, ServiceFrame, Welcome, client_frame, service_frame};

    fn hello(name: &str) -> ClientFrame {
        ClientFrame {
            seq: 7,
            body: Some(client_frame::Body::Hello(Hello {
                api_version: 1,
                client_name: name.into(),
            })),
        }
    }

    #[test]
    fn varint_encoding() {
        for (n, expect) in [
            (0u32, vec![0u8]),
            (1, vec![1]),
            (127, vec![0x7f]),
            (128, vec![0x80, 0x01]),
            (300, vec![0xac, 0x02]),
            (65536, vec![0x80, 0x80, 0x04]),
            (u32::MAX, vec![0xff, 0xff, 0xff, 0xff, 0x0f]),
        ] {
            let mut out = Vec::new();
            put_varint32(n, &mut out);
            assert_eq!(out, expect, "{n}");
        }
    }

    #[test]
    fn round_trip_client_and_service_frames() {
        let c = hello("test");
        let s = ServiceFrame {
            body: Some(service_frame::Body::Welcome(Welcome {
                api_version: 1,
                service_version: "0.1.0".into(),
                capabilities: vec!["mock".into()],
            })),
        };
        let mut bytes = frame_bytes(&c).unwrap();
        bytes.extend(frame_bytes(&s).unwrap());
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next_message::<ClientFrame>().unwrap(), Some(c));
        assert_eq!(d.next_message::<ServiceFrame>().unwrap(), Some(s));
        assert_eq!(d.next_message::<ServiceFrame>().unwrap(), None);
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn empty_message_is_a_valid_frame() {
        let bytes = frame_bytes(&ClientFrame::default()).unwrap();
        assert_eq!(bytes, vec![0]);
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(
            d.next_message::<ClientFrame>().unwrap(),
            Some(ClientFrame::default())
        );
    }

    #[test]
    fn partial_frames_are_buffered_byte_by_byte() {
        let frames: Vec<ClientFrame> = (0..3).map(|i| hello(&"x".repeat(100 * i + 1))).collect();
        let mut bytes = Vec::new();
        for f in &frames {
            encode_frame(f, &mut bytes).unwrap();
        }
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for b in bytes {
            d.push(&[b]);
            while let Some(m) = d.next_message::<ClientFrame>().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, frames);
    }

    #[test]
    fn partial_length_prefix_waits() {
        let mut d = FrameDecoder::new();
        d.push(&[0x80, 0x80]);
        assert_eq!(d.next_body().unwrap(), None);
        d.push(&[0x01]); // length 16384, body missing
        assert_eq!(d.next_body().unwrap(), None);
        assert_eq!(d.buffered(), 3);
    }

    #[test]
    fn largest_frame_is_accepted() {
        let mut bytes = Vec::new();
        put_varint32(MAX_FRAME_LEN as u32, &mut bytes);
        bytes.extend(std::iter::repeat_n(0u8, MAX_FRAME_LEN));
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next_body().unwrap().unwrap().len(), MAX_FRAME_LEN);
    }

    #[test]
    fn oversize_length_is_rejected_before_the_body_arrives() {
        let mut d = FrameDecoder::new();
        let mut prefix = Vec::new();
        put_varint32(MAX_FRAME_LEN as u32 + 1, &mut prefix);
        d.push(&prefix);
        assert_eq!(
            d.next_body(),
            Err(FrameError::TooLong(MAX_FRAME_LEN as u64 + 1))
        );
    }

    #[test]
    fn oversize_message_is_not_encoded() {
        let big = hello(&"y".repeat(MAX_FRAME_LEN));
        let mut out = vec![1, 2, 3];
        assert!(matches!(
            encode_frame(&big, &mut out),
            Err(FrameError::TooLong(_))
        ));
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn varint_longer_than_five_bytes_is_an_error() {
        let mut d = FrameDecoder::new();
        d.push(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]);
        assert_eq!(d.next_body(), Err(FrameError::BadVarint));
        // Five continuation bytes are already enough to know.
        let mut d = FrameDecoder::new();
        d.push(&[0x80, 0x80, 0x80, 0x80, 0x80]);
        assert_eq!(d.next_body(), Err(FrameError::BadVarint));
    }

    #[test]
    fn varint_overflowing_32_bits_is_an_error() {
        let mut d = FrameDecoder::new();
        d.push(&[0xff, 0xff, 0xff, 0xff, 0x1f]);
        assert_eq!(d.next_body(), Err(FrameError::BadVarint));
    }

    #[test]
    fn garbage_body_is_a_decode_error() {
        let mut d = FrameDecoder::new();
        d.push(&[2, 0xff, 0xff]);
        assert!(matches!(
            d.next_message::<ClientFrame>(),
            Err(FrameError::Decode(_))
        ));
    }

    #[test]
    fn decoder_compacts_after_many_frames() {
        let mut d = FrameDecoder::new();
        let f = frame_bytes(&hello(&"z".repeat(1000))).unwrap();
        for _ in 0..100 {
            d.push(&f);
            d.push(&f[..10]);
            assert!(d.next_body().unwrap().is_some());
            d.push(&f[10..]);
            assert!(d.next_body().unwrap().is_some());
        }
        assert!(d.buf.len() < 10_000);
    }
}
