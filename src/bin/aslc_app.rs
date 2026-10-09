//! ASLC desktop control window (Windows-first).
//!
//! A small egui/eframe app that drives a [`aslc::session::SessionHandle`]: pick the phone link, the
//! WASAPI loopback source, the wire sample rate / bit depth, a software volume, then Start/Stop.
//!
//! The PC is the master: changing the sample rate or bit depth while streaming sends a CONFIGURE and
//! the phone follows live (no restart).
//!
//! Closing the window hides it to the system tray. Because eframe stops running `update()` while the
//! window is hidden, tray events are handled on a dedicated thread that restores the window with a
//! raw Win32 `ShowWindow` (egui commands only take effect during a frame).

// No console window: this is a GUI app.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use aslc::aoa::ReceiverDevice;
use aslc::payload::AudioInfo;
use aslc::session::{
    probe_capabilities, PhoneSelector, ProbeResult, SessionConfig, SessionEvent, SessionHandle,
};
use aslc::update::{self, Release};
use aslc::{capabilities::PcmCapabilities, PcmFormat};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 720.0])
            .with_min_inner_size([420.0, 560.0])
            .with_title("ASLC Node")
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "ASLC Node",
        options,
        Box::new(|cc| Box::new(AslcApp::new(cc))),
    )
}

/// A selectable audio source (render endpoint).
#[derive(Clone)]
struct DeviceItem {
    label: String,
    selector: Option<String>,
}

/// The full wire-rate menu; entries above what the phone advertises are filtered out at runtime.
const RATE_CHOICES: [(&str, Option<u32>); 7] = [
    ("Native (device best)", None),
    ("44.1 kHz", Some(44_100)),
    ("48 kHz", Some(48_000)),
    ("88.2 kHz", Some(88_200)),
    ("96 kHz", Some(96_000)),
    ("176.4 kHz", Some(176_400)),
    ("192 kHz", Some(192_000)),
];

/// The full bit-depth menu; filtered like `RATE_CHOICES`.
const DEPTH_CHOICES: [(&str, Option<u8>); 4] = [
    ("Native (device best)", None),
    ("16-bit", Some(16)),
    ("24-bit", Some(24)),
    ("32-bit", Some(32)),
];

/// In-app updater state (rendered under the transport controls).
#[derive(Default)]
enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading,
    Installing,
    Failed(String),
}

/// Messages from the updater worker thread.
enum UpdateMsg {
    UpToDate,
    Available(Release),
    Downloaded(std::path::PathBuf),
    Failed(String),
}

/// Top-level pages of the control window.
#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum Page {
    #[default]
    Home,
    Settings,
    Advanced,
    Diagnostics,
}

/// The user-facing connection state — drives the status pill and the primary button.
#[derive(PartialEq, Eq, Clone, Copy)]
enum ConnState {
    /// No phone is connected.
    NoDevice,
    /// A phone is connected and ready to stream.
    Ready,
    /// Opening the link / negotiating format.
    Connecting,
    /// Streaming audio.
    Streaming,
    /// Streaming, but paused.
    Paused,
    /// Something went wrong.
    Error,
}

struct AslcApp {
    receivers: Vec<ReceiverDevice>,
    receiver_idx: usize,

    devices: Vec<DeviceItem>,
    source_idx: usize,

    rate_labels: Vec<&'static str>,
    rate_values: Vec<Option<u32>>,
    rate_idx: usize,

    depth_labels: Vec<&'static str>,
    depth_values: Vec<Option<u8>>,
    depth_idx: usize,

    /// The phone's advertised PCM capability set (from its CAPABILITIES message); `None` until the
    /// first negotiation completes. Used to restrict the rate/depth menus to what it offers.
    caps: Option<PcmCapabilities>,
    /// The phone's audio-output characteristics (native output rate), from its `AUDIO_INFO` message.
    device_audio: Option<AudioInfo>,
    /// In-flight pre-start capability probe result, if any.
    probe_rx: Option<std::sync::mpsc::Receiver<Result<ProbeResult, String>>>,

    /// In-app updater: state + the in-flight check/download worker channel.
    update_state: UpdateState,
    update_rx: Option<std::sync::mpsc::Receiver<UpdateMsg>>,
    update_auto_checked: bool,

    /// Selected page.
    page: Page,
    /// Last error (cleared on Start) — drives the Home error state.
    error: Option<String>,
    /// Learned device names, keyed by USB serial (the phone reports its name in HELLO).
    known_names: std::collections::HashMap<String, String>,
    /// Last time the receiver list was refreshed (phones can be plugged in after launch).
    last_receiver_refresh: Instant,
    /// Whether an ASLC driver package is present in the driver store (`None` = checking).
    driver_installed: Option<bool>,
    driver_rx: Option<std::sync::mpsc::Receiver<bool>>,
    driver_auto_checked: bool,

    gain: f32,

    session: Option<SessionHandle>,
    terminal: bool,
    paused: bool,
    status: String,
    negotiated: Option<PcmFormat>,
    kbps: f64,
    /// (capture_ms, ring_fill_ms, ring_capacity_ms, device_ms, underruns)
    latency: Option<(u32, u16, u16, u16, u32)>,
    log: Vec<String>,

    // Tray + window management (shared with the tray thread).
    _tray: Option<TrayIcon>,
    hwnd: Arc<AtomicIsize>,
    want_start: Arc<AtomicBool>,
    want_stop: Arc<AtomicBool>,
    hidden: bool,
}

