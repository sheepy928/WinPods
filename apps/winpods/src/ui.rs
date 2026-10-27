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
//! Structure: one [`Controller`] per process lives in a thread-local on the UI thread.
//! Slint callbacks, the UI timer and events posted from other threads ([`post`]) all
//! reach it through [`with_controller`], so no callback holds a strong reference and
//! no `RefCell` borrow is held across calls into Slint.
//!
//! The generated Slint types (`MainWindow`, the `App` global, ...) are re-exported from
//! here; `main.rs` must not call `slint::include_modules!()` itself.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slint::{
    CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
use winpods_app::Config;
use winpods_app::single_instance::InstanceGuard;
use winpods_app::updater::{Release, UpdateError, Updater};
use winpods_engine::{
    CaptureSelection, Command, DeviceInfo, EngineApi, ScanResult, ScanState, Snapshot, State,
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
    /// Primary-instance guard: the UI registers the activation handler and drops the
    /// guard before handing off to an updated executable.
    pub instance: Option<InstanceGuard>,
    /// `None` disables update checks (e.g. temp copy).
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
    let controller = Controller::new(window, ctx, options);
    CONTROLLER.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&controller)));
    controller.start(start_hidden);
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
/// How long a requested scan may take to show up as Running.
const SCAN_START_GRACE: Duration = Duration::from_millis(1500);
/// Default volume shown when the speaker volume is not controlled.
const DEFAULT_VOLUME: f32 = 40.0;
/// Project page.
const PROJECT_URL: &str = "https://github.com/sheepy928/WinPods";
/// Bundled third-party notices (replaced by cargo-about output in release builds).
const THIRD_PARTY_NOTICES: &str = include_str!("../assets/THIRD-PARTY-NOTICES.txt");

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
    },
    UpdateProgress {
        done: u64,
        total: u64,
    },
    /// `Err((message, offer_release_page))`.
    UpdateInstalled(Result<Release, (String, bool)>),
    ShutdownFinished {
        relaunch: bool,
    },
}

/// Hand an event to the UI thread (callable from any thread).
fn post(event: UiEvent) {
    if let Err(err) = slint::invoke_from_event_loop(move || with_controller(|c| c.handle(event))) {
        tracing::debug!("UI event dropped (event loop gone): {err}");
    }
}

/// Requests to the update-check thread.
enum UpdateRequest {
    CheckNow,
    Skip(Option<String>),
}

/// Update banner state.
#[derive(Default)]
struct UpdateUi {
    available: Option<Release>,
    busy: bool,
    status: String,
    can_install: bool,
}

/// Mutable UI-thread state.
struct UiState {
    config: Config,
    shown: bool,
    /// The user's intent: keep a connection up (drives the Connect switch).
    wants: bool,
    quitting: bool,
    last_snapshot: Option<Arc<Snapshot>>,
    last_state: State,
    devices: Option<Arc<Vec<DeviceInfo>>>,
    sources: Vec<CaptureSelection>,
    scan_results: Vec<ScanResult>,
    scan_requested: Option<Instant>,
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
    update_tx: RefCell<Option<mpsc::Sender<UpdateRequest>>>,
    auto_update: Arc<AtomicBool>,
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
                quitting: false,
                last_snapshot: None,
                last_state: snapshot.state,
                devices: None,
                sources: Vec::new(),
                scan_results: Vec::new(),
                scan_requested: None,
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
            update_tx: RefCell::new(None),
            auto_update,
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

