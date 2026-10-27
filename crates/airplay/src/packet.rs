//! RTP audio packet assembly and encryption (workstream: real-time).
//!
//! Header `0x80, 0xE0 (first) / 0x60, seq u16, ts u32, ssrc u32` (big-endian);
//! payload sealed with ChaCha20-Poly1305 keyed by `shk`, nonce = 4 zero bytes +
//! u64 LE counter (starts at 0, +1 per packet incl. silence), AAD = header[4..12];
//! packet = header + ciphertext + tag + the 8-byte LE counter used. No allocation.
//! Fixture: `audio_packets.json`.
//!
//! This is pyatv's `Chacha20Cipher8byteNonce` as used by `LiveAirPlayV2.build_packet`:
//! one cipher per stream, its counter shared by audio and silence packets.

use chacha20poly1305::aead::inout::InOutBuf;
use chacha20poly1305::{AeadInOut, ChaCha20Poly1305, Key, KeyInit, Nonce, Tag};

use crate::types::ALAC_FRAME_BYTES;

/// Size of the RTP header.
pub const RTP_HEADER_BYTES: usize = 12;
/// Size of the Poly1305 tag.
pub const TAG_BYTES: usize = 16;
/// Size of the nonce counter appended to every packet.
pub const NONCE_TRAILER_BYTES: usize = 8;
/// Largest audio packet the sender builds (an ALAC packet: 1452 bytes).
pub const MAX_PACKET_BYTES: usize = sealed_len(ALAC_FRAME_BYTES);

/// RTP payload type byte of the first packet of a stream (marker bit set).
pub const PAYLOAD_TYPE_FIRST: u8 = 0xE0;
/// RTP payload type byte of every following packet.
pub const PAYLOAD_TYPE: u8 = 0x60;

/// Total packet size for a plaintext payload of `payload_len` bytes.
pub const fn sealed_len(payload_len: usize) -> usize {
    RTP_HEADER_BYTES + payload_len + TAG_BYTES + NONCE_TRAILER_BYTES
}

/// Errors from sealing or opening audio packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    #[error("output buffer too small for the packet")]
    BufferTooSmall,
    #[error("packet too short")]
    Truncated,
    #[error("authentication failed")]
    Crypto,
}