impl AslcApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let mut app = Self {
            receivers: Vec::new(),
            receiver_idx: 0,
            devices: Vec::new(),
            source_idx: 0,
            rate_labels: RATE_CHOICES.iter().map(|c| c.0).collect(),
            rate_values: RATE_CHOICES.iter().map(|c| c.1).collect(),
            rate_idx: 0,
            depth_labels: DEPTH_CHOICES.iter().map(|c| c.0).collect(),
            depth_values: DEPTH_CHOICES.iter().map(|c| c.1).collect(),
            depth_idx: 0,
            caps: None,
            device_audio: None,
            probe_rx: None,
            update_state: UpdateState::Idle,
            update_rx: None,
            update_auto_checked: false,
            page: Page::Home,
            error: None,
            known_names: load_known_names(),
            last_receiver_refresh: Instant::now(),
            driver_installed: None,
            driver_rx: None,
            driver_auto_checked: false,
            gain: 1.0,
            session: None,
            terminal: false,
            paused: false,
            status: "Idle".into(),
            negotiated: None,
            kbps: 0.0,
            latency: None,
            log: Vec::new(),
            _tray: None,
            hwnd: Arc::new(AtomicIsize::new(0)),
            want_start: Arc::new(AtomicBool::new(false)),
            want_stop: Arc::new(AtomicBool::new(false)),
            hidden: false,
        };
        app.refresh_devices();
        app.refresh_receivers();
        if !app.receivers.is_empty() {
            app.start_probe();
        }
        app._tray = app.build_tray();
        spawn_tray_thread(
            cc.egui_ctx.clone(),
            app.hwnd.clone(),
            app.want_start.clone(),
            app.want_stop.clone(),
        );
        app
    }

    fn build_tray(&mut self) -> Option<TrayIcon> {
        let menu = Menu::new();
        let show = MenuItem::with_id("show", "Show window", true, None);
        let start = MenuItem::with_id("start", "Start", true, None);
        let stop = MenuItem::with_id("stop", "Stop", true, None);
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        let _ = menu.append_items(&[&show, &start, &stop, &quit]);
        match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("ASLC Node")
            .with_icon(make_icon())
            .build()
        {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("tray icon unavailable: {e}");
                None
            }
        }
    }

    fn running(&self) -> bool {
        self.session.is_some()
    }

    /// The phone's "best" wire format for a Native selection: its reported native output rate when
    /// known (so it plays without resampling), else the highest rate it advertises; and its highest
    /// advertised bit depth. `None` until the capability probe has run.
    fn device_best_format(&self) -> (Option<u32>, Option<u8>) {
        let rate = match (&self.caps, &self.device_audio) {
            (Some(caps), Some(ai))
                if ai.output_sample_rate > 0
                    && caps.sample_rates.contains(&ai.output_sample_rate) =>
            {
                Some(ai.output_sample_rate)
            }
            (Some(caps), _) => caps.sample_rates.iter().copied().max(),
            _ => None,
        };
        let depth = self
            .caps
            .as_ref()
            .and_then(|c| c.bit_depths.iter().copied().max());
        (rate, depth)
    }

    fn refresh_receivers(&mut self) {
        let prev = self
            .receivers
            .get(self.receiver_idx)
            .map(|r| (r.vid, r.pid));
        self.receivers = aslc::aoa::list_receiver_devices();
        // The list is re-sorted (accessories first), so keep the user's pick by identity.
        self.receiver_idx = match prev {
            Some((vid, pid)) => self
                .receivers
                .iter()
                .position(|r| r.vid == vid && r.pid == pid)
                .unwrap_or(0),
            None => 0,
        };
    }

    /// Remember the phone's self-reported name for the currently selected receiver (by serial) and
    /// persist it so it shows immediately next launch.
    fn learn_name(&mut self, name: String) {
        let serial = match self.receivers.get(self.receiver_idx) {
            Some(r) => r.serial.clone(),
            None => String::new(),
        };
        let name = name.trim().to_string();
        if serial.is_empty() || name.is_empty() || name == "Android" {
            return;
        }
        if self.known_names.get(&serial) != Some(&name) {
            self.known_names.insert(serial, name);
            save_known_names(&self.known_names);
        }
    }

    fn selected_selector(&self) -> PhoneSelector {
        match self.receivers.get(self.receiver_idx) {
            Some(r) if r.accessory => PhoneSelector::Accessory,
            Some(r) => PhoneSelector::Handshake {
                vid: r.vid,
                pid: r.pid,
            },
            None => PhoneSelector::Accessory,
        }
    }

    #[cfg(windows)]
    fn refresh_devices(&mut self) {
        let list = aslc::audio::list_render_devices().unwrap_or_default();
        let mut items = vec![DeviceItem {
            label: "System default (follow)".into(),
            selector: None,
        }];
        items.extend(list.into_iter().map(|d| DeviceItem {
            label: format!(
                "{}{}  ({})",
                if d.is_default { "★ " } else { "" },
                d.name,
                d.format_label()
            ),
            selector: Some(d.name),
        }));
        self.devices = items;
        self.source_idx = self.source_idx.min(self.devices.len().saturating_sub(1));
    }

    #[cfg(not(windows))]
    fn refresh_devices(&mut self) {
        self.devices.clear();
    }

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn poll(&mut self) {
        let Some(s) = &self.session else {
            return;
        };
        let mut events = Vec::new();
        while let Some(ev) = s.try_recv() {
            events.push(ev);
        }
        for ev in events {
            match ev {
                SessionEvent::State(s) => {
                    self.status = s.clone();
                    self.push_log(s);
                }
                SessionEvent::Negotiated(f) => {
                    self.negotiated = Some(f);
                    self.push_log(format!("Negotiated {}", f.display_label()));
                }
                SessionEvent::Capabilities(caps) => {
                    self.push_log(format!(
                        "Phone offers {} rate(s), {} depth(s)",
                        caps.sample_rates.len(),
                        caps.bit_depths.len()
                    ));
                    self.caps = Some(caps);
                    self.apply_caps();
                }
                SessionEvent::DeviceAudio(ai) => {
                    if ai.output_sample_rate > 0 {
                        self.push_log(format!(
                            "Phone output: {} Hz · {} frames/buffer",
                            ai.output_sample_rate, ai.output_frames_per_buffer
                        ));
                    }
                    self.device_audio = Some(ai);
                }
                SessionEvent::DeviceName(name) => {
                    self.push_log(format!("Phone name: {name}"));
                    self.learn_name(name);
                }
                SessionEvent::Stats { kbps } => self.kbps = kbps,
                SessionEvent::Latency {
                    capture_ms,
                    ring_fill_ms,
                    ring_capacity_ms,
                    device_ms,
                    underruns,
                } => {
                    self.latency = Some((
                        capture_ms,
                        ring_fill_ms,
                        ring_capacity_ms,
                        device_ms,
                        underruns,
                    ));
                }
                SessionEvent::Paused(p) => self.paused = p,
                SessionEvent::Stopped(s) => {
                    self.status = s.clone();
                    self.push_log(format!("Stopped: {s}"));
                    self.terminal = true;
                }
                SessionEvent::Error(e) => {
                    self.status = format!("Error: {e}");
                    self.push_log(format!("Error: {e}"));
                    self.error = Some(e);
                    self.terminal = true;
                }
            }
        }
        if self.terminal {
            if let Some(mut h) = self.session.take() {
                h.join();
            }
            self.terminal = false;
            self.kbps = 0.0;
            self.paused = false;
            self.latency = None;
        }
    }

    /// Restrict the rate/depth menus to what the connected phone actually advertises (keeping the
    /// "Native" entries). If the current selection is no longer offered, fall back to "Native".
    fn apply_caps(&mut self) {
        let Some(caps) = self.caps.clone() else {
            return;
        };
        let cur_rate = self.rate_values.get(self.rate_idx).copied().flatten();
        let cur_depth = self.depth_values.get(self.depth_idx).copied().flatten();

        let rate_ok =
            |v: &Option<u32>| v.is_none() || v.is_some_and(|r| caps.sample_rates.contains(&r));
        self.rate_labels = RATE_CHOICES
            .iter()
            .filter(|c| rate_ok(&c.1))
            .map(|c| c.0)
            .collect();
        self.rate_values = RATE_CHOICES
            .iter()
            .filter(|c| rate_ok(&c.1))
            .map(|c| c.1)
            .collect();
        self.rate_idx = self
            .rate_values
            .iter()
            .position(|v| *v == cur_rate)
            .unwrap_or(0);

        let depth_ok =
            |v: &Option<u8>| v.is_none() || v.is_some_and(|d| caps.bit_depths.contains(&d));
        self.depth_labels = DEPTH_CHOICES
            .iter()
            .filter(|c| depth_ok(&c.1))
            .map(|c| c.0)
            .collect();
        self.depth_values = DEPTH_CHOICES
            .iter()
            .filter(|c| depth_ok(&c.1))
            .map(|c| c.1)
            .collect();
        self.depth_idx = self
            .depth_values
            .iter()
            .position(|v| *v == cur_depth)
            .unwrap_or(0);

        if self.rate_values.is_empty() {
            self.rate_labels = vec!["Native (follow source)"];
            self.rate_values = vec![None];
            self.rate_idx = 0;
        }
        if self.depth_values.is_empty() {
            self.depth_labels = vec!["Native"];
            self.depth_values = vec![None];
            self.depth_idx = 0;
        }
    }

    /// Kick off a read-only capability probe on the selected receiver (background thread). Safe to
    /// call repeatedly; the previous probe result (if any) is superseded.
    fn start_probe(&mut self) {
        if self.running() {
            return;
        }
        let cfg = SessionConfig {
            phone: self.selected_selector(),
            device: None,
            target_rate: None,
            target_depth: None,
            gain: 1.0,
            tone: false,
            wait_secs: 8,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(probe_capabilities(&cfg));
        });
        self.probe_rx = Some(rx);
        self.push_log("Probing device capabilities…".into());
    }

    /// Drain a finished probe, if any: store the caps + audio output info and constrain the menus.
    fn poll_probe(&mut self) {
        let Some(rx) = &self.probe_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(res)) => {
                self.push_log(format!(
                    "Device offers {} rate(s), {} depth(s)",
                    res.capabilities.sample_rates.len(),
                    res.capabilities.bit_depths.len()
                ));
                if let Some(ai) = res.audio_info {
                    if ai.output_sample_rate > 0 {
                        self.push_log(format!(
                            "Phone output: {} Hz · {} frames/buffer",
                            ai.output_sample_rate, ai.output_frames_per_buffer
                        ));
                    }
                    self.device_audio = Some(ai);
                }
                if let Some(name) = res.device_name {
                    self.push_log(format!("Phone name: {name}"));
                    self.learn_name(name);
                }
                self.caps = Some(res.capabilities);
                self.apply_caps();
                self.probe_rx = None;
            }
            Ok(Err(e)) => {
                self.push_log(format!("Probe: {e}"));
                self.probe_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.probe_rx = None,
        }
    }

    // ---- In-app updater ---------------------------------------------------------------------

    /// Query GitHub for the newest release in the background and see if it is newer than this build.
    fn start_update_check(&mut self) {
        if self.update_rx.is_some() {
            return;
        }
        self.update_state = UpdateState::Checking;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let msg = match update::fetch_latest() {
                Ok(rel) if rel.is_newer_than_this_build() => UpdateMsg::Available(rel),
                Ok(_) => UpdateMsg::UpToDate,
                Err(e) => UpdateMsg::Failed(e),
            };
            let _ = tx.send(msg);
        });
        self.update_rx = Some(rx);
    }

    /// Download the release's installer in the background.
    fn start_update_download(&mut self, rel: Release) {
        if self.update_rx.is_some() {
            return;
        }
        self.update_state = UpdateState::Downloading;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let msg = match update::download(&rel) {
                Ok(path) => UpdateMsg::Downloaded(path),
                Err(e) => UpdateMsg::Failed(e),
            };
            let _ = tx.send(msg);
        });
        self.update_rx = Some(rx);
    }

    /// Drain updater worker messages each frame.
    fn poll_update(&mut self) {
        let Some(rx) = &self.update_rx else {
            return;
        };
        let msg = match rx.try_recv() {
            Ok(m) => m,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.update_rx = None;
                self.update_state = UpdateState::Idle;
                return;
            }
        };
        self.update_rx = None;
        match msg {
            UpdateMsg::UpToDate => {
                self.push_log("Up to date".into());
                self.update_state = UpdateState::UpToDate;
            }
            UpdateMsg::Available(rel) => {
                self.push_log(format!("Update available: {}", rel.tag));
                self.update_state = UpdateState::Available(rel);
            }
            UpdateMsg::Downloaded(path) => {
                self.push_log(
                    "Installer downloaded — launching update, the app will restart".into(),
                );
                self.update_state = UpdateState::Installing;
                if let Err(e) = update::launch_installer(&path) {
                    self.update_state = UpdateState::Failed(e);
                } else {
                    // The installer replaces our binaries; exit so it can.
                    std::process::exit(0);
                }
            }
            UpdateMsg::Failed(e) => {
                self.push_log(format!("Update: {e}"));
                self.update_state = UpdateState::Failed(e);
            }
        }
    }

    // ---- USB driver presence ----------------------------------------------------------------

    /// Check (in the background) whether an ASLC driver package is in the driver store.
    fn start_driver_check(&mut self) {
        if self.driver_rx.is_some() {
            return;
        }
        self.driver_installed = None;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(aslc_driver_present());
        });
        self.driver_rx = Some(rx);
    }

    fn poll_driver(&mut self) {
        let Some(rx) = &self.driver_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(v) => {
                self.driver_installed = Some(v);
                self.driver_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.driver_rx = None,
        }
    }

    fn start(&mut self) {
        self.refresh_receivers();
        if self.receivers.is_empty() {
            self.status = "No phone detected".into();
            self.push_log(
                "No phone found. Set the phone's USB mode to 'File transfer' (MTP), plug it in, \
                 then try again."
                    .into(),
            );
            return;
        }
        let phone = self.selected_selector();
        let device = self
            .devices
            .get(self.source_idx)
            .and_then(|d| d.selector.clone());
        // Native means "let the phone pick its best": resolve it from the device's report.
        let (best_rate, best_depth) = self.device_best_format();
        let target_rate = self.rate_values[self.rate_idx].or(best_rate);
        let target_depth = self.depth_values[self.depth_idx].or(best_depth);
        if self.rate_values[self.rate_idx].is_none() {
            self.push_log(match target_rate {
                Some(r) => format!("Native rate → device best {r} Hz"),
                None => "Native rate → follow source (no device report yet)".into(),
            });
        }
        if self.depth_values[self.depth_idx].is_none() {
            self.push_log(match target_depth {
                Some(d) => format!("Native depth → device best {d}-bit"),
                None => "Native depth → follow source (no device report yet)".into(),
            });
        }
        let cfg = SessionConfig {
            phone,
            device,
            target_rate,
            target_depth,
            gain: self.gain,
            tone: false,
            wait_secs: 30,
        };
        self.status = "Starting…".into();
        self.error = None;
        self.negotiated = None;
        self.kbps = 0.0;
        self.latency = None;
        self.terminal = false;
        self.push_log("== Start ==".into());
        self.session = Some(SessionHandle::start(cfg));
    }

    fn pause(&mut self) {
        if let Some(s) = &self.session {
            s.pause();
        }
        self.status = "Pausing…".into();
    }

    fn resume(&mut self) {
        if let Some(s) = &self.session {
            s.resume();
        }
        self.status = "Resuming…".into();
    }

    fn driver_exe() -> Option<std::path::PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("aslc_driver.exe")))
    }

    /// Install the generic ASLC USB driver (elevated). Needed to talk to a phone without USB
    /// debugging; hides MTP while installed.
    fn install_driver(&mut self) {
        let Some(exe) = Self::driver_exe() else {
            self.push_log("aslc_driver.exe not found next to the app".into());
            return;
        };
        let args = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("driver").join("aslc_aoa.inf")))
            .filter(|p| p.exists())
            .map(|p| format!("install --inf \"{}\"", p.display()))
            .unwrap_or_else(|| "install".to_string());
        if run_elevated(&exe, &args) {
            self.push_log("Installing ASLC USB driver — approve the UAC prompt".into());
        } else {
            self.push_log("Driver install cancelled".into());
        }
    }

    /// Remove the ASLC USB driver so Windows re-binds the inbox MTP driver (file transfer returns).
    fn restore_mtp(&mut self) {
        let Some(exe) = Self::driver_exe() else {
            self.push_log("aslc_driver.exe not found next to the app".into());
            return;
        };
        if run_elevated(&exe, "remove") {
            self.push_log("Restoring file transfer (MTP) — approve the UAC prompt".into());
        } else {
            self.push_log("Restore MTP cancelled".into());
        }
    }

    // ---- Presentation -----------------------------------------------------------------------

    /// The user-facing connection state (drives the pill and the primary button).
    fn conn_state(&self) -> ConnState {
        if self.error.is_some() {
            return ConnState::Error;
        }
        if self.session.is_none() {
            return if self.receivers.is_empty() {
                ConnState::NoDevice
            } else {
                ConnState::Ready
            };
        }
        if self.paused {
            return ConnState::Paused;
        }
        if self.negotiated.is_none() {
            return ConnState::Connecting;
        }
        ConnState::Streaming
    }

    /// The selected source's friendly label.
    fn source_label(&self) -> String {
        self.devices
            .get(self.source_idx)
            .map(|d| d.label.clone())
            .unwrap_or_else(|| "System default".into())
    }

    /// The receiver's friendly name at `i` — the learned name, else the descriptor, else a plain
    /// "Android phone". Never the raw `VID:PID`.
    fn receiver_name_at(&self, i: usize) -> String {
        match self.receivers.get(i) {
            Some(r) => match self.known_names.get(&r.serial) {
                Some(n) if !n.trim().is_empty() => n.clone(),
                _ if !r.name.trim().is_empty() => r.name.clone(),
                _ => "Android phone".into(),
            },
            None => "No device".into(),
        }
    }

    fn receiver_label(&self) -> String {
        self.receiver_name_at(self.receiver_idx)
    }

    /// "48 kHz · 24-bit" for the negotiated format.
    fn format_summary(&self) -> Option<String> {
        self.negotiated.map(|f| {
            format!(
                "{:.0} kHz · {}-bit",
                f.sample_rate as f32 / 1000.0,
                f.bit_depth
            )
        })
    }

    /// Total added latency in ms (PC capture + USB + phone buffer).
    fn latency_total_ms(&self) -> Option<u32> {
        self.latency
            .map(|(cap, ring, _cap, dev, _u)| cap + 10 + ring as u32 + dev as u32)
    }

    fn status_pill(&self, ui: &mut egui::Ui) {
        let (text, dot, hollow) = match self.conn_state() {
            ConnState::NoDevice => ("No device", DOT_DIM, false),
            ConnState::Ready => ("Ready", DOT_MID, false),
            ConnState::Connecting => ("Connecting…", DOT_DIM, false),
            ConnState::Streaming => ("Streaming", DOT_ON, false),
            ConnState::Paused => ("Paused", DOT_MID, true),
            ConnState::Error => ("Problem", DOT_ON, true),
        };
        ui.horizontal(|ui| {
            state_dot(ui, dot, hollow);
            ui.label(egui::RichText::new(text).color(TEXT).strong());
        });
    }

    fn ui_top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("ASLC").size(21.0).strong().color(TEXT));
            ui.add_space(12.0);
            self.status_pill(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (value, label) in [
                    (Page::Diagnostics, "Diagnostics"),
                    (Page::Advanced, "Advanced"),
                    (Page::Settings, "Settings"),
                    (Page::Home, "Home"),
                ] {
                    let selected = self.page == value;
                    let text = egui::RichText::new(label).size(13.0).color(if selected {
                        TEXT
                    } else {
                        MUTED
                    });
                    if ui.selectable_label(selected, text).clicked() {
                        self.page = value;
                    }
                }
            });
        });
    }

    fn ui_home(&mut self, ui: &mut egui::Ui) {
        page_header(ui, "Home", "Stream this PC's audio to your phone over USB.");

        card(ui, "Audio source", |ui| {
            let mut idx = self.source_idx;
            egui::ComboBox::from_id_source("home_source")
                .width(ui.available_width())
                .selected_text(self.source_label())
                .show_ui(ui, |ui| {
                    for i in 0..self.devices.len() {
                        ui.selectable_value(&mut idx, i, self.devices[i].label.clone());
                    }
                });
            if idx != self.source_idx {
                self.source_idx = idx;
                let sel = self.devices.get(idx).and_then(|d| d.selector.clone());
                if let Some(s) = &self.session {
                    s.set_source(sel);
                }
                self.push_log(format!("Source → {}", self.source_label()));
            }
            hint(ui, "What is playing on this PC right now.");
        });

        card(ui, "Receiver", |ui| {
            if self.receivers.is_empty() {
                ui.label(egui::RichText::new("No phone found yet.").color(TEXT));
                hint(
                    ui,
                    "Connect your phone with a USB cable, and on the phone choose \
                     \u{201c}File transfer\u{201d} when asked.",
                );
                ui.add_space(10.0);
                if ui.button("Look again").clicked() {
                    self.refresh_receivers();
                    self.start_probe();
                }
            } else {
                let mut pick: Option<usize> = None;
                for i in 0..self.receivers.len() {
                    let name = self.receiver_name_at(i);
                    let ready = self.receivers[i].accessory;
                    if receiver_row(ui, &name, ready, i == self.receiver_idx).clicked()
                        && i != self.receiver_idx
                    {
                        pick = Some(i);
                    }
                }
                if let Some(i) = pick {
                    self.receiver_idx = i;
                    self.start_probe();
                }
                ui.add_space(10.0);
                if ui.button("Look again").clicked() {
                    self.refresh_receivers();
                    self.start_probe();
                }
            }
        });

        if self.driver_installed == Some(false) {
            card(ui, "One-time setup", |ui| {
                ui.label(
                    egui::RichText::new(
                        "This PC needs a small USB access setup before it can reach your phone.",
                    )
                    .color(TEXT),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Set up USB access").clicked() {
                        self.install_driver();
                    }
                    if ui.button("More info").clicked() {
                        self.page = Page::Advanced;
                    }
                });
            });
        }

        // Primary action
        ui.add_space(4.0);
        let state = self.conn_state();
        let running = self.session.is_some();
        let can_start = !self.receivers.is_empty();
        let (label, enabled) = match state {
            ConnState::Streaming | ConnState::Connecting => ("Stop streaming", true),
            ConnState::Paused => ("Resume streaming", true),
            _ => ("Start streaming", running || can_start),
        };
        let fill = if enabled {
            ACCENT
        } else {
            egui::Color32::from_gray(48)
        };
        let fg = if enabled { ON_ACCENT } else { FAINT };
        let button = egui::Button::new(egui::RichText::new(label).size(15.0).strong().color(fg))
            .fill(fill)
            .rounding(egui::Rounding::same(8.0))
            .min_size(egui::vec2(ui.available_width(), 42.0));
        if ui.add_enabled(enabled, button).clicked() {
            match state {
                ConnState::Streaming | ConnState::Connecting => self.pause(),
                ConnState::Paused => self.resume(),
                _ => self.start(),
            }
        }
        if !running && !can_start {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Connect a phone to start streaming.")
                    .size(12.0)
                    .color(FAINT),
            );
        }

        // Status
        ui.add_space(20.0);
        ui.separator();
        ui.add_space(12.0);
        self.status_pill(ui);
        if let Some(e) = &self.error {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(e.clone()).color(TEXT));
        } else if running {
            let mut bits = Vec::new();
            if let Some(f) = self.format_summary() {
                bits.push(f);
            }
            if let Some(ms) = self.latency_total_ms() {
                bits.push(format!("≈ {ms} ms latency"));
            }
            if !bits.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(bits.join("   ·   "))
                        .size(12.5)
                        .color(MUTED),
                );
            }
        }
        ui.add_space(12.0);
        row(ui, "From", |ui| {
            ui.label(egui::RichText::new(self.source_label()).color(TEXT));
        });
        ui.add_space(4.0);
        row(ui, "To", |ui| {
            ui.label(egui::RichText::new(self.receiver_label()).color(TEXT));
        });
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        page_header(ui, "Settings", "Updates and app information.");

        let mut do_check = false;
        let mut do_download: Option<Release> = None;
        card(ui, "Updates", |ui| {
            hint(
                ui,
                "ASLC checks for updates and always asks before installing anything.",
            );
            ui.add_space(10.0);
            match &self.update_state {
                UpdateState::Idle => {
                    if ui.button("Check for updates").clicked() {
                        do_check = true;
                    }
                }
                UpdateState::Checking => {
                    ui.label(egui::RichText::new("Checking for updates…").color(TEXT));
                }
                UpdateState::UpToDate => {
                    ui.label(egui::RichText::new("You're up to date.").color(TEXT));
                    ui.add_space(8.0);
                    if ui.button("Check again").clicked() {
                        do_check = true;
                    }
                }
                UpdateState::Downloading => {
                    ui.label(egui::RichText::new("Downloading the installer…").color(TEXT));
                }
                UpdateState::Installing => {
                    ui.label(
                        egui::RichText::new("Launching the installer — ASLC will restart.")
                            .color(TEXT),
                    );
                }
                UpdateState::Failed(e) => {
                    ui.label(egui::RichText::new(format!("Update failed: {e}")).color(TEXT));
                    ui.add_space(8.0);
                    if ui.button("Try again").clicked() {
                        do_check = true;
                    }
                }
                UpdateState::Available(rel) => {
                    let kind = if rel.prerelease { "nightly" } else { "release" };
                    ui.label(
                        egui::RichText::new(format!("Update available: {} ({kind})", rel.tag))
                            .color(TEXT),
                    );
                    ui.add_space(8.0);
                    if ui.button("Download & install").clicked() {
                        do_download = Some(rel.clone());
                    }
                }
            }
        });
        if do_check {
            self.start_update_check();
        }
        if let Some(rel) = do_download {
            self.start_update_download(rel);
        }

        card(ui, "About", |ui| {
            ui.label(
                egui::RichText::new(
                    "ASLC sends the audio playing on this PC to an Android phone over a USB cable.",
                )
                .color(TEXT),
            );
            ui.add_space(10.0);
            row(ui, "Version", |ui| {
                ui.label(egui::RichText::new(update::version()).color(TEXT));
            });
            ui.add_space(4.0);
            row(ui, "Build", |ui| {
                ui.label(egui::RichText::new(update::git_sha()).color(MUTED));
            });
        });
    }

    fn ui_advanced(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            "Advanced",
            "The defaults suit most setups — change these only if you need to.",
        );

        card(ui, "Audio quality", |ui| {
            row(ui, "Sample rate", |ui| {
                let mut rate_idx = self.rate_idx;
                egui::ComboBox::from_id_source("adv_rate")
                    .width(ui.available_width())
                    .selected_text(self.rate_labels[rate_idx])
                    .show_ui(ui, |ui| {
                        for i in 0..self.rate_labels.len() {
                            ui.selectable_value(&mut rate_idx, i, self.rate_labels[i]);
                        }
                    });
                if rate_idx != self.rate_idx {
                    self.rate_idx = rate_idx;
                    let v = self.rate_values[rate_idx];
                    let applied = v.or_else(|| self.device_best_format().0);
                    if let Some(s) = &self.session {
                        s.set_target_rate(applied);
                    }
                    self.push_log(format!("Rate → {}", self.rate_labels[rate_idx]));
                }
            });
            ui.add_space(8.0);
            row(ui, "Bit depth", |ui| {
                let mut depth_idx = self.depth_idx;
                egui::ComboBox::from_id_source("adv_depth")
                    .width(ui.available_width())
                    .selected_text(self.depth_labels[depth_idx])
                    .show_ui(ui, |ui| {
                        for i in 0..self.depth_labels.len() {
                            ui.selectable_value(&mut depth_idx, i, self.depth_labels[i]);
                        }
                    });
                if depth_idx != self.depth_idx {
                    self.depth_idx = depth_idx;
                    let v = self.depth_values[depth_idx];
                    let applied = v.or_else(|| self.device_best_format().1);
                    if let Some(s) = &self.session {
                        s.set_target_depth(applied);
                    }
                    self.push_log(format!("Depth → {}", self.depth_labels[depth_idx]));
                }
            });
            hint(
                ui,
                "\u{201c}Automatic\u{201d} lets the phone use its best setting.",
            );
            if let (Some(f), Some(ai)) = (self.negotiated, self.device_audio) {
                if ai.output_sample_rate > 0 && ai.output_sample_rate != f.sample_rate {
                    hint(
                        ui,
                        "Note: the phone will resample this stream to match its own output.",
                    );
                }
            }
            hint(
                ui,
                "Higher settings use more bandwidth and a larger phone buffer, which adds a \
                 little latency.",
            );
        });

        card(ui, "Volume boost", |ui| {
            row(ui, "Boost", |ui| {
                let mut boost = ((self.gain - 1.0).max(0.0) * 100.0).round();
                let w = ui.available_width();
                if ui
                    .add_sized(
                        [w, 24.0],
                        egui::Slider::new(&mut boost, 0.0..=100.0)
                            .suffix("%")
                            .fixed_decimals(0),
                    )
                    .changed()
                {
                    self.gain = 1.0 + boost / 100.0;
                    if let Some(s) = &self.session {
                        s.set_gain(self.gain);
                    }
                }
            });
            hint(
                ui,
                "Adds loudness on top of the PC volume. 0% leaves the audio unchanged.",
            );
        });

        card(ui, "USB access", |ui| {
            row(ui, "Status", |ui| {
                ui.label(
                    egui::RichText::new(match self.driver_installed {
                        Some(true) => "Ready",
                        Some(false) => "Setup needed",
                        None => "Checking…",
                    })
                    .color(TEXT),
                );
            });
            hint(
                ui,
                "ASLC uses a small generic USB driver to talk to any Android phone. While it's \
                 active, Windows file transfer for the phone is paused.",
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Set up USB access").clicked() {
                    self.install_driver();
                }
                if ui.button("Restore file transfer").clicked() {
                    self.restore_mtp();
                }
                if ui.button("Check again").clicked() {
                    self.start_driver_check();
                }
            });
        });
    }

    fn ui_diagnostics(&mut self, ui: &mut egui::Ui) {
        page_header(
            ui,
            "Diagnostics",
            "Connection details and the activity log.",
        );

        card(ui, "Connection", |ui| {
            row(ui, "Status", |ui| {
                ui.label(egui::RichText::new(self.status.clone()).color(TEXT));
            });
            if let Some(f) = self.negotiated {
                ui.add_space(4.0);
                row(ui, "Format", |ui| {
                    ui.label(egui::RichText::new(f.display_label()).color(TEXT));
                });
            }
            if self.kbps > 0.0 {
                ui.add_space(4.0);
                row(ui, "Throughput", |ui| {
                    ui.label(egui::RichText::new(format!("{:.0} kbit/s", self.kbps)).color(TEXT));
                });
            }
            if let Some(ms) = self.latency_total_ms() {
                ui.add_space(4.0);
                row(ui, "Latency", |ui| {
                    ui.label(egui::RichText::new(format!("≈ {ms} ms")).color(TEXT));
                });
            }
            if let Some((cap, ring, ring_cap, dev, underruns)) = self.latency {
                hint(
                    ui,
                    &format!(
                        "PC {} ms · phone {} + {} ms · buffer {ring_cap} ms · underruns {underruns}",
                        cap + 10,
                        ring,
                        dev
                    ),
                );
            }
            ui.add_space(4.0);
            row(ui, "USB driver", |ui| {
                ui.label(
                    egui::RichText::new(match self.driver_installed {
                        Some(true) => "Ready",
                        Some(false) => "Not installed",
                        None => "Checking…",
                    })
                    .color(TEXT),
                );
            });
        });

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Log").size(13.5).strong().color(TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    self.log.clear();
                }
            });
        });
        ui.add_space(8.0);
        let inner = (ui.available_width() - 20.0).max(0.0);
        egui::Frame::none()
            .fill(SUNKEN)
            .stroke(egui::Stroke::new(1.0_f32, HAIRLINE))
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::same(10.0))
            .show(ui, |ui| {
                ui.set_width(inner);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(egui::RichText::new("Nothing yet.").color(FAINT));
                        }
                        for line in &self.log {
                            ui.label(
                                egui::RichText::new(line)
                                    .size(11.5)
                                    .monospace()
                                    .color(MUTED),
                            );
                        }
                    });
            });
    }
}

