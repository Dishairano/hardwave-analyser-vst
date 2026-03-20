//! Hardwave Analyser - VST3/CLAP plugin for streaming audio to Hardwave Suite
//!
//! This plugin captures audio from the DAW and streams FFT data via WebSocket
//! to the Hardwave Suite desktop application for real-time analysis.
//! When built with the `gui` feature, it also embeds a wry webview that loads
//! the Hardwave Analyser from hardwave.studio inside the DAW plugin window.

mod auth;
#[cfg(feature = "gui")]
mod editor;
mod fft;
mod params;
mod protocol;
mod websocket;

use nih_plug::prelude::*;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use fft::{FftProcessor, FFT_SIZE};
use params::HardwaveAnalyserParams;
use protocol::AudioPacket;
use websocket::WebSocketClient;

/// Main plugin struct
pub struct HardwaveAnalyser {
    params: Arc<HardwaveAnalyserParams>,

    /// WebSocket client for streaming to the desktop app
    ws_client: WebSocketClient,

    /// Shared slot for the latest FFT packet (written by audio thread, read by editor)
    packet_slot: Arc<Mutex<Option<AudioPacket>>>,

    /// Editor instance (created once, reused)
    #[cfg(feature = "gui")]
    editor_instance: Option<editor::HardwaveAnalyserEditor>,

    /// FFT processor for left channel
    fft_left: FftProcessor,

    /// FFT processor for right channel
    fft_right: FftProcessor,

    /// Sample buffer for left channel
    buffer_left: VecDeque<f32>,

    /// Sample buffer for right channel
    buffer_right: VecDeque<f32>,

    /// Current sample rate
    sample_rate: f32,

    /// Samples since last FFT send
    samples_since_send: usize,

    /// Samples between FFT sends
    samples_per_send: usize,

    /// Plugin start time for timestamps
    start_time: Instant,

    /// Last port value (for detecting changes)
    last_port: i32,

    /// Last refresh rate value (for detecting changes)
    last_refresh_rate: i32,
}

impl Default for HardwaveAnalyser {
    fn default() -> Self {
        let packet_slot: Arc<Mutex<Option<AudioPacket>>> = Arc::new(Mutex::new(None));

        Self {
            params: Arc::new(HardwaveAnalyserParams::default()),
            ws_client: WebSocketClient::new(),
            packet_slot: Arc::clone(&packet_slot),
            #[cfg(feature = "gui")]
            editor_instance: {
                Some(editor::HardwaveAnalyserEditor::new(packet_slot))
            },
            fft_left: FftProcessor::new(),
            fft_right: FftProcessor::new(),
            buffer_left: VecDeque::with_capacity(FFT_SIZE),
            buffer_right: VecDeque::with_capacity(FFT_SIZE),
            sample_rate: 48000.0,
            samples_since_send: 0,
            samples_per_send: 800, // 48000 / 60 = 800 samples for 60Hz default
            start_time: Instant::now(),
            last_port: 9847,
            last_refresh_rate: 60,
        }
    }
}

impl Plugin for HardwaveAnalyser {
    const NAME: &'static str = "Hardwave Analyser";
    const VENDOR: &'static str = "Hardwave Studios";
    const URL: &'static str = "https://hardwave.studio";
    const EMAIL: &'static str = "support@hardwave.studio";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        // Stereo
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        // Mono (will be duplicated to stereo for analysis)
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;

    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        #[cfg(feature = "gui")]
        {
            self.editor_instance
                .take()
                .map(|e| Box::new(e) as Box<dyn Editor>)
        }
        #[cfg(not(feature = "gui"))]
        {
            None
        }
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;
        let refresh_rate = self.params.refresh_rate.value() as f32;
        self.samples_per_send = (self.sample_rate / refresh_rate) as usize;
        self.last_refresh_rate = self.params.refresh_rate.value();

        // Clear buffers
        self.buffer_left.clear();
        self.buffer_right.clear();

        // Start WebSocket client (deferred from new() to avoid blocking DAW scans)
        self.ws_client.start();

        // Set initial port
        self.ws_client.set_port(self.params.port.value());
        self.last_port = self.params.port.value();

