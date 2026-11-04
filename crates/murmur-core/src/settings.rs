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
    /// Record while the hotkey is held.
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

/// A custom vocabulary entry: how to write a term, and how it sounds.
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
    /// Start and stop sounds.
    pub sounds: bool,
    /// Unload the model after this many idle minutes. `None` keeps it loaded.
    pub unload_model_after_min: Option<u32>,
    /// Optional second hotkey that records in the other language.
    pub alternate_language_hotkey: Option<String>,
    pub onboarding_completed: bool,
    /// Experimental: screen annotations (pictures of the screen plus the spoken words). Off by
    /// default; while off, nothing of the feature runs.
    pub annotations_enabled: bool,
    /// Hotkey that starts and stops an annotation recording (accelerator string).
    pub annotations_hotkey: String,
    /// How many days an annotation bundle is kept on disk.
    pub annotations_retention_days: u32,
    /// Experimental: translation (speak Japanese, English is pasted) by local models
    /// (`refine.rs`). Off by default; while off, no hotkey, no process, no model runs.
    pub translate_enabled: bool,
    /// Hotkey that records Japanese for translation (accelerator string), same record modes as
    /// the main hotkey. Registered only while translation is on.
    pub translate_hotkey: String,
    /// Experimental: light cleanup of Japanese dictation by a local model.
    pub refine_enabled: bool,
    /// Convert spoken enumerations into bullet lists. Snapshotted at recording start.
    pub refine_lists: bool,
}

