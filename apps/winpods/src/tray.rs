//! Notification-area icon: tray-icon 0.26 + muda, created and driven on the UI thread.
//!
//! * Four state icons (grey/amber/green/red, see `icons.rs`) drawn at the system
//!   small-icon size, so they stay crisp at 125/150/200 % scaling.
//! * Left click toggles the window; the context menu offers Show, Connect/Disconnect,
//!   a Latency submenu of check items kept in sync with the setting, "Update to vX"
//!   (only while an update is available), About and Quit.
//! * Raw tray and menu events are forwarded with `slint::invoke_from_event_loop`, so
//!   they are handled after the shell's window procedure and the popup menu's modal
//!   loop have returned.
//! * Explorer restarts are handled by tray-icon (`TaskbarCreated`). On top of that, for
//!   the first 60 s after start and on every state change we probe `set_tooltip`; if
//!   it fails (the icon was never registered, e.g. Explorer was not ready at sign-in)
//!   the icon is recreated.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winpods_engine::State;

use crate::icons::{self, TrayIconKind};
use crate::view::{self, APP_NAME, LATENCY_PRESETS};

/// What the user asked for through the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click on the icon.
    ToggleWindow,
    ShowWindow,
    ToggleConnection,
    SetLatency(u32),
    InstallUpdate,
    ShowAbout,
    Quit,
}

/// Everything the tray shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayModel {
    pub state: State,
    pub tooltip: String,
    /// The user wants to be connected (menu shows "Disconnect").
    pub connected: bool,
    pub latency_ms: u32,
    /// Version offered by "Update to vX", when an update is available.
    pub update_version: Option<String>,
}

impl Default for TrayModel {
    fn default() -> Self {
        Self {
            state: State::Disconnected,
            tooltip: APP_NAME.to_string(),
            connected: false,
            latency_ms: 100,
            update_version: None,
        }
    }
}

/// A tray implementation (the real one is [`SystemTray`]).
pub trait Tray {
    /// Show `model`: icon colour, tooltip, menu labels and check marks. Cheap when
    /// nothing changed; also re-applies the latency check marks (the shell toggles a
    /// check item by itself when it is clicked).
    fn update(&mut self, model: &TrayModel);
    /// Periodic health check; call every second or so from the UI timer.
    fn maintain(&mut self, now: Instant);
}

const ID_SHOW: &str = "winpods.show";
const ID_CONNECT: &str = "winpods.connect";
const ID_UPDATE: &str = "winpods.update";
const ID_ABOUT: &str = "winpods.about";
const ID_QUIT: &str = "winpods.quit";
const LATENCY_PREFIX: &str = "winpods.latency.";

/// Menu position of "Update to vX" (after Show, separator, Connect, Latency, separator).
const UPDATE_POSITION: usize = 5;
/// How long after start-up `set_tooltip` is probed periodically.
const STARTUP_PROBE_WINDOW: Duration = Duration::from_secs(60);
/// Spacing of the start-up probes.
const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// Spacing of attempts to create the icon after a failure.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Menu item id of a latency preset.
pub fn latency_menu_id(ms: u32) -> String {
    format!("{LATENCY_PREFIX}{ms}")
}

/// Map a menu item id to the event it stands for.
pub fn event_for_menu_id(id: &str) -> Option<TrayEvent> {
    match id {
        ID_SHOW => Some(TrayEvent::ShowWindow),
        ID_CONNECT => Some(TrayEvent::ToggleConnection),
        ID_UPDATE => Some(TrayEvent::InstallUpdate),
        ID_ABOUT => Some(TrayEvent::ShowAbout),
        ID_QUIT => Some(TrayEvent::Quit),
        _ => id
            .strip_prefix(LATENCY_PREFIX)
            .and_then(|ms| ms.parse().ok())
            .map(TrayEvent::SetLatency),
    }
}

/// Map a raw icon event: only a completed left click toggles the window (the menu
/// opens on right click inside tray-icon).
pub fn event_for_icon(event: &TrayIconEvent) -> Option<TrayEvent> {
    match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => Some(TrayEvent::ToggleWindow),
        _ => None,
    }
}

/// Text of the update menu item ("Update to v0.3.0").
pub fn update_label(version: &str) -> String {
    format!("Update to v{}", version.trim_start_matches(['v', 'V']))
}

type Sink = Box<dyn Fn(TrayEvent) + Send + Sync + 'static>;

/// Where tray events go; set once per process (tray-icon/muda handlers are global).
static SINK: OnceLock<Sink> = OnceLock::new();

fn forward(event: TrayEvent) {
    if let Some(sink) = SINK.get() {
        // Deliver on the Slint event loop, outside the shell callback.
        if let Err(err) = slint::invoke_from_event_loop(move || sink(event)) {
            tracing::warn!("tray event {event:?} dropped: {err}");
        }
    }
}

