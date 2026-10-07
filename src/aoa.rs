//! Android Open Accessory (AOA) host transport — the real USB byte pipe for ASLC (spec §1, M1).
//!
//! Orientation: the **desktop is the USB host**, the **phone is the accessory**. The desktop talks to
//! the phone with raw USB control + bulk transfers via `nusb` (WinUSB on Windows, native usbfs on
//! Linux). Once the AOA handshake flips the phone into accessory mode it exposes two bulk endpoints
//! that form the ordered byte pipe ASLC frames ride.
//!
//! This module owns ONLY the transport/handshake. ASLC framing/negotiation lives in
//! `frame`/`payload`/`receiver` and runs on top of the pipe, so the code proven over TCP at M0 is
//! unchanged here — nothing above the [`Transport`] trait changed (spec §13).
//!
//! Everything compiles on Linux; the actual open/handshake/bulk paths need a physically-cabled phone
//! and run at Windows test time. Pure helpers (handshake byte shape, PID classification) are tested
//! on any host.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use futures_lite::future::block_on;
use nusb::transfer::{Control, ControlType, Direction, EndpointType, Queue, Recipient, RequestBuffer};
use nusb::{Device, DeviceInfo, Interface};

use crate::frame::AslcError;
use crate::transport::{Halves, Transport};

/// Max bytes requested per bulk-IN packet read. Bulk packets larger than this are carried over.
const MAX_BULK_READ: usize = 16 * 1024;

// ---- AOA protocol constants (shared contract for the host side) ----------------------------

/// Vendor control request codes (host -> device, recipient Device), per the AOA spec.
pub const AOA_GET_PROTOCOL: u8 = 51;
pub const AOA_SEND_STRING: u8 = 52;
pub const AOA_START_ACCESSORY: u8 = 53;

/// wIndex selectors for AOA_SEND_STRING (order is fixed by the protocol). VERIFIED against the
/// Android kernel's slot mapping: 3=VERSION, 4=URI, 5=SERIAL. (Putting anything in URI makes the
/// framework launch the SystemUI accessory-URI chooser instead of dispatching a clean attach.)
pub const AOA_STRING_MANUFACTURER: u16 = 0;
pub const AOA_STRING_MODEL: u16 = 1;
pub const AOA_STRING_DESCRIPTION: u16 = 2;
pub const AOA_STRING_VERSION: u16 = 3;
pub const AOA_STRING_URI: u16 = 4;
pub const AOA_STRING_SERIAL: u16 = 5;

/// Google's USB VID. In accessory mode the phone re-enumerates with this VID and a 0x2Dxx product id.
pub const GOOGLE_VID: u16 = 0x18d1;
pub const ACCESSORY_PID_AOA1: u16 = 0x2d00;
pub const ACCESSORY_PID_AOA1_AUDIO: u16 = 0x2d01;
pub const ACCESSORY_PID_AOA2: u16 = 0x2d04;
pub const ACCESSORY_PID_AOA2_AUDIO: u16 = 0x2d05;

/// AOA identify strings this host presents. Android's `accessory_filter.xml` is permissive today, so
/// these are informational; pin them there to auto-launch only for ASLC nodes. URI MUST stay empty:
/// a non-empty URI makes Android launch the SystemUI accessory-URI chooser instead of a clean attach.
pub const AOA_MANUFACTURER: &str = "Simple Audio Stream";
pub const AOA_MODEL: &str = "ASLC Node";
pub const AOA_DESCRIPTION: &str = "ASLC PCM input source";
pub const AOA_VERSION: &str = "1.0";
pub const AOA_URI: &str = "";
pub const AOA_SERIAL: &str = "1";

pub fn is_accessory_pid(pid: u16) -> bool {
    matches!(
        pid,
        ACCESSORY_PID_AOA1
            | ACCESSORY_PID_AOA1_AUDIO
            | ACCESSORY_PID_AOA2
            | ACCESSORY_PID_AOA2_AUDIO
    )
}

