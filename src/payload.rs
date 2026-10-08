//! Control-plane payload codecs. Exact byte layouts of Android `AslcPayload`.
//! All integers within payloads are Big-Endian, matching the framing header.

use crate::capabilities::PcmCapabilities;
use crate::format::PcmFormat;
use crate::frame::{
    encode_frame, read_u16_be, read_u32_be, write_u16_be, write_u32_be, MSG_PCM_DATA,
};

pub const MAX_SAMPLE_RATES: usize = 16;
pub const MAX_BIT_DEPTHS: usize = 8;
pub const MAX_CHANNEL_COUNTS: usize = 8;
pub const PCM_FRAME_COUNT_SIZE: usize = 4;

// ---- HELLO -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub protocol_version: u8,
    pub is_device: bool,
    pub role_tag: String,
}

pub fn hello_payload(protocol_version: u8, is_device: bool, role_tag: &str) -> Vec<u8> {
    let tag = role_tag.as_bytes();
    let mut buf = vec![0u8; 2 + tag.len()];
    buf[0] = protocol_version;
    buf[1] = if is_device { 1 } else { 0 };
    buf[2..].copy_from_slice(tag);
    buf
}

pub fn parse_hello(payload: &[u8]) -> Option<Hello> {
    if payload.len() < 2 {
        return None;
    }
    let protocol_version = payload[0];
    let is_device = payload[1] != 0;
    let role_tag = String::from_utf8_lossy(&payload[2..]).into_owned();
    Some(Hello {
        protocol_version,
        is_device,
        role_tag,
    })
}

// ---- CAPABILITIES ------------------------------------------------------------------------

pub fn capabilities_payload(caps: &PcmCapabilities) -> Vec<u8> {
    let rates = &caps.sample_rates;
    let bits = &caps.bit_depths;
    let chans = &caps.channels;
    let encs = &caps.encodings;
    assert!(
        rates.len() <= MAX_SAMPLE_RATES
            && bits.len() <= MAX_BIT_DEPTHS
            && chans.len() <= MAX_CHANNEL_COUNTS,
        "capability list too long"
    );
    let mut buf =
        vec![0u8; 1 + rates.len() * 4 + 1 + bits.len() + 1 + chans.len() + 1 + encs.len() + 12];
    let mut p = 0usize;
    buf[p] = rates.len() as u8;
    p += 1;
    for &r in rates {
        write_u32_be(&mut buf, p, r);
        p += 4;
    }
    buf[p] = bits.len() as u8;
    p += 1;
    for &b in bits {
        buf[p] = b;
        p += 1;
    }
    buf[p] = chans.len() as u8;
    p += 1;
    for &c in chans {
        buf[p] = c;
        p += 1;
    }
    buf[p] = encs.len() as u8;
    p += 1;
    for &e in encs {
        buf[p] = e;
        p += 1;
    }
    write_u32_be(&mut buf, p, caps.max_frame_bytes);
    p += 4;
    write_u32_be(&mut buf, p, caps.recommended_buffer_bytes);
    p += 4;
    write_u32_be(&mut buf, p, caps.capability_flags);
    buf
}

pub fn parse_capabilities(payload: &[u8]) -> Option<PcmCapabilities> {
    let mut p = 0usize;
    let need = |p: usize, n: usize| p + n <= payload.len();
    if !need(p, 1) {
        return None;
    }
    let rate_count = payload[p] as usize;
    p += 1;
    if rate_count > MAX_SAMPLE_RATES || !need(p, rate_count * 4) {
        return None;
    }
    let mut sample_rates = Vec::with_capacity(rate_count);
    for _ in 0..rate_count {
        sample_rates.push(read_u32_be(payload, p));
        p += 4;
    }
    if !need(p, 1) {
        return None;
    }
    let bit_count = payload[p] as usize;
    p += 1;
    if bit_count > MAX_BIT_DEPTHS || !need(p, bit_count) {
        return None;
    }
    let bit_depths: Vec<u8> = payload[p..p + bit_count].to_vec();
    p += bit_count;
    if !need(p, 1) {
        return None;
    }
    let chan_count = payload[p] as usize;
    p += 1;
    if chan_count > MAX_CHANNEL_COUNTS || !need(p, chan_count) {
        return None;
    }
    let channels: Vec<u8> = payload[p..p + chan_count].to_vec();
    p += chan_count;
    if !need(p, 1) {
        return None;
    }
    let enc_count = payload[p] as usize;
    p += 1;
    if !need(p, enc_count + 12) {
        return None;
    }
    let encodings: Vec<u8> = payload[p..p + enc_count].to_vec();
    p += enc_count;
    let max_frame_bytes = read_u32_be(payload, p);
    p += 4;
    let recommended_buffer_bytes = read_u32_be(payload, p);
    p += 4;
    let capability_flags = read_u32_be(payload, p);
    Some(PcmCapabilities {
        sample_rates,
        bit_depths,
        channels,
        encodings,
        max_frame_bytes,
        recommended_buffer_bytes,
        capability_flags,
    })
}

