//! ASLC framing header + Big-Endian primitives. Byte-for-byte port of Android `AslcProtocol`.
//!
//! Frame = 12-byte header (all multi-byte fields Big-Endian) + payload. No magic/sync word:
//! the pipe is reliable+ordered, so framing is self-delimiting via `payload_length` — the reader
//! always consumes `HEADER_SIZE + payload_length` bytes and can never desync.

use std::io::{Read, Write};

pub const PROTOCOL_VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 12;
/// Hard upper bound on a single frame payload (256 KiB) — matches Android.
pub const MAX_PAYLOAD: u32 = 1 << 18;

// Message types (byte 1). Shared contract with the Android receiver.
pub const MSG_HELLO: u8 = 0x01;
pub const MSG_CAPABILITIES: u8 = 0x02;
pub const MSG_CONFIGURE: u8 = 0x03;
pub const MSG_CONFIGURE_ACK: u8 = 0x04;
pub const MSG_START: u8 = 0x05;
pub const MSG_PCM_DATA: u8 = 0x06;
pub const MSG_STOP: u8 = 0x07;
pub const MSG_ERROR: u8 = 0x08;
pub const MSG_TELEMETRY: u8 = 0x09;

// Error codes (MSG_ERROR payload).
pub const ERR_VERSION_UNSUPPORTED: u16 = 1;
pub const ERR_FORMAT_UNSUPPORTED: u16 = 2;
pub const ERR_FRAME_LENGTH_INVALID: u16 = 3;
pub const ERR_MALFORMED_MESSAGE: u16 = 4;
pub const ERR_NOT_CONFIGURED: u16 = 5;
pub const ERR_PROTOCOL_SEQUENCE: u16 = 6;
pub const ERR_BUFFER_OVERFLOW: u16 = 7;
pub const ERR_TRANSPORT_FAILURE: u16 = 8;
pub const ERR_INTERNAL: u16 = 9;

pub fn describe_message_type(t: u8) -> String {
    match t {
        MSG_HELLO => "HELLO".into(),
        MSG_CAPABILITIES => "CAPABILITIES".into(),
        MSG_CONFIGURE => "CONFIGURE".into(),
        MSG_CONFIGURE_ACK => "CONFIGURE_ACK".into(),
        MSG_START => "START".into(),
        MSG_PCM_DATA => "PCM_DATA".into(),
        MSG_STOP => "STOP".into(),
        MSG_ERROR => "ERROR".into(),
        other => format!("UNKNOWN(0x{:02x})", other),
    }
}

pub fn describe_error_code(c: u16) -> &'static str {
    match c {
        ERR_VERSION_UNSUPPORTED => "VERSION_UNSUPPORTED",
        ERR_FORMAT_UNSUPPORTED => "FORMAT_UNSUPPORTED",
        ERR_FRAME_LENGTH_INVALID => "FRAME_LENGTH_INVALID",
        ERR_MALFORMED_MESSAGE => "MALFORMED_MESSAGE",
        ERR_NOT_CONFIGURED => "NOT_CONFIGURED",
        ERR_PROTOCOL_SEQUENCE => "PROTOCOL_SEQUENCE",
        ERR_BUFFER_OVERFLOW => "BUFFER_OVERFLOW",
        ERR_TRANSPORT_FAILURE => "TRANSPORT_FAILURE",
        ERR_INTERNAL => "INTERNAL",
        _ => "UNKNOWN",
    }
}

/// A parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AslcHeader {
    pub protocol_version: u8,
    pub message_type: u8,
    pub flags: u16,
    pub sequence: u32,
    pub payload_length: u32,
}

impl AslcHeader {
    pub fn is_valid_version(&self) -> bool {
        self.protocol_version == PROTOCOL_VERSION
    }
}

/// Errors from framing/parsing. Never fatal to the process — callers decide recovery.
#[derive(Debug)]
pub enum AslcError {
    /// The peer closed the stream mid-frame (clean detach or truncated).
    UnexpectedEof,
    /// Structural header problem (unknown type or oversized length) — cannot resync a stream.
    MalformedHeader,
    /// Header is fine but the peer speaks a different protocol version.
    UnsupportedVersion(u8),
    /// A transport I/O failure.
    Io(std::io::Error),
}

impl std::fmt::Display for AslcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AslcError::UnexpectedEof => write!(f, "unexpected end of stream"),
            AslcError::MalformedHeader => write!(f, "malformed frame header"),
            AslcError::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            AslcError::Io(e) => write!(f, "transport I/O error: {e}"),
        }
    }
}
impl std::error::Error for AslcError {}
impl From<std::io::Error> for AslcError {
    fn from(e: std::io::Error) -> Self {
        AslcError::Io(e)
    }
}

