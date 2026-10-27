//! Control channel: timing anchors and retransmissions (workstream: real-time).
//!
//! * PTP anchor 0xD7, 28 bytes `>BBHIQIQ`: 0x90 (first) / 0x80, 0xD7, 0x0006,
//!   (head − latency) as u32, master time ns u64, head as u32, master clock id u64.
//! * NTP sync 0xD4, 20 bytes: 0x90/0x80, 0xD4, 0x0007, (head − latency) u32,
//!   ntp seconds u32, ntp fraction u32, head u32.
//! * Retransmit request: `byte1 & 0x7F == 0x55`, first lost seq at offset 4 (u16),
//!   count at offset 6 (u16); reply `0x80 0xD6` + seq (2 bytes) + stored packet.
//!
//! Runs on a dedicated above-normal thread: anchor every 1 s (first one before any
//! audio), retransmit replies as requests arrive; tolerates WSAECONNRESET.
//! Fixture: `anchors.json`.
