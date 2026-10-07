//! Loudness metering per ITU-R BS.1770-4, EBU R128 (Tech 3341) and EBU Tech 3342.
//!
//! Every sample that reaches the plug-in is K-weighted and measured, so the
//! readings do not depend on the DAW buffer size or on how often the window
//! redraws:
//!
//! - momentary: mean energy of the last 400 ms, sliding sample by sample;
//! - short-term: mean energy of the last 3 s, sliding sample by sample;
//! - integrated: 400 ms gating blocks every 100 ms over the whole programme,
//!   with the -70 LUFS absolute gate and the -10 LU relative gate;
//! - loudness range: 3 s short-term blocks every 100 ms over the whole
//!   programme, -70 LUFS absolute and -20 LU relative gate, 95th minus 10th
//!   percentile.
//!
//! Integrated and range keep every block in a histogram of 0.1 LU bins (the
//! libebur128 approach), so memory is fixed however long the programme runs.
//! Each bin also keeps the exact sum of its block energies, so the gated
//! means are not rounded to the bin grid.
//!
//! All memory is allocated in `new` and `set_sample_rate`; `process`,
//! `reset` and `reading` never allocate.

/// Offset in the BS.1770 loudness formula, L = -0.691 + 10 log10(energy).
const LOUDNESS_OFFSET: f64 = -0.691;
/// Absolute gate for integrated loudness and loudness range (LUFS).
const ABSOLUTE_GATE: f64 = -70.0;
/// Relative gate for integrated loudness, as an energy ratio (-10 LU).
const INTEGRATED_RELATIVE_GATE: f64 = 0.1;
/// Relative gate for loudness range, as an energy ratio (-20 LU).
const RANGE_RELATIVE_GATE: f64 = 0.01;
/// Loudness range percentiles (EBU Tech 3342).
const RANGE_LOW_PERCENTILE: f64 = 0.10;
const RANGE_HIGH_PERCENTILE: f64 = 0.95;

/// Histogram bins per LU.
const BINS_PER_LU: f64 = 10.0;
/// Histogram covers -70 to +30 LUFS; louder blocks share the top bin.
const HISTOGRAM_BINS: usize = 1000;

/// 100 ms blocks per momentary window (400 ms).
const MOMENTARY_BLOCKS: usize = 4;
/// 100 ms blocks per short-term window (3 s).
const SHORT_TERM_BLOCKS: usize = 30;

/// Value reported for momentary, short-term and integrated loudness when
/// there is nothing to measure yet or the signal is below this floor.
pub const LOUDNESS_FLOOR: f32 = -100.0;

/// Per-sample energies are summed as integers in units of 2^-48, so the
/// sliding sums add and remove exactly the same amounts and never drift,
/// however long the plug-in runs. The quantum is about -144 dB, far below
/// anything the meter shows.
const ENERGY_SCALE: f64 = (1u64 << 48) as f64;
/// Largest per-sample energy that is counted (+42 dB). Keeps the fixed-point
/// value inside a u64; real programme material is nowhere near it.
const ENERGY_MAX: f64 = (1u64 << 14) as f64;

/// Flush filter state below this to zero, so a decaying tail does not drop
/// into denormals on hosts that do not set flush-to-zero.
const DENORMAL_FLOOR: f64 = 1e-30;

/// One biquad section with a0 normalised to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

/// The two K-weighting stages for one sample rate.
///
/// The design is the one libebur128 uses: the analogue prototypes from
/// BS.1770 mapped with the bilinear transform, K = tan(pi f0 / fs). At 48 kHz
/// it reproduces the coefficients published in BS.1770-4, Tables 1 and 2.
#[derive(Clone, Copy, Debug, PartialEq)]
struct KWeighting {
    /// Stage 1: high-shelf modelling the acoustic effect of the head.
    shelf: Biquad,
    /// Stage 2: RLB high-pass.
    high_pass: Biquad,
}

