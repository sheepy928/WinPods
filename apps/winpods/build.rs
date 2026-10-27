//! Compiles the Slint UI and embeds the Windows icon/version resources
//! (workstream: UI owns the .slint files and icon assets).

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compiles");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        if std::path::Path::new("assets/winpods.ico").exists() {
            res.set_icon("assets/winpods.ico");
        }
        res.set("ProductName", "WinPods");
        res.set(
            "FileDescription",
            "WinPods - stream Windows audio to a HomePod",
        );
        res.set(
            "LegalCopyright",
            "Copyright (c) 2026 Ziyang Huang. MIT License.",
        );
        if let Err(err) = res.compile() {
            println!("cargo:warning=could not embed Windows resources: {err}");
        }
    }
}
