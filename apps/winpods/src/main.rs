//! WinPods.exe entry point (owned by the lead; the UI lives in `ui.rs`).

#![windows_subsystem = "windows"]

mod demo;
mod icons;
mod tray;
mod ui;
mod view;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--demo [--theme light|dark] [--page settings] [--update]`: mock engine, no I/O.
    if args.iter().any(|arg| arg == "--demo") {
        return demo::run_demo(&args);
    }

    // Integration step wires logging, single instance, config, engine and `ui::run`.
    let window = ui::MainWindow::new()?;
    slint::ComponentHandle::run(&window)?;
    Ok(())
}
