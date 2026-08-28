//! protocol/protobuf.rs — a minimal protobuf wire codec.
//!
//! Ported from oh-my-pi's hand-rolled `protobuf.ts`: the same varint / tag /
//! length-delimited / fixed32 / fixed64 primitives, but as typed Rust readers
//! and writers rather than a descriptor-driven runtime. Message types are
//! encoded and decoded by hand against these primitives (see the `devin` and
//! `cursor` providers), so there is no code generation and no `protoc` in the
//! build.
//!
//! Only the wire shapes these two providers need are covered: varint (bool,
//! int32/64, uint32/64, enum), length-delimited (string, bytes, embedded
//! messages, packed repeated), fixed64 (double) and fixed32 (float). Groups
//! (deprecated wire types 3/4) are not supported.

use std::fmt;

/// A protobuf wire type.
pub const WIRE_VARINT: u32 = 0;
pub const WIRE_FIXED64: u32 = 1;
pub const WIRE_LEN: u32 = 2;
pub const WIRE_FIXED32: u32 = 5;

/// A decode error. Encoding never fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// The buffer ended in the middle of a value.
    UnexpectedEof,
    /// A varint ran past its maximum byte length.
    VarintOverflow,
    /// An unsupported or malformed wire type byte.
    BadWireType(u32),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::UnexpectedEof => write!(f, "unexpected end of protobuf buffer"),
            ProtoError::VarintOverflow => write!(f, "protobuf varint overflow"),
            ProtoError::BadWireType(w) => write!(f, "unsupported protobuf wire type {w}"),
        }
    }
}

impl std::error::Error for ProtoError {}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Appends protobuf wire bytes to an owned buffer.
#[derive(Debug, Default)]
pub struct ProtoWriter {
    buf: Vec<u8>,
}

