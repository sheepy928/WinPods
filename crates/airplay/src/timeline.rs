//! Smoothed send timeline (workstream: real-time).
//!
//! Port of `Timeline` in `legacy/python/winpods/raop_live.py`: maps
//! `rt::mono_ns()` to the RTP head of the frames we send:
//! `head_at(t) = r0 + (t - t0)·44100/1e9 + err`, `observe(t, head)` updates
//! `err += 0.005·((head − nominal(t)) − err)` after every packet. Head values are
//! unwrapped u64/f64 (never u32). The sender publishes the state for the control
//! thread through a seqlock/ArcSwap-style shared cell. Fixture: `timeline.json`.
//!
//! The floating-point evaluation order is Python's, so results are bit-identical:
//! `(t − t0)·rate` is an exact integer converted to `f64` once (round to nearest,
//! ties to even, like Python's `int / float`), then divided by `1e9`.

use std::sync::atomic::{AtomicU64, Ordering, fence};

use crate::types::SAMPLE_RATE;

/// Smoothing factor applied per packet (≈1.6 s time constant at 125 packets/s).
pub const TIMELINE_ALPHA: f64 = 0.005;

/// The send timeline: a nominal clock-to-RTP mapping plus a smoothed error term.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeline {
    rate: u32,
    t0: u64,
    r0: f64,
    err: f64,
    started: bool,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    /// An unstarted timeline at the stream rate (44.1 kHz).
    pub const fn new() -> Self {
        Self::with_rate(SAMPLE_RATE)
    }

    /// An unstarted timeline at `rate` frames per second.
    pub const fn with_rate(rate: u32) -> Self {
        Self {
            rate,
            t0: 0,
            r0: 0.0,
            err: 0.0,
            started: false,
        }
    }

    /// Anchor RTP time `rtp` at `rt::mono_ns()` value `t_ns` and reset the error.
    pub fn start(&mut self, t_ns: u64, rtp: u64) {
        self.t0 = t_ns;
        self.r0 = rtp as f64;
        self.err = 0.0;
        self.started = true;
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Clock value the timeline was started at.
    pub fn t0(&self) -> u64 {
        self.t0
    }

    /// RTP time the timeline was started at.
    pub fn r0(&self) -> f64 {
        self.r0
    }

    /// Current smoothed error (frames) between the sent head and the nominal clock.
    pub fn err(&self) -> f64 {
        self.err
    }

    /// Overwrite the error term (diagnostics and tests).
    pub fn set_err(&mut self, err: f64) {
        self.err = err;
    }

    /// `r0 + (t − t0)·rate / 1e9`.
    pub fn nominal(&self, t_ns: u64) -> f64 {
        let elapsed = i128::from(t_ns) - i128::from(self.t0);
        self.r0 + (elapsed * i128::from(self.rate)) as f64 / 1e9
    }

    /// The RTP head the timeline expects at `t_ns` (fractional frames).
    pub fn head_at(&self, t_ns: u64) -> f64 {
        self.nominal(t_ns) + self.err
    }

    /// Record that the frames up to RTP time `head` were sent at `t_ns`.
    pub fn observe(&mut self, t_ns: u64, head: u64) {
        self.err += TIMELINE_ALPHA * ((head as f64 - self.nominal(t_ns)) - self.err);
    }
}

/// Python's `int(round(x))` for a timeline head: round half to even, clamped at 0.
pub fn round_head(head_at: f64) -> u64 {
    let rounded = head_at.round_ties_even();
    if rounded <= 0.0 { 0 } else { rounded as u64 }
}

/// A single-writer seqlock publishing a [`Timeline`] to other threads without
/// blocking the writer. The sender publishes after every packet; the control thread
/// reads it once a second for the anchors.
pub struct SharedTimeline {
    seq: AtomicU64,
    rate: AtomicU64,
    t0: AtomicU64,
    r0: AtomicU64,
    err: AtomicU64,
    started: AtomicU64,
}

impl Default for SharedTimeline {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedTimeline {
    /// A cell holding an unstarted timeline.
    pub fn new() -> Self {
        let cell = Self {
            seq: AtomicU64::new(0),
            rate: AtomicU64::new(0),
            t0: AtomicU64::new(0),
            r0: AtomicU64::new(0),
            err: AtomicU64::new(0),
            started: AtomicU64::new(0),
        };
        cell.publish(&Timeline::new());
        cell
    }

