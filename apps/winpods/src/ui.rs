//! The Slint window and the tray (workstream: UI). CONTRACT: `UiContext` and `run`
//! are called from `main.rs` (owned by the lead).
//!
//! Responsibilities (see plan section 4): main page and settings page, tray icon via
//! `tray.rs` (tray-icon + muda on this thread), config ownership and atomic saves,
//! start-with-Windows toggle (`winpods_app::autostart`), update banner + install flow
//! (`winpods_app::updater`, work on background threads, results via
//! `slint::invoke_from_event_loop`), single-instance activation (show + focus the
//! window), close → hide to tray with a one-time hint, quit → engine shutdown on a
//! helper thread then `slint::quit_event_loop()`.
//!
//! Update hand-off: after a successful swap the new exe is started first (with
//! `--post-update <pid>`, and `--resume` when the user was connected); it waits for
//! this process to exit before it starts its engine. Only then does this process shut
//! down, so a failed start leaves it running and shows the error. Quit never
//! interrupts an exe swap: it waits for one in progress and keeps a new one from
//! starting ([`SwapGate`]); a download in progress is simply abandoned.
//!
//! Structure: one [`Controller`] per process lives in a thread-local on the UI thread.
//! Slint callbacks, the UI timer and events posted from other threads ([`post`]) all
//! reach it through [`with_controller`], so no callback holds a strong reference and
//! no `RefCell` borrow is held across calls into Slint.
//!
//! The generated Slint types (`MainWindow`, the `App` global, ...) are re-exported from
//! here; `main.rs` must not call `slint::include_modules!()` itself.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slint::{
    CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use winpods_app::Config;
use winpods_app::single_instance::InstanceGuard;
use winpods_app::updater::{Release, UpdateError, Updater};
use winpods_engine::{
    CaptureSelection, Command, DeviceInfo, EngineApi, ScanResult, ScanState, Snapshot,
};

use crate::tray::{SystemTray, Tray, TrayEvent, TrayModel};
use crate::view::{self, LATENCY_PRESETS, LOG_LEVELS, RENDERERS, VolumeThrottle};

#[allow(clippy::all, clippy::pedantic, missing_docs, unused_qualifications)]
mod generated {
    slint::include_modules!();
}

pub use generated::{App, MainWindow, ScanRow, StatusKind};

/// Everything the UI needs from `main`.
pub struct UiContext {
    pub engine: Arc<dyn EngineApi>,
    /// The UI thread owns config from here on (it saves on every change).
    pub config: Config,
    /// Start hidden in the tray (`--minimized` or config `start_minimized`).
    pub start_hidden: bool,
    /// Connect at start even when "Connect automatically" is off: this process was
    /// started by a self-update while the user was connected (`--resume`).
    pub resume_connection: bool,
    /// Primary-instance guard: the UI registers the activation handler. Once Quit (or
    /// the update hand-off) begins, activations are no longer acknowledged and the
    /// guard is kept until the process exits (see [`activation_handler`]).
    pub instance: Option<InstanceGuard>,
    /// `None` disables update checks (demo mode). A temp copy still checks; it is only
    /// offered the release page, never an in-place install.
    pub updater: Option<Arc<Updater>>,
    /// Version string shown in About ("0.2.0").
    pub version: String,
    /// Set after a successful self-update hand-off (old version string).
    pub updated_from: Option<String>,
    /// Engine shutdown hook (blocking, ~8 s max); called once on Quit.
    pub shutdown: Box<dyn FnOnce() + Send>,
    /// Demo mode (`--demo`): mock engine; never save config, touch the Run key or
    /// check for updates.
    pub demo: bool,
}

/// Colour scheme of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSchemeOverride {
    /// Follow the Windows app theme (light/dark), live.
    #[default]
    System,
    /// Force light (demo screenshots). Does not change any Windows setting.
    Light,
    /// Force dark (demo screenshots). Does not change any Windows setting.
    Dark,
}

/// Page shown first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Main,
    Settings,
}

/// Optional presentation settings for [`run_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunOptions {
    pub color_scheme: ColorSchemeOverride,
    pub start_page: Page,
    /// Demo mode only: pretend an update is available from the start.
    pub demo_update_available: bool,
}

/// Run the UI until the user quits. Blocks the calling (main) thread.
#[allow(dead_code)] // CONTRACT: called from main.rs once the integration step lands.
pub fn run(ctx: UiContext) -> anyhow::Result<()> {
    run_with(ctx, RunOptions::default())
}

/// [`run`] with presentation options (theme override for screenshots, start page).
pub fn run_with(ctx: UiContext, options: RunOptions) -> anyhow::Result<()> {
    if ctx.config.renderer.eq_ignore_ascii_case("software")
        && let Err(err) = slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("software".into())
            .select()
    {
        tracing::warn!("software renderer unavailable, using the default: {err}");
    }

    let window = MainWindow::new()?;
    match options.color_scheme {
        ColorSchemeOverride::System => {}
        ColorSchemeOverride::Light => window.invoke_force_color_scheme(1),
        ColorSchemeOverride::Dark => window.invoke_force_color_scheme(2),
    }

    let start_hidden = ctx.start_hidden;
    let resume = ctx.resume_connection;
    let controller = Controller::new(window, ctx, options);
    CONTROLLER.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&controller)));
    controller.start(start_hidden, resume);
    drop(controller);

    let result = slint::run_event_loop_until_quit();
    // Tear down on this thread: drops the tray icon, window, timers and the guard.
    let controller = CONTROLLER.with(|slot| slot.borrow_mut().take());
    drop(controller);
    result.map_err(Into::into)
}

/// Repaint cadence while the window is visible (level meter at ~30 fps).
const TICK_VISIBLE: Duration = Duration::from_millis(33);
/// Cadence while hidden (tray state only).
const TICK_HIDDEN: Duration = Duration::from_millis(500);
/// Custom latency is applied this long after the last SpinBox edit.
const LATENCY_DEBOUNCE: Duration = Duration::from_millis(600);
/// Trailing send of the volume after the last slider movement.
const VOLUME_TRAILING: Duration = Duration::from_millis(150);
/// A second tray click within this time is ignored (double-click = one toggle).
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(400);
/// How often the tray health check runs.
const TRAY_MAINTAIN_EVERY: Duration = Duration::from_secs(1);
/// Default volume shown when the speaker volume is not controlled.
const DEFAULT_VOLUME: f32 = 40.0;
/// Project page.
const PROJECT_URL: &str = "https://github.com/sheepy928/WinPods";
/// Third-party notices shown under About: `assets/THIRD-PARTY-NOTICES.txt`, embedded
/// at compile time exactly as that file is in the source tree (nothing here generates
/// or replaces it).
const THIRD_PARTY_NOTICES: &str = include_str!("../assets/THIRD-PARTY-NOTICES.txt");
/// Hand-off argument: the updated process connects at start (the user was connected).
pub(crate) const RESUME_ARG: &str = "--resume";

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<Controller>>> = const { RefCell::new(None) };
}

/// Run `f` with the UI controller (UI thread only; a no-op before start/after quit).
fn with_controller(f: impl FnOnce(&Controller)) {
    let controller = CONTROLLER.with(|slot| slot.borrow().clone());
    if let Some(controller) = controller {
        f(&controller);
    }
}

/// Events delivered to the UI thread from other threads.
enum UiEvent {
    Tray(TrayEvent),
    Activate,
    UpdateChecked {
        result: Result<Option<Release>, String>,
        /// "Check now" (ignores a skipped version) rather than the schedule.
        manual: bool,
    },
    UpdateProgress {
        done: u64,
        total: u64,
    },
    /// The download is verified; the exe swap starts.
    UpdateSwapping,
    UpdateInstalled(Result<Release, (String, InstallFailure)>),
    ShutdownFinished,
}

/// Hand an event to the UI thread (callable from any thread).
fn post(event: UiEvent) {
    if let Err(err) = slint::invoke_from_event_loop(move || with_controller(|c| c.handle(event))) {
        tracing::debug!("UI event dropped (event loop gone): {err}");
    }
}

/// Activation handler for the single-instance guard (it runs on the guard's listener
/// thread, which acknowledges once the handler returns). Normally it calls `activate`
/// (show the window) and returns. Once `closed` is set (quitting), it never returns,
/// so the new launch gets no acknowledgement: it then waits for this process to exit
/// (it holds the mutex until then) and starts normally. Acknowledging would make it
/// give up while this process goes away, so starting WinPods again would do nothing.
fn activation_handler(
    closed: Arc<AtomicBool>,
    activate: impl Fn() + Send + 'static,
) -> Box<dyn Fn() + Send + 'static> {
    Box::new(move || {
        if closed.load(Ordering::Acquire) {
            tracing::info!("WinPods was started again while quitting; it starts once we exit");
            loop {
                std::thread::park();
            }
        }
        activate();
    })
}

/// Requests to the update-check thread.
enum UpdateRequest {
    /// "Check now": runs even with automatic updates off and ignores (and, when it
    /// finds a release, forgets) a skipped version.
    CheckNow,
    /// Run the automatic check now (automatic updates were just switched on).
    CheckAutomatic,
    Skip(Option<String>),
}

/// Why an install attempt failed; decides what the banner offers next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallFailure {
    /// The signature did not verify when the release was downloaded: it is not
    /// offered again in any form (no "Open release page").
    Refused,
    /// The exe folder cannot be written: this release is offered as its release page.
    NotWritable,
    /// Anything else (network, checksum, swap): "Install" stays available to retry.
    Retryable,
}

impl InstallFailure {
    fn of(err: &UpdateError) -> InstallFailure {
        match err {
            UpdateError::Signature(_) => InstallFailure::Refused,
            UpdateError::NotWritable(_) => InstallFailure::NotWritable,
            _ => InstallFailure::Retryable,
        }
    }
}

/// Keeps Quit from cutting an exe swap in half. The install thread runs the swap
/// through [`SwapGate::run`]; Quit calls [`SwapGate::close`] (never blocks) and, off the
/// UI thread, [`SwapGate::wait_idle`] before the process exits. A swap that started
/// before `close` finishes (or rolls back) first; none starts after it. Downloads are
/// not covered: they are abandoned at exit and leave only a harmless `.new` file.
#[derive(Default)]
struct SwapGate {
    closed: AtomicBool,
    running: Mutex<()>,
}

impl SwapGate {
    /// Run `swap` while holding the gate, or return `None` without running it once
    /// Quit has closed the gate.
    fn run<R>(&self, swap: impl FnOnce() -> R) -> Option<R> {
        let _running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        Some(swap())
    }

