//! Applies settings changes. Guardrail G3: `apply` matches `SettingKey` exhaustively with no `_`,
//! so a new setting does not compile until this file says how it takes effect.
//!
//! Most settings are read right before use (PLAN 2.9) and need no action here; their arm says
//! where they are read. Settings with side effects (hotkeys, model, autostart, retention) act now.

use std::sync::Arc;

use murmur_core::refine::RefineKind;
use murmur_core::session::Event;
use murmur_core::settings::{SettingKey, Settings, SettingsError, same_accelerator};
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
    // Updates run one at a time; a second one (a quick double toggle in the UI) waits for the
    // first instead of failing. The section is short: validation, side effects, one file write.
    let _update = match services.settings_update.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let old = services.settings();
    if let Some(error) = hotkey_clash(&old, &requested) {
        return Err(error);
    }
    requested.validate()?;
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
    save_and_publish(
        &services.paths().settings,
        &next,
        || services.replace_settings(next.clone()),
        || {
            if let Some(hotkeys) = services.hotkeys() {
                if keys.contains(&SettingKey::Hotkey)
                    && let Err(error) = hotkeys.set_main(&old.hotkey)
                {
                    tracing::error!(%error, "could not restore the main hotkey");
                }
                if keys.contains(&SettingKey::AlternateLanguageHotkey)
                    && let Err(error) =
                        hotkeys.set_alternate(old.alternate_language_hotkey.as_deref())
                {
                    tracing::error!(%error, "could not restore the alternate hotkey");
                }
                if keys.iter().any(|k| {
                    matches!(
                        k,
                        SettingKey::AnnotationsEnabled | SettingKey::AnnotationsHotkey
                    )
                }) && let Err(error) = hotkeys.set_annotations(
                    old.annotations_enabled
                        .then_some(old.annotations_hotkey.as_str()),
                ) {
                    tracing::error!(%error, "could not restore the annotations hotkey");
                }
                if keys.iter().any(|k| {
                    matches!(
                        k,
                        SettingKey::TranslateEnabled | SettingKey::TranslateHotkey
                    )
                }) && let Err(error) = hotkeys.set_translate(
                    old.translate_enabled
                        .then_some(old.translate_hotkey.as_str()),
                ) {
                    tracing::error!(%error, "could not restore the translation hotkey");
                }
            }
            if keys.contains(&SettingKey::LaunchAtLogin) {
                sync_autostart(app, old.launch_at_login);
            }
            if keys.contains(&SettingKey::AnnotationsEnabled) {
                set_overlay_present(app, old.annotations_enabled);
            }
        },
    )?;
    if keys.contains(&SettingKey::AnnotationsRetentionDays) {
        spawn_bundle_cleanup(services, next.annotations_retention_days);
    }
    if keys.contains(&SettingKey::HistoryRetention) {
        let store = services.store().clone();
        let retention = next.history_retention;
        tauri::async_runtime::spawn_blocking(move || {
            crate::history::apply_retention(&store, retention)
        });
    }
    if keys
        .iter()
        .any(|k| matches!(k, SettingKey::Language | SettingKey::Models))
    {
        services.ensure_model_loaded(next.language);
    }
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

/// Publish only after the atomic file replacement; undo external changes on save failure.
fn save_and_publish(
    path: &std::path::Path,
    next: &Settings,
    publish: impl FnOnce(),
    rollback: impl FnOnce(),
) -> Result<(), ApplyError> {
    if let Err(error) = settings_file::save(path, next) {
        rollback();
        return Err(ApplyError::Save(error.to_string()));
    }
    publish();
    Ok(())
}

/// A clash between a feature hotkey (annotations, translation; each while its feature is on) and
/// another hotkey, worded for the field the user changed. `validate` reports the same clash under
/// the feature's own key, which fits only when that field changed.
fn hotkey_clash(old: &Settings, next: &Settings) -> Option<ApplyError> {
    let changed = old.diff(next);
    let changed_any = |keys: &[SettingKey]| keys.iter().any(|k| changed.contains(k));
    let features = [
        (
            next.annotations_enabled,
            next.annotations_hotkey.as_str(),
            "this key is the annotations hotkey",
        ),
        (
            next.translate_enabled,
            next.translate_hotkey.as_str(),
            "this key is the translation hotkey",
        ),
    ];
    for (on, feature_key, message) in features {
        if !on {
            continue;
        }
        let clashes = |other: &str| same_accelerator(feature_key, other);
        let hotkey = changed_any(&[SettingKey::Hotkey]) && clashes(&next.hotkey);
        let alternate = changed_any(&[SettingKey::AlternateLanguageHotkey])
            && next
                .alternate_language_hotkey
                .as_deref()
                .is_some_and(clashes);
        if hotkey || alternate {
            return Some(ApplyError::Hotkey(message.to_owned()));
        }
    }
    // The annotations hotkey moved onto the translation hotkey (`validate` words it the other
    // way round, for a change of the translation hotkey).
    if next.annotations_enabled
        && next.translate_enabled
        && changed_any(&[
            SettingKey::AnnotationsEnabled,
            SettingKey::AnnotationsHotkey,
        ])
        && !changed_any(&[SettingKey::TranslateEnabled, SettingKey::TranslateHotkey])
        && same_accelerator(&next.annotations_hotkey, &next.translate_hotkey)
    {
        return Some(ApplyError::Hotkey(
            "this key is the translation hotkey".to_owned(),
        ));
    }
    None
}