/// Default accelerator of the annotations hotkey.
pub const DEFAULT_ANNOTATIONS_HOTKEY: &str = "Option+Shift+KeyS";
pub const DEFAULT_ANNOTATIONS_RETENTION_DAYS: u32 = 7;
pub const ANNOTATIONS_RETENTION_DAYS_RANGE: std::ops::RangeInclusive<u32> = 1..=90;
/// Default accelerator of the translation hotkey.
pub const DEFAULT_TRANSLATE_HOTKEY: &str = "Alt+Shift+KeyE";

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
            annotations_enabled: false,
            annotations_hotkey: DEFAULT_ANNOTATIONS_HOTKEY.to_owned(),
            annotations_retention_days: DEFAULT_ANNOTATIONS_RETENTION_DAYS,
            translate_enabled: false,
            translate_hotkey: DEFAULT_TRANSLATE_HOTKEY.to_owned(),
            refine_enabled: false,
            refine_lists: true,
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
    AnnotationsEnabled,
    AnnotationsHotkey,
    AnnotationsRetentionDays,
    TranslateEnabled,
    TranslateHotkey,
    RefineEnabled,
    RefineLists,
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
    /// The optional cleanup plan for a dictation in its resolved spoken language. Translation
    /// chooses its own plan; English dictation never uses the Japanese-only cleanup rules.
    pub fn dictation_refine(&self, language: Language) -> Option<crate::refine::RefinePlan> {
        (self.refine_enabled && language == Language::Ja).then_some(crate::refine::RefinePlan::new(
            crate::refine::RefineKind::Clean,
            self.refine_lists,
        ))
    }

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
            annotations_enabled,
            annotations_hotkey,
            annotations_retention_days,
            translate_enabled,
            translate_hotkey,
            refine_enabled,
            refine_lists,
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
        check(
            *annotations_enabled != new.annotations_enabled,
            SettingKey::AnnotationsEnabled,
        );
        check(
            *annotations_hotkey != new.annotations_hotkey,
            SettingKey::AnnotationsHotkey,
        );
        check(
            *annotations_retention_days != new.annotations_retention_days,
            SettingKey::AnnotationsRetentionDays,
        );
        check(
            *translate_enabled != new.translate_enabled,
            SettingKey::TranslateEnabled,
        );
        check(
            *translate_hotkey != new.translate_hotkey,
            SettingKey::TranslateHotkey,
        );
        check(
            *refine_enabled != new.refine_enabled,
            SettingKey::RefineEnabled,
        );
        check(*refine_lists != new.refine_lists, SettingKey::RefineLists);
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
            // Only a recognition model transcribes; the refine models list languages too.
            match crate::catalog::find(id) {
                Some(m)
                    if m.role == crate::catalog::Role::Recognition
                        && m.languages.contains(&lang) => {}
                _ => return invalid(SettingKey::Models, "unknown model for language"),
            }
        }
        for entry in &self.dictionary {
            if entry.written.trim().is_empty() {
                return invalid(SettingKey::Dictionary, "written form is empty");
            }
        }
        if self.annotations_hotkey.trim().is_empty() {
            return invalid(SettingKey::AnnotationsHotkey, "empty");
        }
        if !ANNOTATIONS_RETENTION_DAYS_RANGE.contains(&self.annotations_retention_days) {
            return invalid(SettingKey::AnnotationsRetentionDays, "must be 1 to 90");
        }
        if self.translate_hotkey.trim().is_empty() {
            return invalid(SettingKey::TranslateHotkey, "empty");
        }
        // The annotations and translation hotkeys are registered only while their feature is on,
        // so a clash with an unused key must not make the settings file invalid (the loader would
        // reset the field).
        if self.annotations_enabled {
            let clashes = |other: &str| same_accelerator(&self.annotations_hotkey, other);
            if clashes(&self.hotkey) {
                return invalid(
                    SettingKey::AnnotationsHotkey,
                    "same key as the dictation hotkey",
                );
            }
            if self
                .alternate_language_hotkey
                .as_deref()
                .is_some_and(clashes)
            {
                return invalid(
                    SettingKey::AnnotationsHotkey,
                    "same key as the alternate language hotkey",
                );
            }
        }
        if self.translate_enabled {
            let clashes = |other: &str| same_accelerator(&self.translate_hotkey, other);
            if clashes(&self.hotkey) {
                return invalid(
                    SettingKey::TranslateHotkey,
                    "same key as the dictation hotkey",
                );
            }
            if self
                .alternate_language_hotkey
                .as_deref()
                .is_some_and(clashes)
            {
                return invalid(
                    SettingKey::TranslateHotkey,
                    "same key as the alternate language hotkey",
                );
            }
            if self.annotations_enabled && clashes(&self.annotations_hotkey) {
                return invalid(
                    SettingKey::TranslateHotkey,
                    "same key as the annotations hotkey",
                );
            }
        }
        Ok(())
    }

    /// Reads a settings file of any known schema version and returns current settings.
    /// Unknown fields are ignored; missing fields take defaults.
    pub fn migrate(json: &str) -> Result<Settings, SettingsError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| SettingsError::Parse(e.to_string()))?;
        if !value.is_object() {
            // serde would also accept a positional array; a settings file is always an object.
            return Err(SettingsError::Parse("not a JSON object".to_owned()));
        }
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

/// Whether two accelerator strings name the same key combination.
///
/// The hotkey adapter accepts several spellings for one key ("Option" and "Alt", "Cmd" and
/// "Command", "S" and "KeyS", any order of modifiers, any case). Two hotkeys clash when their
/// normalised forms are equal. Syntax is not checked here; the adapter does that when it registers.
pub fn same_accelerator(a: &str, b: &str) -> bool {
    normalise_accelerator(a) == normalise_accelerator(b)
}

fn normalise_accelerator(accelerator: &str) -> (Vec<String>, String) {
    let mut modifiers: Vec<String> = Vec::new();
    let mut key = String::new();
    for part in accelerator
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let lower = part.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "alt" | "option" => Some("alt"),
            "ctrl" | "control" => Some("ctrl"),
            "cmd" | "command" | "super" | "meta" => Some("meta"),
            "shift" => Some("shift"),
            "commandorcontrol" | "cmdorctrl" | "cmdorcontrol" | "commandorctrl" => {
                Some("cmdorctrl")
            }
            _ => None,
        };
        match modifier {
            Some(m) => modifiers.push(m.to_owned()),
            None => key = canonical_key(&lower),
        }
    }
    modifiers.sort();
    modifiers.dedup();
    (modifiers, key)
}

