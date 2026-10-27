//! WinPods.exe entry point: wires logging, the single-instance guard, config,
//! start-with-Windows reconciliation, the engine and the updater into the UI.
//!
//! Arguments:
//! * `--minimized` — start hidden in the tray (used by the Run value);
//! * `--post-update <pid> --updated-from <version> [--resume]` — hand-off after a
//!   self-update: wait for the previous process `<pid>` to exit (its engine must be
//!   stopped before ours starts), retry the single-instance mutex, clean up the
//!   leftover `.old`/`.new` files, and with `--resume` connect at start (the user was
//!   connected before the update) whatever "Connect automatically" says;
//! * `--demo [--theme light|dark] [--page settings] [--update]` — mock engine, no I/O.
//!
//! Unknown arguments are logged and ignored.
//!
//! The single-instance decision comes first, before config or logging: a second
//! launch only signals the running instance and exits, without rotating or writing
//! its log or touching config. A running instance that is quitting does not answer
//! and keeps the mutex until its process has exited; the second launch then waits for
//! it and starts normally ([`second_launch`]), so two engines never overlap.

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
use winpods_engine::{EngineApi, EngineHandle, EngineSettings};

/// How long an updated process waits for the previous one to exit (it stops its
/// engine first, which takes at most ~8 s).
const PREVIOUS_EXIT_WAIT: Duration = Duration::from_secs(30);

/// How long a second launch waits for the running instance to answer the activation.
const ACTIVATION_ACK_WAIT: Duration = Duration::from_millis(1500);

/// How long a second launch waits for a running WinPods 0.2 that did not answer: it
/// is quitting (it stops its engine in at most ~8 s, then exits) or hung.
const UNANSWERED_INSTANCE_WAIT: Duration = Duration::from_secs(12);

#[derive(Debug, Default)]
struct Args {
    minimized: bool,
    post_update: Option<u32>,
    updated_from: Option<String>,
    /// Connect at start (`--resume`, only honoured with `--post-update`).
    resume: bool,
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
            ui::RESUME_ARG => args.resume = true,
            other => args.unknown.push(other.to_string()),
        }
    }
    args
}

/// What a launch that found another instance does.
#[derive(Debug, PartialEq, Eq)]
enum SecondLaunch<G> {
    /// The other instance exited meanwhile: start normally with this guard.
    Primary(G),
    /// Nothing more to do (the running instance showed its window, or a tray start).
    Exit,
    /// Say that WinPods is already running (an older, elevated or hung instance).
    AlreadyRunning,
}

/// Decide what a launch that is not the primary instance does. `signal` asks the
/// running instance to show its window (true = acknowledged); `answers_activations`
/// tells whether a WinPods 0.2 holds the activation event (v0.1 has none, and an
/// elevated instance's is not ours to open); `acquire` retries the single-instance
/// mutex for the given time. A 0.2 instance that is quitting does not acknowledge and
/// keeps the mutex until its process has exited, so waiting for the mutex starts this
/// launch as soon as it is gone, without two engines ever running at once.
fn second_launch<G>(
    minimized: bool,
    signal: impl FnOnce() -> bool,
    answers_activations: impl FnOnce() -> bool,
    acquire: impl FnOnce(Duration) -> Option<G>,
) -> SecondLaunch<G> {
    if minimized {
        // Start with Windows: never show anything.
        return SecondLaunch::Exit;
    }
    if signal() {
        return SecondLaunch::Exit;
    }
    // Without the event, one more immediate try covers an instance that exited just now.
    let wait = if answers_activations() {
        UNANSWERED_INSTANCE_WAIT
    } else {
        Duration::ZERO
    };
    match acquire(wait) {
        Some(guard) => SecondLaunch::Primary(guard),
        None => SecondLaunch::AlreadyRunning,
    }
}