    fn start(&self, start_hidden: bool) {
        // Engine-facing start-up: devices, first snapshot, auto-connect.
        let snapshot = self.engine.snapshot();
        if snapshot.devices.is_empty() {
            self.engine.send(Command::RefreshDevices);
        }
        self.apply_snapshot(&snapshot);
        let auto_connect = {
            let s = self.state.borrow();
            s.config.auto_connect
                && !view::effective_address(&snapshot, &s.config.device_ip).is_empty()
        };
        if auto_connect {
            self.set_wants(true);
            self.engine.send(Command::Connect);
        }

        if !self.demo {
            self.reconcile_autostart();
            self.start_update_checks();
            if let Some(guard) = self.instance.borrow().as_ref() {
                guard.on_activate(Box::new(|| post(UiEvent::Activate)));
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
        app.on_connect_toggled(|on| with_controller(|c| c.set_connection(on)));
        app.on_source_selected(|index| with_controller(|c| c.select_source(index)));
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
            UiEvent::UpdateChecked { result } => self.update_checked(result),
            UiEvent::UpdateProgress { done, total } => {
                let percent = done.saturating_mul(100).checked_div(total).unwrap_or(0);
                self.state.borrow_mut().update.status = format!("Downloading… {percent}%");
                self.push_update_ui();
            }
            UiEvent::UpdateInstalled(result) => self.update_installed(result),
            UiEvent::ShutdownFinished { relaunch } => self.finish_quit(relaunch),
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
                self.set_connection(!wants);
            }
            TrayEvent::SetLatency(ms) => {
                self.latency_timer.stop();
                self.set_latency(ms);
            }
            TrayEvent::InstallUpdate => {
                self.show_window();
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
        self.state.borrow_mut().shown = true;
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
        self.begin_shutdown(false);
    }

    fn begin_shutdown(&self, relaunch: bool) {
        {
            let mut s = self.state.borrow_mut();
            if s.quitting {
                return;
            }
            s.quitting = true;
        }
        self.tick_timer.stop();
        self.latency_timer.stop();
        self.volume_timer.stop();
        let _ = self.window.hide();
        self.tray.borrow_mut().take();
        self.update_tx.borrow_mut().take();

        let shutdown = self.shutdown.borrow_mut().take();
        let spawned = std::thread::Builder::new()
            .name("engine-shutdown".into())
            .spawn(move || {
                if let Some(shutdown) = shutdown {
                    shutdown();
                }
                post(UiEvent::ShutdownFinished { relaunch });
            });
        if let Err(err) = spawned {
            // The closure (and the hook) went down with the failed spawn.
            tracing::error!("could not start the shutdown thread: {err}");
            self.finish_quit(relaunch);
        }
    }

    fn finish_quit(&self, relaunch: bool) {
        if relaunch {
            // Release the single-instance mutex before the new process looks for it.
            self.instance.borrow_mut().take();
            match winpods_app::paths::exe_path() {
                Ok(exe) => {
                    let spawned = std::process::Command::new(&exe)
                        .arg("--post-update")
                        .arg(std::process::id().to_string())
                        .spawn();
                    if let Err(err) = spawned {
                        tracing::error!("could not start {}: {err}", exe.display());
                    }
                }
                Err(err) => tracing::error!("no executable path for the restart: {err}"),
            }
        }
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
        let (title, detail, has_address, devices_changed, wants_changed) = {
            let mut s = self.state.borrow_mut();
            s.last_snapshot = Some(Arc::clone(snap));
            let has_address = !view::effective_address(snap, &s.config.device_ip).is_empty();
            let mut wants_changed = false;
            if snap.state != s.last_state {
                s.last_state = snap.state;
                // The engine gives up when there is no address to connect to.
                if snap.state == State::Error && !has_address && s.wants {
                    s.wants = false;
                    wants_changed = true;
                }
            }
            let devices_changed = s
                .devices
                .as_ref()
                .is_none_or(|known| !Arc::ptr_eq(known, &snap.devices));
            if devices_changed {
                s.devices = Some(Arc::clone(&snap.devices));
            }
            let mut save = false;
            if let Some(name) = &snap.discovered_device_name
                && !name.is_empty()
                && *name != s.config.device_name
            {
                s.config.device_name = name.clone();
                save = true;
            }
            let title = view::speaker_title(snap, &s.config.device_name, &s.config.device_ip);
            let detail = view::speaker_detail(snap, &s.config.device_ip);
            drop(s);
            if save {
                self.save_config();
            }
            (title, detail, has_address, devices_changed, wants_changed)
        };

        app.set_speaker_name(shared(title));
        app.set_speaker_detail(shared(detail));
        app.set_status_text(shared(view::status_text(snap, has_address)));
        app.set_status_kind(ui_status_kind(view::StatusKind::from(snap.state)));
        let error = match snap.state {
            State::Error | State::Reconnecting => snap.error.trim(),
            _ => "",
        };
        app.set_error_text(shared(error));
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
        let snapshot = s
            .last_snapshot
            .clone()
            .unwrap_or_else(|| self.engine.snapshot());
        let speaker = view::speaker_title(&snapshot, &s.config.device_name, &s.config.device_ip);
        TrayModel {
            state: snapshot.state,
            tooltip: view::tray_tooltip(&snapshot, &speaker),
            connected: s.wants,
            latency_ms: s.config.latency_clamped(),
            update_version: s.update.available.as_ref().map(|r| r.version.to_string()),
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

    fn set_connection(&self, on: bool) {
        self.set_wants(on);
        self.engine.send(if on {
            Command::Connect
        } else {
            Command::Disconnect
        });
        self.refresh_tray();
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
            (address_changed, name_changed)
        };
        if address_changed || name_changed {
            self.save_config();
        }
        if address_changed {
            self.engine.send(Command::SetDeviceIp(address));
        }
        self.refresh_tray();
    }

    fn find_speakers(&self) {
        self.engine.send(Command::Scan);
        self.state.borrow_mut().scan_requested = Some(Instant::now());
        let app = self.app();
        app.set_scan_running(true);
        app.set_scan_message(shared(view::scan_view(&ScanState::Running).message));
        app.set_scan_results(ModelRc::new(VecModel::<ScanRow>::default()));
    }

    fn apply_scan(&self, scan: &ScanState) {
        let results = {
            let mut s = self.state.borrow_mut();
            if let Some(requested) = s.scan_requested {
                let started = matches!(scan, ScanState::Running);
                if started || requested.elapsed() > SCAN_START_GRACE {
                    s.scan_requested = None;
                } else {
                    // Still showing the previous scan's result; keep "searching".
                    return;
                }
            }
            let view = view::scan_view(scan);
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
        app.set_update_busy(update.busy);
        app.set_update_can_install(update.can_install);
    }

    fn start_update_checks(&self) {
        let Some(updater) = self.updater.clone() else {
            return;
        };
        let (skipped, first_delay) = {
            let s = self.state.borrow();
            (
                s.config.skipped_version.clone(),
                view::update_first_delay(unix_now(), s.config.last_update_check),
            )
        };
        let (tx, rx) = mpsc::channel();
        let auto = Arc::clone(&self.auto_update);
        let spawned = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || update_check_loop(&updater, &auto, skipped, first_delay, &rx));
        match spawned {
            Ok(_) => *self.update_tx.borrow_mut() = Some(tx),
            Err(err) => tracing::warn!("could not start update checks: {err}"),
        }
    }

    fn set_auto_update(&self, on: bool) {
        self.auto_update.store(on, Ordering::Relaxed);
        self.edit_config(|config| config.auto_update = on);
        let due =
            view::update_first_delay(unix_now(), self.state.borrow().config.last_update_check)
                <= view::UPDATE_STARTUP_DELAY;
        if on && due {
            self.check_updates();
        }
    }

    fn check_updates(&self) {
        {
            let mut s = self.state.borrow_mut();
            if s.update.busy {
                return;
            }
            s.update.busy = true;
            s.update.status = "Checking for updates…".to_string();
        }
        self.push_update_ui();
        if self.demo {
            Timer::single_shot(Duration::from_millis(1200), || {
                with_controller(|c| {
                    c.update_checked(Ok(Some(demo_release())));
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
            self.update_checked(Err("update checks are not available".to_string()));
        }
    }

    fn update_checked(&self, result: Result<Option<Release>, String>) {
        let mut save = false;
        {
            let mut s = self.state.borrow_mut();
            s.update.busy = false;
            match result {
                Ok(release) => {
                    let now = unix_now();
                    if !self.demo {
                        s.config.last_update_check = Some(now);
                        save = true;
                    }
                    s.update.status = match &release {
                        Some(r) => format!("Version {} is available", r.version),
                        None => format!(
                            "WinPods is up to date · checked {}",
                            view::format_ago(now, now)
                        ),
                    };
                    s.update.available = release;
                }
                Err(err) => s.update.status = format!("Could not check for updates: {err}"),
            }
        }
        if save {
            self.save_config();
        }
        self.push_update_ui();
        self.refresh_tray();
    }

    fn install_update(&self) {
        let (release, can_install, busy) = {
            let s = self.state.borrow();
            (
                s.update.available.clone(),
                s.update.can_install,
                s.update.busy,
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
        if !can_install {
            shell_open(&release.html_url);
            return;
        }
        let Some(updater) = self.updater.clone() else {
            return;
        };
        {
            let mut s = self.state.borrow_mut();
            s.update.busy = true;
            s.update.status = "Downloading…".to_string();
        }
        self.push_update_ui();
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
                let result = updater
                    .stage(&release, &progress)
                    .and_then(|staged| updater.install(&staged).map(|()| staged.release));
                post(UiEvent::UpdateInstalled(result.map_err(|err| {
                    let fallback =
                        matches!(err, UpdateError::Signature(_) | UpdateError::NotWritable(_));
                    (err.to_string(), fallback)
                })));
            });
        if let Err(err) = spawned {
            self.update_installed(Err((format!("could not start the download: {err}"), false)));
        }
    }

    fn update_installed(&self, result: Result<Release, (String, bool)>) {
        match result {
            Ok(release) => {
                self.state.borrow_mut().update.status =
                    format!("Restarting into WinPods {}…", release.version);
                self.push_update_ui();
                // Stop the engine, hide the tray, release the mutex, start the new exe.
                self.begin_shutdown(true);
            }
            Err((message, offer_release_page)) => {
                {
                    let mut s = self.state.borrow_mut();
                    s.update.busy = false;
                    s.update.status = format!("Update failed: {message}");
                    if offer_release_page {
                        s.update.can_install = false;
                    }
                }
                self.push_update_ui();
            }
        }
    }

    fn skip_update(&self) {
        let tag = {
            let mut s = self.state.borrow_mut();
            let Some(release) = s.update.available.take() else {
                return;
            };
            s.config.skipped_version = Some(release.tag.clone());
            s.update.status = format!("Skipped {}", release.tag);
            release.tag
        };
        self.save_config();
        if let Some(tx) = self.update_tx.borrow().as_ref() {
            let _ = tx.send(UpdateRequest::Skip(Some(tag)));
        }
        self.push_update_ui();
        self.refresh_tray();
    }
}

/// Background update schedule: first check after `first_delay`, then every 24 h,
/// backing off 1/5/30 min after failures. "Check now" requests run regardless of the
/// automatic-updates switch. Ends when the UI drops the sender.
fn update_check_loop(
    updater: &Updater,
    auto: &AtomicBool,
    mut skipped: Option<String>,
    first_delay: Duration,
    requests: &mpsc::Receiver<UpdateRequest>,
) {
    let mut next = Instant::now() + first_delay;
    let mut failures = 0usize;
    loop {
        let manual = match requests.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(UpdateRequest::CheckNow) => true,
            Ok(UpdateRequest::Skip(version)) => {
                skipped = version;
                continue;
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if !manual && !auto.load(Ordering::Relaxed) {
            next = Instant::now() + view::UPDATE_PERIOD;
            continue;
        }
        let result = updater.check(skipped.as_deref());
        next = Instant::now()
            + match &result {
                Ok(_) => {
                    failures = 0;
                    view::UPDATE_PERIOD
                }
                Err(err) => {
                    tracing::info!("update check failed: {err}");
                    let delay = view::update_retry_delay(failures);
                    failures += 1;
                    delay
                }
            };
        post(UiEvent::UpdateChecked {
            result: result.map_err(|err| err.to_string()),
        });
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
}
