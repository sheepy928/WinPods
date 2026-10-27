//! Audio endpoint description. CONTRACT.

/// Whether an endpoint is captured as loopback (a playback device) or as a recording
/// device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Loopback,
    Input,
}

/// One capturable endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// WASAPI endpoint id (stable across renames).
    pub id: String,
    /// Friendly name, e.g. "CABLE Input (VB-Audio Virtual Cable)".
    pub name: String,
    /// Name shown in the UI and stored in config: loopbacks get " [Loopback]"
    /// appended, matching the Python v0.1 config values.
    pub display_name: String,
    pub kind: DeviceKind,
    /// Mix-format sample rate and channel count.
    pub sample_rate: u32,
    pub channels: u16,
    /// True for the current default render device (Multimedia role).
    pub is_default: bool,
}
