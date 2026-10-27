//! `%APPDATA%\WinPods\config.json`. CONTRACT: field names and defaults are frozen
//! (they match the Python v0.1 keys so existing configs load; unknown keys such as
//! v0.1's `firewall_punch` are ignored).
//!
//! Ownership rule: one owner per process (UI thread in WinPods.exe, main thread in
//! winpods-cli). The engine never writes config. Saves are atomic (write a temp file
//! in the same folder, then rename over the original). The CLI never saves its
//! overrides; it only persists `device_id`/`device_name` via [`Config::persist_identity`].
//!
//! Loading is lenient like v0.1: unknown keys are ignored and a known key with a value
//! of the wrong type keeps its default (logged) instead of discarding the whole file.
//! Only a file that is not a JSON object at all counts as a parse error.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{paths, win};

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
        Self::load_or_init(&paths::config_path())
    }

    /// Load from an explicit path (missing file → defaults; parse error → Err).
    pub fn load_from(path: &Path) -> std::io::Result<Config> {
        match fs::read(path) {
            Ok(bytes) => Self::from_json(&bytes, path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomically save to the standard path.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&paths::config_path())
    }

    /// Atomically save to an explicit path.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        json.push(b'\n');
        win::write_atomic(path, &json)
    }

    /// Reload the file from disk and change only `device_id` and `device_name`
    /// (used by the CLI so its overrides never leak into the file).
    pub fn persist_identity(device_id: &str, device_name: &str) -> std::io::Result<()> {
        Self::persist_identity_at(&paths::config_path(), device_id, device_name)
    }

    /// Latency clamped to 75..=5000 ms.
    pub fn latency_clamped(&self) -> u32 {
        self.latency_ms.clamp(75, 5000)
    }

    /// Give the config a `device_id` when it has none (or an invalid one). Returns
    /// true when a new id was generated (the caller should persist it).
    pub fn ensure_device_id(&mut self) -> bool {
        if self.device_id.as_deref().is_some_and(is_valid_device_id) {
            return false;
        }
        if let Some(bad) = &self.device_id {
            tracing::warn!("config: replacing invalid device_id {bad:?}");
        }
        self.device_id = Some(generate_device_id());
        true
    }

    /// [`Config::load`] for an explicit path.
    ///
    /// * Missing file → defaults.
    /// * Not a JSON object → error logged, the file is kept as `config.json.bad`, and
    ///   the defaults are used (and written, so the next start is clean).
    /// * Unreadable (locked, access denied) → defaults for this run only; nothing is
    ///   written so a transient error never destroys the user's settings.
    ///
    /// A generated `device_id` is persisted at once whenever the file may be written.
    pub(crate) fn load_or_init(path: &Path) -> Config {
        let (mut config, writable) = match Self::load_from(path) {
            Ok(config) => (config, true),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                tracing::error!("could not parse {}: {e}; using defaults", path.display());
                let backup = win::with_suffix(path, ".bad");
                match win::move_file(path, &backup, true) {
                    Ok(()) => {
                        tracing::warn!("kept the unreadable config as {}", backup.display());
                        (Config::default(), true)
                    }
                    Err(e) => {
                        tracing::warn!("could not keep a copy of the unreadable config: {e}");
                        (Config::default(), false)
                    }
                }
            }
            Err(e) => {
                tracing::error!("could not read {}: {e}; using defaults", path.display());
                (Config::default(), false)
            }
        };
        if config.ensure_device_id() {
            if writable {
                match config.save_to(path) {
                    Ok(()) => tracing::info!("generated device id {:?}", config.device_id),
                    Err(e) => tracing::warn!("could not save the new device id: {e}"),
                }
            } else {
                tracing::warn!("using a temporary device id for this run");
            }
        }
        config
    }

    /// [`Config::persist_identity`] for an explicit path. Edits the JSON object in
    /// place, so every other key (unknown ones included) is kept exactly as it was.
    pub(crate) fn persist_identity_at(
        path: &Path,
        device_id: &str,
        device_name: &str,
    ) -> io::Result<()> {
        let mut map = match fs::read(path) {
            Ok(bytes) => parse_object(&bytes, path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let config = Config {
                    device_id: Some(device_id.to_string()),
                    device_name: device_name.to_string(),
                    ..Config::default()
                };
                return config.save_to(path);
            }
            Err(e) => return Err(e),
        };
        map.insert("device_id".into(), Value::String(device_id.to_string()));
        map.insert("device_name".into(), Value::String(device_name.to_string()));
        let mut json = serde_json::to_vec_pretty(&Value::Object(map)).map_err(io::Error::other)?;
        json.push(b'\n');
        win::write_atomic(path, &json)
    }

    /// Lenient decode: known keys overlay the defaults one by one; a key whose value
    /// does not fit the field keeps the default and is logged.
    fn from_json(bytes: &[u8], path: &Path) -> io::Result<Config> {
        let file = parse_object(bytes, path)?;
        let defaults = match serde_json::to_value(Config::default()) {
            Ok(Value::Object(map)) => map,
            _ => return Err(io::Error::other("config defaults do not serialize")),
        };
        let mut merged = defaults.clone();
        for (key, value) in &file {
            if merged.contains_key(key) {
                merged.insert(key.clone(), value.clone());
            }
        }
        if let Ok(config) = serde_json::from_value::<Config>(Value::Object(merged)) {
            return Ok(config);
        }
        // Slow path: find the offending keys.
        let mut good = defaults;
        for (key, value) in file {
            if !good.contains_key(&key) {
                continue;
            }
            let mut candidate = good.clone();
            candidate.insert(key.clone(), value);
            if serde_json::from_value::<Config>(Value::Object(candidate.clone())).is_ok() {
                good = candidate;
            } else {
                tracing::warn!(
                    "config {}: ignoring invalid value for {key:?}",
                    path.display()
                );
            }
        }
        Ok(serde_json::from_value(Value::Object(good)).unwrap_or_default())
    }
}

