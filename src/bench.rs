//! What the plug-in costs the DAW's audio thread, measured on the real `process()` path.
//!
//! ```text
//! cargo test --release audio_thread_cost -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Plays 10 s of 48 kHz stereo (noise plus a 1 kHz sine) through the plug-in in 256-sample
//! blocks and adds up the CPU time spent inside `process()`. The blocks are paced in real time,
//! the way a DAW calls the plug-in, so any thread the plug-in runs next to the audio thread
//! competes for the CPU as it would in a session. Each case runs three times and the median is
//! reported.
//!
//! CPU time rather than wall-clock time: on a machine that is busy with something else, the
//! audio thread waits for a core now and then, and wall-clock time would count that wait as the
//! plug-in's cost.
//!
//! Ignored by default: it takes about a minute and a half and its numbers depend on the machine.

use super::*;
use std::thread;
use std::time::Duration;

const SAMPLE_RATE: f32 = 48_000.0;
const BLOCK: usize = 256;
const SECONDS: usize = 10;
const RUNS: usize = 3;

/// The host side of `initialize()` and `process()`. The Analyser asks it for nothing.
struct Host;

impl InitContext<HardwaveAnalyser> for Host {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }
    fn execute(&self, _task: ()) {}
    fn set_latency_samples(&self, _samples: u32) {}
    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

impl ProcessContext<HardwaveAnalyser> for Host {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }
    fn execute_background(&self, _task: ()) {}
    fn execute_gui(&self, _task: ()) {}
    fn transport(&self) -> &Transport {
        unreachable!("the Analyser does not read the transport")
    }
    fn next_event(&mut self) -> Option<PluginNoteEvent<HardwaveAnalyser>> {
        None
    }
    fn send_event(&mut self, _event: PluginNoteEvent<HardwaveAnalyser>) {}
    fn set_latency_samples(&self, _samples: u32) {}
    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

/// 10 s of stereo: white noise at about -15 dBFS RMS in each channel (different noise left and
/// right) plus a 1 kHz sine at -6 dBFS in both.
fn test_audio() -> (Vec<f32>, Vec<f32>) {
    let len = SECONDS * SAMPLE_RATE as usize;
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };
    let mut left = Vec::with_capacity(len);
    let mut right = Vec::with_capacity(len);
    for i in 0..len {
        let sine = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SAMPLE_RATE).sin();
        left.push(sine + 0.3 * noise());
        right.push(sine + 0.3 * noise());
    }
    (left, right)
}

struct Case {
    name: &'static str,
    refresh_rate: i32,
    editor_open: bool,
}

const CASES: [Case; 3] = [
    Case {
        name: "editor open, 60 Hz",
        refresh_rate: 60,
        editor_open: true,
    },
    Case {
        name: "editor closed, no Suite",
        refresh_rate: 144,
        editor_open: false,
    },
    Case {
        name: "editor open, 144 Hz",
        refresh_rate: 144,
        editor_open: true,
    },
];

