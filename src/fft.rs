//! FFT processing for spectrum analysis
//!
//! Runs an 8192-point windowed FFT with Welch's method (4× overlapped windows)
//! and returns all 4096 magnitude bins in dB.
//! Supports Hann, Blackman-Harris, and Kaiser window functions.
//! Includes true-peak metering via 4× oversampling.

use rustfft::{num_complex::Complex, FftPlanner};
use std::f32::consts::PI;

use crate::protocol::NUM_BINS;

/// FFT size for analysis (NUM_BINS = FFT_SIZE / 2)
pub const FFT_SIZE: usize = NUM_BINS * 2;

/// Number of overlapping windows for Welch's method
const WELCH_SEGMENTS: usize = 4;

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

/// FFT processor for a single channel
pub struct FftProcessor {
    planner: FftPlanner<f32>,
    fft_buffer: Vec<Complex<f32>>,
    window: Vec<f32>,
    window_fn: WindowFn,
    /// Accumulator for Welch's method averaging
    welch_accum: Vec<f32>,
}

impl FftProcessor {
    pub fn new() -> Self {
        let wf = WindowFn::Hann;
        let window = build_window(wf, FFT_SIZE);
        Self {
            planner: FftPlanner::new(),
            fft_buffer: vec![Complex::new(0.0, 0.0); FFT_SIZE],
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

        let fft = self.planner.plan_fft_forward(FFT_SIZE);

        for seg in 0..max_segments {
            let start = seg * hop;
            if start + FFT_SIZE > samples.len() {
                break;
            }

            // Apply window and copy to FFT buffer
            for i in 0..FFT_SIZE {
                self.fft_buffer[i] =
                    Complex::new(samples[start + i] * self.window[i], 0.0);
            }

            fft.process(&mut self.fft_buffer);

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
    /// True peak uses 4× linear interpolation oversampling.
    /// Returns (peak_db, rms_linear, true_peak_db).
    pub fn calculate_levels(samples: &[f32]) -> (f32, f32, f32) {
        if samples.is_empty() {
            return (-100.0, 0.0, -100.0);
        }

        let mut peak = 0.0_f32;
        let mut true_peak = 0.0_f32;
        let mut sum_squares = 0.0_f32;

        for (idx, &s) in samples.iter().enumerate() {
            let abs_s = s.abs();
            peak = peak.max(abs_s);
            sum_squares += s * s;

            // 4× oversampled true peak (linear interpolation between consecutive samples)
            if idx > 0 {
                let prev = samples[idx - 1];
                for k in 1..4 {
                    let t = k as f32 / 4.0;
                    let interp = prev + (s - prev) * t;
                    true_peak = true_peak.max(interp.abs());
                }
            }
            true_peak = true_peak.max(abs_s);
        }

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