impl ProtoWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Consume the writer and return the encoded bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// The bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Write a field tag (`field_number << 3 | wire_type`).
    pub fn tag(&mut self, field: u32, wire_type: u32) {
        self.varint(u64::from((field << 3) | wire_type));
    }

    /// Write a base-128 varint.
    pub fn varint(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte | 0x80);
        }
    }

    /// Write a signed 32-bit integer (sign-extended to 64 bits, as protobuf
    /// encodes negative int32 values in ten bytes).
    pub fn int32(&mut self, value: i32) {
        self.varint(value as i64 as u64);
    }

    /// Write a signed 64-bit integer.
    pub fn int64(&mut self, value: i64) {
        self.varint(value as u64);
    }

    /// Write a boolean.
    pub fn bool(&mut self, value: bool) {
        self.varint(u64::from(value));
    }

    /// Write a little-endian 64-bit fixed value.
    pub fn fixed64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a little-endian 32-bit fixed value.
    pub fn fixed32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a length-delimited byte payload (length prefix then bytes).
    pub fn len_delimited(&mut self, value: &[u8]) {
        self.varint(value.len() as u64);
        self.buf.extend_from_slice(value);
    }

    /// Write raw bytes with no length prefix.
    pub fn raw(&mut self, value: &[u8]) {
        self.buf.extend_from_slice(value);
    }

    // ---- convenience field writers ------------------------------------

    /// `field: varint`.
    pub fn field_varint(&mut self, field: u32, value: u64) {
        self.tag(field, WIRE_VARINT);
        self.varint(value);
    }

    /// `field: int32` (skips the default `0` unless `force`).
    pub fn field_int32(&mut self, field: u32, value: i32) {
        self.tag(field, WIRE_VARINT);
        self.int32(value);
    }

    /// `field: int64`.
    pub fn field_int64(&mut self, field: u32, value: i64) {
        self.tag(field, WIRE_VARINT);
        self.int64(value);
    }

    /// `field: bool`.
    pub fn field_bool(&mut self, field: u32, value: bool) {
        self.tag(field, WIRE_VARINT);
        self.bool(value);
    }

    /// `field: string`.
    pub fn field_string(&mut self, field: u32, value: &str) {
        self.tag(field, WIRE_LEN);
        self.len_delimited(value.as_bytes());
    }

    /// `field: bytes`.
    pub fn field_bytes(&mut self, field: u32, value: &[u8]) {
        self.tag(field, WIRE_LEN);
        self.len_delimited(value);
    }

    /// `field: message` (the encoded sub-message bytes).
    pub fn field_message(&mut self, field: u32, value: &[u8]) {
        self.tag(field, WIRE_LEN);
        self.len_delimited(value);
    }

    /// `field: double`.
    pub fn field_double(&mut self, field: u32, value: f64) {
        self.tag(field, WIRE_FIXED64);
        self.fixed64(value.to_bits());
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Reads protobuf wire values from a borrowed buffer.
#[derive(Debug, Clone)]
pub struct ProtoReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Whether the whole buffer has been consumed.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn byte(&mut self) -> Result<u8, ProtoError> {
        let b = *self.buf.get(self.pos).ok_or(ProtoError::UnexpectedEof)?;
        self.pos += 1;
        Ok(b)
    }

    /// Read a base-128 varint.
    pub fn varint(&mut self) -> Result<u64, ProtoError> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err(ProtoError::VarintOverflow);
            }
        }
    }

    /// Read a field tag, returning `(field_number, wire_type)`.
    pub fn tag(&mut self) -> Result<(u32, u32), ProtoError> {
        let tag = self.varint()?;
        let field = (tag >> 3) as u32;
        let wire_type = (tag & 7) as u32;
        Ok((field, wire_type))
    }

    /// Read a signed 32-bit integer.
    pub fn int32(&mut self) -> Result<i32, ProtoError> {
        Ok(self.varint()? as u32 as i32)
    }

    /// Read a signed 64-bit integer.
    pub fn int64(&mut self) -> Result<i64, ProtoError> {
        Ok(self.varint()? as i64)
    }

    /// Read a boolean.
    pub fn bool(&mut self) -> Result<bool, ProtoError> {
        Ok(self.varint()? != 0)
    }

    /// Read a little-endian 64-bit fixed value.
    pub fn fixed64(&mut self) -> Result<u64, ProtoError> {
        if self.pos + 8 > self.buf.len() {
            return Err(ProtoError::UnexpectedEof);
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&self.buf[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(u64::from_le_bytes(arr))
    }

    /// Read a little-endian 32-bit fixed value.
    pub fn fixed32(&mut self) -> Result<u32, ProtoError> {
        if self.pos + 4 > self.buf.len() {
            return Err(ProtoError::UnexpectedEof);
        }
        let mut arr = [0u8; 4];
        arr.copy_from_slice(&self.buf[self.pos..self.pos + 4]);
        self.pos += 4;
        Ok(u32::from_le_bytes(arr))
    }

    /// Read an IEEE-754 double.
    pub fn double(&mut self) -> Result<f64, ProtoError> {
        Ok(f64::from_bits(self.fixed64()?))
    }

    /// Read a length-delimited byte slice.
    pub fn bytes(&mut self) -> Result<&'a [u8], ProtoError> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len).ok_or(ProtoError::UnexpectedEof)?;
        if end > self.buf.len() {
            return Err(ProtoError::UnexpectedEof);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Read a length-delimited UTF-8 string (lossy on invalid bytes).
    pub fn string(&mut self) -> Result<String, ProtoError> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    /// Read a length-delimited sub-message into its own reader.
    pub fn message(&mut self) -> Result<ProtoReader<'a>, ProtoError> {
        Ok(ProtoReader::new(self.bytes()?))
    }

    /// Skip a field of the given wire type.
    pub fn skip(&mut self, wire_type: u32) -> Result<(), ProtoError> {
        match wire_type {
            WIRE_VARINT => {
                self.varint()?;
            }
            WIRE_FIXED64 => {
                self.fixed64()?;
            }
            WIRE_LEN => {
                self.bytes()?;
            }
            WIRE_FIXED32 => {
                self.fixed32()?;
            }
            other => return Err(ProtoError::BadWireType(other)),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips_across_boundaries() {
        for value in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut w = ProtoWriter::new();
            w.varint(value);
            let bytes = w.finish();
            let mut r = ProtoReader::new(&bytes);
            assert_eq!(r.varint().unwrap(), value);
            assert!(r.is_empty());
        }
    }

    #[test]
    fn negative_int32_encodes_in_ten_bytes_and_round_trips() {
        let mut w = ProtoWriter::new();
        w.int32(-1);
        let bytes = w.finish();
        assert_eq!(bytes.len(), 10);
        let mut r = ProtoReader::new(&bytes);
        assert_eq!(r.int32().unwrap(), -1);
    }

    #[test]
    fn tag_packs_field_and_wire_type() {
        let mut w = ProtoWriter::new();
        w.tag(5, WIRE_LEN);
        let mut r = ProtoReader::new(w.as_slice());
        assert_eq!(r.tag().unwrap(), (5, WIRE_LEN));
    }

    #[test]
    fn string_and_bytes_round_trip() {
        let mut w = ProtoWriter::new();
        w.field_string(1, "héllo");
        w.field_bytes(2, &[0xde, 0xad, 0xbe, 0xef]);
        let bytes = w.finish();

        let mut r = ProtoReader::new(&bytes);
        assert_eq!(r.tag().unwrap(), (1, WIRE_LEN));
        assert_eq!(r.string().unwrap(), "héllo");
        assert_eq!(r.tag().unwrap(), (2, WIRE_LEN));
        assert_eq!(r.bytes().unwrap(), &[0xde, 0xad, 0xbe, 0xef]);
        assert!(r.is_empty());
    }

    #[test]
    fn embedded_message_round_trips() {
        let mut inner = ProtoWriter::new();
        inner.field_int32(1, 42);
        let mut outer = ProtoWriter::new();
        outer.field_message(3, inner.as_slice());

        let bytes = outer.finish();
        let mut r = ProtoReader::new(&bytes);
        assert_eq!(r.tag().unwrap(), (3, WIRE_LEN));
        let mut sub = r.message().unwrap();
        assert_eq!(sub.tag().unwrap(), (1, WIRE_VARINT));
        assert_eq!(sub.int32().unwrap(), 42);
    }

    #[test]
    fn double_round_trips() {
        let mut w = ProtoWriter::new();
        w.field_double(1, 3.5);
        let mut r = ProtoReader::new(w.as_slice());
        assert_eq!(r.tag().unwrap(), (1, WIRE_FIXED64));
        assert_eq!(r.double().unwrap(), 3.5);
    }

    #[test]
    fn skip_advances_past_unknown_fields() {
        let mut w = ProtoWriter::new();
        w.field_string(1, "skip me");
        w.field_int32(2, 7);
        let bytes = w.finish();

        let mut r = ProtoReader::new(&bytes);
        let (field, wire) = r.tag().unwrap();
        assert_eq!(field, 1);
        r.skip(wire).unwrap();
        assert_eq!(r.tag().unwrap(), (2, WIRE_VARINT));
        assert_eq!(r.int32().unwrap(), 7);
    }

    #[test]
    fn truncated_buffer_errors() {
        let bytes = [0x08]; // tag for field 1 varint, but no value byte
        let mut r = ProtoReader::new(&bytes);
        assert_eq!(r.tag().unwrap(), (1, WIRE_VARINT));
        assert_eq!(r.varint(), Err(ProtoError::UnexpectedEof));
    }
}
