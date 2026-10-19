//! Hands stereo samples from the audio thread to the analysis thread.
//!
//! One writer (the audio thread) and one reader (the analysis thread) share a
//! fixed ring of frames. Writing never waits and never allocates: a push
//! stores the frames and moves the write position, and when the reader has
//! fallen a whole ring behind the oldest frames are simply overwritten. The
//! reader always gets the most recent frames in order, and is told how many
//! it missed, so it knows when what it reads does not follow on from what it
//! read last time.
//!
//! Positions count frames since the ring was made and never wrap (2^64 frames
//! is millions of years of audio). Samples are kept as `AtomicU32` bit
//! patterns: a relaxed store or load of one compiles to a plain move, and it
//! makes the overwrite case well defined, because the reader can be copying a
//! slot at the moment the writer reuses it.
//!
//! How the reader knows a copy is good, seqlock style:
//!
//! - the writer publishes `claimed` (the position the push will reach), then a
//!   release fence, then the samples, then `written` with release;
//! - the reader loads `written` with acquire (so every sample below it is
//!   visible), copies, issues an acquire fence and loads `claimed`. If any
//!   sample it copied came from a later push, that push's fence synchronises
//!   with the reader's fence, so the reader sees that push's `claimed` and
//!   drops every slot the push may have reached.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

struct Shared {
    left: Box<[AtomicU32]>,
    right: Box<[AtomicU32]>,
    /// Capacity minus one; the capacity is a power of two.
    mask: u64,
    /// End of the last completed push.
    written: AtomicU64,
    /// End of the push in progress (equal to `written` between pushes).
    claimed: AtomicU64,
    /// Position of the last reset: frames before it belong to the
    /// measurement the host threw away.
    reset_at: AtomicU64,
}

impl Shared {
    fn capacity(&self) -> u64 {
        self.mask + 1
    }
}

/// Make a ring of at least `capacity` frames, rounded up to a power of two.
/// Allocates: call it from `initialize`, never from `process`.
pub fn sample_ring(capacity: usize) -> (RingWriter, RingReader) {
    let capacity = capacity.max(1).next_power_of_two();
    let slots = || (0..capacity).map(|_| AtomicU32::new(0)).collect();
    let shared = Arc::new(Shared {
        left: slots(),
        right: slots(),
        mask: capacity as u64 - 1,
        written: AtomicU64::new(0),
        claimed: AtomicU64::new(0),
        reset_at: AtomicU64::new(0),
    });
    (
        RingWriter {
            shared: Arc::clone(&shared),
            position: 0,
        },
        RingReader {
            shared,
            position: 0,
            reset_seen: 0,
            discard_until: 0,
        },
    )
}

/// The audio thread's end. Not `Clone`: there is exactly one writer.
pub struct RingWriter {
    shared: Arc<Shared>,
    /// Same as `shared.written`; only this side changes it.
    position: u64,
}

impl RingWriter {
    /// Append one block. Wait-free and allocation-free. With different
    /// lengths only the frames both channels have are written.
    pub fn push(&mut self, left: &[f32], right: &[f32]) {
        let len = left.len().min(right.len());
        if len == 0 {
            return;
        }
        let shared = &*self.shared;
        let end = self.position + len as u64;
        // A block longer than the ring only leaves its last ring-full; the
        // frames before that count as written and are reported as missed.
        let keep = (len as u64).min(shared.capacity()) as usize;
        let first = end - keep as u64;
        let (left, right) = (&left[len - keep..len], &right[len - keep..len]);

        shared.claimed.store(end, Ordering::Relaxed);
        fence(Ordering::Release);
        for (i, (&l, &r)) in left.iter().zip(right).enumerate() {
            let slot = ((first + i as u64) & shared.mask) as usize;
            shared.left[slot].store(l.to_bits(), Ordering::Relaxed);
            shared.right[slot].store(r.to_bits(), Ordering::Relaxed);
        }
        shared.written.store(end, Ordering::Release);
        self.position = end;
    }

    /// The host reset the plug-in: frames written so far belong to the old
    /// measurement. The reader drops them and starts its analysis afresh.
    pub fn mark_reset(&self) {
        self.shared.reset_at.store(self.position, Ordering::Release);
    }
}

/// What one `RingReader::read` produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read {
    /// Frames copied out, oldest first.
    pub frames: usize,
    /// True when these frames do not follow on from the previous read:
    /// frames were overwritten before they could be read, or the host reset
    /// the plug-in in between. Anything built from earlier frames is stale.
    pub restart: bool,
}

