//! FFT processing for spectrum analysis
//!
//! Converts time-domain audio samples to frequency bands using
//! logarithmic frequency scaling (20Hz - 20kHz in 64 bands).

use rustfft::{num_complex::Complex, FftPlanner};
use std::f32::consts::PI;

use crate::protocol::NUM_BANDS;

/// FFT size for analysis
pub const FFT_SIZE: usize = 4096;

/// Minimum frequency (Hz)
const MIN_FREQ: f32 = 20.0;

/// Maximum frequency (Hz)
const MAX_FREQ: f32 = 20000.0;

/// FFT processor for a single channel
pub struct FftProcessor {
    planner: FftPlanner<f32>,
    fft_buffer: Vec<Complex<f32>>,
    window: Vec<f32>,
    magnitude_buffer: Vec<f32>,
    band_frequencies: Vec<f32>,
}

impl FftProcessor {
    /// Create a new FFT processor
    pub fn new() -> Self {
        let planner = FftPlanner::new();

        // Pre-compute Hann window
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / (FFT_SIZE - 1) as f32).cos()))
            .collect();

        // Pre-compute band center frequencies (logarithmic scale)
        let band_frequencies: Vec<f32> = (0..NUM_BANDS)
            .map(|i| {
                let log_min = MIN_FREQ.log10();
                let log_max = MAX_FREQ.log10();
                let log_freq = log_min + (i as f32 / NUM_BANDS as f32) * (log_max - log_min);
                10.0_f32.powf(log_freq)
            })
            .collect();

        Self {
            planner,
            fft_buffer: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            window,
            magnitude_buffer: vec![0.0; FFT_SIZE / 2],
            band_frequencies,
        }
    }

    /// Process audio samples and return frequency bands in dB
    ///
    /// # Arguments
    /// * `samples` - Audio samples (should be FFT_SIZE samples)
    /// * `sample_rate` - Current sample rate
    ///
    /// # Returns
    /// Array of 64 dB values (-100 to 0)
    pub fn process(&mut self, samples: &[f32], sample_rate: f32) -> [f32; NUM_BANDS] {
        let mut bands = [-100.0_f32; NUM_BANDS];

        if samples.len() < FFT_SIZE {
            return bands;
        }

        // Apply window and copy to FFT buffer
        for i in 0..FFT_SIZE {
            self.fft_buffer[i] = Complex::new(samples[i] * self.window[i], 0.0);
        }

        // Perform FFT
        let fft = self.planner.plan_fft_forward(FFT_SIZE);
        fft.process(&mut self.fft_buffer);

        // Calculate magnitudes
        for i in 0..FFT_SIZE / 2 {
            let magnitude = self.fft_buffer[i].norm();
            self.magnitude_buffer[i] = magnitude;
        }

        // Map to logarithmic frequency bands
        let bin_width = sample_rate / FFT_SIZE as f32;

        for (band_idx, &center_freq) in self.band_frequencies.iter().enumerate() {
            // Calculate frequency range for this band (1/3 octave)
            let low_freq = center_freq / 2.0_f32.powf(1.0 / 6.0);
            let high_freq = center_freq * 2.0_f32.powf(1.0 / 6.0);

            // Convert to bin indices
            let low_bin = (low_freq / bin_width).floor() as usize;
            let high_bin = (high_freq / bin_width).ceil() as usize;

            // Clamp to valid range
            let low_bin = low_bin.max(1).min(FFT_SIZE / 2 - 1);
            let high_bin = high_bin.max(low_bin + 1).min(FFT_SIZE / 2);

            // Average magnitudes in this band
            let mut sum = 0.0;
            let mut count = 0;

            for bin in low_bin..high_bin {
                sum += self.magnitude_buffer[bin];
                count += 1;
            }

            if count > 0 {
                let avg_magnitude = sum / count as f32;

                // Convert to dB (reference = 1.0)
                // Normalize by FFT size to get proper amplitude
                let normalized = avg_magnitude * 2.0 / FFT_SIZE as f32;
                let db = 20.0 * (normalized + 1e-10).log10();

                // Clamp to valid range
                bands[band_idx] = db.clamp(-100.0, 0.0);
            }
        }

        bands
    }

    /// Calculate peak and RMS levels from samples
    ///
    /// # Returns
    /// (peak_db, rms_linear)
    pub fn calculate_levels(samples: &[f32]) -> (f32, f32) {
        if samples.is_empty() {
            return (-100.0, 0.0);
        }

        let mut peak = 0.0_f32;
        let mut sum_squares = 0.0_f32;

        for &sample in samples {
            let abs_sample = sample.abs();
            peak = peak.max(abs_sample);
            sum_squares += sample * sample;
        }

        let rms = (sum_squares / samples.len() as f32).sqrt();
        let peak_db = 20.0 * (peak + 1e-10).log10();

        (peak_db.clamp(-100.0, 0.0), rms)
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
    fn test_fft_processor() {
        let mut processor = FftProcessor::new();

        // Generate 1kHz sine wave at 48kHz sample rate
        let sample_rate = 48000.0;
        let freq = 1000.0;
        let samples: Vec<f32> = (0..FFT_SIZE)
            .map(|i| (2.0 * PI * freq * i as f32 / sample_rate).sin())
            .collect();

        let bands = processor.process(&samples, sample_rate);

        // Find the peak band (should be around 1kHz)
        let peak_band = bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap()
            .0;

        // 1kHz should fall around band 32-35 (middle of log scale from 20Hz to 20kHz)
        assert!(peak_band >= 25 && peak_band <= 40, "Peak at wrong band: {}", peak_band);
    }

    #[test]
    fn test_calculate_levels() {
        // Test with a known signal
        let samples: Vec<f32> = vec![0.5, -0.5, 0.5, -0.5];
        let (peak_db, rms) = FftProcessor::calculate_levels(&samples);

        assert!((peak_db - (-6.02)).abs() < 0.1); // -6dB for 0.5 amplitude
        assert!((rms - 0.5).abs() < 0.01);
    }
}
