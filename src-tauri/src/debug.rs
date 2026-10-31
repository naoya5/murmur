//! Debug-build-only triggers for end-to-end tests without real keys (ARCHITECTURE, deviations):
//! SIGUSR1 acts as the main hotkey (toggle), SIGUSR2 as Esc. Combined with the `wav:<path>`
//! microphone, a test drives a full dictation: `kill -USR1 <pid>`, wait, `kill -USR1 <pid>`.

use murmur_core::session::Event;
use murmur_core::settings::RecordMode;
use tokio::signal::unix::{SignalKind, signal};

use crate::controller::ControllerHandle;

pub fn install(controller: ControllerHandle) {
    tauri::async_runtime::spawn(async move {
        let (Ok(mut hotkey), Ok(mut esc)) = (
            signal(SignalKind::user_defined1()),
            signal(SignalKind::user_defined2()),
        ) else {
            tracing::warn!("debug signal triggers unavailable");
            return;
        };
        tracing::info!(
            pid = std::process::id(),
            "debug triggers: SIGUSR1 = hotkey, SIGUSR2 = Esc"
        );
        loop {
            tokio::select! {
                Some(()) = hotkey.recv() => controller.event(Event::HotkeyDown { mode: RecordMode::Toggle }),
                Some(()) = esc.recv() => controller.event(Event::Esc),
                else => break,
            }
        }
    });
}
