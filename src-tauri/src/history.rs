//! History bookkeeping around a dictation: saving after delivery (G9), keeping failed audio,
//! retrying, recovering spool files after a crash (M8), and retention.
//!
//! Everything here runs on blocking threads after the paste; nothing here delays delivery.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use murmur_core::Language;
use murmur_core::metrics::{self, Counts};
use murmur_core::session::FailureKind;
use murmur_core::settings::HistoryRetention;
use murmur_core::text::{self, FormatOptions};
use tauri::AppHandle;
use tauri_specta::Event as _;

use crate::adapters::audio;
use crate::adapters::store::{self, Store, TranscriptRow, TranscriptStatus};
use crate::app::App;
use crate::controller::HistoryChanged;
use crate::inference::{InferenceError, TranscribeRequest};

/// What the controller knows about a finished session.
pub struct Record {
    pub language: Language,
    pub text: Option<String>,
    pub raw_text: Option<String>,
    pub duration_ms: u64,
    pub model_id: Option<&'static str>,
    pub latency_ms: Option<u64>,
    pub asr_ms: Option<u64>,
    pub failure: Option<FailureKind>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn notify(app: &AppHandle) {
    if let Err(error) = HistoryChanged.emit(app) {
        tracing::warn!(%error, "history-changed emit failed");
    }
}

fn remove_quietly(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "could not delete an audio file");
    }
}

pub fn failure_text(failure: FailureKind) -> &'static str {
    match failure {
        FailureKind::Timeout => "timeout",
        FailureKind::EmptyResult => "empty_result",
        FailureKind::ModelMissing => "model_missing",
        FailureKind::ModelLoadFailed => "model_load_failed",
        FailureKind::Engine => "engine",
        FailureKind::Audio => "audio",
    }
}

/// After a successful delivery: history row, day totals, spool delete, event.
pub fn save_success(app: AppHandle, store: Arc<Store>, record: Record, spool: PathBuf) {
    tauri::async_runtime::spawn_blocking(move || {
        let text = record.text.unwrap_or_default();
        let counts = metrics::count(&text);
        let created_at_ms = now_ms();
        let row = TranscriptRow {
            id: uuid::Uuid::new_v4().to_string(),
            created_at_ms,
            status: TranscriptStatus::Ok,
            text: Some(text),
            raw_text: record.raw_text,
            language: Some(record.language),
            duration_ms: record.duration_ms,
            latin_words: counts.latin_words,
            cjk_chars: counts.cjk_chars,
            model_id: record.model_id.map(str::to_owned),
            latency_ms: record.latency_ms,
            asr_ms: record.asr_ms,
            audio_path: None,
            error: None,
        };
        // Keep the audio if the row cannot be written: the spool recovery then brings it back.
        match store.insert(&row) {
            Ok(()) => remove_quietly(&spool),
            Err(error) => tracing::error!(%error, "could not save a transcript"),
        }
        let day = store::local_day(created_at_ms);
        if let Err(error) = store.add_day_totals(day, record.language, counts, record.duration_ms) {
            tracing::error!(%error, "could not update day totals");
        }
        notify(&app);
    });
}

/// After a failure: a failed row that keeps the audio for a retry.
pub fn save_failure(app: AppHandle, store: Arc<Store>, record: Record, spool: PathBuf) {
    tauri::async_runtime::spawn_blocking(move || {
        let audio_path = spool.exists().then(|| spool.to_string_lossy().into_owned());
        let row = TranscriptRow {
            id: uuid::Uuid::new_v4().to_string(),
            created_at_ms: now_ms(),
            status: TranscriptStatus::Failed,
            text: None,
            raw_text: record.raw_text,
            language: Some(record.language),
            duration_ms: record.duration_ms,
            latin_words: 0,
            cjk_chars: 0,
            model_id: record.model_id.map(str::to_owned),
            latency_ms: None,
            asr_ms: record.asr_ms,
            audio_path,
            error: record.failure.map(|f| failure_text(f).to_owned()),
        };
        if let Err(error) = store.insert(&row) {
            tracing::error!(%error, "could not save a failed transcript");
        }
        notify(&app);
    });
}

#[derive(Debug, thiserror::Error, serde::Serialize, specta::Type)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum RetryError {
    #[error("not found")]
    NotFound,
    #[error("the audio file is gone")]
    AudioMissing,
    #[error("the model for this language is not downloaded")]
    ModelMissing,
    #[error("no speech was found")]
    NoSpeech,
    #[error("{0}")]
    Failed(String),
}

