//! WinPods.exe entry point (owned by the lead; the UI lives in `ui.rs`).

#![windows_subsystem = "windows"]

#[allow(dead_code)] // wired up in the integration step
mod ui;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    // Integration step wires logging, single instance, config, engine and `ui::run`.
    let window = MainWindow::new()?;
    window.run()?;
    Ok(())
}
