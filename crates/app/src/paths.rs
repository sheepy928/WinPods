//! Well-known paths. CONTRACT.

use std::path::PathBuf;

/// `%APPDATA%\WinPods` (created if missing).
pub fn app_dir() -> PathBuf {
    todo!("paths: app_dir")
}

/// `%APPDATA%\WinPods\config.json`.
pub fn config_path() -> PathBuf {
    todo!("paths: config_path")
}

/// `%APPDATA%\WinPods\winpods.log`.
pub fn log_path() -> PathBuf {
    todo!("paths: log_path")
}

/// Canonical path of the running executable.
pub fn exe_path() -> std::io::Result<PathBuf> {
    todo!("paths: exe_path")
}

/// True when the exe runs from inside the temp directory (e.g. opened straight from
/// a zip by Explorer `Temp1_*.zip`, 7-Zip `7zO*`, WinRAR `Rar$EX*`). Compares
/// canonicalised long paths case-insensitively.
pub fn is_temp_copy() -> bool {
    todo!("paths: is_temp_copy")
}
