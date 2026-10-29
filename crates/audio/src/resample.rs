//! Streaming conversion to 44.1 kHz stereo (workstream: audio).
//!
//! [`StereoResampler`] turns stereo `f32` frames at the device rate into 44.1 kHz
//! stereo `i16` frames for the ring. It wraps a rubato 5 [`Async`] sinc resampler
//! driven with `process_into_buffer` over `audioadapter-buffers` slice adapters, so
//! any input rate works (including odd ones such as 44 056 Hz) and nothing is
//! allocated after construction. A device that already runs at 44.1 kHz is not
//! filtered: its frames go through rubato's [`Slip`], which passes them on unchanged.
//!
//! Either way the rate can be fine-tuned while running ([`StereoResampler::set_rate`],
//! within ±[`MAX_RATE_ADJUST`]): the sender measures how far the device clock runs
//! from the receiver's and asks for the difference, so the stream never drifts (see
//! `winpods_airplay::servo`). The sinc resampler ramps to the new ratio over the next
//! packet; [`Slip`] inserts or drops one frame now and then, hidden by a 128-frame
//! crossfade (about 1 frame a second for a 22 ppm difference).
//!
//! Instead of collecting input in a fixed-size accumulator, every packet is resampled
//! whole: the resampler is sized for the largest packet (the WASAPI buffer) and its
//! chunk size is set to the packet length before each call (rubato's [`Resizable`]),
//! after which `input_frames_next()` equals the packet and `output_frames_next()`
//! gives the exact output size. Output buffers are sized once from
//! `output_frames_max()`. No captured audio ever waits for a chunk to fill; the only
//! hold-back is the filter's own delay ([`StereoResampler::output_delay`], about
//! 1.3 ms at 48 kHz), which [`StereoResampler::flush`] can push out with zero padding
//! (`Indexing::partial_len(0)`). Capture does not flush: when it goes quiet it
//! [`reset`](StereoResampler::reset)s the filter instead, so the next sound never
//! starts with the end of the previous one. It notices after 200 ms, which at
//! latencies up to about 275 ms is at or after the sender's stall, where a flushed
//! tail would land after the silence already sent. At longer latencies the sender
//! stalls a little later (at most 250 ms) and the ~1.3 ms tail is simply lost; that
//! is accepted (see `capture::WAIT_TIMEOUT`).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, Slip, WindowFunction,
};
use winpods_rt::Frame;
use winpods_rt::ring::MAX_RATE_ADJUST;

use crate::convert::frames_to_i16;

/// Output sample rate (the AirPlay stream rate).
pub const OUTPUT_RATE: u32 = winpods_rt::ring::SAMPLE_RATE;

/// Sinc filter length. 128 taps with a Blackman-Harris² window keeps the pass band
/// flat to about 19 kHz at 48 kHz → 44.1 kHz with ~1.3 ms of delay.
const SINC_LEN: usize = 128;
/// Intermediate sinc points between input samples (cubic interpolation between them).
const OVERSAMPLING: usize = 256;
/// rubato's limit for ratio changes, with headroom over the ±[`MAX_RATE_ADJUST`]
/// actually used (rubato checks `1/max ..= max`).
const MAX_RELATIVE_RATIO: f64 = 1.0 + 2.0 * MAX_RATE_ADJUST;
/// [`Slip`] needs at least this many frames per call; shorter pieces pass through.
const MIN_SLIP_CHUNK: usize = 4;