// ---- CONFIGURE ---------------------------------------------------------------------------

pub fn configure_payload(format: PcmFormat) -> Vec<u8> {
    let mut buf = vec![0u8; 4 + 1 + 1 + 1 + 1 + 4];
    let mut p = 0usize;
    write_u32_be(&mut buf, p, format.sample_rate);
    p += 4;
    buf[p] = format.bit_depth;
    p += 1;
    buf[p] = format.channels;
    p += 1;
    buf[p] = format.encoding;
    p += 1;
    buf[p] = 0; // reserved
    p += 1;
    write_u32_be(&mut buf, p, format.flags);
    buf
}

pub fn parse_configure(payload: &[u8]) -> Option<PcmFormat> {
    if payload.len() < 12 {
        return None;
    }
    let mut p = 0usize;
    let sample_rate = read_u32_be(payload, p);
    p += 4;
    let bit_depth = payload[p];
    p += 1;
    let channels = payload[p];
    p += 1;
    let encoding = payload[p];
    p += 2; // encoding + reserved
    let flags = read_u32_be(payload, p);
    Some(PcmFormat {
        sample_rate,
        bit_depth,
        channels,
        encoding,
        flags,
    })
}

// ---- CONFIGURE_ACK -----------------------------------------------------------------------

pub fn configure_ack_payload(format: PcmFormat, max_frame_bytes: u32) -> Vec<u8> {
    let base = configure_payload(format);
    let mut buf = vec![0u8; base.len() + 8];
    buf[..base.len()].copy_from_slice(&base);
    write_u32_be(&mut buf, base.len(), format.bytes_per_frame() as u32);
    write_u32_be(&mut buf, base.len() + 4, max_frame_bytes);
    buf
}

/// Parses a CONFIGURE_ACK and validates the echoed `bytesPerFrame` matches the format geometry
/// (same invariant as Android — a mismatch means the peer lied/desynced and the ack is rejected).
pub fn parse_configure_ack(payload: &[u8]) -> Option<PcmFormat> {
    if payload.len() < 12 + 8 {
        return None;
    }
    let fmt = parse_configure(&payload[..12])?;
    let bytes_per_frame = read_u32_be(payload, 12);
    if bytes_per_frame as usize == fmt.bytes_per_frame() {
        Some(fmt)
    } else {
        None
    }
}

// ---- ERROR -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmError {
    pub error_code: u16,
    pub offending_message_type: u8,
    pub message: String,
}

pub fn error_payload(error_code: u16, offending_message_type: u8, message: &str) -> Vec<u8> {
    let msg = message.as_bytes();
    let mut buf = vec![0u8; 4 + msg.len()];
    write_u16_be(&mut buf, 0, error_code);
    buf[2] = offending_message_type;
    buf[3] = 0;
    buf[4..].copy_from_slice(msg);
    buf
}

pub fn parse_error(payload: &[u8]) -> Option<PcmError> {
    if payload.len() < 4 {
        return None;
    }
    let error_code = read_u16_be(payload, 0);
    let offending_message_type = payload[2];
    let message = String::from_utf8_lossy(&payload[4..]).into_owned();
    Some(PcmError {
        error_code,
        offending_message_type,
        message,
    })
}

// ---- TELEMETRY ---------------------------------------------------------------------------

/// TELEMETRY payload (device -> host): the receiver's buffered audio, sent ~2 Hz while streaming.
/// Layout: u16 ringFillMs, u16 ringCapacityMs, u16 deviceLatencyMs, u16 reserved, u32 underruns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Telemetry {
    pub ring_fill_ms: u16,
    pub ring_capacity_ms: u16,
    pub device_latency_ms: u16,
    pub underruns: u32,
}

pub fn telemetry_payload(t: &Telemetry) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    write_u16_be(&mut b, 0, t.ring_fill_ms);
    write_u16_be(&mut b, 2, t.ring_capacity_ms);
    write_u16_be(&mut b, 4, t.device_latency_ms);
    write_u16_be(&mut b, 6, 0);
    write_u32_be(&mut b, 8, t.underruns);
    b
}

pub fn parse_telemetry(payload: &[u8]) -> Option<Telemetry> {
    if payload.len() < 12 {
        return None;
    }
    Some(Telemetry {
        ring_fill_ms: read_u16_be(payload, 0),
        ring_capacity_ms: read_u16_be(payload, 2),
        device_latency_ms: read_u16_be(payload, 4),
        underruns: read_u32_be(payload, 8),
    })
}

// ---- AUDIO_INFO --------------------------------------------------------------------------

/// The device's audio-output characteristics, sent once per connection (device -> host). Lets the
/// host warn when the negotiated stream rate will be resampled by the phone's output path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioInfo {
    /// The device's declared native output sample rate (Hz); 0 when unknown.
    pub output_sample_rate: u32,
    /// The device's declared output buffer size in frames; 0 when unknown.
    pub output_frames_per_buffer: u32,
    /// Reserved bit flags (0 today).
    pub flags: u32,
}