    /// No swap may start from now on.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Block until a swap in progress (if any) has finished.
    fn wait_idle(&self) {
        drop(self.running.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// What the update machinery is doing. Checking and installing are tracked apart so
/// that a background check finishing during a download cannot re-enable "Install".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum UpdatePhase {
    #[default]
    Idle,
    /// A manual "Check now" is in flight.
    Checking,
    /// Downloading, verifying and swapping in a release (ends with a restart or an error).
    Installing,
}

/// Update banner state.
#[derive(Default)]
struct UpdateUi {
    /// A newer release whose signed checksums verified (from [`Updater::check`]).
    available: Option<Release>,
    phase: UpdatePhase,
    status: String,
    /// Why the last install attempt failed. Shown in both banners until the next
    /// successful check, a new install attempt, Skip or Dismiss.
    error: Option<String>,
    /// Installing in place is possible at all (writable folder, not a temp copy).
    can_install: bool,
    /// Tag of the release whose install found the folder read-only: that release is
    /// offered as its release page (later releases are tried in place again).
    page_only: Option<String>,
    /// The exe was swapped but the new version could not be started: nothing is
    /// offered again until WinPods restarts (a second swap would fail).
    restart_pending: Option<semver::Version>,
}

impl UpdateUi {
    /// Buttons that start update work are disabled while this is true.
    fn busy(&self) -> bool {
        self.phase != UpdatePhase::Idle
    }

    /// "Install & restart" (true) or "Open release page" (false) for the available
    /// release.
    fn offers_install(&self) -> bool {
        self.can_install
            && self
                .available
                .as_ref()
                .is_some_and(|r| self.page_only.as_deref() != Some(r.tag.as_str()))
    }

    /// Start a manual check; false while a check or an install is already running.
    fn begin_check(&mut self) -> bool {
        if self.busy() {
            return false;
        }
        self.phase = UpdatePhase::Checking;
        self.status = "Checking for updates…".to_string();
        true
    }

    /// Show the result of a manual or automatic check finished at `now_unix`. While an
    /// install runs the banner is left alone (returns false): the result must neither
    /// end the install state nor replace the download progress.
    fn finish_check(&mut self, result: Result<Option<Release>, String>, now_unix: u64) -> bool {
        if self.phase == UpdatePhase::Installing {
            return false;
        }
        self.phase = UpdatePhase::Idle;
        if let Some(version) = &self.restart_pending {
            self.status = format!("WinPods {version} is installed; restart WinPods to use it");
            return true;
        }
        match result {
            Ok(release) => {
                self.status = match &release {
                    Some(r) => format!("Version {} is available", r.version),
                    None => format!(
                        "WinPods is up to date · checked {}",
                        view::format_ago(now_unix, now_unix)
                    ),
                };
                self.available = release;
                self.error = None;
            }
            Err(err) => self.status = format!("Could not check for updates: {err}"),
        }
        true
    }

    /// Enter the install state; returns the release to install, or `None` when there is
    /// nothing to install or other update work is running.
    fn begin_install(&mut self) -> Option<Release> {
        if self.busy() {
            return None;
        }
        let release = self.available.clone()?;
        self.phase = UpdatePhase::Installing;
        self.status = "Downloading…".to_string();
        self.error = None;
        Some(release)
    }

    /// The install failed: back to idle with the error shown in both banners.
    fn install_failed(&mut self, message: &str, failure: InstallFailure) {
        self.phase = UpdatePhase::Idle;
        let text = match failure {
            InstallFailure::Refused => {
                self.available = None;
                format!("Update refused: the release signature did not verify ({message})")
            }
            InstallFailure::NotWritable => {
                self.page_only = self.available.as_ref().map(|r| r.tag.clone());
                format!("Update failed: {message}")
            }
            InstallFailure::Retryable => format!("Update failed: {message}"),
        };
        self.status = text.clone();
        self.error = Some(text);
    }

    /// The new exe is in place but could not be started; this process keeps running.
    fn restart_failed(&mut self, release: &Release, message: &str) {
        self.phase = UpdatePhase::Idle;
        self.available = None;
        self.restart_pending = Some(release.version.clone());
        let text = format!(
            "WinPods {} is installed, but it could not be started ({message}). Quit \
             WinPods and start it again to finish.",
            release.version
        );
        self.status = text.clone();
        self.error = Some(text);
    }
}

/// Mutable UI-thread state.
struct UiState {
    config: Config,
    shown: bool,
    /// The user's intent: keep a connection up (drives the Connect switch).
    wants: bool,
    /// Connect was pressed before a speaker address was set: show how to set one
    /// (cleared once an address is set or the connection is switched off).
    no_address_hint: bool,
    quitting: bool,
    last_snapshot: Option<Arc<Snapshot>>,
    devices: Option<Arc<Vec<DeviceInfo>>>,
    sources: Vec<CaptureSelection>,
    /// Rows currently in the Find popup (index = row of `App.scan-results`).
    scan_results: Vec<ScanResult>,
    scan: view::ScanTracker,
    volume: VolumeThrottle,
    meter: f32,
    last_meter_at: Instant,
    last_maintain: Instant,
    last_toggle: Option<Instant>,
    update: UpdateUi,
}

struct Controller {
    window: MainWindow,
    engine: Arc<dyn EngineApi>,
    demo: bool,
    updater: Option<Arc<Updater>>,
    state: RefCell<UiState>,
    tray: RefCell<Option<Box<dyn Tray>>>,
    shutdown: RefCell<Option<Box<dyn FnOnce() + Send>>>,
    instance: RefCell<Option<InstanceGuard>>,
    /// Set when quitting begins: activations from then on are left unanswered.
    activations_closed: Arc<AtomicBool>,
    update_tx: RefCell<Option<mpsc::Sender<UpdateRequest>>>,
    auto_update: Arc<AtomicBool>,
    swap_gate: Arc<SwapGate>,
    tick_timer: Timer,
    latency_timer: Timer,
    volume_timer: Timer,
    focus_attempts: Cell<u32>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn shared(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
}

fn string_model(items: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

fn ui_status_kind(kind: view::StatusKind) -> StatusKind {
    match kind {
        view::StatusKind::Idle => StatusKind::Idle,
        view::StatusKind::Busy => StatusKind::Busy,
        view::StatusKind::Ok => StatusKind::Ok,
        view::StatusKind::Error => StatusKind::Error,
    }
}

/// Open a URL or folder with the shell (UI thread: COM is initialised there by winit).
fn shell_open(target: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    let target_w = HSTRING::from(target);
    // SAFETY: all string arguments are valid NUL-terminated UTF-16 for the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &target_w,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success.
    if result.0 as isize <= 32 {
        tracing::warn!(
            "could not open {target} (ShellExecute {})",
            result.0 as isize
        );
    }
}

/// Restore and focus the native window. Returns false while the HWND does not exist.
fn focus_native_window(window: &slint::Window) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };

    let handle = window.window_handle();
    let Ok(handle) = handle.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return false;
    };
    let hwnd = HWND(win32.hwnd.get() as *mut core::ffi::c_void);
    // SAFETY: `hwnd` is the live top-level window owned by this thread's event loop.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
    true
}

impl Controller {
    fn new(window: MainWindow, ctx: UiContext, options: RunOptions) -> Rc<Controller> {
        let now = Instant::now();
        let mut config = ctx.config;
        let snapshot = ctx.engine.snapshot();
        // A speaker address known to the engine but not to config (first demo run).
        if config.device_ip.trim().is_empty() && !snapshot.device_ip.trim().is_empty() {
            config.device_ip = snapshot.device_ip.trim().to_string();
        }
        // False for a temp copy or a read-only folder: only the release page is offered.
        let can_install = !ctx.demo && ctx.updater.is_some() && Updater::can_install_in_place();
        let auto_update = Arc::new(AtomicBool::new(config.auto_update));
        let mut update = UpdateUi {
            can_install: can_install || ctx.demo,
            status: match (&ctx.updated_from, config.last_update_check) {
                (Some(old), _) => format!("Updated from {old} to {}", ctx.version),
                (None, Some(last)) => {
                    format!("Last checked {}", view::format_ago(unix_now(), last))
                }
                (None, None) => "Not checked yet".to_string(),
            },
            ..UpdateUi::default()
        };
        if ctx.demo && options.demo_update_available {
            let release = demo_release();
            update.status = format!("Version {} is available", release.version);
            update.available = Some(release);
        }

        let controller = Rc::new(Controller {
            window,
            engine: ctx.engine,
            demo: ctx.demo,
            updater: ctx.updater,
            state: RefCell::new(UiState {
                config,
                shown: false,
                wants: false,
                no_address_hint: false,
                quitting: false,
                last_snapshot: None,
                devices: None,
                sources: Vec::new(),
                scan_results: Vec::new(),
                scan: view::ScanTracker::default(),
                volume: VolumeThrottle::default(),
                meter: 0.0,
                last_meter_at: now,
                last_maintain: now,
                last_toggle: None,
                update,
            }),
            tray: RefCell::new(None),
            shutdown: RefCell::new(Some(ctx.shutdown)),
            instance: RefCell::new(ctx.instance),
            activations_closed: Arc::default(),
            update_tx: RefCell::new(None),
            auto_update,
            swap_gate: Arc::default(),
            tick_timer: Timer::default(),
            latency_timer: Timer::default(),
            volume_timer: Timer::default(),
            focus_attempts: Cell::new(0),
        });
        controller.init_properties(&ctx.version, options);
        controller.bind_callbacks();
        controller
    }

    fn app(&self) -> App<'_> {
        self.window.global::<App>()
    }

    // ------------------------------------------------------------------ start-up

    fn init_properties(&self, version: &str, options: RunOptions) {
        let app = self.app();
        let s = self.state.borrow();
        let config = &s.config;
        app.set_version(shared(version));
        app.set_demo(self.demo);
        app.set_page(match options.start_page {
            Page::Main => 0,
            Page::Settings => 1,
        });
        app.set_licenses_text(shared(THIRD_PARTY_NOTICES));

        let latency = config.latency_clamped();
        app.set_latency_ms(latency as i32);
        app.set_latency_custom(view::preset_index(latency).is_none());
        app.set_volume_enabled(config.volume.is_some());
        app.set_volume(view::volume_percent(
            config.volume.unwrap_or(DEFAULT_VOLUME),
        ));

        app.set_speaker_ip(shared(&config.device_ip));
        app.set_start_with_windows(config.start_with_windows);
        app.set_start_minimized(config.start_minimized);
        app.set_auto_connect(config.auto_connect);
        app.set_auto_update(config.auto_update);
        app.set_updates_supported(self.demo || self.updater.is_some());
        app.set_clock_index(view::timing_index(&config.timing_protocol));
        app.set_codec_index(view::codec_index(&config.codec));
        app.set_renderer_index(view::option_index(&RENDERERS, &config.renderer, 0));
        app.set_log_level_index(view::option_index(&LOG_LEVELS, &config.log_level, 1));
        drop(s);
        self.push_update_ui();
        self.update_latency_hint();
    }

