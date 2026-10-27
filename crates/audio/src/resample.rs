//! Streaming conversion to 44.1 kHz stereo (workstream: audio).
//!
//! rubato 5 `Async` sinc (or `Fft`) resampler driven with `process_into_buffer` and
//! preallocated `audioadapter-buffers` adapters; the input accumulator is sized from
//! `input_frames_next()` / `output_frames_max()` instead of a hard-coded chunk.
//! Bypass when the device already runs at 44.1 kHz. Log `output_delay()` per device.
//! On capture timeout, flush partial input (Indexing partial_len or zero padding).
//! No allocation after construction.