impl eframe::App for AslcApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Remember the HWND so the tray thread can restore the window directly.
        if self.hwnd.load(Ordering::SeqCst) == 0 {
            #[cfg(windows)]
            if let Ok(wh) = frame.window_handle() {
                if let RawWindowHandle::Win32(h) = wh.as_raw() {
                    self.hwnd.store(h.hwnd.get(), Ordering::SeqCst);
                }
            }
        }

        // If we got here while marked hidden, we were restored externally (raw ShowWindow):
        // reconcile eframe's viewport state.
        if self.hidden {
            self.hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        }

        // Tray-initiated Start/Stop (set by the tray thread).
        if self.want_start.swap(false, Ordering::SeqCst) {
            if !self.running() {
                self.start();
            } else if self.paused {
                self.resume();
            }
        }
        if self.want_stop.swap(false, Ordering::SeqCst) && self.running() && !self.paused {
            self.pause();
        }

        // Phones can be plugged in (or switched to MTP) after launch: keep the list fresh.
        if self.last_receiver_refresh.elapsed() >= Duration::from_secs(2) {
            self.last_receiver_refresh = Instant::now();
            let before = self.receivers.len();
            self.refresh_receivers();
            if before == 0 && !self.receivers.is_empty() {
                // A phone just appeared: learn its capabilities so "Native" can use its best.
                self.start_probe();
            }
        }

        self.poll();
        self.poll_probe();
        self.poll_update();
        self.poll_driver();
        if !self.update_auto_checked {
            self.update_auto_checked = true;
            self.start_update_check();
        }
        if !self.driver_auto_checked {
            self.driver_auto_checked = true;
            self.start_driver_check();
        }

        // Close → minimize (stays running in the taskbar); tray restores it.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            self.hidden = true;
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(20.0, 18.0)),
            )
            .show(ctx, |ui| {
                // One centred column: identical margins and alignment at any window size.
                let full = ui.available_width();
                let content = full.min(720.0);
                let pad = ((full - content) * 0.5).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_width(content);
                        self.ui_top_bar(ui);
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(16.0);
                        match self.page {
                            Page::Home => self.ui_home(ui),
                            Page::Settings => self.ui_settings(ui),
                            Page::Advanced => self.ui_advanced(ui),
                            Page::Diagnostics => self.ui_diagnostics(ui),
                        }
                    });
                });
            });

        // Keep repainting while visible so session stats update; when hidden this stops (the tray
        // thread handles restore instead).
        let busy = self.session.is_some()
            || self.update_rx.is_some()
            || self.probe_rx.is_some()
            || self.driver_rx.is_some()
            || self.page == Page::Diagnostics;
        // Always wake periodically so the receiver list stays current and the window stays live.
        ctx.request_repaint_after(Duration::from_millis(if busy { 150 } else { 2000 }));
    }
}

