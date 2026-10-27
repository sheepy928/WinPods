//! The WinPods engine: discovers the receiver, runs capture, the AirPlay session and
//! the real-time stream, reconnects with backoff, and publishes status snapshots.
//!
//! The UI and CLI talk to it only through [`EngineApi`] (commands in, snapshots out),
//! so the GUI can be developed and screenshot against a mock.

pub mod connect;
pub mod scan;
pub mod supervisor;
pub mod types;

pub use supervisor::EngineHandle;
pub use types::*;
