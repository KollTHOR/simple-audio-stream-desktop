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
        Some("list-usb") => list_usb(),
        Some("probe") => run_probe(args.collect::<Vec<_>>()),
        Some("aoa") => run_aoa(args.collect::<Vec<_>>()),
        Some(other) => {
            eprintln!("unknown command: {other}\nusage: aslc_node [selftest|list-usb|aoa [--vid V --pid P|--accessory]|version]");
            std::process::exit(2);
        }
    }
}

/// Enumerate USB devices visible to nusb and show how to identify the target phone. On Windows a
/// device only appears here once a WinUSB driver is bound to it (Zadig / MS OS descriptors); on
/// Linux all devices are listed but bulk I/O may need a udev rule / root.
fn list_usb() {
    match nusb::list_devices() {
        Err(e) => {
            eprintln!("could not enumerate USB devices: {e}");
            std::process::exit(1);
        }
        Ok(devs) => {
            let mut n = 0usize;
            println!(
                "{:<4} {:9} {:6} {:>4}  note",
                "idx", "VID:PID", "class", "spd"
            );
            for d in devs {
                let note = if aslc::aoa::is_accessory_device(&d) {
                    format!(
                        "<= AOA accessory ({})",
                        aslc::aoa::accessory_pid_name(d.product_id()).unwrap_or("?")
                    )
                } else if d.vendor_id() == aslc::aoa::GOOGLE_VID {
                    "Google device (maybe pre-handshake phone)".to_string()
                } else {
                    String::new()
                };
                println!(
                    "{:<4} {:04x}:{:04x}  {:>4}  {:?}  {}",
                    n,
                    d.vendor_id(),
                    d.product_id(),
                    format!("{:02x}", d.class()),
                    d.speed(),
                    note
                );
                n += 1;
            }
            if n == 0 {
                println!("(no devices — on Windows, bind WinUSB to the phone first)");
            }
        }
    }
}

fn parse_flag(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

/// Drives a real ASLC negotiation + short PCM burst over the AOA pipe. Requires the phone cabled and
/// the Android receiver app (USB nightly) running. Without a device it prints guidance and exits 1.
fn parse_hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
}