/// Install the global tray-icon and muda handlers once. Later calls keep the first sink.
fn install_handlers(sink: Sink) {
    if SINK.set(sink).is_err() {
        return;
    }
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
        if let Some(event) = event_for_icon(&event) {
            forward(event);
        }
    }));
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        if let Some(event) = event_for_menu_id(event.id().as_ref()) {
            forward(event);
        }
    }));
}

/// Size the shell uses for notification-area icons on the primary monitor.
fn system_icon_size() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
    // SAFETY: GetSystemMetrics has no preconditions; it returns 0 on failure.
    let metric = unsafe { GetSystemMetrics(SM_CXSMICON) };
    icons::tray_icon_size(metric)
}

/// The real tray icon (Windows notification area).
pub struct SystemTray {
    icon: Option<TrayIcon>,
    menu: Menu,
    connect_item: MenuItem,
    latency_items: Vec<(u32, CheckMenuItem)>,
    update_item: MenuItem,
    update_shown: bool,
    icon_size: u32,
    icon_cache: [Option<Icon>; TrayIconKind::ALL.len()],
    model: TrayModel,
    started: Instant,
    last_probe: Instant,
    last_attempt: Instant,
}

impl SystemTray {
    /// Create the icon and its menu. Must run on the UI thread after the Slint
    /// backend is initialised (i.e. after the main window was created). `sink` is
    /// called on the UI thread for every tray event.
    pub fn new(
        sink: impl Fn(TrayEvent) + Send + Sync + 'static,
        model: &TrayModel,
    ) -> anyhow::Result<SystemTray> {
        install_handlers(Box::new(sink));

        let menu = Menu::new();
        let show_item = MenuItem::with_id(ID_SHOW, format!("Show {APP_NAME}"), true, None);
        let connect_item =
            MenuItem::with_id(ID_CONNECT, connect_label(model.connected), true, None);
        let latency_menu = Submenu::new("Latency", true);
        let mut latency_items = Vec::with_capacity(LATENCY_PRESETS.len());
        for ms in LATENCY_PRESETS {
            let item = CheckMenuItem::with_id(
                latency_menu_id(ms),
                view::tray_latency_label(ms),
                true,
                ms == model.latency_ms,
                None,
            );
            latency_menu.append(&item)?;
            latency_items.push((ms, item));
        }
        let update_item = MenuItem::with_id(ID_UPDATE, update_label("0"), true, None);
        let about_item = MenuItem::with_id(ID_ABOUT, format!("About {APP_NAME}"), true, None);
        let quit_item = MenuItem::with_id(ID_QUIT, format!("Quit {APP_NAME}"), true, None);
        menu.append_items(&[
            &show_item,
            &PredefinedMenuItem::separator(),
            &connect_item,
            &latency_menu,
            &PredefinedMenuItem::separator(),
            &about_item,
            &PredefinedMenuItem::separator(),
            &quit_item,
        ])?;

        let now = Instant::now();
        let mut tray = SystemTray {
            icon: None,
            menu,
            connect_item,
            latency_items,
            update_item,
            update_shown: false,
            icon_size: system_icon_size(),
            icon_cache: Default::default(),
            model: model.clone(),
            started: now,
            last_probe: now,
            last_attempt: now,
        };
        tray.sync_menu(model);
        tray.icon = Some(tray.build_icon()?);
        Ok(tray)
    }

    fn icon_for(&mut self, kind: TrayIconKind) -> Option<Icon> {
        let slot = &mut self.icon_cache[kind.index()];
        if slot.is_none() {
            let size = self.icon_size;
            match Icon::from_rgba(icons::render_tray_icon(kind, size), size, size) {
                Ok(icon) => *slot = Some(icon),
                Err(err) => tracing::warn!("tray icon {kind:?} ({size}px): {err}"),
            }
        }
        slot.clone()
    }

    fn build_icon(&mut self) -> anyhow::Result<TrayIcon> {
        self.last_attempt = Instant::now();
        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(self.menu.clone()))
            .with_menu_on_left_click(false)
            .with_tooltip(&self.model.tooltip);
        if let Some(icon) = self.icon_for(TrayIconKind::from(self.model.state)) {
            builder = builder.with_icon(icon);
        }
        Ok(builder.build()?)
    }

    /// Drop the current icon (removing it from the shell) and register a new one.
    fn recreate(&mut self, reason: &str) {
        tracing::info!("recreating the tray icon ({reason})");
        self.icon = None;
        match self.build_icon() {
            Ok(icon) => self.icon = Some(icon),
            Err(err) => tracing::warn!("could not create the tray icon: {err}"),
        }
    }

    /// Probe the shell with the current tooltip; recreate the icon when it fails.
    fn probe(&mut self, reason: &str) {
        let failed = match &self.icon {
            Some(icon) => icon.set_tooltip(Some(&self.model.tooltip)).is_err(),
            None => true,
        };
        if failed {
            self.recreate(reason);
        }
    }

    fn sync_menu(&mut self, model: &TrayModel) {
        if model.connected != self.model.connected {
            self.connect_item.set_text(connect_label(model.connected));
        }
        for (ms, item) in &self.latency_items {
            let checked = *ms == model.latency_ms;
            if item.is_checked() != checked {
                item.set_checked(checked);
            }
        }
        match (&model.update_version, self.update_shown) {
            (Some(version), shown) => {
                self.update_item.set_text(update_label(version));
                if !shown {
                    match self.menu.insert(&self.update_item, UPDATE_POSITION) {
                        Ok(()) => self.update_shown = true,
                        Err(err) => tracing::warn!("tray update item: {err}"),
                    }
                }
            }
            (None, true) => match self.menu.remove(&self.update_item) {
                Ok(()) => self.update_shown = false,
                Err(err) => tracing::warn!("tray update item: {err}"),
            },
            (None, false) => {}
        }
    }
}

