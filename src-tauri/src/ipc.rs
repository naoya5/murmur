//! SOT: ipc
//!
//! What: every Tauri command and event, registered in one specta builder. The builder also
//! generates `src/ipc/bindings.ts`, the only TypeScript view of these types.
//! Why: the Rust types are the truth (guardrail G4). The UI never writes an IPC type by hand,
//! and CI regenerates the file and fails on any diff.
//! Where: add a command here and in `COMMANDS`, then in `build.rs` and the window capability
//! under `capabilities/`. Run `cargo test -p murmur export_bindings` to refresh the bindings.
//! Commands are thin: they call services and adapters; rules live in the core.

use std::sync::Arc;

use murmur_core::Language;
use murmur_core::catalog::{self, ModelEntry};
use murmur_core::metrics::{self, Dashboard, TypingSpeeds};
use murmur_core::session::Event;
use murmur_core::settings::Settings;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, State, Wry};
use tauri_specta::{Builder, Event as _, collect_commands, collect_events};

use crate::adapters::audio::{self, InputDevice};
use crate::adapters::models::{DownloadProgress, ModelStatus};
use crate::adapters::output;
use crate::adapters::store::{self, TranscriptRow};
use crate::app::App;
use crate::controller::{AudioLevel, HistoryChanged, PillState};
use crate::history::{self, RetryError};
use crate::platform::macos::{self, Permission, PrivacyPane};
use crate::settings_apply::{self, ApplyError, SettingsChanged};
use crate::updater::{self, UpdateAvailable, UpdateError, UpdateInfo};

/// Command names in the order of `collect_commands!`. `build.rs` and the capabilities must
/// list the same names; a test below checks `build.rs` and the capability files.
#[cfg(test)]
pub const COMMANDS: &[&str] = &[
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
];

/// Commands the pill window may call (its capability lists exactly these).
#[cfg(test)]
pub const PILL_COMMANDS: &[&str] = &["pill_clicked", "set_pill_width", "get_pill_state"];

type Services<'a> = State<'a, Arc<App>>;

/// Static facts about the running app.
#[derive(Debug, Clone, Serialize, Type)]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
    pub speech_engine: String,
}

/// The error type of plain commands. It crosses the IPC boundary as a tagged object.
#[derive(Debug, thiserror::Error, Serialize, Type)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum IpcError {
    #[error("storage error: {0}")]
    Store(String),
    #[error("model error: {0}")]
    Model(String),
    #[error("clipboard error: {0}")]
    Clipboard(String),
    #[error("hotkey error: {0}")]
    Hotkey(String),
    #[error("this link cannot be opened: {0}")]
    Url(String),
}

fn store_err(error: impl std::fmt::Display) -> IpcError {
    IpcError::Store(error.to_string())
}

/// Runs blocking store work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(store_err)?
}

fn remove_files(paths: &[String]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

#[tauri::command]
#[specta::specta]
pub fn app_info(services: Services<'_>) -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        data_dir: services.paths().data.to_string_lossy().into_owned(),
        speech_engine: crate::adapters::engine::ENGINE_NAME.to_owned(),
    }
}

#[tauri::command]
#[specta::specta]
pub fn get_settings(services: Services<'_>) -> Settings {
    services.settings()
}

#[tauri::command]
#[specta::specta]
pub async fn update_settings(
    app: AppHandle,
    services: Services<'_>,
    settings: Settings,
) -> Result<Settings, ApplyError> {
    // Off the main thread: the file write and hotkey registration may take a moment.
    let services = services.inner().clone();
    tauri::async_runtime::spawn_blocking(move || settings_apply::update(&app, &services, settings))
        .await
        .map_err(|e| ApplyError::Save(e.to_string()))?
}

#[tauri::command]
#[specta::specta]
pub async fn list_input_devices() -> Vec<InputDevice> {
    tauri::async_runtime::spawn_blocking(audio::input_devices)
        .await
        .unwrap_or_default()
}

