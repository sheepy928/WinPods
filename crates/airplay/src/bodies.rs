//! SETUP / SET_PARAMETER / metadata bodies and response parsing
//! (workstream: control plane).
//!
//! Base and stream SETUP plists exactly as `legacy/python/winpods/raop_live.py`
//! builds them (see the plan appendix and `tests/fixtures/setup_bodies.json`),
//! `latencyMin = min(L, 11025)`, `latencyMax = max(L, 88200)` with
//! `L = crate::latency_frames(ms)`; volume `-30 + 0.3·pct` dB (−144 at 0 %)
//! formatted like Python `str(float)`; DMAP `mlit{minm, asal, asar}` metadata.
//! Fixtures: `setup_bodies.json`, `volume.json`.
