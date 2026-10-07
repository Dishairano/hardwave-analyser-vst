//! Binary protocol for audio data transmission

use serde::{Deserialize, Serialize};

use crate::loudness::LoudnessReading;

/// Number of raw FFT magnitude bins (FFT_SIZE / 2)
pub const NUM_BINS: usize = 4096;

/// Minimum analysis frequency in Hz (sub-bass visibility)
#[allow(dead_code)]
pub const MIN_FREQ_HZ: f32 = 5.0;

/// Maximum analysis frequency defaults to Nyquist (sample_rate / 2).
/// This constant is used as a sentinel; the actual max is computed at runtime.
#[allow(dead_code)]
pub const MAX_FREQ_HZ_DEFAULT: f32 = 0.0;

/// Number of time-domain samples sent per packet for the oscilloscope
pub const WAVE_SIZE: usize = 512;

/// Packet type identifiers
pub const PACKET_TYPE_FFT: u8 = 0;
pub const PACKET_TYPE_HEARTBEAT: u8 = 1;

/// Audio packet sent from VST to Hardwave Suite
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPacket {
    /// Packet type (0=FFT, 1=Heartbeat)
    pub packet_type: u8,

    /// Sample rate of the audio context
    pub sample_rate: u32,

    /// Timestamp in milliseconds since plugin start
    pub timestamp_ms: u64,

    /// Left channel raw FFT magnitude bins in dB (-100 to 0), length = NUM_BINS
    pub left_bins: Vec<f32>,

    /// Right channel raw FFT magnitude bins in dB (-100 to 0), length = NUM_BINS
    pub right_bins: Vec<f32>,

    /// Left channel peak level in dB
    pub left_peak: f32,

    /// Right channel peak level in dB
    pub right_peak: f32,

    /// Left channel RMS level (linear, 0-1)
    pub left_rms: f32,

    /// Right channel RMS level (linear, 0-1)
    pub right_rms: f32,

    /// Left channel oscilloscope waveform samples, linear amplitude -1..1, length = WAVE_SIZE
    pub left_wave: Vec<f32>,

    /// Right channel oscilloscope waveform samples, linear amplitude -1..1, length = WAVE_SIZE
    pub right_wave: Vec<f32>,

    /// Left channel true peak in dBTP (4× oversampled).
    /// Appended fields: bincode's `deserialize` allows trailing bytes, so
    /// older consumers still parse the prefix; JSON consumers ignore extras.
    #[serde(default = "default_true_peak")]
    pub left_true_peak: f32,

    /// Right channel true peak in dBTP (4× oversampled).
    #[serde(default = "default_true_peak")]
    pub right_true_peak: f32,

    /// Momentary loudness (400 ms window), LUFS, measured on every sample by
    /// the plug-in. -100 means silence or no reading yet.
    #[serde(default = "default_loudness")]
    pub lufs_momentary: f32,

    /// Short-term loudness (3 s window), LUFS. -100 means silence or no
    /// reading yet.
    #[serde(default = "default_loudness")]
    pub lufs_short_term: f32,

    /// Integrated loudness since the last reset (BS.1770-4 gating), LUFS.
    /// -100 means no gating block has passed the gates yet.
    #[serde(default = "default_loudness")]
    pub lufs_integrated: f32,

    /// Loudness range since the last reset (EBU Tech 3342), LU.
    #[serde(default)]
    pub loudness_range: f32,

    /// True on the first packet after the loudness measurement restarted
    /// (plug-in reset, sample-rate change): the window clears its history.
    #[serde(default)]
    pub loudness_reset: bool,

    /// Increments on every loudness reset. A window that missed the packet
    /// carrying `loudness_reset` sees the change here.
    #[serde(default)]
    pub loudness_epoch: u32,
}

fn default_true_peak() -> f32 {
    -100.0
}

fn default_loudness() -> f32 {
    crate::loudness::LOUDNESS_FLOOR
}

