//! protocol/connect.rs — Connect protocol framing.
//!
//! Both the Devin (`server.codeium.com`, HTTP/1.1) and Cursor
//! (`api2.cursor.sh`, HTTP/2) providers speak Connect's streaming envelope over
//! a raw body: a 1-byte flag, a 4-byte big-endian length, then that many
//! payload bytes. The compressed flag (`0x01`) marks a gzip payload; the
//! end-stream flag (`0x02`) marks a final frame whose payload is JSON trailers
//! (status / error / metadata) rather than a message.
//!
//! `encode_frame` writes one client frame (always uncompressed here); `ConnectDecoder`
//! accumulates server bytes and yields whole frames, decompressing gzip payloads
//! as they complete.

use std::io::Read;

/// The gzip-compressed payload flag.
pub const FLAG_COMPRESSED: u8 = 0x01;

/// The end-of-stream flag: the payload is JSON trailers, not a message.
pub const FLAG_END_STREAM: u8 = 0x02;

/// The fixed frame header length (1 flag byte + 4 length bytes).
const HEADER_LEN: usize = 5;

/// A decoded Connect frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFrame {
    /// The raw flag byte.
    pub flags: u8,
    /// The payload, gzip-decompressed when the compressed flag was set.
    pub payload: Vec<u8>,
}

impl ConnectFrame {
    /// Whether this frame carries the end-stream trailers.
    pub fn is_end_stream(&self) -> bool {
        self.flags & FLAG_END_STREAM != 0
    }
}

/// A frame decode error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// A gzip payload could not be inflated.
    Gzip(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Gzip(e) => write!(f, "Connect gzip payload could not be inflated: {e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Encode one Connect frame (flag byte, big-endian length, payload).
///
/// Client frames are sent uncompressed, so `flags` is normally `0`.
pub fn encode_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Inflate a gzip payload.
fn gunzip(payload: &[u8]) -> Result<Vec<u8>, ConnectError> {
    let mut decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| ConnectError::Gzip(e.to_string()))?;
    Ok(out)
}

/// Accumulates streamed bytes and yields whole Connect frames.
#[derive(Debug, Default)]
pub struct ConnectDecoder {
    buf: Vec<u8>,
}

impl ConnectDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append newly received bytes.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Pull the next complete frame, if one has fully arrived.
    ///
    /// Returns `Ok(None)` when more bytes are needed, `Some(Err(..))` when a
    /// completed frame's gzip payload could not be inflated.
    pub fn next_frame(&mut self) -> Result<Option<ConnectFrame>, ConnectError> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let flags = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        let total = HEADER_LEN + len;
        if self.buf.len() < total {
            return Ok(None);
        }

        let raw = self.buf[HEADER_LEN..total].to_vec();
        self.buf.drain(..total);

        let payload = if flags & FLAG_COMPRESSED != 0 {
            gunzip(&raw)?
        } else {
            raw
        };
        Ok(Some(ConnectFrame { flags, payload }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn encode_frame_writes_flag_length_and_payload() {
        let frame = encode_frame(0, b"hello");
        assert_eq!(frame[0], 0);
        assert_eq!(&frame[1..5], &5u32.to_be_bytes());
        assert_eq!(&frame[5..], b"hello");
    }

    #[test]
    fn decoder_yields_one_uncompressed_frame() {
        let mut decoder = ConnectDecoder::new();
        decoder.push(&encode_frame(0, b"payload"));
        let frame = decoder.next_frame().unwrap().expect("a frame");
        assert_eq!(frame.payload, b"payload");
        assert!(!frame.is_end_stream());
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn decoder_reassembles_a_frame_split_across_chunks() {
        let frame = encode_frame(0, b"chunky");
        let mut decoder = ConnectDecoder::new();
        decoder.push(&frame[..3]);
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(&frame[3..]);
        assert_eq!(decoder.next_frame().unwrap().unwrap().payload, b"chunky");
    }

    #[test]
    fn decoder_yields_multiple_frames_from_one_chunk() {
        let mut buf = encode_frame(0, b"one");
        buf.extend_from_slice(&encode_frame(FLAG_END_STREAM, b"{}"));
        let mut decoder = ConnectDecoder::new();
        decoder.push(&buf);
        assert_eq!(decoder.next_frame().unwrap().unwrap().payload, b"one");
        let end = decoder.next_frame().unwrap().unwrap();
        assert!(end.is_end_stream());
        assert_eq!(end.payload, b"{}");
    }

    #[test]
    fn decoder_inflates_a_compressed_frame() {
        let compressed = gzip(b"the real message");
        let mut decoder = ConnectDecoder::new();
        decoder.push(&encode_frame(FLAG_COMPRESSED, &compressed));
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(frame.payload, b"the real message");
    }
}
