//! FFT processing for spectrum analysis
//!
//! Runs a 4096-point windowed FFT and returns all 2048 magnitude bins in dB.
//! Frequency-to-display mapping and smoothing happen on the JS side.

use rustfft::{num_complex::Complex, FftPlanner};
use std::f32::consts::PI;

use crate::protocol::NUM_BINS;

/// FFT size for analysis (NUM_BINS = FFT_SIZE / 2)
pub const FFT_SIZE: usize = NUM_BINS * 2;

/// FFT processor for a single channel
pub struct FftProcessor {
    planner: FftPlanner<f32>,
    fft_buffer: Vec<Complex<f32>>,
    window: Vec<f32>,
}

impl FftProcessor {
    pub fn new() -> Self {
        // Pre-compute Hann window
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / (FFT_SIZE - 1) as f32).cos()))
            .collect();

        Self {
            planner: FftPlanner::new(),
            fft_buffer: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            window,
        }
    }

    /// Process audio samples and return NUM_BINS raw magnitude values in dB.
    ///
    /// Bin `i` corresponds to frequency `i * sample_rate / FFT_SIZE` Hz.
    /// Bins above ~20 kHz are included but will be ignored by the JS renderer.
    pub fn process(&mut self, samples: &[f32], _sample_rate: f32) -> Vec<f32> {
        if samples.len() < FFT_SIZE {
            return vec![-100.0; NUM_BINS];
        }

        // Apply Hann window and copy to FFT buffer
        for i in 0..FFT_SIZE {
            self.fft_buffer[i] = Complex::new(samples[i] * self.window[i], 0.0);
        }

        // In-place forward FFT
        let fft = self.planner.plan_fft_forward(FFT_SIZE);
        fft.process(&mut self.fft_buffer);

        // Convert bins to dB.
        // Hann window coherent gain = 0.5, so the correct amplitude scale is:
        //   2 / (FFT_SIZE * coherent_gain) = 4 / FFT_SIZE
        // Without this correction a 0 dBFS sine reads −6 dB.
        let scale = 4.0 / FFT_SIZE as f32;
        (0..NUM_BINS)
            .map(|i| {
                let mag = self.fft_buffer[i].norm() * scale;
                let db = 20.0 * (mag + 1e-10).log10();
                db.clamp(-100.0, 0.0)
            })
            .collect()
    }

    /// Calculate peak and RMS levels from samples.
    /// Returns (peak_db, rms_linear).
    pub fn calculate_levels(samples: &[f32]) -> (f32, f32) {
        if samples.is_empty() {
            return (-100.0, 0.0);
        }

        let mut peak = 0.0_f32;
        let mut sum_squares = 0.0_f32;

        for &s in samples {
            peak = peak.max(s.abs());
            sum_squares += s * s;
        }

        let rms = (sum_squares / samples.len() as f32).sqrt();
        let peak_db = (20.0 * (peak + 1e-10).log10()).clamp(-100.0, 0.0);

        (peak_db, rms)
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
        let samples = vec![0.0f32; FFT_SIZE];
        let bins = processor.process(&samples, sample_rate);
        assert_eq!(bins.len(), NUM_BINS);
    }

    #[test]
    fn test_fft_sine_peak() {
        let mut processor = FftProcessor::new();
        let sample_rate = 48000.0;
        let freq = 1000.0;
        let samples: Vec<f32> = (0..FFT_SIZE)
            .map(|i| (2.0 * PI * freq * i as f32 / sample_rate).sin())
            .collect();

        let bins = processor.process(&samples, sample_rate);

        // Expected bin for 1kHz at 48kHz/4096: bin ≈ 85
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
        let (peak_db, rms) = FftProcessor::calculate_levels(&samples);
        assert!((peak_db - (-6.02)).abs() < 0.1);
        assert!((rms - 0.5).abs() < 0.01);
    }
}