impl AudioPacket {
    /// Create a new FFT packet
    pub fn new_fft(
        sample_rate: u32,
        timestamp_ms: u64,
        left_bins: Vec<f32>,
        right_bins: Vec<f32>,
        left_peak: f32,
        right_peak: f32,
        left_rms: f32,
        right_rms: f32,
        left_wave: Vec<f32>,
        right_wave: Vec<f32>,
        left_true_peak: f32,
        right_true_peak: f32,
    ) -> Self {
        Self {
            packet_type: PACKET_TYPE_FFT,
            sample_rate,
            timestamp_ms,
            left_bins,
            right_bins,
            left_peak,
            right_peak,
            left_rms,
            right_rms,
            left_wave,
            right_wave,
            left_true_peak,
            right_true_peak,
            lufs_momentary: default_loudness(),
            lufs_short_term: default_loudness(),
            lufs_integrated: default_loudness(),
            loudness_range: 0.0,
            loudness_reset: false,
            loudness_epoch: 0,
        }
    }

    /// Attach the plug-in's loudness reading.
    pub fn with_loudness(mut self, reading: &LoudnessReading) -> Self {
        self.lufs_momentary = reading.momentary;
        self.lufs_short_term = reading.short_term;
        self.lufs_integrated = reading.integrated;
        self.loudness_range = reading.range;
        self.loudness_reset = reading.reset;
        self.loudness_epoch = reading.epoch;
        self
    }

    /// Create a heartbeat packet. Bins and wave are empty — receivers must
    /// check packet_type before accessing those fields.
    pub fn new_heartbeat(sample_rate: u32, timestamp_ms: u64) -> Self {
        Self {
            packet_type: PACKET_TYPE_HEARTBEAT,
            sample_rate,
            timestamp_ms,
            left_bins: vec![],
            right_bins: vec![],
            left_peak: -100.0,
            right_peak: -100.0,
            left_rms: 0.0,
            right_rms: 0.0,
            left_wave: vec![],
            right_wave: vec![],
            left_true_peak: -100.0,
            right_true_peak: -100.0,
            lufs_momentary: default_loudness(),
            lufs_short_term: default_loudness(),
            lufs_integrated: default_loudness(),
            loudness_range: 0.0,
            loudness_reset: false,
            loudness_epoch: 0,
        }
    }

    /// Serialize the packet to binary format
    pub fn to_bytes(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_bytes(data: &[u8]) -> Result<AudioPacket, bincode::Error> {
        bincode::deserialize(data)
    }

    #[test]
    fn test_packet_roundtrip() {
        let packet = AudioPacket::new_fft(
            48000,
            12345,
            vec![-60.0; NUM_BINS],
            vec![-60.0; NUM_BINS],
            -3.0,
            -3.0,
            0.5,
            0.5,
            vec![0.0; WAVE_SIZE],
            vec![0.0; WAVE_SIZE],
            -1.2,
            -1.2,
        );

        let bytes = packet.to_bytes();
        let decoded = from_bytes(&bytes).unwrap();

        assert_eq!(decoded.packet_type, PACKET_TYPE_FFT);
        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.timestamp_ms, 12345);
        assert_eq!(decoded.left_bins.len(), NUM_BINS);
        assert_eq!(decoded.left_true_peak, -1.2);
    }

    /// Old consumers (pre-true-peak struct) must still parse new packets:
    /// bincode's `deserialize` permits trailing bytes.
    #[test]
    fn test_backward_compat_trailing_bytes() {
        #[derive(serde::Deserialize)]
        struct OldPacket {
            packet_type: u8,
            sample_rate: u32,
            timestamp_ms: u64,
            left_bins: Vec<f32>,
            right_bins: Vec<f32>,
            left_peak: f32,
            right_peak: f32,
            left_rms: f32,
            right_rms: f32,
            left_wave: Vec<f32>,
            right_wave: Vec<f32>,
        }

        let packet = AudioPacket::new_fft(
            48000,
            7,
            vec![-60.0; NUM_BINS],
            vec![-60.0; NUM_BINS],
            -3.0,
            -3.0,
            0.5,
            0.5,
            vec![0.0; WAVE_SIZE],
            vec![0.0; WAVE_SIZE],
            -0.4,
            -0.4,
        );
        let bytes = packet.to_bytes();
        let old: OldPacket =
            bincode::deserialize(&bytes).expect("old struct must tolerate appended fields");
        assert_eq!(old.packet_type, PACKET_TYPE_FFT);
        assert_eq!(old.timestamp_ms, 7);
        assert_eq!(old.right_wave.len(), WAVE_SIZE);
    }

