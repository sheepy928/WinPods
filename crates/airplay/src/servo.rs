//! Rate servo (workstream: real-time): keeps the captured audio on the stream's
//! [timeline](crate::timeline).
//!
//! The sound card (or virtual cable) WinPods captures from runs on its own clock, a
//! little faster or slower than the receiver's; VB-Audio Cable on the development PC
//! runs 22 ppm fast. The receiver plays 44 100 frames per second of its own clock, so
//! audio produced faster piles up in its buffer as latency (about 80 ms an hour at
//! 22 ppm) and audio produced slower runs it dry.
//!
//! After every packet of captured audio the sender measures how far the audio it has
//! sent is ahead of the timeline (`rtptime − head_at(now)`, the quantity v0.2.0's
//! timeline followed, so the latency stays where it was). [`RateServo`] turns that
//! error into a rate ratio for the capture resampler (`RingShared::set_rate_ratio`): a
//! proportional-integral controller on the error low-passed over [`FILTER_TAU_S`] of
//! packets (every packet weighs the same, like v0.2.0's timeline: packets of one burst
//! go out together, and weighing by elapsed time would favour the first of each),
//! critically damped with both poles at [`TIME_CONSTANT_S`]. Windows' 10–25 ms bursts,
//! the 352-frame packets and PTP noise then move the ratio by a few ppm, a constant
//! drift of 22 ppm is absorbed with about half a millisecond of error (and none once
//! learned: the next stream starts from the ratio the last one found), and the ratio
//! never leaves 1 ± [`MAX_RATE_ADJUST`]. The bursts also leave a new stream's audio a
//! few milliseconds off the timeline at first; that is closed over a few minutes.

pub use winpods_rt::ring::MAX_RATE_ADJUST;

use crate::types::{FRAMES_PER_PACKET, SAMPLE_RATE};

/// Time constant of the closed loop (s); both poles sit at `−1/τ`.
pub const TIME_CONSTANT_S: f64 = 60.0;
/// Time constant of the low-pass on the measured error (s of packets).
pub const FILTER_TAU_S: f64 = 4.0;
/// Weight of one packet's error in the low-pass: one packet's duration over
/// [`FILTER_TAU_S`] (about 0.002).
const FILTER_WEIGHT: f64 = FRAMES_PER_PACKET as f64 / SAMPLE_RATE as f64 / FILTER_TAU_S;
/// Proportional gain (per second): `2/τ`.
const KP: f64 = 2.0 / TIME_CONSTANT_S;
/// Integral gain (per second²): `1/τ²`.
const KI: f64 = 1.0 / (TIME_CONSTANT_S * TIME_CONSTANT_S);

/// PI controller from "audio ahead of the timeline" to a capture rate ratio.
#[derive(Debug, Clone, PartialEq)]
pub struct RateServo {
    /// Low-passed error: seconds of audio sent ahead of the timeline.
    error_s: f64,
    /// Integral term: the rate difference learned so far (positive = capture fast).
    integral: f64,
    /// `rt::mono_ns()` of the last update; `None` before the first and after `hold`.
    last_ns: Option<u64>,
    ratio: f64,
}

impl RateServo {
    /// A servo that starts at `ratio` (what the previous stream had learned, or 1.0).
    pub fn new(ratio: f64) -> Self {
        let ratio = clamp(ratio);
        Self {
            error_s: 0.0,
            integral: 1.0 - ratio,
            last_ns: None,
            ratio,
        }
    }

