fn main() {
    // Per-command permissions: a window can call a command only if its capability lists
    // `allow-<command>`. Keep this list in sync with `ipc::COMMANDS` (a test checks it).
    let attributes =
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "app_info",
            "get_settings",
            "update_settings",
            "list_input_devices",
            "get_dashboard",
            "list_history",
            "delete_history_item",
            "clear_history",
            "retry_history_item",
            "copy_text",
            "list_models",
            "download_model",
            "cancel_download",
            "delete_model",
            "get_permissions",
            "request_microphone",
            "request_accessibility",
            "open_privacy_pane",
            "suspend_hotkeys",
            "resume_hotkeys",
            "reset_stats",
            "toggle_recording",
            "pill_clicked",
            "set_pill_width",
            "get_pill_state",
            "open_url",
            "check_for_updates",
            "install_update",
            "submit_ink_stroke",
            "open_annotations_folder",
            "overlay_ready",
            "get_refiner_status",
        ]));
    if let Err(error) = tauri_build::try_build(attributes) {
        eprintln!("tauri build failed: {error:#}");
        std::process::exit(1);
    }
}