/// Errors from building or running a [`StereoResampler`].
#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    #[error("invalid input sample rate: {0} Hz")]
    InvalidRate(u32),
    #[error("cannot create resampler: {0}")]
    Construction(#[from] rubato::ResamplerConstructionError),
    #[error("output buffer too small: {needed} frames needed, {available} available")]
    OutputTooSmall { needed: usize, available: usize },
    #[error("resampling failed: {0}")]
    Process(#[from] rubato::ResampleError),
    #[error("resampler buffer error: {0}")]
    Buffer(#[from] rubato::audioadapter_buffers::SizeError),
}

/// Stereo `f32` at the device rate in, 44.1 kHz stereo `i16` out.
pub struct StereoResampler {
    input_rate: u32,
    max_chunk: usize,
    engine: Engine,
    /// Rate adjustment in effect (1.0 = none); see [`set_rate`](Self::set_rate).
    ratio: f64,
}

/// The rubato state plus its preallocated output scratch.
struct Engine {
    /// An [`Async`] sinc resampler, or [`Slip`] for 44.1 kHz input.
    resampler: Box<dyn Resampler<f32>>,
    /// True for [`Slip`]: nothing is filtered, so there is no delay and no tail.
    slip: bool,
    /// Interleaved `f32` output of one `process_into_buffer` call
    /// (`output_frames_max() * 2` samples).
    scratch: Vec<f32>,
    /// Input frames to push through the filter on [`StereoResampler::flush`].
    flush_frames: usize,
    /// True once input went through the filter since the last flush or reset.
    dirty: bool,
}

impl StereoResampler {
    /// Build a resampler for `input_rate` that accepts up to `max_input_frames`
    /// frames per rubato call (larger inputs are split). All buffers are allocated
    /// here; [`process`](Self::process) and [`flush`](Self::flush) never allocate.
    pub fn new(input_rate: u32, max_input_frames: usize) -> Result<Self, ResampleError> {
        if input_rate == 0 {
            return Err(ResampleError::InvalidRate(input_rate));
        }
        let max_chunk = max_input_frames.max(MIN_SLIP_CHUNK);
        let slip = input_rate == OUTPUT_RATE;
        let resampler: Box<dyn Resampler<f32>> = if slip {
            Box::new(Slip::<f32>::new(max_chunk, 2, FixedAsync::Input)?)
        } else {
            let ratio = f64::from(OUTPUT_RATE) / f64::from(input_rate);
            let params =
                SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2)
                    .oversampling_factor(OVERSAMPLING);
            Box::new(Async::<f32>::new_sinc(
                ratio,
                MAX_RELATIVE_RATIO,
                &params,
                max_chunk,
                2,
                FixedAsync::Input,
            )?)
        };
        let scratch = vec![0.0; resampler.output_frames_max() * 2];
        Ok(Self {
            input_rate,
            max_chunk,
            engine: Engine {
                resampler,
                slip,
                scratch,
                // One full filter span of silence moves every output sample that the
                // last real input frame contributes to out of the filter.
                flush_frames: (SINC_LEN + 2).min(max_chunk),
                dirty: false,
            },
            ratio: 1.0,
        })
    }

    /// The device rate this resampler converts from.
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// True when the input is already 44.1 kHz: frames are not filtered, only slipped
    /// now and then to follow [`set_rate`](Self::set_rate).
    pub fn is_bypass(&self) -> bool {
        self.engine.slip
    }

    /// Filter delay in output (44.1 kHz) frames: how much later an input event shows
    /// up in the output. Zero in bypass.
    pub fn output_delay(&self) -> usize {
        self.engine.resampler.output_delay()
    }

    /// Output frames that one [`process`](Self::process) of `input_frames` frames (or
    /// one [`flush`](Self::flush)) can produce at most; size output buffers with it.
    pub fn max_output_frames(&self, input_frames: usize) -> usize {
        input_frames.div_ceil(self.max_chunk).max(1) * self.engine.resampler.output_frames_max()
    }

    /// Fine-tune the rate: produce `ratio` frames per nominal frame (clamped to
    /// 1 ± [`MAX_RATE_ADJUST`]; NaN means 1.0). Takes effect with the next
    /// [`process`](Self::process), ramped over that call by the sinc resampler.
    /// Never allocates.
    pub fn set_rate(&mut self, ratio: f64) {
        let ratio = if ratio.is_nan() {
            1.0
        } else {
            ratio.clamp(1.0 - MAX_RATE_ADJUST, 1.0 + MAX_RATE_ADJUST)
        };
        if ratio != self.ratio {
            self.ratio = ratio;
            self.engine.apply_ratio(ratio, true);
        }
    }

    /// The rate adjustment in effect (1.0 = none).
    pub fn rate(&self) -> f64 {
        self.ratio
    }

    /// Resample `input` and write the 44.1 kHz frames to the front of `output`,
    /// returning how many were written. `output` must hold at least
    /// [`max_output_frames(input.len())`](Self::max_output_frames) frames.
    pub fn process(
        &mut self,
        input: &[[f32; 2]],
        output: &mut [Frame],
    ) -> Result<usize, ResampleError> {
        let needed = self.max_output_frames(input.len());
        if output.len() < needed {
            return Err(ResampleError::OutputTooSmall {
                needed,
                available: output.len(),
            });
        }
        let engine = &mut self.engine;
        let mut written = 0;
        for piece in input.chunks(self.max_chunk) {
            if engine.slip && piece.len() < MIN_SLIP_CHUNK {
                // Too short to hide a slip in: passed on as is.
                frames_to_i16(piece, &mut output[written..]);
                written += piece.len();
                continue;
            }
            if let Some(resizable) = engine.resampler.as_resizable() {
                resizable.set_chunk_size(piece.len())?;
            }
            let adapter_in = InterleavedSlice::new(piece.as_flattened(), 2, piece.len())?;
            written += engine.run(&adapter_in, None, &mut output[written..])?;
        }
        engine.dirty |= !engine.slip && !input.is_empty();
        Ok(written)
    }

    /// Push the filter's delayed tail out by resampling a little silence, so the end
    /// of a burst is not held back while capture is idle. Returns the frames written
    /// (0 in bypass or when nothing was processed since the last flush).
    pub fn flush(&mut self, output: &mut [Frame]) -> Result<usize, ResampleError> {
        let engine = &mut self.engine;
        if !engine.dirty {
            return Ok(0);
        }
        if let Some(resizable) = engine.resampler.as_resizable() {
            resizable.set_chunk_size(engine.flush_frames)?;
        }
        let empty: [f32; 0] = [];
        let adapter_in = InterleavedSlice::new(&empty[..], 2, 0)?;
        // partial_len(0): every input frame of this call is silence.
        let indexing = Indexing::new().partial_len(0);
        let written = engine.run(&adapter_in, Some(&indexing), output)?;
        engine.dirty = false;
        Ok(written)
    }

    /// True when input was processed since the last flush or reset, so the filter
    /// still holds its delayed tail. Always false in bypass.
    pub fn has_tail(&self) -> bool {
        self.engine.dirty
    }

    /// Forget all history (e.g. after a discontinuity the caller wants to start clean)
    /// but keep the rate adjustment. Never allocates.
    pub fn reset(&mut self) {
        self.engine.resampler.reset();
        self.engine.dirty = false;
        // rubato's reset also goes back to the nominal ratio.
        if self.ratio != 1.0 {
            self.engine.apply_ratio(self.ratio, false);
        }
    }
}

impl Engine {
    /// One `process_into_buffer` call at the current chunk size, converting the result
    /// to `i16` at the front of `output`.
    fn run(
        &mut self,
        input: &InterleavedSlice<&[f32]>,
        indexing: Option<&Indexing>,
        output: &mut [Frame],
    ) -> Result<usize, ResampleError> {
        let frames = self.resampler.output_frames_next();
        if output.len() < frames {
            return Err(ResampleError::OutputTooSmall {
                needed: frames,
                available: output.len(),
            });
        }
        let mut adapter_out = InterleavedSlice::new_mut(&mut self.scratch, 2, frames)?;
        let (_, produced) =
            self.resampler
                .process_into_buffer(input, &mut adapter_out, indexing)?;
        let samples = &self.scratch[..produced * 2];
        let (stereo, _) = samples.as_chunks::<2>();
        frames_to_i16(stereo, output);
        Ok(produced)
    }

    /// Set the ratio relative to the nominal one. Both resamplers are adjustable and
    /// `ratio` is within both limits ([`MAX_RELATIVE_RATIO`]; [`Slip`] sustains about
    /// ±0.8 %), so this cannot fail.
    fn apply_ratio(&mut self, ratio: f64, ramp: bool) {
        if let Some(adjustable) = self.resampler.as_adjustable() {
            let _ = adjustable.set_resample_ratio_relative(ratio, ramp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    /// Feed `input` in packets of the given sizes (cycled) and collect the output.
    fn run_packets(rs: &mut StereoResampler, input: &[[f32; 2]], sizes: &[usize]) -> Vec<Frame> {
        let max_packet = sizes.iter().copied().max().unwrap_or(1);
        let mut out = vec![[0i16; 2]; rs.max_output_frames(max_packet)];
        let mut all = Vec::new();
        let mut pos = 0;
        let mut i = 0;
        while pos < input.len() {
            let n = sizes[i % sizes.len()].min(input.len() - pos);
            let produced = rs.process(&input[pos..pos + n], &mut out).unwrap();
            all.extend_from_slice(&out[..produced]);
            pos += n;
            i += 1;
        }
        all
    }

    fn sine(rate: u32, freq: f64, amplitude: f64, frames: usize) -> Vec<[f32; 2]> {
        (0..frames)
            .map(|n| {
                let v = (amplitude * (TAU * freq * n as f64 / f64::from(rate)).sin()) as f32;
                [v, -v]
            })
            .collect()
    }

    /// Frequency from positive-going zero crossings (linearly interpolated).
    fn measured_frequency(samples: &[f64], rate: f64) -> f64 {
        let mut crossings = Vec::new();
        for i in 1..samples.len() {
            let (a, b) = (samples[i - 1], samples[i]);
            if a < 0.0 && b >= 0.0 {
                crossings.push((i - 1) as f64 + a / (a - b));
            }
        }
        assert!(crossings.len() > 10, "too few crossings");
        let span = crossings[crossings.len() - 1] - crossings[0];
        (crossings.len() - 1) as f64 * rate / span
    }

    fn rms(samples: &[f64]) -> f64 {
        (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt()
    }

    #[test]
    fn bypass_at_44100_only_converts() {
        let mut rs = StereoResampler::new(44_100, 512).unwrap();
        assert!(rs.is_bypass());
        assert_eq!(rs.output_delay(), 0);
        assert_eq!(rs.input_rate(), 44_100);
        assert!(rs.max_output_frames(300) >= 300);
        let mut out = vec![[7i16; 2]; rs.max_output_frames(512)];
        // Too short for a slip: converted directly.
        let input = [[0.5f32, -0.5], [1.5, -1.5], [0.0, 1.0]];
        assert_eq!(rs.process(&input, &mut out).unwrap(), 3);
        assert_eq!(out[0], [16_384, -16_384]);
        assert_eq!(out[1], [i16::MAX, i16::MIN], "clamped");
        assert_eq!(out[2], [0, i16::MAX]);
        assert_eq!(out[3], [7, 7]);
        // Through Slip at the nominal rate: unchanged, frame for frame.
        let input = sine(44_100, 1_000.0, 0.5, 480);
        assert_eq!(rs.process(&input, &mut out).unwrap(), 480);
        let mut direct = vec![[0i16; 2]; 480];
        frames_to_i16(&input, &mut direct);
        assert_eq!(out[..480], direct[..]);
        assert_eq!(rs.flush(&mut out).unwrap(), 0);
        assert!(!rs.has_tail());
    }

    /// Output frames per input frame over `seconds` of 10 ms packets at `rate`.
    fn measured_ratio(rate: u32, adjust: f64, seconds: usize) -> f64 {
        let packet = (rate / 100) as usize;
        let mut rs = StereoResampler::new(rate, packet).unwrap();
        rs.set_rate(adjust);
        assert_eq!(rs.rate(), adjust);
        let input = sine(rate, 440.0, 0.25, rate as usize * seconds);
        let out = run_packets(&mut rs, &input, &[packet]);
        out.len() as f64 / (input.len() as f64 * 44_100.0 / f64::from(rate))
    }

    #[test]
    fn rate_adjustment_changes_the_output_rate() {
        // 500 ppm fewer and more frames, through the sinc resampler and through Slip.
        for rate in [48_000, 44_100] {
            for adjust in [1.0 - 5e-4, 1.0 + 5e-4] {
                let ratio = measured_ratio(rate, adjust, 4);
                assert!(
                    (ratio - adjust).abs() < 5e-5,
                    "{rate} Hz at {adjust}: ratio {ratio}"
                );
            }
        }
        // Requests beyond ±0.1 % are clamped.
        let mut rs = StereoResampler::new(48_000, 480).unwrap();
        rs.set_rate(1.5);
        assert_eq!(rs.rate(), 1.0 + MAX_RATE_ADJUST);
        rs.set_rate(f64::NAN);
        assert_eq!(rs.rate(), 1.0);
    }

    #[test]
    fn reset_keeps_the_rate_adjustment() {
        let mut rs = StereoResampler::new(44_100, 441).unwrap();
        rs.set_rate(1.0 + 1e-3);
        rs.reset();
        assert_eq!(rs.rate(), 1.0 + 1e-3);
        let input = sine(44_100, 440.0, 0.25, 44_100 * 2);
        let out = run_packets(&mut rs, &input, &[441]);
        let extra = out.len() as f64 - input.len() as f64;
        assert!((extra - 88.2).abs() < 3.0, "{extra} frames slipped in");
    }

    #[test]
    fn slips_keep_a_tone_clean() {
        // 1 kHz through Slip at +1000 ppm (88 slips in 2 s): every slip is crossfaded,
        // so the waveform never jumps by more than a sample step does on its own.
        let mut rs = StereoResampler::new(44_100, 441).unwrap();
        rs.set_rate(1.0 + 1e-3);
        let input = sine(44_100, 1_000.0, 0.5, 44_100 * 2);
        let out = run_packets(&mut rs, &input, &[441]);
        let step = 0.5 * TAU * 1_000.0 / 44_100.0 * 32_768.0;
        let worst = out
            .windows(2)
            .map(|w| (i32::from(w[1][0]) - i32::from(w[0][0])).abs())
            .max()
            .unwrap();
        assert!(
            f64::from(worst) < step * 1.05,
            "jump of {worst} vs {step:.0}"
        );
    }

    #[test]
    fn rejects_zero_rate_and_small_output() {
        assert!(matches!(
            StereoResampler::new(0, 480),
            Err(ResampleError::InvalidRate(0))
        ));
        let mut rs = StereoResampler::new(48_000, 480).unwrap();
        let input = [[0.0f32; 2]; 480];
        let mut out = [[0i16; 2]; 10];
        assert!(matches!(
            rs.process(&input, &mut out),
            Err(ResampleError::OutputTooSmall { .. })
        ));
    }

    #[test]
    fn sine_48k_keeps_frequency_and_level() {
        let mut rs = StereoResampler::new(48_000, 4_800).unwrap();
        assert!(!rs.is_bypass());
        let amplitude = 0.5;
        let input = sine(48_000, 1_000.0, amplitude, 48_000 * 2);
        // Typical 10 ms packets plus some odd sizes, as real devices deliver.
        let out = run_packets(&mut rs, &input, &[480, 480, 333, 1, 960, 17]);

        let expected = input.len() as f64 * 44_100.0 / 48_000.0;
        let delay = rs.output_delay() as f64;
        assert!(
            (out.len() as f64 - expected).abs() <= delay + 4.0,
            "{} output frames for {expected} expected (delay {delay})",
            out.len()
        );

        // Skip the start-up transient and the end; look at a stable second.
        let left: Vec<f64> = out[4_410..4_410 + 44_100]
            .iter()
            .map(|f| f64::from(f[0]) / 32_768.0)
            .collect();
        let freq = measured_frequency(&left, 44_100.0);
        assert!((freq - 1_000.0).abs() < 0.05, "frequency {freq}");

        let level_db = 20.0 * (rms(&left) * 2f64.sqrt() / amplitude).log10();
        assert!(level_db.abs() < 0.5, "level off by {level_db} dB");

        // Right channel is the inverted left.
        for f in &out[4_410..4_510] {
            assert!((i32::from(f[0]) + i32::from(f[1])).abs() <= 1);
        }
    }

    #[test]
    fn reported_delay_matches_impulse_position() {
        let mut rs = StereoResampler::new(48_000, 1_024).unwrap();
        let delay = rs.output_delay();
        assert!(delay > 0 && delay < 200, "delay {delay}");
        let mut input = vec![[0.0f32; 2]; 48_000];
        let at = 9_600; // 200 ms
        input[at] = [0.9, 0.9];
        let out = run_packets(&mut rs, &input, &[480]);
        let (peak_index, _) = out
            .iter()
            .enumerate()
            .max_by_key(|(_, f)| f[0].unsigned_abs())
            .unwrap();
        let expected = at as f64 * 44_100.0 / 48_000.0 + delay as f64;
        assert!(
            (peak_index as f64 - expected).abs() <= 2.0,
            "impulse at {peak_index}, expected {expected}"
        );
    }

    #[test]
    fn flush_releases_the_delayed_tail() {
        let mut rs = StereoResampler::new(48_000, 960).unwrap();
        let mut out = vec![[0i16; 2]; rs.max_output_frames(960)];
        // Silence, then an impulse in the very last input frame.
        let mut input = vec![[0.0f32; 2]; 960];
        input[959] = [0.8, 0.8];
        let produced = rs.process(&input, &mut out).unwrap();
        let before = out[..produced].iter().map(|f| f[0].unsigned_abs()).max();
        assert!(
            before.unwrap_or(0) < 1_000,
            "impulse still inside the filter"
        );

        let flushed = rs.flush(&mut out).unwrap();
        assert!(
            flushed > rs.output_delay() && flushed <= 2 * rs.output_delay() + 8,
            "{flushed} frames"
        );
        let after = out[..flushed].iter().map(|f| f[0].unsigned_abs()).max();
        assert!(
            after.unwrap_or(0) > 10_000,
            "flush did not release the impulse"
        );

        // Nothing new since the flush: a second flush is a no-op.
        assert_eq!(rs.flush(&mut out).unwrap(), 0);
        rs.reset();
        assert_eq!(rs.flush(&mut out).unwrap(), 0);
    }

    #[test]
    fn reset_drops_the_tail_so_the_next_input_starts_clean() {
        let mut rs = StereoResampler::new(48_000, 480).unwrap();
        let mut out = vec![[0i16; 2]; rs.max_output_frames(480)];
        let loud = sine(48_000, 1_000.0, 0.5, 480);
        let quiet = vec![[0.0f32; 2]; 480];
        assert!(!rs.has_tail());
        rs.process(&loud, &mut out).unwrap();
        assert!(rs.has_tail());
        // Without a reset, the next input comes out after the previous one's tail.
        let produced = rs.process(&quiet, &mut out).unwrap();
        assert!(out[..produced].iter().any(|f| f[0] != 0));

        rs.process(&loud, &mut out).unwrap();
        rs.reset();
        assert!(!rs.has_tail());
        let produced = rs.process(&quiet, &mut out).unwrap();
        assert!(produced > 0);
        assert!(out[..produced].iter().all(|f| *f == [0, 0]), "stale tail");

        // A flush also empties it; bypass never holds anything.
        rs.flush(&mut out).unwrap();
        assert!(!rs.has_tail());
        let mut bypass = StereoResampler::new(44_100, 480).unwrap();
        let mut out = vec![[0i16; 2]; bypass.max_output_frames(480)];
        bypass.process(&loud, &mut out).unwrap();
        assert!(!bypass.has_tail());
    }

    #[test]
    fn oversized_input_is_split() {
        let mut rs = StereoResampler::new(96_000, 256).unwrap();
        let input = sine(96_000, 440.0, 0.3, 5_000);
        let mut out = vec![[0i16; 2]; rs.max_output_frames(input.len())];
        let produced = rs.process(&input, &mut out).unwrap();
        let expected = input.len() as f64 * 44_100.0 / 96_000.0;
        assert!((produced as f64 - expected).abs() <= rs.output_delay() as f64 + 4.0);
    }

    #[test]
    fn many_rates_construct_and_track_ratio() {
        for rate in [
            8_000u32, 11_025, 16_000, 22_050, 32_000, 44_056, 48_000, 88_200, 96_000, 176_400,
            192_000, 384_000,
        ] {
            let packet = (rate / 100) as usize;
            let mut rs = StereoResampler::new(rate, packet).unwrap();
            let input = sine(rate, 440.0, 0.25, rate as usize);
            let out = run_packets(&mut rs, &input, &[packet]);
            let expected = 44_100.0;
            assert!(
                (out.len() as f64 - expected).abs() <= rs.output_delay() as f64 + 4.0,
                "{rate} Hz: {} frames",
                out.len()
            );
        }
    }

    #[test]
    fn processing_does_not_allocate() {
        let mut rs = StereoResampler::new(48_000, 960).unwrap();
        let input = sine(48_000, 1_000.0, 0.5, 4_800);
        let mut slip = StereoResampler::new(44_100, 960).unwrap();
        let mut out = vec![[0i16; 2]; rs.max_output_frames(960).max(slip.max_output_frames(960))];
        // Warm up once (lazy CPU feature detection and the like).
        rs.process(&input[..480], &mut out).unwrap();
        slip.process(&input[..480], &mut out).unwrap();
        let before = crate::test_alloc::allocations();
        for (i, packet) in input.chunks(480).chain(input.chunks(137)).enumerate() {
            rs.set_rate(1.0 + (i % 3) as f64 * 1e-4);
            rs.process(packet, &mut out).unwrap();
        }
        rs.flush(&mut out).unwrap();
        rs.reset();
        slip.set_rate(1.0 + 1e-3);
        for packet in input.chunks(960) {
            slip.process(packet, &mut out).unwrap();
        }
        slip.reset();
        assert_eq!(
            crate::test_alloc::allocations(),
            before,
            "allocated while resampling"
        );
    }
}
