//! Concrete PCM format: exactly how samples are laid out on the wire.
//! Mirrors the Android `usb/PcmFormat.kt` value object (the transport/output-free part).

/// Wire PCM-encoding id. `1` = signed little-endian, interleaved integer PCM.
/// (Matches `AslcPayload.ENCODING_PCM` on Android.)
pub const ENCODING_PCM: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub channels: u8,
    pub encoding: u8,
    pub flags: u32,
}

impl PcmFormat {
    pub fn new(sample_rate: u32, bit_depth: u8, channels: u8) -> Self {
        Self {
            sample_rate,
            bit_depth,
            channels,
            encoding: ENCODING_PCM,
            flags: 0,
        }
    }

    /// Bytes per sample for supported integer PCM widths (16/24/32). 24-bit is 3 (packed, not in-4).
    pub fn bytes_per_sample(self) -> usize {
        (self.bit_depth as usize) / 8
    }

    /// Bytes per interleaved multichannel frame (one sample per channel).
    pub fn bytes_per_frame(self) -> usize {
        self.bytes_per_sample() * self.channels as usize
    }

    pub fn is_supported_bit_depth(self) -> bool {
        matches!(self.bit_depth, 16 | 24 | 32)
    }

    /// Human label for UI/logs, e.g. "24-bit - 96.000 kHz - Stereo" (ASCII to stay log-safe).
    pub fn display_label(&self) -> String {
        let khz = format!(
            "{}.{:03} kHz",
            self.sample_rate / 1000,
            self.sample_rate % 1000
        );
        let ch = match self.channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{}ch", n),
        };
        format!("{}-bit - {} - {}", self.bit_depth, khz, ch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_frame_sizes() {
        assert_eq!(PcmFormat::new(48000, 16, 2).bytes_per_frame(), 4);
        assert_eq!(PcmFormat::new(48000, 24, 2).bytes_per_frame(), 6); // 24-bit packed = 3B
        assert_eq!(PcmFormat::new(48000, 32, 2).bytes_per_frame(), 8);
        assert_eq!(PcmFormat::new(48000, 32, 1).bytes_per_frame(), 4); // mono
    }

    #[test]
    fn unsupported_depth() {
        assert!(!PcmFormat::new(48000, 8, 2).is_supported_bit_depth());
        assert!(PcmFormat::new(48000, 24, 2).is_supported_bit_depth());
    }

    #[test]
    fn display_label_formats_rate() {
        assert_eq!(
            PcmFormat::new(96000, 24, 2).display_label(),
            "24-bit - 96.000 kHz - Stereo"
        );
        assert_eq!(
            PcmFormat::new(44100, 16, 1).display_label(),
            "16-bit - 44.100 kHz - Mono"
        );
    }
}
