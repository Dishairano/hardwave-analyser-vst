//! FFT processing for spectrum analysis
//!
//! Runs an 8192-point windowed FFT with Welch's method (4× overlapped windows)
//! and returns all 4096 magnitude bins in dB.
//! Supports Hann, Blackman-Harris, and Kaiser window functions.
//! Includes true-peak metering via 4× oversampling.

use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::f32::consts::PI;
use std::sync::Arc as StdArc;

use crate::protocol::NUM_BINS;

/// FFT size for analysis (NUM_BINS = FFT_SIZE / 2)
pub const FFT_SIZE: usize = NUM_BINS * 2;

/// Number of overlapping windows for Welch's method
pub const WELCH_SEGMENTS: usize = 4;
/// Minimum sample count for full multi-segment Welch's coverage.
/// `process` degrades to a single segment when fewer samples are buffered.
pub const WELCH_MIN_SAMPLES: usize = FFT_SIZE + (WELCH_SEGMENTS - 1) * (FFT_SIZE / 2);

/// Window function types
#[derive(Clone, Copy, PartialEq)]
pub enum WindowFn {
    Hann = 0,
    BlackmanHarris = 1,
    Kaiser = 2,
}

impl From<i32> for WindowFn {
    fn from(v: i32) -> Self {
        match v {
            1 => WindowFn::BlackmanHarris,
            2 => WindowFn::Kaiser,
            _ => WindowFn::Hann,
        }
    }
}

/// Compute a Hann window
fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / (size - 1) as f32).cos()))
        .collect()
}

/// Compute a Blackman-Harris (4-term) window — excellent sidelobe rejection (-92 dB)
fn blackman_harris_window(size: usize) -> Vec<f32> {
    let a0 = 0.35875;
    let a1 = 0.48829;
    let a2 = 0.14128;
    let a3 = 0.01168;
    (0..size)
        .map(|i| {
            let t = 2.0 * PI * i as f32 / (size - 1) as f32;
            a0 - a1 * t.cos() + a2 * (2.0 * t).cos() - a3 * (3.0 * t).cos()
        })
        .collect()
}

/// Compute a Kaiser window (beta=9, good tradeoff resolution/sidelobe)
fn kaiser_window(size: usize) -> Vec<f32> {
    let beta = 9.0_f32;
    (0..size)
        .map(|i| {
            let t = 2.0 * i as f32 / (size - 1) as f32 - 1.0;
            bessel_i0(beta * (1.0 - t * t).sqrt()) / bessel_i0(beta)
        })
        .collect()
}

/// Zeroth-order modified Bessel function of the first kind (series approximation)
fn bessel_i0(x: f32) -> f32 {
    let mut sum = 1.0_f32;
    let mut term = 1.0_f32;
    let x2 = x * x;
    for k in 1..25 {
        term *= x2 / (4.0 * k as f32 * k as f32);
        sum += term;
        if term < 1e-10 {
            break;
        }
    }
    sum
}

/// Coherent gain for each window function (used for amplitude correction)
fn coherent_gain(wf: WindowFn) -> f32 {
    match wf {
        WindowFn::Hann => 0.5,
        WindowFn::BlackmanHarris => 0.35875,
        WindowFn::Kaiser => 0.4, // approximate for beta=9
    }
}

/// Build a window of the given type
fn build_window(wf: WindowFn, size: usize) -> Vec<f32> {
    match wf {
        WindowFn::Hann => hann_window(size),
        WindowFn::BlackmanHarris => blackman_harris_window(size),
        WindowFn::Kaiser => kaiser_window(size),
    }
}

/// Oversampling factor for true-peak metering.
const TP_PHASES: usize = 4;
/// Taps per polyphase branch (48-tap prototype / 4 phases).
const TP_TAPS: usize = 12;