        true
    }

    fn reset(&mut self) {
        self.buffer_left.clear();
        self.buffer_right.clear();
        self.samples_since_send = 0;
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Check if port changed
        let current_port = self.params.port.value();
        if current_port != self.last_port {
            self.ws_client.set_port(current_port);
            self.last_port = current_port;
        }

        // Check if refresh rate changed
        let current_refresh_rate = self.params.refresh_rate.value();
        if current_refresh_rate != self.last_refresh_rate {
            self.samples_per_send = (self.sample_rate / current_refresh_rate as f32) as usize;
            self.last_refresh_rate = current_refresh_rate;
        }

        // Skip processing if disabled
        if !self.params.enabled.value() {
            return ProcessStatus::Normal;
        }

        let num_channels = buffer.channels();
        let num_samples = buffer.samples();

        // Process each sample
        for sample_idx in 0..num_samples {
            // Get samples (handle mono by duplicating)
            let left = buffer.as_slice()[0][sample_idx];
            let right = if num_channels > 1 {
                buffer.as_slice()[1][sample_idx]
            } else {
                left
            };

            // Add to ring buffers
            self.buffer_left.push_back(left);
            self.buffer_right.push_back(right);

            // Keep buffer at FFT_SIZE
            if self.buffer_left.len() > FFT_SIZE {
                self.buffer_left.pop_front();
                self.buffer_right.pop_front();
            }

            self.samples_since_send += 1;
        }

        // Send FFT data at ~20Hz
        if self.samples_since_send >= self.samples_per_send && self.buffer_left.len() >= FFT_SIZE {
            self.send_fft_data();
            self.samples_since_send = 0;
        }

        // Pass through audio unchanged
        ProcessStatus::Normal
    }
}

impl HardwaveAnalyser {
    /// Write a line to the same debug log as editor.rs
    fn debug_log(msg: &str) {
        use std::io::Write;
        let path = { let mut p = std::env::temp_dir(); p.push("hardwave-debug.log"); p };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            let _ = writeln!(f, "[{}] [lib] {}", now, msg);
        }
    }

    /// Process and send FFT data
    fn send_fft_data(&mut self) {
        // Make VecDeque storage contiguous so we can take a slice.
        let left_slice = self.buffer_left.make_contiguous();
        let left_bins = self.fft_left.process(left_slice, self.sample_rate);
        let (left_peak, left_rms) = FftProcessor::calculate_levels(left_slice);

        let right_slice = self.buffer_right.make_contiguous();
        let right_bins = self.fft_right.process(right_slice, self.sample_rate);
        let (right_peak, right_rms) = FftProcessor::calculate_levels(right_slice);

        // Create and send packet
        let timestamp_ms = self.start_time.elapsed().as_millis() as u64;

        // Log first 3 packets so we know FFT is running
        if timestamp_ms < 3000 || timestamp_ms % 10000 < 100 {
            Self::debug_log(&format!(
                "send_fft_data: ts={}ms sr={} left_peak={:.1} bins={}",
                timestamp_ms, self.sample_rate as u32, left_peak, left_bins.len()
            ));
        }

        // Extract oscilloscope waveform: last WAVE_SIZE samples.
        // buffer_left/right are already contiguous after make_contiguous() above.
        use protocol::WAVE_SIZE;
        let left_slice = self.buffer_left.as_slices().0;
        let left_wave = if left_slice.len() >= WAVE_SIZE {
            left_slice[left_slice.len() - WAVE_SIZE..].to_vec()
        } else {
            vec![0.0_f32; WAVE_SIZE]
        };
        let right_slice = self.buffer_right.as_slices().0;
        let right_wave = if right_slice.len() >= WAVE_SIZE {
            right_slice[right_slice.len() - WAVE_SIZE..].to_vec()
        } else {
            vec![0.0_f32; WAVE_SIZE]
        };

        let packet = AudioPacket::new_fft(
            self.sample_rate as u32,
            timestamp_ms,
            left_bins,
            right_bins,
            left_peak,
            right_peak,
            left_rms,
            right_rms,
            left_wave,
            right_wave,
        );

        // Send to WebSocket (desktop app)
        self.ws_client.send(packet.clone());

        // Write latest packet to shared slot (editor thread takes it when ready)
        *self.packet_slot.lock() = Some(packet);
    }
}

impl ClapPlugin for HardwaveAnalyser {
    const CLAP_ID: &'static str = "studio.hardwave.analyser";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Stream audio to Hardwave Suite for real-time analysis");
    const CLAP_MANUAL_URL: Option<&'static str> = Some("https://hardwave.studio/docs/analyser");
    const CLAP_SUPPORT_URL: Option<&'static str> = Some("https://hardwave.studio/support");
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Analyzer,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for HardwaveAnalyser {
    const VST3_CLASS_ID: [u8; 16] = *b"HardwaveAnalyser";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Analyzer,
        Vst3SubCategory::Tools,
    ];
}

nih_export_clap!(HardwaveAnalyser);
nih_export_vst3!(HardwaveAnalyser);