/// Creates or destroys the drawing layer to match the feature switch.
fn set_overlay_present(app: &AppHandle, present: bool) {
    if present {
        if let Err(error) = crate::windows::create_overlay(app) {
            tracing::error!(%error, "could not create the annotations drawing layer");
        }
    } else {
        crate::windows::destroy_overlay(app);
    }
}

/// Retention and size cap of the annotation bundles, on the blocking pool (G9).
fn spawn_bundle_cleanup(services: &Arc<App>, retention_days: u32) {
    let store = services.store().clone();
    let root = services.paths().annotations.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let now = chrono::Utc::now().timestamp_millis();
        crate::bundles::cleanup(&store, &root, retention_days, now);
    });
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
        if settings.annotations_enabled
            && let Err(error) = hotkeys.set_annotations(Some(&settings.annotations_hotkey))
        {
            tracing::warn!(%error, "could not register the annotations hotkey at start");
        }
        // No sidecar starts here: the first translation recording starts it (D3).
        if settings.translate_enabled
            && let Err(error) = hotkeys.set_translate(Some(&settings.translate_hotkey))
        {
            tracing::warn!(%error, "could not register the translation hotkey at start");
        }
    }
    // Off: no window, no hotkey, no permission check, no folder (spec rule for experiments).
    if settings.annotations_enabled {
        set_overlay_present(app, true);
    }
    // A no-op while the annotations folder does not exist.
    spawn_bundle_cleanup(services, settings.annotations_retention_days);
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
        // Destructive retention work starts only after the settings commit in `update`.
        SettingKey::HistoryRetention => Ok(()),
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
        // Annotations (experimental). On: the drawing layer and the hotkey. Off: both go, and a
        // recording in progress is dropped. Permissions and the folder are never touched here.
        SettingKey::AnnotationsEnabled => {
            if next.annotations_enabled {
                set_overlay_present(app, true);
                let Some(hotkeys) = services.hotkeys() else {
                    return Ok(());
                };
                hotkeys
                    .set_annotations(Some(&next.annotations_hotkey))
                    .map_err(|error| {
                        next.annotations_enabled = false;
                        set_overlay_present(app, false);
                        ApplyError::Hotkey(error.to_string())
                    })
            } else {
                if let Some(hotkeys) = services.hotkeys()
                    && let Err(error) = hotkeys.set_annotations(None)
                {
                    tracing::warn!(%error, "could not unregister the annotations hotkey");
                }
                if let Some(controller) = services.controller() {
                    controller.event(Event::AnnotationsDisabled);
                }
                set_overlay_present(app, false);
                Ok(())
            }
        }
        SettingKey::AnnotationsHotkey => {
            // Registered only while the feature is on; otherwise the value is stored for later.
            let Some(hotkeys) = services.hotkeys().filter(|_| next.annotations_enabled) else {
                return Ok(());
            };
            hotkeys
                .set_annotations(Some(&next.annotations_hotkey))
                .map_err(|error| {
                    next.annotations_hotkey = old.annotations_hotkey.clone();
                    ApplyError::Hotkey(error.to_string())
                })
        }
        // The cleanup runs after the settings commit in `update`.
        SettingKey::AnnotationsRetentionDays => Ok(()),
        // Translation (experimental). On: the hotkey only; the sidecars start with the first
        // translation recording (D3), and the UI guides to the Models screen while a model is
        // missing. Off: the hotkey goes, a recording in progress is dropped and one past its
        // recording is copied unrefined (`Event::RefineDisabled`), and both sidecars stop (the
        // cleanup model serves only the translation in stage 1).
        SettingKey::TranslateEnabled => {
            if next.translate_enabled {
                let Some(hotkeys) = services.hotkeys() else {
                    return Ok(());
                };
                hotkeys
                    .set_translate(Some(&next.translate_hotkey))
                    .map_err(|error| {
                        next.translate_enabled = false;
                        ApplyError::Hotkey(error.to_string())
                    })
            } else {
                if let Some(hotkeys) = services.hotkeys()
                    && let Err(error) = hotkeys.set_translate(None)
                {
                    tracing::warn!(%error, "could not unregister the translation hotkey");
                }
                if let Some(controller) = services.controller() {
                    controller.event(Event::RefineDisabled {
                        kind: RefineKind::Translate,
                    });
                }
                let refiner = services.refiner().clone();
                tauri::async_runtime::spawn_blocking(move || refiner.stop_all());
                Ok(())
            }
        }
        SettingKey::TranslateHotkey => {
            // Registered only while the feature is on; otherwise the value is stored for later.
            let Some(hotkeys) = services.hotkeys().filter(|_| next.translate_enabled) else {
                return Ok(());
            };
            hotkeys
                .set_translate(Some(&next.translate_hotkey))
                .map_err(|error| {
                    next.translate_hotkey = old.translate_hotkey.clone();
                    ApplyError::Hotkey(error.to_string())
                })
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn save_failure_restores_effects_without_publishing() {
        let dir = std::env::temp_dir().join(format!("murmur-apply-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let old = Settings::default();
        settings_file::save(&path, &old).unwrap();
        let next = Settings {
            hotkey: "F14".into(),
            launch_at_login: !old.launch_at_login,
            ..old.clone()
        };
        let memory = RefCell::new(old.clone());
        let effects = RefCell::new(next.clone());
        // Blocking the temporary file forces a real save failure while preserving the old file.
        std::fs::create_dir(dir.join("settings.json.tmp")).unwrap();
        let result = save_and_publish(
            &path,
            &next,
            || *memory.borrow_mut() = next.clone(),
            || *effects.borrow_mut() = old.clone(),
        );
        assert!(matches!(result, Err(ApplyError::Save(_))));
        assert_eq!(*memory.borrow(), old);
        assert_eq!(*effects.borrow(), old);
        assert_eq!(settings_file::load(&path).settings, old);
        std::fs::remove_dir(dir.join("settings.json.tmp")).unwrap();
        save_and_publish(
            &path,
            &next,
            || {
                assert_eq!(
                    settings_file::load(&path).settings,
                    next,
                    "persist before publishing"
                );
                *memory.borrow_mut() = next.clone();
            },
            || panic!("successful save must not roll back"),
        )
        .unwrap();
        assert_eq!(*memory.borrow(), next);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hotkey_clash_is_reported_for_the_field_the_user_changed() {
        let on = Settings {
            annotations_enabled: true,
            ..Settings::default()
        };
        // The dictation hotkey takes the annotations key.
        let next = Settings {
            hotkey: "Alt+Shift+S".into(),
            ..on.clone()
        };
        assert!(matches!(
            hotkey_clash(&on, &next),
            Some(ApplyError::Hotkey(_))
        ));
        // The alternate hotkey takes it.
        let next = Settings {
            alternate_language_hotkey: Some("option+shift+keys".into()),
            ..on.clone()
        };
        assert!(matches!(
            hotkey_clash(&on, &next),
            Some(ApplyError::Hotkey(_))
        ));
        // The annotations hotkey itself moved onto the dictation key: `validate` reports that.
        let next = Settings {
            annotations_hotkey: "Alt+Space".into(),
            ..on.clone()
        };
        assert!(hotkey_clash(&on, &next).is_none());
        assert!(next.validate().is_err());
        // No clash, or the feature off: nothing to report.
        let next = Settings {
            hotkey: "Alt+Shift+D".into(),
            ..on.clone()
        };
        assert!(hotkey_clash(&on, &next).is_none());
        let off = Settings::default();
        let next = Settings {
            hotkey: "Alt+Shift+S".into(),
            ..off.clone()
        };
        assert!(hotkey_clash(&off, &next).is_none());
    }

    #[test]
    fn translation_hotkey_clash_is_reported_for_the_field_the_user_changed() {
        let on = Settings {
            translate_enabled: true,
            ..Settings::default()
        };
        let translation = |e: Option<ApplyError>| matches!(e, Some(ApplyError::Hotkey(m)) if m == "this key is the translation hotkey");
        // The dictation or alternate hotkey takes the translation key.
        let next = Settings {
            hotkey: "Shift+Alt+E".into(),
            ..on.clone()
        };
        assert!(translation(hotkey_clash(&on, &next)));
        let next = Settings {
            alternate_language_hotkey: Some("alt+shift+keye".into()),
            ..on.clone()
        };
        assert!(translation(hotkey_clash(&on, &next)));
        // Annotations switched on with its key on the translation key.
        let next = Settings {
            annotations_enabled: true,
            annotations_hotkey: "Alt+Shift+KeyE".into(),
            ..on.clone()
        };
        assert!(translation(hotkey_clash(&on, &next)));
        // The translation hotkey itself moved onto the dictation key: `validate` reports that.
        let next = Settings {
            translate_hotkey: "Alt+Space".into(),
            ..on.clone()
        };
        assert!(hotkey_clash(&on, &next).is_none());
        assert!(next.validate().is_err());
        // Translation off: no clash.
        let off = Settings::default();
        let next = Settings {
            hotkey: "Alt+Shift+KeyE".into(),
            ..off.clone()
        };
        assert!(hotkey_clash(&off, &next).is_none());
    }
}
