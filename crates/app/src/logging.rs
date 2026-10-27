//! Logging to `%APPDATA%\WinPods\winpods.log` (and optionally the console). CONTRACT.
//!
//! Rotate at start-up when the log exceeds 2 MB (keep `.1` and `.2`), then a
//! non-blocking tracing-appender writer. Level from config ("DEBUG"/"INFO"/
//! "WARNING"/"ERROR"); noisy dependencies (mdns_sd, winit, wgpu, femtovg) one level
//! quieter. The panic hook logs the panic with a backtrace and, for the GUI, shows a
//! message box.
//!
//! Details:
//! * Lines look like `2026-10-26T21:04:05.123456Z  INFO winpods_engine::supervisor:
//!   connected` (UTC timestamps; no ANSI colours in the file or on the console).
//! * The file writer is lossy: if the background thread falls behind, lines are
//!   dropped instead of blocking the caller. Real-time threads must not log anyway.
//! * `log`-crate records (mdns-sd, ureq, ...) are captured through tracing-log.

use std::backtrace::Backtrace;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::Level;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::{paths, win};

/// Rotate the log at start-up once it is larger than this (Python: `maxBytes`).
pub const MAX_LOG_BYTES: u64 = 2_000_000;

/// Log targets that are logged one level quieter than the configured level.
pub const NOISY_TARGETS: &[&str] = &[
    "mdns_sd",
    "winit",
    "wgpu",
    "wgpu_core",
    "wgpu_hal",
    "naga",
    "femtovg",
    "rustls",
    "ureq",
    "ureq_proto",
];

/// Keeps the background log writer alive; drop it at the very end of `main`.
pub struct LogGuard {
    _worker: Option<WorkerGuard>,
}

/// Rotate `winpods.log`, then route `tracing` (and `log`) output to it and, with
/// `console`, to stderr. Never fails: problems are reported on stderr / in the log.
/// Calling it again in the same process keeps the first subscriber.
pub fn init(level: &str, console: bool) -> LogGuard {
    init_at(&paths::log_path(), level, console)
}

/// [`init`] with an explicit log file.
pub(crate) fn init_at(path: &Path, level: &str, console: bool) -> LogGuard {
    let level = parse_level(level);
    let rotate_error = rotate(path, MAX_LOG_BYTES).err();

    let (file_writer, worker, open_error) =
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                let (writer, guard) = NonBlockingBuilder::default()
                    .lossy(true)
                    .thread_name("winpods-log")
                    .finish(file);
                (Some(writer), Some(guard), None)
            }
            Err(e) => (None, None, Some(e)),
        };

    let file_layer = file_writer.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_thread_names(true)
            .with_writer(writer)
    });
    let console_layer = console.then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(io::stderr)
    });

    let installed = tracing_subscriber::registry()
        .with(build_filter(level))
        .with(file_layer)
        .with(console_layer)
        .try_init()
        .is_ok();
    if !installed {
        // Another subscriber is already active (second call or tests). Keep it.
        tracing::debug!("logging was already initialised");
    }
    if let Some(e) = rotate_error {
        tracing::warn!("could not rotate {}: {e}", path.display());
    }
    if let Some(e) = open_error {
        let msg = format!("could not open log file {}: {e}", path.display());
        if !console {
            // Not `eprintln!`: it panics if stderr is unusable.
            let _ = writeln!(io::stderr(), "{msg}");
        }
        tracing::warn!("{msg}");
    }
    LogGuard { _worker: worker }
}

/// Map a config/CLI level name to a tracing level ("WARNING" is Python's name for
/// WARN). Case-insensitive; unknown names mean INFO.
pub fn parse_level(level: &str) -> Level {
    match level.trim().to_ascii_uppercase().as_str() {
        "TRACE" => Level::TRACE,
        "DEBUG" => Level::DEBUG,
        "WARNING" | "WARN" => Level::WARN,
        "ERROR" | "CRITICAL" => Level::ERROR,
        _ => Level::INFO,
    }
}

/// One level quieter than `level` (ERROR stays ERROR so dependency errors are kept).
fn quieter(level: Level) -> Level {
    match level {
        Level::TRACE => Level::DEBUG,
        Level::DEBUG => Level::INFO,
        Level::INFO => Level::WARN,
        _ => Level::ERROR,
    }
}

/// The filter: `level` everywhere, [`NOISY_TARGETS`] one level quieter. Ignores
/// `RUST_LOG` so the config setting is the single source of truth.
///
/// The global level is a bare directive in the string: `with_default_directive`
/// would be ignored as soon as the per-target directives are present.
fn build_filter(level: Level) -> EnvFilter {
    let quiet = quieter(level);
    let mut directives = vec![
        LevelFilter::from_level(level)
            .to_string()
            .to_ascii_lowercase(),
    ];
    directives.extend(
        NOISY_TARGETS
            .iter()
            .map(|target| format!("{target}={}", quiet.as_str().to_ascii_lowercase())),
    );
    EnvFilter::builder().parse_lossy(directives.join(","))
}

/// Start-up rotation: when `path` is larger than `max_bytes`, drop `.2`, move `.1`
/// to `.2` and the log to `.1`. Returns whether a rotation happened.
pub(crate) fn rotate(path: &Path, max_bytes: u64) -> io::Result<bool> {
    let len = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len <= max_bytes {
        return Ok(false);
    }
    let first = win::with_suffix(path, ".1");
    let second = win::with_suffix(path, ".2");
    match fs::remove_file(&second) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if first.exists() {
        fs::rename(&first, &second)?;
    }
    fs::rename(path, &first)?;
    Ok(true)
}

