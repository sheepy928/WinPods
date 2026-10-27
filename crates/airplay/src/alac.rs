//! Uncompressed ALAC escape frames and PCM payloads (workstream: real-time).
//!
//! Port of `legacy/python/winpods/alac.py`: header `(1<<20)|(1<<3)|1` in 23 bits,
//! 32-bit frame count (352), samples L then R as 16-bit MSB-first starting at bit
//! offset 55, END tag `0b111`, zero padding → 1416 bytes. PCM payload = the same
//! frames as `i16::to_be_bytes`, 1408 bytes. Inputs are native `[i16; 2]` frames.
//! Must not allocate (writes into caller buffers). Fixture: `alac.json`.
//!
//! Bitstream layout (Apple's `ALACDecoder.cpp`):
//!
//! ```text
//!  3 bits  element tag    1 = CPE (channel pair)
//!  4 bits  instance tag   0
//! 12 bits  unused         0
//!  1 bit   partial frame  1 = a 32-bit sample count follows
//!  2 bits  bytes shifted  0
//!  1 bit   escape         1 = uncompressed
//! 32 bits  sample count
//! N × 2 × 16 bits         samples, left then right, MSB first
//!  3 bits  element tag    7 = END
//!          zero padding to a byte boundary
//! ```

use winpods_rt::Frame;

use crate::types::{ALAC_FRAME_BYTES, FRAMES_PER_PACKET, PCM_FRAME_BYTES};

/// The 23 header bits: CPE, instance 0, partial-frame flag, escape flag.
pub const ALAC_HEADER: u32 = (1 << 20) | (1 << 3) | 1;
const HEADER_BITS: u32 = 23;
const COUNT_BITS: u32 = 32;
const END_TAG: u32 = 0b111;
const END_BITS: u32 = 3;

/// Size in bytes of an escape frame holding `frames` stereo frames.
pub const fn alac_frame_size(frames: usize) -> usize {
    let bits = HEADER_BITS as usize + COUNT_BITS as usize + frames * 32 + END_BITS as usize;
    bits.div_ceil(8)
}

/// Size in bytes of a big-endian PCM payload holding `frames` stereo frames.
pub const fn pcm_frame_size(frames: usize) -> usize {
    frames * 4
}

const _: () = assert!(alac_frame_size(FRAMES_PER_PACKET) == ALAC_FRAME_BYTES);
const _: () = assert!(pcm_frame_size(FRAMES_PER_PACKET) == PCM_FRAME_BYTES);

/// MSB-first bit writer over a byte slice that is known to be large enough.
struct BitWriter<'a> {
    out: &'a mut [u8],
    pos: usize,
    acc: u64,
    pending: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        Self {
            out,
            pos: 0,
            acc: 0,
            pending: 0,
        }
    }

    /// Append the low `bits` bits of `value` (`bits` ≤ 32).
    #[inline]
    fn put(&mut self, value: u32, bits: u32) {
        let mask = (1u64 << bits) - 1;
        // At most 7 bits are pending before a put, so 7 + 32 bits fit the u64.
        self.acc = (self.acc << bits) | (u64::from(value) & mask);
        self.pending += bits;
        while self.pending >= 8 {
            self.pending -= 8;
            self.out[self.pos] = (self.acc >> self.pending) as u8;
            self.pos += 1;
        }
    }

    /// Flush the last partial byte (zero padded); returns the bytes written.
    fn finish(mut self) -> usize {
        if self.pending > 0 {
            self.out[self.pos] = (self.acc << (8 - self.pending)) as u8;
            self.pos += 1;
        }
        self.pos
    }
}

/// Wrap `frames` in one uncompressed ALAC escape frame written to the front of `out`.
///
/// Returns the number of bytes written ([`alac_frame_size`]), or `None` when `out`
/// is too small or `frames` does not fit the 32-bit sample count.
pub fn encode_alac(frames: &[Frame], out: &mut [u8]) -> Option<usize> {
    let count = u32::try_from(frames.len()).ok()?;
    let out = out.get_mut(..alac_frame_size(frames.len()))?;
    Some(write_alac(frames, count, out))
}

/// [`encode_alac`] for exactly one packet of audio; cannot fail.
pub fn encode_alac_packet(
    frames: &[Frame; FRAMES_PER_PACKET],
    out: &mut [u8; ALAC_FRAME_BYTES],
) -> usize {
    // `out` is exactly alac_frame_size(352) bytes (checked at compile time above).
    write_alac(frames, FRAMES_PER_PACKET as u32, out)
}

/// The escape frame for `frames` into `out`, which holds exactly
/// `alac_frame_size(frames.len())` bytes.
fn write_alac(frames: &[Frame], count: u32, out: &mut [u8]) -> usize {
    let mut writer = BitWriter::new(out);
    writer.put(ALAC_HEADER, HEADER_BITS);
    writer.put(count, COUNT_BITS);
    for &[left, right] in frames {
        writer.put(u32::from(left as u16), 16);
        writer.put(u32::from(right as u16), 16);
    }
    writer.put(END_TAG, END_BITS);
    writer.finish()
}

/// Write `frames` as interleaved big-endian 16-bit PCM (L then R) to the front of
/// `out`. Returns the bytes written, or `None` when `out` is too small.
pub fn encode_pcm(frames: &[Frame], out: &mut [u8]) -> Option<usize> {
    let out = out.get_mut(..pcm_frame_size(frames.len()))?;
    write_pcm(frames, out);
    Some(out.len())
}

