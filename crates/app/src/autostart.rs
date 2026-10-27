//! Start with Windows via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//! CONTRACT.
//!
//! Value name `WinPods`, data `"<exe>" --minimized`. `StartupApproved\Run\WinPods`
//! (first byte 0x03 = disabled by the user in Task Manager) is read for status only.
//! Reconcile only after taking the single-instance mutex; never write a Run value for
//! a temp copy. Only rewrite silently when the old target no longer exists.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartStatus {
    Disabled,
    Enabled {
        /// Executable the Run value points to.
        path: PathBuf,
        /// False when disabled in Task Manager (StartupApproved).
        approved: bool,
        /// True when `path` is this executable.
        is_current_exe: bool,
    },
}

pub fn status() -> AutostartStatus {
    todo!("autostart: status")
}

/// Write the Run value for `exe` (`"<exe>" --minimized`).
pub fn enable(exe: &Path) -> std::io::Result<()> {
    let _ = exe;
    todo!("autostart: enable")
}

/// Remove the Run value (no error if absent).
pub fn disable() -> std::io::Result<()> {
    todo!("autostart: disable")
}

/// Make the registry agree with the config flag where safe (see module docs) and
/// return the resulting status.
pub fn reconcile(config_enabled: bool) -> AutostartStatus {
    let _ = config_enabled;
    todo!("autostart: reconcile")
}
