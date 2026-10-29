//! AirPlay 2 audio sender for WinPods.
//!
//! Ported from the Python v0.1 implementation (`legacy/python/winpods/raop_live.py`,
//! `ptp.py`, `alac.py`) and the parts of pyatv it used. Those sources are normative;
//! the JSON fixtures in `tests/fixtures/` were generated from them and must match
//! byte for byte (plists: by meaning).
//!
//! Layout (each module is owned by one workstream; shared types live in [`types`]):
//!
//! * control plane — [`tlv8`], [`srp`], [`hap`], [`rtsp`], [`bodies`],
//!   [`event_channel`], [`session`]
//! * real-time path — [`alac`], [`packet`], [`timeline`], [`backlog`], [`ptp`],
//!   [`control`], [`ntp`], [`sender`], [`servo`], [`runtime`]
//! * discovery — [`mdns`]

pub mod types;

// control plane
pub mod bodies;
pub mod event_channel;
pub mod hap;
pub mod rtsp;
pub mod session;
pub mod srp;
pub mod tlv8;

// real-time path
pub mod alac;
pub mod backlog;
pub mod control;
pub mod ntp;
pub mod packet;
pub mod ptp;
pub mod runtime;
pub mod sender;
pub mod servo;
pub mod timeline;

// discovery
pub mod mdns;

pub use types::*;
