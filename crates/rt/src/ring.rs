//! The audio ring between the capture thread (producer) and the sender (consumer).
//!
//! Frames are native-endian stereo `i16` at 44.1 kHz. The producer commits frames
//! and then signals an auto-reset event, so the sender can sleep on the event and
//! wake the moment audio is available. Shared atomics carry the timing facts the
//! sender's stall logic needs (`last_write_ns`) and the counters the UI shows.
//!
//! Overflow policy: when the ring is full new frames are dropped and counted. This
//! only happens while no session is consuming (the ring holds 3 s); the sender
//! clears the ring right before its first packet, so stale audio never plays.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::clock::mono_ns;
use crate::event::AutoEvent;

/// One stereo frame: `[left, right]`.
pub type Frame = [i16; 2];

/// Output sample rate of the ring (the AirPlay stream rate).
pub const SAMPLE_RATE: u32 = 44_100;

/// Default capacity: 3 seconds.
pub const DEFAULT_CAPACITY: usize = SAMPLE_RATE as usize * 3;

/// State shared by both ends (and readable by the engine for statistics).
pub struct RingShared {
    last_write_ns: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    peak_bits: AtomicU32,
    last_chunk: AtomicU32,
    event: AutoEvent,
    capacity: usize,
}

impl RingShared {
    /// [`mono_ns`] time of the most recent producer write (0 = never written).
    pub fn last_write_ns(&self) -> u64 {
        self.last_write_ns.load(Ordering::Acquire)
    }
    /// Total frames accepted into the ring.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }
    /// Frames discarded because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    /// Peak level (0.0..=1.0) of the most recent capture chunk, for the level meter.
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }
    /// Size in frames of the most recent producer write.
    pub fn last_chunk(&self) -> u32 {
        self.last_chunk.load(Ordering::Relaxed)
    }
    /// Event signaled after every producer write.
    pub fn event_handle(&self) -> crate::HANDLE {
        self.event.handle()
    }
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Producer end, owned by the capture (or test-tone) thread.
pub struct RingProducer {
    inner: rtrb::Producer<Frame>,
    shared: Arc<RingShared>,
}

/// Consumer end, owned by the sender thread.
pub struct RingConsumer {
    inner: rtrb::Consumer<Frame>,
    shared: Arc<RingShared>,
}

/// Create a ring holding `capacity_frames` frames.
pub fn ring(capacity_frames: usize) -> std::io::Result<(RingProducer, RingConsumer)> {
    let event = AutoEvent::new().map_err(std::io::Error::other)?;
    let shared = Arc::new(RingShared {
        last_write_ns: AtomicU64::new(0),
        written: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        peak_bits: AtomicU32::new(0f32.to_bits()),
        last_chunk: AtomicU32::new(0),
        event,
        capacity: capacity_frames,
    });
    let (producer, consumer) = rtrb::RingBuffer::new(capacity_frames);
    Ok((
        RingProducer {
            inner: producer,
            shared: shared.clone(),
        },
        RingConsumer {
            inner: consumer,
            shared,
        },
    ))
}

impl RingProducer {
    /// Append frames; returns how many were accepted (the rest were dropped).
    /// Always updates `last_write_ns` and signals the consumer.
    pub fn push(&mut self, frames: &[Frame]) -> usize {
        let accepted = frames.len().min(self.inner.slots());
        if accepted > 0
            && let Ok(chunk) = self.inner.write_chunk_uninit(accepted)
        {
            chunk.fill_from_iter(frames[..accepted].iter().copied());
        }
        let shared = &self.shared;
        if accepted < frames.len() {
            shared
                .dropped
                .fetch_add((frames.len() - accepted) as u64, Ordering::Relaxed);
        }
        shared.written.fetch_add(accepted as u64, Ordering::Relaxed);
        shared
            .last_chunk
            .store(frames.len() as u32, Ordering::Relaxed);
        shared.last_write_ns.store(mono_ns(), Ordering::Release);
        // Commit happened above (fill_from_iter commits); wake the sender after it.
        shared.event.set();
        accepted
    }

