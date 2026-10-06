//! ASLC virtual-audio user-mode reader (development probe).
//!
//! Opens the driver's capture control device (`\\.\AslcAudio`), reads the negotiated stream
//! format, pulls rendered PCM, and writes it to a WAV file. This is the first proof that PCM
//! reaches user mode from the virtual render endpoint; the real ASLC companion will replace the
//! "write a WAV" sink with the ASLC protocol/transport, behind the same primitive.
//!
//! Usage: aslc_audio_probe [seconds] [output.wav]

#[cfg(windows)]
fn main() {
    if let Err(e) = run() {
        eprintln!("aslc_audio_probe: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("aslc_audio_probe is Windows-only");
    std::process::exit(1);
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateFileA(
            name: *const u8,
            access: u32,
            share: u32,
            sec: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        pub fn DeviceIoControl(
            h: Handle,
            code: u32,
            inbuf: *mut c_void,
            insize: u32,
            outbuf: *mut c_void,
            outsize: u32,
            bytes_returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        pub fn CloseHandle(h: Handle) -> i32;
    }

    pub const GENERIC_READ: u32 = 0x8000_0000;
    pub const FILE_SHARE_READ: u32 = 0x1;
    pub const FILE_SHARE_WRITE: u32 = 0x2;
    pub const OPEN_EXISTING: u32 = 3;
    // CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, 0x900/0x901, METHOD_BUFFERED, FILE_READ_DATA)
    pub const IOCTL_GET_FORMAT: u32 = 0x0022_6400;
    pub const IOCTL_GET_PCM: u32 = 0x0022_6404;
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
struct Format {
    sample_rate: u32,
    avg_bytes_per_sec: u32,
    channels: u16,
    bits_per_sample: u16,
    block_align: u16,
    valid: u16,
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use std::ffi::c_void;
    use win::*;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds: u64 = args
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let out_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "aslc_capture.wav".to_string());

    let dev_path = b"\\\\.\\AslcAudio\0";
    let h = unsafe {
        CreateFileA(
            dev_path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() || h as isize == -1 {
        return Err(format!(
            "could not open \\\\.\\AslcAudio: {}",
            std::io::Error::last_os_error()
        ));
    }
    let _guard = HandleGuard(h);

    // Negotiated format.
    let mut fmt_bytes = [0u8; 16];
    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_GET_FORMAT,
            std::ptr::null_mut(),
            0,
            fmt_bytes.as_mut_ptr() as *mut c_void,
            16,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || ret < 16 {
        return Err(format!(
            "GET_FORMAT failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let fmt = Format {
        sample_rate: u32::from_le_bytes(fmt_bytes[0..4].try_into().unwrap()),
        avg_bytes_per_sec: u32::from_le_bytes(fmt_bytes[4..8].try_into().unwrap()),
        channels: u16::from_le_bytes(fmt_bytes[8..10].try_into().unwrap()),
        bits_per_sample: u16::from_le_bytes(fmt_bytes[10..12].try_into().unwrap()),
        block_align: u16::from_le_bytes(fmt_bytes[12..14].try_into().unwrap()),
        valid: u16::from_le_bytes(fmt_bytes[14..16].try_into().unwrap()),
    };
    println!(
        "ASLC stream format: {} Hz, {} ch, {}-bit, {} B/s (valid={})",
        fmt.sample_rate, fmt.channels, fmt.bits_per_sample, fmt.avg_bytes_per_sec, fmt.valid
    );

    // Capture PCM for `seconds`.
    let mut pcm: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 65536];
    let request = buf.len() as u32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut empty_polls = 0u64;
    while std::time::Instant::now() < deadline {
        let mut got: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_GET_PCM,
                &request as *const u32 as *mut c_void,
                4,
                buf.as_mut_ptr() as *mut c_void,
                request,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && got > 0 {
            pcm.extend_from_slice(&buf[..got as usize]);
        } else {
            empty_polls += 1;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    // Peak level (16-bit samples only) for a quick "is this real audio" check.
    let peak = if fmt.bits_per_sample == 16 {
        pcm.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
            .map(|s| s.abs())
            .max()
            .unwrap_or(0)
    } else {
        -1
    };

    write_wav(&out_path, &fmt, &pcm)?;
    println!(
        "captured {} bytes ({:.2}s) -> {} | empty polls={} | peak={}/32767",
        pcm.len(),
        pcm.len() as f64 / fmt.avg_bytes_per_sec.max(1) as f64,
        out_path,
        empty_polls,
        peak
    );
    Ok(())
}

#[cfg(windows)]
struct HandleGuard(win::Handle);
#[cfg(windows)]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            win::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn write_wav(path: &str, fmt: &Format, pcm: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut f = std::fs::File::create(path).map_err(|e| format!("create {path}: {e}"))?;
    let bits = fmt.bits_per_sample.max(8);
    let channels = fmt.channels.max(1);
    let sr = fmt.sample_rate.max(8000);
    let block_align = (channels * bits / 8) as u16;
    let byte_rate = sr * block_align as u32;
    let data_len = pcm.len() as u32;
    let riff_len = 36 + data_len;

    f.write_all(b"RIFF").unwrap();
    f.write_all(&riff_len.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&sr.to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&bits.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    f.write_all(pcm).unwrap();
    Ok(())
}