    /// One measurement, taken right after a packet of audio went out: the audio sent
    /// so far is `error_frames` ahead of the timeline (negative: behind) at
    /// `rt::mono_ns()` value `t_ns`. Returns the rate ratio to ask the capture for.
    pub fn update(&mut self, t_ns: u64, error_frames: f64) -> f64 {
        let error_s = error_frames / f64::from(SAMPLE_RATE);
        match self.last_ns {
            // (Re)start: the error is about zero (a stream starts on the timeline, and a
            // stall fills up to it), and one sample may sit anywhere in the burst
            // pattern, so the average starts from zero; nothing is integrated.
            None => self.error_s = 0.0,
            Some(last) => {
                let dt = t_ns.saturating_sub(last) as f64 / 1e9;
                self.error_s += FILTER_WEIGHT * (error_s - self.error_s);
                self.integral = (self.integral + KI * self.error_s * dt)
                    .clamp(-MAX_RATE_ADJUST, MAX_RATE_ADJUST);
            }
        }
        self.last_ns = Some(t_ns);
        self.ratio = clamp(1.0 - (KP * self.error_s + self.integral));
        self.ratio
    }

    /// Capture has paused (silence is being sent instead): keep what was learned, but
    /// do not integrate across the pause, and average afresh once audio is back.
    pub fn hold(&mut self) {
        self.last_ns = None;
    }

    /// The ratio last returned (1.0 = nominal).
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// The low-passed error in frames (positive: audio ahead of the timeline).
    pub fn error_frames(&self) -> f64 {
        self.error_s * f64::from(SAMPLE_RATE)
    }
}