/// Interpolation filter from ITU-R BS.1770-4 Annex 2, one row per phase.
const TP_REFERENCE: [[f32; TP_TAPS]; TP_PHASES] = [
    [
        0.001_708_984_375,
        0.010_986_328_125,
        -0.019_653_320_312_5,
        0.033_203_125,
        -0.059_448_242_187_5,
        0.137_329_101_562_5,
        0.972_167_968_75,
        -0.102_294_921_875,
        0.047_607_421_875,
        -0.026_611_328_125,
        0.014_892_578_125,
        -0.008_300_781_25,
    ],
    [
        -0.029_174_804_687_5,
        0.029_296_875,
        -0.051_757_812_5,
        0.089_111_328_125,
        -0.166_503_906_25,
        0.465_087_890_625,
        0.779_785_156_25,
        -0.200_317_382_812_5,
        0.101_562_5,
        -0.058_227_539_062_5,
        0.033_081_054_687_5,
        -0.018_920_898_437_5,
    ],
    [
        -0.018_920_898_437_5,
        0.033_081_054_687_5,
        -0.058_227_539_062_5,
        0.101_562_5,
        -0.200_317_382_812_5,
        0.779_785_156_25,
        0.465_087_890_625,
        -0.166_503_906_25,
        0.089_111_328_125,
        -0.051_757_812_5,
        0.029_296_875,
        -0.029_174_804_687_5,
    ],
    [
        -0.008_300_781_25,
        0.014_892_578_125,
        -0.026_611_328_125,
        0.047_607_421_875,
        -0.102_294_921_875,
        0.972_167_968_75,
        0.137_329_101_562_5,
        -0.059_448_242_187_5,
        0.033_203_125,
        -0.019_653_320_312_5,
        0.010_986_328_125,
        0.001_708_984_375,
    ],
];

/// The reference filter with every phase scaled to unity gain at DC. The
/// published coefficients are rounded, which leaves the phase gains between
/// about -0.24 dB and +0.01 dB; without this a constant signal would read
/// slightly above its own sample value.
const TP_FILTER: [[f32; TP_TAPS]; TP_PHASES] = normalise_phases(TP_REFERENCE);

const fn normalise_phases(
    mut filter: [[f32; TP_TAPS]; TP_PHASES],
) -> [[f32; TP_TAPS]; TP_PHASES] {
    let mut p = 0;
    while p < TP_PHASES {
        let mut sum = 0.0;
        let mut k = 0;
        while k < TP_TAPS {
            sum += filter[p][k];
            k += 1;
        }
        let mut k = 0;
        while k < TP_TAPS {
            filter[p][k] /= sum;
            k += 1;
        }
        p += 1;
    }
    filter
}

/// Largest absolute value of the 4× oversampled signal.
///
/// The caller passes the whole analysis window, so the filter history comes
/// from the window itself: an output is only computed where all 12 input
/// samples lie inside the slice. Nothing is carried between calls, and the
/// few intervals at the very edges are covered by the sample peak and by
/// the next, overlapping window.
fn oversampled_peak(samples: &[f32]) -> f32 {
    let mut max = 0.0_f32;
    for history in samples.windows(TP_TAPS) {
        for phase in &TP_FILTER {
            // history[TP_TAPS - 1] is the newest sample, paired with tap 0.
            let y: f32 = phase
                .iter()
                .zip(history.iter().rev())
                .map(|(h, x)| h * x)
                .sum();
            max = max.max(y.abs());
        }
    }
    max
}

/// FFT processor for a single channel
pub struct FftProcessor {
    /// FFT plan, created once — FFT_SIZE never changes at runtime.
    fft: StdArc<dyn Fft<f32>>,
    fft_buffer: Vec<Complex<f32>>,
    /// Preallocated scratch so process() never allocates on the audio thread.
    fft_scratch: Vec<Complex<f32>>,
    window: Vec<f32>,
    window_fn: WindowFn,
    /// Accumulator for Welch's method averaging
    welch_accum: Vec<f32>,
}

impl FftProcessor {
    pub fn new() -> Self {
        let wf = WindowFn::Hann;
        let window = build_window(wf, FFT_SIZE);
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let scratch_len = fft.get_inplace_scratch_len();
        Self {
            fft,
            fft_buffer: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            fft_scratch: vec![Complex::new(0.0, 0.0); scratch_len],
            window,
            window_fn: wf,
            welch_accum: vec![0.0; NUM_BINS],
        }
    }