// ---- Greyscale palette (no hues anywhere) -------------------------------------------------

const BG: egui::Color32 = egui::Color32::from_gray(17);
const CARD: egui::Color32 = egui::Color32::from_gray(27);
const HAIRLINE: egui::Color32 = egui::Color32::from_gray(46);
const SUNKEN: egui::Color32 = egui::Color32::from_gray(11);
const TEXT: egui::Color32 = egui::Color32::from_gray(236);
const MUTED: egui::Color32 = egui::Color32::from_gray(150);
const FAINT: egui::Color32 = egui::Color32::from_gray(104);
const ACCENT: egui::Color32 = egui::Color32::from_gray(238);
const ON_ACCENT: egui::Color32 = egui::Color32::from_gray(18);
const DOT_ON: egui::Color32 = egui::Color32::from_gray(242);
const DOT_MID: egui::Color32 = egui::Color32::from_gray(168);
const DOT_DIM: egui::Color32 = egui::Color32::from_gray(108);

/// The fixed width of the label column in a settings row.
const LABEL_W: f32 = 104.0;

/// A dark, purely greyscale theme: every widget state is a shade of grey (no hues), with uniform
/// rounding, hairline borders and no shadows. Nothing from egui's stock blue theme survives.
fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;

    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = SUNKEN;
    v.faint_bg_color = egui::Color32::from_gray(32);
    v.window_rounding = egui::Rounding::same(10.0);
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.selection.bg_fill = egui::Color32::from_gray(64);
    v.selection.stroke = egui::Stroke::new(1.0_f32, TEXT);
    v.hyperlink_color = TEXT;
    v.text_cursor = egui::Stroke::new(1.5_f32, TEXT);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, HAIRLINE);
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, MUTED);
    w.noninteractive.rounding = egui::Rounding::same(8.0);
    w.inactive.bg_fill = egui::Color32::from_gray(38);
    w.inactive.weak_bg_fill = egui::Color32::from_gray(33);
    w.inactive.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(58));
    w.inactive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    w.inactive.rounding = egui::Rounding::same(8.0);
    w.hovered.bg_fill = egui::Color32::from_gray(54);
    w.hovered.weak_bg_fill = egui::Color32::from_gray(48);
    w.hovered.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(96));
    w.hovered.fg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(255));
    w.hovered.rounding = egui::Rounding::same(8.0);
    w.active.bg_fill = egui::Color32::from_gray(70);
    w.active.weak_bg_fill = egui::Color32::from_gray(62);
    w.active.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(128));
    w.active.fg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(255));
    w.active.rounding = egui::Rounding::same(8.0);
    w.open.bg_fill = egui::Color32::from_gray(42);
    w.open.weak_bg_fill = egui::Color32::from_gray(38);
    w.open.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(70));
    w.open.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    w.open.rounding = egui::Rounding::same(8.0);

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);
    style.spacing.interact_size.y = 28.0;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating = false;

    ctx.set_style(style);
}

