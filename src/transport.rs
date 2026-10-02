//! Transport abstraction: a reliable, ordered, bidirectional byte pipe the ASLC framing runs over.
//! Mirrors Android `usb/UsbTransport`. TCP is implemented now (Milestone 0) so the whole protocol +
//! audio path can be proven without USB; the AOA transport (Milestone 1) implements the same trait,
//! and a future Network-PCM source too — the pipeline never depends on the byte source (spec §13).

use std::io::{Read, Write};

use crate::frame::AslcError;

/// Independently-owned read/write halves of a transport (the node runs a reader thread and a writer
/// thread concurrently, like the Android service).
pub type Halves = (Box<dyn Read + Send>, Box<dyn Write + Send>);

/// A byte pipe that can be opened into read/write halves. May block until the link is up.
pub trait Transport {
    /// Releases resources. Idempotent; safe to call after a peer vanish.
    fn open(&mut self) -> Result<Halves, AslcError>;
    fn close(&mut self);
}

/// A Transport backed by a TCP connection. Used for Milestone-0 hardware-free end-to-end tests and
/// as the reference implementation of the [`Transport`] contract that AOA will follow.
pub struct TcpTransport {
    addr: String,
    stream: Option<std::net::TcpStream>,
}

impl TcpTransport {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            stream: None,
        }
    }
}

impl Transport for TcpTransport {
    fn open(&mut self) -> Result<Halves, AslcError> {
        let stream = std::net::TcpStream::connect(&self.addr).map_err(AslcError::Io)?;
        let reader = stream.try_clone().map_err(AslcError::Io)?;
        let writer = stream;
        self.stream = Some(writer.try_clone().map_err(AslcError::Io)?);
        Ok((Box::new(reader), Box::new(writer)))
    }

    fn close(&mut self) {
        if let Some(s) = self.stream.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn tcp_transport_round_trips_bytes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4];
            sock.read_exact(&mut buf).unwrap();
            sock.write_all(&[buf[3], buf[2], buf[1], buf[0]]).unwrap();
        });

        let mut t = TcpTransport::new(addr);
        let (mut r, mut w) = t.open().unwrap();
        w.write_all(&[1, 2, 3, 4]).unwrap();
        w.flush().unwrap();
        let mut got = [0u8; 4];
        r.read_exact(&mut got).unwrap();
        assert_eq!(got, [4, 3, 2, 1]);
        t.close();
        server.join().unwrap();
    }
}
