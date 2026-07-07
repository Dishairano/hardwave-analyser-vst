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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use fft::{FftProcessor, FFT_SIZE, WELCH_MIN_SAMPLES};
use params::HardwaveAnalyserParams;
use protocol::AudioPacket;
use websocket::WebSocketClient;

/// Hardwave data directory (shared between plugin and Suite).
fn hardwave_data_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("hardwave")
}

/// Path to the crash log file.
fn crash_log_path() -> std::path::PathBuf {
    hardwave_data_dir().join("analyser-crash.log")
}

/// Sentinel file: existence = "there's an unsent crash report".
/// The Suite deletes it after the user accepts/dismisses the upload prompt.
fn crash_pending_path() -> std::path::PathBuf {
    hardwave_data_dir().join("analyser-crash-pending")
}

/// Install a panic hook that writes crash details to a persistent log file.
/// Called once per process; subsequent calls are no-ops.
mod crash_reporter;

fn install_crash_handler() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            use std::io::Write;
            let path = crash_log_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let ts = chrono_timestamp();
                let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = info.payload().downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                let location = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "unknown location".to_string());
                let bt = std::backtrace::Backtrace::force_capture();
                // Stash for the telemetry hook (runs after this one) so it
                // doesn't capture a second backtrace on the panicking thread.
                if let Ok(mut g) = crash_reporter::LAST_BACKTRACE.lock() {
                    *g = Some(bt.to_string());
                }

                let _ = writeln!(f, "========================================");
                let _ = writeln!(f, "HARDWAVE ANALYSER CRASH REPORT");
                let _ = writeln!(f, "Time:     {}", ts);
                let _ = writeln!(f, "Version:  {}", env!("CARGO_PKG_VERSION"));
                let _ = writeln!(f, "OS:       {}", std::env::consts::OS);
                let _ = writeln!(f, "Arch:     {}", std::env::consts::ARCH);
                let _ = writeln!(f, "Location: {}", location);
                let _ = writeln!(f, "Message:  {}", payload);
                let _ = writeln!(f, "");
                let _ = writeln!(f, "Backtrace:");
                let _ = writeln!(f, "{}", bt);
                let _ = writeln!(f, "========================================");
                let _ = writeln!(f);
            }
            // Write sentinel so the Suite knows there's a crash to report.
            let pending = crash_pending_path();
            if let Some(parent) = pending.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let crash_ts = chrono_timestamp();
            let _ = std::fs::write(&pending, format!("analyser\n{}\n{}", env!("CARGO_PKG_VERSION"), crash_ts));
            // Call the previous hook so nih-plug / DAW logging still works
            prev(info);
        }));
    });
}

/// Simple UTC timestamp without pulling in chrono.
fn chrono_timestamp() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Format as seconds-since-epoch (readable via any converter)
    // plus a human-approx: days since 2025-01-01
    format!("{} (unix)", secs)
}

/// Main plugin struct
pub struct HardwaveAnalyser {
    params: Arc<HardwaveAnalyserParams>,

    /// WebSocket client for streaming to the desktop app
    ws_client: WebSocketClient,

    /// Shared slot for the latest FFT packet (written by audio thread, read by
    /// editor). Arc'd so the audio thread never deep-copies the ~37 KB packet.
    packet_slot: Arc<Mutex<Option<std::sync::Arc<AudioPacket>>>>,

    // Editor is constructed fresh per `editor()` call — see comment on the
    // `editor()` impl below for why the previous one-shot Option pattern was
    // a re-open bug, not an optimisation.

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

    /// Shared interval (ms) between FFT sends, read by the editor thread for its sleep duration
    #[cfg(feature = "gui")]
    refresh_interval_ms: Arc<AtomicU32>,

}

