//! HAP transient pair-setup and encrypted framing (workstream: control plane).
//!
//! * pair-setup M1..M4 over `POST /pair-pin-start` and `/pair-setup` (HTTP/1.1,
//!   headers `User-Agent: AirPlay/320.20`, `Connection: keep-alive`,
//!   `X-Apple-HKP: 4`, `Content-Type: application/octet-stream`); parse M4 (Error
//!   TLV = failure; server proof checked for logging only); the caller retries a
//!   failed pairing up to 3 times.
//! * key derivation: HKDF-SHA512 over the 64-byte SRP key, 32-byte outputs:
//!   RTSP write/read = Control-Salt / Control-Write|Read-Encryption-Key; event
//!   channel send = Events-Salt / Events-Read-Encryption-Key, receive =
//!   Events-Write-Encryption-Key; `shk` = Events-Salt / Events-Write-Encryption-Key.
//! * framing (`pyatv/auth/hap_session.py`): `u16 LE length` + ChaCha20-Poly1305 of
//!   at most 1024 plaintext bytes + 16-byte tag; AAD = the two length bytes; nonce =
//!   4 zero bytes + u64 LE counter, one counter per direction per connection;
//!   decryption buffers partial frames across reads.
//!
//! Fixtures: `srp.json` (hkdf), `hap_frames.json`.

use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use sha2::Sha512;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};

use crate::rtsp::{RtspClient, RtspError};
use crate::srp::{SrpClient, SrpError, SrpProof};
use crate::tlv8::{self, tag};

/// HKDF salt/info pairs (pyatv `CONTROL_*` / `EVENTS_*`).
pub const CONTROL_SALT: &str = "Control-Salt";
pub const CONTROL_WRITE_INFO: &str = "Control-Write-Encryption-Key";
pub const CONTROL_READ_INFO: &str = "Control-Read-Encryption-Key";
pub const EVENTS_SALT: &str = "Events-Salt";
pub const EVENTS_WRITE_INFO: &str = "Events-Write-Encryption-Key";
pub const EVENTS_READ_INFO: &str = "Events-Read-Encryption-Key";

/// Most plaintext bytes in one frame (HAP specification, section 5.2.2).
pub const FRAME_LENGTH: usize = 1024;
/// Poly1305 tag length.
pub const TAG_LENGTH: usize = 16;

/// Headers of the pairing posts (`_AIRPLAY_HEADERS` in pyatv's `hap_transient`).
pub const PAIRING_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "AirPlay/320.20"),
    ("Connection", "keep-alive"),
    ("X-Apple-HKP", "4"),
    ("Content-Type", "application/octet-stream"),
];

/// `hkdf_expand(salt, info, secret)`: HKDF-SHA512 with a 32-byte output.
pub fn hkdf_expand(salt: &str, info: &str, secret: &[u8]) -> [u8; 32] {
    let hkdf = Hkdf::<Sha512>::new(Some(salt.as_bytes()), secret);
    let mut out = [0u8; 32];
    // 32 bytes is far below SHA-512's HKDF limit (255 * 64), so this cannot fail.
    if hkdf.expand(info.as_bytes(), &mut out).is_err() {
        unreachable!("HKDF-SHA512 output of 32 bytes is always valid");
    }
    out
}

/// A pair of ChaCha20-Poly1305 keys for one connection, from our point of view.
#[derive(Clone, PartialEq, Eq)]
pub struct CipherKeys {
    /// Key for data we send.
    pub output: [u8; 32],
    /// Key for data we receive.
    pub input: [u8; 32],
}

impl CipherKeys {
    /// The same keys from the peer's point of view (output and input swapped).
    pub fn swapped(&self) -> CipherKeys {
        CipherKeys {
            output: self.input,
            input: self.output,
        }
    }
}

impl std::fmt::Debug for CipherKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CipherKeys(..)")
    }
}

/// The 64-byte SRP session key `K` from transient pairing.
#[derive(Clone, PartialEq, Eq)]
pub struct SharedSecret([u8; 64]);

impl std::fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedSecret(..)")
    }
}

impl SharedSecret {
    pub fn new(key: [u8; 64]) -> SharedSecret {
        SharedSecret(key)
    }

