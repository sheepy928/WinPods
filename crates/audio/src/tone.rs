//! Test tone: 880 Hz, ~120 ms beep with 5 ms ramps, once per second, about −22 dBFS
//! (workstream: audio). Port of `legacy/python/winpods/tone.py`; delivered in
//! ~5 ms chunks (220 frames) paced by an `rt::HiResTimer`, so the sender sees the
//! same bursty arrival pattern as real capture. Plays only on the receiver.
//!
//! [`ToneGenerator`] precomputes one second of the track (the same samples as the
//! Python `ToneSource`, including truncation to `i16`) and loops it. The pacing loop
//! that feeds the ring runs on the capture handle's `tone` thread; it only ever
//! writes into the ring and never opens an audio device.

use std::f64::consts::TAU;
use std::time::Duration;

use winpods_rt::{Frame, HANDLE, HiResTimer, RingProducer, Wait, mono_ns, wait_any};

use crate::capture::CaptureExit;
use crate::convert::peak;

/// Sample rate of the generated track (the ring rate).
pub const RATE: u32 = winpods_rt::ring::SAMPLE_RATE;
/// Frames per ring write, about 5 ms like a WASAPI packet.
pub const CHUNK_FRAMES: usize = 220;
/// Beep pitch.
pub const BEEP_HZ: f64 = 880.0;
/// Beep length, including the ramps.
pub const BEEP_SECONDS: f64 = 0.12;
/// Length of the fade-in and fade-out ramps.
pub const RAMP_SECONDS: f64 = 0.005;
/// Peak amplitude as a fraction of full scale: 0.08 ≈ −22 dBFS.
pub const AMPLITUDE: f64 = 0.08;
/// [`crate::CaptureInfo::device_id`] of the test tone.
pub const DEVICE_ID: &str = "test-tone";
/// [`crate::CaptureInfo::device_name`] of the test tone (the v0.1 label).
pub const DEVICE_NAME: &str = "Test tone (880 Hz beep each second)";

/// Interval between pacing wake-ups.
const TICK: Duration = Duration::from_millis(5);
/// Upper bound for one wait, in case the timer ever fails to fire.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// Endless beep track: one precomputed second, looped.
pub struct ToneGenerator {
    second: Vec<i16>,
    position: usize,
}

impl ToneGenerator {
    /// Precompute the one-second track (the only allocation).
    pub fn new() -> Self {
        let rate = f64::from(RATE);
        let second = (0..RATE)
            .map(|n| {
                let t = f64::from(n) / rate;
                let envelope = if t >= BEEP_SECONDS {
                    0.0
                } else {
                    (t.min(BEEP_SECONDS - t) / RAMP_SECONDS).clamp(0.0, 1.0)
                };
                // Same expression order as numpy, and `as` truncates toward zero
                // like `astype(np.int16)`.
                (AMPLITUDE * 32_767.0 * envelope * (TAU * BEEP_HZ * t).sin()) as i16
            })
            .collect();
        Self {
            second,
            position: 0,
        }
    }

    /// Fill `out` with the next frames of the track (mono, duplicated to both sides).
    pub fn fill(&mut self, out: &mut [Frame]) {
        for slot in out {
            let sample = self.second[self.position];
            *slot = [sample, sample];
            self.position += 1;
            if self.position == self.second.len() {
                self.position = 0;
            }
        }
    }

    /// Frame index within the current second of the track.
    pub fn position(&self) -> usize {
        self.position
    }
}

impl Default for ToneGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Frames of the track that are due `elapsed_ns` after the start.
fn frames_due(elapsed_ns: u64) -> u64 {
    (u128::from(elapsed_ns) * u128::from(RATE) / 1_000_000_000) as u64
}

