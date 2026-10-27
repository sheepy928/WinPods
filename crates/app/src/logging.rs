//! Logging to `%APPDATA%\WinPods\winpods.log` (and optionally the console). CONTRACT.
//!
//! Rotate at start-up when the log exceeds 2 MB (keep `.1` and `.2`), then a
//! non-blocking tracing-appender writer. Level from config ("DEBUG"/"INFO"/
//! "WARNING"/"ERROR"); noisy dependencies (mdns_sd, winit, wgpu, femtovg) one level
//! quieter. The panic hook logs the panic with a backtrace and, for the GUI, shows a
//! message box.

/// Keeps the background log writer alive; drop it at the very end of `main`.
pub struct LogGuard {
    _private: (),
}

pub fn init(level: &str, console: bool) -> LogGuard {
    let _ = (level, console);
    todo!("logging: init")
}

/// Install the panic hook; `show_dialog` = also show a MessageBox (GUI only).
pub fn install_panic_hook(show_dialog: bool) {
    let _ = show_dialog;
    todo!("logging: install_panic_hook")
}