    fn start(&self, start_hidden: bool, resume: bool) {
        // Engine-facing start-up: devices, first snapshot, auto-connect.
        let snapshot = self.engine.snapshot();
        if snapshot.devices.is_empty() {
            self.engine.send(Command::RefreshDevices);
        }
        self.apply_snapshot(&snapshot);
        // After a self-update the stream resumes when the user was connected, whatever
        // "Connect automatically" says.
        let connect = {
            let s = self.state.borrow();
            (s.config.auto_connect || resume)
                && !view::effective_address(&snapshot, &s.config.device_ip).is_empty()
        };
        if resume {
            tracing::info!(connect, "resuming the connection after the update");
        }
        if connect {
            self.set_wants(true);
            self.engine.send(Command::Connect);
        }

        if !self.demo {
            self.reconcile_autostart();
            self.start_update_checks();
            if let Some(guard) = self.instance.borrow().as_ref() {
                guard.on_activate(activation_handler(
                    Arc::clone(&self.activations_closed),
                    || post(UiEvent::Activate),
                ));
            }
        } else {
            self.app().set_autostart_note(shared(
                "Demo mode: the Windows startup entry is not changed",
            ));
        }

        // Tray (after the window exists, so the Slint/winit event loop is initialised).
        let model = self.tray_model();
        match SystemTray::new(
            |event| with_controller(|c| c.handle(UiEvent::Tray(event))),
            &model,
        ) {
            Ok(tray) => *self.tray.borrow_mut() = Some(Box::new(tray)),
            Err(err) => tracing::warn!("no tray icon: {err}"),
        }

        self.window.window().on_close_requested(|| {
            let mut response = CloseRequestResponse::HideWindow;
            with_controller(|c| response = c.on_close_requested());
            response
        });

        self.tick_timer.start(TimerMode::Repeated, TICK_HIDDEN, || {
            with_controller(|c| c.tick());
        });

        // Without a tray there would be no way back to a hidden window.
        if !start_hidden || self.tray.borrow().is_none() {
            self.show_window();
        }
    }

    fn bind_callbacks(&self) {
        let app = self.app();
        app.on_connect_toggled(|on| {
            with_controller(|c| {
                c.set_connection(on);
            })
        });
        app.on_source_selected(|index| with_controller(|c| c.select_source(index)));
        app.on_refresh_sources(|| with_controller(|c| c.engine.send(Command::RefreshDevices)));
        app.on_dismiss_update_error(|| with_controller(|c| c.dismiss_update_error()));
        app.on_latency_preset(|index| {
            with_controller(|c| {
                if let Some(&ms) = usize::try_from(index)
                    .ok()
                    .and_then(|i| LATENCY_PRESETS.get(i))
                {
                    c.latency_timer.stop();
                    c.set_latency(ms);
                }
            })
        });
        app.on_latency_custom_edited(|_| with_controller(|c| c.custom_latency_edited()));
        app.on_volume_enabled_toggled(|on| with_controller(|c| c.set_volume_control(on)));
        app.on_volume_moved(|value| with_controller(|c| c.volume_moved(value)));
        app.on_volume_released(|value| with_controller(|c| c.volume_released(value)));
        app.on_speaker_ip_accepted(|text| with_controller(|c| c.speaker_ip_accepted(&text)));
        app.on_find_speakers(|| with_controller(|c| c.find_speakers()));
        app.on_scan_result_chosen(|index| with_controller(|c| c.scan_result_chosen(index)));
        app.on_start_with_windows_toggled(|on| with_controller(|c| c.set_start_with_windows(on)));
        app.on_use_this_copy(|| with_controller(|c| c.use_this_copy()));
        app.on_start_minimized_toggled(|on| {
            with_controller(|c| c.edit_config(|config| config.start_minimized = on))
        });
        app.on_auto_connect_toggled(|on| {
            with_controller(|c| c.edit_config(|config| config.auto_connect = on))
        });
        app.on_auto_update_toggled(|on| with_controller(|c| c.set_auto_update(on)));
        app.on_check_updates(|| with_controller(|c| c.check_updates()));
        app.on_install_update(|| with_controller(|c| c.install_update()));
        app.on_skip_update(|| with_controller(|c| c.skip_update()));
        app.on_clock_selected(|index| {
            with_controller(|c| {
                let timing = view::timing_from_index(index);
                c.edit_config(|config| config.timing_protocol = timing.as_str().to_string());
                c.engine.send(Command::SetTiming(timing));
            })
        });
        app.on_codec_selected(|index| {
            with_controller(|c| {
                let codec = view::codec_from_index(index);
                c.edit_config(|config| config.codec = codec.as_str().to_string());
                c.engine.send(Command::SetCodec(codec));
            })
        });
        app.on_renderer_selected(|index| {
            with_controller(|c| {
                let renderer = view::option_value(&RENDERERS, index, 0).to_string();
                c.edit_config(|config| config.renderer = renderer);
            })
        });
        app.on_log_level_selected(|index| {
            with_controller(|c| {
                let level = view::option_value(&LOG_LEVELS, index, 1).to_string();
                c.edit_config(|config| config.log_level = level);
            })
        });
        app.on_open_log_folder(|| with_controller(|c| c.open_log_folder()));
        app.on_open_url(|url| shell_open(&url));
        app.on_close_hint_closed(|quit| with_controller(|c| c.close_hint_closed(quit)));
    }

    // ------------------------------------------------------------------- events

    fn handle(&self, event: UiEvent) {
        match event {
            UiEvent::Tray(event) => self.handle_tray(event),
            UiEvent::Activate => self.show_window(),
            UiEvent::UpdateChecked { result, manual } => self.update_checked(result, manual),
            UiEvent::UpdateProgress { done, total } => {
                let percent = done.saturating_mul(100).checked_div(total).unwrap_or(0);
                self.state.borrow_mut().update.status = format!("Downloading… {percent}%");
                self.push_update_ui();
            }
            UiEvent::UpdateSwapping => {
                self.state.borrow_mut().update.status = "Installing…".to_string();
                self.push_update_ui();
            }
            UiEvent::UpdateInstalled(result) => self.update_installed(result),
            UiEvent::ShutdownFinished => self.finish_quit(),
        }
    }

    fn handle_tray(&self, event: TrayEvent) {
        match event {
            TrayEvent::ToggleWindow => {
                let now = Instant::now();
                let debounced = {
                    let mut s = self.state.borrow_mut();
                    let recent = s
                        .last_toggle
                        .is_some_and(|at| now.saturating_duration_since(at) < TOGGLE_DEBOUNCE);
                    if !recent {
                        s.last_toggle = Some(now);
                    }
                    recent
                };
                if !debounced {
                    if self.is_visible() {
                        self.hide_window();
                    } else {
                        self.show_window();
                    }
                }
            }
            TrayEvent::ShowWindow => self.show_window(),
            TrayEvent::ToggleConnection => {
                let wants = self.state.borrow().wants;
                if !self.set_connection(!wants) {
                    // No speaker address yet: show where the hint and the setting are.
                    self.show_window();
                }
            }
            TrayEvent::SetLatency(ms) => {
                self.latency_timer.stop();
                self.set_latency(ms);
            }
            TrayEvent::InstallUpdate => {
                // The main page's banner shows the progress and any failure.
                self.show_window();
                self.app().set_page(0);
                self.install_update();
            }
            TrayEvent::ShowAbout => {
                self.show_window();
                self.app().set_page(1);
                // Scroll once the settings page has been laid out.
                Timer::single_shot(Duration::from_millis(60), || {
                    with_controller(|c| {
                        let app = c.app();
                        app.set_about_request(app.get_about_request().wrapping_add(1));
                    })
                });
            }
            TrayEvent::Quit => self.quit(),
        }
        // The shell toggles check items by itself; re-apply ours.
        self.refresh_tray();
    }

    // ------------------------------------------------------------------- window

    fn is_visible(&self) -> bool {
        self.state.borrow().shown && !self.window.window().is_minimized()
    }

    fn show_window(&self) {
        if self.state.borrow().quitting {
            return;
        }
        if let Err(err) = self.window.show() {
            tracing::warn!("could not show the window: {err}");
            return;
        }
        let was_shown = std::mem::replace(&mut self.state.borrow_mut().shown, true);
        if !was_shown {
            // Devices may have been plugged in or removed while the window was hidden.
            self.engine.send(Command::RefreshDevices);
        }
        if self.window.window().is_minimized() {
            self.window.window().set_minimized(false);
        }
        self.focus_attempts.set(0);
        self.focus_when_ready();
        self.tick_timer.set_interval(TICK_VISIBLE);
        self.tick();
    }

    /// SetForegroundWindow once the HWND exists (it appears after an event loop turn).
    fn focus_when_ready(&self) {
        if focus_native_window(self.window.window()) {
            return;
        }
        let attempts = self.focus_attempts.get() + 1;
        self.focus_attempts.set(attempts);
        if attempts <= 20 {
            Timer::single_shot(Duration::from_millis(50), || {
                with_controller(|c| c.focus_when_ready())
            });
        }
    }

    fn hide_window(&self) {
        if let Err(err) = self.window.hide() {
            tracing::warn!("could not hide the window: {err}");
        }
        {
            let mut s = self.state.borrow_mut();
            s.shown = false;
            s.meter = 0.0;
        }
        let app = self.app();
        app.set_level(0.0);
        app.set_close_hint_visible(false);
        self.tick_timer.set_interval(TICK_HIDDEN);
    }

    fn on_close_requested(&self) -> CloseRequestResponse {
        if self.tray.borrow().is_none() {
            self.quit();
            return CloseRequestResponse::KeepWindowShown;
        }
        let first_close = {
            let mut s = self.state.borrow_mut();
            let first = !s.config.close_hint_shown;
            s.config.close_hint_shown = true;
            first
        };
        if first_close {
            self.save_config();
            self.app().set_close_hint_visible(true);
            return CloseRequestResponse::KeepWindowShown;
        }
        self.hide_window();
        CloseRequestResponse::KeepWindowShown
    }

    fn close_hint_closed(&self, quit: bool) {
        self.app().set_close_hint_visible(false);
        if quit {
            self.quit();
        } else {
            self.hide_window();
        }
    }

    // ---------------------------------------------------------------- quitting

    /// Hide everything at once, stop the engine on a helper thread, then quit.
    fn quit(&self) {
        self.begin_shutdown();
    }

    /// Quit (also the second half of the update hand-off, once the new exe started).
    fn begin_shutdown(&self) {
        {
            let mut s = self.state.borrow_mut();
            if s.quitting {
                return;
            }
            s.quitting = true;
        }
        // No exe swap may start from here on; one in progress is waited for below.
        self.swap_gate.close();
        // WinPods started again from now on is not acknowledged: it waits for this
        // process to exit and then starts normally (main.rs), instead of "activating"
        // an instance that is going away. Until then this process keeps the
        // single-instance mutex, so the two engines never overlap (the new one would
        // find UDP 319/320 busy and pair with the speaker while our session is still
        // up). The OS closes the mutex when the process exits; the guard is never
        // dropped, because dropping joins its listener thread, which an activation
        // arriving from now on parks for good.
        self.activations_closed.store(true, Ordering::Release);
        if let Some(guard) = self.instance.borrow_mut().take() {
            std::mem::forget(guard);
        }
        self.tick_timer.stop();
        self.latency_timer.stop();
        self.volume_timer.stop();
        let _ = self.window.hide();
        self.tray.borrow_mut().take();
        self.update_tx.borrow_mut().take();

        let shutdown = self.shutdown.borrow_mut().take();
        let gate = Arc::clone(&self.swap_gate);
        let spawned = std::thread::Builder::new()
            .name("engine-shutdown".into())
            .spawn(move || {
                if let Some(shutdown) = shutdown {
                    shutdown();
                }
                // Never exit between the two renames of an exe swap.
                gate.wait_idle();
                post(UiEvent::ShutdownFinished);
            });
        if let Err(err) = spawned {
            // The closure (and the hook) went down with the failed spawn.
            tracing::error!("could not start the shutdown thread: {err}");
            self.swap_gate.wait_idle();
            self.finish_quit();
        }
    }

