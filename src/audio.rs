//! WASAPI loopback capture (Windows) as the ASLC desktop PCM source.
//!
//! The user selects a Windows **render** endpoint; we open a WASAPI loopback capture on it
//! (the `wasapi` crate sets `AUDCLNT_STREAMFLAGS_LOOPBACK` for render→capture in shared mode),
//! optionally mute that endpoint while streaming, and feed the captured PCM into the ASLC
//! pipeline via [`LoopbackSource::fill`]. No kernel driver; no changes to the ASLC core.
//!
//! Typical use with a third-party virtual cable (e.g. VB-CABLE): point apps/system output at
//! "CABLE Input", then capture that endpoint here.

#![cfg(windows)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use wasapi::{initialize_mta, Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use crate::format::PcmFormat;

/// ~2 s cap on the capture buffer at 48 kHz stereo.
const MAX_SAMPLES: usize = 96_000 * 2;

/// Ready message from the capture thread: (rate, channels, bits, subformat, device id, device name,
/// period ms).
type CaptureReady = Result<(u32, u16, u16, String, String, String, u32), String>;

#[derive(Debug, Clone)]
pub struct RenderDeviceInfo {
    pub index: usize,
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub sample_type: String,
}

impl RenderDeviceInfo {
    pub fn format_label(&self) -> String {
        format!(
            "{} Hz, {} ch, {}-bit {}",
            self.sample_rate, self.channels, self.bits, self.sample_type
        )
    }
}

/// Enumerate active Windows render endpoints (friendly name + shared-mode mix format).
pub fn list_render_devices() -> Result<Vec<RenderDeviceInfo>, String> {
    let _ = initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let default_id = enumerator
        .get_default_device(&Direction::Render)
        .ok()
        .and_then(|d| d.get_id().ok());

    let collection = enumerator
        .get_device_collection(&Direction::Render)
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    for (i, device) in (&collection).into_iter().enumerate() {
        let dev = match device {
            Ok(d) => d,
            Err(_) => continue,
        };
        let name = dev.get_friendlyname().unwrap_or_else(|_| "?".into());
        let id = dev.get_id().unwrap_or_default();
        let (sr, ch, bits, st) = match dev.get_iaudioclient().and_then(|c| c.get_mixformat()) {
            Ok(f) => (
                f.get_samplespersec(),
                f.get_nchannels(),
                f.get_bitspersample(),
                f.get_subformat()
                    .map(|s| format!("{s}"))
                    .unwrap_or_else(|_| "?".into()),
            ),
            Err(_) => (0, 0, 0, "?".to_string()),
        };
        out.push(RenderDeviceInfo {
            index: i,
            id: id.clone(),
            name,
            is_default: default_id.as_deref() == Some(id.as_str()),
            sample_rate: sr,
            channels: ch,
            bits,
            sample_type: st,
        });
    }
    Ok(out)
}

/// The endpoint id of the current default render device (for "follow system default").
pub fn default_render_device_id() -> Option<String> {
    list_render_devices()
        .ok()?
        .into_iter()
        .find(|d| d.is_default)
        .map(|d| d.id)
}

/// Resolve a selector (index, or case-insensitive substring of the name) to a render device.
fn resolve_device(enumerator: &DeviceEnumerator, selector: Option<&str>) -> Result<Device, String> {    if let Some(sel) = selector {
        if let Ok(idx) = sel.parse::<usize>() {
            let collection = enumerator
                .get_device_collection(&Direction::Render)
                .map_err(|e| e.to_string())?;
            return (&collection)
                .into_iter()
                .nth(idx)
                .ok_or_else(|| format!("no render device at index {idx}"))?
                .map_err(|e| e.to_string());
        }
        let needle = sel.to_ascii_lowercase();
        let collection = enumerator
            .get_device_collection(&Direction::Render)
            .map_err(|e| e.to_string())?;
        for device in (&collection).into_iter().flatten() {
            if let Ok(name) = device.get_friendlyname() {
                if name.to_ascii_lowercase().contains(&needle) {
                    return Ok(device);
                }
            }
        }
        return Err(format!("no render device matching '{sel}'"));
    }
    enumerator
        .get_default_device(&Direction::Render)
        .map_err(|e| format!("no default render device: {e}"))
}

pub struct LoopbackSource {
    queue: Arc<Mutex<VecDeque<f32>>>,
    native_rate: u32,
    native_channels: u16,
    native_bits: u16,
    device_id: String,
    device_name: String,
    period_ms: u32,
    /// Software output gain applied to every sample (1.0 = unity, set via [`set_gain`]).
    gain: f32,
    scratch: Vec<f32>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LoopbackSource {
    /// Open a loopback capture on the selected render endpoint. If `mute`, that endpoint is
    /// muted while streaming and restored when the source is dropped.
    pub fn open(selector: Option<&str>) -> Result<Self, String> {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(MAX_SAMPLES)));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = channel::<CaptureReady>();