impl KWeighting {
    fn new(sample_rate: f64) -> Self {
        use std::f64::consts::PI;

        let f0 = 1_681.974_450_955_533;
        let gain_db = 3.999_843_853_973_347;
        let q = 0.707_175_236_955_419_6;
        let k = (PI * f0 / sample_rate).tan();
        let vh = 10.0_f64.powf(gain_db / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        let shelf = Biquad {
            b0: (vh + vb * k / q + k * k) / a0,
            b1: 2.0 * (k * k - vh) / a0,
            b2: (vh - vb * k / q + k * k) / a0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / q + k * k) / a0,
        };

        let f0 = 38.135_470_876_024_44;
        let q = 0.500_327_037_323_877_3;
        let k = (PI * f0 / sample_rate).tan();
        let a0 = 1.0 + k / q + k * k;
        let high_pass = Biquad {
            b0: 1.0,
            b1: -2.0,
            b2: 1.0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / q + k * k) / a0,
        };

        Self { shelf, high_pass }
    }
}

/// Transposed direct form II state of one biquad section.
#[derive(Clone, Copy, Default)]
struct BiquadState {
    s1: f64,
    s2: f64,
}

impl BiquadState {
    #[inline]
    fn tick(&mut self, c: &Biquad, x: f64) -> f64 {
        let y = c.b0 * x + self.s1;
        self.s1 = c.b1 * x - c.a1 * y + self.s2;
        self.s2 = c.b2 * x - c.a2 * y;
        if self.s1.abs() < DENORMAL_FLOOR {
            self.s1 = 0.0;
        }
        if self.s2.abs() < DENORMAL_FLOOR {
            self.s2 = 0.0;
        }
        y
    }
}

/// K-weighting filter state for one channel.
#[derive(Clone, Copy, Default)]
struct ChannelFilter {
    shelf: BiquadState,
    high_pass: BiquadState,
}

impl ChannelFilter {
    /// K-weights one sample and returns its energy (the square).
    /// A non-finite sample is measured as silence so it cannot poison the
    /// filter state or the sums.
    #[inline]
    fn energy(&mut self, k: &KWeighting, x: f32) -> f64 {
        let x = if x.is_finite() { f64::from(x) } else { 0.0 };
        let y = self
            .high_pass
            .tick(&k.high_pass, self.shelf.tick(&k.shelf, x));
        y * y
    }
}

/// Converts a mean energy to loudness (LUFS). Zero gives negative infinity.
#[inline]
fn loudness(energy: f64) -> f64 {
    LOUDNESS_OFFSET + 10.0 * energy.log10()
}

/// Converts a mean energy to a reading for the packet, floored at
/// `LOUDNESS_FLOOR`.
#[inline]
fn reading_from_energy(energy: f64) -> f32 {
    let l = loudness(energy);
    if l > f64::from(LOUDNESS_FLOOR) {
        l as f32
    } else {
        LOUDNESS_FLOOR
    }
}

#[inline]
fn quantise(energy: f64) -> u64 {
    // NaN fails the comparison and counts as zero; `as` saturates.
    if energy > 0.0 {
        (energy.min(ENERGY_MAX) * ENERGY_SCALE) as u64
    } else {
        0
    }
}

/// Block loudness histogram: count and energy sum per 0.1 LU bin.
struct Histogram {
    counts: Vec<u64>,
    energies: Vec<f64>,
}

impl Histogram {
    fn new() -> Self {
        Self {
            counts: vec![0; HISTOGRAM_BINS],
            energies: vec![0.0; HISTOGRAM_BINS],
        }
    }

    fn clear(&mut self) {
        self.counts.fill(0);
        self.energies.fill(0.0);
    }

    /// Adds one block. Blocks at or below the absolute gate are dropped
    /// here, as no measurement ever uses them.
    fn add(&mut self, energy: f64) {
        let l = loudness(energy);
        if l.is_nan() || l <= ABSOLUTE_GATE {
            return;
        }
        // Float-to-int `as` saturates, and `min` keeps very loud blocks in
        // the top bin.
        let bin = (((l - ABSOLUTE_GATE) * BINS_PER_LU) as usize).min(HISTOGRAM_BINS - 1);
        if let (Some(count), Some(sum)) = (self.counts.get_mut(bin), self.energies.get_mut(bin)) {
            *count += 1;
            *sum += energy;
        }
    }