/// The page title block — identical on every page so the pages line up.
fn page_header(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.label(egui::RichText::new(title).size(19.0).strong().color(TEXT));
    ui.add_space(2.0);
    ui.label(egui::RichText::new(subtitle).size(12.5).color(MUTED));
    ui.add_space(16.0);
}

/// A card: a full-width, evenly-padded surface with a hairline border.
fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    let inner = (ui.available_width() - 32.0).max(0.0);
    egui::Frame::none()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0_f32, HAIRLINE))
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::same(16.0))
        .show(ui, |ui| {
            ui.set_width(inner);
            ui.label(egui::RichText::new(title).size(13.5).strong().color(TEXT));
            ui.add_space(10.0);
            body(ui);
        });
    ui.add_space(12.0);
}

/// A secondary line inside a card.
fn hint(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(text).size(12.0).color(FAINT));
}

/// A label/control row with a fixed-width label column, so rows align down the page.
fn row<R>(ui: &mut egui::Ui, label: &str, control: impl FnOnce(&mut egui::Ui) -> R) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_W, 28.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(egui::RichText::new(label).size(12.5).color(MUTED));
            },
        );
        control(ui);
    });
}

/// A small circle (a real shape, so it always renders). Hollow = outlined.
fn state_dot(ui: &mut egui::Ui, color: egui::Color32, hollow: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let center = rect.center();
    if hollow {
        ui.painter()
            .circle_stroke(center, 4.0, egui::Stroke::new(1.5_f32, color));
    } else {
        ui.painter().circle_filled(center, 4.0, color);
    }
}