        let (q, s) = (queue.clone(), stop.clone());
        let selector = selector.map(|s| s.to_string());
        let thread = std::thread::Builder::new()
            .name("aslc-loopback".into())
            .spawn(move || capture_thread(selector.as_deref(), q, s, ready_tx))
            .map_err(|e| format!("spawn capture thread: {e}"))?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok((rate, channels, bits, stype, id, name, period_ms))) => {
                println!("loopback capture: {rate} Hz, {channels} ch, {bits}-bit {stype} [{name}]");
                Ok(Self {
                    queue,
                    native_rate: rate,
                    native_channels: channels,
                    native_bits: bits,
                    device_id: id,
                    device_name: name,
                    period_ms,
                    gain: 1.0,
                    scratch: Vec::with_capacity(8192),
                    stop,
                    thread: Some(thread),
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("loopback capture did not start within 10s".into()),
        }
    }

    pub fn native_rate(&self) -> u32 {
        self.native_rate
    }

    pub fn native_channels(&self) -> u16 {
        self.native_channels
    }

    /// Capture device mix bit depth (what we prefer to negotiate, to avoid quantizing).
    pub fn native_bits(&self) -> u16 {
        self.native_bits
    }

    /// Stable endpoint id of the opened device (for "follow the system default" tracking).
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Friendly name of the opened device (for display).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Capture buffer period (ms) the loopback was opened with (a lower bound on capture latency).
    pub fn period_ms(&self) -> u32 {
        self.period_ms
    }

    /// Software output gain applied to every captured sample (1.0 = unity). Clamped to 0..=4.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain.clamp(0.0, 4.0);
    }

    /// Fill `out` with interleaved integer PCM in `fmt`, converting sample format, channel count
    /// and (if needed) resampling. Returns the number of output frames that were silence.
    pub fn fill(&mut self, fmt: &PcmFormat, out: &mut [u8]) -> usize {
        let bpf = fmt.bytes_per_frame();
        if bpf == 0 || fmt.channels == 0 {
            return 0;
        }
        let out_frames = out.len() / bpf;
        let nc = self.native_channels.max(1) as usize;
        let out_ch = fmt.channels as usize;

        let mut guard = match self.queue.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let (want_native, mode) = if fmt.sample_rate == self.native_rate {
            (out_frames, Resample::Direct)
        } else if self.native_rate % fmt.sample_rate == 0 {
            let d = (self.native_rate / fmt.sample_rate) as usize;
            (out_frames * d, Resample::Decimate(d))
        } else {
            (
                ((out_frames as u64 * self.native_rate as u64) / fmt.sample_rate as u64) as usize + 1,
                Resample::Linear,
            )
        };
        let have_frames = guard.len() / nc;
        let take = want_native.min(have_frames);

        self.scratch.clear();
        self.scratch.extend(guard.drain(0..take * nc));
        drop(guard);

        let mut silent = 0usize;
        for i in 0..out_frames {
            let mut frame_silent = true;
            for ch in 0..out_ch {
                let v = match mode {
                    Resample::Direct => self.sample(i, ch, nc, out_ch, take),
                    Resample::Decimate(d) => {
                        let base = i * d;
                        (0..d)
                            .map(|k| self.sample(base + k, ch, nc, out_ch, take))
                            .sum::<f32>()
                            / d as f32
                    }
                    Resample::Linear => {
                        let src = i as f64 * self.native_rate as f64 / fmt.sample_rate as f64;
                        let nf = src.floor() as usize;
                        let frac = (src - src.floor()) as f32;
                        let a = self.sample(nf, ch, nc, out_ch, take);
                        let b = self.sample(nf + 1, ch, nc, out_ch, take);
                        a + (b - a) * frac
                    }
                };
                if v != 0.0 {
                    frame_silent = false;
                }
                write_sample(fmt, out, i, ch, v * self.gain);
            }
            if frame_silent {
                silent += 1;
            }
        }
        silent
    }

    fn sample(&self, idx: usize, ch: usize, nc: usize, out_ch: usize, take: usize) -> f32 {
        if idx >= take {
            return 0.0;
        }
        let base = idx * nc;
        if nc == 1 {
            self.scratch[base]
        } else if out_ch == 1 {
            self.scratch[base..base + nc].iter().sum::<f32>() / nc as f32
        } else {
            self.scratch[base + ch.min(nc - 1)]
        }
    }
}

impl Drop for LoopbackSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone, Copy)]
enum Resample {
    Direct,
    Decimate(usize),
    Linear,
}