/// Parse `bytes` (optionally with a UTF-8 BOM, as Notepad writes it) as a JSON object.
fn parse_object(bytes: &[u8], path: &Path) -> io::Result<Map<String, Value>> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a JSON object", path.display()),
        )),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

/// True for `XX:XX:XX:XX:XX:XX` with six hex octets (either case).
pub fn is_valid_device_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A random locally administered unicast MAC (`XX:XX:XX:XX:XX:XX`, upper case): the
/// first octet has bit 1 (locally administered) set and bit 0 (multicast) clear, so
/// it can never collide with a real vendor-assigned address.
pub fn generate_device_id() -> String {
    let mut octets = [0u8; 6];
    if getrandom::fill(&mut octets).is_err() {
        // Practically unreachable; fall back to std's randomly seeded hasher.
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        hasher.write_u32(std::process::id());
        octets.copy_from_slice(&hasher.finish().to_le_bytes()[..6]);
    }
    octets[0] = (octets[0] | 0x02) & !0x01;
    octets
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvVarGuard;

    fn temp_config() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        (dir, path)
    }

    #[test]
    fn defaults_match_the_frozen_contract() {
        let c = Config::default();
        assert_eq!(c.latency_ms, 100);
        assert_eq!(c.capture_device, "default");
        assert!(c.auto_connect && c.auto_update);
        assert_eq!(c.timing_protocol, "PTP");
        assert_eq!(c.codec, "alac");
        assert_eq!(c.log_level, "INFO");
        assert_eq!(c.renderer, "auto");
        assert_eq!(c.device_id, None);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let (_dir, path) = temp_config();
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn python_v01_file_loads_and_unknown_keys_are_ignored() {
        let (_dir, path) = temp_config();
        fs::write(
            &path,
            r#"{
  "device_ip": "192.168.1.155",
  "device_name": "Living Room",
  "latency_ms": 250,
  "capture_device": "CABLE Input (VB-Audio Virtual Cable) [Loopback]",
  "volume": 40,
  "auto_connect": false,
  "start_minimized": true,
  "timing_protocol": "NTP",
  "codec": "pcm",
  "sender_name": "Desk PC",
  "log_level": "DEBUG",
  "firewall_punch": true
}"#,
        )
        .unwrap();
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.device_ip, "192.168.1.155");
        assert_eq!(c.device_name, "Living Room");
        assert_eq!(c.latency_ms, 250);
        assert_eq!(c.volume, Some(40.0));
        assert!(!c.auto_connect);
        assert!(c.start_minimized);
        assert_eq!(c.timing_protocol, "NTP");
        assert_eq!(c.codec, "pcm");
        assert_eq!(c.sender_name, "Desk PC");
        assert_eq!(c.log_level, "DEBUG");
        // keys new in 0.2 keep their defaults
        assert_eq!(c.renderer, "auto");
        assert!(c.auto_update);
    }

    #[test]
    fn wrong_typed_values_keep_their_defaults() {
        let (_dir, path) = temp_config();
        fs::write(
            &path,
            "\u{feff}{\"latency_ms\": \"fast\", \"device_ip\": null, \"codec\": \"pcm\", \"volume\": null}",
        )
        .unwrap();
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.latency_ms, 100);
        assert_eq!(c.device_ip, "");
        assert_eq!(c.codec, "pcm");
        assert_eq!(c.volume, None);
    }

    #[test]
    fn invalid_json_is_an_error() {
        let (_dir, path) = temp_config();
        fs::write(&path, "{ not json").unwrap();
        let e = Config::load_from(&path).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        fs::write(&path, "[1, 2]").unwrap();
        let e = Config::load_from(&path).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn save_round_trips_atomically() {
        let (dir, path) = temp_config();
        let c = Config {
            device_ip: "10.0.0.2".into(),
            volume: Some(55.5),
            capture_device_id: Some("{0.0.0.00000000}.{abc}".into()),
            skipped_version: Some("v0.3.0".into()),
            last_update_check: Some(1_700_000_000),
            ..Config::default()
        };
        c.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), c);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\n  \"device_ip\": \"10.0.0.2\""));
        // only config.json remains (the temp file was renamed over it)
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn load_or_init_generates_and_persists_device_id() {
        let (_dir, path) = temp_config();
        let first = Config::load_or_init(&path);
        let id = first.device_id.clone().unwrap();
        assert!(is_valid_device_id(&id));
        assert_eq!(
            Config::load_from(&path).unwrap().device_id,
            Some(id.clone())
        );
        // stable across loads
        assert_eq!(Config::load_or_init(&path).device_id, Some(id));
    }

    #[test]
    fn load_or_init_replaces_an_invalid_device_id() {
        let (_dir, path) = temp_config();
        fs::write(&path, r#"{"device_id": "not-a-mac", "latency_ms": 500}"#).unwrap();
        let c = Config::load_or_init(&path);
        assert!(is_valid_device_id(c.device_id.as_deref().unwrap()));
        assert_eq!(c.latency_ms, 500);
        assert_eq!(Config::load_from(&path).unwrap(), c);
    }

    #[test]
    fn load_or_init_keeps_a_corrupt_file_aside() {
        let (dir, path) = temp_config();
        fs::write(&path, "garbage").unwrap();
        let c = Config::load_or_init(&path);
        assert_eq!(
            Config {
                device_id: None,
                ..c.clone()
            },
            Config::default()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("config.json.bad")).unwrap(),
            "garbage"
        );
        assert_eq!(Config::load_from(&path).unwrap(), c);
    }

    #[test]
    fn persist_identity_changes_only_identity() {
        let (_dir, path) = temp_config();
        fs::write(
            &path,
            r#"{"device_ip": "1.2.3.4", "latency_ms": "weird", "firewall_punch": true, "device_name": "old"}"#,
        )
        .unwrap();
        Config::persist_identity_at(&path, "02:11:22:33:44:55", "Kitchen").unwrap();
        let raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["device_ip"], "1.2.3.4");
        assert_eq!(raw["latency_ms"], "weird");
        assert_eq!(raw["firewall_punch"], true);
        assert_eq!(raw["device_id"], "02:11:22:33:44:55");
        assert_eq!(raw["device_name"], "Kitchen");
    }

    #[test]
    fn persist_identity_creates_a_missing_file_and_refuses_corrupt_ones() {
        let (_dir, path) = temp_config();
        Config::persist_identity_at(&path, "06:00:00:00:00:01", "HomePod").unwrap();
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.device_id.as_deref(), Some("06:00:00:00:00:01"));
        assert_eq!(c.device_name, "HomePod");
        assert_eq!(c.latency_ms, 100);

        fs::write(&path, "{broken").unwrap();
        assert!(Config::persist_identity_at(&path, "06:00:00:00:00:01", "x").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn device_ids_are_locally_administered_unicast() {
        for _ in 0..256 {
            let id = generate_device_id();
            assert!(is_valid_device_id(&id), "{id}");
            assert_eq!(id, id.to_uppercase());
            let first = u8::from_str_radix(&id[..2], 16).unwrap();
            assert_eq!(first & 0x02, 0x02, "locally administered bit: {id}");
            assert_eq!(first & 0x01, 0x00, "unicast bit: {id}");
        }
        assert_ne!(generate_device_id(), generate_device_id());
        assert!(is_valid_device_id("aa:bb:cc:dd:ee:ff"));
        assert!(!is_valid_device_id("AA:BB:CC:DD:EE"));
        assert!(!is_valid_device_id("AA:BB:CC:DD:EE:FG"));
        assert!(!is_valid_device_id("AABBCCDDEEFF"));
    }

    #[test]
    fn latency_is_clamped() {
        let with = |latency_ms| Config {
            latency_ms,
            ..Config::default()
        };
        assert_eq!(with(10).latency_clamped(), 75);
        assert_eq!(with(9000).latency_clamped(), 5000);
        assert_eq!(with(250).latency_clamped(), 250);
    }

    #[test]
    fn standard_path_api_uses_the_app_dir_override() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set(paths::APP_DIR_ENV, tmp.path());
        let loaded = Config::load();
        let id = loaded.device_id.clone().unwrap();
        let path = tmp.path().join("config.json");
        assert!(path.is_file());

        let mut changed = loaded.clone();
        changed.latency_ms = 250;
        changed.save().unwrap();
        Config::persist_identity(&id, "Bedroom").unwrap();
        let reloaded = Config::load();
        assert_eq!(reloaded.latency_ms, 250);
        assert_eq!(reloaded.device_name, "Bedroom");
        assert_eq!(reloaded.device_id, Some(id));
    }
}
