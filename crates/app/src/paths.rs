//! Well-known paths. CONTRACT.
//!
//! The application folder is `%APPDATA%\WinPods`. Set the environment variable
//! [`APP_DIR_ENV`] (`WINPODS_APP_DIR`) to a non-empty path to use that folder instead;
//! tests (and portable/diagnostic runs) use it so they never touch the real
//! `%APPDATA%\WinPods`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::win;

/// Folder name under `%APPDATA%`.
pub const APP_NAME: &str = "WinPods";

/// Environment variable that overrides [`app_dir`] (used by tests and diagnostics).
pub const APP_DIR_ENV: &str = "WINPODS_APP_DIR";

/// `%APPDATA%\WinPods` (created if missing), or `%WINPODS_APP_DIR%` when set.
pub fn app_dir() -> PathBuf {
    let dir = resolve_app_dir(
        std::env::var_os(APP_DIR_ENV),
        std::env::var_os("APPDATA"),
        std::env::var_os("USERPROFILE"),
    );
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not create {}: {e}", dir.display());
    }
    dir
}

/// Pure resolution of the application folder: the override wins, then `%APPDATA%`,
/// then the user profile (the Python v0.1 fallback), then the current directory.
fn resolve_app_dir(
    override_dir: Option<OsString>,
    appdata: Option<OsString>,
    profile: Option<OsString>,
) -> PathBuf {
    let non_empty = |v: Option<OsString>| v.filter(|s| !s.is_empty());
    if let Some(dir) = non_empty(override_dir) {
        return PathBuf::from(dir);
    }
    match non_empty(appdata).or_else(|| non_empty(profile)) {
        Some(base) => PathBuf::from(base).join(APP_NAME),
        None => PathBuf::from(APP_NAME),
    }
}

/// `%APPDATA%\WinPods\config.json`.
pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

/// `%APPDATA%\WinPods\winpods.log`.
pub fn log_path() -> PathBuf {
    app_dir().join("winpods.log")
}

/// Canonical path of the running executable: long file names (no 8.3 short names)
/// and no `\\?\` prefix, so it can be written to the registry and compared.
pub fn exe_path() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(win::long_path(&exe))
}

/// True when the exe runs from inside the temp directory (e.g. opened straight from
/// a zip by Explorer `Temp1_*.zip`, 7-Zip `7zO*`, WinRAR `Rar$EX*`). Compares
/// canonicalised long paths case-insensitively.
pub fn is_temp_copy() -> bool {
    match exe_path() {
        Ok(exe) => is_in_temp_dir(&exe),
        Err(e) => {
            tracing::warn!("could not determine the executable path: {e}");
            false
        }
    }
}

/// True when `path` lies inside [`std::env::temp_dir`] (both sides long-path
/// normalised, compared case-insensitively per component).
pub(crate) fn is_in_temp_dir(path: &Path) -> bool {
    is_inside(path, &std::env::temp_dir())
}

/// True when `path` is `dir` or lies below it (long paths, case-insensitive).
pub(crate) fn is_inside(path: &Path, dir: &Path) -> bool {
    win::starts_with_ci(&win::long_path(path), &win::long_path(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvVarGuard;

    #[test]
    fn override_wins_then_appdata_then_profile() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            resolve_app_dir(some(r"D:\portable"), some(r"C:\AD"), some(r"C:\U")),
            PathBuf::from(r"D:\portable")
        );
        assert_eq!(
            resolve_app_dir(some(""), some(r"C:\AD"), some(r"C:\U")),
            PathBuf::from(r"C:\AD\WinPods")
        );
        assert_eq!(
            resolve_app_dir(None, None, some(r"C:\U")),
            PathBuf::from(r"C:\U\WinPods")
        );
        assert_eq!(resolve_app_dir(None, None, None), PathBuf::from("WinPods"));
    }

    #[test]
    fn env_override_is_used_and_created() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("app");
        let _env = EnvVarGuard::set(APP_DIR_ENV, &dir);
        assert_eq!(app_dir(), dir);
        assert!(dir.is_dir());
        assert_eq!(config_path(), dir.join("config.json"));
        assert_eq!(log_path(), dir.join("winpods.log"));
    }

    #[test]
    fn temp_detection() {
        let temp = win::long_path(&std::env::temp_dir());
        assert!(is_in_temp_dir(
            &temp.join("Temp1_WinPods.zip").join("WinPods.exe")
        ));
        let upper = PathBuf::from(temp.to_string_lossy().to_uppercase()).join("7zO1234");
        assert!(is_in_temp_dir(&upper.join("WinPods.exe")));
        assert!(!is_inside(
            Path::new(r"C:\Program Files\WinPods\WinPods.exe"),
            &temp
        ));
        // The test binary lives in target\, not in %TEMP%.
        assert!(!is_temp_copy());
    }

    #[test]
    fn exe_path_is_long_and_exists() {
        let exe = exe_path().unwrap();
        assert!(exe.is_file());
        assert!(!exe.to_string_lossy().starts_with(r"\\?\"));
        assert!(!exe.to_string_lossy().contains('~'));
    }
}
