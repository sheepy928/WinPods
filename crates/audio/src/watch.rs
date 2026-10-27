//! Device enumeration and default-device notifications. CONTRACT.
//!
//! A dedicated `device-watch` thread initialises COM as MTA, owns the wasapi
//! `DeviceEnumerator` and the notification registration for the whole process, and
//! serves list/default requests over a channel. The notification callback stays
//! trivial: ignore non-Render directions and the Communications role, forward the
//! new default id for Console/Multimedia with a non-blocking send. A 5 s poll of
//! `get_default_device_for_role(Render, Multimedia)` backs it up. Events with the
//! same id as the last one sent are suppressed.

use tokio::sync::mpsc::UnboundedSender;

use crate::devices::DeviceInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// The default render device (Multimedia role) changed to the endpoint `id`.
    DefaultRenderChanged { id: String },
}

/// Handle to the device-watch thread. Dropping it stops the thread.
pub struct DeviceWatch {
    _private: (),
}

impl DeviceWatch {
    pub fn start(events: UnboundedSender<DeviceEvent>) -> std::io::Result<DeviceWatch> {
        let _ = events;
        todo!("audio: DeviceWatch::start")
    }
    /// All loopback-capable render endpoints (as `Loopback`) followed by recording
    /// endpoints (as `Input`), active devices only, sorted by display name.
    pub fn list(&self) -> std::io::Result<Vec<DeviceInfo>> {
        todo!("audio: DeviceWatch::list")
    }
    /// The current default render endpoint (Multimedia role).
    pub fn default_render(&self) -> std::io::Result<Option<DeviceInfo>> {
        todo!("audio: DeviceWatch::default_render")
    }
}
