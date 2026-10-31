fn main() {
    // Per-command permissions: a window can call a command only if its capability lists
    // `allow-<command>`. Keep this list in sync with `ipc::COMMANDS` (a test checks it).
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(&["app_info"]));
    if let Err(error) = tauri_build::try_build(attributes) {
        eprintln!("tauri build failed: {error:#}");
        std::process::exit(1);
    }
}