    /// Bins whose mean block energy is above `threshold`, as
    /// (count, energy sum, mean energy).
    fn bins_above(&self, threshold: f64) -> impl Iterator<Item = (u64, f64, f64)> + '_ {
        self.counts
            .iter()
            .zip(&self.energies)
            .filter(|&(&n, _)| n > 0)
            .map(|(&n, &e)| (n, e, e / n as f64))
            .filter(move |&(_, _, mean)| mean > threshold)
    }

    /// Mean energy of all blocks above the relative gate, which sits
    /// `relative_gate` (an energy ratio) below the mean of the absolute-gated
    /// blocks. None when no block passes.
    fn gated_mean(&self, relative_gate: f64) -> Option<(f64, f64)> {
        let (n, e) = self
            .bins_above(0.0)
            .fold((0, 0.0), |(n, e), (bn, be, _)| (n + bn, e + be));
        if n == 0 {
            return None;
        }
        let gate = e / n as f64 * relative_gate;
        let (n, e) = self
            .bins_above(gate)
            .fold((0, 0.0), |(n, e), (bn, be, _)| (n + bn, e + be));
        if n == 0 {
            return None;
        }
        Some((e / n as f64, gate))
    }

    /// Integrated loudness (LUFS) of the 400 ms gating blocks.
    fn integrated(&self) -> f32 {
        match self.gated_mean(INTEGRATED_RELATIVE_GATE) {
            Some((mean, _)) => reading_from_energy(mean),
            None => LOUDNESS_FLOOR,
        }
    }

    /// Loudness range (LU) of the short-term blocks, per EBU Tech 3342.
    fn range(&self) -> f32 {
        let Some((_, gate)) = self.gated_mean(RANGE_RELATIVE_GATE) else {
            return 0.0;
        };
        let count: u64 = self.bins_above(gate).map(|(n, _, _)| n).sum();
        let Some(last) = count.checked_sub(1) else {
            return 0.0;
        };
        let low_rank = (last as f64 * RANGE_LOW_PERCENTILE).round() as u64;
        let high_rank = (last as f64 * RANGE_HIGH_PERCENTILE).round() as u64;

        let mut low = None;
        let mut high = None;
        let mut seen = 0;
        for (n, _, mean) in self.bins_above(gate) {
            seen += n;
            if low.is_none() && seen > low_rank {
                low = Some(loudness(mean));
            }
            if seen > high_rank {
                high = Some(loudness(mean));
                break;
            }
        }
        match (low, high) {
            (Some(low), Some(high)) => (high - low).max(0.0) as f32,
            _ => 0.0,
        }
    }
}

/// One loudness reading, as sent to the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessReading {
    /// Momentary loudness (400 ms), LUFS.
    pub momentary: f32,
    /// Short-term loudness (3 s), LUFS.
    pub short_term: f32,
    /// Integrated loudness since the last reset, LUFS.
    pub integrated: f32,
    /// Loudness range since the last reset, LU.
    pub range: f32,
    /// True for the first reading taken after a reset.
    pub reset: bool,
    /// Counts resets, so a reader that missed the flag still sees one.
    pub epoch: u32,
}

/// Streaming BS.1770 / EBU R128 loudness meter for mono or stereo input.
pub struct LoudnessMeter {
    k: KWeighting,
    filters: [ChannelFilter; 2],

    /// Weighted energy of each of the last 3 s of samples, fixed point.
    ring: Vec<u64>,
    /// Next write position in `ring`.
    pos: usize,
    /// Samples per 100 ms block.
    block_len: usize,
    /// Samples per momentary window.
    momentary_len: usize,
    /// Samples written into the current 100 ms block.
    block_fill: usize,
    /// 100 ms blocks completed since the last reset.
    blocks: u64,
    /// Sum of the last `momentary_len` ring entries.
    momentary_sum: u128,
    /// Sum of every ring entry (the last 3 s).
    short_term_sum: u128,

    /// 400 ms gating blocks for integrated loudness.
    gating_blocks: Histogram,
    /// 3 s short-term blocks for loudness range.
    short_term_blocks: Histogram,
    integrated: f32,
    range: f32,

    epoch: u32,
    reset_pending: bool,
}

impl LoudnessMeter {
    pub fn new(sample_rate: f32) -> Self {
        let mut meter = Self {
            k: KWeighting::new(48_000.0),
            filters: [ChannelFilter::default(); 2],
            ring: Vec::new(),
            pos: 0,
            block_len: 1,
            momentary_len: MOMENTARY_BLOCKS,
            block_fill: 0,
            blocks: 0,
            momentary_sum: 0,
            short_term_sum: 0,
            gating_blocks: Histogram::new(),
            short_term_blocks: Histogram::new(),
            integrated: LOUDNESS_FLOOR,
            range: 0.0,
            epoch: 0,
            reset_pending: false,
        };
        meter.set_sample_rate(sample_rate);
        meter
    }