/// "s" and "keys" are the same key; "5" and "digit5" too.
fn canonical_key(lower: &str) -> String {
    let mut chars = lower.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_lowercase() => format!("key{c}"),
        (Some(c), None) if c.is_ascii_digit() => format!("digit{c}"),
        _ => lower.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(written: &str) -> DictionaryEntry {
        DictionaryEntry {
            id: "11111111-2222-3333-4444-555555555555".to_owned(),
            written: written.to_owned(),
            readings: vec!["クロードコード".to_owned()],
            scope: DictionaryScope::Both,
        }
    }

    /// A change to apply to default settings, and the key it must be reported under.
    type Case = (Box<dyn Fn(&mut Settings)>, SettingKey);

    // --- defaults and validation ---

    #[test]
    fn defaults_validate() {
        let s = Settings::default();
        assert_eq!(s.validate(), Ok(()));
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        assert_eq!(s.hotkey, "Alt+Space");
        assert_eq!(s.record_mode, RecordMode::Toggle);
        assert_eq!(s.language, Language::Ja);
        assert_eq!(s.models.ja, crate::catalog::DEFAULT_JA_MODEL);
        assert_eq!(s.models.en, crate::catalog::DEFAULT_EN_MODEL);
        assert!(s.auto_paste);
        assert!(!s.restore_clipboard);
        assert!(s.remove_fillers);
        assert!(s.add_final_punctuation);
        assert_eq!(s.typing_wpm_en, 40);
        assert_eq!(s.typing_cpm_ja, 80);
        assert_eq!(s.history_retention, HistoryRetention::Forever);
        assert!(!s.launch_at_login);
        assert!(s.auto_check_updates);
        assert_eq!(s.ui_language, UiLanguage::System);
        assert!(!s.onboarding_completed);
        assert!(!s.annotations_enabled);
        assert_eq!(s.annotations_hotkey, "Option+Shift+KeyS");
        assert_eq!(s.annotations_retention_days, 7);
        assert!(!s.translate_enabled);
        assert_eq!(s.translate_hotkey, "Alt+Shift+KeyE");
        assert!(!s.refine_enabled);
        assert!(s.refine_lists);
    }

    #[test]
    fn annotations_hotkey_may_not_clash_while_enabled() {
        let clash = |mutate: &dyn Fn(&mut Settings)| {
            let mut s = Settings {
                annotations_enabled: true,
                ..Settings::default()
            };
            mutate(&mut s);
            s.validate()
        };
        // Same key, other spelling and order: refused under the annotations key.
        assert_eq!(
            clash(&|s| s.hotkey = "Shift+Alt+S".to_owned()),
            Err(SettingsError::Invalid {
                key: SettingKey::AnnotationsHotkey,
                reason: "same key as the dictation hotkey".to_owned(),
            })
        );
        assert!(matches!(
            clash(&|s| s.alternate_language_hotkey = Some("option+shift+keys".to_owned())),
            Err(SettingsError::Invalid {
                key: SettingKey::AnnotationsHotkey,
                ..
            })
        ));
        // A different key is fine.
        assert_eq!(
            clash(&|s| s.alternate_language_hotkey = Some("Alt+Shift+Space".to_owned())),
            Ok(())
        );
        // While the feature is off the clash is not an error: the key is not registered.
        let s = Settings {
            hotkey: "Option+Shift+KeyS".to_owned(),
            ..Settings::default()
        };
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn a_refine_model_cannot_be_the_speech_model() {
        let mut s = Settings::default();
        s.models.ja = crate::catalog::DEFAULT_CLEAN_MODEL.to_owned();
        assert!(matches!(
            s.validate(),
            Err(SettingsError::Invalid {
                key: SettingKey::Models,
                ..
            })
        ));
    }

    #[test]
    fn translate_hotkey_may_not_clash_while_enabled() {
        let clash = |mutate: &dyn Fn(&mut Settings)| {
            let mut s = Settings {
                translate_enabled: true,
                ..Settings::default()
            };
            mutate(&mut s);
            s.validate()
        };
        assert_eq!(clash(&|_| {}), Ok(()));
        assert_eq!(
            clash(&|s| s.hotkey = "Shift+Option+E".to_owned()),
            Err(SettingsError::Invalid {
                key: SettingKey::TranslateHotkey,
                reason: "same key as the dictation hotkey".to_owned(),
            })
        );
        assert!(matches!(
            clash(&|s| s.alternate_language_hotkey = Some("alt+shift+keye".to_owned())),
            Err(SettingsError::Invalid {
                key: SettingKey::TranslateHotkey,
                ..
            })
        ));
        // The annotations hotkey counts only while annotations are on too.
        assert_eq!(
            clash(&|s| s.annotations_hotkey = "Alt+Shift+KeyE".to_owned()),
            Ok(())
        );
        assert_eq!(
            clash(&|s| {
                s.annotations_enabled = true;
                s.translate_hotkey = "Option+Shift+KeyS".to_owned();
            }),
            Err(SettingsError::Invalid {
                key: SettingKey::TranslateHotkey,
                reason: "same key as the annotations hotkey".to_owned(),
            })
        );
        assert!(matches!(
            clash(&|s| s.translate_hotkey = " ".to_owned()),
            Err(SettingsError::Invalid {
                key: SettingKey::TranslateHotkey,
                ..
            })
        ));
        // While translation is off the clash is not an error: the key is not registered.
        let s = Settings {
            hotkey: "Alt+Shift+KeyE".to_owned(),
            ..Settings::default()
        };
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn same_accelerator_ignores_spelling_case_and_order() {
        assert!(same_accelerator("Option+Shift+KeyS", "Shift+Alt+S"));
        assert!(same_accelerator("Cmd+Shift+D", "shift+command+keyd"));
        assert!(same_accelerator("CommandOrControl+5", "CmdOrCtrl+Digit5"));
        assert!(same_accelerator(" Alt + Space ", "alt+space"));
        assert!(!same_accelerator("Alt+Space", "Alt+Shift+Space"));
        assert!(!same_accelerator("Alt+KeyS", "Ctrl+KeyS"));
        assert!(!same_accelerator("F5", "F6"));
    }

    #[test]
    fn validate_rejects_bad_values() {
        let cases: Vec<Case> = vec![
            (
                Box::new(|s| s.hotkey = "   ".to_owned()),
                SettingKey::Hotkey,
            ),
            (Box::new(|s| s.typing_wpm_en = 4), SettingKey::TypingWpmEn),
            (Box::new(|s| s.typing_wpm_en = 301), SettingKey::TypingWpmEn),
            (Box::new(|s| s.typing_cpm_ja = 9), SettingKey::TypingCpmJa),
            (Box::new(|s| s.typing_cpm_ja = 601), SettingKey::TypingCpmJa),
            (
                Box::new(|s| s.history_retention = HistoryRetention::Days { days: 0 }),
                SettingKey::HistoryRetention,
            ),
            (
                Box::new(|s| s.unload_model_after_min = Some(0)),
                SettingKey::UnloadModelAfterMin,
            ),
            (
                Box::new(|s| s.models.ja = "no-such-model".to_owned()),
                SettingKey::Models,
            ),
            // A real model, but for the other language.
            (
                Box::new(|s| s.models.en = crate::catalog::DEFAULT_JA_MODEL.to_owned()),
                SettingKey::Models,
            ),
            (
                Box::new(|s| s.models.ja = crate::catalog::VAD_MODEL.to_owned()),
                SettingKey::Models,
            ),
            (
                Box::new(|s| s.dictionary = vec![entry("  ")]),
                SettingKey::Dictionary,
            ),
            (
                Box::new(|s| s.annotations_hotkey = " ".to_owned()),
                SettingKey::AnnotationsHotkey,
            ),
            (
                Box::new(|s| s.annotations_retention_days = 0),
                SettingKey::AnnotationsRetentionDays,
            ),
            (
                Box::new(|s| s.annotations_retention_days = 91),
                SettingKey::AnnotationsRetentionDays,
            ),
        ];
        for (i, (mutate, key)) in cases.iter().enumerate() {
            let mut s = Settings::default();
            mutate(&mut s);
            match s.validate() {
                Err(SettingsError::Invalid { key: k, .. }) => assert_eq!(k, *key, "case {i}"),
                other => panic!("case {i}: expected Invalid {key:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_accepts_edge_values() {
        let s = Settings {
            typing_wpm_en: 5,
            typing_cpm_ja: 600,
            history_retention: HistoryRetention::Days { days: 1 },
            unload_model_after_min: Some(1),
            models: ModelChoice {
                ja: "qwen3-asr-0.6b-q8_0".to_owned(),
                en: "qwen3-asr-0.6b-q8_0".to_owned(),
            },
            dictionary: vec![entry("Claude Code")],
            annotations_retention_days: 90,
            ..Settings::default()
        };
        assert_eq!(s.validate(), Ok(()));
        let s = Settings {
            annotations_retention_days: 1,
            ..Settings::default()
        };
        assert_eq!(s.validate(), Ok(()));
    }

    // --- diff ---

    #[test]
    fn diff_of_equal_settings_is_empty() {
        let s = Settings::default();
        assert!(s.diff(&s.clone()).is_empty());
        let mut other = s.clone();
        other.schema_version = 99; // not a user setting
        assert!(s.diff(&other).is_empty());
    }

    #[test]
    fn diff_reports_each_changed_key() {
        let cases: Vec<Case> = vec![
            (
                Box::new(|s| s.hotkey = "Cmd+Shift+Space".to_owned()),
                SettingKey::Hotkey,
            ),
            (
                Box::new(|s| s.record_mode = RecordMode::PushToTalk),
                SettingKey::RecordMode,
            ),
            (
                Box::new(|s| s.microphone = Some("USB Mic".to_owned())),
                SettingKey::Microphone,
            ),
            (
                Box::new(|s| s.language = Language::En),
                SettingKey::Language,
            ),
            (
                Box::new(|s| s.models.ja = "haas-ja-v1.0-q5_k".to_owned()),
                SettingKey::Models,
            ),
            (Box::new(|s| s.auto_paste = false), SettingKey::AutoPaste),
            (
                Box::new(|s| s.restore_clipboard = true),
                SettingKey::RestoreClipboard,
            ),
            (
                Box::new(|s| s.remove_fillers = false),
                SettingKey::RemoveFillers,
            ),
            (
                Box::new(|s| s.add_final_punctuation = false),
                SettingKey::AddFinalPunctuation,
            ),
            (
                Box::new(|s| s.dictionary = vec![entry("Claude Code")]),
                SettingKey::Dictionary,
            ),
            (Box::new(|s| s.typing_wpm_en = 60), SettingKey::TypingWpmEn),
            (Box::new(|s| s.typing_cpm_ja = 100), SettingKey::TypingCpmJa),
            (
                Box::new(|s| s.history_retention = HistoryRetention::Days { days: 30 }),
                SettingKey::HistoryRetention,
            ),
            (
                Box::new(|s| s.launch_at_login = true),
                SettingKey::LaunchAtLogin,
            ),
            (
                Box::new(|s| s.auto_check_updates = false),
                SettingKey::AutoCheckUpdates,
            ),
            (
                Box::new(|s| s.ui_language = UiLanguage::Ja),
                SettingKey::UiLanguage,
            ),
            (Box::new(|s| s.sounds = true), SettingKey::Sounds),
            (
                Box::new(|s| s.unload_model_after_min = Some(10)),
                SettingKey::UnloadModelAfterMin,
            ),
            (
                Box::new(|s| s.alternate_language_hotkey = Some("Alt+Shift+Space".to_owned())),
                SettingKey::AlternateLanguageHotkey,
            ),
            (
                Box::new(|s| s.onboarding_completed = true),
                SettingKey::OnboardingCompleted,
            ),
            (
                Box::new(|s| s.annotations_enabled = true),
                SettingKey::AnnotationsEnabled,
            ),
            (
                Box::new(|s| s.annotations_hotkey = "Option+Shift+KeyA".to_owned()),
                SettingKey::AnnotationsHotkey,
            ),
            (
                Box::new(|s| s.annotations_retention_days = 30),
                SettingKey::AnnotationsRetentionDays,
            ),
            (
                Box::new(|s| s.translate_enabled = true),
                SettingKey::TranslateEnabled,
            ),
            (
                Box::new(|s| s.translate_hotkey = "Alt+Shift+KeyT".to_owned()),
                SettingKey::TranslateHotkey,
            ),
            (
                Box::new(|s| s.refine_enabled = true),
                SettingKey::RefineEnabled,
            ),
            (
                Box::new(|s| s.refine_lists = false),
                SettingKey::RefineLists,
            ),
        ];
        let old = Settings::default();
        let mut seen = Vec::new();
        for (mutate, key) in &cases {
            let mut new = old.clone();
            mutate(&mut new);
            assert_eq!(old.diff(&new), vec![*key], "{key:?}");
            assert_eq!(new.diff(&old), vec![*key], "{key:?} reversed");
            seen.push(*key);
        }
        // Every key is exercised exactly once (keep this list in step with `SettingKey`).
        seen.sort_by_key(|k| format!("{k:?}"));
        seen.dedup();
        assert_eq!(seen.len(), 27);
    }

    #[test]
    fn diff_reports_several_keys_in_field_order() {
        let old = Settings::default();
        let mut new = old.clone();
        new.sounds = true;
        new.hotkey = "F5".to_owned();
        new.language = Language::En;
        assert_eq!(
            old.diff(&new),
            vec![SettingKey::Hotkey, SettingKey::Language, SettingKey::Sounds]
        );
    }

    // --- migrate ---

    #[test]
    fn migrate_from_empty_object_gives_defaults() {
        let s = Settings::migrate("{}").unwrap();
        assert_eq!(s, Settings::default());
        assert_eq!(s.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_keeps_known_fields_and_ignores_unknown_ones() {
        let json = r#"{
            "schema_version": 1,
            "hotkey": "Cmd+Shift+D",
            "record_mode": "push_to_talk",
            "language": "en",
            "typing_wpm_en": 70,
            "history_retention": { "kind": "days", "days": 14 },
            "dictionary": [
                { "id": "a", "written": "Claude Code", "readings": ["cloud code"], "scope": "en" }
            ],
            "some_future_field": true,
            "another": { "nested": [1, 2, 3] }
        }"#;
        let s = Settings::migrate(json).unwrap();
        assert_eq!(s.hotkey, "Cmd+Shift+D");
        assert_eq!(s.record_mode, RecordMode::PushToTalk);
        assert_eq!(s.language, Language::En);
        assert_eq!(s.typing_wpm_en, 70);
        assert_eq!(s.history_retention, HistoryRetention::Days { days: 14 });
        assert_eq!(s.dictionary.len(), 1);
        assert_eq!(s.dictionary[0].scope, DictionaryScope::En);
        // Everything else is the default.
        assert_eq!(s.typing_cpm_ja, 80);
        assert_eq!(s.models, Settings::default().models);
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn migrate_version_zero_file() {
        // A file without `schema_version` is version 0 and gets the current version.
        let s = Settings::migrate(r#"{ "auto_paste": false }"#).unwrap();
        assert!(!s.auto_paste);
        assert_eq!(s.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_refuses_a_future_version() {
        let future = SCHEMA_VERSION + 1;
        let json = format!(r#"{{ "schema_version": {future} }}"#);
        assert_eq!(
            Settings::migrate(&json),
            Err(SettingsError::FutureVersion(future))
        );
    }

    #[test]
    fn migrate_reports_broken_json() {
        for bad in [
            "",
            "not json",
            "{ \"hotkey\": ",
            "[]",
            "{ \"typing_wpm_en\": \"fast\" }",
        ] {
            match Settings::migrate(bad) {
                Err(SettingsError::Parse(_)) => {}
                other => panic!("{bad:?}: expected Parse, got {other:?}"),
            }
        }
    }

    #[test]
    fn json_round_trip() {
        let s = Settings {
            hotkey: "Ctrl+Alt+M".to_owned(),
            record_mode: RecordMode::PushToTalk,
            microphone: Some("Built-in".to_owned()),
            language: Language::En,
            dictionary: vec![entry("Claude Code")],
            history_retention: HistoryRetention::Days { days: 7 },
            ui_language: UiLanguage::En,
            unload_model_after_min: Some(15),
            alternate_language_hotkey: Some("Alt+Shift+Space".to_owned()),
            annotations_enabled: true,
            annotations_retention_days: 14,
            translate_enabled: true,
            translate_hotkey: "Ctrl+Alt+KeyT".to_owned(),
            ..Settings::default()
        };
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(Settings::migrate(&json).unwrap(), s);
        // Enum spellings are snake_case on disk.
        assert!(json.contains("\"record_mode\": \"push_to_talk\""));
        assert!(json.contains("\"kind\": \"days\""));
        assert!(json.contains("\"language\": \"en\""));
    }

    #[test]
    fn cleanup_settings_validate_and_round_trip_in_every_combination() {
        for enabled in [false, true] {
            for lists in [false, true] {
                for language in [Language::Ja, Language::En] {
                    let s = Settings {
                        refine_enabled: enabled,
                        refine_lists: lists,
                        language,
                        ..Settings::default()
                    };
                    assert_eq!(s.validate(), Ok(()));
                    assert_eq!(
                        s.dictation_refine(language),
                        (enabled && language == Language::Ja).then_some(
                            crate::refine::RefinePlan::new(crate::refine::RefineKind::Clean, lists)
                        )
                    );
                    let json = serde_json::to_string(&s).unwrap();
                    assert_eq!(Settings::migrate(&json).unwrap(), s);
                }
            }
        }
        for bad in [r#"{"refine_enabled":"true"}"#, r#"{"refine_lists":0}"#] {
            assert!(matches!(
                Settings::migrate(bad),
                Err(SettingsError::Parse(_))
            ));
        }
    }

    #[test]
    fn old_settings_keep_cleanup_off_and_lists_on() {
        let s = Settings::migrate(r#"{"translate_enabled":true}"#).unwrap();
        assert!(!s.refine_enabled);
        assert!(s.refine_lists);
        assert!(s.translate_enabled);
    }

    #[test]
    fn dictionary_scope_includes() {
        assert!(DictionaryScope::Both.includes(Language::Ja));
        assert!(DictionaryScope::Both.includes(Language::En));
        assert!(DictionaryScope::Ja.includes(Language::Ja));
        assert!(!DictionaryScope::Ja.includes(Language::En));
        assert!(DictionaryScope::En.includes(Language::En));
        assert!(!DictionaryScope::En.includes(Language::Ja));
    }

    #[test]
    fn model_choice_for_language() {
        let m = ModelChoice {
            ja: "a".to_owned(),
            en: "b".to_owned(),
        };
        assert_eq!(m.for_language(Language::Ja), "a");
        assert_eq!(m.for_language(Language::En), "b");
    }
}