/// True when a WinPods 0.2 instance's activation event exists and we may set it (the
/// access `single_instance::signal_existing` needs).
fn activation_event_exists() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW};
    use windows::core::HSTRING;

    let name = HSTRING::from(single_instance::ACTIVATE_EVENT_NAME);
    // SAFETY: plain Win32 call; the returned handle is closed below.
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name) } {
        Ok(handle) => {
            // SAFETY: closing the handle opened above exactly once.
            unsafe {
                let _ = CloseHandle(handle);
            }
            true
        }
        Err(_) => false,
    }
}

/// Wait up to `timeout` for process `pid` to exit. True when it has exited (or no
/// longer exists); false on timeout. Never waits for this process itself.
fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    if pid == 0 || pid == std::process::id() {
        return true;
    }
    // SAFETY: plain Win32 call; the returned handle is closed below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return true; // already gone
    };
    let millis = u32::try_from(timeout.as_millis())
        .unwrap_or(u32::MAX - 1)
        .min(u32::MAX - 1);
    // SAFETY: `process` is a valid handle owned here.
    let exited = unsafe { WaitForSingleObject(process, millis) } == WAIT_OBJECT_0;
    // SAFETY: closing the handle opened above exactly once.
    unsafe {
        let _ = CloseHandle(process);
    }
    exited
}

fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|arg| arg == "--demo") {
        return demo::run_demo(&raw);
    }
    winpods_rt::clock::init();
    let args = parse_args(&raw);

    // After a self-update: the previous process starts us before it stops its engine
    // and releases the exe image, so let it finish first.
    let previous_exited = args
        .post_update
        .map(|pid| wait_for_exit(pid, PREVIOUS_EXIT_WAIT));
    let retry = if args.post_update.is_some() {
        Duration::from_secs(10)
    } else {
        Duration::ZERO
    };
    let instance = match single_instance::acquire(retry) {
        Ok(Instance::Primary(guard)) => guard,
        Ok(Instance::Secondary) => match second_launch(
            args.minimized,
            || single_instance::signal_existing(ACTIVATION_ACK_WAIT),
            activation_event_exists,
            |wait| match single_instance::acquire(wait) {
                Ok(Instance::Primary(guard)) => Some(guard),
                Ok(Instance::Secondary) | Err(_) => None,
            },
        ) {
            SecondLaunch::Primary(guard) => guard,
            SecondLaunch::Exit => return Ok(()),
            SecondLaunch::AlreadyRunning => {
                message_box(
                    "WinPods is already running (possibly an older version, or one started as \
                     administrator). Quit it from its tray icon first.",
                );
                return Ok(());
            }
        },
        Err(err) => {
            let _log = logging::init(&Config::load().log_level, false);
            tracing::error!("single-instance check failed: {err}");
            return Err(err.into());
        }
    };

    let config = Config::load();
    let _log = logging::init(&config.log_level, false);
    logging::install_panic_hook(true);
    info!(version = env!("CARGO_PKG_VERSION"), "WinPods starting");
    if !args.unknown.is_empty() {
        warn!(arguments = ?args.unknown, "ignoring unknown arguments");
    }
    if let Some(pid) = args.post_update {
        if previous_exited == Some(false) {
            warn!(pid, "the previous WinPods did not exit in time");
        }
        Updater::finish_post_update();
        info!(from = ?args.updated_from, resume = args.resume, "update completed");
    }

    let temp_copy = paths::is_temp_copy();
    if temp_copy {
        warn!(
            "running from a temporary folder (inside a zip?): start with Windows is off and \
             updates are offered as the release page only"
        );
    } else {
        let status = autostart::reconcile(config.start_with_windows);
        info!(?status, "start with Windows");
    }

    // The UI sends Connect (auto-connect or --resume), so the switch and tray agree.
    let engine = Arc::new(EngineHandle::start(engine_settings(&config))?);

    // Also for a temp copy: it is told about updates, but the UI only offers the
    // release page there (Updater::can_install_in_place is false).
    let updater = Some(Arc::new(Updater::new(
        &Updater::repo_from_env(),
        updater::current_version(),
    )));

    let shutdown_engine = engine.clone();
    let context = ui::UiContext {
        engine: engine.clone() as Arc<dyn EngineApi>,
        start_hidden: args.minimized || config.start_minimized,
        resume_connection: args.resume && args.post_update.is_some(),
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
        // HomePods reject SETUP (400) when we cannot listen on UDP 319/320, so report
        // the busy ports clearly instead of trying the dynamic-port fallback.
        require_standard_ptp_ports: true,
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
        assert!(!a.resume);
        assert_eq!(a.unknown, vec!["-x".to_string()]);
        let a = parse_args(&strings(&["--post-update", "7", "--resume"]));
        assert!(a.resume);
        assert_eq!(a.post_update, Some(7));
    }

    #[test]
    fn handoff_arguments_round_trip() {
        // What the old process passes is what the new one understands.
        for resume in [false, true] {
            let a = parse_args(&ui::handoff_args(4321, "0.2.0", resume));
            assert_eq!(a.post_update, Some(4321));
            assert_eq!(a.updated_from.as_deref(), Some("0.2.0"));
            assert_eq!(a.resume, resume);
            assert!(a.unknown.is_empty(), "{:?}", a.unknown);
        }
    }

    #[test]
    fn waits_for_the_previous_process_to_exit() {
        // Never waits for itself, nor for a process that does not exist.
        assert!(wait_for_exit(std::process::id(), Duration::from_secs(5)));
        assert!(wait_for_exit(0, Duration::from_secs(5)));
        let start = std::time::Instant::now();
        assert!(wait_for_exit(u32::MAX - 3, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(2));

        // A child of this test that lives for about a second.
        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Milliseconds 1000",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start a short-lived child");
        assert!(!wait_for_exit(child.id(), Duration::from_millis(50)));
        assert!(wait_for_exit(child.id(), Duration::from_secs(20)));
        child.wait().unwrap();
    }

    #[test]
    fn a_second_launch_activates_the_running_instance() {
        let mut waited = None;
        let outcome = second_launch(
            false,
            || true,
            || panic!("not needed once acknowledged"),
            |wait| -> Option<()> {
                waited = Some(wait);
                None
            },
        );
        assert_eq!(outcome, SecondLaunch::Exit);
        assert_eq!(waited, None);

        // A tray start (--minimized) never signals nor waits.
        let outcome = second_launch(
            true,
            || panic!("a tray start must not show the window"),
            || panic!("no wait"),
            |_| -> Option<()> { panic!("no wait") },
        );
        assert_eq!(outcome, SecondLaunch::Exit);
    }

    #[test]
    fn a_second_launch_waits_for_an_instance_that_is_quitting() {
        // No ack from a 0.2 instance: it is quitting; start once its mutex is gone.
        let mut waited = None;
        let outcome = second_launch(
            false,
            || false,
            || true,
            |wait| {
                waited = Some(wait);
                Some("guard")
            },
        );
        assert_eq!(outcome, SecondLaunch::Primary("guard"));
        assert_eq!(waited, Some(UNANSWERED_INSTANCE_WAIT));
        // Long enough for the engine to stop (~8 s at most) and the process to exit.
        assert!(UNANSWERED_INSTANCE_WAIT >= Duration::from_secs(10));

        // Still there after the wait (hung): say so.
        let outcome = second_launch(false, || false, || true, |_| -> Option<()> { None });
        assert_eq!(outcome, SecondLaunch::AlreadyRunning);
    }

    #[test]
    fn a_second_launch_reports_an_older_or_elevated_instance_at_once() {
        // No activation event (v0.1, or an elevated instance): no long wait.
        let mut waited = None;
        let outcome = second_launch(
            false,
            || false,
            || false,
            |wait| -> Option<()> {
                waited = Some(wait);
                None
            },
        );
        assert_eq!(outcome, SecondLaunch::AlreadyRunning);
        assert_eq!(waited, Some(Duration::ZERO));
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
