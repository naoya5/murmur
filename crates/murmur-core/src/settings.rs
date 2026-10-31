//! SOT: settings
//! What: the settings type, its defaults, the schema version, migration from older files,
//!       validation, and `SettingKey` (one key per field).
//! Why: a setting that the UI shows but nothing applies was the most common defect of the
//!      reference implementation. `Settings::diff` destructures every field without `..`, so a
//!      new field does not compile until it has a `SettingKey`; the app's `apply(key)` matches
//!      `SettingKey` exhaustively, so the new key does not compile until it is applied (G3).
//! Where: the file adapter is `src-tauri/src/adapters/settings_file.rs`; side effects are applied
//!        in `src-tauri/src/settings_apply.rs`; the UI edits them in `src/views/Settings.tsx`.

use serde::{Deserialize, Serialize};
use specta::Type;
use thiserror::Error;

use crate::Language;

/// Current schema version of `settings.json`. Raise it and add a step to `migrate` when a
/// change is not backward compatible (renames, meaning changes). Added fields only need
/// `#[serde(default)]`.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RecordMode {
    /// Press to start, press again to stop.
    Toggle,
    /// Record while the hotkey is held (S2).
    PushToTalk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum UiLanguage {
    System,
    En,
    Ja,
}

/// Which language a dictionary entry applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum DictionaryScope {
    Ja,
    En,
    Both,
}

impl DictionaryScope {
    pub fn includes(self, lang: Language) -> bool {
        matches!(
            (self, lang),
            (DictionaryScope::Both, _)
                | (DictionaryScope::Ja, Language::Ja)
                | (DictionaryScope::En, Language::En)
        )
    }
}

/// A custom vocabulary entry: how to write a term, and how it sounds (M11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DictionaryEntry {
    /// Stable id (UUID string) so the UI can edit entries in place.
    pub id: String,
    /// The written form, for example "Claude Code".
    pub written: String,
    /// Readings in kana, for example ["クロードコード"]. Empty when the model already writes it.
    /// English entries use readings as "heard as" spellings to replace, for example ["cloud code"].
    pub readings: Vec<String>,
    pub scope: DictionaryScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryRetention {
    Forever,
    Days { days: u32 },
}

/// Model chosen for each language (catalog ids).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ModelChoice {
    pub ja: String,
    pub en: String,
}

impl ModelChoice {
    pub fn for_language(&self, lang: Language) -> &str {
        match lang {
            Language::Ja => &self.ja,
            Language::En => &self.en,
        }
    }
}

/// All user settings. Stored in `settings.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    /// Global hotkey as a Tauri accelerator string, for example "Alt+Space".
    pub hotkey: String,
    pub record_mode: RecordMode,
    /// Input device name. `None` follows the system default.
    pub microphone: Option<String>,
    pub language: Language,
    pub models: ModelChoice,
    pub auto_paste: bool,
    pub restore_clipboard: bool,
    pub remove_fillers: bool,
    pub add_final_punctuation: bool,
    pub dictionary: Vec<DictionaryEntry>,
    pub typing_wpm_en: u32,
    pub typing_cpm_ja: u32,
    pub history_retention: HistoryRetention,
    pub launch_at_login: bool,
    pub auto_check_updates: bool,
    pub ui_language: UiLanguage,
    /// Start and stop sounds (S4).
    pub sounds: bool,
    /// Unload the model after this many idle minutes. `None` keeps it loaded (S5).
    pub unload_model_after_min: Option<u32>,
    /// Optional second hotkey that records in the other language (S7).
    pub alternate_language_hotkey: Option<String>,
    pub onboarding_completed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            hotkey: "Alt+Space".to_owned(),
            record_mode: RecordMode::Toggle,
            microphone: None,
            language: Language::Ja,
            models: ModelChoice {
                ja: crate::catalog::DEFAULT_JA_MODEL.to_owned(),
                en: crate::catalog::DEFAULT_EN_MODEL.to_owned(),
            },
            auto_paste: true,
            restore_clipboard: false,
            remove_fillers: true,
            add_final_punctuation: true,
            dictionary: Vec::new(),
            typing_wpm_en: 40,
            typing_cpm_ja: 80,
            history_retention: HistoryRetention::Forever,
            launch_at_login: false,
            auto_check_updates: true,
            ui_language: UiLanguage::System,
            sounds: false,
            unload_model_after_min: None,
            alternate_language_hotkey: None,
            onboarding_completed: false,
        }
    }
}

/// One key per settings field. `apply` in the app matches this exhaustively (no `_`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SettingKey {
    Hotkey,
    RecordMode,
    Microphone,
    Language,
    Models,
    AutoPaste,
    RestoreClipboard,
    RemoveFillers,
    AddFinalPunctuation,
    Dictionary,
    TypingWpmEn,
    TypingCpmJa,
    HistoryRetention,
    LaunchAtLogin,
    AutoCheckUpdates,
    UiLanguage,
    Sounds,
    UnloadModelAfterMin,
    AlternateLanguageHotkey,
    OnboardingCompleted,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error("settings file is not valid JSON: {0}")]
    Parse(String),
    #[error("settings schema version {0} is newer than this app supports")]
    FutureVersion(u32),
    #[error("invalid value for {key:?}: {reason}")]
    Invalid { key: SettingKey, reason: String },
}