    fn loudness_packet() -> AudioPacket {
        AudioPacket::new_fft(
            48000,
            9,
            vec![-60.0; NUM_BINS],
            vec![-60.0; NUM_BINS],
            -3.0,
            -3.0,
            0.5,
            0.5,
            vec![0.0; WAVE_SIZE],
            vec![0.0; WAVE_SIZE],
            -1.0,
            -1.0,
        )
        .with_loudness(&LoudnessReading {
            momentary: -14.5,
            short_term: -15.25,
            integrated: -16.0,
            range: 7.5,
            reset: true,
            epoch: 3,
        })
    }

    #[test]
    fn test_loudness_fields_roundtrip() {
        let decoded = from_bytes(&loudness_packet().to_bytes()).unwrap();
        assert_eq!(decoded.lufs_momentary, -14.5);
        assert_eq!(decoded.lufs_short_term, -15.25);
        assert_eq!(decoded.lufs_integrated, -16.0);
        assert_eq!(decoded.loudness_range, 7.5);
        assert!(decoded.loudness_reset);
        assert_eq!(decoded.loudness_epoch, 3);
    }

    /// Windows from before the loudness fields still parse the packet, over
    /// bincode (trailing bytes) and JSON (unknown keys ignored).
    #[test]
    fn test_loudness_fields_are_appended() {
        #[derive(serde::Deserialize)]
        struct TruePeakPacket {
            packet_type: u8,
            sample_rate: u32,
            timestamp_ms: u64,
            left_bins: Vec<f32>,
            right_bins: Vec<f32>,
            left_peak: f32,
            right_peak: f32,
            left_rms: f32,
            right_rms: f32,
            left_wave: Vec<f32>,
            right_wave: Vec<f32>,
            left_true_peak: f32,
            right_true_peak: f32,
        }

        let packet = loudness_packet();
        let old: TruePeakPacket = bincode::deserialize(&packet.to_bytes()).unwrap();
        assert_eq!(old.packet_type, PACKET_TYPE_FFT);
        assert_eq!(old.sample_rate, 48000);
        assert_eq!(old.timestamp_ms, 9);
        assert_eq!(old.left_bins.len() + old.right_bins.len(), 2 * NUM_BINS);
        assert_eq!(old.left_wave.len() + old.right_wave.len(), 2 * WAVE_SIZE);
        assert_eq!((old.left_peak, old.right_peak), (-3.0, -3.0));
        assert_eq!((old.left_rms, old.right_rms), (0.5, 0.5));
        assert_eq!((old.left_true_peak, old.right_true_peak), (-1.0, -1.0));

        let json = serde_json::to_string(&packet).unwrap();
        let old: TruePeakPacket = serde_json::from_str(&json).unwrap();
        assert_eq!(old.right_true_peak, -1.0);
        assert!(json.contains("\"lufs_integrated\":-16.0"));

        // A JSON packet without the new keys gets the "no reading" defaults.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for key in [
            "lufs_momentary",
            "lufs_short_term",
            "lufs_integrated",
            "loudness_range",
            "loudness_reset",
            "loudness_epoch",
        ] {
            value.as_object_mut().unwrap().remove(key);
        }
        let decoded: AudioPacket = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.lufs_integrated, -100.0);
        assert_eq!(decoded.loudness_range, 0.0);
        assert!(!decoded.loudness_reset);
    }

    #[test]
    fn test_packet_size() {
        let packet = AudioPacket::new_fft(
            48000,
            0,
            vec![-60.0; NUM_BINS],
            vec![-60.0; NUM_BINS],
            -3.0,
            -3.0,
            0.5,
            0.5,
            vec![0.0; WAVE_SIZE],
            vec![0.0; WAVE_SIZE],
            -1.0,
            -1.0,
        );

        let bytes = packet.to_bytes();
        // 4096 bins × 2 channels × 4 bytes + 512 wave × 2 channels × 4 bytes + overhead ≈ 37 KB
        assert!(
            bytes.len() < 42_000,
            "Packet too large: {} bytes",
            bytes.len()
        );
        println!("Packet size: {} bytes", bytes.len());
    }
}
