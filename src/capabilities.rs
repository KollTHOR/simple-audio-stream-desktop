//! Advertised PCM capability set + the pure narrowing rule.
//! Mirrors Android `usb/PcmCapabilities.kt` and the pure half of `UsbPcmProber.narrow`.

/// A set of PCM capabilities the node can offer. Lists are extensible so future rates/depths/
/// channels/encodings do not change the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmCapabilities {
    pub sample_rates: Vec<u32>,
    pub bit_depths: Vec<u8>,
    pub channels: Vec<u8>,
    pub encodings: Vec<u8>,
    /// Largest single PCM_DATA payload (bytes) the receiver accepts.
    pub max_frame_bytes: u32,
    /// Receiver's preferred sink buffer size (bytes) for smooth playout.
    pub recommended_buffer_bytes: u32,
    pub capability_flags: u32,
}

impl PcmCapabilities {
    pub fn supports(&self, f: crate::format::PcmFormat) -> bool {
        self.sample_rates.contains(&f.sample_rate)
            && self.bit_depths.contains(&f.bit_depth)
            && self.channels.contains(&f.channels)
            && self.encodings.contains(&f.encoding)
    }

    /// The ceiling capability set (pre runtime probe) — matches Android `fullMatrix()`.
    pub fn full_matrix() -> Self {
        Self {
            sample_rates: vec![44100, 48000, 88200, 96000, 176400, 192000],
            bit_depths: vec![16, 24, 32],
            channels: vec![1, 2],
            encodings: vec![crate::format::ENCODING_PCM],
            max_frame_bytes: 16384,
            recommended_buffer_bytes: 460800,
            capability_flags: 0,
        }
    }

    /// Pure: drop any rate/depth/channel that has no renderable combination under `probe`.
    /// Mirrors `UsbPcmProber.narrow`. On the Android device `probe` is `getMinBufferSize`;
    /// here the desktop uses it to trim to formats it can actually capture/convert.
    pub fn narrow<F: Fn(crate::format::PcmFormat) -> bool>(&self, probe: F) -> PcmCapabilities {
        let mut ok_cells = std::collections::HashSet::new();
        let mut ok_channels = std::collections::HashSet::new();
        for &rate in &self.sample_rates {
            for &depth in &self.bit_depths {
                for &ch in &self.channels {
                    if probe(crate::format::PcmFormat::new(rate, depth, ch)) {
                        ok_cells.insert((rate, depth));
                        ok_channels.insert(ch);
                    }
                }
            }
        }
        PcmCapabilities {
            sample_rates: self
                .sample_rates
                .iter()
                .filter(|&&r| self.bit_depths.iter().any(|&d| ok_cells.contains(&(r, d))))
                .copied()
                .collect(),
            bit_depths: self
                .bit_depths
                .iter()
                .filter(|&&d| {
                    self.sample_rates
                        .iter()
                        .any(|&r| ok_cells.contains(&(r, d)))
                })
                .copied()
                .collect(),
            channels: self
                .channels
                .iter()
                .filter(|&&c| ok_channels.contains(&c))
                .copied()
                .collect(),
            encodings: self.encodings.clone(),
            max_frame_bytes: self.max_frame_bytes,
            recommended_buffer_bytes: self.recommended_buffer_bytes,
            capability_flags: self.capability_flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::PcmFormat;

    #[test]
    fn supports_checks_membership() {
        let caps = PcmCapabilities::full_matrix();
        assert!(caps.supports(PcmFormat::new(96000, 24, 2)));
        assert!(!caps.supports(PcmFormat::new(22050, 24, 2)));
        assert!(!caps.supports(PcmFormat::new(48000, 8, 2)));
    }

    #[test]
    fn narrows_to_probeable_subset() {
        let caps = PcmCapabilities::full_matrix();
        // Only 16-bit stereo at 48k/96k passes.
        let narrowed = caps.narrow(|f| {
            f.bit_depth == 16
                && f.channels == 2
                && (f.sample_rate == 48000 || f.sample_rate == 96000)
        });
        assert_eq!(narrowed.sample_rates, vec![48000, 96000]);
        assert_eq!(narrowed.bit_depths, vec![16]);
        assert_eq!(narrowed.channels, vec![2]);
    }

    #[test]
    fn empty_when_nothing_probeable() {
        let narrowed = PcmCapabilities::full_matrix().narrow(|_| false);
        assert!(narrowed.sample_rates.is_empty());
        assert!(narrowed.bit_depths.is_empty());
        assert!(narrowed.channels.is_empty());
    }
}