/// Build the 12-byte RTP header (`ts` is the unwrapped RTP time; only its low 32
/// bits go on the wire).
pub fn rtp_header(first: bool, seq: u16, ts: u64, ssrc: u32) -> [u8; RTP_HEADER_BYTES] {
    let mut header = [0u8; RTP_HEADER_BYTES];
    header[0] = 0x80;
    header[1] = if first {
        PAYLOAD_TYPE_FIRST
    } else {
        PAYLOAD_TYPE
    };
    header[2..4].copy_from_slice(&seq.to_be_bytes());
    header[4..8].copy_from_slice(&(ts as u32).to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    header
}

fn nonce_for(counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(nonce)
}

/// Stream cipher state: the key and the next nonce counter.
pub struct AudioCipher {
    aead: ChaCha20Poly1305,
    counter: u64,
}

impl AudioCipher {
    /// A cipher whose first packet uses nonce counter 0.
    pub fn new(shk: &[u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(&Key::from(*shk)),
            counter: 0,
        }
    }

    /// The nonce counter the next [`seal`](Self::seal) will use.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// Assemble `header + ChaCha20-Poly1305(payload) + tag + counter (LE)` into the
    /// front of `out` and advance the counter. Returns the packet length.
    pub fn seal(
        &mut self,
        header: &[u8; RTP_HEADER_BYTES],
        payload: &[u8],
        out: &mut [u8],
    ) -> Result<usize, PacketError> {
        let total = sealed_len(payload.len());
        let out = out.get_mut(..total).ok_or(PacketError::BufferTooSmall)?;
        let (head, rest) = out.split_at_mut(RTP_HEADER_BYTES);
        head.copy_from_slice(header);
        let (body, rest) = rest.split_at_mut(payload.len());
        let (tag_out, counter_out) = rest.split_at_mut(TAG_BYTES);
        body.copy_from_slice(payload);
        let counter = self.counter;
        let tag = self
            .aead
            .encrypt_inout_detached(&nonce_for(counter), &header[4..12], InOutBuf::from(body))
            .map_err(|_| PacketError::Crypto)?;
        tag_out.copy_from_slice(tag.as_slice());
        counter_out.copy_from_slice(&counter.to_le_bytes());
        self.counter = counter.wrapping_add(1);
        Ok(total)
    }
}

/// Decrypt an audio packet built by [`AudioCipher::seal`] (for mock receivers and
/// diagnostics). Writes the plaintext payload to the front of `out` and returns its
/// length. The nonce is taken from the packet's trailing counter.
pub fn open_packet(shk: &[u8; 32], packet: &[u8], out: &mut [u8]) -> Result<usize, PacketError> {
    if packet.len() < sealed_len(0) {
        return Err(PacketError::Truncated);
    }
    let payload_len = packet.len() - sealed_len(0);
    let header = &packet[..RTP_HEADER_BYTES];
    let body = &packet[RTP_HEADER_BYTES..RTP_HEADER_BYTES + payload_len];
    let tag_bytes = &packet[RTP_HEADER_BYTES + payload_len..packet.len() - NONCE_TRAILER_BYTES];
    let mut counter = [0u8; NONCE_TRAILER_BYTES];
    counter.copy_from_slice(&packet[packet.len() - NONCE_TRAILER_BYTES..]);
    let mut tag = [0u8; TAG_BYTES];
    tag.copy_from_slice(tag_bytes);
    let plain = out
        .get_mut(..payload_len)
        .ok_or(PacketError::BufferTooSmall)?;
    plain.copy_from_slice(body);
    ChaCha20Poly1305::new(&Key::from(*shk))
        .decrypt_inout_detached(
            &nonce_for(u64::from_le_bytes(counter)),
            &header[4..12],
            InOutBuf::from(plain),
            &Tag::from(tag),
        )
        .map_err(|_| PacketError::Crypto)?;
    Ok(payload_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alac::{encode_alac_packet, encode_pcm_packet};
    use crate::types::{FRAMES_PER_PACKET, PCM_FRAME_BYTES};
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/audio_packets.json")).unwrap()
    }

    fn key(fx: &Value) -> [u8; 32] {
        hex::decode(fx["key"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn frames(packet: &Value) -> [[i16; 2]; FRAMES_PER_PACKET] {
        let mut out = [[0i16; 2]; FRAMES_PER_PACKET];
        for (f, pair) in out
            .iter_mut()
            .zip(packet["samples"].as_array().unwrap().iter())
        {
            *f = [
                pair[0].as_i64().unwrap() as i16,
                pair[1].as_i64().unwrap() as i16,
            ];
        }
        out
    }

    #[test]
    fn fixture_packets_match_byte_for_byte() {
        let fx = fixture();
        let shk = key(&fx);
        let ssrc = fx["ssrc"].as_u64().unwrap() as u32;
        assert_eq!(fx["frames_per_packet"].as_u64(), Some(352));
        let mut checked = 0;
        for sequence in fx["sequences"].as_array().unwrap() {
            let codec = sequence["codec"].as_str().unwrap();
            // A fresh cipher per sequence: the counter starts at 0.
            let mut cipher = AudioCipher::new(&shk);
            for p in sequence["packets"].as_array().unwrap() {
                let seq = p["seq"].as_u64().unwrap() as u16;
                let rtptime = p["rtptime"].as_u64().unwrap();
                let first = p["first"].as_bool().unwrap();
                let header = rtp_header(first, seq, rtptime, ssrc);
                assert_eq!(hex::encode(header), p["header"].as_str().unwrap());
                assert_eq!(
                    u64::from(u32::from_be_bytes(header[4..8].try_into().unwrap())),
                    p["ts"].as_u64().unwrap()
                );

                let samples = frames(p);
                let mut payload = [0u8; ALAC_FRAME_BYTES];
                let n = match codec {
                    "alac" => encode_alac_packet(&samples, &mut payload),
                    "pcm" => {
                        let mut pcm = [0u8; PCM_FRAME_BYTES];
                        let n = encode_pcm_packet(&samples, &mut pcm);
                        payload[..n].copy_from_slice(&pcm);
                        n
                    }
                    other => panic!("unknown codec {other}"),
                };
                assert_eq!(hex::encode(&payload[..n]), p["payload"].as_str().unwrap());

                let expected_counter = hex::decode(p["nonce"].as_str().unwrap()).unwrap();
                assert_eq!(cipher.counter().to_le_bytes().to_vec(), expected_counter);
                let mut packet = [0u8; MAX_PACKET_BYTES];
                let len = cipher.seal(&header, &payload[..n], &mut packet).unwrap();
                assert_eq!(hex::encode(&packet[..len]), p["packet"].as_str().unwrap());

                let mut plain = [0u8; ALAC_FRAME_BYTES];
                let m = open_packet(&shk, &packet[..len], &mut plain).unwrap();
                assert_eq!(&plain[..m], &payload[..n]);
                checked += 1;
            }
        }
        assert_eq!(checked, 6);
    }

    #[test]
    fn seal_rejects_small_buffer_and_open_rejects_tampering() {
        let shk = [7u8; 32];
        let mut cipher = AudioCipher::new(&shk);
        let header = rtp_header(true, 1, 2, 3);
        let mut small = [0u8; 20];
        assert_eq!(
            cipher.seal(&header, &[1, 2, 3], &mut small),
            Err(PacketError::BufferTooSmall)
        );
        assert_eq!(cipher.counter(), 0, "failed seal must not use a nonce");
        let mut packet = [0u8; 64];
        let len = cipher.seal(&header, &[1, 2, 3], &mut packet).unwrap();
        assert_eq!(len, sealed_len(3));
        assert_eq!(cipher.counter(), 1);
        packet[13] ^= 1;
        let mut out = [0u8; 8];
        assert_eq!(
            open_packet(&shk, &packet[..len], &mut out),
            Err(PacketError::Crypto)
        );
        assert_eq!(
            open_packet(&shk, &packet[..10], &mut out),
            Err(PacketError::Truncated)
        );
    }
}