    /// Designs the filters and sizes the 3 s window for a sample rate, then
    /// resets the measurement. Allocates when the window grows: call it from
    /// `initialize`, not from `process`.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        // A host never sends a rate this low; the guard keeps the filter
        // design and the block length well defined if one ever did.
        let sample_rate = if sample_rate.is_finite() {
            f64::from(sample_rate).max(1_000.0)
        } else {
            48_000.0
        };
        self.k = KWeighting::new(sample_rate);
        self.block_len = ((sample_rate / 10.0).round() as usize).max(1);
        self.momentary_len = self.block_len * MOMENTARY_BLOCKS;
        self.ring.clear();
        self.ring.resize(self.block_len * SHORT_TERM_BLOCKS, 0);
        self.reset();
    }

    /// Starts a new measurement. Does not allocate.
    pub fn reset(&mut self) {
        self.filters = [ChannelFilter::default(); 2];
        self.ring.fill(0);
        self.pos = 0;
        self.block_fill = 0;
        self.blocks = 0;
        self.momentary_sum = 0;
        self.short_term_sum = 0;
        self.gating_blocks.clear();
        self.short_term_blocks.clear();
        self.integrated = LOUDNESS_FLOOR;
        self.range = 0.0;
        self.epoch = self.epoch.wrapping_add(1);
        self.reset_pending = true;
    }

    /// Measures a run of samples. Pass `None` for `right` on a mono track:
    /// BS.1770 weights a mono channel once, so it is not counted twice.
    /// With two channels, only the samples both slices have are measured.
    pub fn process(&mut self, left: &[f32], right: Option<&[f32]>) {
        match right {
            Some(right) => {
                for (&l, &r) in left.iter().zip(right) {
                    let [fl, fr] = &mut self.filters;
                    let energy = fl.energy(&self.k, l) + fr.energy(&self.k, r);
                    self.push(energy);
                }
            }
            None => {
                for &l in left {
                    let energy = self.filters[0].energy(&self.k, l);
                    self.push(energy);
                }
            }
        }
    }

    /// Adds the weighted energy of one sample frame.
    #[inline]
    fn push(&mut self, energy: f64) {
        let q = quantise(energy);
        let len = self.ring.len();
        // The window always holds whole blocks, so len >= momentary_len.
        let momentary_out = if self.pos >= self.momentary_len {
            self.pos - self.momentary_len
        } else {
            self.pos + len - self.momentary_len
        };
        let leaving_momentary = self.ring.get(momentary_out).copied().unwrap_or(0);
        let Some(slot) = self.ring.get_mut(self.pos) else {
            return;
        };
        let leaving_short_term = std::mem::replace(slot, q);

        self.momentary_sum =
            (self.momentary_sum + u128::from(q)).saturating_sub(u128::from(leaving_momentary));
        self.short_term_sum =
            (self.short_term_sum + u128::from(q)).saturating_sub(u128::from(leaving_short_term));

        self.pos += 1;
        if self.pos >= len {
            self.pos = 0;
        }
        self.block_fill += 1;
        if self.block_fill >= self.block_len {
            self.block_fill = 0;
            self.end_block();
        }
    }

    /// Every 100 ms: one gating block and one short-term block, once their
    /// windows have filled since the reset.
    fn end_block(&mut self) {
        self.blocks = self.blocks.saturating_add(1);
        if self.blocks >= MOMENTARY_BLOCKS as u64 {
            self.gating_blocks.add(self.momentary_energy());
            self.integrated = self.gating_blocks.integrated();
        }
        if self.blocks >= SHORT_TERM_BLOCKS as u64 {
            self.short_term_blocks.add(self.short_term_energy());
            self.range = self.short_term_blocks.range();
        }
    }

    fn momentary_energy(&self) -> f64 {
        self.momentary_sum as f64 / ENERGY_SCALE / self.momentary_len as f64
    }

    fn short_term_energy(&self) -> f64 {
        self.short_term_sum as f64 / ENERGY_SCALE / self.ring.len().max(1) as f64
    }

    /// The current values. The reset flag is reported once.
    pub fn reading(&mut self) -> LoudnessReading {
        let reset = std::mem::take(&mut self.reset_pending);
        LoudnessReading {
            momentary: reading_from_energy(self.momentary_energy()),
            short_term: reading_from_energy(self.short_term_energy()),
            integrated: self.integrated,
            range: self.range,
            reset,
            epoch: self.epoch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Sine at `freq` Hz, peak amplitude `dbfs`, starting at `start` samples.
    fn sine(sample_rate: f32, freq: f64, dbfs: f64, start: usize, len: usize) -> Vec<f32> {
        let amp = 10.0_f64.powf(dbfs / 20.0);
        let sr = f64::from(sample_rate);
        (start..start + len)
            .map(|i| (amp * (2.0 * PI * freq * i as f64 / sr).sin()) as f32)
            .collect()
    }

    fn seconds(sample_rate: f32, s: f64) -> usize {
        (f64::from(sample_rate) * s).round() as usize
    }

    /// Feeds `left` (and `right`, if given) in host-sized buffers.
    fn feed(meter: &mut LoudnessMeter, left: &[f32], right: Option<&[f32]>, buffer: usize) {
        match right {
            Some(right) => {
                for (l, r) in left.chunks(buffer).zip(right.chunks(buffer)) {
                    meter.process(l, Some(r));
                }
            }
            None => {
                for l in left.chunks(buffer) {
                    meter.process(l, None);
                }
            }
        }
    }

    /// A stereo 1 kHz sine, the same in both channels, at the given levels
    /// for the given durations.
    fn levels(sample_rate: f32, parts: &[(f64, f64)]) -> Vec<f32> {
        let mut out = Vec::new();
        for &(dbfs, secs) in parts {
            out.extend(sine(
                sample_rate,
                1000.0,
                dbfs,
                out.len(),
                seconds(sample_rate, secs),
            ));
        }
        out
    }

    #[test]
    fn k_weighting_reproduces_published_48k_coefficients() {
        let k = KWeighting::new(48_000.0);
        // ITU-R BS.1770-4, Annex 1, Tables 1 and 2.
        let shelf = [
            (k.shelf.b0, 1.535_124_859_586_97),
            (k.shelf.b1, -2.691_696_189_406_38),
            (k.shelf.b2, 1.198_392_810_852_85),
            (k.shelf.a1, -1.690_659_293_182_41),
            (k.shelf.a2, 0.732_480_774_215_85),
        ];
        let high_pass = [
            (k.high_pass.b0, 1.0),
            (k.high_pass.b1, -2.0),
            (k.high_pass.b2, 1.0),
            (k.high_pass.a1, -1.990_047_454_833_98),
            (k.high_pass.a2, 0.990_072_250_366_21),
        ];
        for (got, published) in shelf.iter().chain(&high_pass) {
            // The table is printed to 14 decimals.
            assert!(
                (got - published).abs() < 1e-13,
                "coefficient {got} published {published}"
            );
        }
    }

    #[test]
    fn stereo_sine_reads_minus_20_at_every_rate_and_buffer_size() {
        for sr in [44_100.0, 48_000.0, 96_000.0] {
            let signal = sine(sr, 1000.0, -20.0, 0, seconds(sr, 5.0));
            for buffer in [64, 512, 2048] {
                let mut meter = LoudnessMeter::new(sr);
                feed(&mut meter, &signal, Some(&signal), buffer);
                let r = meter.reading();
                for (name, value) in [
                    ("momentary", r.momentary),
                    ("short-term", r.short_term),
                    ("integrated", r.integrated),
                ] {
                    assert!(
                        (value + 20.0).abs() <= 0.1,
                        "{name} {value} at {sr} Hz, buffer {buffer}"
                    );
                }
            }
        }
    }

    #[test]
    fn mono_sine_is_weighted_once() {
        for sr in [44_100.0, 48_000.0, 96_000.0] {
            let signal = sine(sr, 1000.0, -20.0, 0, seconds(sr, 5.0));
            let mut meter = LoudnessMeter::new(sr);
            feed(&mut meter, &signal, None, 512);
            let r = meter.reading();
            assert!(
                (r.integrated + 23.0).abs() <= 0.1,
                "integrated {} at {sr}",
                r.integrated
            );
            assert!(
                (r.short_term + 23.0).abs() <= 0.1,
                "short-term {} at {sr}",
                r.short_term
            );
        }
    }

    #[test]
    fn momentary_and_short_term_settle_on_time() {
        for sr in [44_100.0, 48_000.0, 96_000.0] {
            // The step lands in the middle of a 100 ms block, so the windows
            // must slide by the sample, not by the block.
            let before = seconds(sr, 4.037);
            let mut meter = LoudnessMeter::new(sr);
            let quiet = sine(sr, 1000.0, -40.0, 0, before);
            feed(&mut meter, &quiet, Some(&quiet), 512);

            let loud = sine(sr, 1000.0, -20.0, before, seconds(sr, 3.0));
            let (first, rest) = loud.split_at(seconds(sr, 0.4));
            feed(&mut meter, first, Some(first), 512);
            let m = meter.reading().momentary;
            assert!(
                (m + 20.0).abs() <= 0.1,
                "momentary {m} 0.4 s after the step at {sr}"
            );

            feed(&mut meter, rest, Some(rest), 512);
            let s = meter.reading().short_term;
            assert!(
                (s + 20.0).abs() <= 0.1,
                "short-term {s} 3.0 s after the step at {sr}"
            );
        }
    }

    #[test]
    fn integrated_two_level_signal_matches_reference() {
        // 10 s at -20 LUFS, then 10 s at -30 LUFS, stereo, 48 kHz.
        // `ffmpeg -i two_level.wav -af ebur128 -f null -` reports I: -22.6 LUFS:
        // the relative gate (-32.6) keeps every block, so the result is the
        // mean energy of all 197 gating blocks.
        const REFERENCE: f32 = -22.6;
        let sr = 48_000.0;
        let signal = levels(sr, &[(-20.0, 10.0), (-30.0, 10.0)]);
        let mut meter = LoudnessMeter::new(sr);
        feed(&mut meter, &signal, Some(&signal), 512);
        let i = meter.reading().integrated;
        assert!((i - REFERENCE).abs() <= 0.1, "integrated {i}");
    }

    #[test]
    fn integrated_applies_both_gates() {
        // -20 LUFS for 10 s, -40 LUFS for 10 s, then silence: the quiet part is
        // below the -10 LU relative gate and silence is below -70 LUFS, so
        // integrated reads the loud part alone.
        let sr = 48_000.0;
        let mut signal = levels(sr, &[(-20.0, 10.0), (-40.0, 10.0)]);
        signal.extend(std::iter::repeat(0.0).take(seconds(sr, 10.0)));
        let mut meter = LoudnessMeter::new(sr);
        feed(&mut meter, &signal, Some(&signal), 512);
        let i = meter.reading().integrated;
        assert!((i + 20.0).abs() <= 0.1, "integrated {i}");
    }

    #[test]
    fn integrated_covers_the_whole_programme() {
        // A loud first minute is still counted after 15 quiet minutes. A low
        // rate keeps the test quick; 1 kHz fits whole cycles into a second, so
        // the quiet second repeats seamlessly.
        let sr = 8_000.0;
        let mut meter = LoudnessMeter::new(sr);
        let loud = sine(sr, 1000.0, -20.0, 0, seconds(sr, 60.0));
        feed(&mut meter, &loud, Some(&loud), 2048);
        let loud_only = f64::from(meter.reading().integrated);
        let quiet = sine(sr, 1000.0, -26.0, 0, seconds(sr, 1.0));
        for _ in 0..900 {
            feed(&mut meter, &quiet, Some(&quiet), 2048);
        }
        // Mean energy of 60 s at the loud level and 900 s 6 dB lower: 5.6 LU
        // below the loud part. A meter that forgot the start would read 6 LU
        // below it.
        let expected = loud_only + 10.0 * ((60.0 + 900.0 * 10f64.powf(-0.6)) / 960.0).log10();
        let i = meter.reading().integrated;
        assert!(
            (f64::from(i) - expected).abs() <= 0.1,
            "integrated {i} expected {expected}"
        );
    }

    #[test]
    fn loudness_range_of_ebu_3342_case_1() {
        // EBU Tech 3342 test signal 1: 20 s at -20 LUFS, then 20 s at -30 LUFS.
        for sr in [44_100.0, 48_000.0, 96_000.0] {
            let signal = levels(sr, &[(-20.0, 20.0), (-30.0, 20.0)]);
            let mut meter = LoudnessMeter::new(sr);
            feed(&mut meter, &signal, Some(&signal), 512);
            let lra = meter.reading().range;
            assert!((lra - 10.0).abs() <= 1.0, "LRA {lra} at {sr}");
        }
    }

    #[test]
    fn loudness_range_of_steady_signal_is_zero() {
        let sr = 48_000.0;
        let signal = levels(sr, &[(-20.0, 10.0)]);
        let mut meter = LoudnessMeter::new(sr);
        feed(&mut meter, &signal, Some(&signal), 512);
        assert!(meter.reading().range.abs() < 0.1);
    }

    #[test]
    fn short_term_and_range_appear_at_high_sample_rates() {
        for sr in [88_200.0, 96_000.0, 192_000.0] {
            let signal = levels(sr, &[(-20.0, 4.0), (-30.0, 4.0)]);
            let mut meter = LoudnessMeter::new(sr);
            feed(&mut meter, &signal, Some(&signal), 2048);
            let r = meter.reading();
            assert!(
                r.short_term > -31.0 && r.short_term < -29.0,
                "short-term {} at {sr}",
                r.short_term
            );
            assert!(r.range > 1.0, "LRA {} at {sr}", r.range);
        }
    }

    #[test]
    fn silence_and_empty_meter_read_the_floor() {
        let mut meter = LoudnessMeter::new(48_000.0);
        let r = meter.reading();
        assert_eq!(r.momentary, LOUDNESS_FLOOR);
        assert_eq!(r.integrated, LOUDNESS_FLOOR);
        assert_eq!(r.range, 0.0);

        let silence = vec![0.0; 48_000 * 4];
        feed(&mut meter, &silence, Some(&silence), 512);
        let r = meter.reading();
        assert_eq!(r.momentary, LOUDNESS_FLOOR);
        assert_eq!(r.short_term, LOUDNESS_FLOOR);
        assert_eq!(r.integrated, LOUDNESS_FLOOR);
    }

    #[test]
    fn reset_flag_is_reported_once_and_clears_the_measurement() {
        let sr = 48_000.0;
        let mut meter = LoudnessMeter::new(sr);
        let first = meter.reading();
        assert!(first.reset);
        assert!(!meter.reading().reset);

        let signal = levels(sr, &[(-20.0, 2.0)]);
        feed(&mut meter, &signal, Some(&signal), 512);
        assert!(meter.reading().integrated > -21.0);

        meter.reset();
        let r = meter.reading();
        assert!(r.reset);
        assert_ne!(r.epoch, first.epoch);
        assert_eq!(r.integrated, LOUDNESS_FLOOR);
        assert_eq!(r.momentary, LOUDNESS_FLOOR);
    }

    #[test]
    fn extreme_input_stays_finite() {
        let sr = 48_000.0;
        let mut meter = LoudnessMeter::new(sr);
        let mut signal = sine(sr, 1000.0, -20.0, 0, seconds(sr, 1.0));
        signal[100] = f32::NAN;
        signal[200] = f32::INFINITY;
        signal[300] = f32::NEG_INFINITY;
        signal[400] = f32::MAX;
        signal[500] = 1e-40; // denormal
        feed(&mut meter, &signal, Some(&signal), 64);
        let r = meter.reading();
        for v in [r.momentary, r.short_term, r.integrated, r.range] {
            assert!(v.is_finite(), "{r:?}");
        }

        // The filters recover: a clean second afterwards reads normally.
        let clean = sine(sr, 1000.0, -20.0, 0, seconds(sr, 1.0));
        feed(&mut meter, &clean, Some(&clean), 64);
        let m = meter.reading().momentary;
        assert!((m + 20.0).abs() <= 0.1, "momentary {m}");
    }

    #[test]
    fn sliding_sums_do_not_drift() {
        // Loud then silent for a long time: the momentary sum returns to
        // exactly zero.
        let sr = 8_000.0;
        let mut meter = LoudnessMeter::new(sr);
        let loud = sine(sr, 1000.0, 0.0, 0, seconds(sr, 120.0));
        feed(&mut meter, &loud, Some(&loud), 2048);
        let silence = vec![0.0; seconds(sr, 5.0)];
        feed(&mut meter, &silence, Some(&silence), 2048);
        assert_eq!(meter.momentary_sum, 0);
        assert_eq!(meter.short_term_sum, 0);
    }
}
