//! Reusable ASLC streaming session: opens the AOA pipe, negotiates a format with the phone,
//! captures WASAPI loopback (or a test tone), and paces `PCM_DATA` with a software gain.
//!
//! The PC is the master:
//! - **Live reconfiguration**: changing the wire sample rate / bit depth sends a CONFIGURE and the
//!   phone follows without a restart.
//! - **Live source switching**: changing the capture endpoint (or "follow the system default")
//!   reopens the loopback capture; a format change follows via reconfiguration.
//! - **Pause/resume**: `pause()` sends STOP and keeps the USB pipe (and the phone's listening)
//!   alive; `resume()` re-negotiates on the same pipe, so Start never has to re-claim the
//!   accessory interface.
//!
//! The wire logic is unchanged — it reuses [`crate::receiver::Receiver`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver as EventReceiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::aoa::AoaTransport;
use crate::capabilities::PcmCapabilities;
use crate::frame::{
    FrameWriter, MSG_CONFIGURE, MSG_HELLO, MSG_PCM_DATA, MSG_START, MSG_STOP, PROTOCOL_VERSION,
};
use crate::payload::{configure_payload, hello_payload, AudioInfo, PCM_FRAME_COUNT_SIZE};
use crate::receiver::{Inbound, InboundReader, Negotiation, Receiver};
use crate::transport::{Halves, Transport};
use crate::PcmFormat;

#[cfg(windows)]
use crate::audio::LoopbackSource;
#[cfg(not(windows))]
type LoopbackSource = ();

/// How to locate/attach the phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhoneSelector {
    /// Attach to a phone that already enumerates as an AOA accessory.
    Accessory,
    /// Run the AOA handshake on this device (its pre-handshake `vid:pid`), then attach.
    Handshake { vid: u16, pid: u16 },
}

/// One session's configuration. `device`: `Some(selector)` = a specific render endpoint (index or
/// name substring); `None` = follow the Windows default render endpoint.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub phone: PhoneSelector,
    pub device: Option<String>,
    /// `None` follows the capture device's native rate; `Some` forces this wire rate.
    pub target_rate: Option<u32>,
    /// `None` follows the capture device's native depth; `Some` forces this wire depth.
    pub target_depth: Option<u8>,
    pub gain: f32,
    pub tone: bool,
    pub wait_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            phone: PhoneSelector::Accessory,
            device: None,
            target_rate: None,
            target_depth: None,
            gain: 1.0,
            tone: false,
            wait_secs: 30,
        }
    }
}

/// Progress events emitted by a running session (polled by the GUI).
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// Human-readable phase/status line.
    State(String),
    /// Negotiation completed for this wire format.
    Negotiated(PcmFormat),
    /// The device's advertised PCM capability set (from its `CAPABILITIES` message). The GUI uses
    /// this to restrict the rate/bit-depth choices to what the phone actually offers.
    Capabilities(PcmCapabilities),
    /// The device's audio-output characteristics (native output rate/buffer), from `AUDIO_INFO`.
    DeviceAudio(AudioInfo),
    /// The device's self-reported display name (from its `HELLO`), if non-empty.
    DeviceName(String),
    /// Streaming paused (true) or resumed (false).
    Paused(bool),
    /// Rolling throughput (kbit/s).
    Stats { kbps: f64 },
    /// Latency figures (ms) from the PC pipeline and the phone.
    Latency {
        capture_ms: u32,
        ring_fill_ms: u16,
        ring_capacity_ms: u16,
        device_ms: u16,
        underruns: u32,
    },
    /// Terminal: the session ended normally.
    Stopped(String),
    /// Terminal: the session failed.
    Error(String),
}

