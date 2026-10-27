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