/// Transcribes a failed or recovered row again from its audio file.
pub async fn retry(
    app: AppHandle,
    services: Arc<App>,
    id: String,
) -> Result<TranscriptRow, RetryError> {
    let store = services.store().clone();
    let lookup_id = id.clone();
    let row = tauri::async_runtime::spawn_blocking(move || store.get(&lookup_id))
        .await
        .map_err(|e| RetryError::Failed(e.to_string()))?
        .map_err(|e| RetryError::Failed(e.to_string()))?
        .ok_or(RetryError::NotFound)?;
    let audio_path = row.audio_path.clone().ok_or(RetryError::AudioMissing)?;
    let path = PathBuf::from(&audio_path);
    let pcm = tauri::async_runtime::spawn_blocking(move || audio::read_spool(&path))
        .await
        .map_err(|e| RetryError::Failed(e.to_string()))?
        .map_err(|_| RetryError::AudioMissing)?;
    let settings = services.settings();
    let language = row.language.unwrap_or(settings.language);
    let (model, vad) = services
        .model_specs(language)
        .ok_or(RetryError::ModelMissing)?;
    let vocabulary = settings
        .dictionary
        .iter()
        .filter(|e| e.scope.includes(language))
        .cloned()
        .collect();
    let duration_ms = (pcm.len() as u64 * 1000) / u64::from(murmur_core::SAMPLE_RATE_HZ);
    let request = TranscribeRequest {
        pcm: Arc::new(pcm),
        language,
        model,
        vad,
        vocabulary,
        cancel: murmur_core::ports::CancelToken::new(),
    };
    let output = services
        .inference()
        .transcribe(request)
        .await
        .map_err(|e| match e {
            InferenceError::ModelMissing => RetryError::ModelMissing,
            other => RetryError::Failed(other.to_string()),
        })?;
    let raw = output.raw.ok_or(RetryError::NoSpeech)?;
    let opts = FormatOptions {
        remove_fillers: settings.remove_fillers,
        add_final_punctuation: settings.add_final_punctuation,
        apply_dictionary: !output.custom_vocabulary,
    };
    let formatted = text::format(&raw.text, language, &opts, &settings.dictionary);
    if formatted.is_empty() {
        return Err(RetryError::NoSpeech);
    }
    let counts = metrics::count(&formatted);
    let updated = TranscriptRow {
        status: TranscriptStatus::Ok,
        text: Some(formatted),
        raw_text: Some(raw.text),
        language: Some(language),
        duration_ms: if row.duration_ms == 0 {
            duration_ms
        } else {
            row.duration_ms
        },
        latin_words: counts.latin_words,
        cjk_chars: counts.cjk_chars,
        model_id: Some(output.model_id.to_owned()),
        asr_ms: Some(output.asr_ms),
        audio_path: None,
        error: None,
        ..row
    };
    let store = services.store().clone();
    let saved = updated.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        store
            .complete_retry(&id, &saved)
            .map_err(|e| e.to_string())?;
        let day = store::local_day(saved.created_at_ms);
        store
            .add_day_totals(
                day,
                language,
                Counts {
                    latin_words: saved.latin_words,
                    cjk_chars: saved.cjk_chars,
                },
                saved.duration_ms,
            )
            .map_err(|e| e.to_string())?;
        remove_quietly(Path::new(&audio_path));
        Ok(())
    })
    .await
    .map_err(|e| RetryError::Failed(e.to_string()))?
    .map_err(RetryError::Failed)?;
    notify(&app);
    Ok(updated)
}

/// On start: every spool file that no history row references becomes a "recovered" row (M8).
pub fn recover_spool(store: &Store, spool_dir: &Path) -> usize {
    let known = match store.audio_paths() {
        Ok(paths) => paths,
        Err(error) => {
            tracing::error!(%error, "spool recovery skipped: could not read the history");
            return 0;
        }
    };
    let Ok(entries) = std::fs::read_dir(spool_dir) else {
        return 0;
    };
    let mut recovered = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pcm") {
            continue;
        }
        let path_text = path.to_string_lossy().into_owned();
        if known.contains(&path_text) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        // Less than 0.25 s of audio is not worth a history row.
        let samples = meta.len() / 2;
        if samples < u64::from(murmur_core::SAMPLE_RATE_HZ) / 4 {
            remove_quietly(&path);
            continue;
        }
        let created_at_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or_else(now_ms, |d| d.as_millis() as i64);
        let row = TranscriptRow {
            id: uuid::Uuid::new_v4().to_string(),
            created_at_ms,
            status: TranscriptStatus::Recovered,
            text: None,
            raw_text: None,
            language: None,
            duration_ms: samples * 1000 / u64::from(murmur_core::SAMPLE_RATE_HZ),
            latin_words: 0,
            cjk_chars: 0,
            model_id: None,
            latency_ms: None,
            asr_ms: None,
            audio_path: Some(path_text),
            error: None,
        };
        match store.insert(&row) {
            Ok(()) => recovered += 1,
            Err(error) => tracing::error!(%error, "could not record a recovered recording"),
        }
    }
    if recovered > 0 {
        tracing::info!(recovered, "recovered recordings from the spool");
    }
    recovered
}

/// Deletes successful rows older than the retention (failed and recovered rows keep their audio).
pub fn apply_retention(store: &Store, retention: HistoryRetention) {
    let HistoryRetention::Days { days } = retention else {
        return;
    };
    let cutoff = now_ms() - i64::from(days) * 86_400_000;
    match store.purge_before(cutoff) {
        Ok(paths) => paths.iter().for_each(|p| remove_quietly(Path::new(p))),
        Err(error) => tracing::error!(%error, "retention purge failed"),
    }
}
