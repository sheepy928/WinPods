//! `%APPDATA%\WinPods\config.json`. CONTRACT: field names and defaults are frozen
//! (they match the Python v0.1 keys so existing configs load; unknown keys such as
//! v0.1's `firewall_punch` are ignored).
//!
//! Ownership rule: one owner per process (UI thread in WinPods.exe, main thread in
//! winpods-cli). The engine never writes config. Saves are atomic (write a temp file
//! in the same folder, then rename over the original). The CLI never saves its
//! overrides; it only persists `device_id`/`device_name` via [`Config::persist_identity`].

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Receiver IPv4 address ("" = not set yet).
    pub device_ip: String,
    /// Receiver name learned by discovery (display only).
    pub device_name: String,
    pub latency_ms: u32,
    /// "default" or a capture device display name (loopbacks end in " [Loopback]").
    pub capture_device: String,
    /// WASAPI endpoint id of `capture_device`, when known; wins over the name.
    pub capture_device_id: Option<String>,
    /// Receiver volume in percent; `None` = leave the receiver volume alone.
    pub volume: Option<f32>,
    pub auto_connect: bool,
    pub start_minimized: bool,
    /// "PTP" or "NTP".
    pub timing_protocol: String,
    /// "alac" or "pcm".
    pub codec: String,
    pub sender_name: String,
    /// "DEBUG", "INFO", "WARNING", "ERROR".
    pub log_level: String,
    /// Random locally administered MAC "XX:XX:XX:XX:XX:XX" used as AirPlay deviceID.
    /// Generated on first load when missing.
    pub device_id: Option<String>,
    pub start_with_windows: bool,
    pub auto_update: bool,
    /// Release tag the user chose to skip.
    pub skipped_version: Option<String>,
    /// Unix seconds of the last successful update check.
    pub last_update_check: Option<u64>,
    /// The one-time "still running in the tray" hint was shown.
    pub close_hint_shown: bool,
    /// "auto" or "software" (Slint renderer).
    pub renderer: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_ip: String::new(),
            device_name: String::new(),
            latency_ms: 100,
            capture_device: "default".to_string(),
            capture_device_id: None,
            volume: None,
            auto_connect: true,
            start_minimized: false,
            timing_protocol: "PTP".to_string(),
            codec: "alac".to_string(),
            sender_name: "WinPods".to_string(),
            log_level: "INFO".to_string(),
            device_id: None,
            start_with_windows: false,
            auto_update: true,
            skipped_version: None,
            last_update_check: None,
            close_hint_shown: false,
            renderer: "auto".to_string(),
        }
    }
}

impl Config {
    /// Load from the standard path; defaults when missing or unreadable (logged).
    /// Generates and immediately persists `device_id` when absent.
    pub fn load() -> Config {
        todo!("config: load")
    }

    /// Load from an explicit path (missing file → defaults; parse error → Err).
    pub fn load_from(path: &Path) -> std::io::Result<Config> {
        let _ = path;
        todo!("config: load_from")
    }

    /// Atomically save to the standard path.
    pub fn save(&self) -> std::io::Result<()> {
        todo!("config: save")
    }

    /// Atomically save to an explicit path.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let _ = path;
        todo!("config: save_to")
    }

    /// Reload the file from disk and change only `device_id` and `device_name`
    /// (used by the CLI so its overrides never leak into the file).
    pub fn persist_identity(device_id: &str, device_name: &str) -> std::io::Result<()> {
        let _ = (device_id, device_name);
        todo!("config: persist_identity")
    }

    /// Latency clamped to 75..=5000 ms.
    pub fn latency_clamped(&self) -> u32 {
        self.latency_ms.clamp(75, 5000)
    }
}