#[tauri::command]
#[specta::specta]
pub async fn get_dashboard(services: Services<'_>) -> Result<Dashboard, IpcError> {
    let store = services.store().clone();
    let settings = services.settings();
    let speeds = TypingSpeeds {
        wpm_en: settings.typing_wpm_en,
        cpm_ja: settings.typing_cpm_ja,
    };
    blocking(move || {
        let totals = store.day_totals().map_err(store_err)?;
        let latencies = store.recent_latencies(100).map_err(store_err)?;
        let today = store::local_day(chrono::Utc::now().timestamp_millis());
        Ok(metrics::dashboard(&totals, &latencies, today, speeds))
    })
    .await
}

#[derive(Debug, Clone, Serialize, Type)]
pub struct HistoryPage {
    pub items: Vec<TranscriptRow>,
    pub total: u32,
}

#[tauri::command]
#[specta::specta]
pub async fn list_history(
    services: Services<'_>,
    query: String,
    limit: u32,
    offset: u32,
) -> Result<HistoryPage, IpcError> {
    let store = services.store().clone();
    blocking(move || {
        let items = store
            .list(&query, limit.min(500), offset)
            .map_err(store_err)?;
        let total = store.count(&query).map_err(store_err)?;
        Ok(HistoryPage { items, total })
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn delete_history_item(
    app: AppHandle,
    services: Services<'_>,
    id: String,
) -> Result<(), IpcError> {
    let store = services.store().clone();
    blocking(move || {
        let audio = store.delete(&id).map_err(store_err)?;
        remove_files(&audio.into_iter().collect::<Vec<_>>());
        Ok(())
    })
    .await?;
    let _ = HistoryChanged.emit(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn clear_history(app: AppHandle, services: Services<'_>) -> Result<(), IpcError> {
    let store = services.store().clone();
    blocking(move || {
        let audio = store.delete_all().map_err(store_err)?;
        remove_files(&audio);
        Ok(())
    })
    .await?;
    let _ = HistoryChanged.emit(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn retry_history_item(
    app: AppHandle,
    services: Services<'_>,
    id: String,
) -> Result<TranscriptRow, RetryError> {
    history::retry(app, services.inner().clone(), id).await
}

#[tauri::command]
#[specta::specta]
pub async fn copy_text(text: String) -> Result<(), IpcError> {
    tauri::async_runtime::spawn_blocking(move || output::copy_text(&text))
        .await
        .map_err(|e| IpcError::Clipboard(e.to_string()))?
        .map_err(|e| IpcError::Clipboard(e.to_string()))
}

/// A catalog entry with its state on this Mac.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ModelInfo {
    pub entry: ModelEntry,
    pub status: ModelStatus,
    /// Languages for which this model is the current choice.
    pub selected_for: Vec<Language>,
}

#[tauri::command]
#[specta::specta]
pub fn list_models(services: Services<'_>) -> Vec<ModelInfo> {
    let settings = services.settings();
    let statuses = services.models().statuses();
    catalog::CATALOG
        .iter()
        .filter_map(|entry| {
            let status = statuses.iter().find(|s| s.id == entry.id)?.clone();
            let selected_for = Language::ALL
                .into_iter()
                .filter(|l| settings.models.for_language(*l) == entry.id)
                .collect();
            Some(ModelInfo {
                entry: *entry,
                status,
                selected_for,
            })
        })
        .collect()
}

/// Download progress of one model, about 10 per second.
#[derive(Debug, Clone, Serialize, Type, tauri_specta::Event)]
pub struct ModelProgress {
    pub id: String,
    pub progress: DownloadProgress,
}

/// A download finished, failed or was cancelled, or a model was deleted. The UI refetches.
#[derive(Debug, Clone, Copy, Serialize, Type, tauri_specta::Event)]
pub struct ModelsChanged;

#[tauri::command]
#[specta::specta]
pub async fn download_model(
    app: AppHandle,
    services: Services<'_>,
    id: String,
) -> Result<(), IpcError> {
    let progress_app = app.clone();
    let progress_id = id.clone();
    let _ = ModelsChanged.emit(&app);
    let result = services
        .models()
        .download(&id, move |progress| {
            let _ = ModelProgress {
                id: progress_id.clone(),
                progress,
            }
            .emit(&progress_app);
        })
        .await;
    let _ = ModelsChanged.emit(&app);
    result.map_err(|e| IpcError::Model(e.to_string()))?;
    // A newly installed model for the current language is loaded at once.
    services.ensure_model_loaded(services.settings().language);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn cancel_download(services: Services<'_>, id: String) {
    services.models().cancel(&id);
}

#[tauri::command]
#[specta::specta]
pub fn delete_model(app: AppHandle, services: Services<'_>, id: String) -> Result<(), IpcError> {
    if services.inference().loaded_model() == Some(id.as_str()) {
        services.inference().unload();
    }
    let result = services
        .models()
        .delete(&id)
        .map_err(|e| IpcError::Model(e.to_string()));
    let _ = ModelsChanged.emit(&app);
    result
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct Permissions {
    pub microphone: Permission,
    pub accessibility: bool,
}

/// Uses the same permission functions as recording and pasting (G8).
#[tauri::command]
#[specta::specta]
pub fn get_permissions() -> Permissions {
    Permissions {
        microphone: macos::microphone_permission(),
        accessibility: macos::accessibility_trusted(),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn request_microphone() -> Permission {
    match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        macos::request_microphone(),
    )
    .await
    {
        Ok(permission) => permission,
        Err(_) => macos::microphone_permission(),
    }
}

#[tauri::command]
#[specta::specta]
pub fn request_accessibility() {
    macos::request_accessibility();
}

#[tauri::command]
#[specta::specta]
pub fn open_privacy_pane(pane: PrivacyPane) {
    macos::open_privacy_pane(pane);
}

/// While the hotkey recorder listens, the current hotkeys must not fire (PLAN 2.9).
#[tauri::command]
#[specta::specta]
pub fn suspend_hotkeys(services: Services<'_>) -> Result<(), IpcError> {
    match services.hotkeys() {
        Some(hotkeys) => hotkeys
            .suspend()
            .map_err(|e| IpcError::Hotkey(e.to_string())),
        None => Ok(()),
    }
}

#[tauri::command]
#[specta::specta]
pub fn resume_hotkeys(services: Services<'_>) -> Result<(), IpcError> {
    match services.hotkeys() {
        Some(hotkeys) => hotkeys
            .resume()
            .map_err(|e| IpcError::Hotkey(e.to_string())),
        None => Ok(()),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn reset_stats(app: AppHandle, services: Services<'_>) -> Result<(), IpcError> {
    let store = services.store().clone();
    blocking(move || store.reset_stats().map_err(store_err)).await?;
    let _ = HistoryChanged.emit(&app);
    Ok(())
}

/// Same as pressing the hotkey (toggle mode). Used by the onboarding test and the tray.
#[tauri::command]
#[specta::specta]
pub fn toggle_recording(services: Services<'_>) {
    if let Some(controller) = services.controller() {
        controller.event(Event::HotkeyDown {
            mode: murmur_core::settings::RecordMode::Toggle,
        });
    }
}

#[tauri::command]
#[specta::specta]
pub fn pill_clicked(services: Services<'_>) {
    if let Some(controller) = services.controller() {
        controller.event(Event::PillClicked);
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_pill_width(app: AppHandle, width: f64) {
    crate::windows::set_pill_width(&app, width.clamp(80.0, 480.0));
}

#[tauri::command]
#[specta::specta]
pub fn get_pill_state(services: Services<'_>) -> PillState {
    services.pill_state()
}

/// Hosts the About and Models screens may open in the browser (license and source pages).
const OPEN_URL_HOSTS: &[&str] = &[
    "github.com",
    "huggingface.co",
    "opensource.org",
    "creativecommons.org",
];

/// Opens a license or source page in the default browser. Only https URLs on the hosts above;
/// the app itself makes no request (G11).
#[tauri::command]
#[specta::specta]
pub fn open_url(url: String) -> Result<(), IpcError> {
    let host = url
        .strip_prefix("https://")
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or_default();
    let allowed = OPEN_URL_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")));
    if !allowed || url.chars().any(char::is_whitespace) {
        return Err(IpcError::Url(url));
    }
    std::process::Command::new("/usr/bin/open")
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|_| IpcError::Url(url))
}

/// Checks the update endpoint now (Settings > About). `None` = up to date.
#[tauri::command]
#[specta::specta]
pub async fn check_for_updates(app: AppHandle) -> Result<Option<UpdateInfo>, UpdateError> {
    updater::check(&app).await
}

/// Downloads, verifies (minisign) and installs the update, then restarts the app.
#[tauri::command]
#[specta::specta]
pub async fn install_update(app: AppHandle) -> Result<(), UpdateError> {
    updater::install(&app).await
}

/// Tray or second launch asks the main window to show a view.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct Navigate {
    pub view: String,
}

/// The one specta builder: all commands and events.
pub fn builder() -> Builder<Wry> {
    Builder::<Wry>::new()
        .commands(collect_commands![
            app_info,
            get_settings,
            update_settings,
            list_input_devices,
            get_dashboard,
            list_history,
            delete_history_item,
            clear_history,
            retry_history_item,
            copy_text,
            list_models,
            download_model,
            cancel_download,
            delete_model,
            get_permissions,
            request_microphone,
            request_accessibility,
            open_privacy_pane,
            suspend_hotkeys,
            resume_hotkeys,
            reset_stats,
            toggle_recording,
            pill_clicked,
            set_pill_width,
            get_pill_state,
            open_url,
            check_for_updates,
            install_update,
        ])
        .events(collect_events![
            PillState,
            AudioLevel,
            HistoryChanged,
            SettingsChanged,
            ModelProgress,
            ModelsChanged,
            Navigate,
            UpdateAvailable
        ])
}

/// Where the generated bindings live, relative to `src-tauri/`.
#[cfg(any(debug_assertions, test))]
const BINDINGS_PATH: &str = "../src/ipc/bindings.ts";

/// Write `src/ipc/bindings.ts`. Output is deterministic: no timestamps, stable ordering.
/// Debug builds and tests only; release builds never write into the source tree.
#[cfg(any(debug_assertions, test))]
pub fn export_bindings(builder: &Builder<Wry>) -> Result<(), Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(BINDINGS_PATH);
    // The generated file has helpers we do not use; keep strict unused checks off for it only.
    let typescript = specta_typescript::Typescript::default()
        .header("// @ts-nocheck")
        .bigint(specta_typescript::BigIntExportBehavior::Number);
    builder.export(typescript, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI runs this test and then diffs `src/ipc/bindings.ts`.
    #[test]
    fn export_bindings_writes_file() {
        export_bindings(&builder()).unwrap();
    }

    #[test]
    fn commands_are_in_build_manifest_and_capabilities() {
        let root = env!("CARGO_MANIFEST_DIR");
        let build = std::fs::read_to_string(format!("{root}/build.rs")).unwrap();
        let main_cap = std::fs::read_to_string(format!("{root}/capabilities/main.json")).unwrap();
        let pill_cap = std::fs::read_to_string(format!("{root}/capabilities/pill.json")).unwrap();
        for command in COMMANDS {
            assert!(
                build.contains(&format!("\"{command}\"")),
                "{command} missing in build.rs"
            );
            let permission = format!("allow-{}", command.replace('_', "-"));
            if PILL_COMMANDS.contains(command) {
                assert!(
                    pill_cap.contains(&permission),
                    "{permission} missing in pill.json"
                );
            } else {
                assert!(
                    main_cap.contains(&permission),
                    "{permission} missing in main.json"
                );
                assert!(
                    !pill_cap.contains(&permission),
                    "{permission} must not be in pill.json"
                );
            }
        }
    }
}