    fn finish_quit(&self) {
        if let Err(err) = slint::quit_event_loop() {
            tracing::error!("could not quit the event loop: {err}");
        }
    }

    // ------------------------------------------------------------------ polling

    fn tick(&self) {
        let now = Instant::now();
        let visible = self.is_visible();
        if visible {
            let peak = self.engine.peak();
            let level = {
                let mut s = self.state.borrow_mut();
                let dt = now.saturating_duration_since(s.last_meter_at);
                s.last_meter_at = now;
                s.meter = view::meter_level(s.meter, peak, dt);
                s.meter
            };
            self.app().set_level(level);
        }

        let snapshot = self.engine.snapshot();
        let changed = self
            .state
            .borrow()
            .last_snapshot
            .as_ref()
            .is_none_or(|last| !Arc::ptr_eq(last, &snapshot));
        if changed {
            self.apply_snapshot(&snapshot);
        }

        let maintain = {
            let mut s = self.state.borrow_mut();
            let due = now.saturating_duration_since(s.last_maintain) >= TRAY_MAINTAIN_EVERY;
            if due {
                s.last_maintain = now;
            }
            due
        };
        if maintain && let Some(tray) = self.tray.borrow_mut().as_mut() {
            tray.maintain(now);
        }

        let desired = if visible { TICK_VISIBLE } else { TICK_HIDDEN };
        if self.tick_timer.interval() != desired {
            self.tick_timer.set_interval(desired);
        }
    }

    fn apply_snapshot(&self, snap: &Arc<Snapshot>) {
        let app = self.app();
        let (title, detail, devices_changed, wants_changed) = {
            let mut s = self.state.borrow_mut();
            s.last_snapshot = Some(Arc::clone(snap));
            // The engine gives up (and stays in Error) when there is no address to
            // connect to; checked on every snapshot, not only on state transitions.
            let wants_changed = s.wants && view::engine_gave_up(snap, &s.config.device_ip);
            if wants_changed {
                s.wants = false;
            }
            // Compared by value: every refresh (window shown, Source opened) publishes
            // a new list, and rebuilding the combo box needlessly would disturb it.
            let devices_changed = view::devices_changed(s.devices.as_ref(), &snap.devices);
            s.devices = Some(Arc::clone(&snap.devices));
            // Only a name discovered for the configured address is saved.
            let discovered =
                view::name_to_persist(snap, &s.config.device_ip, &s.config.device_name)
                    .map(str::to_string);
            let save = discovered.is_some();
            if let Some(name) = discovered {
                s.config.device_name = name;
            }
            let title = view::speaker_title(snap, &s.config.device_name, &s.config.device_ip);
            let detail = view::speaker_detail(snap, &s.config.device_ip);
            drop(s);
            if save {
                self.save_config();
            }
            (title, detail, devices_changed, wants_changed)
        };

        app.set_speaker_name(shared(title));
        app.set_speaker_detail(shared(detail));
        self.refresh_status();
        app.set_capture_detail(shared(view::capture_detail(snap)));
        app.set_stats_text(shared(view::stats_line(snap)));
        if wants_changed {
            app.set_connect_on(false);
        }
        if devices_changed {
            self.rebuild_sources();
        }
        self.update_latency_hint();
        self.apply_scan(&snap.scan);
        self.refresh_tray();
    }

    // -------------------------------------------------------------------- tray

    fn tray_model(&self) -> TrayModel {
        let s = self.state.borrow();
        let raw = s
            .last_snapshot
            .clone()
            .unwrap_or_else(|| self.engine.snapshot());
        let snapshot = view::presented(&raw, &s.config.device_ip, s.wants);
        let speaker = view::speaker_title(&snapshot, &s.config.device_name, &s.config.device_ip);
        TrayModel {
            state: snapshot.state,
            tooltip: view::tray_tooltip(&snapshot, &speaker),
            connected: s.wants,
            latency_ms: s.config.latency_clamped(),
            update_version: s.update.available.as_ref().map(|r| r.version.to_string()),
            update_in_place: s.update.offers_install(),
        }
    }

    fn refresh_tray(&self) {
        let model = self.tray_model();
        if let Some(tray) = self.tray.borrow_mut().as_mut() {
            tray.update(&model);
        }
    }

    // -------------------------------------------------------------- connection

    fn set_wants(&self, wants: bool) {
        self.state.borrow_mut().wants = wants;
        self.app().set_connect_on(wants);
    }

    /// Switch the connection on or off. Returns false when "on" is impossible because
    /// no speaker address is set: the switch springs back and the card shows how to
    /// set one. The engine is not asked to connect then (it would only enter an error
    /// state that outlives the address being set).
    fn set_connection(&self, on: bool) -> bool {
        let possible = on && {
            let s = self.state.borrow();
            let snapshot = s
                .last_snapshot
                .clone()
                .unwrap_or_else(|| self.engine.snapshot());
            !view::effective_address(&snapshot, &s.config.device_ip).is_empty()
        };
        self.set_wants(possible);
        self.state.borrow_mut().no_address_hint = on && !possible;
        if possible {
            self.engine.send(Command::Connect);
        } else if !on {
            self.engine.send(Command::Disconnect);
        }
        self.refresh_status();
        self.refresh_tray();
        possible || !on
    }

    /// Status pill and the line under the speaker card (engine error or the address
    /// hint), from the last snapshot as presented (a stale "no address" error is shown
    /// as Disconnected once an address is set).
    fn refresh_status(&self) {
        let (text, kind, error) = {
            let s = self.state.borrow();
            let raw = s
                .last_snapshot
                .clone()
                .unwrap_or_else(|| self.engine.snapshot());
            let snap = view::presented(&raw, &s.config.device_ip, s.wants);
            let has_address = !view::effective_address(&snap, &s.config.device_ip).is_empty();
            (
                view::status_text(&snap, has_address),
                view::StatusKind::from(snap.state),
                view::error_line(&snap, s.no_address_hint),
            )
        };
        let app = self.app();
        app.set_status_text(shared(text));
        app.set_status_kind(ui_status_kind(kind));
        app.set_error_text(shared(error));
    }

    // ------------------------------------------------------------------ source

    fn rebuild_sources(&self) {
        let choices = {
            let mut s = self.state.borrow_mut();
            let devices = s.devices.clone().unwrap_or_default();
            let current = CaptureSelection {
                name: s.config.capture_device.clone(),
                id: s.config.capture_device_id.clone(),
            };
            let choices = view::source_choices(&devices, &current);
            s.sources = choices.selections.clone();
            choices
        };
        let app = self.app();
        app.set_sources(string_model(choices.labels));
        app.set_source_index(i32::try_from(choices.current).unwrap_or(0));
    }

    fn select_source(&self, index: i32) {
        let selection = {
            let mut s = self.state.borrow_mut();
            let Some(selection) = usize::try_from(index)
                .ok()
                .and_then(|i| s.sources.get(i))
                .cloned()
            else {
                return;
            };
            s.config.capture_device = selection.name.clone();
            s.config.capture_device_id = selection.id.clone();
            selection
        };
        self.save_config();
        self.engine.send(Command::SetCapture(selection));
    }

    // ----------------------------------------------------------------- latency

    fn set_latency(&self, ms: u32) {
        let ms = view::clamp_latency(i64::from(ms));
        let changed = {
            let mut s = self.state.borrow_mut();
            let changed = s.config.latency_ms != ms;
            s.config.latency_ms = ms;
            changed
        };
        let app = self.app();
        app.set_latency_ms(ms as i32);
        if view::preset_index(ms).is_some() {
            app.set_latency_custom(false);
        }
        self.update_latency_hint();
        if changed {
            self.save_config();
            self.engine.send(Command::SetLatency(ms));
        }
        self.refresh_tray();
    }

    fn custom_latency_edited(&self) {
        // The SpinBox writes App.latency-ms directly; show the hint now, apply later.
        self.update_latency_hint();
        self.latency_timer
            .start(TimerMode::SingleShot, LATENCY_DEBOUNCE, || {
                with_controller(|c| {
                    let ms = c.app().get_latency_ms();
                    c.set_latency(u32::try_from(ms).unwrap_or(0));
                })
            });
    }

    fn update_latency_hint(&self) {
        let app = self.app();
        let render_ms = self
            .state
            .borrow()
            .last_snapshot
            .as_ref()
            .and_then(|s| s.render_ms);
        let latency = view::clamp_latency(i64::from(app.get_latency_ms()));
        let hint = view::latency_hint(latency, render_ms);
        app.set_latency_hint(shared(hint.text()));
        app.set_latency_warning(hint.warning);
    }

    // ------------------------------------------------------------------ volume

    fn send_volume(&self, value: Option<f32>) {
        self.state.borrow_mut().config.volume = value;
        self.engine.send(Command::SetVolume(value));
    }

    fn set_volume_control(&self, on: bool) {
        self.volume_timer.stop();
        let value = on.then(|| view::volume_percent(self.app().get_volume()));
        self.state.borrow_mut().volume.reset(value);
        self.send_volume(value);
        self.save_config();
    }

    fn volume_moved(&self, value: f32) {
        if !self.app().get_volume_enabled() {
            return;
        }
        let send = self
            .state
            .borrow_mut()
            .volume
            .on_drag(value, Instant::now());
        match send {
            Some(value) => self.send_volume(Some(value)),
            None => {
                // Deliver the latest value even if no release follows (keyboard).
                self.volume_timer
                    .start(TimerMode::SingleShot, VOLUME_TRAILING, || {
                        with_controller(|c| c.volume_released(c.app().get_volume()))
                    });
            }
        }
    }

    fn volume_released(&self, value: f32) {
        self.volume_timer.stop();
        if !self.app().get_volume_enabled() {
            return;
        }
        let send = self
            .state
            .borrow_mut()
            .volume
            .on_release(value, Instant::now());
        if let Some(value) = send {
            self.send_volume(Some(value));
        }
        self.save_config();
    }

    // ---------------------------------------------------------- speaker / scan