/// Install the panic hook; `show_dialog` = also show a MessageBox (GUI only).
pub fn install_panic_hook(show_dialog: bool) {
    // Resolve the log path now, not inside the hook.
    let log_file = show_dialog.then(paths::log_path);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info.payload());
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| String::from("<unknown location>"));
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let backtrace = Backtrace::force_capture();
        tracing::error!(
            target: "panic",
            "thread '{thread_name}' panicked at {location}: {message}\nbacktrace:\n{backtrace}"
        );
        previous(info);
        if let Some(log_file) = &log_file {
            show_panic_dialog(&message, &location, log_file);
        }
    }));
}

/// The panic payload as text (`panic!("...")` gives `&str` or `String`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        String::from("(non-string panic payload)")
    }
}

/// Text of the crash dialog.
fn dialog_text(message: &str, location: &str, log_file: &Path) -> String {
    format!(
        "WinPods ran into an unexpected error:\n\n{message}\n\nat {location}\n\n\
         Details were written to:\n{}",
        log_file.display()
    )
}

/// Show one crash dialog at a time (several threads may panic together).
fn show_panic_dialog(message: &str, location: &str, log_file: &Path) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    };
    use windows::core::PCWSTR;

    static SHOWING: AtomicBool = AtomicBool::new(false);
    if SHOWING.swap(true, Ordering::AcqRel) {
        return;
    }
    let text = win::wide(dialog_text(message, location, log_file).as_ref());
    let caption = win::wide("WinPods".as_ref());
    // SAFETY: both strings are NUL-terminated and live across the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
    SHOWING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_map_like_python() {
        assert_eq!(parse_level("DEBUG"), Level::DEBUG);
        assert_eq!(parse_level("info"), Level::INFO);
        assert_eq!(parse_level("WARNING"), Level::WARN);
        assert_eq!(parse_level("warn"), Level::WARN);
        assert_eq!(parse_level(" ERROR "), Level::ERROR);
        assert_eq!(parse_level("nonsense"), Level::INFO);
        assert_eq!(parse_level(""), Level::INFO);
    }

    #[test]
    fn noisy_targets_are_one_level_quieter() {
        assert_eq!(quieter(Level::DEBUG), Level::INFO);
        assert_eq!(quieter(Level::INFO), Level::WARN);
        assert_eq!(quieter(Level::WARN), Level::ERROR);
        assert_eq!(quieter(Level::ERROR), Level::ERROR);
        let filter = build_filter(Level::DEBUG);
        assert_eq!(filter.max_level_hint(), Some(LevelFilter::DEBUG));
        let text = filter.to_string();
        assert!(text.contains("mdns_sd=info"), "{text}");
        assert!(text.contains("femtovg=info"), "{text}");
        assert!(text.split(',').any(|d| d == "debug"), "{text}");
        let filter = build_filter(Level::WARN);
        assert_eq!(filter.max_level_hint(), Some(LevelFilter::WARN));
        assert!(filter.to_string().contains("winit=error"));
    }

    #[test]
    fn rotation_keeps_two_backups() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("winpods.log");
        // missing file: nothing to do
        assert!(!rotate(&log, 10).unwrap());
        fs::write(&log, b"small").unwrap();
        assert!(!rotate(&log, 10).unwrap());

        fs::write(&log, b"first generation!").unwrap();
        assert!(rotate(&log, 10).unwrap());
        assert!(!log.exists());
        fs::write(&log, b"second generation").unwrap();
        assert!(rotate(&log, 10).unwrap());
        fs::write(&log, b"third generation!").unwrap();
        assert!(rotate(&log, 10).unwrap());

        let read = |suffix: &str| fs::read_to_string(win::with_suffix(&log, suffix)).unwrap();
        assert_eq!(read(".1"), "third generation!");
        assert_eq!(read(".2"), "second generation");
        assert!(!win::with_suffix(&log, ".3").exists());
    }

    #[test]
    fn panic_payloads_and_dialog_text() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new("static str");
        assert_eq!(panic_message(boxed.as_ref()), "static str");
        let boxed: Box<dyn std::any::Any + Send> = Box::new(String::from("owned"));
        assert_eq!(panic_message(boxed.as_ref()), "owned");
        let boxed: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(panic_message(boxed.as_ref()), "(non-string panic payload)");
        let text = dialog_text("boom", "src/x.rs:1:2", Path::new(r"C:\logs\winpods.log"));
        assert!(text.contains("boom") && text.contains("src/x.rs:1:2"));
        assert!(text.contains(r"C:\logs\winpods.log"));
    }

    /// The only test that installs the global subscriber and the panic hook (both
    /// are process-wide); it writes to a temp file, never to %APPDATA%.
    #[test]
    fn init_writes_to_the_file_and_logs_panics() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("winpods.log");
        fs::write(&log, vec![b'x'; 64]).unwrap();
        let guard = init_at(&log, "INFO", false);
        // a second init keeps the first subscriber and does not panic
        drop(init_at(&dir.path().join("other.log"), "DEBUG", false));

        tracing::info!(target: "winpods_app::logging_test", "hello from the test");
        tracing::debug!(target: "winpods_app::logging_test", "debug is filtered at INFO");

        install_panic_hook(false);
        let caught = std::panic::catch_unwind(|| panic!("deliberate test panic"));
        assert!(caught.is_err());
        let _ = std::panic::take_hook(); // back to the default hook

        drop(guard); // flushes the background writer
        let text = fs::read_to_string(&log).unwrap();
        assert!(
            text.starts_with(&"x".repeat(64)),
            "appends to the existing log"
        );
        assert!(text.contains("hello from the test"), "{text}");
        assert!(!text.contains("debug is filtered"), "{text}");
        assert!(text.contains("deliberate test panic"), "{text}");
        assert!(text.contains("backtrace:"), "{text}");
    }
}
