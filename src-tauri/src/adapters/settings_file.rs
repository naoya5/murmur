//! Adapter: settings file (`settings.json`). Load with migration and recovery, save atomically.
//!
//! - Missing file: defaults.
//! - Corrupt file: moved to `settings.json.bak`, defaults, `recovered = true`.
//! - File from a newer app: left untouched, defaults, `recovered = true`. The caller must not call `save`
//!   until the user changes a setting, so the newer file is not overwritten by accident.
//! - Invalid single values: only that field returns to its default.
#![cfg_attr(not(test), allow(dead_code))] // Wired by T3-3 (settings apply).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use murmur_core::settings::{SettingKey, Settings, SettingsError};

pub struct Loaded {
    pub settings: Settings,
    /// The existing file could not be used (corrupt or from a newer version).
    pub recovered: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsFileError {
    #[error("settings file error: {0}")]
    Io(String),
    #[error("settings could not be serialized: {0}")]
    Serialize(String),
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Never fails: every problem ends in usable settings.
pub fn load(path: &Path) -> Loaded {
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                settings: Settings::default(),
                recovered: false,
            };
        }
        Err(e) => {
            tracing::warn!("settings file unreadable ({e}); using defaults");
            // Invalid UTF-8 means the file is corrupt; other errors (permissions) leave it alone.
            if e.kind() == std::io::ErrorKind::InvalidData {
                move_aside(path);
            }
            return Loaded {
                settings: Settings::default(),
                recovered: true,
            };
        }
    };
    match Settings::migrate(&json) {
        Ok(settings) => Loaded {
            settings: reset_invalid_fields(settings),
            recovered: false,
        },
        Err(SettingsError::FutureVersion(version)) => {
            tracing::warn!(
                "settings file is from a newer version ({version}); using defaults, file kept"
            );
            Loaded {
                settings: Settings::default(),
                recovered: true,
            }
        }
        Err(e) => {
            tracing::warn!("settings file is corrupt ({e}); moved to .bak, using defaults");
            move_aside(path);
            Loaded {
                settings: Settings::default(),
                recovered: true,
            }
        }
    }
}

fn move_aside(path: &Path) {
    if let Err(e) = fs::rename(path, sibling(path, ".bak")) {
        tracing::warn!("could not move the corrupt settings file aside: {e}");
    }
}

/// Resets each field that fails validation, one at a time, until the settings are valid.
fn reset_invalid_fields(mut settings: Settings) -> Settings {
    // Bounded: every pass fixes one key, and there are only a few keys that `validate` can report.
    for _ in 0..32 {
        let Err(error) = settings.validate() else {
            return settings;
        };
        let SettingsError::Invalid { key, reason } = error else {
            break;
        };
        tracing::warn!("invalid setting {key:?} ({reason}); reset to its default");
        reset_key(&mut settings, key);
    }
    Settings::default()
}

fn reset_key(settings: &mut Settings, key: SettingKey) {
    let defaults = Settings::default();
    match key {
        SettingKey::Hotkey => settings.hotkey = defaults.hotkey,
        SettingKey::TypingWpmEn => settings.typing_wpm_en = defaults.typing_wpm_en,
        SettingKey::TypingCpmJa => settings.typing_cpm_ja = defaults.typing_cpm_ja,
        SettingKey::HistoryRetention => settings.history_retention = defaults.history_retention,
        SettingKey::UnloadModelAfterMin => {
            settings.unload_model_after_min = defaults.unload_model_after_min
        }
        SettingKey::Models => settings.models = defaults.models,
        // Keep the good entries, drop only the broken ones.
        SettingKey::Dictionary => settings.dictionary.retain(|e| !e.written.trim().is_empty()),
        // `validate` reports no other key today; if it starts to, fall back to all defaults.
        _ => *settings = defaults,
    }
}

/// Writes a temporary file in the same directory, fsyncs it, then renames it over `path`.
pub fn save(path: &Path, settings: &Settings) -> Result<(), SettingsFileError> {
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| SettingsFileError::Serialize(e.to_string()))?;
    let io = |e: std::io::Error| SettingsFileError::Io(e.to_string());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = sibling(path, ".tmp");
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io(e));
    }
    // Make the rename durable. Best effort: not every file system allows fsync on a directory.
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_core::settings::HistoryRetention;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "murmur-settings-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        let loaded = load(&dir.join("settings.json"));
        assert_eq!(loaded.settings, Settings::default());
        assert!(!loaded.recovered);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("settings.json");
        let settings = Settings {
            hotkey: "Ctrl+Shift+K".into(),
            history_retention: HistoryRetention::Days { days: 30 },
            typing_wpm_en: 77,
            ..Settings::default()
        };
        save(&path, &settings).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings, settings);
        assert!(!loaded.recovered);
        assert!(!sibling(&path, ".tmp").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = temp_dir("corrupt");
        let path = dir.join("settings.json");
        fs::write(&path, "{ this is not json").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.recovered);
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(sibling(&path, ".bak")).unwrap(),
            "{ this is not json"
        );
        // A second load starts clean.
        assert!(!load(&path).recovered);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn future_version_is_kept_untouched() {
        let dir = temp_dir("future");
        let path = dir.join("settings.json");
        let content = r#"{"schema_version": 999, "hotkey": "F13", "brand_new": true}"#;
        fs::write(&path, content).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.recovered);
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(!sibling(&path, ".bak").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_values_reset_only_that_field() {
        let dir = temp_dir("invalid");
        let path = dir.join("settings.json");
        let settings = Settings {
            hotkey: "Ctrl+Shift+K".into(),
            typing_wpm_en: 1,                // out of range
            unload_model_after_min: Some(0), // invalid
            ..Settings::default()
        };
        fs::write(&path, serde_json::to_string(&settings).unwrap()).unwrap();
        let loaded = load(&path);
        assert!(!loaded.recovered);
        assert_eq!(
            loaded.settings.hotkey, "Ctrl+Shift+K",
            "valid fields are kept"
        );
        assert_eq!(
            loaded.settings.typing_wpm_en,
            Settings::default().typing_wpm_en
        );
        assert_eq!(loaded.settings.unload_model_after_min, None);
        assert!(loaded.settings.validate().is_ok());
        assert!(path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn older_file_without_version_loads() {
        let dir = temp_dir("old");
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"hotkey": "F13"}"#).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.settings.hotkey, "F13");
        assert!(!loaded.recovered);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_save_leaves_no_partial_file() {
        let dir = temp_dir("atomic");
        let path = dir.join("settings.json");
        let first = Settings {
            hotkey: "F13".into(),
            ..Settings::default()
        };
        save(&path, &first).unwrap();

        // A directory in the way makes the final rename fail after the temp file was fully written.
        let blocked = dir.join("blocked.json");
        fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(save(&blocked, &Settings::default()).is_err());
        assert!(
            !sibling(&blocked, ".tmp").exists(),
            "temp file is cleaned up"
        );
        assert!(blocked.is_dir());

        // The earlier good file is untouched by an unrelated failure, and a later save replaces it whole.
        assert_eq!(load(&path).settings.hotkey, "F13");
        let second = Settings {
            hotkey: "F14".into(),
            ..Settings::default()
        };
        save(&path, &second).unwrap();
        assert_eq!(load(&path).settings.hotkey, "F14");
        let _ = fs::remove_dir_all(dir);
    }
}
