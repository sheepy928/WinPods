//! Application services for WinPods.exe and winpods-cli.exe (workstream: app).
//! CONTRACT: the public items are used by both binaries; keep signatures.

pub mod autostart;
pub mod config;
pub mod logging;
pub mod paths;
pub mod single_instance;
pub mod update_key;
pub mod updater;

pub use config::Config;
