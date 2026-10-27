//! WinPods.exe entry point: wires logging, the single-instance guard, config,
//! start-with-Windows reconciliation, the engine and the updater into the UI.
//!
//! Arguments:
//! * `--minimized` — start hidden in the tray (used by the Run value);
//! * `--post-update <pid> --updated-from <version>` — hand-off after a self-update:
//!   keep retrying the single-instance mutex while the old process exits, then clean
//!   up the leftover `.old`/`.new` files;
//! * `--demo [--theme light|dark] [--page settings] [--update]` — mock engine, no I/O.
//!
//! Unknown arguments are logged and ignored.

#![windows_subsystem = "windows"]

mod demo;
mod icons;
mod tray;
mod ui;
mod view;

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};
use winpods_airplay::{Codec, MAX_LATENCY_MS, MIN_LATENCY_MS, TimingProtocol};
use winpods_app::single_instance::{self, Instance};
use winpods_app::updater::{self, Updater};
use winpods_app::{Config, autostart, logging, paths};
use winpods_engine::supervisor::capture_selection;
use winpods_engine::{Command, EngineApi, EngineHandle, EngineSettings};

#[derive(Debug, Default)]
struct Args {
    minimized: bool,
    post_update: Option<u32>,
    updated_from: Option<String>,
    unknown: Vec<String>,
}

fn parse_args(raw: &[String]) -> Args {
    let mut args = Args::default();
    let mut iter = raw.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--minimized" => args.minimized = true,
            "--post-update" => {
                args.post_update = iter.next().and_then(|v| v.parse().ok()).or(Some(0));
            }
            "--updated-from" => args.updated_from = iter.next().cloned(),
            other => args.unknown.push(other.to_string()),
        }
    }
    args
}

fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|arg| arg == "--demo") {
        return demo::run_demo(&raw);
    }
    winpods_rt::clock::init();
    let args = parse_args(&raw);

    let config = Config::load();
    let _log = logging::init(&config.log_level, false);
    logging::install_panic_hook(true);
    info!(version = env!("CARGO_PKG_VERSION"), "WinPods starting");
    if !args.unknown.is_empty() {
        warn!(arguments = ?args.unknown, "ignoring unknown arguments");
    }

    // After a self-update the old process may still be shutting down.
    let retry = if args.post_update.is_some() {
        Duration::from_secs(10)
    } else {
        Duration::ZERO
    };
    let instance = match single_instance::acquire(retry)? {
        Instance::Primary(guard) => guard,
        Instance::Secondary => {
            if !args.minimized && !single_instance::signal_existing(Duration::from_millis(1500)) {
                message_box(
                    "WinPods is already running (possibly an older version, or one started as \
                     administrator). Quit it from its tray icon first.",
                );
            }
            return Ok(());
        }
    };
    if args.post_update.is_some() {
        Updater::finish_post_update();
        info!(from = ?args.updated_from, "update completed");
    }

    let temp_copy = paths::is_temp_copy();
    if temp_copy {
        warn!("running from a temporary folder (inside a zip?); autostart and updates are off");
    } else {
        let status = autostart::reconcile(config.start_with_windows);
        info!(?status, "start with Windows");
    }

    let engine = Arc::new(EngineHandle::start(engine_settings(&config))?);
    if config.auto_connect && !config.device_ip.trim().is_empty() {
        engine.send(Command::Connect);
    }

    let updater = (!temp_copy).then(|| {
        Arc::new(Updater::new(
            &Updater::repo_from_env(),
            updater::current_version(),
        ))
    });

    let shutdown_engine = engine.clone();
    let context = ui::UiContext {
        engine: engine.clone() as Arc<dyn EngineApi>,
        start_hidden: args.minimized || config.start_minimized,
        config,
        instance: Some(instance),
        updater,
        version: env!("CARGO_PKG_VERSION").to_string(),
        updated_from: args.updated_from.filter(|_| args.post_update.is_some()),
        shutdown: Box::new(move || shutdown_engine.shutdown()),
        demo: false,
    };
    let result = ui::run(context);
    engine.shutdown(); // no-op if the UI already shut it down
    info!("WinPods exiting");
    result
}

/// Engine settings from the saved config (the UI owns config from here on).
fn engine_settings(config: &Config) -> EngineSettings {
    EngineSettings {
        device_ip: config.device_ip.clone(),
        latency_ms: config.latency_ms.clamp(MIN_LATENCY_MS, MAX_LATENCY_MS),
        capture: capture_selection(&config.capture_device, config.capture_device_id.clone()),
        volume: config.volume,
        timing: TimingProtocol::parse(&config.timing_protocol).unwrap_or_default(),
        codec: Codec::parse(&config.codec).unwrap_or_default(),
        sender_name: config.sender_name.clone(),
        device_id: config
            .device_id
            .clone()
            .unwrap_or_else(|| "02:00:00:00:00:01".to_string()),
        test_tone: false,
        drop_every: None,
        // The GUI falls back to a dynamic PTP port if 319/320 are taken.
        require_standard_ptp_ports: false,
    }
}

fn message_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from("WinPods"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_known_arguments() {
        let a = parse_args(&strings(&["--minimized"]));
        assert!(a.minimized && a.post_update.is_none());
        let a = parse_args(&strings(&[
            "--post-update",
            "1234",
            "--updated-from",
            "0.2.0",
            "-x",
        ]));
        assert_eq!(a.post_update, Some(1234));
        assert_eq!(a.updated_from.as_deref(), Some("0.2.0"));
        assert_eq!(a.unknown, vec!["-x".to_string()]);
    }

    #[test]
    fn engine_settings_clamp_and_parse() {
        let config = Config {
            latency_ms: 10,
            timing_protocol: "ntp".into(),
            codec: "PCM".into(),
            ..Config::default()
        };
        let s = engine_settings(&config);
        assert_eq!(s.latency_ms, MIN_LATENCY_MS);
        assert_eq!(s.timing, TimingProtocol::Ntp);
        assert_eq!(s.codec, Codec::Pcm);
        assert!(s.capture.is_default());
    }
}
