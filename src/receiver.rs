//! Desktop-node control peer: parses Android's device->host frames and drives negotiation.
//!
//! The Android receiver is the ASLC *device* (it speaks first: HELLO + CAPABILITIES on attach). This
//! module is the *host/controller* side — it consumes those inbound frames, picks a format within the
//! advertised capabilities, and validates the CONFIGURE_ACK. Outbound CONFIGURE/START/PCM_DATA/STOP
//! frames are built with the [`crate::frame`] + [`crate::payload`] helpers.
//!
//! Kept transport-agnostic: it only ever touches a [`FrameReader`]. A loopback over TCP (Milestone 0)
//! or the AOA pipe (Milestone 1) both drive this identically.

use crate::capabilities::PcmCapabilities;
use crate::format::PcmFormat;
use crate::frame::{
    decode_header, AslcError, AslcHeader, FrameReader, HEADER_SIZE, MSG_CAPABILITIES,
    MSG_CONFIGURE_ACK, MSG_ERROR, MSG_HELLO, MSG_TELEMETRY,
};
use crate::payload::{
    parse_capabilities, parse_configure_ack, parse_error, parse_hello, parse_telemetry, Hello,
    PcmError, Telemetry,
};

/// A decoded inbound (device->host) control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    Hello(Hello),
    Capabilities(PcmCapabilities),
    ConfigureAck(PcmFormat),
    Error(PcmError),
    /// Periodic buffer/latency figures from the device (while streaming).
    Telemetry(Telemetry),
}

/// Outcome of the negotiation state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Negotiation {
    /// Received capabilities; a target format has been chosen and should be CONFIGURE'd.
    Ready(PcmCapabilities, PcmFormat),
    /// CONFIGURE_ACK matched our request; send START then PCM_DATA.
    Accepted(PcmFormat),
    /// Android rejected the request.
    Rejected(PcmError),
}

/// Upper bound on bytes skipped while resyncing before giving up (guards against scanning a
/// non-ASLC stream forever).
const MAX_RESYNC_BYTES: usize = 64 * 1024;

/// Reads + decodes inbound frames from a byte source into [`Inbound`] values. `Ok(None)` = clean EOF
/// (Android detached). Non-version/recoverable framing errors surface as [`AslcError`].
pub struct InboundReader<R: std::io::Read> {
    frames: FrameReader<R>,
    payload: Vec<u8>,
    resynced_bytes: u64,
}

impl<R: std::io::Read> InboundReader<R> {
    pub fn new(src: R) -> Self {
        Self {
            frames: FrameReader::new(src),
            payload: Vec::new(),
            resynced_bytes: 0,
        }
    }

    /// Total bytes discarded while resyncing past stale/desynced data (diagnostics).
    pub fn resynced_bytes(&self) -> u64 {
        self.resynced_bytes
    }

    pub fn next_inbound(&mut self) -> Result<Option<Inbound>, AslcError> {
        let mut window = [0u8; HEADER_SIZE];
        if !self.frames.read_fully(&mut window)? {
            return Ok(None); // clean EOF
        }
        // Resync past stale bytes. The AOA pipe is reused across sessions and ASLC framing has no
        // sync word, so frames an earlier session wrote but never had read can precede ours on the
        // next open. Scan byte-by-byte for the first offset that decodes as a valid header; the
        // version/type/length checks reject essentially all random alignments.
        let mut dropped = 0usize;
        let header = loop {
            match decode_header(&window) {
                Ok(h) => break h,
                Err(AslcError::MalformedHeader) | Err(AslcError::UnsupportedVersion(_)) => {
                    if dropped >= MAX_RESYNC_BYTES {
                        return Err(AslcError::MalformedHeader);
                    }
                    window.copy_within(1..HEADER_SIZE, 0);
                    let mut one = [0u8; 1];
                    if !self.frames.read_fully(&mut one)? {
                        return Ok(None);
                    }
                    window[HEADER_SIZE - 1] = one[0];
                    dropped += 1;
                }
                Err(e) => return Err(e),
            }
        };
        if dropped > 0 {
            self.resynced_bytes += dropped as u64;
        }
        self.frames
            .read_payload_into(&mut self.payload, header.payload_length as usize)?;
        Ok(Some(decode_inbound(&header, &self.payload)?))
    }
}