    /// Update the window function if the parameter changed.
    pub fn set_window_fn(&mut self, wf: WindowFn) {
        if wf != self.window_fn {
            self.window = build_window(wf, FFT_SIZE);
            self.window_fn = wf;
        }
    }

    /// Process audio samples using Welch's method (4× overlapped Hann windows).
    /// Returns NUM_BINS raw magnitude values in dB.
    ///
    /// Requires at least FFT_SIZE * 1.5 samples for full 4-segment overlap.
    /// Falls back to single-window if fewer samples are available.
    pub fn process(&mut self, samples: &[f32], _sample_rate: f32) -> Vec<f32> {
        if samples.len() < FFT_SIZE {
            return vec![-100.0; NUM_BINS];
        }

        let gain = coherent_gain(self.window_fn);
        let scale = 2.0 / (FFT_SIZE as f32 * gain);

        // Determine how many overlapped segments we can fit
        let hop = FFT_SIZE / 2; // 50% overlap
        let max_segments = if samples.len() >= FFT_SIZE + (WELCH_SEGMENTS - 1) * hop {
            WELCH_SEGMENTS
        } else {
            1
        };

        // Clear accumulator
        for v in self.welch_accum.iter_mut() {
            *v = 0.0;
        }

        for seg in 0..max_segments {
            let start = seg * hop;
            if start + FFT_SIZE > samples.len() {
                break;
            }

            // Apply window and copy to FFT buffer
            for i in 0..FFT_SIZE {
                self.fft_buffer[i] = Complex::new(samples[start + i] * self.window[i], 0.0);
            }

            self.fft
                .process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);

            // Accumulate magnitude squared (power spectrum)
            for i in 0..NUM_BINS {
                let mag = self.fft_buffer[i].norm() * scale;
                self.welch_accum[i] += mag * mag;
            }
        }

        // Average and convert to dB
        let inv_segments = 1.0 / max_segments as f32;
        (0..NUM_BINS)
            .map(|i| {
                let rms_mag = (self.welch_accum[i] * inv_segments).sqrt();
                let db = 20.0 * (rms_mag + 1e-10).log10();
                db.clamp(-100.0, 0.0)
            })
            .collect()
    }

    /// Calculate peak, RMS, and true-peak levels from samples.
    /// True peak follows ITU-R BS.1770-4 Annex 2: 4× oversampling through a
    /// 48-tap polyphase FIR, then the maximum absolute value. The sample peak
    /// is included, so the true peak never reads below it.
    /// Returns (peak_db, rms_linear, true_peak_db).
    pub fn calculate_levels(samples: &[f32]) -> (f32, f32, f32) {
        if samples.is_empty() {
            return (-100.0, 0.0, -100.0);
        }

        let mut peak = 0.0_f32;
        let mut sum_squares = 0.0_f32;

        for &s in samples {
            peak = peak.max(s.abs());
            sum_squares += s * s;
        }

        let true_peak = peak.max(oversampled_peak(samples));

        let rms = (sum_squares / samples.len() as f32).sqrt();
        let peak_db = (20.0 * (peak + 1e-10).log10()).clamp(-100.0, 0.0);
        let true_peak_db = (20.0 * (true_peak + 1e-10).log10()).clamp(-100.0, 0.0);

        (peak_db, rms, true_peak_db)
    }
}