// ---- Big-Endian primitives ---------------------------------------------------------------

#[inline]
pub fn write_u16_be(buf: &mut [u8], off: usize, value: u16) {
    buf[off] = (value >> 8) as u8;
    buf[off + 1] = value as u8;
}

#[inline]
pub fn read_u16_be(buf: &[u8], off: usize) -> u16 {
    ((buf[off] as u16) << 8) | (buf[off + 1] as u16)
}

#[inline]
pub fn write_u32_be(buf: &mut [u8], off: usize, value: u32) {
    buf[off] = (value >> 24) as u8;
    buf[off + 1] = (value >> 16) as u8;
    buf[off + 2] = (value >> 8) as u8;
    buf[off + 3] = value as u8;
}

#[inline]
pub fn read_u32_be(buf: &[u8], off: usize) -> u32 {
    ((buf[off] as u32) << 24)
        | ((buf[off + 1] as u32) << 16)
        | ((buf[off + 2] as u32) << 8)
        | (buf[off + 3] as u32)
}

fn write_header_into(buf: &mut [u8], h: &AslcHeader) {
    buf[0] = h.protocol_version;
    buf[1] = h.message_type;
    write_u16_be(buf, 2, h.flags);
    write_u32_be(buf, 4, h.sequence);
    write_u32_be(buf, 8, h.payload_length);
}

/// Encodes a complete frame (header + payload) into a fresh Vec. For CONTROL-plane messages only;
/// the PCM hot path uses [`FrameWriter`] with a reusable scratch.
pub fn encode_frame(message_type: u8, payload: &[u8], sequence: u32) -> Vec<u8> {
    assert!(
        payload.len() as u32 <= MAX_PAYLOAD,
        "payload exceeds MAX_PAYLOAD"
    );
    let mut frame = vec![0u8; HEADER_SIZE + payload.len()];
    write_header_into(
        &mut frame,
        &AslcHeader {
            protocol_version: PROTOCOL_VERSION,
            message_type,
            flags: 0,
            sequence,
            payload_length: payload.len() as u32,
        },
    );
    frame[HEADER_SIZE..].copy_from_slice(payload);
    frame
}

/// Parses the 12-byte header in `buf[..HEADER_SIZE]`. Rejects unknown message types and payloads
/// over `MAX_PAYLOAD` (returns `MalformedHeader`), and rejects a wrong protocol version (returns
/// `UnsupportedVersion`) so callers can distinguish the two, mirroring Android `decodeHeader` + the
/// pump's version check.
pub fn decode_header(buf: &[u8]) -> Result<AslcHeader, AslcError> {
    if buf.len() < HEADER_SIZE {
        return Err(AslcError::MalformedHeader);
    }
    let protocol_version = buf[0];
    let message_type = buf[1];
    let flags = read_u16_be(buf, 2);
    let sequence = read_u32_be(buf, 4);
    let payload_length = read_u32_be(buf, 8);

    if !(MSG_HELLO..=MSG_ERROR).contains(&message_type) {
        return Err(AslcError::MalformedHeader);
    }
    if payload_length > MAX_PAYLOAD {
        return Err(AslcError::MalformedHeader);
    }
    let header = AslcHeader {
        protocol_version,
        message_type,
        flags,
        sequence,
        payload_length,
    };
    if !header.is_valid_version() {
        return Err(AslcError::UnsupportedVersion(protocol_version));
    }
    Ok(header)
}

/// A reusable-scratch frame writer over a byte sink. Avoids a full frame allocation per write.
pub struct FrameWriter<W: Write> {
    sink: W,
    scratch: [u8; HEADER_SIZE],
}

impl<W: Write> FrameWriter<W> {
    pub fn new(sink: W) -> Self {
        Self {
            sink,
            scratch: [0u8; HEADER_SIZE],
        }
    }

    /// Writes header + `&payload[offset..offset+len]` straight to the sink (no frame-sized copy).
    pub fn write_frame(
        &mut self,
        message_type: u8,
        payload: &[u8],
        offset: usize,
        len: usize,
        sequence: u32,
    ) -> Result<(), AslcError> {
        if offset + len > payload.len() {
            return Err(AslcError::MalformedHeader);
        }
        if len as u32 > MAX_PAYLOAD {
            return Err(AslcError::MalformedHeader);
        }
        write_header_into(
            &mut self.scratch,
            &AslcHeader {
                protocol_version: PROTOCOL_VERSION,
                message_type,
                flags: 0,
                sequence,
                payload_length: len as u32,
            },
        );
        self.sink.write_all(&self.scratch)?;
        if len > 0 {
            self.sink.write_all(&payload[offset..offset + len])?;
        }
        self.sink.flush()?;
        Ok(())
    }

