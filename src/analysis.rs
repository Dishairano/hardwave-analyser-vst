//! Spectrum, levels and scope, worked out away from the audio thread.
//!
//! The audio thread measures loudness (that stays per sample, in `process`)
//! and copies each block into a [`ring`](crate::ring). That is all it does
//! for the analysis. This thread wakes at the refresh rate, takes the new
//! samples and builds the same packet the audio thread used to build: Welch
//! FFTs, peak, RMS, true peak, the scope waveform and the loudness reading.
//! The packet goes to the editor's slot and to the Suite connection, the same
//! two places as before.
//!
//! Nobody looking means no work: while no editor is open and the Suite is not
//! connected, this thread does no analysis and only wakes now and then to keep
//! the Suite port in step with the parameter. Opening the editor or connecting
//! the Suite wakes it at once, and because the audio thread kept filling the
//! ring, the first packet already has a full window of recent audio.

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::fft::{self, FftProcessor, TruePeakWindow, FFT_SIZE, WELCH_MIN_SAMPLES};
use crate::loudness::{LoudnessReading, LOUDNESS_FLOOR};
use crate::params::HardwaveAnalyserParams;
use crate::protocol::{AudioPacket, WAVE_SIZE};
use crate::ring::RingReader;
use crate::websocket::SuiteLink;

/// Frames the ring between the audio thread and this thread holds: how far
/// the thread can fall behind before it misses audio (1.4 s at 48 kHz,
/// 0.34 s at 192 kHz). More than the 20480-sample Welch window, so the first
/// packet after an idle spell has a full window. 512 KiB per instance.
pub const RING_FRAMES: usize = 1 << 16;

/// How often an idle thread looks at the port parameter. Editors and the
/// Suite wake it directly, so this is not how fast it resumes.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// The thread's name, also how the benchmark finds its CPU time.
pub const THREAD_NAME: &str = "hw-analysis";

/// The latest packet, handed to the editor.
pub type PacketSlot = Arc<Mutex<Option<Arc<AudioPacket>>>>;