    /// The raw key.
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    /// RTSP connection keys: write = Control-Write, read = Control-Read.
    pub fn control_keys(&self) -> CipherKeys {
        CipherKeys {
            output: hkdf_expand(CONTROL_SALT, CONTROL_WRITE_INFO, &self.0),
            input: hkdf_expand(CONTROL_SALT, CONTROL_READ_INFO, &self.0),
        }
    }

    /// Event channel keys: send = Events-Read, receive = Events-Write (pyatv passes
    /// `EVENTS_READ_INFO` as the output info).
    pub fn event_keys(&self) -> CipherKeys {
        CipherKeys {
            output: hkdf_expand(EVENTS_SALT, EVENTS_READ_INFO, &self.0),
            input: hkdf_expand(EVENTS_SALT, EVENTS_WRITE_INFO, &self.0),
        }
    }

    /// Audio key `shk` = Events-Salt / Events-Write-Encryption-Key.
    pub fn shk(&self) -> [u8; 32] {
        hkdf_expand(EVENTS_SALT, EVENTS_WRITE_INFO, &self.0)
    }
}

/// Framing errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HapError {
    #[error("HAP frame failed authentication")]
    Decrypt,
    #[error("HAP encryption failed")]
    Encrypt,
    #[error("HAP nonce counter exhausted")]
    CounterExhausted,
}

fn nonce(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(bytes)
}

fn cipher(key: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&(*key).into())
}

/// Encrypting half of a HAP connection.
pub struct HapWriter {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl HapWriter {
    pub fn new(key: &[u8; 32]) -> HapWriter {
        HapWriter {
            cipher: cipher(key),
            counter: 0,
        }
    }

    /// Encrypt `plaintext` into frames of at most 1024 bytes and append them to
    /// `out`. An empty plaintext produces nothing and does not use a nonce.
    pub fn encrypt_into(&mut self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), HapError> {
        out.reserve(plaintext.len() + plaintext.len().div_ceil(FRAME_LENGTH) * (2 + TAG_LENGTH));
        for frame in plaintext.chunks(FRAME_LENGTH) {
            // chunks(1024) keeps the length within u16.
            let length = (frame.len() as u16).to_le_bytes();
            let start = out.len();
            out.extend_from_slice(&length);
            out.extend_from_slice(frame);
            let nonce = nonce(self.counter);
            self.counter = self
                .counter
                .checked_add(1)
                .ok_or(HapError::CounterExhausted)?;
            let tag = self
                .cipher
                .encrypt_inout_detached(&nonce, &length, (&mut out[start + 2..]).into())
                .map_err(|_| HapError::Encrypt)?;
            out.extend_from_slice(&tag);
        }
        Ok(())
    }

    /// Encrypt `plaintext` into a new buffer.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, HapError> {
        let mut out = Vec::new();
        self.encrypt_into(plaintext, &mut out)?;
        Ok(out)
    }
}

/// Decrypting half of a HAP connection; buffers partial frames across reads.
pub struct HapReader {
    cipher: ChaCha20Poly1305,
    counter: u64,
    pending: Vec<u8>,
}

impl HapReader {
    pub fn new(key: &[u8; 32]) -> HapReader {
        HapReader {
            cipher: cipher(key),
            counter: 0,
            pending: Vec::new(),
        }
    }

    /// Feed received bytes; every complete frame is decrypted and appended to `out`.
    /// A failed tag check is fatal for the connection.
    pub fn decrypt(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), HapError> {
        self.pending.extend_from_slice(data);
        let mut pos = 0;
        while self.pending.len() - pos >= 2 {
            let length_bytes = [self.pending[pos], self.pending[pos + 1]];
            let length = u16::from_le_bytes(length_bytes) as usize;
            let frame_end = pos + 2 + length + TAG_LENGTH;
            if self.pending.len() < frame_end {
                break;
            }
            let (body, tag) = self.pending[pos + 2..frame_end].split_at(length);
            let tag = chacha20poly1305::Tag::try_from(tag).map_err(|_| HapError::Decrypt)?;
            let start = out.len();
            out.extend_from_slice(body);
            let nonce = nonce(self.counter);
            self.counter = self
                .counter
                .checked_add(1)
                .ok_or(HapError::CounterExhausted)?;
            if self
                .cipher
                .decrypt_inout_detached(&nonce, &length_bytes, (&mut out[start..]).into(), &tag)
                .is_err()
            {
                out.truncate(start);
                return Err(HapError::Decrypt);
            }
            pos = frame_end;
        }
        self.pending.drain(..pos);
        Ok(())
    }