/// [`encode_pcm`] for exactly one packet of audio; cannot fail.
pub fn encode_pcm_packet(
    frames: &[Frame; FRAMES_PER_PACKET],
    out: &mut [u8; PCM_FRAME_BYTES],
) -> usize {
    write_pcm(frames, out);
    PCM_FRAME_BYTES
}

fn write_pcm(frames: &[Frame], out: &mut [u8]) {
    for (chunk, &[left, right]) in out.as_chunks_mut::<4>().0.iter_mut().zip(frames) {
        let [l0, l1] = left.to_be_bytes();
        let [r0, r1] = right.to_be_bytes();
        *chunk = [l0, l1, r0, r1];
    }
}

/// Decode an escape frame produced by [`encode_alac`] back into frames (used by
/// tests and mock receivers). Returns the number of frames written to `out`, or
/// `None` if `data` is not a well-formed stereo escape frame or `out` is too small.
pub fn decode_alac(data: &[u8], out: &mut [Frame]) -> Option<usize> {
    let mut reader = BitReader { data, bit: 0 };
    if reader.take(HEADER_BITS)? != ALAC_HEADER {
        return None;
    }
    let count = usize::try_from(reader.take(COUNT_BITS)?).ok()?;
    if data.len() != alac_frame_size(count) || out.len() < count {
        return None;
    }
    for frame in out.iter_mut().take(count) {
        let left = reader.take(16)? as u16 as i16;
        let right = reader.take(16)? as u16 as i16;
        *frame = [left, right];
    }
    if reader.take(END_BITS)? != END_TAG {
        return None;
    }
    Some(count)
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl BitReader<'_> {
    fn take(&mut self, bits: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..bits {
            let byte = *self.data.get(self.bit / 8)?;
            let b = (byte >> (7 - (self.bit % 8))) & 1;
            value = (value << 1) | u32::from(b);
            self.bit += 1;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/alac.json")).unwrap()
    }

    fn frames_of(vector: &Value) -> Vec<Frame> {
        vector["samples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                [
                    pair[0].as_i64().unwrap() as i16,
                    pair[1].as_i64().unwrap() as i16,
                ]
            })
            .collect()
    }

    #[test]
    fn sizes_match_python() {
        assert_eq!(alac_frame_size(352), 1416);
        assert_eq!(alac_frame_size(1), 12);
        assert_eq!(alac_frame_size(3), 20);
        assert_eq!(pcm_frame_size(352), 1408);
    }

    #[test]
    fn fixture_vectors_match_byte_for_byte() {
        let fx = fixture();
        let vectors = fx["vectors"].as_array().unwrap();
        assert!(!vectors.is_empty());
        for vector in vectors {
            let name = vector["name"].as_str().unwrap();
            let frames = frames_of(vector);
            assert_eq!(frames.len() as u64, vector["frames"].as_u64().unwrap());

            let mut alac = [0u8; ALAC_FRAME_BYTES];
            let n = encode_alac(&frames, &mut alac).unwrap();
            assert_eq!(
                hex::encode(&alac[..n]),
                vector["alac"].as_str().unwrap(),
                "alac {name}"
            );

            let mut pcm = [0u8; PCM_FRAME_BYTES];
            let n = encode_pcm(&frames, &mut pcm).unwrap();
            assert_eq!(
                hex::encode(&pcm[..n]),
                vector["pcm_be"].as_str().unwrap(),
                "pcm {name}"
            );
        }
    }

    #[test]
    fn packet_helpers_and_round_trip() {
        let mut frames = [[0i16; 2]; FRAMES_PER_PACKET];
        for (i, f) in frames.iter_mut().enumerate() {
            *f = [
                (i as i16).wrapping_mul(97),
                -(i as i16).wrapping_mul(31) - 1,
            ];
        }
        let mut alac = [0u8; ALAC_FRAME_BYTES];
        assert_eq!(encode_alac_packet(&frames, &mut alac), ALAC_FRAME_BYTES);
        let mut decoded = [[0i16; 2]; FRAMES_PER_PACKET];
        assert_eq!(decode_alac(&alac, &mut decoded), Some(FRAMES_PER_PACKET));
        assert_eq!(decoded, frames);

        let mut pcm = [0u8; PCM_FRAME_BYTES];
        assert_eq!(encode_pcm_packet(&frames, &mut pcm), PCM_FRAME_BYTES);
        assert_eq!(&pcm[..4], &[0, 0, 0xFF, 0xFF]);
    }

    #[test]
    fn small_buffers_are_rejected() {
        let frames = [[1i16, 2i16]; 3];
        let mut out = [0u8; 19];
        assert_eq!(encode_alac(&frames, &mut out), None);
        let mut out = [0u8; 11];
        assert_eq!(encode_pcm(&frames, &mut out), None);
        let mut short = [[0i16; 2]; 2];
        let mut alac = [0u8; 20];
        encode_alac(&frames, &mut alac).unwrap();
        assert_eq!(decode_alac(&alac, &mut short), None);
        assert_eq!(decode_alac(&alac[..19], &mut [[0i16; 2]; 3]), None);
    }
}
