//! Menu bar icon and its menu.
//!
//! `create` builds the tray once; `update` rebuilds the menu from a `TrayModel` (recording state, language,
//! recent items). Menu clicks become `TrayAction`s for the controller. Strings are kept here, English and
//! Japanese, short. Previews are transcript text: they are shown, never logged (G7).

use std::sync::Arc;

use murmur_core::Language;
use tauri::AppHandle;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;

const TRAY_ID: &str = "murmur-tray";
/// Monochrome waveform glyph, 44 px (22 pt at 2x). Used as a template image so macOS tints it.
const ICON: &[u8] = include_bytes!("../icons/tray@2x.png");
const PREVIEW_CHARS: usize = 40;

#[derive(Clone)]
pub struct TrayModel {
    pub recording: bool,
    pub language: Language,
    /// (id, preview). The preview is cut to 40 characters here.
    pub recent: Vec<(String, String)>,
    pub ui_ja: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    OpenMain,
    ToggleRecording,
    SetLanguage(Language),
    CopyLast,
    CopyRecent(String),
    CheckUpdates,
    Quit,
}

#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    #[error("tray error: {0}")]
    Tauri(String),
}

impl From<tauri::Error> for TrayError {
    fn from(e: tauri::Error) -> Self {
        TrayError::Tauri(e.to_string())
    }
}

const ID_OPEN: &str = "open";
const ID_TOGGLE: &str = "toggle";
const ID_LANG_JA: &str = "lang-ja";
const ID_LANG_EN: &str = "lang-en";
const ID_COPY_LAST: &str = "copy-last";
const ID_RECENT_PREFIX: &str = "recent:";
const ID_UPDATES: &str = "updates";
const ID_QUIT: &str = "quit";

fn action_from_id(id: &str) -> Option<TrayAction> {
    match id {
        ID_OPEN => Some(TrayAction::OpenMain),
        ID_TOGGLE => Some(TrayAction::ToggleRecording),
        ID_LANG_JA => Some(TrayAction::SetLanguage(Language::Ja)),
        ID_LANG_EN => Some(TrayAction::SetLanguage(Language::En)),
        ID_COPY_LAST => Some(TrayAction::CopyLast),
        ID_UPDATES => Some(TrayAction::CheckUpdates),
        ID_QUIT => Some(TrayAction::Quit),
        other => other
            .strip_prefix(ID_RECENT_PREFIX)
            .map(|id| TrayAction::CopyRecent(id.to_owned())),
    }
}

struct Labels {
    open: &'static str,
    start: &'static str,
    stop: &'static str,
    language: &'static str,
    copy_last: &'static str,
    recent: &'static str,
    no_history: &'static str,
    updates: &'static str,
    quit: &'static str,
}

fn labels(ui_ja: bool) -> Labels {
    if ui_ja {
        Labels {
            open: "Murmur を開く",
            start: "録音を開始",
            stop: "録音を停止",
            language: "言語",
            copy_last: "直前の文字起こしをコピー",
            recent: "最近の文字起こし",
            no_history: "履歴はありません",
            updates: "アップデートを確認",
            quit: "Murmur を終了",
        }
    } else {
        Labels {
            open: "Open Murmur",
            start: "Start recording",
            stop: "Stop recording",
            language: "Language",
            copy_last: "Copy last transcript",
            recent: "Recent",
            no_history: "No history yet",
            updates: "Check for updates",
            quit: "Quit Murmur",
        }
    }
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::Ja => "日本語",
        Language::En => "English",
    }
}

/// One line, at most 40 characters, with an ellipsis when cut.
fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(PREVIEW_CHARS - 1).collect();
    format!("{cut}…")
}