/// Who is reading the analysis: open editor windows and the Suite.
pub struct Consumers {
    editors: AtomicUsize,
    suite: AtomicBool,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl Consumers {
    pub fn new() -> Arc<Self> {
        let (wake_tx, wake_rx) = bounded(1);
        Arc::new(Self {
            editors: AtomicUsize::new(0),
            suite: AtomicBool::new(false),
            wake_tx,
            wake_rx,
        })
    }

    /// An editor window opened. It counts as open until the guard drops.
    pub fn editor_opened(self: &Arc<Self>) -> EditorOpen {
        self.editors.fetch_add(1, Ordering::AcqRel);
        self.wake();
        EditorOpen(Arc::clone(self))
    }

    /// The Suite connection came up or went down.
    pub fn set_suite_connected(&self, connected: bool) {
        if self.suite.swap(connected, Ordering::AcqRel) != connected && connected {
            self.wake();
        }
    }

    /// Anyone there?
    pub fn any(&self) -> bool {
        self.editors.load(Ordering::Acquire) > 0 || self.suite.load(Ordering::Acquire)
    }

    fn wake(&self) {
        // Full means a wake-up is already waiting; one is enough.
        let _ = self.wake_tx.try_send(());
    }
}

/// Held by an open editor window; dropping it closes the window's claim.
pub struct EditorOpen(Arc<Consumers>);

impl Drop for EditorOpen {
    fn drop(&mut self) {
        self.0.editors.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The loudness reading, shared through a seqlock: the audio thread writes
/// it without ever waiting, the analysis thread reads it and retries in the
/// rare case it caught a write half done.
struct LoudnessCell {
    /// Odd while a write is in progress.
    sequence: AtomicU32,
    momentary: AtomicU32,
    short_term: AtomicU32,
    integrated: AtomicU32,
    range: AtomicU32,
    epoch: AtomicU32,
}

/// Make the pair of handles. Allocates; call it outside `process`.
pub fn loudness_handoff() -> (LoudnessWriter, LoudnessReader) {
    let floor = LOUDNESS_FLOOR.to_bits();
    let cell = Arc::new(LoudnessCell {
        sequence: AtomicU32::new(0),
        momentary: AtomicU32::new(floor),
        short_term: AtomicU32::new(floor),
        integrated: AtomicU32::new(floor),
        range: AtomicU32::new(0.0_f32.to_bits()),
        epoch: AtomicU32::new(0),
    });
    (LoudnessWriter(Arc::clone(&cell)), LoudnessReader(cell))
}

/// The audio thread's end. Not `Clone`: a seqlock has one writer.
pub struct LoudnessWriter(Arc<LoudnessCell>);

impl LoudnessWriter {
    /// Wait-free. The reset flag is not passed on: the analysis thread sets
    /// it on the first packet whose epoch differs from the previous packet's,
    /// which is the packet the audio thread used to flag.
    pub fn publish(&mut self, reading: &LoudnessReading) {
        let cell = &*self.0;
        let sequence = cell.sequence.load(Ordering::Relaxed);
        cell.sequence
            .store(sequence.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        cell.momentary
            .store(reading.momentary.to_bits(), Ordering::Relaxed);
        cell.short_term
            .store(reading.short_term.to_bits(), Ordering::Relaxed);
        cell.integrated
            .store(reading.integrated.to_bits(), Ordering::Relaxed);
        cell.range.store(reading.range.to_bits(), Ordering::Relaxed);
        cell.epoch.store(reading.epoch, Ordering::Relaxed);
        cell.sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
    }
}

/// The analysis thread's end.
#[derive(Clone)]
pub struct LoudnessReader(Arc<LoudnessCell>);

impl LoudnessReader {
    /// The last reading published, never half of one. `reset` is false.
    pub fn latest(&self) -> LoudnessReading {
        let cell = &*self.0;
        loop {
            let before = cell.sequence.load(Ordering::Acquire);
            if before & 1 == 0 {
                let reading = LoudnessReading {
                    momentary: f32::from_bits(cell.momentary.load(Ordering::Relaxed)),
                    short_term: f32::from_bits(cell.short_term.load(Ordering::Relaxed)),
                    integrated: f32::from_bits(cell.integrated.load(Ordering::Relaxed)),
                    range: f32::from_bits(cell.range.load(Ordering::Relaxed)),
                    reset: false,
                    epoch: cell.epoch.load(Ordering::Relaxed),
                };
                fence(Ordering::Acquire);
                if cell.sequence.load(Ordering::Relaxed) == before {
                    return reading;
                }
            }
            std::hint::spin_loop();
        }
    }
}

/// The last `capacity` samples of one channel, oldest first, as one slice.
struct History {
    samples: Vec<f32>,
    len: usize,
}

impl History {
    fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0.0; capacity],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn extend(&mut self, new: &[f32]) {
        let capacity = self.samples.len();
        if new.len() >= capacity {
            self.samples.copy_from_slice(&new[new.len() - capacity..]);
            self.len = capacity;
            return;
        }
        let excess = (self.len + new.len()).saturating_sub(capacity);
        if excess > 0 {
            self.samples.copy_within(excess..self.len, 0);
            self.len -= excess;
        }
        self.samples[self.len..self.len + new.len()].copy_from_slice(new);
        self.len += new.len();
    }

    fn as_slice(&self) -> &[f32] {
        &self.samples[..self.len]
    }
}

/// One channel's analysis state.
struct Channel {
    fft: FftProcessor,
    history: History,
    true_peak: TruePeakWindow,
    /// Where the ring copies new samples.
    incoming: Vec<f32>,
}

impl Channel {
    fn new(ring_frames: usize) -> Self {
        Self {
            fft: FftProcessor::new(),
            history: History::new(WELCH_MIN_SAMPLES),
            true_peak: TruePeakWindow::new(WELCH_MIN_SAMPLES),
            incoming: vec![0.0; ring_frames],
        }
    }

    fn restart(&mut self) {
        self.history.clear();
        self.true_peak.clear();
    }

    fn take(&mut self, frames: usize) {
        let new = &self.incoming[..frames];
        self.history.extend(new);
        self.true_peak.push(new);
    }

    /// Bins, peak, RMS, true peak and scope, exactly as the audio thread
    /// computed them from its buffer.
    fn analyse(&mut self, window_fn: fft::WindowFn, sample_rate: f32) -> ChannelResult {
        self.fft.set_window_fn(window_fn);
        let samples = self.history.as_slice();
        let bins = self.fft.process(samples, sample_rate);
        let (peak, rms, true_peak) = FftProcessor::levels(samples, self.true_peak.peak());
        let wave = if samples.len() >= WAVE_SIZE {
            samples[samples.len() - WAVE_SIZE..].to_vec()
        } else {
            vec![0.0_f32; WAVE_SIZE]
        };
        ChannelResult {
            bins,
            peak,
            rms,
            true_peak,
            wave,
        }
    }
}

struct ChannelResult {
    bins: Vec<f32>,
    peak: f32,
    rms: f32,
    true_peak: f32,
    wave: Vec<f32>,
}

/// Everything the analysis thread works with, moved into it when it starts.
pub struct Analysis {
    params: Arc<HardwaveAnalyserParams>,
    consumers: Arc<Consumers>,
    ring: RingReader,
    loudness: LoudnessReader,
    packet_slot: PacketSlot,
    suite: Option<SuiteLink>,
    #[cfg(feature = "gui")]
    refresh_interval_ms: Arc<AtomicU32>,
    sample_rate: f32,
    start_time: Instant,

    left: Channel,
    right: Channel,
    /// Frames taken in since the last packet.
    fresh: usize,
    /// Loudness epoch of the last packet sent, None before the first.
    last_epoch: Option<u32>,
    last_port: Option<i32>,
    last_refresh_rate: i32,
}

/// What `Analysis::new` needs from the plug-in.
pub struct AnalysisSetup {
    pub params: Arc<HardwaveAnalyserParams>,
    pub consumers: Arc<Consumers>,
    pub ring: RingReader,
    pub loudness: LoudnessReader,
    pub packet_slot: PacketSlot,
    pub suite: Option<SuiteLink>,
    #[cfg(feature = "gui")]
    pub refresh_interval_ms: Arc<AtomicU32>,
    pub sample_rate: f32,
    pub start_time: Instant,
}

impl Analysis {
    /// Allocates the FFT plans and buffers. Call it outside `process`.
    pub fn new(setup: AnalysisSetup) -> Self {
        let ring_frames = setup.ring.capacity();
        // The editor's poll interval only follows changes, as it did when
        // process() owned it.
        let last_refresh_rate = setup.params.refresh_rate.value();
        Self {
            params: setup.params,
            consumers: setup.consumers,
            ring: setup.ring,
            loudness: setup.loudness,
            packet_slot: setup.packet_slot,
            suite: setup.suite,
            #[cfg(feature = "gui")]
            refresh_interval_ms: setup.refresh_interval_ms,
            sample_rate: setup.sample_rate,
            start_time: setup.start_time,
            left: Channel::new(ring_frames),
            right: Channel::new(ring_frames),
            fresh: 0,
            last_epoch: None,
            last_port: None,
            last_refresh_rate,
        }
    }

    /// Follow the port and refresh-rate parameters. Returns the time between
    /// packets.
    fn apply_settings(&mut self) -> Duration {
        let port = self.params.port.value();
        if self.last_port != Some(port) {
            if let Some(suite) = &self.suite {
                suite.set_port(port);
            }
            self.last_port = Some(port);
        }

        let refresh_rate = self.params.refresh_rate.value().max(1);
        if refresh_rate != self.last_refresh_rate {
            self.last_refresh_rate = refresh_rate;
            #[cfg(feature = "gui")]
            self.refresh_interval_ms
                .store((1000.0 / refresh_rate as f32) as u32, Ordering::Relaxed);
        }
        Duration::from_secs_f64(1.0 / f64::from(refresh_rate))
    }

    fn restart(&mut self) {
        self.left.restart();
        self.right.restart();
        self.fresh = 0;
    }

    /// Take the new samples and, if there are any, send a packet.
    fn tick(&mut self) {
        let read = self
            .ring
            .read(&mut self.left.incoming, &mut self.right.incoming);
        if read.restart {
            self.restart();
        }
        self.left.take(read.frames);
        self.right.take(read.frames);
        self.fresh += read.frames;

        // As before: once there is a full FFT's worth, and only when new
        // audio came in (a host that stops calling process gets no packets).
        if self.fresh > 0 && self.left.history.len >= FFT_SIZE {
            self.send_packet();
            self.fresh = 0;
        }
    }

    fn send_packet(&mut self) {
        let window_fn = fft::WindowFn::from(self.params.window_fn.value());
        let left = self.left.analyse(window_fn, self.sample_rate);
        let right = self.right.analyse(window_fn, self.sample_rate);

        let mut loudness = self.loudness.latest();
        loudness.reset = self.last_epoch != Some(loudness.epoch);
        self.last_epoch = Some(loudness.epoch);

        let timestamp_ms = self.start_time.elapsed().as_millis() as u64;
        let packet = AudioPacket::new_fft(
            self.sample_rate as u32,
            timestamp_ms,
            left.bins,
            right.bins,
            left.peak,
            right.peak,
            left.rms,
            right.rms,
            left.wave,
            right.wave,
            left.true_peak,
            right.true_peak,
        )
        .with_loudness(&loudness);

        // One shared allocation: the WS thread and the editor read the same
        // packet through Arcs.
        let packet = Arc::new(packet);
        if let Some(suite) = &self.suite {
            suite.send(Arc::clone(&packet));
        }
        *self.packet_slot.lock() = Some(packet);
    }

    fn run(mut self, stop: Arc<AtomicBool>, wake: Receiver<()>) {
        let mut next_tick = Instant::now();
        while !stop.load(Ordering::Acquire) {
            let interval = self.apply_settings();
            let wait = if self.consumers.any() {
                // A panic here must not end the thread: the window would
                // freeze for the rest of the session. The panic hook has
                // already written the crash log.
                let tick = std::panic::AssertUnwindSafe(|| self.tick());
                if std::panic::catch_unwind(tick).is_err() {
                    crate::HardwaveAnalyser::debug_log(
                        "PANIC caught in the analysis thread, see crash log",
                    );
                    self.restart();
                }
                let now = Instant::now();
                next_tick = (next_tick + interval).max(now);
                next_tick - now
            } else {
                next_tick = Instant::now();
                IDLE_POLL
            };
            match wake.recv_timeout(wait) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// The running analysis thread. Dropping it stops and joins the thread.
pub struct AnalysisThread {
    stop: Arc<AtomicBool>,
    consumers: Arc<Consumers>,
    thread: Option<JoinHandle<()>>,
}

impl AnalysisThread {
    /// None if the system would not give us a thread; the plug-in then still
    /// passes audio and measures loudness, the window just stays still.
    pub fn start(analysis: Analysis) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let consumers = Arc::clone(&analysis.consumers);
        let wake = consumers.wake_rx.clone();
        let thread = thread::Builder::new().name(THREAD_NAME.to_string()).spawn({
            let stop = Arc::clone(&stop);
            move || analysis.run(stop, wake)
        });
        match thread {
            Ok(thread) => Some(Self {
                stop,
                consumers,
                thread: Some(thread),
            }),
            Err(e) => {
                crate::HardwaveAnalyser::debug_log(&format!(
                    "analysis thread could not start: {e}"
                ));
                None
            }
        }
    }
}

impl Drop for AnalysisThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.consumers.wake();
        // Bounded by one tick of work: the thread checks the flag after
        // every tick and the wake cuts its wait short.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_the_newest_samples_in_order() {
        let mut history = History::new(8);
        history.extend(&[1.0, 2.0, 3.0]);
        assert_eq!(history.as_slice(), [1.0, 2.0, 3.0]);
        history.extend(&[4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert_eq!(history.as_slice(), [2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        history.extend(&(10..30).map(|v| v as f32).collect::<Vec<_>>());
        assert_eq!(
            history.as_slice(),
            (22..30).map(|v| v as f32).collect::<Vec<_>>()
        );
        history.clear();
        assert!(history.as_slice().is_empty());
    }

    #[test]
    fn loudness_handoff_passes_the_reading_on() {
        let (mut writer, reader) = loudness_handoff();
        let before = reader.latest();
        assert_eq!(before.momentary, LOUDNESS_FLOOR);
        assert_eq!(before.epoch, 0);

        let reading = LoudnessReading {
            momentary: -14.5,
            short_term: -15.25,
            integrated: -16.0,
            range: 7.5,
            reset: true,
            epoch: 3,
        };
        writer.publish(&reading);
        assert_eq!(
            reader.latest(),
            LoudnessReading {
                reset: false,
                ..reading
            }
        );
    }

    /// The reader never sees fields from two different readings.
    #[test]
    fn loudness_reads_are_never_torn() {
        let (mut writer, reader) = loudness_handoff();
        let writer_thread = thread::spawn(move || {
            for i in 0..200_000_u32 {
                let v = -(i as f32);
                writer.publish(&LoudnessReading {
                    momentary: v,
                    short_term: v,
                    integrated: v,
                    range: v,
                    reset: false,
                    epoch: i,
                });
            }
        });
        while !writer_thread.is_finished() {
            let r = reader.latest();
            if r.epoch == 0 && r.momentary == LOUDNESS_FLOOR {
                continue;
            }
            let v = -(r.epoch as f32);
            assert_eq!(
                (r.momentary, r.short_term, r.integrated, r.range),
                (v, v, v, v)
            );
        }
        writer_thread.join().unwrap();
    }

    /// The old audio-thread path, as it was: a VecDeque of the last
    /// WELCH_MIN_SAMPLES samples per channel, analysed in full for a packet.
    struct OldPath {
        left: std::collections::VecDeque<f32>,
        right: std::collections::VecDeque<f32>,
        fft_left: FftProcessor,
        fft_right: FftProcessor,
    }

    impl OldPath {
        fn push(&mut self, l: &[f32], r: &[f32]) {
            for (&l, &r) in l.iter().zip(r) {
                self.left.push_back(l);
                self.right.push_back(r);
                if self.left.len() > WELCH_MIN_SAMPLES {
                    self.left.pop_front();
                    self.right.pop_front();
                }
            }
        }

        fn clear(&mut self) {
            self.left.clear();
            self.right.clear();
        }

        fn check(&mut self, packet: &AudioPacket) {
            for (buffer, fft, bins, peak, rms, true_peak, wave) in [
                (
                    &mut self.left,
                    &mut self.fft_left,
                    &packet.left_bins,
                    packet.left_peak,
                    packet.left_rms,
                    packet.left_true_peak,
                    &packet.left_wave,
                ),
                (
                    &mut self.right,
                    &mut self.fft_right,
                    &packet.right_bins,
                    packet.right_peak,
                    packet.right_rms,
                    packet.right_true_peak,
                    &packet.right_wave,
                ),
            ] {
                let samples = buffer.make_contiguous();
                assert_eq!(&fft.process(samples, 48_000.0), bins);
                assert_eq!(
                    FftProcessor::calculate_levels(samples),
                    (peak, rms, true_peak)
                );
                assert_eq!(&samples[samples.len() - WAVE_SIZE..], &wave[..]);
            }
        }
    }

    /// What the window and the Suite get from the analysis thread is what the
    /// audio thread used to send for the same audio: bins, levels, true peak,
    /// scope and loudness, through resets as well.
    #[test]
    fn packets_match_what_the_audio_thread_built() {
        let (mut ring, ring_reader) = crate::ring::sample_ring(RING_FRAMES);
        let (mut loudness_out, loudness_in) = loudness_handoff();
        let slot: PacketSlot = Arc::new(Mutex::new(None));
        let mut analysis = Analysis::new(AnalysisSetup {
            params: Arc::new(HardwaveAnalyserParams::default()),
            consumers: Consumers::new(),
            ring: ring_reader,
            loudness: loudness_in,
            packet_slot: Arc::clone(&slot),
            suite: None,
            #[cfg(feature = "gui")]
            refresh_interval_ms: Arc::new(AtomicU32::new(16)),
            sample_rate: 48_000.0,
            start_time: Instant::now(),
        });
        let mut old = OldPath {
            left: Default::default(),
            right: Default::default(),
            fft_left: FftProcessor::new(),
            fft_right: FftProcessor::new(),
        };

        let len = 4 * 48_000;
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        };
        let left: Vec<f32> = (0..len)
            .map(|i| 0.5 * (i as f32 * 0.0131).sin() + 0.3 * noise())
            .collect();
        let right: Vec<f32> = (0..len)
            .map(|i| 0.7 * (i as f32 * 0.25 + 0.785).sin() + 0.1 * noise())
            .collect();

        let mut packets = 0;
        let mut epoch = 1;
        let mut expect_reset = true;
        for (k, (l, r)) in left.chunks(256).zip(right.chunks(256)).enumerate() {
            // A host reset part way through: the old path emptied its
            // buffers, the new one drops what came before the mark.
            if k == 400 {
                old.clear();
                ring.mark_reset();
                epoch += 1;
                expect_reset = true;
            }
            loudness_out.publish(&LoudnessReading {
                momentary: -20.0 - k as f32 * 0.01,
                short_term: -21.0,
                integrated: -22.0,
                range: 3.5,
                reset: false,
                epoch,
            });
            ring.push(l, r);
            old.push(l, r);

            // The thread wakes every few blocks, at its own pace.
            if k % 3 != 2 {
                continue;
            }
            analysis.tick();
            let packet = slot.lock().take();
            if old.left.len() < FFT_SIZE {
                assert!(packet.is_none(), "no packet before a full FFT");
                continue;
            }
            let packet = packet.expect("a packet once there is a full FFT");
            old.check(&packet);
            assert_eq!(packet.sample_rate, 48_000);
            assert_eq!(packet.lufs_momentary, -20.0 - k as f32 * 0.01);
            assert_eq!(packet.loudness_epoch, epoch);
            // Flagged on the first packet of each epoch only.
            assert_eq!(packet.loudness_reset, expect_reset, "block {k}");
            expect_reset = false;
            packets += 1;
        }
        assert!(packets > 200, "{packets} packets");

        // No new audio, no packet.
        analysis.tick();
        assert!(slot.lock().is_none());
    }

    #[test]
    fn consumers_count_editors_and_the_suite() {
        let consumers = Consumers::new();
        assert!(!consumers.any());

        let first = consumers.editor_opened();
        let second = consumers.editor_opened();
        assert!(consumers.any());
        drop(first);
        assert!(consumers.any());
        drop(second);
        assert!(!consumers.any());

        consumers.set_suite_connected(true);
        assert!(consumers.any());
        consumers.set_suite_connected(false);
        assert!(!consumers.any());
    }
}