pub fn accessory_pid_name(pid: u16) -> Option<&'static str> {
    Some(match pid {
        ACCESSORY_PID_AOA1 => "AOA1",
        ACCESSORY_PID_AOA1_AUDIO => "AOA1+audio",
        ACCESSORY_PID_AOA2 => "AOA2",
        ACCESSORY_PID_AOA2_AUDIO => "AOA2+audio",
        _ => return None,
    })
}

pub fn is_accessory_device(dev: &DeviceInfo) -> bool {
    dev.vendor_id() == GOOGLE_VID && is_accessory_pid(dev.product_id())
}

// ---- Read-only capability probe (no START, no mode switch) ----------------------------------

/// One interface's probe outcome: claimable? bulk pair? (claim errors carry the OS reason).
#[derive(Debug, Clone)]
pub struct IfProbe {
    pub number: u8,
    pub claimable: bool,
    pub bulk_in: Option<u8>,
    pub bulk_out: Option<u8>,
    pub note: String,
}

/// Capability report for one connected device. `aoa_protocol: Some(v)` means the firmware's
/// accessory gadget RESPONDED to GET_PROTOCOL (AOA v1/v2 capable); None = no reply/STALL or no
/// claimable interface to ask through.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub vid: u16,
    pub pid: u16,
    pub aoa_protocol: Option<u16>,
    pub proto_error: String,
    pub interfaces: Vec<IfProbe>,
}

impl ProbeReport {
    pub fn summary(&self) -> String {
        match self.aoa_protocol {
            Some(v) => format!("AOA capable: protocol v{v}"),
            None if self.proto_error.is_empty() => {
                "AOA probe inconclusive (no claimable interface to ask through)".into()
            }
            None => format!("AOA not answering: {}", self.proto_error),
        }
    }
}

/// Pick a probe target: any 18d1 device, preferring one already in accessory mode.
pub fn find_probe_target() -> Option<(u16, u16)> {
    let mut first = None;
    for d in nusb::list_devices().ok()? {
        if d.vendor_id() != GOOGLE_VID {
            continue;
        }
        if is_accessory_pid(d.product_id()) {
            return Some((d.vendor_id(), d.product_id()));
        }
        first.get_or_insert((d.vendor_id(), d.product_id()));
    }
    first
}

