//! Debug-build-only triggers for end-to-end tests without real keys (ARCHITECTURE, deviations):
//! SIGUSR1 acts as the main hotkey (toggle), SIGUSR2 as Esc. Combined with the `wav:<path>`
//! microphone, a test drives a full dictation: `kill -USR1 <pid>`, wait, `kill -USR1 <pid>`.
//! SIGINFO flips the experimental annotations switch (the setting). With
//! `MURMUR_DEBUG_HOTKEY=annotations` in the environment, SIGUSR1 acts as the annotations hotkey
//! instead of the dictation one (SIGWINCH is not used: terminals send it on every resize).

use std::sync::Arc;

use murmur_core::session::Event;
use murmur_core::settings::RecordMode;
use tokio::signal::unix::{SignalKind, signal};

use crate::app::App;
use crate::controller::{ControllerHandle, Msg};

pub fn install(app: tauri::AppHandle, services: Arc<App>, controller: ControllerHandle) {
    tauri::async_runtime::spawn(async move {
        let (Ok(mut hotkey), Ok(mut esc), Ok(mut toggle)) = (
            signal(SignalKind::user_defined1()),
            signal(SignalKind::user_defined2()),
            signal(SignalKind::info()),
        ) else {
            tracing::warn!("debug signal triggers unavailable");
            return;
        };
        let annotations_hotkey =
            std::env::var("MURMUR_DEBUG_HOTKEY").is_ok_and(|v| v == "annotations");
        tracing::info!(
            pid = std::process::id(),
            annotations_hotkey,
            "debug triggers: SIGUSR1 = hotkey, SIGUSR2 = Esc, SIGINFO = annotations on/off"
        );
        loop {
            tokio::select! {
                Some(()) = hotkey.recv() => {
                    if annotations_hotkey {
                        controller.send(Msg::AnnotationsHotkey);
                    } else {
                        controller.event(Event::HotkeyDown { mode: RecordMode::Toggle });
                    }
                }
                Some(()) = esc.recv() => controller.event(Event::Esc),
                Some(()) = toggle.recv() => {
                    let (app, services) = (app.clone(), services.clone());
                    tauri::async_runtime::spawn_blocking(move || {
                        let mut settings = services.settings();
                        settings.annotations_enabled = !settings.annotations_enabled;
                        let enabled = settings.annotations_enabled;
                        match crate::settings_apply::update(&app, &services, settings) {
                            Ok(_) => tracing::info!(enabled, "debug: annotations switch flipped"),
                            Err(error) => tracing::warn!(%error, "debug: annotations switch failed"),
                        }
                    });
                }
                else => break,
            }
        }
    });
}

/// `MURMUR_DEBUG_VIEW=<view>` opens the main window on that view at start (visual QA of the real
/// app without clicking through the tray).
pub fn show_initial_view(app: &tauri::AppHandle) {
    use tauri_specta::Event as _;
    let Ok(view) = std::env::var("MURMUR_DEBUG_VIEW") else {
        return;
    };
    crate::windows::show_main(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // Give the page time to subscribe to events.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = crate::ipc::Navigate { view }.emit(&app);
    });
}