    /// Publish the peak level of the latest chunk (0.0..=1.0).
    pub fn set_peak(&self, peak: f32) {
        self.shared
            .peak_bits
            .store(peak.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn shared(&self) -> &Arc<RingShared> {
        &self.shared
    }
}

impl RingConsumer {
    /// Frames currently available.
    pub fn available(&self) -> usize {
        self.inner.slots()
    }

    /// Copy exactly `out.len()` frames out of the ring, or nothing (returns false).
    pub fn pop_exact(&mut self, out: &mut [Frame]) -> bool {
        if self.inner.slots() < out.len() {
            return false;
        }
        match self.inner.read_chunk(out.len()) {
            Ok(chunk) => {
                let (first, second) = chunk.as_slices();
                out[..first.len()].copy_from_slice(first);
                out[first.len()..].copy_from_slice(second);
                chunk.commit_all();
                true
            }
            Err(_) => false,
        }
    }

    /// Drop up to `frames` frames from the front; returns how many were dropped.
    pub fn discard(&mut self, frames: usize) -> usize {
        let n = frames.min(self.inner.slots());
        if n > 0
            && let Ok(chunk) = self.inner.read_chunk(n)
        {
            chunk.commit_all();
        }
        n
    }

    /// Drop everything currently queued; returns how many frames were dropped.
    pub fn clear(&mut self) -> usize {
        let n = self.inner.slots();
        self.discard(n)
    }

    pub fn shared(&self) -> &Arc<RingShared> {
        &self.shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wait::{Wait, wait_any};
    use std::time::Duration;

    fn frames(n: usize, start: i16) -> Vec<Frame> {
        (0..n)
            .map(|i| [start.wrapping_add(i as i16), -start.wrapping_add(i as i16)])
            .collect()
    }

    #[test]
    fn push_then_pop_in_order() {
        let (mut p, mut c) = ring(1000).unwrap();
        assert_eq!(p.push(&frames(300, 1)), 300);
        assert_eq!(c.available(), 300);
        let mut out = [[0i16; 2]; 200];
        assert!(c.pop_exact(&mut out));
        assert_eq!(out[0], [1, -1]);
        assert_eq!(out[199], [200, -200]);
        let mut big = [[0i16; 2]; 200];
        assert!(!c.pop_exact(&mut big), "only 100 left");
        assert_eq!(c.available(), 100);
    }

    #[test]
    fn wraps_around_capacity() {
        let (mut p, mut c) = ring(512).unwrap();
        let mut out = [[0i16; 2]; 352];
        for round in 0..20i16 {
            assert_eq!(p.push(&frames(352, round * 7)), 352);
            assert!(c.pop_exact(&mut out));
            assert_eq!(out[0], [round * 7, -(round * 7)]);
            assert_eq!(out[351][0], round * 7 + 351);
        }
    }

    #[test]
    fn overflow_drops_new_frames_and_counts() {
        let (mut p, c) = ring(100).unwrap();
        assert_eq!(p.push(&frames(80, 0)), 80);
        assert_eq!(p.push(&frames(50, 0)), 20);
        assert_eq!(c.shared().dropped(), 30);
        assert_eq!(c.shared().written(), 100);
        assert_eq!(c.shared().last_chunk(), 50);
    }

    #[test]
    fn discard_and_clear() {
        let (mut p, mut c) = ring(1000).unwrap();
        p.push(&frames(500, 0));
        assert_eq!(c.discard(120), 120);
        assert_eq!(c.available(), 380);
        assert_eq!(c.discard(10_000), 380);
        p.push(&frames(64, 0));
        assert_eq!(c.clear(), 64);
        assert_eq!(c.available(), 0);
    }

    #[test]
    fn push_signals_event_and_stamps_time() {
        crate::clock::init();
        let (mut p, c) = ring(1000).unwrap();
        assert_eq!(c.shared().last_write_ns(), 0);
        let before = mono_ns();
        p.push(&frames(10, 0));
        assert!(c.shared().last_write_ns() >= before);
        let h = [c.shared().event_handle()];
        assert_eq!(
            wait_any(&h, Some(Duration::from_millis(50))).unwrap(),
            Wait::Signaled(0)
        );
    }

    #[test]
    fn producer_and_consumer_on_threads() {
        let (mut p, mut c) = ring(DEFAULT_CAPACITY).unwrap();
        let writer = std::thread::spawn(move || {
            let chunk = frames(220, 0);
            for _ in 0..1000 {
                p.push(&chunk);
            }
            p
        });
        let mut got = 0usize;
        let mut out = [[0i16; 2]; 352];
        let handle = c.shared().event_handle();
        while got < 220 * 1000 - 352 {
            if c.pop_exact(&mut out) {
                got += 352;
            } else {
                let _ = wait_any(&[handle], Some(Duration::from_millis(100)));
                if writer.is_finished() && c.available() < 352 {
                    break;
                }
            }
        }
        let p = writer.join().unwrap();
        assert_eq!(p.shared().dropped(), 0);
        assert!(got + c.available() >= 220 * 1000 - 352);
    }

    #[test]
    fn peak_is_clamped() {
        let (p, c) = ring(10).unwrap();
        p.set_peak(1.7);
        assert_eq!(c.shared().peak(), 1.0);
        p.set_peak(0.25);
        assert_eq!(c.shared().peak(), 0.25);
    }
}
