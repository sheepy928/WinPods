//! Engine ↔ UI contract. FROZEN: the UI (apps/winpods) and CLI depend on these.

use std::sync::Arc;

pub use winpods_airplay::{Codec, TimingProtocol};
pub use winpods_audio::{DeviceInfo, DeviceKind};

/// Connection state shown in the UI and tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum State {
    #[default]
    Disconnected,
    Connecting,
    Streaming,
    Reconnecting,
    Error,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Disconnected => "Disconnected",
            State::Connecting => "Connecting",
            State::Streaming => "Streaming",
            State::Reconnecting => "Reconnecting",
            State::Error => "Error",
        }
    }
}

/// What to capture, as stored in config: `"default"` or a device display name, plus
/// an optional endpoint id that wins when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSelection {
    pub name: String,
    pub id: Option<String>,
}

impl Default for CaptureSelection {
    fn default() -> Self {
        Self::default_output()
    }
}

impl CaptureSelection {
    pub const DEFAULT: &'static str = "default";
    pub fn default_output() -> Self {
        Self {
            name: Self::DEFAULT.to_string(),
            id: None,
        }
    }
    pub fn is_default(&self) -> bool {
        self.id.is_none() && self.name.eq_ignore_ascii_case(Self::DEFAULT)
    }
}

/// Engine start-up settings (built from config + CLI overrides; the engine never
/// writes config itself).
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub device_ip: String,
    pub latency_ms: u32,
    pub capture: CaptureSelection,
    /// Receiver volume in percent; `None` = do not touch the receiver volume.
    pub volume: Option<f32>,
    pub timing: TimingProtocol,
    pub codec: Codec,
    pub sender_name: String,
    /// Stored random MAC used as deviceID.
    pub device_id: String,
    /// Stream the generated test tone instead of capturing.
    pub test_tone: bool,
    /// Diagnostic: hold back every Nth packet (`--drop-every`).
    pub drop_every: Option<u32>,
    /// Fail instead of falling back to a dynamic port when UDP 319/320 are taken
    /// (the CLI preflight uses this during hardware tests).
    pub require_standard_ptp_ports: bool,
}

/// Commands from the UI/CLI.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Connect,
    Disconnect,
    SetLatency(u32),
    SetCapture(CaptureSelection),
    SetDeviceIp(String),
    /// `None` = stop controlling the receiver volume.
    SetVolume(Option<f32>),
    SetTiming(TimingProtocol),
    SetCodec(Codec),
    /// Re-enumerate capture devices into `Snapshot::devices`.
    RefreshDevices,
    /// Run a ~5 s network scan into `Snapshot::scan`.
    Scan,
    Shutdown,
}

/// A receiver found by a network scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub name: String,
    pub address: String,
    pub model: String,
}

/// Progress of the last scan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ScanState {
    #[default]
    Idle,
    Running,
    Done(Arc<Vec<ScanResult>>),
    Failed(String),
}

/// Everything the UI shows. Cheap to clone (big lists are `Arc`).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub state: State,
    /// Last error (connect failure, stream end reason), empty when none.
    pub error: String,
    /// Seconds until the next reconnect attempt (Reconnecting only).
    pub retry_in_s: f32,

    // receiver
    pub device_ip: String,
    pub device_name: String,
    pub device_model: String,
    /// Name discovered by the engine this run, for the config owner to persist.
    pub discovered_device_name: Option<String>,

    // stream settings in effect
    pub latency_ms: u32,
    pub codec: Codec,
    pub timing: TimingProtocol,
    pub volume: Option<f32>,
    /// `arrivalToRenderLatencyMs` reported by the receiver.
    pub render_ms: Option<u32>,

    // capture
    pub capture: CaptureSelection,
    pub capture_name: String,
    pub capture_rate: u32,
    pub capture_format: String,
    pub devices: Arc<Vec<DeviceInfo>>,

    // clock
    /// PTP locked, or NTP timing requests arriving.
    pub clock_ok: bool,
    pub ptp_offset_ms: f64,
    pub ptp_spread_ms: f64,
    pub ptp_syncs: u64,
    pub timing_requests: u64,

    // stream statistics
    pub uptime_s: f64,
    pub packets: u64,
    pub silence_packets: u64,
    pub frames_trimmed: u64,
    pub stalls: u64,
    pub retransmit_requests: u64,
    pub retransmitted: u64,
    pub anchors: u64,
    pub send_errors: u64,
    pub withheld: u64,
    pub ring_ms: f32,
    pub ring_dropped: u64,
    pub capture_discontinuities: u64,
    pub pairing_retries: u32,
    /// Latest receiver feedback (diagnostics only).
    pub feedback: Option<String>,

    pub scan: ScanState,
}

/// The engine as seen by the UI/CLI. Implemented by [`crate::EngineHandle`] and by
/// mocks in UI tests/demo mode.
pub trait EngineApi: Send + Sync {
    fn send(&self, command: Command);
    fn snapshot(&self) -> Arc<Snapshot>;
    /// Current capture peak (0..=1) for the level meter; cheaper than a snapshot.
    fn peak(&self) -> f32;
}
