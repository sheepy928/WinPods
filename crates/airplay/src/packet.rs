//! RTP audio packet assembly and encryption (workstream: real-time).
//!
//! Header `0x80, 0xE0 (first) / 0x60, seq u16, ts u32, ssrc u32` (big-endian);
//! payload sealed with ChaCha20-Poly1305 keyed by `shk`, nonce = 4 zero bytes +
//! u64 LE counter (starts at 0, +1 per packet incl. silence), AAD = header[4..12];
//! packet = header + ciphertext + tag + the 8-byte LE counter used. No allocation.
//! Fixture: `audio_packets.json`.
