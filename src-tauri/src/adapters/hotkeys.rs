//! Adapter: hotkeys, on top of `tauri-plugin-global-shortcut`.
//!
//! Registers the main hotkey, an optional alternate hotkey, the annotations hotkey and the
//! translation hotkey (each only while its feature is on) and plain Esc. Press and release are
//! reported separately so push-to-talk can work. The four configurable keys must differ; each
//! setter refuses a clash with the other three. The plugin itself must be added to the Tauri
//! builder in `lib.rs`.

use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use tauri::AppHandle;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    MainDown,
    MainUp,
    AlternateDown,
    AlternateUp,
    /// The annotations hotkey went down (press to start, press again to stop; no release event).
    Annotations,
    /// The translation hotkey (records Japanese for translation, like the main hotkey).
    TranslateDown,
    TranslateUp,
    Esc,
}

#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    #[error("invalid accelerator: {0}")]
    Invalid(String),
    #[error("the key is already used by another Murmur shortcut")]
    Conflict,
    #[error("could not register the shortcut (another app may own it): {0}")]
    Register(String),
}

type Callback = Arc<dyn Fn(HotkeyEvent) + Send + Sync>;

#[derive(Default)]
struct State {
    main: Option<Shortcut>,
    alternate: Option<Shortcut>,
    annotations: Option<Shortcut>,
    translate: Option<Shortcut>,
    esc: bool,
    /// While suspended, nothing is registered with the system; `resume` restores it all.
    suspended: bool,
}

/// Cheap to share: all state is behind one mutex.
pub struct Hotkeys {
    app: AppHandle,
    on_event: Callback,
    state: Mutex<State>,
}

fn esc_shortcut() -> Shortcut {
    Shortcut::new(None, Code::Escape)
}