    /// Publish `timeline`. Must only be called from one thread at a time.
    pub fn publish(&self, timeline: &Timeline) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.rate.store(u64::from(timeline.rate), Ordering::Relaxed);
        self.t0.store(timeline.t0, Ordering::Relaxed);
        self.r0.store(timeline.r0.to_bits(), Ordering::Relaxed);
        self.err.store(timeline.err.to_bits(), Ordering::Relaxed);
        self.started
            .store(u64::from(timeline.started), Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// A consistent copy of the last published timeline.
    pub fn load(&self) -> Timeline {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let timeline = Timeline {
                rate: self.rate.load(Ordering::Relaxed) as u32,
                t0: self.t0.load(Ordering::Relaxed),
                r0: f64::from_bits(self.r0.load(Ordering::Relaxed)),
                err: f64::from_bits(self.err.load(Ordering::Relaxed)),
                started: self.started.load(Ordering::Relaxed) != 0,
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return timeline;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Arc;

    fn bits(value: &Value) -> u64 {
        u64::from_str_radix(value.as_str().unwrap(), 16).unwrap()
    }

    #[test]
    fn fixture_trace_is_bit_exact() {
        let fx: Value =
            serde_json::from_str(include_str!("../tests/fixtures/timeline.json")).unwrap();
        assert_eq!(fx["alpha"].as_f64(), Some(TIMELINE_ALPHA));
        let mut timeline = Timeline::with_rate(fx["rate"].as_u64().unwrap() as u32);
        timeline.start(fx["t0"].as_u64().unwrap(), fx["r0"].as_u64().unwrap());
        let steps = fx["steps"].as_array().unwrap();
        let (mut observes, mut queries) = (0, 0);
        for (i, step) in steps.iter().enumerate() {
            let t = step["t"].as_u64().unwrap();
            match step["op"].as_str().unwrap() {
                "observe" => {
                    timeline.observe(t, step["head"].as_u64().unwrap());
                    assert_eq!(
                        timeline.err().to_bits(),
                        bits(&step["err_bits"]),
                        "observe step {i}: {} vs {}",
                        timeline.err(),
                        step["err"]
                    );
                    observes += 1;
                }
                "head_at" => {
                    let value = timeline.head_at(t);
                    assert_eq!(
                        value.to_bits(),
                        bits(&step["value_bits"]),
                        "head_at step {i}: {value} vs {}",
                        step["value"]
                    );
                    queries += 1;
                }
                other => panic!("unknown op {other}"),
            }
        }
        assert_eq!((observes, queries), (320, 107));
    }

    #[test]
    fn round_head_is_half_even() {
        assert_eq!(round_head(92610.5), 92610);
        assert_eq!(round_head(92611.5), 92612);
        assert_eq!(round_head(4294967395.6), 4294967396);
        assert_eq!(round_head(-3.0), 0);
        assert_eq!(round_head(12884901895.0), 12884901895);
    }

    #[test]
    fn start_resets_error_and_nominal_tracks_rate() {
        let mut timeline = Timeline::new();
        assert!(!timeline.is_started());
        timeline.start(1_000, 4410);
        timeline.set_err(3.0);
        timeline.start(2_000_000_000, 4410);
        assert_eq!(timeline.err(), 0.0);
        assert_eq!(timeline.nominal(3_000_000_000), 4410.0 + 44100.0);
        // Before t0 the mapping extrapolates backwards instead of wrapping.
        assert_eq!(timeline.nominal(1_000_000_000), 4410.0 - 44100.0);
    }

    #[test]
    fn shared_timeline_round_trips_and_is_consistent_under_contention() {
        let shared = Arc::new(SharedTimeline::new());
        assert!(!shared.load().is_started());
        let mut timeline = Timeline::new();
        timeline.start(5, 4410);
        shared.publish(&timeline);
        assert_eq!(shared.load(), timeline);
        timeline.start(0, 0);
        shared.publish(&timeline);

        let writer = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut t = Timeline::new();
                for i in 0..20_000u64 {
                    // t0 and err move together so a torn read would be detectable.
                    t.start(i, i);
                    t.set_err(i as f64);
                    shared.publish(&t);
                }
            })
        };
        for _ in 0..20_000 {
            let t = shared.load();
            assert_eq!(t.t0() as f64, t.err());
            assert_eq!(t.r0(), t.err());
        }
        writer.join().unwrap();
    }
}
