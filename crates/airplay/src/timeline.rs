//! Smoothed send timeline (workstream: real-time).
//!
//! Port of `Timeline` in `legacy/python/winpods/raop_live.py`: maps
//! `rt::mono_ns()` to the RTP head of the frames we send:
//! `head_at(t) = r0 + (t - t0)·44100/1e9 + err`, `observe(t, head)` updates
//! `err += 0.005·((head − nominal(t)) − err)` after every packet. Head values are
//! unwrapped u64/f64 (never u32). The sender publishes the state for the control
//! thread through a seqlock/ArcSwap-style shared cell. Fixture: `timeline.json`.