/// A full-width receiver row: name on the left, availability on the right.
fn receiver_row(ui: &mut egui::Ui, name: &str, ready: bool, selected: bool) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::click());
    let fill = ui.style().interact_selectable(&resp, selected).weak_bg_fill;
    if selected || resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::Rounding::same(8.0), fill);
    }
    if selected {
        ui.painter().rect_stroke(
            rect,
            egui::Rounding::same(8.0),
            egui::Stroke::new(1.0_f32, HAIRLINE),
        );
    }
    ui.painter().text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.5),
        TEXT,
    );
    ui.painter().text(
        rect.right_center() - egui::vec2(10.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        if ready { "Ready" } else { "Available" },
        egui::FontId::proportional(12.0),
        if ready { MUTED } else { FAINT },
    );
    resp
}

/// True when a driver package whose original name is `aslc_aoa.inf` is present in the driver store.
/// `pnputil /enum-drivers` works without elevation.
fn aslc_driver_present() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        match std::process::Command::new("pnputil")
            .arg("/enum-drivers")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
                text.contains("aslc_aoa.inf")
            }
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Where learned device names live: `%LOCALAPPDATA%\ASLC Node\device-names.txt`, `serial<TAB>name`.
fn names_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("XDG_CONFIG_HOME"))?;
    Some(
        std::path::PathBuf::from(base)
            .join("ASLC Node")
            .join("device-names.txt"),
    )
}

