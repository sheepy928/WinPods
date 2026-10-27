//! Test tone: 880 Hz, ~120 ms beep with 5 ms ramps, once per second, about −22 dBFS
//! (workstream: audio). Port of `legacy/python/winpods/tone.py`; delivered in
//! ~5 ms chunks (220 frames) paced by an `rt::HiResTimer`, so the sender sees the
//! same bursty arrival pattern as real capture. Plays only on the receiver.
