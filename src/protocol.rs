//! Binary protocol for audio data transmission

use serde::{Deserialize, Serialize};

/// Number of raw FFT magnitude bins (FFT_SIZE / 2)
pub const NUM_BINS: usize = 4096;

/// Minimum analysis frequency in Hz (sub-bass visibility)
pub const MIN_FREQ_HZ: f32 = 5.0;

/// Maximum analysis frequency defaults to Nyquist (sample_rate / 2).
/// This constant is used as a sentinel; the actual max is computed at runtime.
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
}

fn default_true_peak() -> f32 {
    -100.0
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
        }
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
        let old: OldPacket = bincode::deserialize(&bytes).expect("old struct must tolerate appended fields");
        assert_eq!(old.packet_type, PACKET_TYPE_FFT);
        assert_eq!(old.timestamp_ms, 7);
        assert_eq!(old.right_wave.len(), WAVE_SIZE);
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
        assert!(bytes.len() < 42_000, "Packet too large: {} bytes", bytes.len());
        println!("Packet size: {} bytes", bytes.len());
    }
}