/// Pure decode of one inbound frame. PCM_DATA never arrives device->host, so it is treated as
/// malformed here (the Android device only sends control frames back).
pub fn decode_inbound(header: &AslcHeader, payload: &[u8]) -> Result<Inbound, AslcError> {
    match header.message_type {
        MSG_HELLO => parse_hello(payload)
            .map(Inbound::Hello)
            .ok_or(AslcError::MalformedHeader),
        MSG_CAPABILITIES => parse_capabilities(payload)
            .map(Inbound::Capabilities)
            .ok_or(AslcError::MalformedHeader),
        MSG_CONFIGURE_ACK => parse_configure_ack(payload)
            .map(Inbound::ConfigureAck)
            .ok_or(AslcError::MalformedHeader),
        MSG_ERROR => parse_error(payload)
            .map(Inbound::Error)
            .ok_or(AslcError::MalformedHeader),
        MSG_TELEMETRY => parse_telemetry(payload)
            .map(Inbound::Telemetry)
            .ok_or(AslcError::MalformedHeader),
        _ => Err(AslcError::MalformedHeader), // unexpected inbound type for a host
    }
}

/// Negotiates a single target format from a received capability set + an inbound stream of ACK/ERROR,
/// holding the last-known capabilities and the pending request.
#[derive(Debug)]
pub struct Receiver {
    capabilities: Option<PcmCapabilities>,
    pending: Option<PcmFormat>,
    state: State,
    /// If set and advertised, negotiate this sample rate (e.g. the capture device's native rate).
    preferred_rate: Option<u32>,
    /// If set and advertised, negotiate this bit depth (e.g. the capture device's native depth).
    preferred_depth: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitingCaps,
    AwaitingAck,
    Streaming,
    Failed,
}

impl Receiver {
    pub fn new() -> Self {
        Self {
            capabilities: None,
            pending: None,
            state: State::AwaitingCaps,
            preferred_rate: None,
            preferred_depth: None,
        }
    }

    /// Prefer this sample rate when it is advertised (avoids resampling at the source).
    pub fn set_preferred_sample_rate(&mut self, rate: Option<u32>) {
        self.preferred_rate = rate;
    }

    /// Prefer this bit depth when it is advertised (avoids quantizing at the source).
    pub fn set_preferred_bit_depth(&mut self, depth: Option<u8>) {
        self.preferred_depth = depth;
    }

    /// Host-initiated live reconfiguration (the PC is master): while streaming, pick a new format
    /// from the stored capabilities honoring `rate`/`depth`, arm it as the pending CONFIGURE, and
    /// return it so the caller can send CONFIGURE. The next matching CONFIGURE_ACK is accepted as
    /// normal, so the device follows the host's format without a teardown.
    pub fn begin_reconfigure(&mut self, rate: Option<u32>, depth: Option<u8>) -> Option<PcmFormat> {
        let caps = self.capabilities.clone()?;
        let fmt = pick_format(&caps, rate, depth)?;
        self.pending = Some(fmt);
        self.state = State::AwaitingAck;
        Some(fmt)
    }

    pub fn capabilities(&self) -> Option<&PcmCapabilities> {
        self.capabilities.as_ref()
    }