impl Settings {
    /// The keys whose values differ between `self` (old) and `new`.
    ///
    /// Destructures every field without `..`: adding a field breaks the build here until it gets
    /// a `SettingKey` (guardrail G3).
    pub fn diff(&self, new: &Settings) -> Vec<SettingKey> {
        let Settings {
            schema_version: _,
            hotkey,
            record_mode,
            microphone,
            language,
            models,
            auto_paste,
            restore_clipboard,
            remove_fillers,
            add_final_punctuation,
            dictionary,
            typing_wpm_en,
            typing_cpm_ja,
            history_retention,
            launch_at_login,
            auto_check_updates,
            ui_language,
            sounds,
            unload_model_after_min,
            alternate_language_hotkey,
            onboarding_completed,
        } = self;
        let mut keys = Vec::new();
        let mut check = |changed: bool, key: SettingKey| {
            if changed {
                keys.push(key);
            }
        };
        check(*hotkey != new.hotkey, SettingKey::Hotkey);
        check(*record_mode != new.record_mode, SettingKey::RecordMode);
        check(*microphone != new.microphone, SettingKey::Microphone);
        check(*language != new.language, SettingKey::Language);
        check(*models != new.models, SettingKey::Models);
        check(*auto_paste != new.auto_paste, SettingKey::AutoPaste);
        check(
            *restore_clipboard != new.restore_clipboard,
            SettingKey::RestoreClipboard,
        );
        check(
            *remove_fillers != new.remove_fillers,
            SettingKey::RemoveFillers,
        );
        check(
            *add_final_punctuation != new.add_final_punctuation,
            SettingKey::AddFinalPunctuation,
        );
        check(*dictionary != new.dictionary, SettingKey::Dictionary);
        check(*typing_wpm_en != new.typing_wpm_en, SettingKey::TypingWpmEn);
        check(*typing_cpm_ja != new.typing_cpm_ja, SettingKey::TypingCpmJa);
        check(
            *history_retention != new.history_retention,
            SettingKey::HistoryRetention,
        );
        check(
            *launch_at_login != new.launch_at_login,
            SettingKey::LaunchAtLogin,
        );
        check(
            *auto_check_updates != new.auto_check_updates,
            SettingKey::AutoCheckUpdates,
        );
        check(*ui_language != new.ui_language, SettingKey::UiLanguage);
        check(*sounds != new.sounds, SettingKey::Sounds);
        check(
            *unload_model_after_min != new.unload_model_after_min,
            SettingKey::UnloadModelAfterMin,
        );
        check(
            *alternate_language_hotkey != new.alternate_language_hotkey,
            SettingKey::AlternateLanguageHotkey,
        );
        check(
            *onboarding_completed != new.onboarding_completed,
            SettingKey::OnboardingCompleted,
        );
        keys
    }

    /// Checks values that the type system cannot. Hotkey syntax is checked by the hotkey adapter
    /// when it registers the key.
    pub fn validate(&self) -> Result<(), SettingsError> {
        let invalid = |key, reason: &str| {
            Err(SettingsError::Invalid {
                key,
                reason: reason.into(),
            })
        };
        if self.hotkey.trim().is_empty() {
            return invalid(SettingKey::Hotkey, "empty");
        }
        if !(5..=300).contains(&self.typing_wpm_en) {
            return invalid(SettingKey::TypingWpmEn, "must be 5 to 300");
        }
        if !(10..=600).contains(&self.typing_cpm_ja) {
            return invalid(SettingKey::TypingCpmJa, "must be 10 to 600");
        }
        if let HistoryRetention::Days { days: 0 } = self.history_retention {
            return invalid(SettingKey::HistoryRetention, "days must be at least 1");
        }
        if self.unload_model_after_min == Some(0) {
            return invalid(
                SettingKey::UnloadModelAfterMin,
                "minutes must be at least 1",
            );
        }
        for (lang, id) in [
            (Language::Ja, &self.models.ja),
            (Language::En, &self.models.en),
        ] {
            match crate::catalog::find(id) {
                Some(m) if m.languages.contains(&lang) => {}
                _ => return invalid(SettingKey::Models, "unknown model for language"),
            }
        }
        for entry in &self.dictionary {
            if entry.written.trim().is_empty() {
                return invalid(SettingKey::Dictionary, "written form is empty");
            }
        }
        Ok(())
    }

    /// Reads a settings file of any known schema version and returns current settings.
    /// Unknown fields are ignored; missing fields take defaults.
    pub fn migrate(json: &str) -> Result<Settings, SettingsError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| SettingsError::Parse(e.to_string()))?;
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        if version > SCHEMA_VERSION {
            return Err(SettingsError::FutureVersion(version));
        }
        // Version 0 (no field) and 1 share the same shape. Add steps here as `match version`.
        let mut settings: Settings =
            serde_json::from_value(value).map_err(|e| SettingsError::Parse(e.to_string()))?;
        settings.schema_version = SCHEMA_VERSION;
        Ok(settings)
    }
}
