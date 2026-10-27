//! Uncompressed ALAC escape frames and PCM payloads (workstream: real-time).
//!
//! Port of `legacy/python/winpods/alac.py`: header `(1<<20)|(1<<3)|1` in 23 bits,
//! 32-bit frame count (352), samples L then R as 16-bit MSB-first starting at bit
//! offset 55, END tag `0b111`, zero padding → 1416 bytes. PCM payload = the same
//! frames as `i16::to_be_bytes`, 1408 bytes. Inputs are native `[i16; 2]` frames.
//! Must not allocate (writes into caller buffers). Fixture: `alac.json`.