    /// Bytes of an incomplete frame still waiting for more data.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Writer and reader for one connection.
pub fn hap_pair(keys: &CipherKeys) -> (HapWriter, HapReader) {
    (HapWriter::new(&keys.output), HapReader::new(&keys.input))
}

/// Both halves of a HAP connection in one value (`HAPSession` in pyatv).
pub struct HapSession {
    pub writer: HapWriter,
    pub reader: HapReader,
}

impl HapSession {
    pub fn new(keys: &CipherKeys) -> HapSession {
        let (writer, reader) = hap_pair(keys);
        HapSession { writer, reader }
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, HapError> {
        self.writer.encrypt(plaintext)
    }

    pub fn decrypt(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), HapError> {
        self.reader.decrypt(data, out)
    }
}

/// Why a transient pair-setup failed.
#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    /// The receiver refused or the exchange did not add up; worth retrying.
    #[error("{0}")]
    Rejected(String),
    /// The connection failed; retrying on it is pointless.
    #[error(transparent)]
    Transport(RtspError),
    /// Local failure (random number generator).
    #[error("{0}")]
    Local(String),
}

impl PairingError {
    /// True for failures a new pair-setup attempt may fix.
    pub fn is_retryable(&self) -> bool {
        matches!(self, PairingError::Rejected(_))
    }
}

impl From<RtspError> for PairingError {
    fn from(err: RtspError) -> PairingError {
        match err {
            RtspError::NotAuthenticated { .. } | RtspError::Status { .. } => {
                PairingError::Rejected(err.to_string())
            }
            other => PairingError::Transport(other),
        }
    }
}

/// Name of a HAP error code (pyatv `ErrorCode`).
fn error_name(code: u8) -> &'static str {
    match code {
        1 => "Unknown",
        2 => "Authentication",
        3 => "BackOff",
        4 => "MaxPeers",
        5 => "MaxTries",
        6 => "Unavailable",
        7 => "Busy",
        _ => "unrecognised",
    }
}

/// Fail with the receiver's Error TLV, if it sent one.
fn check_error_tlv(step: &str, tlv: &tlv8::Tlv8) -> Result<(), PairingError> {
    match tlv.get(tag::ERROR) {
        Some(value) => {
            let code = value.first().copied().unwrap_or(0);
            let backoff = tlv
                .get(tag::BACK_OFF)
                .map(|v| format!(", back off {} s", le_number(v)))
                .unwrap_or_default();
            Err(PairingError::Rejected(format!(
                "receiver answered {step} with error {code} ({}){backoff}",
                error_name(code)
            )))
        }
        None => Ok(()),
    }
}

fn le_number(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .take(8)
        .rev()
        .fold(0, |acc, b| (acc << 8) | u64::from(*b))
}

fn parse_tlv(step: &str, body: &[u8]) -> Result<tlv8::Tlv8, PairingError> {
    tlv8::decode(body).map_err(|err| PairingError::Rejected(format!("{step}: {err}")))
}

/// M1: Method = PairSetup, SeqNo = 1, Flags = TransientPairing.
pub fn m1() -> Vec<u8> {
    tlv8::encode(&[
        (tag::METHOD, &[0x00]),
        (tag::SEQ_NO, &[0x01]),
        (tag::FLAGS, &[0x10]),
    ])
}

/// Salt and public key `B` from the receiver's M2.
pub fn parse_m2(body: &[u8]) -> Result<(Vec<u8>, Vec<u8>), PairingError> {
    let tlv = parse_tlv("M2", body)?;
    check_error_tlv("M2", &tlv)?;
    let salt = tlv
        .get(tag::SALT)
        .ok_or_else(|| PairingError::Rejected("M2 has no salt".into()))?;
    let public = tlv
        .get(tag::PUBLIC_KEY)
        .ok_or_else(|| PairingError::Rejected("M2 has no public key".into()))?;
    Ok((salt.to_vec(), public.to_vec()))
}