pub fn audio_info_payload(info: &AudioInfo) -> Vec<u8> {
    let mut buf = vec![0u8; 12];
    write_u32_be(&mut buf, 0, info.output_sample_rate);
    write_u32_be(&mut buf, 4, info.output_frames_per_buffer);
    write_u32_be(&mut buf, 8, info.flags);
    buf
}

pub fn parse_audio_info(payload: &[u8]) -> Option<AudioInfo> {
    if payload.len() < 12 {
        return None;
    }
    Some(AudioInfo {
        output_sample_rate: read_u32_be(payload, 0),
        output_frames_per_buffer: read_u32_be(payload, 4),
        flags: read_u32_be(payload, 8),
    })
}

// ---- PCM_DATA ----------------------------------------------------------------------------

/// Serializes a complete PCM_DATA frame: `u32 frameCount` + `frames * bytes_per_frame` raw bytes.
pub fn pcm_data_frame(frame_count: u32, pcm: &[u8], sequence: u32) -> Vec<u8> {
    let mut payload = vec![0u8; PCM_FRAME_COUNT_SIZE + pcm.len()];
    write_u32_be(&mut payload, 0, frame_count);
    payload[PCM_FRAME_COUNT_SIZE..].copy_from_slice(pcm);
    encode_frame(MSG_PCM_DATA, &payload, sequence)
}

pub fn pcm_frame_count(payload: &[u8]) -> u32 {
    read_u32_be(payload, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::PcmCapabilities;
    use crate::format::PcmFormat;

    #[test]
    fn hello_roundtrip() {
        let p = hello_payload(crate::frame::PROTOCOL_VERSION, true, "Android");
        let h = parse_hello(&p).unwrap();
        assert_eq!(h.protocol_version, crate::frame::PROTOCOL_VERSION);
        assert!(h.is_device);
        assert_eq!(h.role_tag, "Android");
    }

    #[test]
    fn hello_too_short() {
        assert!(parse_hello(&[1]).is_none());
    }

    #[test]
    fn capabilities_roundtrip_full_matrix() {
        let caps = PcmCapabilities::full_matrix();
        let p = capabilities_payload(&caps);
        let parsed = parse_capabilities(&p).unwrap();
        assert_eq!(caps, parsed);
    }

    #[test]
    fn capabilities_truncated_rejected() {
        let p = capabilities_payload(&PcmCapabilities::full_matrix());
        assert!(parse_capabilities(&p[..p.len() - 4]).is_none());
    }

    #[test]
    fn configure_roundtrip() {
        let fmt = PcmFormat::new(96000, 24, 2);
        let p = configure_payload(fmt);
        assert_eq!(parse_configure(&p), Some(fmt));
    }

    #[test]
    fn configure_ack_validates_frame_size() {
        let fmt = PcmFormat::new(192000, 32, 2);
        let p = configure_ack_payload(fmt, 16384);
        assert_eq!(parse_configure_ack(&p), Some(fmt));
    }

    #[test]
    fn configure_ack_inconsistent_rejected() {
        let fmt = PcmFormat::new(48000, 16, 2);
        let mut p = configure_ack_payload(fmt, 16384);
        write_u32_be(&mut p, 12, fmt.bytes_per_frame() as u32 + 1);
        assert!(parse_configure_ack(&p).is_none());
    }

    #[test]
    fn error_roundtrip() {
        let p = error_payload(
            crate::frame::ERR_FORMAT_UNSUPPORTED,
            crate::frame::MSG_CONFIGURE,
            "no 32-bit",
        );
        let e = parse_error(&p).unwrap();
        assert_eq!(e.error_code, crate::frame::ERR_FORMAT_UNSUPPORTED);
        assert_eq!(e.offending_message_type, crate::frame::MSG_CONFIGURE);
        assert_eq!(e.message, "no 32-bit");
    }

    #[test]
    fn pcm_data_prefix_and_frame_count() {
        let fmt = PcmFormat::new(48000, 16, 2);
        let pcm = vec![7u8; fmt.bytes_per_frame() * 5];
        let frame = pcm_data_frame(5, &pcm, 3);
        // frame = 12 header + 4 count + pcm
        assert_eq!(frame.len(), crate::frame::HEADER_SIZE + 4 + pcm.len());
        assert_eq!(pcm_frame_count(&frame[crate::frame::HEADER_SIZE..]), 5);
    }

    #[test]
    fn audio_info_roundtrip() {
        let info = AudioInfo {
            output_sample_rate: 192_000,
            output_frames_per_buffer: 192,
            flags: 0,
        };
        let p = audio_info_payload(&info);
        assert_eq!(p.len(), 12);
        assert_eq!(parse_audio_info(&p), Some(info));
        assert!(parse_audio_info(&p[..11]).is_none());
    }
}
