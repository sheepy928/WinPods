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

use std::sync::Arc;

use winpods_app::Config;
use winpods_app::single_instance::InstanceGuard;
use winpods_app::updater::Updater;
use winpods_engine::EngineApi;

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
}

/// Run the UI until the user quits. Blocks the calling (main) thread.
pub fn run(ctx: UiContext) -> anyhow::Result<()> {
    let _ = ctx;
    todo!("ui: run")
}