    fn speaker_ip_accepted(&self, text: &str) {
        let app = self.app();
        let text = text.trim();
        let address = if text.is_empty() {
            String::new()
        } else if let Some(ip) = view::parse_ipv4(text) {
            ip.to_string()
        } else {
            app.set_speaker_ip_error(shared("Enter an IPv4 address such as 192.168.1.20"));
            return;
        };
        app.set_speaker_ip_error(SharedString::new());
        app.set_speaker_ip(shared(&address));
        self.set_device(address, None);
    }

    /// Use a new receiver address (`name` when it is known, e.g. from a scan).
    fn set_device(&self, address: String, name: Option<String>) {
        let (address_changed, name_changed) = {
            let mut s = self.state.borrow_mut();
            let address_changed = s.config.device_ip != address;
            // A new address invalidates the stored name unless the scan told us it.
            let new_name = match name {
                Some(name) => name,
                None if address_changed => String::new(),
                None => s.config.device_name.clone(),
            };
            let name_changed = s.config.device_name != new_name;
            s.config.device_ip = address.clone();
            s.config.device_name = new_name;
            if !address.is_empty() {
                // The first-run "no speaker address" hint no longer applies.
                s.no_address_hint = false;
            }
            (address_changed, name_changed)
        };
        if address_changed || name_changed {
            self.save_config();
        }
        if address_changed {
            self.engine.send(Command::SetDeviceIp(address));
        }
        self.refresh_status();
        self.refresh_tray();
    }

    fn find_speakers(&self) {
        self.engine.send(Command::Scan);
        {
            let mut s = self.state.borrow_mut();
            s.scan.request(Instant::now());
            s.scan_results.clear();
        }
        let app = self.app();
        app.set_scan_running(true);
        app.set_scan_message(shared(view::scan_view(&ScanState::Running).message));
        app.set_scan_results(ModelRc::new(VecModel::<ScanRow>::default()));
    }

    /// Refresh the Find popup, but only when the scan state changed: replacing the
    /// rows' model mid-click would destroy the pressed row and lose the click.
    fn apply_scan(&self, scan: &ScanState) {
        let results = {
            let mut s = self.state.borrow_mut();
            let Some(view) = s.scan.update(scan, Instant::now()) else {
                return;
            };
            s.scan_results = view.results.clone();
            view
        };
        let app = self.app();
        app.set_scan_running(results.running);
        app.set_scan_message(shared(&results.message));
        let rows: Vec<ScanRow> = results
            .results
            .iter()
            .map(|r| ScanRow {
                name: shared(&r.name),
                detail: shared(view::scan_row_detail(r)),
            })
            .collect();
        app.set_scan_results(ModelRc::new(VecModel::from(rows)));
    }

    fn scan_result_chosen(&self, index: i32) {
        let chosen = usize::try_from(index)
            .ok()
            .and_then(|i| self.state.borrow().scan_results.get(i).cloned());
        let Some(result) = chosen else {
            return;
        };
        let app = self.app();
        app.set_speaker_ip(shared(&result.address));
        app.set_speaker_ip_error(SharedString::new());
        self.set_device(result.address.clone(), Some(result.name.clone()));
    }

    // ----------------------------------------------------------------- startup

    fn reconcile_autostart(&self) {
        if winpods_app::paths::is_temp_copy() {
            let app = self.app();
            app.set_autostart_available(false);
            app.set_autostart_note(shared(
                "Extract the zip first: WinPods is running from a temporary folder",
            ));
            return;
        }
        let enabled = self.state.borrow().config.start_with_windows;
        let status = winpods_app::autostart::reconcile(enabled);
        self.show_autostart_status(status);
    }

    fn show_autostart_status(&self, status: winpods_app::autostart::AutostartStatus) {
        use winpods_app::autostart::AutostartStatus;

        let app = self.app();
        let (on, note, other_copy) = match status {
            AutostartStatus::Disabled => (false, String::new(), false),
            AutostartStatus::Enabled {
                path,
                approved,
                is_current_exe,
            } => {
                let note = match (approved, is_current_exe) {
                    (false, _) => "Disabled in Task Manager (Startup apps)".to_string(),
                    (true, false) => format!("Starts {}", path.display()),
                    (true, true) => String::new(),
                };
                (true, note, !is_current_exe)
            }
        };
        app.set_start_with_windows(on);
        app.set_autostart_note(shared(note));
        app.set_autostart_other_copy(other_copy);
        let changed = {
            let mut s = self.state.borrow_mut();
            let changed = s.config.start_with_windows != on;
            s.config.start_with_windows = on;
            changed
        };
        if changed {
            self.save_config();
        }
    }

    fn set_start_with_windows(&self, on: bool) {
        if self.demo {
            self.state.borrow_mut().config.start_with_windows = on;
            return;
        }
        let result = if on {
            winpods_app::paths::exe_path().and_then(|exe| winpods_app::autostart::enable(&exe))
        } else {
            winpods_app::autostart::disable()
        };
        if let Err(err) = result {
            tracing::warn!("could not change the startup entry: {err}");
            let app = self.app();
            app.set_start_with_windows(!on);
            app.set_autostart_note(shared(format!("Could not change the startup entry: {err}")));
            return;
        }
        self.show_autostart_status(winpods_app::autostart::status());
    }

    fn use_this_copy(&self) {
        if self.demo {
            return;
        }
        match winpods_app::paths::exe_path().and_then(|exe| winpods_app::autostart::enable(&exe)) {
            Ok(()) => self.show_autostart_status(winpods_app::autostart::status()),
            Err(err) => {
                tracing::warn!("could not update the startup entry: {err}");
                self.app().set_autostart_note(shared(format!(
                    "Could not change the startup entry: {err}"
                )));
            }
        }
    }

    // ------------------------------------------------------------------ config

    fn edit_config(&self, edit: impl FnOnce(&mut Config)) {
        edit(&mut self.state.borrow_mut().config);
        self.save_config();
    }

    /// Atomically save config (never in demo mode).
    fn save_config(&self) {
        if self.demo {
            return;
        }
        let s = self.state.borrow();
        if let Err(err) = s.config.save() {
            tracing::warn!("could not save settings: {err}");
        }
    }

    fn open_log_folder(&self) {
        if self.demo {
            tracing::info!("demo mode: not opening the log folder");
            return;
        }
        let folder = winpods_app::paths::app_dir();
        shell_open(&folder.to_string_lossy());
    }

    // ----------------------------------------------------------------- updates

    fn push_update_ui(&self) {
        let app = self.app();
        let s = self.state.borrow();
        let update = &s.update;
        app.set_update_available(update.available.is_some());
        app.set_update_version(shared(
            update
                .available
                .as_ref()
                .map(|r| r.version.to_string())
                .unwrap_or_default(),
        ));
        app.set_update_status(shared(&update.status));
        app.set_update_busy(update.busy());
        app.set_update_can_install(update.offers_install());
        app.set_update_error(shared(update.error.as_deref().unwrap_or_default()));
    }

    fn start_update_checks(&self) {
        let Some(updater) = self.updater.clone() else {
            return;
        };
        let skipped = self.state.borrow().config.skipped_version.clone();
        let (tx, rx) = mpsc::channel();
        let auto = Arc::clone(&self.auto_update);
        // Plan: check at start (shortly, to keep start-up quiet), then every 24 h, so
        // an update found earlier is offered again after a restart.
        let spawned = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                update_check_loop(
                    &|skipped| updater.check(skipped),
                    &auto,
                    skipped,
                    LoopTiming::PRODUCTION,
                    &rx,
                    &|result, manual| post(UiEvent::UpdateChecked { result, manual }),
                );
            });
        match spawned {
            Ok(_) => *self.update_tx.borrow_mut() = Some(tx),
            Err(err) => tracing::warn!("could not start update checks: {err}"),
        }
    }

    fn set_auto_update(&self, on: bool) {
        self.auto_update.store(on, Ordering::Relaxed);
        self.edit_config(|config| config.auto_update = on);
        let due = view::update_check_due(unix_now(), self.state.borrow().config.last_update_check);
        if on && due {
            // An automatic check: it honours a skipped version (unlike "Check now").
            if let Some(tx) = self.update_tx.borrow().as_ref() {
                let _ = tx.send(UpdateRequest::CheckAutomatic);
            }
        }
    }

    fn check_updates(&self) {
        if !self.state.borrow_mut().update.begin_check() {
            return;
        }
        self.push_update_ui();
        if self.demo {
            Timer::single_shot(Duration::from_millis(1200), || {
                with_controller(|c| {
                    c.update_checked(Ok(Some(demo_release())), true);
                })
            });
            return;
        }
        let sent = self
            .update_tx
            .borrow()
            .as_ref()
            .is_some_and(|tx| tx.send(UpdateRequest::CheckNow).is_ok());
        if !sent {
            self.update_checked(Err("update checks are not available".to_string()), true);
        }
    }

    /// Result of a manual or automatic check (automatic ones arrive at any time, also
    /// while an install is running; the banner then keeps showing the install).
    fn update_checked(&self, result: Result<Option<Release>, String>, manual: bool) {
        let now = unix_now();
        let save = {
            let mut s = self.state.borrow_mut();
            let mut save = false;
            if result.is_ok() {
                s.config.last_update_check = Some(now);
                save = true;
            }
            // "Check now" ignores a skipped version; when it finds a release the skip
            // is forgotten, so the release stays offered (this is how a skip is undone).
            if manual && matches!(result, Ok(Some(_))) && s.config.skipped_version.take().is_some()
            {
                save = true;
            }
            if !s.update.finish_check(result, now) {
                tracing::debug!("update check finished during an install; banner unchanged");
            }
            save
        };
        if save {
            self.save_config();
        }
        self.push_update_ui();
        self.refresh_tray();
    }

    fn install_update(&self) {
        let (release, offers_install, busy) = {
            let s = self.state.borrow();
            (
                s.update.available.clone(),
                s.update.offers_install(),
                s.update.busy(),
            )
        };
        let Some(release) = release else {
            return;
        };
        if busy {
            return;
        }
        if self.demo {
            self.state.borrow_mut().update.status =
                "Demo mode: updates are not downloaded or installed".to_string();
            self.push_update_ui();
            return;
        }
        if !offers_install {
            // Temp copy or read-only folder. The release's signature verified when it
            // was found; the page URL is built from the repository and the tag.
            shell_open(&release.html_url);
            return;
        }
        let Some(updater) = self.updater.clone() else {
            return;
        };
        // Enter the install state before the thread starts: from here until a restart
        // or a failure, nothing (not even a background check) re-enables "Install".
        let Some(release) = self.state.borrow_mut().update.begin_install() else {
            return;
        };
        self.push_update_ui();
        let gate = Arc::clone(&self.swap_gate);
        let spawned = std::thread::Builder::new()
            .name("update-install".into())
            .spawn(move || {
                let last_report = Cell::new(None::<Instant>);
                let progress = |done: u64, total: u64| {
                    let due = last_report
                        .get()
                        .is_none_or(|at| at.elapsed() >= Duration::from_millis(150));
                    if due || done >= total {
                        last_report.set(Some(Instant::now()));
                        post(UiEvent::UpdateProgress { done, total });
                    }
                };
                if let Some(result) = install_worker(&updater, &release, &gate, &progress) {
                    post(UiEvent::UpdateInstalled(result));
                }
            });
        if let Err(err) = spawned {
            self.update_installed(Err((
                format!("could not start the download: {err}"),
                InstallFailure::Retryable,
            )));
        }
    }

    fn update_installed(&self, result: Result<Release, (String, InstallFailure)>) {
        if self.state.borrow().quitting {
            // Quit came first: never start the new version behind the user's back. A
            // swapped exe simply runs next time.
            if let Ok(release) = &result {
                tracing::info!("update {} installed while quitting", release.tag);
            }
            return;
        }
        match result {
            Ok(release) => {
                let resume = self.state.borrow().wants;
                match spawn_updated_exe(resume) {
                    Ok(()) => {
                        tracing::info!(resume, "update {}: handing off", release.tag);
                        self.state.borrow_mut().update.status =
                            format!("Restarting into WinPods {}…", release.version);
                        self.push_update_ui();
                        // The new process waits for this one to exit before starting
                        // its engine: stop ours, hide the tray, quit.
                        self.begin_shutdown();
                    }
                    Err(err) => {
                        // Keep running (the engine is untouched) and say what happened.
                        tracing::error!("update {}: {err}", release.tag);
                        self.state
                            .borrow_mut()
                            .update
                            .restart_failed(&release, &err);
                        self.push_update_ui();
                        self.refresh_tray();
                    }
                }
            }
            Err((message, failure)) => {
                self.state
                    .borrow_mut()
                    .update
                    .install_failed(&message, failure);
                self.push_update_ui();
                self.refresh_tray();
            }
        }
    }

    fn skip_update(&self) {
        let tag = {
            let mut s = self.state.borrow_mut();
            if s.update.busy() {
                return;
            }
            let Some(release) = s.update.available.take() else {
                return;
            };
            s.config.skipped_version = Some(release.tag.clone());
            s.update.status = format!("Skipped {} · Check now offers it again", release.tag);
            s.update.error = None;
            release.tag
        };
        self.save_config();
        if let Some(tx) = self.update_tx.borrow().as_ref() {
            let _ = tx.send(UpdateRequest::Skip(Some(tag)));
        }
        self.push_update_ui();
        self.refresh_tray();
    }

    fn dismiss_update_error(&self) {
        self.state.borrow_mut().update.error = None;
        self.push_update_ui();
    }
}

