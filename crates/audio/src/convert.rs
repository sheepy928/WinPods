//! Sample conversion helpers for the capture path (private to the crate).
//!
//! WASAPI hands out packets in the endpoint's mix format: interleaved little-endian
//! samples of one container type, any number of channels. The capture thread turns
//! each packet into stereo `f32` frames (front left/right, mono duplicated), which
//! the resampler then converts to 44.1 kHz `i16`. Everything here works on caller
//! provided buffers and never allocates.

use winpods_rt::Frame;

/// Container type of one sample in a WASAPI mix format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    /// 32-bit IEEE float (the usual shared-mode mix format).
    F32,
    /// 64-bit IEEE float.
    F64,
    /// 16-bit signed PCM.
    I16,
    /// 24-bit signed PCM packed in three bytes.
    I24,
    /// 32-bit signed PCM container; also covers 24 valid bits left-aligned in 32.
    I32,
}

impl SampleFormat {
    /// Map a WAVEFORMATEXTENSIBLE description (float or integer subformat plus the
    /// container size in bits) to a supported format.
    pub(crate) fn from_parts(float: bool, container_bits: u16) -> Option<Self> {
        match (float, container_bits) {
            (true, 32) => Some(Self::F32),
            (true, 64) => Some(Self::F64),
            (false, 16) => Some(Self::I16),
            (false, 24) => Some(Self::I24),
            (false, 32) => Some(Self::I32),
            _ => None,
        }
    }

    /// Bytes per sample (per channel).
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::F32 | Self::I32 => 4,
            Self::F64 => 8,
        }
    }

    /// Short name used in [`crate::CaptureInfo::format`].
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I16 => "i16",
            Self::I24 => "i24",
            Self::I32 => "i32",
        }
    }
}

