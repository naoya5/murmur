//! Murmur app shell: assembly only. It wires the pure core, the adapters and the windows together.
//!
//! Start-up order: paths -> logging -> settings -> history store (+ spool recovery) -> inference
//! thread -> services -> windows -> controller -> hotkeys -> settings side effects -> tray.
//! Exit: the controller stops capture (the spool is kept for recovery) and the inference thread
//! frees the model before the process ends (Metal aborts if a model is still loaded).

mod adapters;
mod app;
mod controller;
#[cfg(debug_assertions)]
mod debug;
mod history;
mod inference;
mod ipc;
mod platform;
mod settings_apply;
mod tray;
mod windows;

use std::sync::Arc;
use std::time::Duration;

use murmur_core::Language;
use murmur_core::session::{Event, PillView};
use tauri::{AppHandle, Manager, RunEvent};
use tauri_specta::Event as _;

use crate::adapters::hotkeys::{HotkeyEvent, Hotkeys};
use crate::adapters::{engine, settings_file, store::Store};
use crate::app::{App, Paths};
use crate::controller::{ControllerHandle, Msg};
use crate::inference::Inference;
use crate::tray::{TrayAction, TrayModel};

/// How often the idle check runs for the "unload model after N minutes" setting (S5).
const IDLE_CHECK: Duration = Duration::from_secs(60);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = ipc::builder();

    #[cfg(debug_assertions)]
    if let Err(error) = ipc::export_bindings(&builder) {
        eprintln!("failed to export bindings: {error}");
    }

    // A debug instance on a scratch data dir (end-to-end tests) runs beside the real app.
    let isolated = cfg!(debug_assertions) && std::env::var_os("MURMUR_DATA_DIR").is_some();
    let mut app = tauri::Builder::default();
    if !isolated {
        app = app.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            windows::show_main(app);
        }));
    }
    let app = app
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_nspanel::init())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            let handle = app.handle().clone();
            let services = start(&handle)?;
            app.manage(services);
            Ok(())
        })
        .build(tauri::generate_context!());

    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("error while building the app: {error}");
            std::process::exit(1);
        }
    };

    app.run(|handle, event| match event {
        // Closing the main window hides it; Murmur keeps running in the menu bar (M14).
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        RunEvent::Exit => shutdown(handle),
        _ => {}
    });
}

/// Builds every service. An error here stops the app with a message (it cannot work without them).
fn start(handle: &AppHandle) -> Result<Arc<App>, Box<dyn std::error::Error>> {
    let paths = Paths::resolve(handle)?;
    init_logging(&paths);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        engine = engine::ENGINE_NAME,
        "Murmur starting"
    );

    let loaded = settings_file::load(&paths.settings);
    if loaded.recovered {
        tracing::warn!(
            "settings file was unreadable; started with defaults (old file kept as .bak)"
        );
    }
    let store = Store::open(&paths.db)?;
    history::recover_spool(&store, &paths.spool);

    let inference = Inference::spawn(engine::create_engine, engine::create_vad);
    let services = Arc::new(App::new(paths, loaded.settings, store, inference));

    windows::setup(handle)?;
    let controller = controller::spawn(handle.clone(), services.clone());
    services.set_controller(controller.clone());

    let hotkeys = Hotkeys::new(
        handle,
        hotkey_callback(controller.clone(), services.clone()),
    );
    services.set_hotkeys(hotkeys);
    settings_apply::apply_at_start(handle, &services);

    let tray_services = services.clone();
    let tray_handle = handle.clone();
    tray::create(
        handle,
        &tray_model(&services),
        Arc::new(move |action| on_tray(&tray_handle, &tray_services, action)),
    )?;

    if !services.settings().onboarding_completed {
        windows::show_main(handle);
    }
    spawn_idle_unload(services.clone());
    #[cfg(debug_assertions)]
    debug::install(controller);
    Ok(services)
}

fn hotkey_callback(
    controller: ControllerHandle,
    services: Arc<App>,
) -> Arc<dyn Fn(HotkeyEvent) + Send + Sync> {
    Arc::new(move |event| match event {
        HotkeyEvent::MainDown => controller.event(Event::HotkeyDown {
            mode: services.settings().record_mode,
        }),
        HotkeyEvent::MainUp | HotkeyEvent::AlternateUp => controller.event(Event::HotkeyUp),
        HotkeyEvent::AlternateDown => {
            let other = match services.settings().language {
                Language::Ja => Language::En,
                Language::En => Language::Ja,
            };
            controller.send(Msg::HotkeyDownWithLanguage(other));
        }
        HotkeyEvent::Esc => controller.event(Event::Esc),
    })
}