    /// Feed an inbound frame. Returns the resulting negotiation step, or None if the message is not
    /// a negotiation milestone (e.g. the initial HELLO).
    pub fn handle(&mut self, inbound: &Inbound) -> Option<Negotiation> {
        match inbound {
            Inbound::Hello(_) => None,
            Inbound::Telemetry(_) => None,
            Inbound::Capabilities(caps) => {
                // Only negotiate off the first capability set. A reconnect / HELLO-resync can
                // deliver a second CAPABILITIES; re-emitting Ready would send a second CONFIGURE.
                if self.state != State::AwaitingCaps {
                    return None;
                }
                self.capabilities = Some(caps.clone());
                self.state = if caps.sample_rates.is_empty() {
                    State::Failed
                } else {
                    State::AwaitingAck
                };
                // Prefer 48000/16/2 if offered, else the first rate that has any 16-bit stereo slot.
                match pick_format(caps, self.preferred_rate, self.preferred_depth) {
                    Some(fmt) => {
                        self.pending = Some(fmt);
                        Some(Negotiation::Ready(caps.clone(), fmt))
                    }
                    None => {
                        self.state = State::Failed;
                        None
                    }
                }
            }
            Inbound::ConfigureAck(ack_fmt) => {
                if self.state != State::AwaitingAck {
                    return None;
                }
                if self.pending.as_ref() == Some(ack_fmt) {
                    self.state = State::Streaming;
                    Some(Negotiation::Accepted(*ack_fmt))
                } else {
                    // ACK disagrees with what we asked -> treat as a failure to be safe (no silent
                    // resample; spec §4).
                    self.state = State::Failed;
                    Some(Negotiation::Rejected(PcmError {
                        error_code: crate::frame::ERR_FORMAT_UNSUPPORTED,
                        offending_message_type: crate::frame::MSG_CONFIGURE_ACK,
                        message: "CONFIGURE_ACK did not match request".into(),
                    }))
                }
            }
            Inbound::Error(e) => {
                self.state = State::Failed;
                Some(Negotiation::Rejected(e.clone()))
            }
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.state == State::Streaming
    }
}

impl Default for Receiver {
    fn default() -> Self {
        Self::new()
    }
}

/// Pick a sensible initial format from advertised caps: 48k/16/stereo preferred; fall back to the
/// lowest offered rate at 16-bit stereo; mono only if stereo is not advertised.
pub fn pick_format(
    caps: &PcmCapabilities,
    preferred_rate: Option<u32>,
    preferred_depth: Option<u8>,
) -> Option<PcmFormat> {
    let want_channels = if caps.channels.contains(&2) {
        2
    } else if caps.channels.contains(&1) {
        1
    } else {
        return None;
    };
    let want_depth = if let Some(d) = preferred_depth.filter(|d| caps.bit_depths.contains(d)) {
        d
    } else if caps.bit_depths.contains(&16) {
        16
    } else {
        *caps.bit_depths.first()?
    };
    if caps.encodings.contains(&crate::format::ENCODING_PCM) {
        let rate = if let Some(p) = preferred_rate.filter(|p| caps.sample_rates.contains(p)) {
            p
        } else if caps.sample_rates.contains(&48000) {
            48000
        } else {
            *caps.sample_rates.iter().min()?
        };
        Some(PcmFormat {
            sample_rate: rate,
            bit_depth: want_depth,
            channels: want_channels,
            encoding: crate::format::ENCODING_PCM,
            flags: 0,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::PcmCapabilities;
    use crate::format::PcmFormat;
    use crate::frame::{encode_frame, MSG_CAPABILITIES, MSG_CONFIGURE_ACK, MSG_HELLO};
    use crate::payload::{capabilities_payload, configure_ack_payload, hello_payload};

    #[test]
    fn decodes_capabilities_then_negotiates_on_ack() {
        let caps = PcmCapabilities::full_matrix();
        let caps_frame = encode_frame(MSG_CAPABILITIES, &capabilities_payload(&caps), 0);
        let mut reader = InboundReader::new(&caps_frame[..]);
        let inbound = reader.next_inbound().unwrap().unwrap();
        assert!(matches!(inbound, Inbound::Capabilities(_)));

        let mut recv = Receiver::new();
        let ready = recv.handle(&inbound).unwrap();
        let (got_caps, fmt) = match ready {
            Negotiation::Ready(c, f) => (c, f),
            other => panic!("expected Ready, got {other:?}"),
        };
        assert!(got_caps.supports(fmt));
        assert_eq!(
            (fmt.sample_rate, fmt.bit_depth, fmt.channels),
            (48000, 16, 2)
        );

        // Feed a matching ACK.
        let ack_frame = encode_frame(
            MSG_CONFIGURE_ACK,
            &configure_ack_payload(fmt, caps.max_frame_bytes),
            1,
        );
        let ack_in = InboundReader::new(&ack_frame[..])
            .next_inbound()
            .unwrap()
            .unwrap();
        let step = recv.handle(&ack_in).unwrap();
        assert_eq!(step, Negotiation::Accepted(fmt));
        assert!(recv.is_streaming());
    }

    #[test]
    fn resyncs_past_stale_bytes_to_a_valid_frame() {
        let caps = PcmCapabilities::full_matrix();
        let caps_frame = encode_frame(MSG_CAPABILITIES, &capabilities_payload(&caps), 0);
        // A previous session left 5 unread bytes on the reused pipe before a valid frame.
        let mut stream = vec![0xFFu8; 5];
        stream.extend_from_slice(&caps_frame);
        let mut reader = InboundReader::new(&stream[..]);
        let inbound = reader.next_inbound().unwrap().unwrap();
        assert!(matches!(inbound, Inbound::Capabilities(_)));
        assert_eq!(reader.resynced_bytes(), 5);
    }

    #[test]
    fn duplicate_capabilities_only_negotiate_once() {
        let mut recv = Receiver::new();
        let caps = PcmCapabilities::full_matrix();
        let first = recv.handle(&Inbound::Capabilities(caps.clone()));
        assert!(matches!(first, Some(Negotiation::Ready(_, _))));
        // A HELLO-driven resync re-delivers the same set; it must not queue a second CONFIGURE.
        let second = recv.handle(&Inbound::Capabilities(caps));
        assert!(second.is_none());
    }

    #[test]
    fn error_inbound_rejects() {
        let mut recv = Receiver::new();
        recv.handle(&Inbound::Capabilities(PcmCapabilities::full_matrix()));
        let err = PcmError {
            error_code: crate::frame::ERR_FORMAT_UNSUPPORTED,
            offending_message_type: crate::frame::MSG_CONFIGURE,
            message: "no".into(),
        };
        let step = recv.handle(&Inbound::Error(err.clone())).unwrap();
        assert_eq!(step, Negotiation::Rejected(err));
    }

    #[test]
    fn mismatched_ack_is_rejected_not_substituted() {
        let caps = PcmCapabilities::full_matrix();
        let mut recv = Receiver::new();
        recv.handle(&Inbound::Capabilities(caps)); // pending 48k/16/2
                                                   // ACK for a *different* format than we requested.
        let wrong = PcmFormat::new(96000, 24, 2);
        let step = recv.handle(&Inbound::ConfigureAck(wrong)).unwrap();
        assert!(matches!(step, Negotiation::Rejected(_)));
        assert!(!recv.is_streaming());
    }

    #[test]
    fn empty_caps_fail_negotiation() {
        let empty = PcmCapabilities {
            sample_rates: vec![],
            bit_depths: vec![16],
            channels: vec![2],
            encodings: vec![crate::format::ENCODING_PCM],
            max_frame_bytes: 16384,
            recommended_buffer_bytes: 0,
            capability_flags: 0,
        };
        let mut recv = Receiver::new();
        assert!(recv.handle(&Inbound::Capabilities(empty)).is_none());
    }

    #[test]
    fn full_handshake_over_bytes() {
        // Device stream: HELLO, CAPABILITIES, (later) CONFIGURE_ACK. Reader drives a Receiver.
        let caps = PcmCapabilities::full_matrix();
        let stream: Vec<u8> = [
            encode_frame(
                MSG_HELLO,
                &hello_payload(crate::frame::PROTOCOL_VERSION, true, "Android"),
                0,
            ),
            encode_frame(MSG_CAPABILITIES, &capabilities_payload(&caps), 1),
        ]
        .concat();
        let mut reader = InboundReader::new(&stream[..]);
        let mut recv = Receiver::new();
        assert!(recv
            .handle(&reader.next_inbound().unwrap().unwrap())
            .is_none()); // HELLO: no step
        let ready = recv.handle(&reader.next_inbound().unwrap().unwrap());
        assert!(matches!(ready, Some(Negotiation::Ready(..))));
        assert!(reader.next_inbound().unwrap().is_none()); // EOF
    }
}