/// The analysis thread's end. Not `Clone`: there is exactly one reader.
pub struct RingReader {
    shared: Arc<Shared>,
    /// Next frame this reader has not seen.
    position: u64,
    /// The last reset position acted on.
    reset_seen: u64,
    /// Frames below this position are from before a reset and are dropped.
    discard_until: u64,
}

impl RingReader {
    /// Frames the ring holds. A read never returns more than this.
    pub fn capacity(&self) -> usize {
        self.shared.capacity() as usize
    }

    /// Copy every frame written since the last read into `left` and `right`
    /// (each at least `capacity()` long), oldest first.
    pub fn read(&mut self, left: &mut [f32], right: &mut [f32]) -> Read {
        let shared = &*self.shared;
        let capacity = shared.capacity();
        assert!(left.len() >= capacity as usize && right.len() >= capacity as usize);

        let written = shared.written.load(Ordering::Acquire);
        let reset_at = shared.reset_at.load(Ordering::Acquire);
        let mut restart = false;
        if reset_at != self.reset_seen {
            self.reset_seen = reset_at;
            self.discard_until = reset_at;
            restart = true;
        }

        let from = self
            .position
            .max(written.saturating_sub(capacity))
            .max(self.discard_until.min(written));
        let count = (written - from) as usize;
        for i in 0..count {
            let slot = ((from + i as u64) & shared.mask) as usize;
            left[i] = f32::from_bits(shared.left[slot].load(Ordering::Relaxed));
            right[i] = f32::from_bits(shared.right[slot].load(Ordering::Relaxed));
        }
        fence(Ordering::Acquire);
        let claimed = shared.claimed.load(Ordering::Relaxed);

        // Slots below `claimed - capacity` may have been reused while they
        // were being copied; keep only what is certainly intact.
        let valid_from = from.max(claimed.saturating_sub(capacity)).min(written);
        let stale = (valid_from - from) as usize;
        if stale > 0 {
            left.copy_within(stale..count, 0);
            right.copy_within(stale..count, 0);
        }

        // Frames between the last read and the first one returned were lost,
        // unless they were dropped on purpose for a reset.
        let expected = self.position.max(self.discard_until.min(written));
        if valid_from > expected {
            restart = true;
        }
        self.position = written;

        Read {
            frames: count - stale,
            restart,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: u32, len: usize) -> (Vec<f32>, Vec<f32>) {
        let left: Vec<f32> = (start..start + len as u32).map(|i| i as f32).collect();
        let right = left.iter().map(|v| -v).collect();
        (left, right)
    }

    fn read_all(reader: &mut RingReader) -> (Vec<f32>, Vec<f32>, Read) {
        let mut left = vec![0.0; reader.capacity()];
        let mut right = vec![0.0; reader.capacity()];
        let read = reader.read(&mut left, &mut right);
        left.truncate(read.frames);
        right.truncate(read.frames);
        (left, right, read)
    }

    #[test]
    fn frames_come_out_in_order() {
        let (mut writer, mut reader) = sample_ring(64);
        let (l, r) = ramp(0, 10);
        writer.push(&l[..4], &r[..4]);
        writer.push(&l[4..], &r[4..]);
        let (left, right, read) = read_all(&mut reader);
        assert_eq!(left, l);
        assert_eq!(right, r);
        assert!(!read.restart);

        let (l, r) = ramp(10, 30);
        writer.push(&l, &r);
        let (left, _, read) = read_all(&mut reader);
        assert_eq!(left, l);
        assert!(!read.restart);

        let (_, _, read) = read_all(&mut reader);
        assert_eq!(read.frames, 0);
        assert!(!read.restart);
    }

    #[test]
    fn capacity_rounds_up_to_a_power_of_two() {
        let (_, reader) = sample_ring(1000);
        assert_eq!(reader.capacity(), 1024);
    }

    #[test]
    fn a_slow_reader_gets_the_newest_frames_and_a_restart() {
        let (mut writer, mut reader) = sample_ring(16);
        let (l, r) = ramp(0, 40);
        for (l, r) in l.chunks(5).zip(r.chunks(5)) {
            writer.push(l, r);
        }
        let (left, right, read) = read_all(&mut reader);
        assert_eq!(left, &l[24..]);
        assert_eq!(right, &r[24..]);
        assert!(read.restart);

        // From here on it follows on again.
        let (l, r) = ramp(40, 3);
        writer.push(&l, &r);
        let (left, _, read) = read_all(&mut reader);
        assert_eq!(left, l);
        assert!(!read.restart);
    }

    #[test]
    fn a_block_longer_than_the_ring_keeps_its_end() {
        let (mut writer, mut reader) = sample_ring(8);
        let (l, r) = ramp(0, 20);
        writer.push(&l, &r);
        let (left, _, read) = read_all(&mut reader);
        assert_eq!(left, &l[12..]);
        assert!(read.restart);
    }

    #[test]
    fn mono_and_mismatched_blocks_write_the_common_length() {
        let (mut writer, mut reader) = sample_ring(8);
        writer.push(&[1.0, 2.0, 3.0], &[4.0, 5.0]);
        writer.push(&[], &[]);
        let (left, right, _) = read_all(&mut reader);
        assert_eq!(left, [1.0, 2.0]);
        assert_eq!(right, [4.0, 5.0]);
    }

    #[test]
    fn a_reset_drops_the_frames_before_it() {
        let (mut writer, mut reader) = sample_ring(64);
        let (l, r) = ramp(0, 10);
        writer.push(&l, &r);
        writer.mark_reset();
        let (l2, r2) = ramp(10, 5);
        writer.push(&l2, &r2);

        let (left, right, read) = read_all(&mut reader);
        assert_eq!(left, l2);
        assert_eq!(right, r2);
        assert!(read.restart);

        // A reset after the last read but before any new frame: nothing to
        // drop, still a restart.
        writer.mark_reset();
        let (l3, r3) = ramp(15, 4);
        writer.push(&l3, &r3);
        let (left, _, read) = read_all(&mut reader);
        assert_eq!(left, l3);
        assert!(read.restart);
    }

    #[test]
    fn a_reset_after_the_read_position_drops_frames_not_read_yet() {
        let (mut writer, mut reader) = sample_ring(64);
        let (l, r) = ramp(0, 6);
        writer.push(&l, &r);
        let (left, _, _) = read_all(&mut reader);
        assert_eq!(left, l);

        // Frames 6..9 are written, then the host resets, then 9..12.
        let (l, r) = ramp(6, 6);
        writer.push(&l[..3], &r[..3]);
        writer.mark_reset();
        writer.push(&l[3..], &r[3..]);
        let (left, _, read) = read_all(&mut reader);
        assert_eq!(left, &l[3..]);
        assert!(read.restart);
    }

    /// The writer laps a small ring over and over while the reader copies.
    /// Every frame the reader is handed must be exactly the frame written at
    /// that position: in order, never torn, never a newer frame in an old
    /// slot. Positions are carried in the samples themselves.
    #[test]
    fn concurrent_reads_never_return_overwritten_frames() {
        stress(64);
    }

    /// The same with a ring the reader mostly keeps up with, so most frames
    /// go through the ordinary path.
    #[test]
    fn concurrent_reads_hand_over_every_frame_when_keeping_up() {
        stress(1 << 16);
    }

    fn stress(capacity: usize) {
        const FRAMES: u32 = 2_000_000;
        let (mut writer, mut reader) = sample_ring(capacity);

        let producer = std::thread::spawn(move || {
            let mut block_l = [0.0_f32; 37];
            let mut block_r = [0.0_f32; 37];
            let mut next = 0_u32;
            while next < FRAMES {
                let len = ((next % 37) + 1).min(FRAMES - next) as usize;
                for i in 0..len {
                    // Exact: every position here is below 2^24.
                    let v = (next + i as u32) as f32;
                    block_l[i] = v;
                    block_r[i] = -v;
                }
                writer.push(&block_l[..len], &block_r[..len]);
                next += len as u32;
            }
        });

        let mut left = vec![0.0; reader.capacity()];
        let mut right = vec![0.0; reader.capacity()];
        let mut last: Option<u32> = None;
        let mut seen = 0_u64;
        let mut restarts = 0_u64;
        // A restart can come with no frames (all of them were overwritten
        // during the copy); it then applies to the next frame handed over.
        let mut restart_pending = false;
        loop {
            let done = producer.is_finished();
            let read = reader.read(&mut left, &mut right);
            restart_pending |= read.restart;
            for i in 0..read.frames {
                let v = left[i] as u32;
                assert_eq!(right[i], -left[i], "torn frame");
                if let Some(prev) = last {
                    if i == 0 && restart_pending {
                        assert!(v > prev, "frames went backwards: {prev} then {v}");
                    } else {
                        assert_eq!(v, prev + 1, "gap without a restart");
                    }
                }
                last = Some(v);
            }
            if read.frames > 0 {
                restart_pending = false;
            }
            seen += read.frames as u64;
            restarts += u64::from(read.restart);
            if done && read.frames == 0 {
                break;
            }
        }
        producer.join().unwrap();
        assert_eq!(last, Some(FRAMES - 1));
        assert!(seen > 0);
        // Not asserted, but useful when this test is run with --nocapture.
        println!("ring of {capacity}: read {seen} of {FRAMES} frames, {restarts} restarts");
    }
}
