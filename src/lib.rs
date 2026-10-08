//! `aslc` — Audio Stream Link Codec, desktop-node side.
//!
//! A byte-for-byte port of the Android reference protocol
//! (`app/src/main/kotlin/com/example/audiostreamer/usb/Aslc*.kt`) so a Windows/Linux
//! node can drive the Android USB (AOA) PCM receiver. Kept dependency-free at the core
//! so the wire logic is portable and testable everywhere.
//!
//! Modules:
//! - [`format`] / [`capabilities`] — PCM value models.
//! - [`frame`] — the 12-byte framing header + BE primitives (transport-agnostic).
//! - [`payload`] — control-plane payload codecs (HELLO/CAPABILITIES/CONFIGURE/...).
//! - [`transport`] — an ordered byte pipe abstraction (TCP now, AOA later).

pub mod aoa;
pub mod capabilities;
pub mod format;
pub mod frame;
#[cfg(windows)]
pub mod audio;
pub mod payload;
pub mod receiver;
pub mod session;
pub mod transport;
pub mod update;

pub use aoa::AoaTransport;
pub use capabilities::PcmCapabilities;
pub use format::{PcmFormat, ENCODING_PCM};
pub use frame::{
    AslcError, AslcHeader, FrameReader, FrameWriter, HEADER_SIZE, MAX_PAYLOAD, PROTOCOL_VERSION,
};
pub use payload::{Hello, PcmError};
pub use receiver::Receiver;
pub use session::{PhoneSelector, SessionConfig, SessionEvent, SessionHandle};
pub use transport::{Halves, TcpTransport, Transport};