/// True if the device advertises any of `needles` in its Windows registry `CompatibleIDs`.
/// Vendor-agnostic: the strings checked are Microsoft/Android *class* IDs, never device IDs.
#[cfg(windows)]
fn registry_compatible_has(vid: u16, pid: u16, needles: &[&str]) -> bool {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(usb) = hklm.open_subkey("SYSTEM\\CurrentControlSet\\Enum\\USB") else {
        return false;
    };
    let prefix = format!("VID_{vid:04X}&PID_{pid:04X}");
    for key_name in usb.enum_keys().flatten() {
        if !key_name.starts_with(&prefix) {
            continue;
        }
        let Ok(key) = usb.open_subkey(&key_name) else {
            continue;
        };
        for inst in key.enum_keys().flatten() {
            let Ok(sub) = key.open_subkey(&inst) else {
                continue;
            };
            if let Ok(ids) = sub.get_value::<Vec<String>, _>("CompatibleIDs") {
                if ids.iter().any(|s| needles.iter().any(|n| s.contains(n))) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(not(windows))]
fn registry_compatible_has(_vid: u16, _pid: u16, _needles: &[&str]) -> bool {
    false
}

/// Android's MTP function (`USB\MS_COMP_MTP`) — present on essentially every phone without USB
/// debugging. This is the interface we bind WinUSB to so the host can send the AOA handshake.
fn registry_is_mtp(vid: u16, pid: u16) -> bool {
    registry_compatible_has(vid, pid, &["MS_COMP_MTP"])
}

/// Android's ADB function (`USB\Class_ff&SubClass_42&Prot_01` / `USB\MS_COMP_ADB`) — present when
/// USB debugging is on. Windows already binds WinUSB to it, so it needs no driver from us.
fn registry_is_adb(vid: u16, pid: u16) -> bool {
    registry_compatible_has(vid, pid, &["SubClass_42", "MS_COMP_ADB"])
}

/// A candidate ASLC receiver: a phone/DAP that can run the AOA accessory.
#[derive(Debug, Clone)]
pub struct ReceiverDevice {
    pub vid: u16,
    pub pid: u16,
    /// Already enumerates in AOA accessory mode (ready to attach, no handshake needed).
    pub accessory: bool,
    /// USB descriptor name (may be a board/empty string on some devices).
    pub name: String,
    pub serial: String,
}

impl ReceiverDevice {
    /// Default label when no nickname is set.
    pub fn default_label(&self) -> String {
        if self.name.is_empty() {
            format!("Receiver {:04x}:{:04x}", self.vid, self.pid)
        } else {
            self.name.clone()
        }
    }
}

/// Read the device's real name from the Windows registry. Explorer/File Manager shows the MTP
/// (WPD) device's name, which the registry stores as `DeviceDesc`/`Mfg` on the
/// `...\Enum\USB\VID_xxxx&PID_yyyy&MI_00` key. That key persists after the phone switches to
/// accessory mode, so this also names an already-attached accessory.
#[cfg(windows)]
fn registry_device_name(vid: u16, pid: Option<u16>) -> Option<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let usb = hklm
        .open_subkey("SYSTEM\\CurrentControlSet\\Enum\\USB")
        .ok()?;
    let exact = pid.map(|p| format!("VID_{vid:04X}&PID_{p:04X}&MI_00"));
    for key_name in usb.enum_keys().flatten() {
        let matches = match &exact {
            Some(want) => &key_name == want,
            None => key_name.starts_with(&format!("VID_{vid:04X}&PID_")) && key_name.ends_with("&MI_00"),
        };
        if !matches {
            continue;
        }
        let Ok(key) = usb.open_subkey(&key_name) else {
            continue;
        };
        for inst in key.enum_keys().flatten() {
            let Ok(sub) = key.open_subkey(&inst) else {
                continue;
            };
            let desc: String = sub.get_value("DeviceDesc").unwrap_or_default();
            let desc = desc.rsplit(';').next().unwrap_or(&desc).trim().to_string();
            if desc.is_empty()
                || desc.starts_with('@')
                || desc.contains("ASLC")
                || desc.eq_ignore_ascii_case("USB Composite Device")
            {
                continue;
            }
            let mfg: String = sub.get_value("Mfg").unwrap_or_default();
            let mfg = mfg.rsplit(';').next().unwrap_or(&mfg).trim().to_string();
            return Some(if mfg.is_empty() {
                desc
            } else {
                format!("{mfg} {desc}")
            });
        }
    }
    None
}

/// Enumerate AOA-capable receivers: devices already in accessory mode, plus Google-VID (`0x18d1`)
/// Android/DAP devices that a handshake can switch into accessory mode.
pub fn list_receiver_devices() -> Vec<ReceiverDevice> {
    let mut out = Vec::new();
    if let Ok(devs) = nusb::list_devices() {
        for d in devs {
            let vid = d.vendor_id();
            let pid = d.product_id();
            let accessory = is_accessory_device(&d);
            // Capability-based (no vendor/device IDs): AOA accessory, Android MTP function, or
            // Android ADB function — all Microsoft/Android *class* IDs the phone itself publishes.
            let candidate = accessory || registry_is_mtp(vid, pid) || registry_is_adb(vid, pid);
            if !candidate {
                continue;
            }
            let m = d.manufacturer_string().unwrap_or("").trim();
            let p = d.product_string().unwrap_or("").trim();
            // Some devices (notably Qualcomm reference boards) put the serial in the product
            // string as "..._SN:xxxx"; split it out so the name is cleaner.
            let (p_clean, embedded_sn) = match p.split_once("_SN:") {
                Some((before, after)) => (before.trim(), after.trim()),
                None => (p, ""),
            };
            let descriptor_name = format!("{m} {p_clean}").trim().to_string();
            // Prefer the device's real MTP/WPD name (what Explorer/File Manager shows); the USB
            // descriptor strings are often just a board id. The VID-wide fallback is only for
            // accessory-mode devices, whose MTP PID differs from the one we saw at handshake time.
            #[cfg(windows)]
            let name = registry_device_name(vid, Some(pid))
                .or_else(|| {
                    if accessory {
                        registry_device_name(vid, None)
                    } else {
                        None
                    }
                })
                .unwrap_or(descriptor_name);
            #[cfg(not(windows))]
            let name = descriptor_name;
            let serial = if !embedded_sn.is_empty() {
                embedded_sn.to_string()
            } else {
                d.serial_number().unwrap_or("").trim().to_string()
            };
            out.push(ReceiverDevice {
                vid,
                pid,
                accessory,
                name,
                serial,
            });
        }
    }
    out.sort_by(|a, b| {
        b.accessory
            .cmp(&a.accessory)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Non-destructive capability probe: open the device, try claiming each low interface number,
/// and — through the first claimable interface with a bulk pair — send `GET_PROTOCOL` only.
/// Does NOT send the strings/START, so the device stays in its current USB mode.
pub fn probe_device(vid: u16, pid: u16) -> Result<ProbeReport, AslcError> {
    let dev = AoaTransport::find_device(|d| d.vendor_id() == vid && d.product_id() == pid)
        .ok_or_else(|| io_other(format!("device {vid:04x}:{pid:04x} not present")))?;
    let device = dev.open().map_err(|e| {
        io_other(format!(
            "open failed: {e} (on Windows the device needs a WinUSB binding)"
        ))
    })?;
    let mut report = ProbeReport {
        vid,
        pid,
        aoa_protocol: None,
        proto_error: String::new(),
        interfaces: Vec::new(),
    };
    let mut ask_through: Option<Interface> = None;
    for n in 0..=5u8 {
        match device.detach_and_claim_interface(n) {
            Ok(iface) => {
                let mut b_in = None;
                let mut b_out = None;
                for alt in iface.descriptors() {
                    for ep in alt.endpoints() {
                        if ep.transfer_type() == EndpointType::Bulk {
                            match ep.direction() {
                                Direction::In => b_in = Some(ep.address()),
                                Direction::Out => b_out = Some(ep.address()),
                            }
                        }
                    }
                }
                report.interfaces.push(IfProbe {
                    number: n,
                    claimable: true,
                    bulk_in: b_in,
                    bulk_out: b_out,
                    note: String::new(),
                });
                if b_in.is_some() && b_out.is_some() && ask_through.is_none() {
                    ask_through = Some(iface); // keep it claimed while we ask
                }
            }
            Err(e) => report.interfaces.push(IfProbe {
                number: n,
                claimable: false,
                bulk_in: None,
                bulk_out: None,
                note: e.to_string(),
            }),
        }
    }
    if let Some(iface) = &ask_through {
        let mut buf = [0u8; 2];
        match iface.control_in_blocking(
            vendor_dev(AOA_GET_PROTOCOL, 0, 0),
            &mut buf,
            Duration::from_millis(500),
        ) {
            Ok(got) if got >= 2 => report.aoa_protocol = Some(u16::from_le_bytes(buf)),
            Ok(_) => report.proto_error = "short reply".into(),
            Err(e) => report.proto_error = e.to_string(),
        }
    }
    Ok(report)
}

fn vendor_dev(request: u8, value: u16, index: u16) -> Control {
    Control {
        control_type: ControlType::Vendor,
        recipient: Recipient::Device,
        request,
        value,
        index,
    }
}

fn io_other(msg: impl Into<String>) -> AslcError {
    AslcError::Io(std::io::Error::other(msg.into()))
}

// ---- Interface / endpoint discovery --------------------------------------------------------

/// Claim the AOA data interface of an accessory-mode device. The Android accessory gadget always
/// exposes it as **interface 0**; anything else (e.g. the ADB interface, which also has bulk
/// endpoints!) would be the wrong pipe, so we claim exactly 0 and fail with guidance otherwise.
fn discover_bulk_interface(dev: &Device) -> Result<(Interface, u8, u8), AslcError> {
    let iface = dev.detach_and_claim_interface(0).map_err(|e| {
        io_other(format!(
            "accessory data interface 0 not claimable ({e}) - on Windows install the WinUSB \
             driver: pnputil /add-driver platform\\winusb\\aslc_aoa.inf /install"
        ))
    })?;
    let mut in_ep = None;
    let mut out_ep = None;
    for alt in iface.descriptors() {
        for ep in alt.endpoints() {
            if ep.transfer_type() == EndpointType::Bulk {
                match ep.direction() {
                    Direction::In => in_ep = Some(ep.address()),
                    Direction::Out => out_ep = Some(ep.address()),
                }
            }
        }
    }
    match (in_ep, out_ep) {
        (Some(i), Some(o)) => Ok((iface, i, o)),
        _ => Err(io_other(
            "interface 0 has no bulk IN/OUT pair - not an AOA data interface",
        )),
    }
}

// ---- Byte-pipe adapters over bulk endpoints ------------------------------------------------

/// A `Transport` over an AOA accessory. Opening performs the full handshake if needed.
pub struct AoaTransport {
    /// The phone's VID:PID as it enumerates *before* handshake (to select + switch it). Ignored when
    /// the device is already in accessory mode.
    pre_handshake: Option<(u16, u16)>,
    /// Holds the claimed accessory interface so it stays alive while the halves use it (Interface is
    /// Arc-backed; the halves clone it, this keeps a reference so the claim survives `open` returning).
    keepalive: Option<Interface>,
    /// Cancellation for the bulk-IN reader, so a terminated session releases the interface.
    reader_cancel: Option<Arc<AtomicBool>>,
    reader_waker: Option<Arc<Mutex<Option<Waker>>>>,
}

impl AoaTransport {
    /// Target the device at `vid:pid` (pre-handshake) and switch it into accessory mode. Use
    /// `aslc_node list-usb` to discover that VID:PID on the target machine.
    pub fn new(vid: u16, pid: u16) -> Self {
        Self {
            pre_handshake: Some((vid, pid)),
            keepalive: None,
            reader_cancel: None,
            reader_waker: None,
        }
    }

    /// Attach to a device already in accessory mode (no handshake).
    pub fn from_attached_accessory() -> Self {
        Self {
            pre_handshake: None,
            keepalive: None,
            reader_cancel: None,
            reader_waker: None,
        }
    }

    /// Cancel the bulk-IN reader, unblocking it so it exits and drops its interface reference.
    pub fn cancel_reader(&self) {
        if let Some(c) = &self.reader_cancel {
            c.store(true, Ordering::SeqCst);
        }
        if let Some(w) = &self.reader_waker {
            if let Some(wk) = w.lock().ok().and_then(|mut g| g.take()) {
                wk.wake();
            }
        }
    }

    fn find_device(pred: impl Fn(&DeviceInfo) -> bool) -> Option<DeviceInfo> {
        nusb::list_devices().ok()?.find(pred)
    }

    /// Runs the AOA control handshake on a claimed interface of the pre-handshake device. Returns the
    /// device's AOA protocol version.
    fn do_handshake(iface: &Interface) -> Result<u16, AslcError> {
        let mut proto = [0u8; 2];
        let got = iface
            .control_in_blocking(
                vendor_dev(AOA_GET_PROTOCOL, 0, 0),
                &mut proto,
                Duration::from_millis(500),
            )
            .map_err(|e| io_other(format!("GET_PROTOCOL failed: {e}")))?;
        if got < 2 {
            return Err(io_other("GET_PROTOCOL returned short reply"));
        }
        let version = u16::from_le_bytes(proto);
        if version < 1 {
            return Err(io_other(format!("device AOA protocol too old: {version}")));
        }
        for (idx, s) in [
            (AOA_STRING_MANUFACTURER, AOA_MANUFACTURER),
            (AOA_STRING_MODEL, AOA_MODEL),
            (AOA_STRING_DESCRIPTION, AOA_DESCRIPTION),
            (AOA_STRING_VERSION, AOA_VERSION),
            (AOA_STRING_URI, AOA_URI),
            (AOA_STRING_SERIAL, AOA_SERIAL),
        ] {
            iface
                .control_out_blocking(
                    vendor_dev(AOA_SEND_STRING, 0, idx),
                    s.as_bytes(),
                    Duration::from_millis(500),
                )
                .map_err(|e| io_other(format!("SEND_STRING({idx}) failed: {e}")))?;
        }
        iface
            .control_out_blocking(
                vendor_dev(AOA_START_ACCESSORY, 0, 0),
                &[],
                Duration::from_millis(500),
            )
            .map_err(|e| io_other(format!("START_ACCESSORY failed: {e}")))?;
        Ok(version)
    }

    /// Waits for the phone to re-enumerate as an accessory after START, then opens + claims its bulk
    /// interface. Windows PnP can take several seconds to settle a re-enumerated composite, so this
    /// polls patiently and reports progress.
    fn wait_for_accessory(&mut self) -> Result<(Interface, u8, u8), AslcError> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut announced = false;
        loop {
            if let Some(dev) = Self::find_device(is_accessory_device) {
                if !announced {
                    eprintln!(
                        "accessory present ({}), opening...",
                        accessory_pid_name(dev.product_id()).unwrap_or("?")
                    );
                    announced = true;
                }
                match dev.open() {
                    Ok(device) => {
                        return discover_bulk_interface(&device);
                    }
                    // open() can transiently fail mid re-enumeration; keep polling until the deadline.
                    Err(_) if Instant::now() < deadline => {}
                    Err(e) => return Err(io_other(format!("accessory open failed: {e}"))),
                }
            }
            if Instant::now() >= deadline {
                return Err(io_other("timed out waiting for accessory mode"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Transport for AoaTransport {
    fn open(&mut self) -> Result<Halves, AslcError> {
        // Case B (pre-handshake): switch the target phone into accessory mode first.
        if let Some((vid, pid)) = self.pre_handshake {
            let dev = Self::find_device(|d| d.vendor_id() == vid && d.product_id() == pid)
                .ok_or_else(|| {
                    io_other(format!("target device {vid:04x}:{pid:04x} not present"))
                })?;
            let device = dev
                .open()
                .map_err(|e| io_other(format!("open pre-handshake device: {e}")))?;
            // Claim the FIRST interface we can: on Windows only WinUSB-bound interfaces are
            // claimable, so on an MTP+ADB composite this lands on the ADB interface (bound to
            // WINUSB by the system driver), giving us a handle for the device-directed AOA
            // control transfers. Interface 0 (MTP) is owned by WPD and will fail, which is fine.
            let mut claimed: Option<Interface> = None;
            let mut claim_errs = Vec::new();
            for n in 0..=6u8 {
                match device.detach_and_claim_interface(n) {
                    Ok(i) => {
                        claimed = Some(i);
                        break;
                    }
                    Err(e) => claim_errs.push(format!("if{n}: {e}")),
                }
            }
            let iface = claimed.ok_or_else(|| {
                io_other(format!(
                    "no WinUSB-bound interface to claim -> {}",
                    claim_errs.join(" | ")
                ))
            })?;
            let version = Self::do_handshake(&iface)?;
            eprintln!(
                "AOA handshake accepted (device protocol v{version}); waiting for accessory..."
            );
            drop(iface);
            drop(device); // accessory re-enumerates under a new device node
        }

        // Case A/B: attach to the accessory and hand back bulk halves.
        let (iface, in_ep, out_ep) = self.wait_for_accessory()?;
        self.keepalive = Some(iface.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
        self.reader_cancel = Some(cancel.clone());
        self.reader_waker = Some(waker.clone());
        let queue = iface.bulk_in_queue(in_ep);
        Ok((
            Box::new(QueueReader {
                queue,
                carry: Vec::new(),
                cancel,
                waker,
                in_flight: false,
                done: false,
            }),
            Box::new(BulkWriter { iface, out_ep }),
        ))
    }

    fn close(&mut self) {
        self.keepalive = None;
    }
}

/// Reader half: a cancelable queue-based bulk-IN reader. Reads a packet at a time, buffering any
/// over-read (`carry`) so `io::Read` partial-fill semantics hold even though delivery is
/// packet-granular. Cancellation (see [`AoaTransport::cancel_reader`]) unblocks a pending read so a
/// terminated session lets go of the interface.
struct QueueReader {
    queue: Queue<RequestBuffer>,
    carry: Vec<u8>,
    cancel: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
    in_flight: bool,
    done: bool,
}

impl Read for QueueReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.carry.is_empty() {
            let n = buf.len().min(self.carry.len());
            buf[..n].copy_from_slice(&self.carry[..n]);
            self.carry.drain(..n);
            return Ok(n);
        }
        if buf.is_empty() || self.done {
            return Ok(0);
        }
        if !self.in_flight {
            self.queue.submit(RequestBuffer::new(MAX_BULK_READ));
            self.in_flight = true;
        }
        let cancel = self.cancel.clone();
        let waker_slot = self.waker.clone();
        let mut outcome: Option<Result<Vec<u8>, String>> = None;
        block_on(futures_lite::future::poll_fn(|cx| {
            if cancel.load(Ordering::Relaxed) {
                return Poll::Ready(());
            }
            *waker_slot.lock().unwrap() = Some(cx.waker().clone());
            match self.queue.poll_next(cx) {
                Poll::Ready(c) => {
                    outcome = Some(match c.status {
                        Ok(()) => Ok(c.data),
                        Err(e) => Err(e.to_string()),
                    });
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            }
        }));
        self.in_flight = false;
        match outcome {
            Some(Ok(data)) => {
                let n = buf.len().min(data.len());
                buf[..n].copy_from_slice(&data[..n]);
                if data.len() > n {
                    self.carry.extend_from_slice(&data[n..]);
                }
                Ok(n)
            }
            Some(Err(e)) => Err(std::io::Error::other(e)),
            None => {
                self.queue.cancel_all();
                self.done = true;
                Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "reader cancelled",
                ))
            }
        }
    }
}

/// Writer half: one bulk-OUT transfer per write; `write_all` loops until the whole slice is sent.
struct BulkWriter {
    iface: Interface,
    out_ep: u8,
}

impl Write for BulkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let data = buf.to_vec();
        block_on(self.iface.bulk_out(self.out_ep, data))
            .into_result()
            .map_err(io_to_std)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn io_to_std(e: nusb::transfer::TransferError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_accessory_pids() {
        assert!(is_accessory_pid(ACCESSORY_PID_AOA1));
        assert!(is_accessory_pid(ACCESSORY_PID_AOA2_AUDIO));
        assert!(!is_accessory_pid(0x4e20));
        assert_eq!(accessory_pid_name(ACCESSORY_PID_AOA1), Some("AOA1"));
        assert_eq!(accessory_pid_name(0x1234), None);
    }

    #[test]
    fn vendor_control_shape() {
        let c = vendor_dev(AOA_GET_PROTOCOL, 0x1122, 3);
        assert_eq!(c.control_type, ControlType::Vendor);
        assert_eq!(c.recipient, Recipient::Device);
        assert_eq!(c.request, 51);
        assert_eq!(c.value, 0x1122);
        assert_eq!(c.index, 3);
    }

    #[test]
    fn handshake_strings_are_valid_ascii_and_bounded() {
        for s in [
            AOA_MANUFACTURER,
            AOA_MODEL,
            AOA_DESCRIPTION,
            AOA_SERIAL,
            AOA_VERSION,
        ] {
            assert!(
                s.is_ascii() && !s.is_empty() && s.len() < 64,
                "bad AOA string {s:?}"
            );
        }
        // Non-empty URI hijacks attach dispatch into the SystemUI URI chooser (verified on real hardware).
        assert!(AOA_URI.is_empty(), "URI slot must stay empty");
        // Kernel string-slot mapping must not drift: 3=version, 4=uri, 5=serial.
        assert_eq!(
            (AOA_STRING_VERSION, AOA_STRING_URI, AOA_STRING_SERIAL),
            (3, 4, 5)
        );
    }

    #[test]
    fn list_devices_is_safe_without_a_phone() {
        // Enumeration must not panic with no accessory present; any device count is fine.
        let _ = nusb::list_devices().map(|it| it.count()).unwrap_or(0);
    }
}