/// M3: SeqNo = 3, PublicKey = A, Proof = M1 (in that order, like pyatv).
pub fn m3(public_key: &[u8], proof: &[u8]) -> Vec<u8> {
    tlv8::encode(&[
        (tag::SEQ_NO, &[0x03]),
        (tag::PUBLIC_KEY, public_key),
        (tag::PROOF, proof),
    ])
}

/// Check M4: an Error TLV fails the pairing; the server proof is only logged
/// (pyatv does not look at M4 at all).
pub fn check_m4(body: &[u8], proof: &SrpProof) -> Result<(), PairingError> {
    let tlv = parse_tlv("M4", body)?;
    check_error_tlv("M4", &tlv)?;
    match tlv.get(tag::PROOF) {
        Some(server_proof) if proof.verify_server_proof(server_proof) => {
            debug!("pair-setup M4: receiver proof verified");
        }
        Some(_) => warn!("pair-setup M4: receiver proof does not match (continuing)"),
        None => debug!("pair-setup M4 carries no proof"),
    }
    Ok(())
}

/// Run one transient pair-setup (M1..M4) over `client` (still unencrypted) and
/// return the shared secret. The caller decides whether to retry.
pub async fn transient_pair_setup<S>(
    client: &mut RtspClient<S>,
) -> Result<SharedSecret, PairingError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let srp = SrpClient::generate().map_err(|err| PairingError::Local(err.to_string()))?;

    client
        .post_http("/pair-pin-start", &PAIRING_HEADERS, &[])
        .await?;
    let m2 = client
        .post_http("/pair-setup", &PAIRING_HEADERS, &m1())
        .await?;
    let (salt, server_public) = parse_m2(&m2.body)?;

    let proof = srp
        .process(&salt, &server_public)
        .map_err(|err| match err {
            SrpError::InvalidServerKey => PairingError::Rejected(err.to_string()),
            SrpError::Random(_) => PairingError::Local(err.to_string()),
        })?;
    let m4 = client
        .post_http(
            "/pair-setup",
            &PAIRING_HEADERS,
            &m3(srp.public_key(), proof.client_proof()),
        )
        .await?;
    check_m4(&m4.body, &proof)?;
    Ok(SharedSecret::new(*proof.session_key()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;

    fn json(text: &str) -> Json {
        serde_json::from_str(text).expect("fixture")
    }

    fn unhex(value: &Json) -> Vec<u8> {
        hex::decode(value.as_str().expect("hex string")).expect("valid hex")
    }

    fn key32(value: &Json) -> [u8; 32] {
        unhex(value).try_into().expect("32 bytes")
    }

    #[test]
    fn hkdf_keys_match_pyatv() {
        let fx = json(include_str!("../tests/fixtures/srp.json"));
        for v in fx["vectors"].as_array().unwrap() {
            let case = v["case"].as_str().unwrap();
            let secret = SharedSecret::new(unhex(&v["K"]).try_into().unwrap());
            let h = &v["hkdf"];
            let control = secret.control_keys();
            assert_eq!(control.output, key32(&h["control_write"]), "{case}");
            assert_eq!(control.input, key32(&h["control_read"]), "{case}");
            let events = secret.event_keys();
            assert_eq!(events.output, key32(&h["events_read"]), "{case}");
            assert_eq!(events.input, key32(&h["events_write"]), "{case}");
            assert_eq!(secret.shk(), key32(&h["events_write"]), "{case}");
        }
    }

    #[test]
    fn shk_matches_setup_fixture() {
        let srp = json(include_str!("../tests/fixtures/srp.json"));
        let setup = json(include_str!("../tests/fixtures/setup_bodies.json"));
        let v = srp["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["case"] == "a_32_bytes")
            .unwrap();
        let secret = SharedSecret::new(unhex(&v["K"]).try_into().unwrap());
        assert_eq!(secret.shk(), key32(&setup["events_write_key"]));
    }

    #[test]
    fn m3_matches_fixture() {
        let fx = json(include_str!("../tests/fixtures/srp.json"));
        for v in fx["vectors"].as_array().unwrap() {
            assert_eq!(m3(&unhex(&v["A"]), &unhex(&v["M1"])), unhex(&v["m3_tlv"]));
        }
    }

    /// (plaintext, ciphertext) pairs.
    type Frames = Vec<(Vec<u8>, Vec<u8>)>;

    fn frames_fixture() -> (CipherKeys, Frames) {
        let fx = json(include_str!("../tests/fixtures/hap_frames.json"));
        let keys = CipherKeys {
            output: key32(&fx["output_key"]),
            input: key32(&fx["input_key"]),
        };
        let messages = fx["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (unhex(&m["plaintext"]), unhex(&m["ciphertext"])))
            .collect();
        (keys, messages)
    }

    #[test]
    fn frames_match_pyatv() {
        let (keys, messages) = frames_fixture();
        let mut writer = HapWriter::new(&keys.output);
        for (plaintext, ciphertext) in &messages {
            assert_eq!(
                &writer.encrypt(plaintext).unwrap(),
                ciphertext,
                "{}",
                plaintext.len()
            );
        }
        // The empty message must not have consumed a nonce: 5 messages, 1+1+1+2+3 frames.
        assert_eq!(writer.counter, 8);
    }

    #[test]
    fn peer_with_swapped_keys_decrypts_whole_and_in_pieces() {
        let (keys, messages) = frames_fixture();
        let stream: Vec<u8> = messages.iter().flat_map(|(_, c)| c.clone()).collect();
        let expected: Vec<u8> = messages.iter().flat_map(|(p, _)| p.clone()).collect();

        let mut peer = HapReader::new(&keys.swapped().input);
        let mut out = Vec::new();
        peer.decrypt(&stream, &mut out).unwrap();
        assert_eq!(out, expected);

        let mut peer = HapReader::new(&keys.output);
        let mut out = Vec::new();
        for piece in stream.chunks(777) {
            peer.decrypt(piece, &mut out).unwrap();
        }
        assert_eq!(out, expected);
        assert_eq!(peer.pending_len(), 0);
    }

    #[test]
    fn tampered_frame_is_rejected() {
        let keys = CipherKeys {
            output: [3; 32],
            input: [4; 32],
        };
        let mut session = HapSession::new(&keys);
        let mut frames = session.encrypt(b"hello").unwrap();
        frames[3] ^= 1;
        let mut peer = HapSession::new(&keys.swapped());
        let mut out = Vec::new();
        assert_eq!(peer.decrypt(&frames, &mut out), Err(HapError::Decrypt));
        assert!(out.is_empty());
    }

    #[test]
    fn m2_and_m4_parsing() {
        let fx = json(include_str!("../tests/fixtures/tlv8.json"));
        let case = |name: &str| {
            unhex(
                &fx["decode_cases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["name"] == name)
                    .unwrap()["encoded"],
            )
        };
        let (salt, public) = parse_m2(&case("m2_salt_and_public_key")).unwrap();
        assert_eq!((salt.len(), public.len()), (16, 384));

        let err = parse_m2(&case("error_authentication")).unwrap_err();
        assert!(err.is_retryable());
        assert!(err.to_string().contains("Authentication"), "{err}");
        let err = parse_m2(&case("error_backoff")).unwrap_err();
        assert!(err.to_string().contains("back off 60 s"), "{err}");
        assert!(parse_m2(&case("m1")).is_err());

        let srp = SrpClient::new(&[5; 32]);
        let proof = srp.process(&salt, &public).unwrap();
        assert!(check_m4(&case("error_authentication"), &proof).is_err());
        // A wrong proof is only logged.
        assert!(check_m4(&case("m4_proof"), &proof).is_ok());
        let good = tlv8::encode(&[
            (tag::SEQ_NO, &[4]),
            (tag::PROOF, proof.expected_server_proof()),
        ]);
        assert!(check_m4(&good, &proof).is_ok());
    }

    #[test]
    fn transport_errors_are_not_retryable() {
        assert!(!PairingError::from(RtspError::Closed).is_retryable());
        assert!(
            PairingError::from(RtspError::NotAuthenticated {
                method: "POST".into(),
                code: 403
            })
            .is_retryable()
        );
    }
}