/// Arguments that start the updated exe: `--post-update <pid> --updated-from
/// <version>`, plus [`RESUME_ARG`] when the user was connected (the new process then
/// connects regardless of "Connect automatically").
pub(crate) fn handoff_args(pid: u32, from_version: &str, resume: bool) -> Vec<String> {
    let mut args = vec![
        "--post-update".to_string(),
        pid.to_string(),
        "--updated-from".to_string(),
        from_version.to_string(),
    ];
    if resume {
        args.push(RESUME_ARG.to_string());
    }
    args
}

/// Start the freshly swapped-in exe (it waits for this process to exit).
fn spawn_updated_exe(resume: bool) -> Result<(), String> {
    let exe = winpods_app::paths::exe_path()
        .map_err(|err| format!("no executable path for the restart: {err}"))?;
    std::process::Command::new(&exe)
        .args(handoff_args(
            std::process::id(),
            env!("CARGO_PKG_VERSION"),
            resume,
        ))
        .spawn()
        .map(drop)
        .map_err(|err| format!("could not start {}: {err}", exe.display()))
}

/// The install thread: download and verify `release`, then swap it in unless Quit has
/// begun. `None` when Quit came first (nothing was swapped; the staged files are
/// removed).
fn install_worker(
    updater: &Updater,
    release: &Release,
    gate: &SwapGate,
    progress: &dyn Fn(u64, u64),
) -> Option<Result<Release, (String, InstallFailure)>> {
    let failed = |err: UpdateError| {
        tracing::warn!("update {} failed: {err}", release.tag);
        (err.to_string(), InstallFailure::of(&err))
    };
    let staged = match updater.stage(release, progress) {
        Ok(staged) => staged,
        Err(err) => return Some(Err(failed(err))),
    };
    post(UiEvent::UpdateSwapping);
    match gate.run(|| updater.install(&staged)) {
        Some(Ok(())) => Some(Ok(staged.release)),
        Some(Err(err)) => Some(Err(failed(err))),
        None => {
            for (file, _) in &staged.files {
                let _ = std::fs::remove_file(file);
            }
            tracing::info!("update {}: not installed, WinPods is quitting", release.tag);
            None
        }
    }
}

/// One update check (`skipped` → newest offered release), as [`Updater::check`].
type CheckFn<'a> = dyn Fn(Option<&str>) -> Result<Option<Release>, UpdateError> + 'a;

/// Timing of [`update_check_loop`] (shortened in tests).
#[derive(Debug, Clone, Copy)]
struct LoopTiming {
    /// Delay of the first check after start-up.
    first: Duration,
    /// Time between automatic checks.
    period: Duration,
}

impl LoopTiming {
    const PRODUCTION: LoopTiming = LoopTiming {
        first: view::UPDATE_STARTUP_DELAY,
        period: view::UPDATE_PERIOD,
    };
}