fn build_menu(app: &AppHandle, model: &TrayModel) -> tauri::Result<Menu<tauri::Wry>> {
    let t = labels(model.ui_ja);
    let open = MenuItem::with_id(app, ID_OPEN, t.open, true, None::<&str>)?;
    let toggle = MenuItem::with_id(
        app,
        ID_TOGGLE,
        if model.recording { t.stop } else { t.start },
        true,
        None::<&str>,
    )?;
    let languages = [Language::Ja, Language::En]
        .into_iter()
        .map(|language| {
            let id = match language {
                Language::Ja => ID_LANG_JA,
                Language::En => ID_LANG_EN,
            };
            CheckMenuItem::with_id(
                app,
                id,
                language_name(language),
                true,
                model.language == language,
                None::<&str>,
            )
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let language_refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
        languages.iter().map(|i| i as _).collect();
    let language_menu = Submenu::with_items(app, t.language, true, &language_refs)?;

    let has_history = !model.recent.is_empty();
    let copy_last = MenuItem::with_id(app, ID_COPY_LAST, t.copy_last, has_history, None::<&str>)?;
    let recent_items = if has_history {
        model
            .recent
            .iter()
            .map(|(id, text)| {
                MenuItem::with_id(
                    app,
                    format!("{ID_RECENT_PREFIX}{id}"),
                    preview(text),
                    true,
                    None::<&str>,
                )
            })
            .collect::<tauri::Result<Vec<_>>>()?
    } else {
        vec![MenuItem::with_id(
            app,
            "no-history",
            t.no_history,
            false,
            None::<&str>,
        )?]
    };
    let recent_refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
        recent_items.iter().map(|i| i as _).collect();
    let recent_menu = Submenu::with_items(app, t.recent, true, &recent_refs)?;

    let updates = MenuItem::with_id(app, ID_UPDATES, t.updates, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ID_QUIT, t.quit, true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    Menu::with_items(
        app,
        &[
            &open,
            &toggle,
            &sep1,
            &language_menu,
            &sep2,
            &copy_last,
            &recent_menu,
            &sep3,
            &updates,
            &quit,
        ],
    )
}

/// Creates the tray icon. Call once, on the main thread (the `setup` closure).
pub fn create(
    app: &AppHandle,
    model: &TrayModel,
    on_action: Arc<dyn Fn(TrayAction) + Send + Sync>,
) -> Result<(), TrayError> {
    let menu = build_menu(app, model)?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(ICON)?)
        .icon_as_template(true)
        .tooltip("Murmur")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |_app, event| {
            if let Some(action) = action_from_id(event.id().as_ref()) {
                on_action(action);
            }
        })
        .build(app)?;
    Ok(())
}

/// Rebuilds the menu. Any thread: the work hops to the main thread.
pub fn update(app: &AppHandle, model: &TrayModel) {
    let model = model.clone();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = handle.tray_by_id(TRAY_ID) else {
            return;
        };
        match build_menu(&handle, &model) {
            Ok(menu) => {
                let _ = tray.set_menu(Some(menu));
            }
            Err(e) => tracing::warn!("could not rebuild the tray menu: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_map_to_actions() {
        assert_eq!(action_from_id("open"), Some(TrayAction::OpenMain));
        assert_eq!(action_from_id("toggle"), Some(TrayAction::ToggleRecording));
        assert_eq!(
            action_from_id("lang-ja"),
            Some(TrayAction::SetLanguage(Language::Ja))
        );
        assert_eq!(
            action_from_id("lang-en"),
            Some(TrayAction::SetLanguage(Language::En))
        );
        assert_eq!(action_from_id("copy-last"), Some(TrayAction::CopyLast));
        assert_eq!(
            action_from_id("recent:abc-123"),
            Some(TrayAction::CopyRecent("abc-123".into()))
        );
        assert_eq!(action_from_id("updates"), Some(TrayAction::CheckUpdates));
        assert_eq!(action_from_id("quit"), Some(TrayAction::Quit));
        assert_eq!(action_from_id("no-history"), None);
    }

    #[test]
    fn preview_is_one_line_and_short() {
        assert_eq!(preview("hello\n  world"), "hello world");
        let long = "あ".repeat(100);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW_CHARS);
        assert!(p.ends_with('…'));
        assert_eq!(preview("short"), "short");
    }

    #[test]
    fn labels_exist_in_both_languages() {
        assert_ne!(labels(true).quit, labels(false).quit);
        assert!(ICON.starts_with(b"\x89PNG"));
    }
}
