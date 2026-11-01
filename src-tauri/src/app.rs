//! Services shared by the controller and the IPC commands. Assembly only: no business rules.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use murmur_core::Language;
use murmur_core::catalog;
use murmur_core::ports::ModelSpec;
use murmur_core::session::PillView;
use murmur_core::settings::Settings;
use tauri::{AppHandle, Manager};

use crate::adapters::hotkeys::Hotkeys;
use crate::adapters::models::ModelStore;
use crate::adapters::store::Store;
use crate::controller::{ControllerHandle, PillState};
use crate::inference::Inference;

/// Where Murmur keeps its data.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub models: PathBuf,
    pub spool: PathBuf,
    pub db: PathBuf,
    pub settings: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    pub fn resolve(app: &AppHandle) -> Result<Paths, String> {
        // Debug builds may run on a scratch data dir (fresh onboarding tests).
        let override_dir = cfg!(debug_assertions)
            .then(|| std::env::var_os("MURMUR_DATA_DIR"))
            .flatten()
            .map(PathBuf::from);
        // An isolated debug instance keeps its logs with its data, so parallel runs stay apart.
        let logs = match &override_dir {
            Some(dir) => dir.join("logs"),
            None => app.path().app_log_dir().map_err(|e| e.to_string())?,
        };
        let data = match override_dir {
            Some(dir) => dir,
            None => app.path().app_data_dir().map_err(|e| e.to_string())?,
        };
        let paths = Paths {
            models: data.join("models"),
            spool: data.join("spool"),
            db: data.join("murmur.db"),
            settings: data.join("settings.json"),
            data,
            logs,
        };
        for dir in [&paths.data, &paths.models, &paths.spool, &paths.logs] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        Ok(paths)
    }
}

pub struct App {
    paths: Paths,
    settings: RwLock<Settings>,
    pub(crate) settings_update: Mutex<()>,
    store: Arc<Store>,
    models: ModelStore,
    inference: Inference,
    hotkeys: OnceLock<Hotkeys>,
    controller: OnceLock<ControllerHandle>,
    /// The last delivered text, for "copy last result" (S6). Memory only.
    last_text: Mutex<Option<String>>,
    /// What the pill shows now; the pill reads it when its page loads.
    pill_state: Mutex<PillState>,
}

impl App {
    pub fn new(paths: Paths, settings: Settings, store: Store, inference: Inference) -> App {
        let models = ModelStore::new(paths.models.clone());
        let pill_state = PillState {
            view: PillView::Hidden,
            language: settings.language,
        };
        App {
            paths,
            settings: RwLock::new(settings),
            settings_update: Mutex::new(()),
            store: Arc::new(store),
            models,
            inference,
            hotkeys: OnceLock::new(),
            controller: OnceLock::new(),
            last_text: Mutex::new(None),
            pill_state: Mutex::new(pill_state),
        }
    }

    /// A snapshot of the settings. Read right before use, so changes apply at once.
    pub fn settings(&self) -> Settings {
        match self.settings.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Replaces the settings. Only `settings_apply` calls this.
    pub fn replace_settings(&self, settings: Settings) {
        match self.settings.write() {
            Ok(mut guard) => *guard = settings,
            Err(poisoned) => *poisoned.into_inner() = settings,
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn models(&self) -> &ModelStore {
        &self.models
    }

    pub fn inference(&self) -> &Inference {
        &self.inference
    }

    pub fn set_hotkeys(&self, hotkeys: Hotkeys) {
        let _ = self.hotkeys.set(hotkeys);
    }

    pub fn hotkeys(&self) -> Option<&Hotkeys> {
        self.hotkeys.get()
    }

    pub fn set_controller(&self, controller: ControllerHandle) {
        let _ = self.controller.set(controller);
    }

    pub fn controller(&self) -> Option<&ControllerHandle> {
        self.controller.get()
    }

    /// The installed recognition model chosen for `language` and the installed VAD.
    pub fn model_specs(&self, language: Language) -> Option<(ModelSpec, ModelSpec)> {
        let settings = self.settings();
        let model = self.models.spec(settings.models.for_language(language))?;
        let vad = self.models.spec(catalog::VAD_MODEL)?;
        Some((model, vad))
    }

    /// Loads the model for `language` in the background if it is installed.
    pub fn ensure_model_loaded(&self, language: Language) {
        if let Some((model, vad)) = self.model_specs(language)
            && self.inference.loaded_model() != Some(model.entry.id)
        {
            self.inference.preload(model, vad);
        }
    }

    pub fn remember_last(&self, text: String) {
        if let Ok(mut guard) = self.last_text.lock() {
            *guard = Some(text);
        }
    }

    pub fn last_text(&self) -> Option<String> {
        self.last_text.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn set_pill_state(&self, state: PillState) {
        if let Ok(mut guard) = self.pill_state.lock() {
            *guard = state;
        }
    }

    pub fn pill_state(&self) -> PillState {
        match self.pill_state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}