/// Handle to a running session on a background thread.
pub struct SessionHandle {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    gain: Arc<AtomicU32>,
    target_rate: Arc<AtomicU32>,
    target_depth: Arc<AtomicU32>,
    source: Arc<Mutex<Option<String>>>,
    events: EventReceiver<SessionEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SessionHandle {
    pub fn start(cfg: SessionConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let gain = Arc::new(AtomicU32::new(cfg.gain.to_bits()));
        let target_rate = Arc::new(AtomicU32::new(cfg.target_rate.unwrap_or(0)));
        let target_depth = Arc::new(AtomicU32::new(cfg.target_depth.unwrap_or(0) as u32));
        let source = Arc::new(Mutex::new(cfg.device.clone()));
        let (tx, rx) = channel();
        let stop_t = stop.clone();
        let paused_t = paused.clone();
        let gain_t = gain.clone();
        let tr = target_rate.clone();
        let td = target_depth.clone();
        let src = source.clone();
        let thread = std::thread::Builder::new()
            .name("aslc-session".into())
            .spawn(move || run_session(cfg, stop_t, paused_t, gain_t, tr, td, src, tx))
            .ok();
        Self {
            stop,
            paused,
            gain,
            target_rate,
            target_depth,
            source,
            events: rx,
            thread,
        }
    }

    /// Full teardown (used on quit). Closes the USB pipe.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Pause streaming; the phone stops playing but the USB pipe and its listening stay up.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Resume streaming on the same pipe (re-negotiates, so it also recovers a phone-side restart).
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    /// Update the software gain live (clamped 0..=4).
    pub fn set_gain(&self, gain: f32) {
        self.gain
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }

    /// Live: force the wire sample rate (`None` = follow the source).
    pub fn set_target_rate(&self, rate: Option<u32>) {
        self.target_rate.store(rate.unwrap_or(0), Ordering::Relaxed);
    }

    /// Live: force the wire bit depth (`None` = follow the source).
    pub fn set_target_depth(&self, depth: Option<u8>) {
        self.target_depth
            .store(depth.unwrap_or(0) as u32, Ordering::Relaxed);
    }

    /// Live: switch the capture source (`None` = follow the Windows default endpoint).
    pub fn set_source(&self, source: Option<String>) {
        if let Ok(mut g) = self.source.lock() {
            *g = source;
        }
    }

    /// Non-blocking: next event from the worker, if any.
    pub fn try_recv(&self) -> Option<SessionEvent> {
        self.events.try_recv().ok()
    }

    /// Join the worker. Safe once a terminal event has been received.
    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open_pipe(sel: &PhoneSelector) -> Result<(AoaTransport, Halves), String> {
    match sel {
        PhoneSelector::Accessory => {
            let mut t = AoaTransport::from_attached_accessory();
            let h = t
                .open()
                .map_err(|e| format!("no accessory attached: {e}"))?;
            Ok((t, h))
        }
        PhoneSelector::Handshake { vid, pid } => {
            let mut t = AoaTransport::new(*vid, *pid);
            let h = t.open().map_err(|e| format!("AOA handshake failed: {e}"))?;
            Ok((t, h))
        }
    }
}

/// The result of a read-only capability probe: what the phone offers plus its audio-output
/// characteristics. No CONFIGURE/START, so the phone stays idle.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub capabilities: PcmCapabilities,
    pub audio_info: Option<AudioInfo>,
    /// The device's self-reported display name (from its `HELLO`), if non-empty.
    pub device_name: Option<String>,
}

/// Read-only probe: open the AOA pipe, greet the phone, and collect its `CAPABILITIES` (and a
/// trailing `AUDIO_INFO`) without configuring or starting a stream. Lets the GUI constrain its
/// rate/depth menus before the first Start.
pub fn probe_capabilities(cfg: &SessionConfig) -> Result<ProbeResult, String> {
    let (mut transport, (reader, writer)) = open_pipe(&cfg.phone)?;

    let (itx, irx) = channel::<Result<Inbound, String>>();
    std::thread::Builder::new()
        .name("aslc-probe".into())
        .spawn(move || {
            let mut r = InboundReader::new(reader);
            loop {
                match r.next_inbound() {
                    Ok(Some(m)) => {
                        if itx.send(Ok(m)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => return,
                }
            }
        })
        .ok();

    let mut outbound = FrameWriter::new(writer);
    let hello = hello_payload(PROTOCOL_VERSION, false, "ASLC Node");
    if let Err(e) = outbound.write_frame(MSG_HELLO, &hello, 0, hello.len(), 0) {
        transport.close();
        return Err(format!("USB write failed (HELLO): {e}"));
    }

    let deadline = Instant::now() + Duration::from_secs(cfg.wait_secs.max(1));
    let mut caps: Option<PcmCapabilities> = None;
    let mut audio_info: Option<AudioInfo> = None;
    let mut device_name: Option<String> = None;
    let mut caps_at: Option<Instant> = None;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        // Once the caps are in, wait briefly for a trailing AUDIO_INFO, then finish.
        if caps_at.is_some_and(|t| t.elapsed() > Duration::from_millis(400)) {
            break;
        }
        match irx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(Inbound::Capabilities(c))) => {
                caps = Some(c);
                caps_at = Some(Instant::now());
            }
            Ok(Ok(Inbound::AudioInfo(ai))) => audio_info = Some(ai),
            Ok(Ok(Inbound::Hello(h))) => {
                let n = h.role_tag.trim();
                if !n.is_empty() {
                    device_name = Some(n.to_string());
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    transport.close();
    match caps {
        Some(capabilities) => Ok(ProbeResult {
            capabilities,
            audio_info,
            device_name,
        }),
        None => Err("no capabilities from the phone — open the app and enable USB Receive".into()),
    }
}

#[cfg(windows)]
fn open_source(sel: &Option<String>) -> Result<Option<LoopbackSource>, String> {
    LoopbackSource::open(sel.as_deref()).map(Some)
}

#[cfg(not(windows))]
fn open_source(_sel: &Option<String>) -> Result<Option<LoopbackSource>, String> {
    Ok(None)
}

#[cfg(windows)]
fn lb_native(lb: Option<&LoopbackSource>) -> (Option<u32>, Option<u8>) {
    lb.map(|l| (Some(l.native_rate()), Some(l.native_bits() as u8)))
        .unwrap_or((None, None))
}

#[cfg(not(windows))]
fn lb_native(_lb: Option<&LoopbackSource>) -> (Option<u32>, Option<u8>) {
    (None, None)
}

/// Resolve the desired (rate, depth) from the live targets + the capture source's native values.
fn resolve_want(
    target_rate: &AtomicU32,
    target_depth: &AtomicU32,
    loopback: Option<&LoopbackSource>,
) -> (Option<u32>, Option<u8>) {
    let (nrate, ndepth) = lb_native(loopback);
    let rate = match target_rate.load(Ordering::Relaxed) {
        0 => nrate,
        v => Some(v),
    };
    let depth = match target_depth.load(Ordering::Relaxed) {
        0 => ndepth,
        v => Some(v as u8),
    };
    (rate, depth)
}

/// Reopen the capture source if the requested selector (or, for "follow default", the Windows
/// default endpoint) changed. Returns an optional status message for the log.
#[cfg(windows)]
fn switch_source_if_needed(
    source: &Mutex<Option<String>>,
    applied: &mut Option<String>,
    loopback: &mut Option<LoopbackSource>,
    follow_check: &mut Instant,
) -> Option<String> {
    let want = source.lock().map(|g| g.clone()).unwrap_or(None);
    let now = Instant::now();
    let explicit_changed = want != *applied;
    let follow_changed = if want.is_none() && now >= *follow_check {
        *follow_check = now + Duration::from_secs(1);
        let cur = crate::audio::default_render_device_id();
        match (cur, loopback.as_ref().map(|l| l.device_id().to_string())) {
            (Some(c), Some(o)) => c != o,
            (Some(_), None) => true,
            _ => false,
        }
    } else {
        false
    };
    if !explicit_changed && !follow_changed {
        return None;
    }
    match open_source(&want) {
        Ok(Some(lb)) => {
            let msg = format!("Source: {}", lb.device_name());
            *loopback = Some(lb);
            *applied = want;
            Some(msg)
        }
        Ok(None) => {
            *loopback = None;
            *applied = want;
            None
        }
        Err(e) => Some(format!("Source unavailable: {e}")),
    }
}

#[cfg(not(windows))]
fn switch_source_if_needed(
    _source: &Mutex<Option<String>>,
    _applied: &mut Option<String>,
    _loopback: &mut Option<LoopbackSource>,
    _follow_check: &mut Instant,
) -> Option<String> {
    None
}

/// Send HELLO and drive negotiation to a START. Recreates the receiver state so it also works as a
/// re-negotiation after a pause (or a phone-side restart).
#[allow(clippy::too_many_arguments)]
fn negotiate(
    outbound: &mut FrameWriter<Box<dyn std::io::Write + Send>>,
    recv: &mut Receiver,
    irx: &EventReceiver<Result<Inbound, String>>,
    seq: &mut u32,
    stop: &AtomicBool,
    wait_secs: u64,
    want_rate: Option<u32>,
    want_depth: Option<u8>,
    emit: &dyn Fn(SessionEvent),
) -> Result<PcmFormat, String> {
    *recv = Receiver::new();
    recv.set_preferred_sample_rate(want_rate);
    recv.set_preferred_bit_depth(want_depth);

    let hello = hello_payload(PROTOCOL_VERSION, false, "ASLC Node");
    outbound
        .write_frame(MSG_HELLO, &hello, 0, hello.len(), *seq)
        .map_err(|e| format!("USB write failed (HELLO): {e}"))?;
    *seq += 1;

    let deadline = Instant::now() + Duration::from_secs(wait_secs.max(1));
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        if Instant::now() >= deadline {
            return Err("no frames from the phone — is it in Receive mode and listening?".into());
        }
        match irx.recv_timeout(Duration::from_millis(150)) {
            Ok(Ok(m)) => {
                if let Inbound::Hello(h) = &m {
                    let n = h.role_tag.trim();
                    if !n.is_empty() {
                        emit(SessionEvent::DeviceName(n.to_string()));
                    }
                }
                if let Inbound::AudioInfo(ai) = &m {
                    emit(SessionEvent::DeviceAudio(*ai));
                }
                if let Some(step) = recv.handle(&m) {
                    match step {
                        Negotiation::Ready(caps, fmt) => {
                            emit(SessionEvent::Capabilities(caps));
                            let payload = configure_payload(fmt);
                            outbound
                                .write_frame(MSG_CONFIGURE, &payload, 0, payload.len(), *seq)
                                .map_err(|e| format!("USB write failed (CONFIGURE): {e}"))?;
                            *seq += 1;
                        }
                        Negotiation::Accepted(ack) => {
                            outbound
                                .write_frame(MSG_START, &[], 0, 0, *seq)
                                .map_err(|e| format!("USB write failed (START): {e}"))?;
                            *seq += 1;
                            return Ok(ack);
                        }
                        Negotiation::Rejected(err) => {
                            return Err(format!("phone rejected the format: {}", err.message));
                        }
                    }
                }
            }
            Ok(Err(e)) => return Err(format!("inbound: {e}")),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("reader thread ended".into()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    cfg: SessionConfig,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    gain: Arc<AtomicU32>,
    target_rate: Arc<AtomicU32>,
    target_depth: Arc<AtomicU32>,
    source: Arc<Mutex<Option<String>>>,
    tx: Sender<SessionEvent>,
) {
    let emit = |e: SessionEvent| {
        let _ = tx.send(e);
    };

    emit(SessionEvent::State("Opening USB pipe…".into()));
    let (mut transport, halves) = match open_pipe(&cfg.phone) {
        Ok(v) => v,
        Err(e) => {
            emit(SessionEvent::Error(e));
            return;
        }
    };
    let (reader, writer) = halves;

    // Inbound reader thread (device -> host control frames) for the whole session.
    let (itx, irx) = channel::<Result<Inbound, String>>();
    let reader_handle = std::thread::Builder::new()
        .name("aslc-inbound".into())
        .spawn(move || {
            let mut r = InboundReader::new(reader);
            loop {
                match r.next_inbound() {
                    Ok(Some(m)) => {
                        if itx.send(Ok(m)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = itx.send(Err("device closed the pipe".into()));
                        return;
                    }
                    Err(e) => {
                        let _ = itx.send(Err(e.to_string()));
                        return;
                    }
                }
            }
        })
        .ok();

    let mut outbound = FrameWriter::new(writer);
    let mut seq: u32 = 0;

    let mut applied_source = source.lock().map(|g| g.clone()).unwrap_or(None);
    let mut loopback = match open_source(&applied_source) {
        Ok(lb) => lb,
        Err(e) => {
            emit(SessionEvent::Error(format!("audio source: {e}")));
            transport.close();
            return;
        }
    };
    let mut follow_check = Instant::now();

    let mut recv = Receiver::new();
    let mut streaming = false;
    let mut fmt = PcmFormat::new(48_000, 16, 2);
    let mut frames_per_msg = (fmt.sample_rate / 100).max(1);
    let mut pcm = vec![0u8; fmt.bytes_per_frame() * frames_per_msg as usize];
    let mut phase = 0f64;
    let mut applied_req = resolve_want(&target_rate, &target_depth, loopback.as_ref());
    let mut next_send = Instant::now();
    let mut last_stat = Instant::now();
    let mut last_messages: u64 = 0;
    let mut messages: u64 = 0;
    // Set when the phone stopped its USB input / the pipe closed, so the terminal event reads as a
    // clean stop rather than a generic "stopped after N messages".
    let mut stopped_by_device = false;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        // --- Paused: phone stops playing, pipe stays up --------------------------------
        if paused.load(Ordering::SeqCst) {
            if streaming {
                let _ = outbound.write_frame(MSG_STOP, &[], 0, 0, seq);
                seq += 1;
                streaming = false;
                emit(SessionEvent::Paused(true));
                emit(SessionEvent::State("Paused".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        // --- (Re)negotiate when idle: initial start and every resume -------------------
        if !streaming {
            if let Some(msg) = switch_source_if_needed(
                &source,
                &mut applied_source,
                &mut loopback,
                &mut follow_check,
            ) {
                emit(SessionEvent::State(msg));
            }
            let (wr, wd) = resolve_want(&target_rate, &target_depth, loopback.as_ref());
            match negotiate(
                &mut outbound,
                &mut recv,
                &irx,
                &mut seq,
                &stop,
                cfg.wait_secs,
                wr,
                wd,
                &emit,
            ) {
                Ok(f) => {
                    fmt = f;
                    frames_per_msg = (fmt.sample_rate / 100).max(1);
                    pcm = vec![0u8; fmt.bytes_per_frame() * frames_per_msg as usize];
                    applied_req = (wr, wd);
                    streaming = true;
                    messages = 0;
                    last_messages = 0;
                    last_stat = Instant::now();
                    next_send = Instant::now();
                    emit(SessionEvent::Paused(false));
                    emit(SessionEvent::Negotiated(fmt));
                    emit(SessionEvent::State(format!(
                        "Streaming {}",
                        fmt.display_label()
                    )));
                }
                Err(e) => {
                    emit(SessionEvent::Error(e));
                    break;
                }
            }
            continue;
        }

        // --- Streaming -----------------------------------------------------------------
        // Detect a phone-side restart: when the receiver is stopped and started again it re-sends
        // HELLO/CAPABILITIES on the same pipe (the accessory connection persists). If we keep
        // streaming blindly, both sides end up stuck. Renegotiate instead.
        let capture_ms = {
            #[cfg(windows)]
            {
                loopback.as_ref().map(|l| l.period_ms()).unwrap_or(0)
            }
            #[cfg(not(windows))]
            {
                0u32
            }
        };
        let mut restarted = false;
        let mut reader_dead = false;
        while let Ok(ev) = irx.try_recv() {
            match ev {
                Ok(Inbound::Telemetry(t)) => emit(SessionEvent::Latency {
                    capture_ms,
                    ring_fill_ms: t.ring_fill_ms,
                    ring_capacity_ms: t.ring_capacity_ms,
                    device_ms: t.device_latency_ms,
                    underruns: t.underruns,
                }),
                Ok(Inbound::Hello(h)) => {
                    let n = h.role_tag.trim();
                    if !n.is_empty() {
                        emit(SessionEvent::DeviceName(n.to_string()));
                    }
                    restarted = true
                }
                Ok(Inbound::Capabilities(_)) | Ok(Inbound::Error(_)) => restarted = true,
                Ok(Inbound::AudioInfo(ai)) => emit(SessionEvent::DeviceAudio(ai)),
                Ok(Inbound::ConfigureAck(_)) => {}
                Err(_) => reader_dead = true,
            }
        }
        if reader_dead {
            // The phone stopped its USB input (or the pipe closed): a clean stop, not an error.
            stopped_by_device = true;
            break;
        }
        if restarted {
            streaming = false;
            emit(SessionEvent::State(
                "Phone reconnected — renegotiating…".into(),
            ));
            continue;
        }

        if let Some(msg) = switch_source_if_needed(
            &source,
            &mut applied_source,
            &mut loopback,
            &mut follow_check,
        ) {
            emit(SessionEvent::State(msg));
        }

        let req = resolve_want(&target_rate, &target_depth, loopback.as_ref());
        if req != applied_req {
            applied_req = req;
            if let Some(new_fmt) = recv.begin_reconfigure(req.0, req.1) {
                emit(SessionEvent::State(format!(
                    "Reconfiguring to {}…",
                    new_fmt.display_label()
                )));
                let payload = configure_payload(new_fmt);
                if outbound
                    .write_frame(MSG_CONFIGURE, &payload, 0, payload.len(), seq)
                    .is_err()
                {
                    emit(SessionEvent::Error(
                        "USB write failed (re-CONFIGURE)".into(),
                    ));
                    break;
                }
                seq += 1;
                let ack_deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if Instant::now() >= ack_deadline {
                        emit(SessionEvent::Error(
                            "no CONFIGURE_ACK for live reconfiguration".into(),
                        ));
                        break;
                    }
                    match irx.recv_timeout(Duration::from_millis(100)) {
                        Ok(Ok(m)) => {
                            if let Some(step) = recv.handle(&m) {
                                match step {
                                    Negotiation::Accepted(ack) => {
                                        fmt = ack;
                                        frames_per_msg = (fmt.sample_rate / 100).max(1);
                                        pcm = vec![
                                            0u8;
                                            fmt.bytes_per_frame() * frames_per_msg as usize
                                        ];
                                        emit(SessionEvent::Negotiated(fmt));
                                        emit(SessionEvent::State(format!(
                                            "Streaming {}",
                                            fmt.display_label()
                                        )));
                                        break;
                                    }
                                    Negotiation::Rejected(err) => {
                                        emit(SessionEvent::Error(format!(
                                            "phone rejected reconfigure: {}",
                                            err.message
                                        )));
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            emit(SessionEvent::Error(format!("inbound: {e}")));
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        }

        let g = f32::from_bits(gain.load(Ordering::Relaxed));
        #[cfg(windows)]
        let filled = if let Some(lb) = loopback.as_mut() {
            lb.set_gain(g);
            lb.fill(&fmt, &mut pcm);
            true
        } else {
            false
        };
        #[cfg(not(windows))]
        let filled = false;

        if !filled {
            if cfg.tone {
                fill_tone(&mut pcm, &fmt, frames_per_msg, &mut phase, g);
            } else {
                pcm.fill(0);
            }
        }

        let mut payload = vec![0u8; PCM_FRAME_COUNT_SIZE + pcm.len()];
        payload[..PCM_FRAME_COUNT_SIZE].copy_from_slice(&frames_per_msg.to_be_bytes());
        payload[PCM_FRAME_COUNT_SIZE..].copy_from_slice(&pcm);
        if outbound
            .write_frame(MSG_PCM_DATA, &payload, 0, payload.len(), seq)
            .is_err()
        {
            emit(SessionEvent::Error(
                "USB write failed — did the phone stop listening?".into(),
            ));
            break;
        }
        seq += 1;
        messages += 1;

        if last_stat.elapsed() >= Duration::from_millis(500) {
            let secs = last_stat.elapsed().as_secs_f64().max(1e-6);
            let kbps = (messages - last_messages) as f64 * pcm.len() as f64 * 8.0 / secs / 1000.0;
            emit(SessionEvent::Stats { kbps });
            last_stat = Instant::now();
            last_messages = messages;
        }

        next_send += Duration::from_millis(10);
        let now = Instant::now();
        if next_send > now {
            std::thread::sleep(next_send - now);
        } else {
            next_send = now;
        }
    }

    if streaming {
        let _ = outbound.write_frame(MSG_STOP, &[], 0, 0, seq);
    }
    // Unblock + join the reader so it releases the accessory interface before this session ends;
    // otherwise a later session cannot re-claim interface 0 (Access denied).
    transport.cancel_reader();
    if let Some(h) = reader_handle {
        let _ = h.join();
    }
    transport.close();
    emit(SessionEvent::Paused(false));
    emit(SessionEvent::Stopped(if stopped_by_device {
        "phone stopped the USB input".into()
    } else {
        format!("stopped after {messages} messages")
    }));
}

/// Fill `pcm` with a continuous-phase 440 Hz test tone (mono duplicated), scaled by `gain`.
fn fill_tone(pcm: &mut [u8], fmt: &PcmFormat, frames: u32, phase: &mut f64, gain: f32) {
    let ch = fmt.channels as usize;
    for f in 0..frames as usize {
        let s = (tone_sample(*phase + f as f64, fmt.sample_rate as f64) as f32 * gain)
            .clamp(-32768.0, 32767.0) as i32;
        for c in 0..ch {
            let off = (f * ch + c) * fmt.bytes_per_sample();
            match fmt.bytes_per_sample() {
                2 => pcm[off..off + 2].copy_from_slice(&(s as i16).to_le_bytes()),
                3 => {
                    let v = s << 8;
                    pcm[off..off + 3].copy_from_slice(&v.to_le_bytes()[..3]);
                }
                4 => pcm[off..off + 4].copy_from_slice(&(s << 16).to_le_bytes()),
                _ => {}
            }
        }
    }
    *phase += frames as f64;
}

fn tone_sample(phase: f64, sample_rate: f64) -> i16 {
    (2.0 * std::f64::consts::PI * 440.0 * phase / sample_rate)
        .sin()
        .mul_add(0.3 * 32767.0, 0.0)
        .round() as i16
}
