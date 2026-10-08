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
use std::time::Duration;

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
use aslc::{capabilities::PcmCapabilities, PcmFormat};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 600.0])
            .with_min_inner_size([460.0, 420.0])
            .with_title("ASLC Node"),
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
    ("Native (follow source)", None),
    ("44.1 kHz", Some(44_100)),
    ("48 kHz", Some(48_000)),
    ("88.2 kHz", Some(88_200)),
    ("96 kHz", Some(96_000)),
    ("176.4 kHz", Some(176_400)),
    ("192 kHz", Some(192_000)),
];

/// The full bit-depth menu; filtered like `RATE_CHOICES`.
const DEPTH_CHOICES: [(&str, Option<u8>); 4] = [
    ("Native", None),
    ("16-bit", Some(16)),
    ("24-bit", Some(24)),
    ("32-bit", Some(32)),
];

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

    fn refresh_receivers(&mut self) {
        self.receivers = aslc::aoa::list_receiver_devices();
        self.receiver_idx = self.receiver_idx.min(self.receivers.len().saturating_sub(1));
    }

    fn receiver_display(&self, r: &ReceiverDevice) -> String {
        if r.accessory {
            format!("{}  · ready", r.default_label())
        } else {
            r.default_label()
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
                SessionEvent::Stats { kbps } => self.kbps = kbps,
                SessionEvent::Latency {
                    capture_ms,
                    ring_fill_ms,
                    ring_capacity_ms,
                    device_ms,
                    underruns,
                } => {
                    self.latency =
                        Some((capture_ms, ring_fill_ms, ring_capacity_ms, device_ms, underruns));
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

        let rate_ok = |v: &Option<u32>| v.is_none() || v.is_some_and(|r| caps.sample_rates.contains(&r));
        self.rate_labels = RATE_CHOICES.iter().filter(|c| rate_ok(&c.1)).map(|c| c.0).collect();
        self.rate_values = RATE_CHOICES.iter().filter(|c| rate_ok(&c.1)).map(|c| c.1).collect();
        self.rate_idx = self.rate_values.iter().position(|v| *v == cur_rate).unwrap_or(0);

        let depth_ok =
            |v: &Option<u8>| v.is_none() || v.is_some_and(|d| caps.bit_depths.contains(&d));
        self.depth_labels =
            DEPTH_CHOICES.iter().filter(|c| depth_ok(&c.1)).map(|c| c.0).collect();
        self.depth_values =
            DEPTH_CHOICES.iter().filter(|c| depth_ok(&c.1)).map(|c| c.1).collect();
        self.depth_idx = self.depth_values.iter().position(|v| *v == cur_depth).unwrap_or(0);

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

    fn start(&mut self) {
        let phone = self.selected_selector();
        let device = self
            .devices
            .get(self.source_idx)
            .and_then(|d| d.selector.clone());
        let cfg = SessionConfig {
            phone,
            device,
            target_rate: self.rate_values[self.rate_idx],
            target_depth: self.depth_values[self.depth_idx],
            gain: self.gain,
            tone: false,
            wait_secs: 30,
        };
        self.status = "Starting…".into();
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

        self.poll();
        self.poll_probe();

        // Close → minimize (stays running in the taskbar); tray restores it.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            self.hidden = true;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("ASLC Node");
            ui.label("Stream this PC's audio to the phone over USB (AOA). The PC controls the format.");
            ui.separator();

            egui::Grid::new("settings")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Receiver:");
                    ui.horizontal(|ui| {
                        let mut idx = self.receiver_idx;
                        let label = self
                            .receivers
                            .get(idx)
                            .map(|r| self.receiver_display(r))
                            .unwrap_or_else(|| "(no receiver found)".into());
                        egui::ComboBox::from_id_source("receiver")
                            .width(300.0)
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                for i in 0..self.receivers.len() {
                                    let text = self.receiver_display(&self.receivers[i]);
                                    ui.selectable_value(&mut idx, i, text);
                                }
                            });
                        if idx != self.receiver_idx {
                            self.receiver_idx = idx;
                            self.start_probe();
                        }
                        if ui.button("Refresh").clicked() {
                            self.refresh_receivers();
                            self.start_probe();
                        }
                    });
                    ui.end_row();

                    ui.label("Source:");
                    ui.horizontal(|ui| {
                        let mut source_idx = self.source_idx;
                        let label = self
                            .devices
                            .get(source_idx)
                            .map(|d| d.label.clone())
                            .unwrap_or_else(|| "(none)".into());
                        egui::ComboBox::from_id_source("source")
                            .width(300.0)
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                for i in 0..self.devices.len() {
                                    let text = self.devices[i].label.clone();
                                    ui.selectable_value(&mut source_idx, i, text);
                                }
                            });
                        if source_idx != self.source_idx {
                            self.source_idx = source_idx;
                            let sel = self
                                .devices
                                .get(source_idx)
                                .and_then(|d| d.selector.clone());
                            let lbl = self.devices[source_idx].label.clone();
                            if let Some(s) = &self.session {
                                s.set_source(sel);
                                self.push_log(format!("Source → {lbl}"));
                            }
                        }
                        if ui.button("Refresh").clicked() {
                            self.refresh_devices();
                        }
                    });
                    ui.end_row();

                    ui.label("Sample rate:");
                    let mut rate_idx = self.rate_idx;
                    egui::ComboBox::from_id_source("rate")
                        .selected_text(self.rate_labels[rate_idx])
                        .show_ui(ui, |ui| {
                            for i in 0..self.rate_labels.len() {
                                ui.selectable_value(&mut rate_idx, i, self.rate_labels[i]);
                            }
                        });
                    if rate_idx != self.rate_idx {
                        self.rate_idx = rate_idx;
                        let v = self.rate_values[rate_idx];
                        if let Some(s) = &self.session {
                            s.set_target_rate(v);
                            self.push_log(format!("Wire rate → {}", self.rate_labels[rate_idx]));
                        }
                    }
                    ui.end_row();

                    ui.label("Bit depth:");
                    let mut depth_idx = self.depth_idx;
                    egui::ComboBox::from_id_source("depth")
                        .selected_text(self.depth_labels[depth_idx])
                        .show_ui(ui, |ui| {
                            for i in 0..self.depth_labels.len() {
                                ui.selectable_value(&mut depth_idx, i, self.depth_labels[i]);
                            }
                        });
                    if depth_idx != self.depth_idx {
                        self.depth_idx = depth_idx;
                        let v = self.depth_values[depth_idx];
                        if let Some(s) = &self.session {
                            s.set_target_depth(v);
                            self.push_log(format!("Wire depth → {}", self.depth_labels[depth_idx]));
                        }
                    }
                    ui.end_row();

                    if let (Some(f), Some(ai)) = (self.negotiated, self.device_audio) {
                        if ai.output_sample_rate > 0 && ai.output_sample_rate != f.sample_rate {
                            ui.label("");
                            ui.colored_label(
                                egui::Color32::from_rgb(230, 180, 60),
                                format!(
                                    "⚠ Phone outputs at {} Hz — a {} Hz stream is resampled by the phone",
                                    ai.output_sample_rate, f.sample_rate
                                ),
                            );
                            ui.end_row();
                        }
                    }

                    ui.label("Volume:");
                    let mut pct = self.gain * 100.0;
                    if ui
                        .add(
                            egui::Slider::new(&mut pct, 0.0..=200.0)
                                .suffix("%")
                                .fixed_decimals(0),
                        )
                        .changed()
                    {
                        self.gain = pct / 100.0;
                        if let Some(s) = &self.session {
                            s.set_gain(self.gain);
                        }
                    }
                    ui.end_row();
                });

            ui.small(
                "Latency note: a higher sample rate / bit depth enlarges the phone's jitter buffer, \
                 which adds latency. 48 kHz · 16-bit is the lowest-latency option; 32-bit 96/192 kHz \
                 buffers more for smoothness.",
            );

            ui.separator();

            ui.horizontal(|ui| {
                let running = self.running();
                let paused = self.paused;
                let start_label = if running && paused {
                    "▶  Resume"
                } else {
                    "▶  Start"
                };
                if ui
                    .add_enabled(!running || paused, egui::Button::new(start_label))
                    .clicked()
                {
                    if !running {
                        self.start();
                    } else {
                        self.resume();
                    }
                }
                if ui
                    .add_enabled(running && !paused, egui::Button::new("■  Stop"))
                    .clicked()
                {
                    self.pause();
                }
                if running && !paused {
                    ui.spinner();
                }
                if running && paused {
                    ui.label("paused");
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("USB driver (Windows):");
                if ui.button("Install ASLC driver").clicked() {
                    self.install_driver();
                }
                if ui.button("Restore file transfer (MTP)").clicked() {
                    self.restore_mtp();
                }
            });

            ui.separator();
            ui.label(format!("Status: {}", self.status));
            if let Some(f) = self.negotiated {
                ui.label(format!("Format: {}", f.display_label()));
            }
            if self.kbps > 0.0 {
                ui.label(format!("Throughput: {:.0} kbit/s", self.kbps));
            }
            if let Some((cap, ring, ring_cap, dev, underruns)) = self.latency {
                let total = cap + 10 + ring as u32 + dev as u32;
                ui.label(format!(
                    "Latency ≈ {total} ms   (PC {} ms · phone {} + {} ms)",
                    cap + 10,
                    ring,
                    dev
                ));
                ui.small(format!("phone buffer {ring_cap} ms · underruns {underruns}"));
            }

            ui.separator();
            ui.label("Log:");
            egui::ScrollArea::vertical()
                .max_height(150.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.small(line);
                    }
                });
        });

        // Keep repainting while visible so session stats update; when hidden this stops (the tray
        // thread handles restore instead).
        if self.session.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }
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

/// Build a simple 32x32 tray icon (a green disc) without shipping an asset file.
fn make_icon() -> tray_icon::Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let c = 15.5f32;
    let r = 13.5f32;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            if (dx * dx + dy * dy).sqrt() <= r {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = 0x2e;
                rgba[i + 1] = 0xc4;
                rgba[i + 2] = 0x6b;
                rgba[i + 3] = 255;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("valid icon")
}