impl Default for HardwaveAnalyser {
    fn default() -> Self {
        install_crash_handler();
        crash_reporter::install("analyser");

        let packet_slot: Arc<Mutex<Option<std::sync::Arc<AudioPacket>>>> = Arc::new(Mutex::new(None));

        #[cfg(feature = "gui")]
        let refresh_interval_ms = Arc::new(AtomicU32::new(16)); // 1000 / 60Hz

        Self {
            params: Arc::new(HardwaveAnalyserParams::default()),
            ws_client: WebSocketClient::new(),
            packet_slot: Arc::clone(&packet_slot),
            fft_left: FftProcessor::new(),
            fft_right: FftProcessor::new(),
            buffer_left: VecDeque::with_capacity(WELCH_MIN_SAMPLES),
            buffer_right: VecDeque::with_capacity(WELCH_MIN_SAMPLES),
            sample_rate: 48000.0,
            samples_since_send: 0,
            samples_per_send: 800, // 48000 / 60 = 800 samples for 60Hz default
            start_time: Instant::now(),
            last_port: 9847,
            last_refresh_rate: 60,
            #[cfg(feature = "gui")]
            refresh_interval_ms,
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
        // Construct a fresh editor on every call — `editor()` is invoked
        // once per attach, not once per plug-in instance. The previous
        // `Option::take()` pattern returned `Some` once and `None` forever,
        // so re-opening the GUI in a session showed nothing. (F9 in the
        // VST stability audit.)
        #[cfg(feature = "gui")]
        {
            Some(Box::new(editor::HardwaveAnalyserEditor::new(
                Arc::clone(&self.packet_slot),
                Arc::clone(&self.refresh_interval_ms),
                Arc::clone(&self.params),
            )) as Box<dyn Editor>)
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
        Self::debug_log(&format!(
            "Hardwave Analyser v{} initialized (sr={}, crash log: {:?})",
            env!("CARGO_PKG_VERSION"),
            buffer_config.sample_rate,
            crash_log_path(),
        ));

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
            #[cfg(feature = "gui")]
            self.refresh_interval_ms.store(
                (1000.0 / current_refresh_rate as f32) as u32,
                Ordering::Relaxed,
            );
        }

        // Skip processing if disabled
        if !self.params.enabled.value() {
            return ProcessStatus::Normal;
        }

        let num_channels = buffer.channels();
        let num_samples = buffer.samples();

        // Process each sample
        let channel_slices = buffer.as_slice();
        for sample_idx in 0..num_samples {
            // Get samples (handle mono by duplicating)
            let left = channel_slices[0][sample_idx];
            let right = if num_channels > 1 {
                channel_slices[1][sample_idx]
            } else {
                left
            };

            // Add to ring buffers
            self.buffer_left.push_back(left);
            self.buffer_right.push_back(right);

            // Keep buffer sized for full Welch's-method coverage.
            // (FFT_SIZE + (WELCH_SEGMENTS-1) * FFT_SIZE/2 = 20480 samples at 8192 FFT)
            // The previous cap of FFT_SIZE * 2 silently degraded to single-segment Welch.
            if self.buffer_left.len() > WELCH_MIN_SAMPLES {
                self.buffer_left.pop_front();
                self.buffer_right.pop_front();
            }

            self.samples_since_send += 1;
        }

        // Send FFT data at the configured refresh rate (60–144 Hz param)
        if self.samples_since_send >= self.samples_per_send && self.buffer_left.len() >= FFT_SIZE {
            // Catch panics so a crash in FFT/WS code doesn't take down the DAW.
            // The panic hook still writes the crash log before we get here.
            let wrapper = std::panic::AssertUnwindSafe(|| self.send_fft_data());
            if std::panic::catch_unwind(move || wrapper()).is_err() {
                Self::debug_log("PANIC caught in send_fft_data — see crash log");
            }
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
        // Update window function if changed
        let wf = fft::WindowFn::from(self.params.window_fn.value());
        self.fft_left.set_window_fn(wf);
        self.fft_right.set_window_fn(wf);

        // Make VecDeque storage contiguous so we can take a slice.
        let left_slice = self.buffer_left.make_contiguous();
        let left_bins = self.fft_left.process(left_slice, self.sample_rate);
        let (left_peak, left_rms, _left_true_peak) = FftProcessor::calculate_levels(left_slice);

        let right_slice = self.buffer_right.make_contiguous();
        let right_bins = self.fft_right.process(right_slice, self.sample_rate);
        let (right_peak, right_rms, _right_true_peak) = FftProcessor::calculate_levels(right_slice);

        // Create and send packet
        let timestamp_ms = self.start_time.elapsed().as_millis() as u64;

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

        // One shared allocation: the WS thread and the editor read the same
        // packet through Arcs instead of the audio thread deep-copying it.
        let packet = std::sync::Arc::new(packet);
        self.ws_client.send(std::sync::Arc::clone(&packet));

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
