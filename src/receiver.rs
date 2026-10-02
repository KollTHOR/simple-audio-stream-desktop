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
    AslcError, AslcHeader, FrameReader, MSG_CAPABILITIES, MSG_CONFIGURE_ACK, MSG_ERROR, MSG_HELLO,
};
use crate::payload::{
    parse_capabilities, parse_configure_ack, parse_error, parse_hello, Hello, PcmError,
};

/// A decoded inbound (device->host) control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    Hello(Hello),
    Capabilities(PcmCapabilities),
    ConfigureAck(PcmFormat),
    Error(PcmError),
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

/// Reads + decodes inbound frames from a byte source into [`Inbound`] values. `Ok(None)` = clean EOF
/// (Android detached). Non-version/recoverable framing errors surface as [`AslcError`].
pub struct InboundReader<R: std::io::Read> {
    frames: FrameReader<R>,
    payload: Vec<u8>,
}

impl<R: std::io::Read> InboundReader<R> {
    pub fn new(src: R) -> Self {
        Self {
            frames: FrameReader::new(src),
            payload: Vec::new(),
        }
    }

    pub fn next_inbound(&mut self) -> Result<Option<Inbound>, AslcError> {
        let header = match self.frames.read_header()? {
            Some(h) => h,
            None => return Ok(None), // EOF
        };
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
        }
    }

    pub fn capabilities(&self) -> Option<&PcmCapabilities> {
        self.capabilities.as_ref()
    }

    /// Feed an inbound frame. Returns the resulting negotiation step, or None if the message is not
    /// a negotiation milestone (e.g. the initial HELLO).
    pub fn handle(&mut self, inbound: &Inbound) -> Option<Negotiation> {
        match inbound {
            Inbound::Hello(_) => None,
            Inbound::Capabilities(caps) => {
                self.capabilities = Some(caps.clone());
                self.state = if caps.sample_rates.is_empty() {
                    State::Failed
                } else {
                    State::AwaitingAck
                };
                // Prefer 48000/16/2 if offered, else the first rate that has any 16-bit stereo slot.
                match pick_format(caps) {
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
pub fn pick_format(caps: &PcmCapabilities) -> Option<PcmFormat> {
    let want_channels = if caps.channels.contains(&2) {
        2
    } else if caps.channels.contains(&1) {
        1
    } else {
        return None;
    };
    let want_depth = if caps.bit_depths.contains(&16) {
        16
    } else {
        *caps.bit_depths.first()?
    };
    if caps.encodings.contains(&crate::format::ENCODING_PCM) {
        let rate = if caps.sample_rates.contains(&48000) {
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
