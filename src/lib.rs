//! Hardwave Bridge - VST3/CLAP plugin for streaming audio to Hardwave Suite
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

use crossbeam_channel::{bounded, Sender};
use nih_plug::prelude::*;
use std::sync::Arc;
use std::time::Instant;

use fft::{FftProcessor, FFT_SIZE};
use params::HardwaveBridgeParams;
use protocol::AudioPacket;
use websocket::WebSocketClient;

/// Main plugin struct
pub struct HardwaveBridge {
    params: Arc<HardwaveBridgeParams>,

    /// WebSocket client for streaming to the desktop app
    ws_client: WebSocketClient,

    /// Sender for the editor webview (gui feature)
    editor_packet_tx: Sender<AudioPacket>,

    /// Editor instance (created once, reused)
    #[cfg(feature = "gui")]
    editor_instance: Option<editor::HardwaveBridgeEditor>,

    /// FFT processor for left channel
    fft_left: FftProcessor,

    /// FFT processor for right channel
    fft_right: FftProcessor,

    /// Sample buffer for left channel
    buffer_left: Vec<f32>,

    /// Sample buffer for right channel
    buffer_right: Vec<f32>,

    /// Current sample rate
    sample_rate: f32,

    /// Samples since last FFT send
    samples_since_send: usize,

    /// Samples between FFT sends (for ~20Hz update rate)
    samples_per_send: usize,

    /// Plugin start time for timestamps
    start_time: Instant,

    /// Last port value (for detecting changes)
    last_port: i32,
}

impl Default for HardwaveBridge {
    fn default() -> Self {
        let (editor_packet_tx, editor_packet_rx) = bounded::<AudioPacket>(32);

        Self {
            params: Arc::new(HardwaveBridgeParams::default()),
            ws_client: WebSocketClient::new(),
            editor_packet_tx,
            #[cfg(feature = "gui")]
            editor_instance: {
                Some(editor::HardwaveBridgeEditor::new(editor_packet_rx))
            },
            fft_left: FftProcessor::new(),
            fft_right: FftProcessor::new(),
            buffer_left: Vec::with_capacity(FFT_SIZE),
            buffer_right: Vec::with_capacity(FFT_SIZE),
            sample_rate: 48000.0,
            samples_since_send: 0,
            samples_per_send: 2400, // 48000 / 20 = 2400 samples for 20Hz
            start_time: Instant::now(),
            last_port: 9847,
        }
    }
}

impl Plugin for HardwaveBridge {
    const NAME: &'static str = "Hardwave Bridge";
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
        self.samples_per_send = (self.sample_rate / 20.0) as usize; // 20Hz update rate

        // Clear buffers
        self.buffer_left.clear();
        self.buffer_right.clear();

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

            // Add to buffers
            self.buffer_left.push(left);
            self.buffer_right.push(right);

            // Keep buffer at FFT_SIZE
            if self.buffer_left.len() > FFT_SIZE {
                self.buffer_left.remove(0);
                self.buffer_right.remove(0);
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

impl HardwaveBridge {
    /// Process and send FFT data
    fn send_fft_data(&mut self) {
        // Process FFT for both channels
        let left_bands = self.fft_left.process(&self.buffer_left, self.sample_rate);
        let right_bands = self.fft_right.process(&self.buffer_right, self.sample_rate);

        // Calculate levels
        let (left_peak, left_rms) = FftProcessor::calculate_levels(&self.buffer_left);
        let (right_peak, right_rms) = FftProcessor::calculate_levels(&self.buffer_right);

        // Create and send packet
        let timestamp_ms = self.start_time.elapsed().as_millis() as u64;

        let packet = AudioPacket::new_fft(
            self.sample_rate as u32,
            timestamp_ms,
            left_bands,
            right_bands,
            left_peak,
            right_peak,
            left_rms,
            right_rms,
        );

        // Send to WebSocket (desktop app)
        self.ws_client.send(packet.clone());

        // Send to editor webview (non-blocking, drops if full)
        let _ = self.editor_packet_tx.try_send(packet);
    }
}

impl ClapPlugin for HardwaveBridge {
    const CLAP_ID: &'static str = "studio.hardwave.bridge";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Stream audio to Hardwave Suite for real-time analysis");
    const CLAP_MANUAL_URL: Option<&'static str> = Some("https://hardwave.studio/docs/bridge");
    const CLAP_SUPPORT_URL: Option<&'static str> = Some("https://hardwave.studio/support");
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Analyzer,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for HardwaveBridge {
    const VST3_CLASS_ID: [u8; 16] = *b"HardwaveBridge00";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Analyzer,
        Vst3SubCategory::Tools,
    ];
}

nih_export_clap!(HardwaveBridge);
nih_export_vst3!(HardwaveBridge);