/// The tray menu as it should look now.
pub fn tray_model(services: &App) -> TrayModel {
    let settings = services.settings();
    let recent = services
        .store()
        .list("", 5, 0)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| Some((row.id, row.text?)))
                .collect()
        })
        .unwrap_or_default();
    let ui_ja = match settings.ui_language {
        murmur_core::settings::UiLanguage::Ja => true,
        murmur_core::settings::UiLanguage::En => false,
        murmur_core::settings::UiLanguage::System => sys_locale_is_ja(),
    };
    TrayModel {
        recording: !matches!(
            services.pill_state().view,
            PillView::Hidden | PillView::Notice { .. }
        ),
        language: settings.language,
        recent,
        ui_ja,
    }
}

/// Rebuilds the tray menu (after settings or history changes).
pub fn refresh_tray(app: &AppHandle, services: &Arc<App>) {
    // The history read runs on a blocking thread, never on the main thread.
    let app = app.clone();
    let services = services.clone();
    tauri::async_runtime::spawn_blocking(move || tray::update(&app, &tray_model(&services)));
}

fn sys_locale_is_ja() -> bool {
    sys_locale::get_locale().is_some_and(|locale| locale.starts_with("ja"))
}

fn on_tray(app: &AppHandle, services: &Arc<App>, action: TrayAction) {
    match action {
        TrayAction::OpenMain => windows::show_main(app),
        TrayAction::ToggleRecording => {
            if let Some(controller) = services.controller() {
                controller.event(Event::HotkeyDown {
                    mode: murmur_core::settings::RecordMode::Toggle,
                });
            }
        }
        TrayAction::SetLanguage(language) => {
            let (app, services) = (app.clone(), services.clone());
            tauri::async_runtime::spawn_blocking(move || {
                let mut settings = services.settings();
                settings.language = language;
                if let Err(error) = settings_apply::update(&app, &services, settings) {
                    tracing::warn!(%error, "language change from the tray failed");
                }
            });
        }
        TrayAction::CopyLast => {
            let services = services.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Some(text) = services.last_text().or_else(|| last_saved_text(&services)) {
                    copy_text_now(&text);
                }
            });
        }
        TrayAction::CopyRecent(id) => {
            let services = services.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Ok(Some(row)) = services.store().get(&id)
                    && let Some(text) = row.text
                {
                    copy_text_now(&text);
                }
            });
        }
        TrayAction::CheckUpdates => {
            windows::show_main(app);
            let _ = ipc::Navigate {
                view: "settings".into(),
            }
            .emit(app);
        }
        TrayAction::Quit => app.exit(0),
    }
}

fn last_saved_text(services: &App) -> Option<String> {
    services
        .store()
        .list("", 1, 0)
        .ok()?
        .into_iter()
        .next()?
        .text
}

fn copy_text_now(text: &str) {
    if let Err(error) = adapters::output::copy_text(text) {
        tracing::warn!(%error, "copy failed");
    }
}

/// S5: frees the model after the configured idle time. Off by default (speed first).
fn spawn_idle_unload(services: Arc<App>) {
    tauri::async_runtime::spawn(async move {
        let mut idle_minutes = 0u32;
        loop {
            tokio::time::sleep(IDLE_CHECK).await;
            let busy = !matches!(
                services.pill_state().view,
                PillView::Hidden | PillView::Notice { .. }
            );
            idle_minutes = if busy {
                0
            } else {
                idle_minutes.saturating_add(1)
            };
            if let Some(limit) = services.settings().unload_model_after_min
                && idle_minutes >= limit
                && services.inference().loaded_model().is_some()
            {
                tracing::info!(idle_minutes, "unloading the model after idle time");
                services.inference().unload();
            }
        }
    });
}

fn shutdown(handle: &AppHandle) {
    if let Some(services) = handle.try_state::<Arc<App>>() {
        if let Some(controller) = services.controller() {
            controller.send(Msg::Shutdown);
        }
        services.inference().shutdown(Duration::from_secs(10));
    }
}

/// Logs to `~/Library/Logs/io.github.naoya5.murmur/`, one file per day, 7 days kept.
/// Transcript text is never logged (G7).
fn init_logging(paths: &Paths) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("murmur")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&paths.logs);
    let filter = tracing_subscriber::EnvFilter::try_from_env("MURMUR_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file_layer = file.ok().map(|appender| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(appender)
    });
    let stderr_layer = cfg!(debug_assertions)
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
    let result = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
    if result.is_err() {
        eprintln!("logging was already initialized");
    }
}