fn clamp(ratio: f64) -> f64 {
    if ratio.is_nan() {
        1.0
    } else {
        ratio.clamp(1.0 - MAX_RATE_ADJUST, 1.0 + MAX_RATE_ADJUST)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 44_100.0;

    /// How packets reach the servo in [`simulate`].
    #[derive(Clone, Copy, PartialEq)]
    enum Arrival {
        /// Measured when complete, give or take up to 5 ms (zero-mean jitter).
        Jittered,
        /// Sent at the end of the 10 ms Windows burst that completes it, as loopback
        /// capture delivers: on average the sent audio trails the timeline by ~5 ms.
        Bursts,
        /// Bursts, and the timeline reads 8 ms high for 4 s of every 100 s (the PTP
        /// offset steps seen with the HomePod).
        BurstsAndPtpOutliers,
    }

    /// The sender's view of a capture clock `drift` off the receiver's (22e-6 = 22 ppm
    /// fast) for `seconds`, the servo's ratio scaling the capture rate. Returns the
    /// largest low-passed error (ms) after `settle` seconds.
    fn simulate(
        servo: &mut RateServo,
        drift: f64,
        seconds: f64,
        settle: f64,
        arrival: Arrival,
    ) -> f64 {
        let mut complete = 0.0f64; // master time at which the packet was complete, s
        let mut rtptime = 0.0f64;
        let mut worst = 0.0f64;
        let mut n = 0u64;
        while complete < seconds {
            complete += 352.0 / (FS * (1.0 + drift) * servo.ratio());
            rtptime += 352.0;
            n += 1;
            let measured = match arrival {
                Arrival::Jittered => complete + ((n as f64 * 0.618_034).fract() - 0.5) * 0.010,
                Arrival::Bursts | Arrival::BurstsAndPtpOutliers => {
                    (complete / 0.010).ceil() * 0.010
                }
            };
            let mut target = measured * FS;
            if arrival == Arrival::BurstsAndPtpOutliers
                && (50.0..54.0).contains(&(measured % 100.0))
            {
                target += 0.008 * FS;
            }
            servo.update((measured * 1e9) as u64, rtptime - target);
            if complete > settle {
                worst = worst.max((servo.error_frames() / FS * 1e3).abs());
            }
        }
        worst
    }

    #[test]
    fn learns_a_constant_drift_with_little_error() {
        for drift in [22e-6, -22e-6, 100e-6, -300e-6] {
            let mut servo = RateServo::new(1.0);
            // Learning from scratch, the error peaks at drift·τ/e (0.5 ms for 22 ppm).
            let peak = simulate(&mut servo, drift, 600.0, 0.0, Arrival::Jittered);
            let theory = drift.abs() * TIME_CONSTANT_S / std::f64::consts::E * 1e3;
            assert!(
                peak < theory * 1.1 + 0.1,
                "{drift}: peak {peak} ms (theory {theory})"
            );
            // Once learned, the ratio cancels the drift and the error stays tiny.
            let after = simulate(&mut servo, drift, 300.0, 0.0, Arrival::Jittered);
            assert!(after < 0.1, "{drift}: {after} ms after learning");
            let ideal = 1.0 / (1.0 + drift);
            assert!(
                (servo.ratio() - ideal).abs() < 1e-6,
                "{drift}: ratio {} vs {ideal}",
                servo.ratio()
            );
        }
    }

    #[test]
    fn a_warm_start_has_no_drift_to_learn() {
        let drift = 300e-6;
        let mut learned = RateServo::new(1.0);
        simulate(&mut learned, drift, 900.0, 0.0, Arrival::Jittered);
        let mut next = RateServo::new(learned.ratio());
        assert_eq!(next.ratio(), learned.ratio());
        let worst = simulate(&mut next, drift, 300.0, 0.0, Arrival::Jittered);
        assert!(worst < 0.1, "{worst} ms");
    }

    #[test]
    fn bursty_arrival_is_brought_onto_the_timeline() {
        // Loopback bursts put the sent audio ~5 ms behind the timeline at first; the
        // servo closes that gap within a few minutes, without overshooting the drift.
        let mut servo = RateServo::new(1.0 / (1.0 + 22e-6));
        let early = simulate(&mut servo, 22e-6, 10.0, 9.0, Arrival::Bursts);
        assert!(early > 2.0, "{early} ms: the start-up gap is real");
        let late = simulate(&mut servo, 22e-6, 300.0, 299.0, Arrival::Bursts);
        assert!(late < 0.3, "{late} ms after 5 minutes");
        assert!((servo.ratio() - 1.0 / (1.0 + 22e-6)).abs() < 2e-6);
    }

    #[test]
    fn ptp_outliers_barely_move_the_audio() {
        let mut servo = RateServo::new(1.0 / (1.0 + 22e-6));
        simulate(&mut servo, 22e-6, 600.0, 0.0, Arrival::Bursts);
        let worst = simulate(&mut servo, 22e-6, 600.0, 0.0, Arrival::BurstsAndPtpOutliers);
        // The low-passed error follows an 8 ms, 4 s excursion only partly (5 ms)...
        assert!(worst < 5.5, "{worst} ms");
        // ...and the ratio settles back on the drift afterwards.
        simulate(&mut servo, 22e-6, 300.0, 0.0, Arrival::Bursts);
        assert!((servo.ratio() - 1.0 / (1.0 + 22e-6)).abs() < 2e-6);
    }

    #[test]
    fn hold_does_not_integrate_the_pause() {
        let mut servo = RateServo::new(1.0);
        servo.update(0, 441.0);
        servo.update(1_000_000_000, 441.0);
        let integral = servo.integral;
        servo.hold();
        // 100 s later, audio again: a fresh average, nothing integrated for the gap.
        servo.update(101_000_000_000, 441.0);
        assert_eq!(servo.integral, integral);
        assert_eq!(servo.error_frames(), 0.0);
        assert_eq!(servo.ratio(), 1.0 - integral);
    }

    #[test]
    fn the_ratio_stays_within_its_limits() {
        let mut servo = RateServo::new(2.0);
        assert_eq!(servo.ratio(), 1.0 + MAX_RATE_ADJUST);
        // A second of audio ahead for ten minutes: fully slowed down, never beyond.
        for i in 0..60_000u64 {
            assert!(servo.update(i * 10_000_000, FS) >= 1.0 - MAX_RATE_ADJUST);
        }
        assert_eq!(servo.ratio(), 1.0 - MAX_RATE_ADJUST);
        // The integral is clamped too, so it recovers as soon as the error flips.
        let mut t = 600_000_000_000u64;
        while servo.update(t, -FS) < 1.0 {
            t += 10_000_000;
            assert!(t < 700_000_000_000, "wound up");
        }
        assert_eq!(RateServo::new(f64::NAN).ratio(), 1.0);
    }
}
