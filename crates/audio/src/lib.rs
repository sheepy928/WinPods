//! Windows audio input for WinPods (workstream: audio). CONTRACT: the public items in
//! `devices`, `watch` and `capture` are used by the engine and UI; keep signatures.
//!
//! Port of `legacy/python/winpods/capture.py` + `tone.py` on top of the `wasapi`
//! crate: loopback capture of render devices (default device follows Windows
//! default-device changes), recording devices, conversion of any mix format/rate to
//! 44.1 kHz stereo i16 frames pushed into the `rt` ring, and a generated test tone.

pub mod capture;
pub mod devices;
pub mod resample;
pub mod tone;
pub mod watch;

pub use capture::{CaptureError, CaptureExit, CaptureHandle, CaptureInfo, CaptureSource};
pub use devices::{DeviceInfo, DeviceKind};
pub use watch::{DeviceEvent, DeviceWatch};
