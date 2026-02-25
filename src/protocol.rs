//! Binary protocol for audio data transmission
//!
//! Packet format is designed to be compact (~536 bytes per packet)
//! and efficiently deserializable on the receiving end.

use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

/// Number of frequency bands in FFT analysis
pub const NUM_BANDS: usize = 64;

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

    /// Left channel FFT bands in dB (-100 to 0)
    #[serde(with = "BigArray")]
    pub left_bands: [f32; NUM_BANDS],

    /// Right channel FFT bands in dB (-100 to 0)
    #[serde(with = "BigArray")]
    pub right_bands: [f32; NUM_BANDS],

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
        left_bands: [f32; NUM_BANDS],
        right_bands: [f32; NUM_BANDS],
        left_peak: f32,
        right_peak: f32,
        left_rms: f32,
        right_rms: f32,
    ) -> Self {
        Self {
            packet_type: PACKET_TYPE_FFT,
            sample_rate,
            timestamp_ms,
            left_bands,
            right_bands,
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
            left_bands: [0.0; NUM_BANDS],
            right_bands: [0.0; NUM_BANDS],
            left_peak: -100.0,
            right_peak: -100.0,
            left_rms: 0.0,
            right_rms: 0.0,
        }
    }

    /// Serialize the packet to binary format.
    ///
    /// Returns an empty `Vec` on the (practically impossible) serialization
    /// error rather than panicking, which would crash the DAW host process.
    pub fn to_bytes(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap_or_default()
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
            [-60.0; NUM_BANDS],
            [-60.0; NUM_BANDS],
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
    }

    #[test]
    fn test_packet_size() {
        let packet = AudioPacket::new_fft(
            48000,
            0,
            [-60.0; NUM_BANDS],
            [-60.0; NUM_BANDS],
            -3.0,
            -3.0,
            0.5,
            0.5,
        );

        let bytes = packet.to_bytes();
        // Should be around 536 bytes
        assert!(bytes.len() < 600, "Packet too large: {} bytes", bytes.len());
    }
}