/// Read-only capability probe of the connected device: enumerates claimable interfaces and asks
/// GET_PROTOCOL through one — no strings, no START, so the device does NOT leave its USB mode.
fn run_probe(args: Vec<String>) {
    let vid = parse_flag(&args, "--vid").and_then(|s| parse_hex(&s));
    let pid = parse_flag(&args, "--pid").and_then(|s| parse_hex(&s));
    let (v, p) = match (vid, pid) {
        (Some(v), Some(p)) => (v, p),
        _ => aslc::aoa::find_probe_target().unwrap_or_else(|| {
            eprintln!("no Android (18d1) device found — plug the phone in or pass --vid/--pid");
            std::process::exit(1);
        }),
    };
    println!("probing {v:04x}:{p:04x} (read-only, no mode switch)...");
    match aslc::aoa::probe_device(v, p) {
        Ok(rep) => {
            println!("  AOA capability: {}", rep.summary());
            for i in rep.interfaces.iter().filter(|i| i.claimable) {
                println!(
                    "  iface {}: claimable, bulk in={} out={}",
                    i.number,
                    i.bulk_in
                        .map(|a| format!("{a:#04x}"))
                        .unwrap_or_else(|| "-".into()),
                    i.bulk_out
                        .map(|a| format!("{a:#04x}"))
                        .unwrap_or_else(|| "-".into()),
                );
            }
            let blocked: Vec<String> = rep
                .interfaces
                .iter()
                .filter(|i| !i.claimable)
                .map(|i| format!("iface {}: {}", i.number, i.note))
                .collect();
            if !blocked.is_empty() {
                println!("  not claimable: {}", blocked.join(" | "));
            }
        }
        Err(e) => {
            eprintln!("probe failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_aoa(args: Vec<String>) {
    use aslc::transport::Transport;

    let mut transport = if args.iter().any(|a| a == "--accessory") {
        aslc::AoaTransport::from_attached_accessory()
    } else {
        let vid = parse_flag(&args, "--vid")
            .and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        let pid = parse_flag(&args, "--pid")
            .and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        match (vid, pid) {
            (Some(v), Some(p)) => aslc::AoaTransport::new(v, p),
            _ => {
                eprintln!("usage: aslc_node aoa --vid <hex> --pid <hex>   (or --accessory)");
                eprintln!("find the phone's VID:PID with `aslc_node list-usb`");
                std::process::exit(2);
            }
        }
    };

    println!("opening AOA pipe...");
    let (reader, writer) = match transport.open() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("AOA open failed: {e}");
            eprintln!("Checklist: phone in accessory mode? WinUSB bound (Windows)? Android USB nightly running?");
            std::process::exit(1);
        }
    };

    // Reuse the exact node-side negotiation proven at M0, now over the real pipe.
    // Reads run on a helper thread so a silent device cannot hang the CLI forever: the first
    // HELLO/CAPABILITIES must arrive within --wait seconds (default 12) or we print diagnostics.
    let wait_secs: u64 = parse_flag(&args, "--wait")
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    eprintln!(
        "AOA pipe open. Waiting for device HELLO/CAPABILITIES (keep the app open on the phone)..."
    );
    let (tx, rx) = std::sync::mpsc::channel::<Result<aslc::receiver::Inbound, String>>();
    std::thread::spawn(move || {
        let mut reader = InboundReader::new(reader);
        loop {
            match reader.next_inbound() {
                Ok(Some(m)) => {
                    if tx.send(Ok(m)).is_err() {
                        return; // main thread gone
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Err("device closed the pipe (EOF)".into()));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            }
        }
    });
    let mut outbound = FrameWriter::new(writer);
    let mut recv = Receiver::new();
    let mut seq: u32 = 0;
    let fmt_target;

    loop {
        let msg = match rx.recv_timeout(std::time::Duration::from_secs(wait_secs)) {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                eprintln!("inbound error: {e}");
                std::process::exit(1);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eprintln!(
                    "no frames from the device after {wait_secs}s.\n\
                     The bulk pipe is open, but nothing on the device has opened the accessory yet:\n\
                     - the Android USB-input feature must be running and hold the accessory\n\
                     - check the phone's USB Input card / app logs (accessory permission prompt?)\n\
                     exiting."
                );
                std::process::exit(1);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("reader thread ended unexpectedly");
                std::process::exit(1);
            }
        };
        if let Some(step) = recv.handle(&msg) {
            match step {
                Negotiation::Ready(_caps, fmt) => {
                    println!("got capabilities; requesting {}", fmt.display_label());
                    let payload = configure_payload(fmt);
                    outbound
                        .write_frame(MSG_CONFIGURE, &payload, 0, payload.len(), seq)
                        .unwrap();
                    seq += 1;
                }
                Negotiation::Accepted(ack) => {
                    println!("CONFIGURE_ACK: {}", ack.display_label());
                    outbound.write_frame(MSG_START, &[], 0, 0, seq).unwrap();
                    seq += 1;
                    fmt_target = ack;
                    break;
                }
                Negotiation::Rejected(err) => {
                    eprintln!(
                        "rejected: {} — {}",
                        aslc::frame::describe_error_code(err.error_code),
                        err.message
                    );
                    std::process::exit(1);
                }
            }
        }
    }

    let fmt = fmt_target;
    let tone = args.iter().any(|a| a == "--tone");
    let seconds: u64 = parse_flag(&args, "--for")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    println!(
        "streaming {} PCM for ~{}s...",
        if tone { "440 Hz tone" } else { "silence" },
        seconds
    );
    let frames_per_msg = fmt.sample_rate / 100; // 10 ms
    let mut phase = 0f64;
    let mut pcm = vec![0u8; fmt.bytes_per_frame() * frames_per_msg as usize];
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(seconds) {
        if tone {
            // Continuous-phase sine so chunks don't click; keep mono-duplicated into both channels.
            for f in 0..frames_per_msg as usize {
                let s = tone_sample(phase + f as f64, fmt.sample_rate as f64);
                for c in 0..fmt.channels as usize {
                    let off = (f * fmt.channels as usize + c) * fmt.bytes_per_sample();
                    match fmt.bytes_per_sample() {
                        2 => pcm[off..off + 2].copy_from_slice(&s.to_le_bytes()),
                        // 24-bit packed: place in high 3 bytes of an i32 pattern; 32-bit: i32 LE.
                        3 => {
                            let v = (s as i32) << 8;
                            pcm[off..off + 3].copy_from_slice(&v.to_le_bytes()[..3]);
                        }
                        4 => pcm[off..off + 4].copy_from_slice(&((s as i32) << 16).to_le_bytes()),
                        _ => {}
                    }
                }
            }
            phase += frames_per_msg as f64;
        }
        let mut payload = vec![0u8; PCM_FRAME_COUNT_SIZE + pcm.len()];
        payload[..PCM_FRAME_COUNT_SIZE].copy_from_slice(&frames_per_msg.to_be_bytes());
        payload[PCM_FRAME_COUNT_SIZE..].copy_from_slice(&pcm);
        outbound
            .write_frame(MSG_PCM_DATA, &payload, 0, payload.len(), seq)
            .unwrap();
        seq += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    outbound.write_frame(MSG_STOP, &[], 0, 0, seq).unwrap();
    transport.close();
    println!(
        "done: sent {} PCM messages over AOA; check the Android USB Input card / Diagnostics.",
        seq.saturating_sub(1)
    );
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

/// 440 Hz test tone, ~30% of full scale, continuous phase. NOTE: amplitude must be
/// scaled to the i16 range *before* the cast - `(sin * 0.3) as i16` truncates every
/// sample to 0, which is how a "tone" once shipped as pure silence.
fn tone_sample(phase: f64, sample_rate: f64) -> i16 {
    (2.0 * std::f64::consts::PI * 440.0 * phase / sample_rate)
        .sin()
        .mul_add(0.3 * 32767.0, 0.0)
        .round() as i16
}

#[cfg(test)]
mod tone_tests {
    use super::tone_sample;

    #[test]
    fn tone_has_real_amplitude_not_truncated_silence() {
        // A quarter-period in at 48k should sit near +0.3 FS (~9830), not 0.
        let quarter = tone_sample(48000.0 / 4.0 / 440.0, 48000.0);
        assert!(
            (9000..10500).contains(&quarter),
            "tone sample too quiet: {quarter}"
        );
        let three_quarter = tone_sample(3.0 * 48000.0 / 4.0 / 440.0, 48000.0);
        assert!(
            (-10500..-9000).contains(&three_quarter),
            "tone negative wrong: {three_quarter}"
        );
        assert!(tone_sample(0.0, 48000.0).abs() < 2); // zero crossing stays ~0
    }
}