/// Feed the ring in [`CHUNK_FRAMES`] chunks at real-time pace until `stop` is
/// signaled. Allocation-free; returns how the loop ended.
pub(crate) fn pace(
    producer: &mut RingProducer,
    generator: &mut ToneGenerator,
    timer: &HiResTimer,
    stop: HANDLE,
) -> CaptureExit {
    let mut chunk = [[0i16; 2]; CHUNK_FRAMES];
    let handles = [stop, timer.handle()];
    let start = mono_ns();
    let mut written: u64 = 0;
    loop {
        let due = frames_due(mono_ns().saturating_sub(start));
        while written + CHUNK_FRAMES as u64 <= due {
            generator.fill(&mut chunk);
            producer.set_peak(peak(&chunk));
            producer.push(&chunk);
            written += CHUNK_FRAMES as u64;
        }
        if let Err(e) = timer.arm(TICK) {
            return CaptureExit::Failed(format!("test tone timer failed: {e}"));
        }
        match wait_any(&handles, Some(MAX_WAIT)) {
            Ok(Wait::Signaled(0)) => return CaptureExit::Stopped,
            Ok(_) => {}
            Err(e) => return CaptureExit::Failed(format!("test tone wait failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_second() -> Vec<i16> {
        let mut generator = ToneGenerator::new();
        let mut frames = vec![[0i16; 2]; RATE as usize];
        generator.fill(&mut frames);
        assert!(frames.iter().all(|f| f[0] == f[1]), "mono on both sides");
        frames.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn peak_level_is_about_minus_22_dbfs() {
        let samples = one_second();
        let max = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!((2_615..=2_621).contains(&max), "peak {max}");
        let dbfs = 20.0 * (f64::from(max) / 32_768.0).log10();
        assert!((dbfs + 21.94).abs() < 0.05, "{dbfs} dBFS");
    }

    /// First silent frame after the beep (0.12 s × 44 100 = 5292).
    fn beep_end() -> usize {
        (BEEP_SECONDS * f64::from(RATE)).round() as usize
    }

    #[test]
    fn beep_lasts_120_ms_each_second() {
        let samples = one_second();
        let beep_end = beep_end();
        assert_eq!(beep_end, 5_292);
        assert!(
            samples[beep_end..].iter().all(|&s| s == 0),
            "silent after the beep"
        );
        let last_loud = samples.iter().rposition(|&s| s.unsigned_abs() > 50);
        assert!(last_loud.is_some_and(|i| i > beep_end - 300 && i < beep_end));
        // Steady part: RMS of a sine at the full amplitude.
        let ramp = (RAMP_SECONDS * f64::from(RATE)).ceil() as usize; // 220.5 -> 221
        let steady = &samples[ramp..beep_end - ramp];
        let rms = (steady.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / steady.len() as f64)
            .sqrt();
        let expected = AMPLITUDE * 32_767.0 / 2f64.sqrt();
        assert!(
            (rms / expected - 1.0).abs() < 0.01,
            "rms {rms} vs {expected}"
        );
    }

    #[test]
    fn ramps_take_5_ms() {
        let samples = one_second();
        let full = AMPLITUDE * 32_767.0;
        let ramp = RAMP_SECONDS * f64::from(RATE); // 220.5 frames
        let beep_end = beep_end();
        // Fade-in and fade-out stay under the linear 5 ms envelope ...
        for n in 0..ramp.ceil() as usize {
            let bound = full * (n as f64 / ramp) + 1.0;
            assert!(
                f64::from(samples[n].unsigned_abs()) <= bound,
                "fade-in at {n}"
            );
            let k = beep_end - 1 - n;
            let bound = full * ((n + 1) as f64 / ramp) + 1.0;
            assert!(
                f64::from(samples[k].unsigned_abs()) <= bound,
                "fade-out at {k}"
            );
        }
        // ... and the full level is reached right after it.
        let after = &samples[ramp.ceil() as usize..2 * ramp.ceil() as usize];
        let max = after.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(max >= 2_600, "full level after the ramp, got {max}");
    }

    #[test]
    fn frequency_is_880_hz() {
        let samples = one_second();
        let beep_end = beep_end();
        let rising = samples[..beep_end]
            .windows(2)
            .filter(|w| w[0] < 0 && w[1] >= 0)
            .count();
        // 880 Hz for 0.12 s = 105.6 cycles; ramps may hide one crossing.
        assert!((104..=106).contains(&rising), "{rising} cycles");
    }

    #[test]
    fn track_repeats_every_second_and_matches_across_chunk_boundaries() {
        let mut generator = ToneGenerator::new();
        let mut chunk = [[0i16; 2]; CHUNK_FRAMES];
        let mut streamed = Vec::new();
        for _ in 0..(2 * RATE as usize).div_ceil(CHUNK_FRAMES) {
            generator.fill(&mut chunk);
            streamed.extend(chunk.iter().map(|f| f[0]));
        }
        let rate = RATE as usize;
        assert_eq!(
            streamed[..rate],
            streamed[rate..2 * rate],
            "period of one second"
        );
        assert_eq!(streamed[..rate], one_second()[..]);
        assert_eq!(generator.position(), streamed.len() % rate);
    }

    #[test]
    fn feeding_the_ring_does_not_allocate() {
        let (mut producer, mut consumer) = winpods_rt::ring::ring(4_096).unwrap();
        let mut generator = ToneGenerator::new();
        let mut chunk = [[0i16; 2]; CHUNK_FRAMES];
        let before = crate::test_alloc::allocations();
        for _ in 0..1_000 {
            generator.fill(&mut chunk);
            producer.set_peak(peak(&chunk));
            producer.push(&chunk);
            consumer.clear();
        }
        assert_eq!(crate::test_alloc::allocations(), before);
    }

    #[test]
    fn due_frames_follow_the_clock() {
        assert_eq!(frames_due(0), 0);
        assert_eq!(frames_due(5_000_000), 220); // 5 ms
        assert_eq!(frames_due(1_000_000_000), 44_100);
        assert_eq!(frames_due(3_600 * 1_000_000_000), 3_600 * 44_100);
    }
}