/// Decode interleaved packet bytes into stereo `f32` frames in `-1.0..1.0`.
///
/// Channel 0 becomes left and channel 1 right; a mono stream is duplicated to both
/// sides and channels beyond the first two are ignored. Decodes as many whole frames
/// as fit in both `raw` and `out` and returns that count.
pub(crate) fn decode_stereo(
    format: SampleFormat,
    channels: usize,
    raw: &[u8],
    out: &mut [[f32; 2]],
) -> usize {
    match format {
        SampleFormat::F32 => decode_with::<4>(channels, raw, out, f32::from_le_bytes),
        SampleFormat::F64 => decode_with::<8>(channels, raw, out, |b| f64::from_le_bytes(b) as f32),
        SampleFormat::I16 => decode_with::<2>(channels, raw, out, |b| {
            f32::from(i16::from_le_bytes(b)) / 32_768.0
        }),
        SampleFormat::I24 => decode_with::<3>(channels, raw, out, |b| {
            // Place the three bytes in the top of an i32; the arithmetic shift
            // sign-extends.
            (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
        }),
        SampleFormat::I32 => decode_with::<4>(channels, raw, out, |b| {
            i32::from_le_bytes(b) as f32 / 2_147_483_648.0
        }),
    }
}

/// Shared decode loop, monomorphised per sample size so the per-sample reader is
/// inlined.
fn decode_with<const N: usize>(
    channels: usize,
    raw: &[u8],
    out: &mut [[f32; 2]],
    read: impl Fn([u8; N]) -> f32,
) -> usize {
    if channels == 0 {
        return 0;
    }
    let block = N * channels;
    let right = if channels >= 2 { N } else { 0 };
    let mut frames = 0;
    for (frame, slot) in raw.chunks_exact(block).zip(out.iter_mut()) {
        *slot = [read(sample(frame, 0)), read(sample(frame, right))];
        frames += 1;
    }
    frames
}

/// Copy the `N` bytes of one sample starting at `offset` (always in bounds: the
/// caller passes whole frames and offsets below `N * channels`).
fn sample<const N: usize>(frame: &[u8], offset: usize) -> [u8; N] {
    let mut bytes = [0u8; N];
    bytes.copy_from_slice(&frame[offset..offset + N]);
    bytes
}

/// Convert a float sample to `i16`, rounding and clamping to the `i16` range.
/// NaN becomes 0.
#[inline]
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    // `as` saturates and maps NaN to 0; the explicit clamp documents the intent.
    (sample * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

/// Convert stereo float frames to `i16` frames (`out` must be at least as long).
pub(crate) fn frames_to_i16(input: &[[f32; 2]], out: &mut [Frame]) {
    for (dst, src) in out.iter_mut().zip(input) {
        *dst = [f32_to_i16(src[0]), f32_to_i16(src[1])];
    }
}

/// Peak level of a chunk as a fraction of full scale (0.0..=1.0).
pub(crate) fn peak(frames: &[Frame]) -> f32 {
    let max = frames
        .iter()
        .flat_map(|frame| frame.iter())
        .map(|sample| sample.unsigned_abs())
        .max()
        .unwrap_or(0);
    f32::from(max) / 32_768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of<T: Copy, const N: usize>(samples: &[T], to_le: impl Fn(T) -> [u8; N]) -> Vec<u8> {
        samples.iter().flat_map(|&s| to_le(s)).collect()
    }

    #[test]
    fn format_mapping() {
        assert_eq!(SampleFormat::from_parts(true, 32), Some(SampleFormat::F32));
        assert_eq!(SampleFormat::from_parts(true, 64), Some(SampleFormat::F64));
        assert_eq!(SampleFormat::from_parts(false, 16), Some(SampleFormat::I16));
        assert_eq!(SampleFormat::from_parts(false, 24), Some(SampleFormat::I24));
        assert_eq!(SampleFormat::from_parts(false, 32), Some(SampleFormat::I32));
        assert_eq!(SampleFormat::from_parts(false, 8), None);
        assert_eq!(SampleFormat::from_parts(true, 16), None);
        for (format, bytes, name) in [
            (SampleFormat::F32, 4, "f32"),
            (SampleFormat::F64, 8, "f64"),
            (SampleFormat::I16, 2, "i16"),
            (SampleFormat::I24, 3, "i24"),
            (SampleFormat::I32, 4, "i32"),
        ] {
            assert_eq!(format.bytes(), bytes);
            assert_eq!(format.name(), name);
        }
    }

    #[test]
    fn decodes_f32_stereo() {
        let raw = bytes_of(&[0.5f32, -0.25, 1.0, -1.0], f32::to_le_bytes);
        let mut out = [[9.0f32; 2]; 4];
        assert_eq!(decode_stereo(SampleFormat::F32, 2, &raw, &mut out), 2);
        assert_eq!(out[0], [0.5, -0.25]);
        assert_eq!(out[1], [1.0, -1.0]);
        assert_eq!(out[2], [9.0, 9.0], "untouched beyond the decoded frames");
    }

    #[test]
    fn decodes_f64() {
        let raw = bytes_of(&[0.75f64, -0.5], f64::to_le_bytes);
        let mut out = [[0.0f32; 2]; 1];
        assert_eq!(decode_stereo(SampleFormat::F64, 2, &raw, &mut out), 1);
        assert_eq!(out[0], [0.75, -0.5]);
    }

    #[test]
    fn decodes_i16_full_scale() {
        let raw = bytes_of(&[i16::MIN, i16::MAX, 0, 16_384], i16::to_le_bytes);
        let mut out = [[0.0f32; 2]; 2];
        assert_eq!(decode_stereo(SampleFormat::I16, 2, &raw, &mut out), 2);
        assert_eq!(out[0][0], -1.0);
        assert!((out[0][1] - 32_767.0 / 32_768.0).abs() < 1e-7);
        assert_eq!(out[1], [0.0, 0.5]);
    }

    #[test]
    fn decodes_packed_i24_with_sign_extension() {
        // -8388608 (min), +8388607 (max), -1, +4194304 (0.5)
        let samples: [i32; 4] = [-8_388_608, 8_388_607, -1, 4_194_304];
        let raw: Vec<u8> = samples
            .iter()
            .flat_map(|s| {
                let b = s.to_le_bytes();
                [b[0], b[1], b[2]]
            })
            .collect();
        assert_eq!(raw.len(), 12);
        let mut out = [[0.0f32; 2]; 2];
        assert_eq!(decode_stereo(SampleFormat::I24, 2, &raw, &mut out), 2);
        assert_eq!(out[0][0], -1.0);
        assert!((out[0][1] - 8_388_607.0 / 8_388_608.0).abs() < 1e-7);
        assert!((out[1][0] + 1.0 / 8_388_608.0).abs() < 1e-9);
        assert_eq!(out[1][1], 0.5);
    }

    #[test]
    fn decodes_i32_and_24_in_32() {
        // 24 valid bits left-aligned in a 32-bit container read the same way.
        let raw = bytes_of(
            &[i32::MIN, 0x4000_0000, 0x7FFF_FF00, -256],
            i32::to_le_bytes,
        );
        let mut out = [[0.0f32; 2]; 2];
        assert_eq!(decode_stereo(SampleFormat::I32, 2, &raw, &mut out), 2);
        assert_eq!(out[0], [-1.0, 0.5]);
        assert!((out[1][0] - 1.0).abs() < 1e-6);
        assert!((out[1][1] + 1.0 / 8_388_608.0).abs() < 1e-9);
    }

    #[test]
    fn mono_is_duplicated() {
        let raw = bytes_of(&[0.25f32, -0.5, 0.125], f32::to_le_bytes);
        let mut out = [[0.0f32; 2]; 3];
        assert_eq!(decode_stereo(SampleFormat::F32, 1, &raw, &mut out), 3);
        assert_eq!(out, [[0.25, 0.25], [-0.5, -0.5], [0.125, 0.125]]);
    }

    #[test]
    fn surround_takes_front_left_and_right() {
        // 5.1: FL FR FC LFE BL BR
        let raw = bytes_of(
            &[1i16, 2, 3, 4, 5, 6, 11, 12, 13, 14, 15, 16],
            i16::to_le_bytes,
        );
        let mut out = [[0.0f32; 2]; 2];
        assert_eq!(decode_stereo(SampleFormat::I16, 6, &raw, &mut out), 2);
        let scale = 32_768.0;
        assert_eq!(out[0], [1.0 / scale, 2.0 / scale]);
        assert_eq!(out[1], [11.0 / scale, 12.0 / scale]);
    }

    #[test]
    fn partial_frames_and_short_output_are_ignored() {
        // 2.5 stereo i16 frames of input, room for 1 frame of output.
        let raw = bytes_of(&[1i16, 2, 3, 4, 5], i16::to_le_bytes);
        let mut out = [[0.0f32; 2]; 1];
        assert_eq!(decode_stereo(SampleFormat::I16, 2, &raw, &mut out), 1);
        let mut roomy = [[0.0f32; 2]; 8];
        assert_eq!(decode_stereo(SampleFormat::I16, 2, &raw, &mut roomy), 2);
        assert_eq!(decode_stereo(SampleFormat::I16, 0, &raw, &mut roomy), 0);
    }

    #[test]
    fn float_to_i16_rounds_and_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(-1.0), i16::MIN);
        assert_eq!(f32_to_i16(1.0), i16::MAX, "+1.0 clamps to 32767");
        assert_eq!(f32_to_i16(3.5), i16::MAX);
        assert_eq!(f32_to_i16(-7.0), i16::MIN);
        assert_eq!(f32_to_i16(0.5), 16_384);
        assert_eq!(f32_to_i16(1.6 / 32_768.0), 2, "rounds to nearest");
        assert_eq!(f32_to_i16(-1.6 / 32_768.0), -2);
        assert_eq!(f32_to_i16(f32::NAN), 0);
        assert_eq!(f32_to_i16(f32::INFINITY), i16::MAX);
        assert_eq!(f32_to_i16(f32::NEG_INFINITY), i16::MIN);
    }

    #[test]
    fn i16_round_trips_through_float() {
        for value in [i16::MIN, -12_345, -1, 0, 1, 777, i16::MAX] {
            let raw = value.to_le_bytes();
            let mut out = [[0.0f32; 2]; 1];
            decode_stereo(SampleFormat::I16, 1, &raw, &mut out);
            assert_eq!(f32_to_i16(out[0][0]), value);
        }
    }

    #[test]
    fn conversion_does_not_allocate() {
        let raw = vec![0x11u8; 480 * 6 * 4];
        let mut stereo = vec![[0.0f32; 2]; 480];
        let mut frames = vec![[0i16; 2]; 480];
        let before = crate::test_alloc::allocations();
        for format in [
            SampleFormat::F32,
            SampleFormat::I16,
            SampleFormat::I24,
            SampleFormat::I32,
        ] {
            decode_stereo(format, 6, &raw, &mut stereo);
            frames_to_i16(&stereo, &mut frames);
            let _ = peak(&frames);
        }
        assert_eq!(crate::test_alloc::allocations(), before);
    }

    #[test]
    fn frames_and_peak() {
        let input = [[0.5f32, -0.25], [-1.0, 0.0]];
        let mut out = [[0i16; 2]; 2];
        frames_to_i16(&input, &mut out);
        assert_eq!(out, [[16_384, -8_192], [i16::MIN, 0]]);
        assert_eq!(peak(&out), 1.0);
        assert_eq!(peak(&[[100, -200]]), 200.0 / 32_768.0);
        assert_eq!(peak(&[]), 0.0);
    }
}
