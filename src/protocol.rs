//! Binary protocol for audio data transmission

use serde::{Deserialize, Serialize};

/// Number of raw FFT magnitude bins (FFT_SIZE / 2)
pub const NUM_BINS: usize = 2048;

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
        }
    }

    /// Create a heartbeat packet
    pub fn new_heartbeat(sample_rate: u32, timestamp_ms: u64) -> Self {
        Self {
            packet_type: PACKET_TYPE_HEARTBEAT,
            sample_rate,
            timestamp_ms,
            left_bins: vec![0.0; NUM_BINS],
            right_bins: vec![0.0; NUM_BINS],
            left_peak: -100.0,
            right_peak: -100.0,
            left_rms: 0.0,
            right_rms: 0.0,
        }
    }

    /// Serialize the packet to binary format
    pub fn to_bytes(&self) -> Vec<u8> {
        bincode::serialize(self).expect("Failed to serialize packet")
    }

    /// Deserialize a packet from binary format
    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );

        let bytes = packet.to_bytes();
        let decoded = AudioPacket::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.packet_type, PACKET_TYPE_FFT);
        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.timestamp_ms, 12345);
        assert_eq!(decoded.left_bins.len(), NUM_BINS);
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
        );

        let bytes = packet.to_bytes();
        // 2048 bins × 2 channels × 4 bytes + overhead ≈ 16.4 KB
        assert!(bytes.len() < 20_000, "Packet too large: {} bytes", bytes.len());
        println!("Packet size: {} bytes", bytes.len());
    }
}
