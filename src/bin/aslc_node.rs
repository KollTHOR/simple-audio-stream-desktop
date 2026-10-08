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
        Some("session") => run_session_cmd(args.collect::<Vec<_>>()),
        Some("receivers") => run_receivers(),
        Some("audio") => run_audio_list(),
        Some("capture") => run_capture(args.collect::<Vec<_>>()),
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
                    "{:<4} {:04x}:{:04x}  {:<24}  {:?}  {}",
                    n,
                    d.vendor_id(),
                    d.product_id(),
                    format!(
                        "{} {}",
                        d.manufacturer_string().unwrap_or(""),
                        d.product_string().unwrap_or("")
                    )
                    .trim(),
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

// --- Optional WASAPI loopback source (Windows) ---------------------------------------------
//
// The node can capture a selected Windows render endpoint via WASAPI loopback and feed the PCM
// into the ASLC pipeline. No core changes: the source just fills the same PCM buffer the tone
// would.

#[cfg(windows)]
type LoopbackSource = aslc::audio::LoopbackSource;
#[cfg(not(windows))]
type LoopbackSource = ();

#[cfg(windows)]
fn open_loopback_source(selector: Option<&str>) -> Option<LoopbackSource> {
    selector.map(|sel| {
        aslc::audio::LoopbackSource::open(Some(sel)).unwrap_or_else(|e| {
            eprintln!("loopback unavailable: {e}");
            std::process::exit(1);
        })
    })
}

#[cfg(not(windows))]
fn open_loopback_source(selector: Option<&str>) -> Option<LoopbackSource> {
    if selector.is_some() {
        eprintln!("--device (WASAPI loopback) is Windows-only");
        std::process::exit(1);
    }
    None
}

#[cfg(windows)]
fn fill_from_loopback(
    src: &mut Option<LoopbackSource>,
    fmt: &aslc::PcmFormat,
    pcm: &mut [u8],
) -> bool {
    if let Some(s) = src.as_mut() {
        s.fill(fmt, pcm);
        true
    } else {
        false
    }
}

#[cfg(not(windows))]
fn fill_from_loopback(
    _src: &mut Option<LoopbackSource>,
    _fmt: &aslc::PcmFormat,
    _pcm: &mut [u8],
) -> bool {
    false
}

#[cfg(windows)]
fn loopback_is_some(src: &Option<LoopbackSource>) -> bool {
    src.is_some()
}

#[cfg(not(windows))]
fn loopback_is_some(_src: &Option<LoopbackSource>) -> bool {
    false
}

#[cfg(windows)]
fn set_preferred_rate_from_loopback(recv: &mut aslc::Receiver, src: &Option<LoopbackSource>) {
    if let Some(s) = src.as_ref() {
        recv.set_preferred_sample_rate(Some(s.native_rate()));
        recv.set_preferred_bit_depth(Some(s.native_bits() as u8));
    }
}

#[cfg(not(windows))]
fn set_preferred_rate_from_loopback(_recv: &mut aslc::Receiver, _src: &Option<LoopbackSource>) {}

