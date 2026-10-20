//! Hardwave Analyser - VST3/CLAP plugin for streaming audio to Hardwave Suite
//!
//! This plugin captures audio from the DAW and streams FFT data via WebSocket
//! to the Hardwave Suite desktop application for real-time analysis.
//! When built with the `gui` feature, it also embeds a wry webview that loads
//! the Hardwave Analyser from hardwave.studio inside the DAW plugin window.

#![allow(
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::type_complexity
)]
// The vst3_com vtable macro, expanded by nih_export_vst3!, ends an
// expression with a semicolon. Newer rustc warns about that, and builds
// that deny warnings then fail on code that is not ours. Remove this when
// vst3_com is updated.
#![allow(unknown_lints)]
#![allow(semicolon_in_expressions_from_macros)]
#![allow(semicolon_in_expressions_from_non_local_macros)]
mod analysis;
mod auth;
#[cfg(test)]
mod bench;
#[macro_use]
pub mod diag;
#[cfg(feature = "gui")]
mod editor;
mod fft;
mod loudness;
mod params;
mod protocol;
mod ring;
mod websocket;
#[cfg(any(target_os = "windows", test))]
mod webview_win;

use nih_plug::prelude::*;
use parking_lot::Mutex;
#[cfg(feature = "gui")]
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;

use analysis::{
    Analysis, AnalysisSetup, AnalysisThread, Consumers, LoudnessReader, LoudnessWriter, PacketSlot,
};
use loudness::LoudnessMeter;
use params::HardwaveAnalyserParams;
use ring::RingWriter;
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
                let _ = writeln!(f);
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
            let _ = std::fs::write(
                &pending,
                format!("analyser\n{}\n{}", env!("CARGO_PKG_VERSION"), crash_ts),
            );
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

    /// Shared slot for the latest FFT packet (written by the analysis thread,
    /// read by the editor). Arc'd so nobody deep-copies the ~37 KB packet.
    packet_slot: PacketSlot,

    // Editor is constructed fresh per `editor()` call — see comment on the
    // `editor()` impl below for why the previous one-shot Option pattern was
    // a re-open bug, not an optimisation.
    /// Open editor windows and the Suite connection. The analysis thread
    /// only works while one of them is there.
    consumers: Arc<Consumers>,

    /// The audio thread's end of the sample hand-off to the analysis thread.
    /// None until `initialize()`: a host that calls `process()` first still
    /// gets audio through and loudness measured.
    ring: Option<RingWriter>,

    /// Spectrum, levels and scope, worked out off the audio thread. Runs
    /// between `initialize()` and `deactivate()`.
    analysis: Option<AnalysisThread>,

    /// BS.1770 / EBU R128 loudness, measured on every sample
    loudness: LoudnessMeter,

    /// Where `process()` leaves the latest loudness reading for the analysis
    /// thread, and the analysis thread's end of it.
    loudness_out: LoudnessWriter,
    loudness_in: LoudnessReader,

    /// Current sample rate
    sample_rate: f32,

    /// Plugin start time for timestamps
    start_time: Instant,

    /// Shared interval (ms) between FFT sends, read by the editor thread for its sleep duration
    #[cfg(feature = "gui")]
    refresh_interval_ms: Arc<AtomicU32>,
}