    pub fn get_ref(&self) -> &W {
        &self.sink
    }
    pub fn into_inner(self) -> W {
        self.sink
    }
}

/// Reads one frame from a byte source, reusing `buf` for the payload (grown as needed).
/// Returns the header and the payload bytes (copied out of `buf` on each call is avoided by
/// handing back a range into the caller's buffer via [`read_header`] + [`read_payload`]).
pub struct FrameReader<R: Read> {
    src: R,
}

impl<R: Read> FrameReader<R> {
    pub fn new(src: R) -> Self {
        Self { src }
    }

    /// Reads exactly `dest.len()` bytes. `Ok(true)` = filled; `Ok(false)` = clean EOF before fill;
    /// `Err` = transport fault. Mirrors Android `readFully` (EOF is not an error).
    pub fn read_fully(&mut self, dest: &mut [u8]) -> Result<bool, AslcError> {
        let mut done = 0usize;
        while done < dest.len() {
            match self.src.read(&mut dest[done..]) {
                Ok(0) => return Ok(false), // EOF
                Ok(n) => done += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(AslcError::Io(e)),
            }
        }
        Ok(true)
    }

    /// Reads + decodes a header. `Ok(None)` on clean EOF.
    pub fn read_header(&mut self) -> Result<Option<AslcHeader>, AslcError> {
        let mut header = [0u8; HEADER_SIZE];
        if !self.read_fully(&mut header)? {
            return Ok(None);
        }
        Ok(Some(decode_header(&header)?))
    }

    /// Reads `len` payload bytes into `buf[..len]`, growing it if needed.
    pub fn read_payload_into(&mut self, buf: &mut Vec<u8>, len: usize) -> Result<(), AslcError> {
        buf.resize(len, 0);
        if len == 0 {
            return Ok(());
        }
        // read_fully needs a &mut [u8]; the Vec slice is exactly len after resize.
        let ok = self.read_fully(&mut buf[..len])?;
        if !ok {
            return Err(AslcError::UnexpectedEof);
        }
        Ok(())
    }

    pub fn into_inner(self) -> R {
        self.src
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header() {
        let payload = [1u8, 2, 3, 4, 5];
        let frame = encode_frame(MSG_CAPABILITIES, &payload, 0xDEAD_BEEF);
        assert_eq!(frame.len(), HEADER_SIZE + payload.len());
        let h = decode_header(&frame).unwrap();
        assert_eq!(h.protocol_version, PROTOCOL_VERSION);
        assert_eq!(h.message_type, MSG_CAPABILITIES);
        assert_eq!(h.flags, 0);
        assert_eq!(h.sequence, 0xDEAD_BEEF);
        assert_eq!(h.payload_length, 5);
    }

    #[test]
    fn sequence_full_u32_range() {
        let frame = encode_frame(MSG_HELLO, &[], 0xFFFF_FFFF);
        assert_eq!(decode_header(&frame).unwrap().sequence, 0xFFFF_FFFF);
    }

    #[test]
    fn unknown_type_rejected() {
        let mut b = [0u8; HEADER_SIZE];
        b[1] = 0x7F;
        assert!(matches!(decode_header(&b), Err(AslcError::MalformedHeader)));
    }

    #[test]
    fn oversized_length_rejected() {
        let mut b = [0u8; HEADER_SIZE];
        b[1] = MSG_HELLO;
        write_u32_be(&mut b, 8, MAX_PAYLOAD + 1);
        assert!(matches!(decode_header(&b), Err(AslcError::MalformedHeader)));
    }

    #[test]
    fn version_mismatch_distinguished() {
        let mut frame = encode_frame(MSG_HELLO, &[], 0);
        frame[0] = 99;
        assert!(matches!(
            decode_header(&frame),
            Err(AslcError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn writer_then_reader_roundtrip_with_pcm_slice() {
        let mut w = FrameWriter::new(Vec::new());
        let payload = vec![9u8; 1000];
        w.write_frame(MSG_PCM_DATA, &payload, 10, 990, 42).unwrap();
        let bytes = w.into_inner();

        let mut r = FrameReader::new(&bytes[..]);
        let h = r.read_header().unwrap().unwrap();
        assert_eq!(h.message_type, MSG_PCM_DATA);
        assert_eq!(h.payload_length, 990);
        let mut body = Vec::new();
        r.read_payload_into(&mut body, h.payload_length as usize)
            .unwrap();
        assert_eq!(body, vec![9u8; 990]);
    }

    #[test]
    fn reader_reports_clean_eof() {
        let bytes: Vec<u8> = Vec::new();
        let mut r = FrameReader::new(&bytes[..]);
        assert!(r.read_header().unwrap().is_none());
    }
}