impl Default for FftProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fft_processor_bin_count() {
        let mut processor = FftProcessor::new();
        let sample_rate = 48000.0;
        let samples = vec![0.0f32; FFT_SIZE * 2];
        let bins = processor.process(&samples, sample_rate);
        assert_eq!(bins.len(), NUM_BINS);
    }

    #[test]
    fn test_fft_sine_peak() {
        let mut processor = FftProcessor::new();
        let sample_rate = 48000.0;
        let freq = 1000.0;
        let samples: Vec<f32> = (0..FFT_SIZE * 2)
            .map(|i| (2.0 * PI * freq * i as f32 / sample_rate).sin())
            .collect();

        let bins = processor.process(&samples, sample_rate);

        let peak_bin = bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;

        let expected_bin = (freq / (sample_rate / FFT_SIZE as f32)).round() as usize;
        assert!(
            (peak_bin as isize - expected_bin as isize).abs() <= 2,
            "Peak at bin {} expected ~{}",
            peak_bin,
            expected_bin
        );
    }

    #[test]
    fn test_calculate_levels() {
        let samples = vec![0.5f32, -0.5, 0.5, -0.5];
        let (peak_db, rms, true_peak_db) = FftProcessor::calculate_levels(&samples);
        assert!((peak_db - (-6.02)).abs() < 0.1);
        assert!((rms - 0.5).abs() < 0.01);
        assert!(true_peak_db >= peak_db); // true peak >= sample peak
    }

    fn sine(len: usize, cycles_per_sample: f32, phase: f32, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (2.0 * PI * cycles_per_sample * i as f32 + phase).sin())
            .collect()
    }

    #[test]
    fn test_true_peak_finds_inter_sample_peak() {
        // fs/4 with a 45 degree offset: every sample sits at ±0.707 (-3.01 dBFS)
        // while the waveform itself reaches ±1.0 between samples.
        let samples = sine(4096, 0.25, PI / 4.0, 1.0);
        let (peak_db, _, true_peak_db) = FftProcessor::calculate_levels(&samples);
        assert!((peak_db - (-3.01)).abs() < 0.05, "sample peak {peak_db}");
        assert!(true_peak_db.abs() < 0.5, "true peak {true_peak_db}");
    }

    #[test]
    fn test_true_peak_low_frequency_sine_matches_sample_peak() {
        // 997 Hz at 48 kHz, -6 dBFS.
        let samples = sine(48000, 997.0 / 48000.0, 0.0, 0.5);
        let (peak_db, _, true_peak_db) = FftProcessor::calculate_levels(&samples);
        assert!(
            (true_peak_db - peak_db).abs() < 0.1,
            "peak {peak_db} true peak {true_peak_db}"
        );
    }

    #[test]
    fn test_true_peak_dc_and_silence() {
        let (_, _, silence_db) = FftProcessor::calculate_levels(&[0.0; 1024]);
        assert_eq!(silence_db, -100.0);

        let (peak_db, _, dc_db) = FftProcessor::calculate_levels(&[0.5; 1024]);
        assert!((dc_db - peak_db).abs() < 1e-4, "peak {peak_db} dc {dc_db}");

        let (_, _, full_dc_db) = FftProcessor::calculate_levels(&[1.0; 1024]);
        assert_eq!(full_dc_db, 0.0);

        let (_, _, neg_dc_db) = FftProcessor::calculate_levels(&[-0.25; 1024]);
        assert!((neg_dc_db - (-12.04)).abs() < 0.01, "negative dc {neg_dc_db}");
    }

    #[test]
    fn test_true_peak_filter_phases_have_unity_dc_gain() {
        for phase in &TP_FILTER {
            let sum: f32 = phase.iter().sum();
            assert!((sum - 1.0).abs() < 1e-6, "phase gain {sum}");
        }
    }

    #[test]
    fn test_true_peak_short_and_non_finite_input() {
        // Shorter than the filter: falls back to the sample peak.
        let (peak_db, _, tp_db) = FftProcessor::calculate_levels(&[0.1, -0.2, 0.3]);
        assert_eq!(tp_db, peak_db);

        let mut samples = sine(256, 0.01, 0.0, 0.5);
        samples[100] = f32::NAN;
        samples[150] = f32::INFINITY;
        let (_, _, tp_db) = FftProcessor::calculate_levels(&samples);
        assert!(tp_db.is_finite());
    }

    #[test]
    fn test_blackman_harris_window() {
        let w = blackman_harris_window(256);
        assert_eq!(w.len(), 256);
        // Endpoints should be near zero
        assert!(w[0] < 0.01);
        assert!(w[255] < 0.01);
        // Middle should be near 1.0
        assert!(w[128] > 0.9);
    }

    #[test]
    fn test_kaiser_window() {
        let w = kaiser_window(256);
        assert_eq!(w.len(), 256);
        // Middle should be 1.0
        assert!((w[127] - 1.0).abs() < 0.01);
    }
}