fn load_known_names() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(path) = names_file() else {
        return map;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return map;
    };
    for line in text.lines() {
        if let Some((serial, name)) = line.split_once('\t') {
            let (serial, name) = (serial.trim(), name.trim());
            if !serial.is_empty() && !name.is_empty() {
                map.insert(serial.to_string(), name.to_string());
            }
        }
    }
    map
}

fn save_known_names(map: &std::collections::HashMap<String, String>) {
    let Some(path) = names_file() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut text = String::new();
    for (serial, name) in map {
        text.push_str(serial);
        text.push('\t');
        text.push_str(name);
        text.push('\n');
    }
    let _ = std::fs::write(&path, text);
}

/// Handle tray events on a dedicated thread. While the window is hidden eframe does not run
/// `update()`, so restoring must not depend on the UI loop.
fn spawn_tray_thread(
    ctx: egui::Context,
    hwnd: Arc<AtomicIsize>,
    want_start: Arc<AtomicBool>,
    want_stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || loop {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.0.as_str() {
                "show" => restore_window(&ctx, &hwnd),
                "start" => {
                    want_start.store(true, Ordering::SeqCst);
                    restore_window(&ctx, &hwnd);
                }
                "stop" => {
                    want_stop.store(true, Ordering::SeqCst);
                    restore_window(&ctx, &hwnd);
                }
                "quit" => std::process::exit(0),
                _ => {}
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                restore_window(&ctx, &hwnd);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    });
}

