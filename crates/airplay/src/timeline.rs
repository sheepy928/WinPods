//! The stream's RTP timeline (workstream: real-time).
//!
//! Maps the timing master's clock to RTP time at exactly the stream rate:
//! `head_at(m) = r0 + (m − m0)·44100/1e9`, where `m` is the master's time in ns: the
//! followed PTP master's with PTP, or `rt::mono_ns()` with NTP (the receiver follows
//! our NTP clock, which runs on it). It is fixed when the stream starts. Anchors are
//! read off it ([`crate::control`]), so every anchor lies on the same line, and the
//! sender keeps the audio on it: silence when capture stalls, and otherwise a rate
//! adjustment of the capture ([`crate::servo`]).
//!
//! The receiver plays 44 100 frames per second of master time. v0.2.0 and the Python
//! version instead smoothed the timeline toward the audio actually sent, so the
//! anchors followed the capture clock: with VB-Audio Cable running 22 ppm fast, the
//! HomePod played about 80 ms later for every hour a stream ran, and a second late by
//! the next morning. Head values are unwrapped u64/f64 (never u32).
//!
//! `(m − m0)·rate` is an exact integer converted to `f64` once (round to nearest, ties
//! to even, like Python's `int / float`), then divided by `1e9`.

use crate::types::SAMPLE_RATE;

/// A fixed master-clock-to-RTP mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timeline {
    rate: u32,
    m0: u64,
    r0: f64,
}

impl Timeline {
    /// RTP time `r0` at master time `m0_ns`, at the stream rate (44.1 kHz).
    pub const fn new(m0_ns: u64, r0: u64) -> Self {
        Self::with_rate(SAMPLE_RATE, m0_ns, r0)
    }

    /// RTP time `r0` at master time `m0_ns`, at `rate` frames per second.
    pub const fn with_rate(rate: u32, m0_ns: u64, r0: u64) -> Self {
        Self {
            rate,
            m0: m0_ns,
            r0: r0 as f64,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Master time the timeline starts at.
    pub fn m0(&self) -> u64 {
        self.m0
    }

    /// RTP time at [`m0`](Self::m0).
    pub fn r0(&self) -> f64 {
        self.r0
    }

    /// The RTP head (fractional frames) at master time `m_ns`:
    /// `r0 + (m − m0)·rate / 1e9`. Before `m0` it extrapolates backwards.
    pub fn head_at(&self, m_ns: u64) -> f64 {
        let elapsed = i128::from(m_ns) - i128::from(self.m0);
        self.r0 + (elapsed * i128::from(self.rate)) as f64 / 1e9
    }
}

/// Python's `int(round(x))` for a timeline head: round half to even, clamped at 0.
pub fn round_head(head_at: f64) -> u64 {
    let rounded = head_at.round_ties_even();
    if rounded <= 0.0 { 0 } else { rounded as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_head_is_half_even() {
        assert_eq!(round_head(92610.5), 92610);
        assert_eq!(round_head(92611.5), 92612);
        assert_eq!(round_head(4294967395.6), 4294967396);
        assert_eq!(round_head(-3.0), 0);
        assert_eq!(round_head(12884901895.0), 12884901895);
    }

    #[test]
    fn head_runs_at_exactly_the_stream_rate() {
        // A PTP master time of the size the HomePod sends (about 1.8e18 ns).
        let m0 = 1_793_300_000_123_456_789;
        let timeline = Timeline::new(m0, 4410);
        assert_eq!(timeline.head_at(m0), 4410.0);
        assert_eq!(timeline.head_at(m0 + 1_000_000_000), 4410.0 + 44100.0);
        // A frame is 22 675.7 ns: just under half a frame on, the head still rounds to
        // the same frame; just over, to the next.
        assert_eq!(round_head(timeline.head_at(m0 + 11_337)), 4410);
        assert_eq!(round_head(timeline.head_at(m0 + 11_338)), 4411);
        // 30 days on, the head is past 2^32 and still exact.
        let month = 30 * 86_400 * 1_000_000_000u64;
        assert_eq!(
            timeline.head_at(m0 + month),
            4410.0 + 30.0 * 86_400.0 * 44100.0
        );
        // Before m0 the mapping extrapolates backwards instead of wrapping.
        assert_eq!(timeline.head_at(m0 - 100_000_000), 4410.0 - 4410.0);
        assert_eq!(
            Timeline::with_rate(48_000, 0, 0).head_at(500_000_000),
            24_000.0
        );
    }
}
