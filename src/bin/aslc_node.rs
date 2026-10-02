//! `aslc_node` desktop node — Milestone 0 entrypoint.
//!
//! No USB/audio yet. `aslc_node selftest` runs a full ASLC negotiation + PCM streaming exchange
//! against an in-process mock "Android device" over a localhost TCP pipe, exercising the exact
//! framing/payload/state-machine code paths that the AOA transport (M1) and WASAPI capture (M2) will
//! drive. This proves the protocol end-to-end on any machine, offline, with zero hardware.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::thread;

use aslc::capabilities::PcmCapabilities;
use aslc::frame::{
    encode_frame, FrameReader, FrameWriter, MSG_CAPABILITIES, MSG_CONFIGURE, MSG_CONFIGURE_ACK,
    MSG_HELLO, MSG_PCM_DATA, MSG_START, MSG_STOP, PROTOCOL_VERSION,
};
use aslc::payload::{
    capabilities_payload, configure_ack_payload, configure_payload, hello_payload, parse_configure,
    pcm_frame_count, PCM_FRAME_COUNT_SIZE,
};
use aslc::receiver::{InboundReader, Negotiation, Receiver};

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("selftest") => run_selftest(),
        Some("version") => println!("aslc_node 0.1.0 (protocol v{PROTOCOL_VERSION})"),
        Some(other) => {
            eprintln!("unknown command: {other}\nusage: aslc_node [selftest|version]");
            std::process::exit(2);
        }
    }
}

fn run_selftest() {
    println!("ASLC self-test: negotiating + streaming PCM over a localhost loopback...");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();

    // Mock Android device: speaks HELLO+CAPS, ACKs a CONFIGURE, counts PCM bytes until STOP.
    let device = thread::spawn(move || -> (u32, usize) {
        let (sock, _) = listener.accept().unwrap();
        let mut writer = sock.try_clone().unwrap(); // device->host replies
        let caps = PcmCapabilities::full_matrix();
        let hello = encode_frame(
            MSG_HELLO,
            &hello_payload(PROTOCOL_VERSION, true, "Android"),
            0,
        );
        let caps_frame = encode_frame(MSG_CAPABILITIES, &capabilities_payload(&caps), 1);
        writer.write_all(&hello).unwrap();
        writer.write_all(&caps_frame).unwrap();
        writer.flush().unwrap();

        let mut reader = FrameReader::new(sock); // host->device inbound
        let mut pcm_frames = 0u32;
        let mut pcm_bytes = 0usize;
        let mut configured = false;
        while let Some(h) = reader.read_header().unwrap() {
            let mut payload = Vec::new();
            reader
                .read_payload_into(&mut payload, h.payload_length as usize)
                .unwrap();
            match h.message_type {
                MSG_CONFIGURE => {
                    let fmt = parse_configure(&payload).unwrap();
                    // Grant the requested format as-is (a real device narrows by probe).
                    let ack = configure_ack_payload(fmt, caps.max_frame_bytes);
                    writer
                        .write_all(&encode_frame(MSG_CONFIGURE_ACK, &ack, 10))
                        .unwrap();
                    writer.flush().unwrap();
                    configured = true;
                }
                MSG_START => {}
                MSG_PCM_DATA => {
                    pcm_frames += pcm_frame_count(&payload);
                    pcm_bytes += payload.len() - PCM_FRAME_COUNT_SIZE;
                }
                MSG_STOP => break,
                _ => {}
            }
        }
        assert!(configured, "device never received CONFIGURE");
        (pcm_frames, pcm_bytes)
    });

    // ---- Node (host/controller) side over the client socket ----
    let sock = TcpStream::connect(addr).expect("connect");
    let mut writer = FrameWriter::new(sock.try_clone().unwrap());
    let mut inbound = InboundReader::new(sock);

    let mut recv = Receiver::new();
    let mut seq: u32 = 0;
    let fmt_target;

    // Drive negotiation purely from inbound frames.
    loop {
        let msg = inbound
            .next_inbound()
            .expect("inbound")
            .expect("device detached early");
        if let Some(step) = recv.handle(&msg) {
            match step {
                Negotiation::Ready(_caps, fmt) => {
                    let payload = configure_payload(fmt);
                    writer
                        .write_frame(MSG_CONFIGURE, &payload, 0, payload.len(), seq)
                        .unwrap();
                    seq += 1;
                }
                Negotiation::Accepted(ack_fmt) => {
                    writer.write_frame(MSG_START, &[], 0, 0, seq).unwrap();
                    seq += 1;
                    fmt_target = ack_fmt;
                    break;
                }
                Negotiation::Rejected(err) => panic!("negotiation rejected: {err:?}"),
            }
        }
    }
    assert!(recv.is_streaming(), "never reached STREAMING");

    // Send PCM_DATA frames (synthetic PCM bytes in the negotiated frame geometry).
    let frames_per_msg = 10u32;
    let pcm = vec![0xABu8; fmt_target.bytes_per_frame() * frames_per_msg as usize];
    let mut sent_frames = 0u32;
    let mut sent_bytes = 0usize;
    for _ in 0..4 {
        let mut payload = vec![0u8; PCM_FRAME_COUNT_SIZE + pcm.len()];
        payload[..PCM_FRAME_COUNT_SIZE].copy_from_slice(&frames_per_msg.to_be_bytes());
        payload[PCM_FRAME_COUNT_SIZE..].copy_from_slice(&pcm);
        writer
            .write_frame(MSG_PCM_DATA, &payload, 0, payload.len(), seq)
            .unwrap();
        seq += 1;
        sent_frames += frames_per_msg;
        sent_bytes += pcm.len();
    }
    writer.write_frame(MSG_STOP, &[], 0, 0, seq).unwrap();

    let (dev_frames, dev_bytes) = device.join().expect("device thread panicked");
    assert_eq!(
        dev_frames, sent_frames,
        "device saw fewer PCM frames than sent"
    );
    assert_eq!(
        dev_bytes, sent_bytes,
        "device saw fewer PCM bytes than sent"
    );

    println!(
        "PASS: negotiated {} -> streaming; sent {} PCM frames / {} bytes, device counted {} / {}.",
        fmt_target.display_label(),
        sent_frames,
        sent_bytes,
        dev_frames,
        dev_bytes
    );
}