fn capture_thread(
    selector: Option<&str>,
    queue: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
    ready: Sender<CaptureReady>,
) {
    if initialize_mta().is_err() {
        let _ = ready.send(Err("COM (MTA) init failed".into()));
        return;
    }
    let enumerator = match DeviceEnumerator::new() {
        Ok(e) => e,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let device = match resolve_device(&enumerator, selector) {
        Ok(d) => d,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let device_id = device.get_id().unwrap_or_default();
    let device_name = device
        .get_friendlyname()
        .unwrap_or_else(|_| "?".into());
    let mut client = match device.get_iaudioclient() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(format!("get IAudioClient: {e}")));
            return;
        }
    };
    let mix = match client.get_mixformat() {
        Ok(f) => f,
        Err(e) => {
            let _ = ready.send(Err(format!("get mix format: {e}")));
            return;
        }
    };
    // Use the device's default period; some endpoints (notably at high rates) deliver no loopback
    // data when initialized with the minimum period.
    let (def, _min) = client.get_device_period().unwrap_or((100_000, 100_000));
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def,
    };
    if let Err(e) = client.initialize_client(&mix, &Direction::Capture, &mode) {
        let _ = ready.send(Err(format!("initialize loopback capture: {e}")));
        return;
    }
    let event = match client.set_get_eventhandle() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(format!("event handle: {e}")));
            return;
        }
    };
    let capture = match client.get_audiocaptureclient() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(format!("capture client: {e}")));
            return;
        }
    };

    if let Err(e) = client.start_stream() {
        let _ = ready.send(Err(format!("start stream: {e}")));
        return;
    }
    let _ = ready.send(Ok((
        mix.get_samplespersec(),
        mix.get_nchannels(),
        mix.get_bitspersample(),
        mix.get_subformat()
            .map(|s| format!("{s}"))
            .unwrap_or_else(|_| "?".into()),
        device_id,
        device_name,
        (def / 10_000) as u32,
    )));

    let mut local: VecDeque<u8> = VecDeque::with_capacity(65536);
    while !stop.load(Ordering::SeqCst) {
        if event.wait_for_event(200).is_err() {
            break;
        }
        if capture.read_from_device_to_deque(&mut local).is_err() {
            break;
        }
        if !local.is_empty() {
            if let Ok(mut q) = queue.lock() {
                convert_to_f32(&local, &mix, &mut q);
                while q.len() > MAX_SAMPLES {
                    q.pop_front();
                }
            }
            local.clear();
        }
    }

    let _ = client.stop_stream();
}

/// Convert a raw mix-format byte run into interleaved f32 samples.
fn convert_to_f32(bytes: &VecDeque<u8>, mix: &WaveFormat, out: &mut VecDeque<f32>) {
    let channels = mix.get_nchannels() as usize;
    if channels == 0 {
        return;
    }
    let bytes_per_sample = (mix.get_bitspersample() as usize).max(8) / 8;
    let frame_bytes = bytes_per_sample * channels;
    if frame_bytes == 0 {
        return;
    }
    let n_frames = bytes.len() / frame_bytes;
    let slice: Vec<u8> = bytes.iter().copied().collect();
    for fr in 0..n_frames {
        let base = fr * frame_bytes;
        for _c in 0..channels {
            let off = base + _c * bytes_per_sample;
            out.push_back(sample_to_f32(&slice[off..off + bytes_per_sample], mix));
        }
    }
}

fn sample_to_f32(s: &[u8], mix: &WaveFormat) -> f32 {
    let bits = mix.get_bitspersample();
    match mix.get_subformat().unwrap_or(SampleType::Float) {
        SampleType::Float => {
            if bits == 32 && s.len() >= 4 {
                f32::from_le_bytes([s[0], s[1], s[2], s[3]])
            } else if bits == 64 && s.len() >= 8 {
                let d = f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
                d as f32
            } else {
                0.0
            }
        }
        SampleType::Int => match bits {
            16 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
            24 => {
                let v = (s[0] as i32) | ((s[1] as i32) << 8) | ((s[2] as i32) << 16);
                let v = (v << 8) >> 8; // sign-extend 24-bit
                v as f32 / 8_388_608.0
            }
            32 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
            _ => 0.0,
        },
    }
}

fn write_sample(fmt: &PcmFormat, out: &mut [u8], frame: usize, ch: usize, v: f32) {
    let bps = fmt.bytes_per_sample();
    let off = (frame * fmt.channels as usize + ch) * bps;
    let v = v.clamp(-1.0, 1.0);
    match fmt.bit_depth {
        16 => out[off..off + 2].copy_from_slice(&((v * 32767.0).round() as i16).to_le_bytes()),
        24 => out[off..off + 3]
            .copy_from_slice(&((v * 8_388_607.0).round() as i32).to_le_bytes()[..3]),
        32 => out[off..off + 4]
            .copy_from_slice(&((v * 2_147_483_647.0).round() as i32).to_le_bytes()),
        _ => {}
    }
}