impl Default for HardwaveAnalyser {
    fn default() -> Self {
        install_crash_handler();
        crash_reporter::install("analyser");

        let packet_slot: PacketSlot = Arc::new(Mutex::new(None));
        let consumers = Consumers::new();
        let (loudness_out, loudness_in) = analysis::loudness_handoff();

        #[cfg(feature = "gui")]
        let refresh_interval_ms = Arc::new(AtomicU32::new(16)); // 1000 / 60Hz

        Self {
            params: Arc::new(HardwaveAnalyserParams::default()),
            ws_client: WebSocketClient::new(Arc::clone(&consumers)),
            packet_slot,
            consumers,
            ring: None,
            analysis: None,
            loudness: LoudnessMeter::new(48000.0),
            loudness_out,
            loudness_in,
            sample_rate: 48000.0,
            start_time: Instant::now(),
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
                Arc::clone(&self.consumers),
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

        // Redesign the K-weighting for this rate and size the 3 s window
        // (allocates here, never in process). Restarts the measurement.
        self.loudness.set_sample_rate(self.sample_rate);

        // Start WebSocket client (deferred from new() to avoid blocking DAW scans)
        self.ws_client.start(self.params.port.value());

        // A host may initialize again without deactivating in between (a new
        // sample rate): stop the old analysis thread before starting one for
        // this rate, so there is never more than one.
        self.analysis = None;
        let (ring, ring_reader) = ring::sample_ring(analysis::RING_FRAMES);
        self.ring = Some(ring);
        self.analysis = AnalysisThread::start(Analysis::new(AnalysisSetup {
            params: Arc::clone(&self.params),
            consumers: Arc::clone(&self.consumers),
            ring: ring_reader,
            loudness: self.loudness_in.clone(),
            packet_slot: Arc::clone(&self.packet_slot),
            suite: self.ws_client.link(),
            #[cfg(feature = "gui")]
            refresh_interval_ms: Arc::clone(&self.refresh_interval_ms),
            sample_rate: self.sample_rate,
            start_time: self.start_time,
        }));

        true
    }

    fn reset(&mut self) {
        self.loudness.reset();
        if let Some(ring) = &self.ring {
            ring.mark_reset();
        }
    }

    fn deactivate(&mut self) {
        // Joins the thread, so nothing of ours runs once the host unloads us.
        self.analysis = None;
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Skip processing if disabled
        if !self.params.enabled.value() {
            return ProcessStatus::Normal;
        }

        let channel_slices = buffer.as_slice();

        if let Some(left) = channel_slices.first() {
            let left: &[f32] = left;
            let right = channel_slices.get(1).map(|right| &**right);

            // Loudness sees every sample of every buffer. A mono track has no
            // right channel here, so BS.1770 weights it once.
            self.loudness.process(left, right);
            // Before the samples go into the ring, so the analysis thread
            // never sees samples newer than the reading it sends with them.
            self.loudness_out.publish(&self.loudness.reading());

            // Everything else the window shows is worked out on the analysis
            // thread; this only copies the block across, without waiting or
            // allocating. A mono track feeds the same samples to both sides
            // of the spectrum and the scope.
            if let Some(ring) = &mut self.ring {
                ring.push(left, right.unwrap_or(left));
            }
        }

        // Pass through audio unchanged
        ProcessStatus::Normal
    }
}

impl HardwaveAnalyser {
    /// Write a line to the same debug log as editor.rs
    fn debug_log(msg: &str) {
        use std::io::Write;
        let path = {
            let mut p = std::env::temp_dir();
            p.push("hardwave-debug.log");
            p
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = writeln!(f, "[{}] [lib] {}", now, msg);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{activate, plugin_with, with_block, Host};
    use crate::protocol::{NUM_BINS, PACKET_TYPE_FFT};
    use std::thread;
    use std::time::Duration;

    /// Analysis threads alive in this process, counted by name in /proc.
    /// None where there is no /proc to count them in.
    fn analysis_threads() -> Option<usize> {
        let tasks = std::fs::read_dir("/proc/self/task").ok()?;
        Some(
            tasks
                .flatten()
                .filter(|task| {
                    std::fs::read_to_string(task.path().join("comm"))
                        .is_ok_and(|comm| comm.trim() == analysis::THREAD_NAME)
                })
                .count(),
        )
    }

    /// A joined thread can stay listed for a moment while the kernel
    /// finishes with it.
    fn assert_analysis_threads(expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let Some(count) = analysis_threads() else {
                return;
            };
            if count == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{count} analysis threads, expected {expected}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// A loopback port nothing listens on.
    fn unused_port() -> i32 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        i32::from(listener.local_addr().unwrap().port())
    }

    /// Play `blocks` blocks of a sine, as fast as the plug-in takes them.
    fn play(plugin: &mut HardwaveAnalyser, blocks: usize) {
        let mut left = [0.0_f32; 256];
        let mut right = [0.0_f32; 256];
        for _ in 0..blocks {
            for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
                *l = 0.5 * (i as f32 * 0.05).sin();
                *r = -*l;
            }
            with_block(&mut left, &mut right, |buffer, aux| {
                plugin.process(buffer, aux, &mut Host)
            });
        }
    }

    /// Play audio in real time until `done` holds, for at most `limit`.
    fn play_until(
        plugin: &mut HardwaveAnalyser,
        limit: Duration,
        mut done: impl FnMut(&HardwaveAnalyser) -> bool,
    ) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            play(plugin, 1);
            if done(plugin) {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// One test, not several: the thread counts would see each other's
    /// plug-ins if they ran in parallel.
    #[test]
    fn analysis_runs_only_while_someone_looks_and_never_outlives_the_plugin() {
        let mut plugin = plugin_with(144, unused_port());

        // A host that calls process() before initialize(): audio passes and
        // loudness is measured, nothing else happens and nothing breaks.
        play(&mut plugin, 4);
        assert!(plugin.analysis.is_none() && plugin.ring.is_none());
        assert_analysis_threads(0);

        activate(&mut plugin);
        assert_analysis_threads(1);
        // Initialized again without a deactivate in between (a new sample
        // rate): still exactly one thread.
        activate(&mut plugin);
        assert_analysis_threads(1);

        // Nobody looking: no analysis, no packets.
        play(&mut plugin, 200);
        thread::sleep(Duration::from_millis(150));
        play(&mut plugin, 20);
        thread::sleep(Duration::from_millis(50));
        assert!(plugin.packet_slot.lock().is_none());

        // An editor opens: packets at once, with a full window of the audio
        // that played while nobody looked.
        let editor = plugin.consumers.editor_opened();
        assert!(play_until(&mut plugin, Duration::from_secs(3), |p| p
            .packet_slot
            .lock()
            .is_some()));
        let packet = plugin.packet_slot.lock().take().unwrap();
        assert_eq!(packet.packet_type, PACKET_TYPE_FFT);
        assert_eq!(packet.left_bins.len(), NUM_BINS);
        assert!(packet.left_peak > -7.0 && packet.left_peak < -5.0);

        // It closes: packets stop.
        drop(editor);
        // A tick already under way may still deliver one packet.
        thread::sleep(Duration::from_millis(200));
        plugin.packet_slot.lock().take();
        play(&mut plugin, 50);
        thread::sleep(Duration::from_millis(150));
        assert!(plugin.packet_slot.lock().is_none());

        // Deactivate joins the thread; the next activation starts a new one;
        // dropping the plug-in leaves none behind.
        plugin.deactivate();
        assert!(plugin.analysis.is_none());
        assert_analysis_threads(0);
        activate(&mut plugin);
        assert_analysis_threads(1);
        drop(plugin);
        assert_analysis_threads(0);

        // The Suite connecting is enough to start the analysis, and it gets
        // FFT packets over the same WebSocket as before.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = i32::from(listener.local_addr().unwrap().port());
        let suite = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            loop {
                match socket.read().expect("the plug-in keeps sending") {
                    tungstenite::Message::Binary(data) => {
                        let packet: protocol::AudioPacket = bincode::deserialize(&data).unwrap();
                        if packet.packet_type == PACKET_TYPE_FFT {
                            return packet;
                        }
                    }
                    _ => continue,
                }
            }
        });
        let mut plugin = plugin_with(144, port);
        activate(&mut plugin);
        assert!(play_until(&mut plugin, Duration::from_secs(5), |_| suite.is_finished()));
        let packet = suite.join().unwrap();
        assert_eq!(packet.sample_rate, 48_000);
        assert_eq!(packet.right_bins.len(), NUM_BINS);
        assert_eq!(packet.right_wave.len(), protocol::WAVE_SIZE);
        drop(plugin);
        assert_analysis_threads(0);
    }
}