/// Stands in for the plug-in window: takes the latest packet from the slot at the refresh rate
/// and turns it into JSON, as the Linux and macOS editors do.
struct FakeEditor {
    running: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeEditor {
    fn open(plugin: &HardwaveAnalyser, refresh_rate: i32) -> Self {
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let slot = Arc::clone(&plugin.packet_slot);
        let thread = thread::spawn({
            let running = Arc::clone(&running);
            move || {
                let interval = Duration::from_secs_f64(1.0 / f64::from(refresh_rate));
                while running.load(Ordering::Relaxed) {
                    if let Some(packet) = slot.lock().take() {
                        std::hint::black_box(serde_json::to_string(&*packet).unwrap_or_default());
                    }
                    thread::sleep(interval);
                }
            }
        });
        Self {
            running,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeEditor {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// CPU time this thread has used so far.
#[cfg(unix)]
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// No thread CPU clock here: fall back to wall-clock time.
#[cfg(not(unix))]
fn thread_cpu() -> Duration {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// What reading the clock twice costs by itself, taken off every measurement.
fn clock_overhead() -> Duration {
    let mut pairs: Vec<Duration> = (0..2_000)
        .map(|_| {
            let t = thread_cpu();
            thread_cpu().saturating_sub(t)
        })
        .collect();
    pairs.sort();
    pairs[pairs.len() / 2]
}

/// CPU time used so far by the plug-in's analysis thread, if it has one (Linux only: read from
/// /proc, where the thread shows up under the name the plug-in gives it).
fn analysis_thread_cpu() -> Option<Duration> {
    let tasks = std::fs::read_dir("/proc/self/task").ok()?;
    for task in tasks.flatten() {
        let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        if comm.trim() == "hw-analysis" {
            let schedstat = std::fs::read_to_string(task.path().join("schedstat")).ok()?;
            let nanos: u64 = schedstat.split_whitespace().next()?.parse().ok()?;
            return Some(Duration::from_nanos(nanos));
        }
    }
    None
}

struct Run {
    /// CPU time inside process() per second of audio.
    audio: Duration,
    /// CPU time of the slowest single process() call.
    worst_block: Duration,
    /// CPU time of the analysis thread per second of audio, where there is one.
    analysis: Option<Duration>,
}

fn run(case: &Case, left: &[f32], right: &[f32], overhead: Duration) -> Run {
    let mut params = HardwaveAnalyserParams::default();
    params.refresh_rate = IntParam::new(
        "Refresh Rate",
        case.refresh_rate,
        IntRange::Linear { min: 60, max: 144 },
    );

    let mut plugin = HardwaveAnalyser::default();
    plugin.params = Arc::new(params);
    let layout = HardwaveAnalyser::AUDIO_IO_LAYOUTS[0];
    let config = BufferConfig {
        sample_rate: SAMPLE_RATE,
        min_buffer_size: None,
        max_buffer_size: BLOCK as u32,
        process_mode: ProcessMode::Realtime,
    };
    assert!(plugin.initialize(&layout, &config, &mut Host));
    plugin.reset();
    let editor = case
        .editor_open
        .then(|| FakeEditor::open(&plugin, case.refresh_rate));

    let mut block_left = vec![0.0_f32; BLOCK];
    let mut block_right = vec![0.0_f32; BLOCK];
    let mut busy = Duration::ZERO;
    let mut worst_block = Duration::ZERO;
    let block_time = Duration::from_secs_f64(BLOCK as f64 / f64::from(SAMPLE_RATE));
    let start = Instant::now();
    for (k, (l, r)) in left.chunks(BLOCK).zip(right.chunks(BLOCK)).enumerate() {
        let due = start + block_time * k as u32;
        let now = Instant::now();
        if due > now {
            thread::sleep(due - now);
        }

        block_left[..l.len()].copy_from_slice(l);
        block_right[..r.len()].copy_from_slice(r);
        let mut buffer = Buffer::default();
        unsafe {
            buffer.set_slices(l.len(), |slices| {
                slices.clear();
                slices.push(&mut block_left[..l.len()]);
                slices.push(&mut block_right[..r.len()]);
            });
        }
        let mut aux = AuxiliaryBuffers {
            inputs: &mut [],
            outputs: &mut [],
        };

        let t = thread_cpu();
        plugin.process(&mut buffer, &mut aux, &mut Host);
        let dt = thread_cpu().saturating_sub(t).saturating_sub(overhead);

        busy += dt;
        worst_block = worst_block.max(dt);
    }
    let analysis = analysis_thread_cpu();

    drop(editor);
    plugin.deactivate();
    drop(plugin);

    Run {
        audio: busy / SECONDS as u32,
        worst_block,
        analysis: analysis.map(|cpu| cpu / SECONDS as u32),
    }
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values[values.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[test]
#[ignore = "benchmark: run with --ignored --nocapture, takes about 90 s"]
fn audio_thread_cost() {
    let (left, right) = test_audio();
    let overhead = clock_overhead();
    let block_ms = BLOCK as f64 / f64::from(SAMPLE_RATE) * 1000.0;

    println!();
    println!(
        "{SECONDS} s of 48 kHz stereo in {BLOCK}-sample blocks ({block_ms:.2} ms each), \
         median of {RUNS} runs, CPU time"
    );
    println!(
        "{:<26} {:>14} {:>12} {:>14} {:>18}",
        "case", "audio ms/s", "% of a core", "worst block", "analysis ms/s"
    );
    for case in &CASES {
        let runs: Vec<Run> = (0..RUNS)
            .map(|_| run(case, &left, &right, overhead))
            .collect();
        let audio = median(runs.iter().map(|r| r.audio).collect());
        let worst = median(runs.iter().map(|r| r.worst_block).collect());
        let analysis = if runs.iter().all(|r| r.analysis.is_some()) {
            format!(
                "{:.2}",
                ms(median(runs.iter().filter_map(|r| r.analysis).collect()))
            )
        } else {
            "-".to_string()
        };
        println!(
            "{:<26} {:>14.3} {:>11.2}% {:>11.0} us {:>18}",
            case.name,
            ms(audio),
            ms(audio) / 10.0,
            worst.as_secs_f64() * 1e6,
            analysis,
        );
    }
}