/// Background update schedule: first check `timing.first` after start, then every
/// `timing.period`, backing off 1/5/30 min after failures. Automatic checks honour
/// `skipped` and the automatic-updates switch; "Check now" runs regardless and ignores
/// the skip (and forgets it when it finds a release). Every result goes to `report`
/// with whether it was manual. Ends when the UI drops the sender.
fn update_check_loop(
    check: &CheckFn<'_>,
    auto: &AtomicBool,
    mut skipped: Option<String>,
    timing: LoopTiming,
    requests: &mpsc::Receiver<UpdateRequest>,
    report: &dyn Fn(Result<Option<Release>, String>, bool),
) {
    let mut next = Instant::now() + timing.first;
    let mut failures = 0usize;
    loop {
        let manual = match requests.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(UpdateRequest::CheckNow) => true,
            Ok(UpdateRequest::CheckAutomatic) | Err(RecvTimeoutError::Timeout) => false,
            Ok(UpdateRequest::Skip(version)) => {
                skipped = version;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if !manual && !auto.load(Ordering::Relaxed) {
            next = Instant::now() + timing.period;
            continue;
        }
        let result = check(if manual { None } else { skipped.as_deref() });
        if manual && matches!(result, Ok(Some(_))) {
            skipped = None;
        }
        next = Instant::now()
            + match &result {
                Ok(_) => {
                    failures = 0;
                    timing.period
                }
                Err(err) => {
                    tracing::info!("update check failed: {err}");
                    let delay = view::update_retry_delay(failures);
                    failures += 1;
                    delay
                }
            };
        report(result.map_err(|err| err.to_string()), manual);
    }
}

/// The fake release offered in demo mode.
fn demo_release() -> Release {
    Release {
        tag: "v0.3.0".to_string(),
        version: semver::Version::new(0, 3, 0),
        html_url: format!("{PROJECT_URL}/releases"),
        notes: "Demo release".to_string(),
        assets: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activations_are_answered_until_quitting_begins() {
        let closed = Arc::new(AtomicBool::new(false));
        let (shown_tx, shown_rx) = mpsc::channel();
        let shown_tx = Mutex::new(shown_tx);
        let handler = activation_handler(Arc::clone(&closed), move || {
            shown_tx.lock().unwrap().send(()).unwrap();
        });

        // Normal: the window is shown and the handler returns (the guard then acks).
        handler();
        assert!(shown_rx.try_recv().is_ok());

        // Quitting: the handler neither shows the window nor returns, so the new
        // launch gets no ack and waits for this process to exit.
        closed.store(true, Ordering::Release);
        let (returned_tx, returned_rx) = mpsc::channel();
        std::thread::spawn(move || {
            handler();
            let _ = returned_tx.send(());
        });
        assert_eq!(
            returned_rx.recv_timeout(Duration::from_millis(300)),
            Err(RecvTimeoutError::Timeout),
            "an activation while quitting must not be acknowledged"
        );
        assert!(shown_rx.try_recv().is_err());
        // The parked thread ends with the test process, as the listener does in the app.
    }

    #[test]
    fn status_kinds_map_to_slint_enum() {
        assert_eq!(ui_status_kind(view::StatusKind::Idle), StatusKind::Idle);
        assert_eq!(ui_status_kind(view::StatusKind::Busy), StatusKind::Busy);
        assert_eq!(ui_status_kind(view::StatusKind::Ok), StatusKind::Ok);
        assert_eq!(ui_status_kind(view::StatusKind::Error), StatusKind::Error);
    }

    #[test]
    fn run_options_default_to_system_theme_and_main_page() {
        let options = RunOptions::default();
        assert_eq!(options.color_scheme, ColorSchemeOverride::System);
        assert_eq!(options.start_page, Page::Main);
        assert!(!options.demo_update_available);
    }

    #[test]
    fn demo_release_is_newer_than_this_build() {
        let release = demo_release();
        assert!(release.version > winpods_app::updater::current_version());
        assert!(release.html_url.starts_with(PROJECT_URL));
        assert_eq!(release.tag, format!("v{}", release.version));
    }

    #[test]
    fn manual_checks_do_not_overlap() {
        let mut update = UpdateUi::default();
        assert!(update.begin_check());
        assert!(update.busy());
        assert!(!update.begin_check());
        assert!(update.finish_check(Ok(None), 1_000));
        assert!(!update.busy());
        assert!(update.status.starts_with("WinPods is up to date"));
        assert!(update.begin_check());
        assert!(update.finish_check(Err("offline".into()), 1_000));
        assert_eq!(update.status, "Could not check for updates: offline");
        assert!(!update.busy());
    }

    #[test]
    fn background_check_cannot_end_an_install() {
        let mut update = UpdateUi::default();
        // Nothing to install yet.
        assert!(update.begin_install().is_none());
        // An automatic check (no begin_check) offers a release.
        assert!(update.finish_check(Ok(Some(demo_release())), 1_000));
        assert!(!update.busy());
        let release = update.begin_install().expect("release to install");
        assert_eq!(release.tag, "v0.3.0");
        assert_eq!(update.phase, UpdatePhase::Installing);
        update.status = "Downloading… 40%".to_string();

        // The 24 h / retry check lands mid-download: the banner keeps the install.
        assert!(!update.finish_check(Ok(None), 2_000));
        assert!(!update.finish_check(Err("offline".into()), 2_000));
        assert_eq!(update.phase, UpdatePhase::Installing);
        assert_eq!(update.status, "Downloading… 40%");
        assert!(update.available.is_some());
        // So neither a second install nor a manual check can start.
        assert!(update.begin_install().is_none());
        assert!(!update.begin_check());

        update.install_failed("connection reset", InstallFailure::Retryable);
        assert!(!update.busy());
        assert_eq!(update.status, "Update failed: connection reset");
        assert!(update.begin_install().is_some());
    }

    fn release(tag: &str) -> Release {
        Release {
            tag: tag.to_string(),
            version: winpods_app::updater::parse_tag(tag).unwrap(),
            html_url: format!("https://github.com/sheepy928/WinPods/releases/tag/{tag}"),
            notes: String::new(),
            assets: Vec::new(),
        }
    }

    fn offering(tag: &str) -> UpdateUi {
        let mut update = UpdateUi {
            can_install: true,
            ..UpdateUi::default()
        };
        assert!(update.finish_check(Ok(Some(release(tag))), 1_000));
        update
    }

    #[test]
    fn read_only_or_temp_installs_only_offer_the_release_page() {
        let mut update = offering("v0.3.0");
        assert!(update.offers_install());
        update.can_install = false; // temp copy / read-only folder
        assert!(!update.offers_install());
        assert!(!UpdateUi::default().offers_install(), "nothing available");
    }

    #[test]
    fn a_failed_install_is_shown_until_the_next_check() {
        let mut update = offering("v0.3.0");
        update.begin_install().unwrap();
        update.install_failed("SHA-256 does not match", InstallFailure::Retryable);
        // Visible (both banners show `error`), and Install stays available to retry.
        assert_eq!(
            update.error.as_deref(),
            Some("Update failed: SHA-256 does not match")
        );
        assert!(update.offers_install());
        // A new attempt clears it...
        update.begin_install().unwrap();
        assert_eq!(update.error, None);
        update.install_failed("offline", InstallFailure::Retryable);
        // ...so does a successful check, but not a failed one.
        assert!(update.finish_check(Err("offline".into()), 2_000));
        assert!(update.error.is_some());
        assert!(update.finish_check(Ok(Some(release("v0.3.0"))), 2_000));
        assert_eq!(update.error, None);
    }

    #[test]
    fn a_signature_failure_hides_the_release_but_not_later_ones() {
        let mut update = offering("v0.3.0");
        update.begin_install().unwrap();
        update.install_failed("SHA256SUMS.txt does not verify", InstallFailure::Refused);
        // No "Open release page" for a release whose signature failed.
        assert_eq!(update.available, None);
        assert!(!update.offers_install());
        assert!(
            update
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("Update refused"))
        );
        // A later (verified) release installs in place again.
        assert!(update.finish_check(Ok(Some(release("v0.3.1"))), 3_000));
        assert!(update.offers_install());
        assert_eq!(update.error, None);
    }

    #[test]
    fn a_read_only_folder_turns_only_that_release_into_a_page() {
        let mut update = offering("v0.3.0");
        update.begin_install().unwrap();
        update.install_failed(
            "C:\\Program Files\\WinPods is not writable",
            InstallFailure::NotWritable,
        );
        assert!(update.available.is_some());
        assert!(!update.offers_install(), "same release: release page");
        assert!(update.finish_check(Ok(Some(release("v0.4.0"))), 3_000));
        assert!(
            update.offers_install(),
            "a new release is tried in place again"
        );
    }

    #[test]
    fn a_failed_restart_keeps_running_and_offers_nothing_more() {
        let mut update = offering("v0.3.0");
        let installing = update.begin_install().unwrap();
        update.restart_failed(&installing, "access denied");
        assert!(!update.busy());
        assert_eq!(update.available, None);
        let error = update.error.clone().unwrap();
        assert!(
            error.contains("0.3.0") && error.contains("access denied"),
            "{error}"
        );
        // The exe is already swapped: a later check must not offer it again.
        assert!(update.finish_check(Ok(Some(release("v0.3.0"))), 4_000));
        assert_eq!(update.available, None);
        assert!(update.status.contains("restart WinPods"));
    }

    #[test]
    fn install_failures_follow_the_error_kind() {
        assert_eq!(
            InstallFailure::of(&UpdateError::Signature("x".into())),
            InstallFailure::Refused
        );
        assert_eq!(
            InstallFailure::of(&UpdateError::NotWritable("x".into())),
            InstallFailure::NotWritable
        );
        for err in [
            UpdateError::Network("x".into()),
            UpdateError::Integrity("x".into()),
            UpdateError::Swap("x".into()),
            UpdateError::Other("x".into()),
        ] {
            assert_eq!(InstallFailure::of(&err), InstallFailure::Retryable);
        }
    }

    #[test]
    fn swap_gate_refuses_to_start_a_swap_after_quit() {
        let gate = SwapGate::default();
        assert_eq!(gate.run(|| 1), Some(1));
        gate.close();
        let mut ran = false;
        assert_eq!(gate.run(|| ran = true), None);
        assert!(!ran);
        gate.wait_idle(); // nothing running: returns at once
    }

    #[test]
    fn quit_waits_for_a_swap_in_progress() {
        let gate = Arc::new(SwapGate::default());
        let finished = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = mpsc::channel();
        let swapper = {
            let (gate, finished) = (Arc::clone(&gate), Arc::clone(&finished));
            std::thread::spawn(move || {
                gate.run(|| {
                    started_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    finished.store(true, Ordering::SeqCst);
                })
            })
        };
        started_rx.recv().unwrap();
        // Quit begins mid-swap: close never blocks, wait_idle returns only afterwards.
        gate.close();
        gate.wait_idle();
        assert!(
            finished.load(Ordering::SeqCst),
            "exited between the renames"
        );
        assert_eq!(swapper.join().unwrap(), Some(()));
    }

    #[test]
    fn handoff_resumes_only_when_the_user_was_connected() {
        let plain = handoff_args(42, "0.2.0", false);
        assert_eq!(plain, ["--post-update", "42", "--updated-from", "0.2.0"]);
        let resume = handoff_args(42, "0.2.0", true);
        assert_eq!(resume.last().map(String::as_str), Some(RESUME_ARG));
    }

    /// Runs [`update_check_loop`] on a thread with a fake `check` that records the
    /// `skipped` argument and finds `v0.3.0` only when nothing is skipped.
    struct LoopHarness {
        requests: Option<mpsc::Sender<UpdateRequest>>,
        calls: mpsc::Receiver<Option<String>>,
        reports: mpsc::Receiver<(bool, bool)>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl LoopHarness {
        fn start(auto: bool, skipped: Option<&str>, timing: LoopTiming) -> LoopHarness {
            let (req_tx, req_rx) = mpsc::channel();
            let (call_tx, calls) = mpsc::channel();
            let (report_tx, reports) = mpsc::channel();
            let skipped = skipped.map(str::to_string);
            let thread = std::thread::spawn(move || {
                let auto = AtomicBool::new(auto);
                let check = |skipped: Option<&str>| {
                    call_tx.send(skipped.map(str::to_string)).unwrap();
                    Ok(skipped.is_none().then(|| release("v0.3.0")))
                };
                let report = |result: Result<Option<Release>, String>, manual: bool| {
                    let found = matches!(result, Ok(Some(_)));
                    report_tx.send((manual, found)).unwrap();
                };
                update_check_loop(&check, &auto, skipped, timing, &req_rx, &report);
            });
            LoopHarness {
                requests: Some(req_tx),
                calls,
                reports,
                thread: Some(thread),
            }
        }

        fn send(&self, request: UpdateRequest) {
            self.requests.as_ref().unwrap().send(request).unwrap();
        }

        fn next_call(&self) -> Option<String> {
            self.calls
                .recv_timeout(Duration::from_secs(10))
                .expect("a check")
        }

        fn next_report(&self) -> (bool, bool) {
            self.reports
                .recv_timeout(Duration::from_secs(10))
                .expect("a report")
        }
    }

    impl Drop for LoopHarness {
        fn drop(&mut self) {
            self.requests.take();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    #[test]
    fn updates_are_checked_at_start_and_then_periodically() {
        let timing = LoopTiming {
            first: Duration::from_millis(20),
            period: Duration::from_millis(40),
        };
        let harness = LoopHarness::start(true, None, timing);
        // Start-up check (production: a few seconds after start, whatever the last
        // check was), then the periodic ones.
        for _ in 0..3 {
            assert_eq!(harness.next_call(), None);
            assert_eq!(harness.next_report(), (false, true));
        }
        assert!(LoopTiming::PRODUCTION.first <= Duration::from_secs(10));
        assert_eq!(LoopTiming::PRODUCTION.period, view::UPDATE_PERIOD);
    }

    #[test]
    fn check_now_ignores_and_forgets_a_skipped_version() {
        let timing = LoopTiming {
            first: Duration::from_millis(10),
            period: Duration::from_millis(50),
        };
        let harness = LoopHarness::start(true, Some("v0.3.0"), timing);
        // Automatic checks honour the skip (nothing found).
        assert_eq!(harness.next_call().as_deref(), Some("v0.3.0"));
        assert_eq!(harness.next_report(), (false, false));
        // "Check now" does not, and finds the skipped release...
        harness.send(UpdateRequest::CheckNow);
        loop {
            let call = harness.next_call();
            let report = harness.next_report();
            if report.0 {
                assert_eq!(call, None);
                assert_eq!(report, (true, true));
                break;
            }
        }
        // ...which undoes the skip for later automatic checks too.
        assert_eq!(harness.next_call(), None);
        assert_eq!(harness.next_report(), (false, true));
    }

    #[test]
    fn automatic_checks_respect_the_switch_but_check_now_does_not() {
        let timing = LoopTiming {
            first: Duration::from_millis(10),
            period: Duration::from_millis(10),
        };
        let harness = LoopHarness::start(false, Some("v0.3.0"), timing);
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            harness.calls.try_recv().is_err(),
            "automatic updates are off"
        );
        harness.send(UpdateRequest::CheckNow);
        assert_eq!(harness.next_call(), None);
        assert_eq!(harness.next_report(), (true, true));
    }

    #[test]
    fn string_model_keeps_order() {
        use slint::Model;
        let model = string_model(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(model.row_count(), 2);
        assert_eq!(model.row_data(1).as_deref(), Some("b"));
    }

    #[test]
    fn notices_are_bundled() {
        assert!(THIRD_PARTY_NOTICES.contains("Slint"));
    }

    /// The custom-latency box on the main page: driven with real pointer and key
    /// events in a headless window (software renderer, no screen).
    mod wheel_safe_spin_box {
        use std::rc::Rc;

        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
        use slint::{ComponentHandle, LogicalPosition, PhysicalSize, PlatformError};

        #[allow(clippy::all, clippy::pedantic, missing_docs, unused_qualifications)]
        mod harness {
            slint::slint! {
                #[style = "fluent"]
                import { WheelSafeSpinBox } from "../ui/wheel-safe-spinbox.slint";

                export component Harness inherits Window {
                    width: 300px;
                    height: 120px;
                    in-out property <int> value: 100;
                    out property <int> edits;
                    out property <length> page-scrolled;
                    out property <bool> spin-focused: spin.has-focus;
                    out property <bool> spin-armed: spin.armed;
                    spin := WheelSafeSpinBox {
                        x: 10px;
                        y: 10px;
                        width: 132px;
                        minimum: 75;
                        maximum: 5000;
                        step-size: 5;
                        value <=> root.value;
                        edited(value) => {
                            root.edits += 1;
                        }
                        scroll-page(delta) => {
                            root.page-scrolled += delta;
                        }
                    }
                    // Something else that takes focus when clicked.
                    FocusScope {
                        x: 200px;
                        y: 60px;
                        width: 80px;
                        height: 40px;
                    }
                }
            }
        }
        use harness::Harness;

        struct HeadlessPlatform(Rc<MinimalSoftwareWindow>);

        impl Platform for HeadlessPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
                Ok(self.0.clone())
            }
        }

        struct Driver {
            ui: Harness,
        }

        impl Driver {
            fn pump(&self) {
                // Runs the `changed` handlers, as each event-loop iteration does.
                slint::platform::update_timers_and_animations();
            }

            fn send(&self, event: WindowEvent) {
                self.ui.window().dispatch_event(event);
                self.pump();
            }

            fn move_to(&self, x: f32, y: f32) {
                self.send(WindowEvent::PointerMoved {
                    position: LogicalPosition::new(x, y),
                });
            }

            fn click(&self, x: f32, y: f32) {
                self.move_to(x, y);
                let position = LogicalPosition::new(x, y);
                let button = PointerEventButton::Left;
                self.send(WindowEvent::PointerPressed { position, button });
                self.send(WindowEvent::PointerReleased { position, button });
            }

            /// One wheel notch "up" (which a plain SpinBox turns into +step).
            fn wheel(&self, x: f32, y: f32) {
                self.move_to(x, y);
                self.send(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(x, y),
                    delta_x: 0.0,
                    delta_y: 40.0,
                });
            }

            fn key(&self, text: slint::SharedString) {
                self.send(WindowEvent::KeyPressed { text: text.clone() });
                self.send(WindowEvent::KeyReleased { text });
            }
        }

        #[test]
        fn the_wheel_scrolls_the_page_unless_the_box_was_just_clicked() {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
            slint::platform::set_platform(Box::new(HeadlessPlatform(window.clone())))
                .expect("a fresh test thread has no Slint platform yet");
            let ui = Harness::new().unwrap();
            ui.show().unwrap();
            window.set_size(PhysicalSize::new(300, 120));
            let d = Driver { ui };
            let ui = &d.ui;

            // Box at x 10..142, y 10..~40: text, then the up and down arrows.
            let (text_x, up_x, down_x, row_y) = (40.0, 94.0, 126.0, 25.0);
            let outside = (250.0, 20.0);

            // Never touched: the wheel over it scrolls the page.
            d.wheel(text_x, row_y);
            assert_eq!(ui.get_value(), 100);
            assert_eq!(ui.get_edits(), 0);
            assert_eq!(ui.get_page_scrolled(), 40.0);
            assert!(!ui.get_spin_focused());

            // A click arms it: now the wheel changes the value, as a SpinBox does.
            d.click(text_x, row_y);
            assert!(ui.get_spin_focused() && ui.get_spin_armed());
            d.wheel(text_x, row_y);
            assert_eq!(ui.get_value(), 105);
            assert_eq!(ui.get_edits(), 1);
            assert_eq!(ui.get_page_scrolled(), 40.0);

            // The pointer leaves: disarmed, though it keeps focus (typing still works).
            d.move_to(outside.0, outside.1);
            assert!(!ui.get_spin_armed());
            assert!(ui.get_spin_focused());
            d.wheel(text_x, row_y);
            assert_eq!(ui.get_value(), 105, "scrolling the page past it");
            assert_eq!(ui.get_page_scrolled(), 80.0);
            d.key("0".into());
            assert_eq!(ui.get_value(), 1050);

            // Enter (accepting a typed value) disarms it as well.
            d.click(text_x, row_y);
            assert!(ui.get_spin_armed());
            d.key(Key::Return.into());
            assert!(!ui.get_spin_armed());
            d.wheel(text_x, row_y);
            assert_eq!(ui.get_value(), 1050);
            assert_eq!(ui.get_page_scrolled(), 120.0);

            // So does Escape.
            d.click(text_x, row_y);
            assert!(ui.get_spin_armed());
            d.key(Key::Escape.into());
            assert!(!ui.get_spin_armed());

            // Losing focus (Tab, the pointer still over it) disarms it.
            d.click(text_x, row_y);
            assert!(ui.get_spin_armed());
            d.key(Key::Tab.into());
            assert!(!ui.get_spin_focused());
            assert!(!ui.get_spin_armed());
            d.wheel(text_x, row_y);
            assert_eq!(ui.get_value(), 1050);
            assert_eq!(ui.get_page_scrolled(), 160.0);

            // The first click on an arrow both arms the box and steps the value; the
            // arrows then work as usual.
            let edits = ui.get_edits();
            d.click(up_x, row_y);
            assert!(ui.get_spin_focused() && ui.get_spin_armed());
            assert_eq!(ui.get_value(), 1055);
            d.click(down_x, row_y);
            d.click(down_x, row_y);
            assert_eq!(ui.get_value(), 1045);
            assert_eq!(ui.get_edits(), edits + 3);

            // And the first click on the down arrow steps down.
            d.click(240.0, 80.0);
            d.click(down_x, row_y);
            assert_eq!(ui.get_value(), 1040);
            assert_eq!(ui.get_page_scrolled(), 160.0);
        }
    }

    /// The main page's Source list asks for a device refresh each time it opens
    /// (`opened` -> `App.refresh-sources()`): driven with real pointer and key events
    /// in a headless window (software renderer, no screen).
    mod open_aware_combo_box {
        use std::rc::Rc;

        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
        use slint::{ComponentHandle, LogicalPosition, PhysicalSize, PlatformError};

        #[allow(clippy::all, clippy::pedantic, missing_docs, unused_qualifications)]
        mod harness {
            slint::slint! {
                #[style = "fluent"]
                import { OpenAwareComboBox } from "../ui/open-aware-combo-box.slint";

                export component Harness inherits Window {
                    width: 300px;
                    height: 400px;
                    in-out property <[string]> sources: ["Speakers", "Headphones"];
                    in-out property <int> index;
                    out property <int> opened;
                    out property <int> picked: -1;
                    out property <bool> focused: combo.has-focus;
                    out property <bool> expanded: combo.accessible-expanded;
                    combo := OpenAwareComboBox {
                        x: 10px;
                        y: 10px;
                        width: 200px;
                        model: root.sources;
                        current-index <=> root.index;
                        opened => {
                            root.opened += 1;
                        }
                        selected => {
                            root.picked = self.current-index;
                        }
                    }
                    // Something else that takes focus when clicked.
                    FocusScope {
                        x: 10px;
                        y: 300px;
                        width: 80px;
                        height: 40px;
                    }
                }
            }
        }
        use harness::Harness;

        struct HeadlessPlatform(Rc<MinimalSoftwareWindow>);

        impl Platform for HeadlessPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
                Ok(self.0.clone())
            }
        }

        struct Driver {
            ui: Harness,
        }

        impl Driver {
            fn send(&self, event: WindowEvent) {
                self.ui.window().dispatch_event(event);
                // Runs the `changed` handlers, as each event-loop iteration does.
                slint::platform::update_timers_and_animations();
            }

            fn click(&self, x: f32, y: f32) {
                let position = LogicalPosition::new(x, y);
                self.send(WindowEvent::PointerMoved { position });
                let button = PointerEventButton::Left;
                self.send(WindowEvent::PointerPressed { position, button });
                self.send(WindowEvent::PointerReleased { position, button });
            }

            fn key(&self, text: slint::SharedString) {
                self.send(WindowEvent::KeyPressed { text: text.clone() });
                self.send(WindowEvent::KeyReleased { text });
            }
        }

        #[test]
        fn opened_fires_on_every_opening_whatever_closed_the_list() {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
            slint::platform::set_platform(Box::new(HeadlessPlatform(window.clone())))
                .expect("a fresh test thread has no Slint platform yet");
            let ui = Harness::new().unwrap();
            ui.show().unwrap();
            window.set_size(PhysicalSize::new(300, 400));
            let d = Driver { ui };
            let ui = &d.ui;

            // The closed box spans x 10..210, y 10..42; the open list covers it, one
            // 40px row per entry starting at y 10.
            let combo = (100.0, 26.0);
            let row = |i: f32| (100.0, 30.0 + 40.0 * i);
            let outside = (250.0, 380.0);
            let other_focusable = (50.0, 320.0);

            // Focus alone (Tab) is not an opening.
            d.key(Key::Tab.into());
            assert!(ui.get_focused());
            assert_eq!(ui.get_opened(), 0);

            // First opening, closed with Escape: the box keeps focus.
            d.click(combo.0, combo.1);
            assert!(ui.get_expanded());
            assert_eq!(ui.get_opened(), 1);
            d.key(Key::Escape.into());
            assert!(!ui.get_expanded());
            assert!(ui.get_focused());

            // Reopened (still focused, which a has-focus rule misses), then closed by a
            // click outside on something that takes no focus.
            d.click(combo.0, combo.1);
            assert!(ui.get_expanded());
            assert_eq!(ui.get_opened(), 2);
            d.click(outside.0, outside.1);
            assert!(!ui.get_expanded());
            assert!(ui.get_focused());

            // Reopened; the refreshed device list arrives while it is open (as the
            // engine's answer to `refresh-sources` does) and its new row can be picked.
            d.click(combo.0, combo.1);
            assert_eq!(ui.get_opened(), 3);
            ui.set_sources(slint::ModelRc::new(slint::VecModel::from(vec![
                slint::SharedString::from("Speakers"),
                slint::SharedString::from("Headphones"),
                slint::SharedString::from("USB headset"),
            ])));
            slint::platform::update_timers_and_animations();
            assert!(ui.get_expanded(), "a new list does not close the popup");
            d.click(row(2.0).0, row(2.0).1);
            assert!(!ui.get_expanded());
            assert_eq!(ui.get_picked(), 2);
            assert_eq!(ui.get_index(), 2);
            assert!(ui.get_focused());

            // Reopened after picking a row.
            d.click(combo.0, combo.1);
            assert_eq!(ui.get_opened(), 4);
            d.key(Key::Return.into());
            assert!(!ui.get_expanded());

            // Opened from the keyboard.
            d.key(Key::Return.into());
            assert!(ui.get_expanded());
            assert_eq!(ui.get_opened(), 5);
            d.key(Key::Escape.into());

            // Focus moving away and back (another control, the window losing and
            // regaining activation) is not an opening either.
            d.click(other_focusable.0, other_focusable.1);
            assert!(!ui.get_focused());
            d.send(WindowEvent::WindowActiveChanged(false));
            d.send(WindowEvent::WindowActiveChanged(true));
            d.key(Key::Tab.into());
            assert_eq!(ui.get_opened(), 5);
            d.click(combo.0, combo.1);
            assert_eq!(ui.get_opened(), 6);
        }
    }
}
