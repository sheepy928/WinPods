//! Supervisor (integration step, owned by the lead). Port of
//! `legacy/python/winpods/engine.py`: desired state, reconnect backoff
//! 2/3/5/10/20/30 s, restart on settings change, capture restarts on device loss or
//! default-device change, snapshots via `ArcSwap`.

use std::sync::Arc;

use crate::types::{Command, EngineApi, EngineSettings, Snapshot};

/// Handle to the engine thread (tokio current-thread runtime inside).
pub struct EngineHandle {
    _private: (),
}

impl EngineHandle {
    pub fn start(settings: EngineSettings) -> std::io::Result<EngineHandle> {
        let _ = settings;
        todo!("engine: start")
    }

    /// Stop streaming (TEARDOWN) and end the engine thread; blocks up to ~8 s.
    pub fn shutdown(&self) {
        todo!("engine: shutdown")
    }
}

impl EngineApi for EngineHandle {
    fn send(&self, command: Command) {
        let _ = command;
        todo!("engine: send")
    }
    fn snapshot(&self) -> Arc<Snapshot> {
        todo!("engine: snapshot")
    }
    fn peak(&self) -> f32 {
        todo!("engine: peak")
    }
}
