//! Applies settings changes. Guardrail G3: `apply` matches `SettingKey` exhaustively with no `_`,
//! so a new setting does not compile until this file says how it takes effect.
//!
//! Most settings are read right before use (PLAN 2.9) and need no action here; their arm says
//! where they are read. Settings with side effects (hotkeys, model, autostart, retention) act now.

use std::sync::Arc;

use murmur_core::settings::{SettingKey, Settings, SettingsError};
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event as _;

use crate::adapters::settings_file;
use crate::app::App;

/// Settings were saved; every window re-reads what it shows.
#[derive(Debug, Clone, Serialize, Type, tauri_specta::Event)]
pub struct SettingsChanged {
    pub settings: Settings,
}

#[derive(Debug, thiserror::Error, Serialize, Type)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum ApplyError {
    #[error("{0}")]
    Invalid(String),
    #[error("hotkey: {0}")]
    Hotkey(String),
    #[error("launch at login: {0}")]
    Autostart(String),
    #[error("could not save settings: {0}")]
    Save(String),
}

impl From<SettingsError> for ApplyError {
    fn from(error: SettingsError) -> Self {
        ApplyError::Invalid(error.to_string())
    }
}

/// Validates, applies and saves new settings. A key whose side effect fails keeps its old value;
/// the other keys still apply. Returns the settings now in effect, or the first error.
pub fn update(
    app: &AppHandle,
    services: &Arc<App>,
    requested: Settings,
) -> Result<Settings, ApplyError> {
    requested.validate()?;
    let old = services.settings();
    let mut next = requested;
    next.schema_version = old.schema_version;
    let mut first_error = None;
    let keys = old.diff(&next);
    for &key in &keys {
        if let Err(error) = apply(key, &old, &mut next, app, services) {
            tracing::warn!(?key, %error, "setting not applied");
            first_error.get_or_insert(error);
        }
    }
    services.replace_settings(next.clone());
    if keys
        .iter()
        .any(|k| matches!(k, SettingKey::Language | SettingKey::Models))
    {
        services.ensure_model_loaded(next.language);
    }
    settings_file::save(&services.paths().settings, &next)
        .map_err(|e| ApplyError::Save(e.to_string()))?;
    if let Err(error) = (SettingsChanged {
        settings: next.clone(),
    })
    .emit(app)
    {
        tracing::warn!(%error, "settings-changed emit failed");
    }
    crate::refresh_tray(app, services);
    match first_error {
        Some(error) => Err(error),
        None => Ok(next),
    }
}

/// Side effects at startup, for keys whose state lives outside the settings file.
pub fn apply_at_start(app: &AppHandle, services: &Arc<App>) {
    let settings = services.settings();
    if let Some(hotkeys) = services.hotkeys() {
        if let Err(error) = hotkeys.set_main(&settings.hotkey) {
            tracing::error!(%error, "could not register the hotkey at start");
        }
        if let Err(error) = hotkeys.set_alternate(settings.alternate_language_hotkey.as_deref()) {
            tracing::warn!(%error, "could not register the alternate hotkey at start");
        }
    }
    sync_autostart(app, settings.launch_at_login);
    services.ensure_model_loaded(settings.language);
    let store = services.store().clone();
    let retention = settings.history_retention;
    tauri::async_runtime::spawn_blocking(move || {
        crate::history::apply_retention(&store, retention)
    });
}

#[deny(clippy::wildcard_enum_match_arm)]
fn apply(
    key: SettingKey,
    old: &Settings,
    next: &mut Settings,
    app: &AppHandle,
    services: &Arc<App>,
) -> Result<(), ApplyError> {
    match key {
        SettingKey::Hotkey => {
            let Some(hotkeys) = services.hotkeys() else {
                return Ok(());
            };
            hotkeys.set_main(&next.hotkey).map_err(|error| {
                // The adapter keeps the old key registered on failure; keep the settings in step.
                next.hotkey = old.hotkey.clone();
                ApplyError::Hotkey(error.to_string())
            })
        }
        SettingKey::AlternateLanguageHotkey => {
            let Some(hotkeys) = services.hotkeys() else {
                return Ok(());
            };
            hotkeys
                .set_alternate(next.alternate_language_hotkey.as_deref())
                .map_err(|error| {
                    next.alternate_language_hotkey = old.alternate_language_hotkey.clone();
                    ApplyError::Hotkey(error.to_string())
                })
        }
        SettingKey::Language | SettingKey::Models => {
            // The model is preloaded in `update` once the new settings are in place; the old model
            // serves until then. Language is read at the start of each recording.
            Ok(())
        }
        SettingKey::LaunchAtLogin => {
            if sync_autostart(app, next.launch_at_login) {
                Ok(())
            } else {
                next.launch_at_login = old.launch_at_login;
                Err(ApplyError::Autostart(
                    "the login item could not be changed".into(),
                ))
            }
        }
        SettingKey::HistoryRetention => {
            let store = services.store().clone();
            let retention = next.history_retention;
            tauri::async_runtime::spawn_blocking(move || {
                crate::history::apply_retention(&store, retention)
            });
            Ok(())
        }
        SettingKey::UnloadModelAfterMin => {
            // Read by the idle check in lib.rs every minute.
            Ok(())
        }
        // Read when the hotkey is pressed (controller translate / hotkeys callback).
        SettingKey::RecordMode => Ok(()),
        // Read when the next recording opens the microphone (controller::start_capture).
        SettingKey::Microphone => Ok(()),
        // Read at delivery (controller::deliver).
        SettingKey::AutoPaste | SettingKey::RestoreClipboard => Ok(()),
        // Read when the text is formatted (controller::on_inferred, history::retry).
        SettingKey::RemoveFillers | SettingKey::AddFinalPunctuation | SettingKey::Dictionary => {
            Ok(())
        }
        // Read when the dashboard is computed (ipc::get_dashboard); the UI refetches on SettingsChanged.
        SettingKey::TypingWpmEn | SettingKey::TypingCpmJa => Ok(()),
        // Read by the update scheduler before each check (updater.rs).
        SettingKey::AutoCheckUpdates => Ok(()),
        // The UI listens to SettingsChanged; the tray is rebuilt in `update`.
        SettingKey::UiLanguage => Ok(()),
        // Read when a recording starts or stops (controller).
        SettingKey::Sounds => Ok(()),
        // Read by the main window on start.
        SettingKey::OnboardingCompleted => Ok(()),
    }
}

/// Makes the login item match `enabled`. Returns false if it could not.
fn sync_autostart(app: &AppHandle, enabled: bool) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let current = manager.is_enabled().unwrap_or(!enabled);
    if current == enabled {
        return true;
    }
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    match result {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, enabled, "autostart change failed");
            false
        }
    }
}
