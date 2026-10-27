//! Streaming conversion to 44.1 kHz stereo (workstream: audio).
//!
//! [`StereoResampler`] turns stereo `f32` frames at the device rate into 44.1 kHz
//! stereo `i16` frames for the ring. It wraps a rubato 5 [`Async`] sinc resampler
//! driven with `process_into_buffer` over `audioadapter-buffers` slice adapters, so
//! any input rate works (including odd ones such as 44 056 Hz) and nothing is
//! allocated after construction. A device that already runs at 44.1 kHz bypasses
//! rubato entirely and is only converted to `i16`.
//!
//! Instead of collecting input in a fixed-size accumulator, every packet is resampled
//! whole: the resampler is sized for the largest packet (the WASAPI buffer) and its
//! chunk size is set to the packet length before each call (rubato's [`Resizable`]),
//! after which `input_frames_next()` equals the packet and `output_frames_next()`
//! gives the exact output size. Output buffers are sized once from
//! `output_frames_max()`. No captured audio ever waits for a chunk to fill; the only
//! hold-back is the filter's own delay ([`StereoResampler::output_delay`], about
//! 1.3 ms at 48 kHz), which [`StereoResampler::flush`] pushes out with zero padding
//! (`Indexing::partial_len(0)`) when capture goes quiet.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, Resizable, SincInterpolationParameters, WindowFunction,
};
use winpods_rt::Frame;

use crate::convert::frames_to_i16;

/// Output sample rate (the AirPlay stream rate).
pub const OUTPUT_RATE: u32 = winpods_rt::ring::SAMPLE_RATE;

/// Sinc filter length. 128 taps with a Blackman-Harris² window keeps the pass band
/// flat to about 19 kHz at 48 kHz → 44.1 kHz with ~1.3 ms of delay.
const SINC_LEN: usize = 128;
/// Intermediate sinc points between input samples (cubic interpolation between them).
const OVERSAMPLING: usize = 256;

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
    /// `None` when the input already runs at 44.1 kHz.
    engine: Option<Engine>,
}

/// The rubato state plus its preallocated output scratch.
struct Engine {
    resampler: Async<f32>,
    /// Interleaved `f32` output of one `process_into_buffer` call
    /// (`output_frames_max() * 2` samples).
    scratch: Vec<f32>,
    /// Input frames to push through the filter on [`StereoResampler::flush`].
    flush_frames: usize,
    /// True once input was processed since the last flush or reset.
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
        let max_chunk = max_input_frames.max(1);
        let engine = if input_rate == OUTPUT_RATE {
            None
        } else {
            let ratio = f64::from(OUTPUT_RATE) / f64::from(input_rate);
            let params =
                SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2)
                    .oversampling_factor(OVERSAMPLING);
            // Fixed ratio: the maximum relative adjustment is 1.0 (none).
            let resampler =
                Async::<f32>::new_sinc(ratio, 1.0, &params, max_chunk, 2, FixedAsync::Input)?;
            let scratch = vec![0.0; resampler.output_frames_max() * 2];
            Some(Engine {
                resampler,
                scratch,
                // One full filter span of silence moves every output sample that the
                // last real input frame contributes to out of the filter.
                flush_frames: (SINC_LEN + 2).min(max_chunk),
                dirty: false,
            })
        };
        Ok(Self {
            input_rate,
            max_chunk,
            engine,
        })
    }

    /// The device rate this resampler converts from.
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// True when the input is already 44.1 kHz and rubato is not used.
    pub fn is_bypass(&self) -> bool {
        self.engine.is_none()
    }

    /// Filter delay in output (44.1 kHz) frames: how much later an input event shows
    /// up in the output. Zero in bypass.
    pub fn output_delay(&self) -> usize {
        self.engine
            .as_ref()
            .map_or(0, |engine| engine.resampler.output_delay())
    }

    /// Output frames that one [`process`](Self::process) of `input_frames` frames (or
    /// one [`flush`](Self::flush)) can produce at most; size output buffers with it.
    pub fn max_output_frames(&self, input_frames: usize) -> usize {
        match &self.engine {
            None => input_frames,
            Some(engine) => {
                input_frames.div_ceil(self.max_chunk).max(1) * engine.resampler.output_frames_max()
            }
        }
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
        let Some(engine) = self.engine.as_mut() else {
            frames_to_i16(input, output);
            return Ok(input.len());
        };
        let mut written = 0;
        for piece in input.chunks(self.max_chunk) {
            engine.resampler.set_chunk_size(piece.len())?;
            let adapter_in = InterleavedSlice::new(piece.as_flattened(), 2, piece.len())?;
            written += engine.run(&adapter_in, None, &mut output[written..])?;
        }
        engine.dirty |= !input.is_empty();
        Ok(written)
    }

    /// Push the filter's delayed tail out by resampling a little silence, so the end
    /// of a burst is not held back while capture is idle. Returns the frames written
    /// (0 in bypass or when nothing was processed since the last flush).
    pub fn flush(&mut self, output: &mut [Frame]) -> Result<usize, ResampleError> {
        let Some(engine) = self.engine.as_mut() else {
            return Ok(0);
        };
        if !engine.dirty {
            return Ok(0);
        }
        engine.resampler.set_chunk_size(engine.flush_frames)?;
        let empty: [f32; 0] = [];
        let adapter_in = InterleavedSlice::new(&empty[..], 2, 0)?;
        // partial_len(0): every input frame of this call is silence.
        let indexing = Indexing::new().partial_len(0);
        let written = engine.run(&adapter_in, Some(&indexing), output)?;
        engine.dirty = false;
        Ok(written)
    }

    /// Forget all history (e.g. after a discontinuity the caller wants to start clean).
    pub fn reset(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            engine.resampler.reset();
            engine.dirty = false;
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
        assert_eq!(rs.max_output_frames(300), 300);
        let input = [[0.5f32, -0.5], [1.5, -1.5], [0.0, 1.0]];
        let mut out = [[7i16; 2]; 4];
        assert_eq!(rs.process(&input, &mut out).unwrap(), 3);
        assert_eq!(out[0], [16_384, -16_384]);
        assert_eq!(out[1], [i16::MAX, i16::MIN], "clamped");
        assert_eq!(out[2], [0, i16::MAX]);
        assert_eq!(out[3], [7, 7]);
        assert_eq!(rs.flush(&mut out).unwrap(), 0);
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
        let mut out = vec![[0i16; 2]; rs.max_output_frames(960)];
        // Warm up once (lazy CPU feature detection and the like).
        rs.process(&input[..480], &mut out).unwrap();
        let before = crate::test_alloc::allocations();
        for packet in input.chunks(480).chain(input.chunks(137)) {
            rs.process(packet, &mut out).unwrap();
        }
        rs.flush(&mut out).unwrap();
        assert_eq!(
            crate::test_alloc::allocations(),
            before,
            "allocated while resampling"
        );
    }
}
