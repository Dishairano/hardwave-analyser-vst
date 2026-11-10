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
    /// Appended fields: the binary layout puts them at the end, so older
    /// consumers still parse the prefix; JSON consumers ignore extras.
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

    /// Serialize the packet to binary format.
    ///
    /// The layout is bincode 1's default, which is what receivers of these
    /// packets read: fields in declaration order, integers and floats
    /// little-endian at their full width, a `bool` as one byte, and a
    /// `Vec<f32>` as its length (u64) followed by its values. Fields added
    /// later go at the end, so a receiver that knows fewer fields reads the
    /// prefix it knows. Written out here so the plug-in does not depend on
    /// the bincode crate, which is no longer maintained (RUSTSEC-2025-0141).
    pub fn to_bytes(&self) -> Vec<u8> {
        let floats = self.left_bins.len()
            + self.right_bins.len()
            + self.left_wave.len()
            + self.right_wave.len();
        let mut out = Vec::with_capacity(70 + 4 * floats);
        out.push(self.packet_type);
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        put_floats(&mut out, &self.left_bins);
        put_floats(&mut out, &self.right_bins);
        for value in [
            self.left_peak,
            self.right_peak,
            self.left_rms,
            self.right_rms,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        put_floats(&mut out, &self.left_wave);
        put_floats(&mut out, &self.right_wave);
        for value in [
            self.left_true_peak,
            self.right_true_peak,
            self.lufs_momentary,
            self.lufs_short_term,
            self.lufs_integrated,
            self.loudness_range,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.push(u8::from(self.loudness_reset));
        out.extend_from_slice(&self.loudness_epoch.to_le_bytes());
        out
    }

    /// Read a packet written by [`AudioPacket::to_bytes`]. Only tests need
    /// it: the plug-in sends packets and never reads them.
    #[cfg(test)]
    pub(crate) fn from_bytes(data: &[u8]) -> Option<AudioPacket> {
        let mut r = Reader(data);
        let packet = AudioPacket {
            packet_type: r.u8()?,
            sample_rate: r.u32()?,
            timestamp_ms: r.u64()?,
            left_bins: r.floats()?,
            right_bins: r.floats()?,
            left_peak: r.f32()?,
            right_peak: r.f32()?,
            left_rms: r.f32()?,
            right_rms: r.f32()?,
            left_wave: r.floats()?,
            right_wave: r.floats()?,
            left_true_peak: r.f32()?,
            right_true_peak: r.f32()?,
            lufs_momentary: r.f32()?,
            lufs_short_term: r.f32()?,
            lufs_integrated: r.f32()?,
            loudness_range: r.f32()?,
            loudness_reset: r.u8()? != 0,
            loudness_epoch: r.u32()?,
        };
        Some(packet)
    }
}

fn put_floats(out: &mut Vec<u8>, values: &[f32]) {
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// Reads the layout [`AudioPacket::to_bytes`] writes, front to back, the
/// way a receiver does. Trailing bytes it does not ask for are left alone.
#[cfg(test)]
pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

#[cfg(test)]
impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
    pub(crate) fn f32(&mut self) -> Option<f32> {
        self.take().map(f32::from_le_bytes)
    }
    pub(crate) fn floats(&mut self) -> Option<Vec<f32>> {
        let len = usize::try_from(self.u64()?).ok()?;
        if len > self.0.len() / 4 {
            return None;
        }
        (0..len).map(|_| self.f32()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_bytes(data: &[u8]) -> Option<AudioPacket> {
        AudioPacket::from_bytes(data)
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
    /// they read the fields they know from the front and leave the rest.
    #[test]
    fn test_backward_compat_trailing_bytes() {
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
        let mut old = Reader(&bytes);
        assert_eq!(old.u8(), Some(PACKET_TYPE_FFT));
        assert_eq!(old.u32(), Some(48000));
        assert_eq!(old.u64(), Some(7));
        assert_eq!(old.floats().map(|v| v.len()), Some(NUM_BINS));
        assert_eq!(old.floats().map(|v| v.len()), Some(NUM_BINS));
        for _ in 0..4 {
            assert!(old.f32().is_some());
        }
        assert_eq!(old.floats().map(|v| v.len()), Some(WAVE_SIZE));
        assert_eq!(old.floats().map(|v| v.len()), Some(WAVE_SIZE));
        // What an old consumer does not read is the appended fields.
        assert_eq!(old.0.len(), 4 * 6 + 1 + 4);
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

    /// The layout, byte for byte. These bytes are what bincode 1.3 wrote
    /// for this packet: the hand-written encoder was checked against
    /// bincode itself on Linux, Windows and macOS before bincode was
    /// removed, so a receiver sees no difference.
    #[test]
    fn test_bytes_are_the_bincode_layout() {
        let packet = AudioPacket::new_fft(
            48000,
            9,
            vec![-1.0, -2.0],
            vec![-3.0],
            -3.0,
            -3.0,
            0.5,
            0.5,
            vec![0.25],
            vec![],
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
        });
        let expected = concat!(
            "00",               // packet_type
            "80bb0000",         // sample_rate 48000
            "0900000000000000", // timestamp_ms 9
            "0200000000000000",
            "000080bf",
            "000000c0", // left_bins
            "0100000000000000",
            "000040c0", // right_bins
            "000040c0",
            "000040c0", // peaks
            "0000003f",
            "0000003f", // rms
            "0100000000000000",
            "0000803e",         // left_wave
            "0000000000000000", // right_wave
            "000080bf",
            "000080bf", // true peaks
            "000068c1",
            "000074c1",
            "000080c1", // momentary, short-term, integrated
            "0000f040", // loudness_range
            "01",       // loudness_reset
            "03000000", // loudness_epoch
        );
        let hex: String = packet
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, expected);
        let decoded = from_bytes(&packet.to_bytes()).unwrap();
        assert_eq!(decoded.to_bytes(), packet.to_bytes());
    }

    /// A truncated packet is refused, not read past its end.
    #[test]
    fn test_short_packets_are_refused() {
        let bytes = loudness_packet().to_bytes();
        for len in [0, 1, 12, 20, bytes.len() - 1] {
            assert!(from_bytes(&bytes[..len]).is_none(), "{len} bytes");
        }
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
    /// the binary layout (a prefix) and JSON (unknown keys ignored).
    #[test]
    fn test_loudness_fields_are_appended() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
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
        let bytes = packet.to_bytes();
        let mut old = Reader(&bytes);
        assert_eq!(old.u8(), Some(PACKET_TYPE_FFT));
        assert_eq!(old.u32(), Some(48000));
        assert_eq!(old.u64(), Some(9));
        let bins = old.floats().unwrap().len() + old.floats().unwrap().len();
        assert_eq!(bins, 2 * NUM_BINS);
        assert_eq!((old.f32(), old.f32()), (Some(-3.0), Some(-3.0)));
        assert_eq!((old.f32(), old.f32()), (Some(0.5), Some(0.5)));
        let wave = old.floats().unwrap().len() + old.floats().unwrap().len();
        assert_eq!(wave, 2 * WAVE_SIZE);
        assert_eq!((old.f32(), old.f32()), (Some(-1.0), Some(-1.0)));
        // The loudness fields follow, and nothing after them.
        assert_eq!(old.0.len(), 4 * 4 + 1 + 4);

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
