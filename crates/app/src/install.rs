//! The installer's registration in Settings > Apps (workstream: app).
//!
//! `installer/WinPods.iss` registers a per-user install under
//! `HKCU\…\Uninstall\{AppId}_is1` with the version it installed. WinPods then updates
//! itself in place, so after a self-update [`refresh_registration`] writes the new
//! version there, as long as that registration is for the folder this copy runs from.
//! Copies not installed by the installer (a zip, a build folder) have no such key and
//! are left alone.

use std::io;
use std::path::Path;

use windows_registry::CURRENT_USER;

use crate::win;

/// The installer's uninstall key (Inno Setup appends `_is1` to the AppId). Keep the
/// AppId in sync with `installer/WinPods.iss`.
pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{5C2004B3-E8ED-45AF-89C6-CE42A6C1310D}_is1";

/// After a self-update of the copy in `exe_dir`: show `version` in Settings > Apps if
/// the installer registered that folder. Failures are logged and otherwise ignored.
pub fn refresh_registration(exe_dir: &Path, version: &str) {
    match refresh_with(UNINSTALL_KEY, exe_dir, version) {
        Ok(true) => tracing::info!("installed version shown in Settings > Apps is now {version}"),
        Ok(false) => {}
        Err(e) => tracing::warn!("could not update the installer's version entry: {e}"),
    }
}

/// [`refresh_registration`] for the key at `key_path` (under `HKCU`). Returns whether
/// it wrote the version.
pub(crate) fn refresh_with(key_path: &str, exe_dir: &Path, version: &str) -> io::Result<bool> {
    let mut options = CURRENT_USER.options();
    options.read().write();
    let key = match options.open(key_path).map_err(win::to_io) {
        Ok(key) => key,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let Ok(location) = key.get_string("InstallLocation") else {
        return Ok(false);
    };
    let location = location.trim_end_matches(['\\', '/']);
    if location.is_empty() || !win::same_path(Path::new(location), exe_dir) {
        return Ok(false);
    }
    if key.get_string("DisplayVersion").ok().as_deref() == Some(version) {
        return Ok(false);
    }
    key.set_string("DisplayVersion", version)
        .map_err(win::to_io)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_suffix;

    /// A throw-away `HKCU\Software\WinPods-Test-<random>` key, deleted on drop.
    struct TestKey(String);

    impl TestKey {
        fn new(location: Option<&Path>) -> Self {
            let path = format!(r"Software\WinPods-Test-{}", unique_suffix());
            let key = CURRENT_USER.create(&path).unwrap();
            key.set_string("DisplayVersion", "0.2.1").unwrap();
            if let Some(location) = location {
                // Inno Setup writes the folder with a trailing backslash.
                let value = format!(r"{}\", location.display());
                key.set_string("InstallLocation", &value).unwrap();
            }
            Self(path)
        }

        fn version(&self) -> String {
            CURRENT_USER
                .open(&self.0)
                .unwrap()
                .get_string("DisplayVersion")
                .unwrap()
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.0);
        }
    }

    #[test]
    fn updates_the_version_of_the_installed_folder_only() {
        let installed = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let key = TestKey::new(Some(installed.path()));

        // Another copy (a build folder, a zip) leaves the installer's entry alone.
        assert!(!refresh_with(&key.0, elsewhere.path(), "0.2.2").unwrap());
        assert_eq!(key.version(), "0.2.1");

        assert!(refresh_with(&key.0, installed.path(), "0.2.2").unwrap());
        assert_eq!(key.version(), "0.2.2");
        // Already current: nothing to write.
        assert!(!refresh_with(&key.0, installed.path(), "0.2.2").unwrap());
    }

    #[test]
    fn no_registration_means_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let missing = format!(r"Software\WinPods-Test-{}", unique_suffix());
        assert!(!refresh_with(&missing, dir.path(), "0.2.2").unwrap());
        let key = TestKey::new(None);
        assert!(!refresh_with(&key.0, dir.path(), "0.2.2").unwrap());
        assert_eq!(key.version(), "0.2.1");
    }
}
