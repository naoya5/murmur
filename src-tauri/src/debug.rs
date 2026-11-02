//! Debug-build-only inputs for end-to-end tests without real keys, a real voice or a real
//! screen (ARCHITECTURE, deviations). Signals: SIGUSR1 acts as the main hotkey (toggle), SIGUSR2
//! as Esc, SIGINFO flips the experimental annotations switch (the setting). With
//! `MURMUR_DEBUG_HOTKEY=annotations`, SIGUSR1 acts as the annotations hotkey instead (SIGWINCH is
//! not used: terminals send it on every resize). Combined with the `wav:<path>` microphone, a
//! test drives a full dictation: `kill -USR1 <pid>`, wait, `kill -USR1 <pid>`.
//!
//! Annotation inputs, read by the controller through the functions below (release builds get
//! `None`): `MURMUR_DEBUG_SCREEN=png-dir:<dir>` plays a PNG sequence as the screen at 2 fps;
//! `MURMUR_DEBUG_POINTER=<script.json>` plays Shift presses and strokes from a script instead
//! of watching the real keys (so a person at the Mac never changes a test);
//! `MURMUR_DEBUG_COMPOSE_DELAY_MS=<n>` delays the compose step to force its timeout.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::adapters::pointer::{PointerEvent, PointerFeed};
use crate::ipc::InkPoint;

fn debug_var(name: &str) -> Option<String> {
    cfg!(debug_assertions)
        .then(|| std::env::var(name).ok())
        .flatten()
        .filter(|v| !v.is_empty())
}

/// The folder of PNGs that stands in for the screen, when the test asks for one.
pub fn screen_png_dir() -> Option<PathBuf> {
    debug_var("MURMUR_DEBUG_SCREEN")?
        .strip_prefix("png-dir:")
        .map(PathBuf::from)
}

/// The pointer script that stands in for the real clicks, cursor and strokes.
pub fn pointer_script() -> Option<PathBuf> {
    debug_var("MURMUR_DEBUG_POINTER").map(PathBuf::from)
}

/// An artificial delay before the compose step (to exercise its timeout).
pub fn compose_delay() -> Option<Duration> {
    debug_var("MURMUR_DEBUG_COMPOSE_DELAY_MS")?
        .parse::<u64>()
        .ok()
        .map(Duration::from_millis)
}

/// Plays a pointer script: `{"events": [...]}` with `kind` one of `shift` (`t_ms`, `held`) or
/// `stroke` (`t0_ms`, `t1_ms`, `points`). Times are recording milliseconds; positions are
/// shares of the display. Strokes are reported like the drawing layer does, in epoch
/// milliseconds. Any other kind is ignored.
pub struct ScriptPlayer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub fn play_pointer_script(
    path: &std::path::Path,
    origin: Instant,
    origin_epoch_ms: f64,
    on_pointer: Arc<dyn Fn(PointerEvent) + Send + Sync>,
    on_stroke: Arc<dyn Fn(Vec<InkPoint>, f64, f64) + Send + Sync>,
) -> Result<ScriptPlayer, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let script: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut events: Vec<(u64, serde_json::Value)> = script["events"]
        .as_array()
        .ok_or("script without an events array")?
        .iter()
        .map(|e| {
            let t = e["t_ms"]
                .as_u64()
                .or_else(|| e["t1_ms"].as_u64())
                .unwrap_or(0);
            (t, e.clone())
        })
        .collect();
    events.sort_by_key(|(t, _)| *t);
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("pointer-script".into())
        .spawn(move || {
            for (t, event) in events {
                let due = origin + Duration::from_millis(t);
                while Instant::now() < due {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                let at = |v: &serde_json::Value| -> Option<(f32, f32)> {
                    Some((v.get(0)?.as_f64()? as f32, v.get(1)?.as_f64()? as f32))
                };
                match event["kind"].as_str().unwrap_or("") {
                    "shift" => on_pointer(PointerEvent::Shift(
                        event["held"].as_bool().unwrap_or(false),
                    )),
                    "stroke" => {
                        let points: Vec<InkPoint> = event["points"]
                            .as_array()
                            .map(|pts| {
                                pts.iter()
                                    .filter_map(|p| at(p).map(|(x, y)| InkPoint { x, y }))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let t0 = event["t0_ms"].as_u64().unwrap_or(t);
                        on_stroke(
                            points,
                            origin_epoch_ms + t0 as f64,
                            origin_epoch_ms + t as f64,
                        );
                    }
                    _ => {}
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(ScriptPlayer {
        stop,
        thread: Some(thread),
    })
}

impl PointerFeed for ScriptPlayer {
    fn stop(mut self: Box<Self>) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(debug_assertions)]
pub fn install(
    app: tauri::AppHandle,
    services: Arc<crate::app::App>,
    controller: crate::controller::ControllerHandle,
) {
    use crate::controller::Msg;
    use murmur_core::session::Event;
    use murmur_core::settings::RecordMode;
    use tokio::signal::unix::{SignalKind, signal};

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
#[cfg(debug_assertions)]
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