fn restore_window(ctx: &egui::Context, hwnd: &AtomicIsize) {
    let h = hwnd.load(Ordering::SeqCst);
    if h != 0 {
        #[cfg(windows)]
        show_window_raw(h);
    }
    // Wake the UI loop so it resumes repainting (and reconciles the viewport state).
    ctx.request_repaint();
}

#[cfg(windows)]
#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut std::ffi::c_void,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        nshow: i32,
    ) -> *mut std::ffi::c_void;
}

/// Launch `exe` elevated (UAC) with `args`. Returns false if the user declined.
#[cfg(windows)]
fn run_elevated(exe: &std::path::Path, args: &str) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }
    let op = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let params = wide(OsStr::new(args));
    let h = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1,
        )
    };
    (h as isize) > 32
}

#[cfg(not(windows))]
fn run_elevated(_exe: &std::path::Path, _args: &str) -> bool {
    false
}

#[cfg(windows)]
fn show_window_raw(hwnd: isize) {
    use std::ffi::c_void;
    extern "system" {
        fn ShowWindow(hwnd: *mut c_void, ncmdshow: i32) -> i32;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
    }
    const SW_RESTORE: i32 = 9;
    unsafe {
        ShowWindow(hwnd as *mut c_void, SW_RESTORE);
        SetForegroundWindow(hwnd as *mut c_void);
    }
}

/// Rasterise a filled, anti-aliased disc — the shared mark for the tray and window icons.
fn disc_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    let r = c - 1.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = (dx * dx + dy * dy).sqrt();
            let a = ((r + 0.5 - d).clamp(0.0, 1.0) * 255.0).round() as u8;
            if a > 0 {
                let i = ((y * size + x) * 4) as usize;
                rgba[i] = 0xEC;
                rgba[i + 1] = 0xEC;
                rgba[i + 2] = 0xEC;
                rgba[i + 3] = a;
            }
        }
    }
    rgba
}

/// A simple light disc for the system tray, without shipping an asset file.
fn make_icon() -> tray_icon::Icon {
    let size = 32u32;
    tray_icon::Icon::from_rgba(disc_rgba(size), size, size).expect("valid icon")
}

/// The same disc, larger, for the taskbar / Alt-Tab icon.
fn window_icon() -> egui::IconData {
    let size = 64u32;
    egui::IconData {
        rgba: disc_rgba(size),
        width: size,
        height: size,
    }
}