/// Parse and validate an accelerator such as `Alt+Space` or `Cmd+Shift+D`.
/// A bare key is only allowed for function keys, so normal typing keeps working.
pub fn parse_accelerator(accelerator: &str) -> Result<Shortcut, HotkeyError> {
    let shortcut = Shortcut::from_str(accelerator)
        .map_err(|e| HotkeyError::Invalid(format!("{accelerator}: {e}")))?;
    let is_function_key = {
        let name = format!("{:?}", shortcut.key);
        name.strip_prefix('F')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    if shortcut.mods.is_empty() && !is_function_key {
        return Err(HotkeyError::Invalid(format!(
            "{accelerator}: needs a modifier key"
        )));
    }
    Ok(shortcut)
}

impl Hotkeys {
    pub fn new(app: &AppHandle, on_event: Arc<dyn Fn(HotkeyEvent) + Send + Sync>) -> Hotkeys {
        Hotkeys {
            app: app.clone(),
            on_event,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means another thread panicked; the data is still consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(
        &self,
        shortcut: Shortcut,
        down: HotkeyEvent,
        up: Option<HotkeyEvent>,
    ) -> Result<(), HotkeyError> {
        let on_event = Arc::clone(&self.on_event);
        self.app
            .global_shortcut()
            .on_shortcut(shortcut, move |_app, _shortcut, event| match event.state {
                ShortcutState::Pressed => on_event(down),
                ShortcutState::Released => {
                    if let Some(up) = up {
                        on_event(up);
                    }
                }
            })
            .map_err(|e| HotkeyError::Register(e.to_string()))
    }

    fn unregister(&self, shortcut: Shortcut) {
        let _ = self.app.global_shortcut().unregister(shortcut);
    }

    /// Replace the main hotkey. On failure the old one stays registered.
    pub fn set_main(&self, accelerator: &str) -> Result<(), HotkeyError> {
        let new = parse_accelerator(accelerator)?;
        let mut state = self.lock();
        if [state.alternate, state.annotations, state.translate].contains(&Some(new)) {
            return Err(HotkeyError::Conflict);
        }
        if state.main == Some(new) {
            return Ok(());
        }
        if !state.suspended {
            self.register(new, HotkeyEvent::MainDown, Some(HotkeyEvent::MainUp))?;
            if let Some(old) = state.main {
                self.unregister(old);
            }
        }
        state.main = Some(new);
        Ok(())
    }

    /// Set or clear the alternate hotkey. On failure the old one stays registered.
    pub fn set_alternate(&self, accelerator: Option<&str>) -> Result<(), HotkeyError> {
        let new = accelerator.map(parse_accelerator).transpose()?;
        let mut state = self.lock();
        if new.is_some() && [state.main, state.annotations, state.translate].contains(&new) {
            return Err(HotkeyError::Conflict);
        }
        if new == state.alternate {
            return Ok(());
        }
        if !state.suspended {
            if let Some(new) = new {
                self.register(
                    new,
                    HotkeyEvent::AlternateDown,
                    Some(HotkeyEvent::AlternateUp),
                )?;
            }
            if let Some(old) = state.alternate {
                self.unregister(old);
            }
        }
        state.alternate = new;
        Ok(())
    }

    /// Set or clear the annotations hotkey (cleared while the feature is off). On failure the
    /// old one stays registered.
    pub fn set_annotations(&self, accelerator: Option<&str>) -> Result<(), HotkeyError> {
        let new = accelerator.map(parse_accelerator).transpose()?;
        let mut state = self.lock();
        if new.is_some() && [state.main, state.alternate, state.translate].contains(&new) {
            return Err(HotkeyError::Conflict);
        }
        if new == state.annotations {
            return Ok(());
        }
        if !state.suspended {
            if let Some(new) = new {
                self.register(new, HotkeyEvent::Annotations, None)?;
            }
            if let Some(old) = state.annotations {
                self.unregister(old);
            }
        }
        state.annotations = new;
        Ok(())
    }

    /// Set or clear the translation hotkey (cleared while the feature is off). On failure the
    /// old one stays registered.
    pub fn set_translate(&self, accelerator: Option<&str>) -> Result<(), HotkeyError> {
        let new = accelerator.map(parse_accelerator).transpose()?;
        let mut state = self.lock();
        if new.is_some() && [state.main, state.alternate, state.annotations].contains(&new) {
            return Err(HotkeyError::Conflict);
        }
        if new == state.translate {
            return Ok(());
        }
        if !state.suspended {
            if let Some(new) = new {
                self.register(
                    new,
                    HotkeyEvent::TranslateDown,
                    Some(HotkeyEvent::TranslateUp),
                )?;
            }
            if let Some(old) = state.translate {
                self.unregister(old);
            }
        }
        state.translate = new;
        Ok(())
    }

    /// Turn plain Esc on or off. Esc is only registered while a session is active, so other
    /// apps keep their Esc the rest of the time.
    pub fn set_esc(&self, on: bool) -> Result<(), HotkeyError> {
        let mut state = self.lock();
        if state.esc == on {
            return Ok(());
        }
        if !state.suspended {
            if on {
                self.register(esc_shortcut(), HotkeyEvent::Esc, None)?;
            } else {
                self.unregister(esc_shortcut());
            }
        }
        state.esc = on;
        Ok(())
    }

    /// Unregister everything while the key recorder listens for a new combination.
    pub fn suspend(&self) -> Result<(), HotkeyError> {
        let mut state = self.lock();
        if state.suspended {
            return Ok(());
        }
        self.app
            .global_shortcut()
            .unregister_all()
            .map_err(|e| HotkeyError::Register(e.to_string()))?;
        state.suspended = true;
        Ok(())
    }

    /// Register again what `suspend` removed. Stops at the first failure and reports it; the
    /// entries that did register stay registered.
    pub fn resume(&self) -> Result<(), HotkeyError> {
        let mut state = self.lock();
        if !state.suspended {
            return Ok(());
        }
        state.suspended = false;
        let mut first_error = None;
        let mut note = |result: Result<(), HotkeyError>| {
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        };
        if let Some(main) = state.main {
            note(self.register(main, HotkeyEvent::MainDown, Some(HotkeyEvent::MainUp)));
        }
        if let Some(alternate) = state.alternate {
            note(self.register(
                alternate,
                HotkeyEvent::AlternateDown,
                Some(HotkeyEvent::AlternateUp),
            ));
        }
        if let Some(annotations) = state.annotations {
            note(self.register(annotations, HotkeyEvent::Annotations, None));
        }
        if let Some(translate) = state.translate {
            note(self.register(
                translate,
                HotkeyEvent::TranslateDown,
                Some(HotkeyEvent::TranslateUp),
            ));
        }
        if state.esc {
            note(self.register(esc_shortcut(), HotkeyEvent::Esc, None));
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_modifier_combos_and_function_keys() {
        assert!(parse_accelerator("Alt+Space").is_ok());
        // The defaults of the feature hotkeys, registered only at start when the feature is on.
        assert!(parse_accelerator(murmur_core::settings::DEFAULT_ANNOTATIONS_HOTKEY).is_ok());
        assert!(parse_accelerator(murmur_core::settings::DEFAULT_TRANSLATE_HOTKEY).is_ok());
        assert!(parse_accelerator("CommandOrControl+Shift+D").is_ok());
        assert!(parse_accelerator("F5").is_ok());
        assert!(parse_accelerator("Ctrl+Option+KeyM").is_ok());
    }

    #[test]
    fn rejects_bare_keys_and_garbage() {
        assert!(matches!(
            parse_accelerator("A"),
            Err(HotkeyError::Invalid(_))
        ));
        assert!(matches!(
            parse_accelerator("Escape"),
            Err(HotkeyError::Invalid(_))
        ));
        assert!(matches!(
            parse_accelerator(""),
            Err(HotkeyError::Invalid(_))
        ));
        assert!(matches!(
            parse_accelerator("Alt+"),
            Err(HotkeyError::Invalid(_))
        ));
        assert!(matches!(
            parse_accelerator("Alt+NoSuchKey"),
            Err(HotkeyError::Invalid(_))
        ));
        assert!(matches!(
            parse_accelerator("Alt+A+B"),
            Err(HotkeyError::Invalid(_))
        ));
    }

    #[test]
    fn modifiers_are_case_insensitive() {
        assert_eq!(
            parse_accelerator("alt+space").ok(),
            parse_accelerator("Alt+Space").ok()
        );
    }
}