fn connect_label(connected: bool) -> &'static str {
    if connected { "Disconnect" } else { "Connect" }
}

impl Tray for SystemTray {
    fn update(&mut self, model: &TrayModel) {
        self.sync_menu(model);
        if *model == self.model {
            return;
        }
        let state_changed = model.state != self.model.state;
        let tooltip_changed = model.tooltip != self.model.tooltip;
        let kind_changed = TrayIconKind::from(model.state) != TrayIconKind::from(self.model.state);
        self.model = model.clone();

        if self.icon.is_none() {
            self.recreate("no icon");
            return;
        }
        if kind_changed {
            let icon = self.icon_for(TrayIconKind::from(model.state));
            if let (Some(tray), Some(icon)) = (&self.icon, icon)
                && let Err(err) = tray.set_icon(Some(icon))
            {
                tracing::debug!("tray set_icon failed: {err}");
            }
        }
        if state_changed || tooltip_changed {
            self.probe("tooltip update failed");
        }
    }

    fn maintain(&mut self, now: Instant) {
        if self.icon.is_none() {
            if now.saturating_duration_since(self.last_attempt) >= RETRY_INTERVAL {
                self.recreate("retry");
            }
            return;
        }
        if now.saturating_duration_since(self.started) < STARTUP_PROBE_WINDOW
            && now.saturating_duration_since(self.last_probe) >= PROBE_INTERVAL
        {
            self.last_probe = now;
            self.probe("start-up probe failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_round_trip() {
        assert_eq!(event_for_menu_id(ID_SHOW), Some(TrayEvent::ShowWindow));
        assert_eq!(
            event_for_menu_id(ID_CONNECT),
            Some(TrayEvent::ToggleConnection)
        );
        assert_eq!(event_for_menu_id(ID_UPDATE), Some(TrayEvent::InstallUpdate));
        assert_eq!(event_for_menu_id(ID_ABOUT), Some(TrayEvent::ShowAbout));
        assert_eq!(event_for_menu_id(ID_QUIT), Some(TrayEvent::Quit));
        for ms in LATENCY_PRESETS {
            assert_eq!(
                event_for_menu_id(&latency_menu_id(ms)),
                Some(TrayEvent::SetLatency(ms))
            );
        }
        assert_eq!(event_for_menu_id("winpods.latency.abc"), None);
        assert_eq!(event_for_menu_id("something-else"), None);
    }

    #[test]
    fn only_left_click_release_toggles() {
        use tray_icon::dpi::{PhysicalPosition, PhysicalSize};
        use tray_icon::{Rect, TrayIconId};

        let rect = Rect {
            position: PhysicalPosition::new(0.0, 0.0),
            size: PhysicalSize::new(16, 16),
        };
        let click = |button, button_state| TrayIconEvent::Click {
            id: TrayIconId::new("t"),
            position: PhysicalPosition::new(1.0, 1.0),
            rect,
            button,
            button_state,
        };
        assert_eq!(
            event_for_icon(&click(MouseButton::Left, MouseButtonState::Up)),
            Some(TrayEvent::ToggleWindow)
        );
        assert_eq!(
            event_for_icon(&click(MouseButton::Left, MouseButtonState::Down)),
            None
        );
        assert_eq!(
            event_for_icon(&click(MouseButton::Right, MouseButtonState::Up)),
            None
        );
    }

    #[test]
    fn update_label_strips_v_prefix() {
        assert_eq!(update_label("v0.3.0"), "Update to v0.3.0");
        assert_eq!(update_label("0.3.0"), "Update to v0.3.0");
    }

    #[test]
    fn default_model_is_disconnected() {
        let model = TrayModel::default();
        assert_eq!(model.state, State::Disconnected);
        assert_eq!(model.tooltip, "WinPods");
        assert!(!model.connected);
        assert_eq!(model.update_version, None);
        assert_eq!(connect_label(true), "Disconnect");
        assert_eq!(connect_label(false), "Connect");
    }
}