#[cfg(windows)]
fn run_audio_list() {
    match aslc::audio::list_render_devices() {
        Ok(devices) => {
            println!("{:<4} {:<8} {:<34} name", "idx", "default", "format");
            for d in devices {
                println!(
                    "{:<4} {:<8} {:<34} {}",
                    d.index,
                    if d.is_default { "yes" } else { "" },
                    d.format_label(),
                    d.name
                );
            }
        }
        Err(e) => {
            eprintln!("could not enumerate render devices: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
fn run_audio_list() {
    eprintln!("audio enumeration is Windows-only");
    std::process::exit(1);
}

#[cfg(windows)]
fn run_capture(args: Vec<String>) {
    let selector = args.first().cloned();
    let seconds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let out = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "aslc_capture.wav".to_string());

    let mut src = match aslc::audio::LoopbackSource::open(selector.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("loopback unavailable: {e}");
            std::process::exit(1);
        }
    };
    let fmt = aslc::PcmFormat::new(src.native_rate(), 16, src.native_channels() as u8);
    let bpf = fmt.bytes_per_frame();
    let chunk_frames = (fmt.sample_rate / 100).max(1);
    let mut buf = vec![0u8; bpf * chunk_frames as usize];
    let mut pcm: Vec<u8> = Vec::new();

    println!("capturing {} for {seconds}s -> {out}", fmt.display_label());
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(seconds) {
        src.fill(&fmt, &mut buf);
        pcm.extend_from_slice(&buf);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    write_wav(&out, &fmt, &pcm);

    let peak = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
        .map(|s| s.abs())
        .max()
        .unwrap_or(0);
    println!(
        "wrote {} bytes ({:.2}s), peak {peak}/32767",
        pcm.len(),
        pcm.len() as f64 / (fmt.sample_rate * fmt.bytes_per_frame() as u32) as f64
    );
}

#[cfg(not(windows))]
fn run_capture(_args: Vec<String>) {
    eprintln!("capture (WASAPI loopback) is Windows-only");
    std::process::exit(1);
}

#[cfg(windows)]
fn write_wav(path: &str, fmt: &aslc::PcmFormat, pcm: &[u8]) {
    use std::io::Write as _;
    let mut f = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("could not create {path}: {e}");
            return;
        }
    };
    let bits = fmt.bit_depth as u16;
    let channels = fmt.channels as u16;
    let sr = fmt.sample_rate;
    let block_align = channels * bits / 8;
    let byte_rate = sr * block_align as u32;
    let data_len = pcm.len() as u32;
    let _ = f.write_all(b"RIFF");
    let _ = f.write_all(&(36 + data_len).to_le_bytes());
    let _ = f.write_all(b"WAVEfmt ");
    let _ = f.write_all(&16u32.to_le_bytes());
    let _ = f.write_all(&1u16.to_le_bytes());
    let _ = f.write_all(&channels.to_le_bytes());
    let _ = f.write_all(&sr.to_le_bytes());
    let _ = f.write_all(&byte_rate.to_le_bytes());
    let _ = f.write_all(&block_align.to_le_bytes());
    let _ = f.write_all(&bits.to_le_bytes());
    let _ = f.write_all(b"data");
    let _ = f.write_all(&data_len.to_le_bytes());
    let _ = f.write_all(pcm);
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

    // Host-initiated resync. The device advertises HELLO/CAPABILITIES only once, on adoption, then
    // waits for CONFIGURE. Without a nudge, a reconnect ("Listen" again after a stream ends)
    // deadlocks: the fresh host waits for CAPS that were already sent. Sending our HELLO makes an
    // already-adopted app re-advertise, so re-negotiation always works.
    let hello = hello_payload(PROTOCOL_VERSION, false, "ASLC Node");
    outbound
        .write_frame(MSG_HELLO, &hello, 0, hello.len(), seq)
        .unwrap();
    seq += 1;

    // Optional WASAPI loopback source (Windows). Opened before negotiation so the receiver can
    // negotiate the capture device's *native* sample rate (no resampling/downsampling).
    let device = parse_flag(&args, "--device");
    let mut loopback = open_loopback_source(device.as_deref());
    set_preferred_rate_from_loopback(&mut recv, &loopback);

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
        if loopback_is_some(&loopback) {
            "WASAPI loopback"
        } else if tone {
            "440 Hz tone"
        } else {
            "silence"
        },
        seconds
    );
    let frames_per_msg = fmt.sample_rate / 100; // 10 ms
    let mut phase = 0f64;
    let mut pcm = vec![0u8; fmt.bytes_per_frame() * frames_per_msg as usize];
    let start = std::time::Instant::now();
    // Pace sends to an absolute 10 ms schedule. Sleeping a flat 10 ms *per iteration* adds the
    // USB write time on top, so the host runs slower than realtime (~87 msg/s observed) and the
    // Android ring starves -> constant underruns + crackle. Deadline pacing holds ~100 msg/s.
    let mut next_send = start;
    while start.elapsed() < std::time::Duration::from_secs(seconds) {
        if !fill_from_loopback(&mut loopback, &fmt, &mut pcm) && tone {
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
        next_send += std::time::Duration::from_millis(10);
        let now = std::time::Instant::now();
        if next_send > now {
            std::thread::sleep(next_send - now);
        } else {
            next_send = now; // fell behind (slow USB); resume without a catch-up burst
        }
    }
    outbound.write_frame(MSG_STOP, &[], 0, 0, seq).unwrap();
    transport.close();
    println!(
        "done: sent {} PCM messages over AOA; check the Android USB Input card / Diagnostics.",
        seq.saturating_sub(1)
    );
}

/// List AOA-capable receivers (phones/DAPs) the desktop can connect to.
fn run_receivers() {
    let list = aslc::aoa::list_receiver_devices();
    if list.is_empty() {
        println!("(no AOA receivers found)");
        return;
    }
    println!(
        "{:<4} {:<12} {:<11} {:<30} serial",
        "idx", "VID:PID", "mode", "name"
    );
    for (i, d) in list.iter().enumerate() {
        println!(
            "{:<4} {:04x}:{:04x}  {:<11} {:<30} {}",
            i,
            d.vid,
            d.pid,
            if d.accessory { "accessory" } else { "mtp/other" },
            d.default_label(),
            d.serial
        );
    }
}

/// Headless driver for the shared `session` module (the same engine the GUI uses). Lets the PC be
/// the master: `--cycle` rotates the wire sample rate live so the phone follows without a restart.
fn run_session_cmd(args: Vec<String>) {
    let device = parse_flag(&args, "--device");
    let rate = parse_flag(&args, "--rate").and_then(|s| s.parse::<u32>().ok());
    let depth = parse_flag(&args, "--depth").and_then(|s| s.parse::<u8>().ok());
    let cycle = args.iter().any(|a| a == "--cycle");
    let pause_cycle = args.iter().any(|a| a == "--pause-cycle");
    let seconds: u64 = parse_flag(&args, "--for")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let phone = match (
        parse_flag(&args, "--vid").and_then(|s| parse_hex(&s)),
        parse_flag(&args, "--pid").and_then(|s| parse_hex(&s)),
    ) {
        (Some(vid), Some(pid)) => aslc::PhoneSelector::Handshake { vid, pid },
        _ => aslc::PhoneSelector::Accessory,
    };
    let cfg = aslc::SessionConfig {
        phone,
        device,
        target_rate: rate,
        target_depth: depth,
        gain: 1.0,
        tone: false,
        wait_secs: 30,
    };
    let mut handle = aslc::SessionHandle::start(cfg);
    let start = std::time::Instant::now();
    let mut next_cycle = start + std::time::Duration::from_secs(8);
    let cycle_rates: [Option<u32>; 4] = [Some(48_000), Some(96_000), Some(192_000), None];
    let mut cycle_idx = 0usize;
    let mut is_paused = false;
    let mut next_pause = start + std::time::Duration::from_secs(8);
    let mut done = false;

    while !done && start.elapsed() < std::time::Duration::from_secs(seconds) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        while let Some(ev) = handle.try_recv() {
            match ev {
                aslc::SessionEvent::State(s) => println!("[state] {s}"),
                aslc::SessionEvent::Negotiated(f) => println!("[format] {}", f.display_label()),
                aslc::SessionEvent::Capabilities(caps) => println!(
                    "[caps] rates {:?} · depths {:?} · channels {:?}",
                    caps.sample_rates, caps.bit_depths, caps.channels
                ),
                aslc::SessionEvent::DeviceAudio(ai) => println!(
                    "[audio] phone output: {} Hz · {} frames/buffer",
                    ai.output_sample_rate, ai.output_frames_per_buffer
                ),
                aslc::SessionEvent::Paused(p) => println!("[paused] {p}"),
                aslc::SessionEvent::Stats { kbps } => println!("[stats] {kbps:.0} kbit/s"),
                aslc::SessionEvent::Latency {
                    capture_ms,
                    ring_fill_ms,
                    device_ms,
                    underruns,
                    ..
                } => println!(
                    "[latency] pc {}+10 ms · phone {}+{} ms · underruns {}",
                    capture_ms, ring_fill_ms, device_ms, underruns
                ),
                aslc::SessionEvent::Stopped(s) => {
                    println!("[stopped] {s}");
                    done = true;
                }
                aslc::SessionEvent::Error(e) => {
                    println!("[error] {e}");
                    done = true;
                }
            }
        }
        if cycle && std::time::Instant::now() >= next_cycle {
            cycle_idx = (cycle_idx + 1) % cycle_rates.len();
            let r = cycle_rates[cycle_idx];
            println!("[reconfigure] rate -> {r:?}");
            handle.set_target_rate(r);
            next_cycle = std::time::Instant::now() + std::time::Duration::from_secs(8);
        }
        if pause_cycle && std::time::Instant::now() >= next_pause {
            if is_paused {
                handle.resume();
                is_paused = false;
                println!("[cmd] resume");
            } else {
                handle.pause();
                is_paused = true;
                println!("[cmd] pause");
            }
            next_pause = std::time::Instant::now() + std::time::Duration::from_secs(8);
        }
    }

    handle.request_stop();
    handle.join();
    println!("session done");
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
