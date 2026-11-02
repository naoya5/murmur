//! The controller: runs the session state machine and executes its effects.
//!
//! It owns `murmur_core::session::Machine`. Every input (hotkeys, pill clicks, timers, adapter
//! results) arrives on one channel as a `Msg`, becomes a core `Event`, goes through
//! `Machine::step`, and the returned effects are executed here by calling the adapters. Slow work
//! runs on spawned tasks that report back with another `Msg`, so the loop never blocks.
//!
//! Rules kept here: the language is fixed per recording (read at start); permissions are checked
//! right before use (G8); every wait has a timeout (G5); history and stats are written only after
//! delivery (G9); transcript text never reaches the log (G7).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use murmur_core::Language;
use murmur_core::ports::{CancelToken, RawTranscript};
use murmur_core::session::{
    self, CaptureError, DeliveryOutcome, Effect, Event, FailureKind, Machine, PillView, SessionId,
    SessionKind, TranscribeOutcome, TranscriptText,
};
use murmur_core::text::{self, FormatOptions};
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event as _;
use tokio::sync::{mpsc, watch};

use crate::adapters::audio::{self, AudioConfig, AudioError, AudioEvent, CapturedAudio, Recorder};
use crate::adapters::output::{self, DeliverOptions};
use crate::adapters::sounds::{self, Cue};
use crate::app::App;
use crate::history::{self, Record};
use crate::inference::{
    InferenceError, InferenceOutput, StreamId, StreamRequest, TranscribeRequest,
};
use crate::platform::macos::{self, Permission};

/// Paste must finish within this time; the output adapter has its own 1 s limit.
const DELIVER_TIMEOUT: Duration = Duration::from_millis(1_500);
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// Covers a pending open (3.5 s), owner close (3 s), and converter close (3 s).
const CAPTURE_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// The microphone permission prompt waits for the user; give them time, but not forever.
const PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// What the pill window receives after every step.
#[derive(Debug, Clone, Serialize, Type, tauri_specta::Event)]
pub struct PillState {
    pub view: PillView,
    pub language: Language,
    /// Present while the session is a screen annotation (absent: dictation or idle).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub annotation: Option<AnnotationPill>,
}

/// The annotation part of the pill: a camera mark, the pictures kept so far, a warning when the
/// screen capture stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct AnnotationPill {
    /// Candidate pictures kept so far while recording; the keyframe count once chosen.
    pub images: u32,
    /// The screen capture failed; the recording goes on with the pictures taken before.
    pub screen_lost: bool,
}

/// Audio level for the pill's waveform, about 30 per second while recording.
#[derive(Debug, Clone, Copy, Serialize, Type, tauri_specta::Event)]
pub struct AudioLevel {
    pub level: f32,
}

/// The history changed (insert, update, delete). The UI reloads what it shows.
#[derive(Debug, Clone, Copy, Serialize, Type, tauri_specta::Event)]
pub struct HistoryChanged;

/// Messages into the controller loop.
pub enum Msg {
    /// A core event from hotkeys, the pill, timers or debug triggers.
    Core(Event),
    /// The main hotkey with a language override (alternate hotkey).
    HotkeyDownWithLanguage(Language),
    CaptureOpened {
        session: SessionId,
        result: Result<Recorder, CaptureError>,
    },
    AudioReady {
        session: SessionId,
        result: Result<CapturedMeta, String>,
    },
    Inferred {
        session: SessionId,
        result: Result<InferenceOutput, InferenceError>,
    },
    Shutdown,
}

/// Facts about the captured audio that the history row needs.
#[derive(Debug, Clone)]
pub struct CapturedMeta {
    pub duration_ms: u64,
    pub spool_path: PathBuf,
}

/// The stopped recording: its facts and, unless stopping failed, the PCM in memory.
#[derive(Clone)]
pub struct Captured {
    meta: CapturedMeta,
    pcm: Option<Arc<Vec<f32>>>,
}

/// A cheap, cloneable way to talk to the controller.
#[derive(Clone)]
pub struct ControllerHandle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl ControllerHandle {
    pub fn send(&self, msg: Msg) {
        // The loop only stops at shutdown; a failed send then is expected and harmless.
        let _ = self.tx.send(msg);
    }

    pub fn event(&self, event: Event) {
        self.send(Msg::Core(event));
    }
}

/// Per-session data that the pure machine does not hold.
struct SessionCtx {
    language: Language,
    spool_path: PathBuf,
    t_hotkey: Instant,
    t_stop: Option<Instant>,
    recorder: Option<Recorder>,
    paused: bool,
    stop_requested: bool,
    /// Resolves once the recorder has stopped and the spool is complete.
    audio_tx: Option<watch::Sender<Option<Captured>>>,
    audio_rx: watch::Receiver<Option<Captured>>,
    meta: Option<CapturedMeta>,
    cancel: CancelToken,
    /// The streaming transcription running alongside the recording, when the model and
    /// the VAD are installed.
    stream: Option<StreamId>,
    raw: Option<RawTranscript>,
    text: Option<String>,
    asr_ms: Option<u64>,
    model_id: Option<&'static str>,
    latency_ms: Option<u64>,
}

pub struct Controller {
    app: AppHandle,
    services: Arc<App>,
    machine: Machine,
    origin: Instant,
    sessions: HashMap<SessionId, SessionCtx>,
    language_override: Option<Language>,
    last_language: Language,
    tx: mpsc::UnboundedSender<Msg>,
    pill_visible: bool,
}

/// Starts the controller loop on the async runtime and returns its handle.
pub fn spawn(app: AppHandle, services: Arc<App>) -> ControllerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let last_language = services.settings().language;
    let controller = Controller {
        app,
        services,
        machine: Machine::new(),
        origin: Instant::now(),
        sessions: HashMap::new(),
        language_override: None,
        last_language,
        tx: tx.clone(),
        pill_visible: false,
    };
    tauri::async_runtime::spawn(controller.run(rx));
    ControllerHandle { tx }
}

impl Controller {
    fn now(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            if matches!(msg, Msg::Shutdown) {
                self.shutdown();
                break;
            }
            if let Some(event) = self.translate(msg) {
                let now = self.now();
                let effects = self.machine.step(event, now);
                for effect in effects {
                    self.execute(effect);
                }
                // An alternate-hotkey press the machine ignored must not change a later recording.
                self.language_override = None;
                self.publish();
            }
        }
    }

    /// Turns a message into a core event, updating session context on the way.
    fn translate(&mut self, msg: Msg) -> Option<Event> {
        match msg {
            Msg::Core(event) => Some(event),
            Msg::HotkeyDownWithLanguage(language) => {
                self.language_override = Some(language);
                Some(Event::HotkeyDown {
                    mode: self.services.settings().record_mode,
                })
            }
            Msg::CaptureOpened { session, result } => self.on_capture_opened(session, result),
            Msg::AudioReady { session, result } => {
                if let Some(ctx) = self.sessions.get_mut(&session)
                    && let Ok(meta) = &result
                {
                    ctx.meta = Some(meta.clone());
                }
                None
            }
            Msg::Inferred { session, result } => self.on_inferred(session, result),
            Msg::Shutdown => None,
        }
    }

    fn on_capture_opened(
        &mut self,
        session: SessionId,
        result: Result<Recorder, CaptureError>,
    ) -> Option<Event> {
        let Some(ctx) = self.sessions.get_mut(&session) else {
            // The session ended while the microphone was opening: close it again.
            if let Ok(recorder) = result {
                tauri::async_runtime::spawn_blocking(move || {
                    if let Ok(audio) = recorder.stop() {
                        let _ = std::fs::remove_file(&audio.spool_path);
                    }
                });
            }
            return None;
        };
        match result {
            Ok(recorder) => {
                tracing::info!(
                    session = session.0,
                    open_ms = ctx.t_hotkey.elapsed().as_millis() as u64,
                    "recording started"
                );
                if ctx.paused {
                    recorder.set_paused(true);
                }
                ctx.recorder = Some(recorder);
                if ctx.stop_requested {
                    Self::stop_recorder(ctx, self.tx.clone(), session);
                }
                None
            }
            Err(error) => {
                tracing::warn!(
                    session = session.0,
                    ?error,
                    "the microphone could not start"
                );
                Some(Event::CaptureFailed { session, error })
            }
        }
    }

    fn on_inferred(
        &mut self,
        session: SessionId,
        result: Result<InferenceOutput, InferenceError>,
    ) -> Option<Event> {
        let settings = self.services.settings();
        let ctx = self.sessions.get_mut(&session)?;
        let outcome = match result {
            Ok(output) => {
                ctx.asr_ms = Some(output.asr_ms);
                ctx.model_id = Some(output.model_id);
                match output.raw {
                    None => TranscribeOutcome::NoSpeech,
                    Some(raw) if raw.text.trim().is_empty() => {
                        ctx.raw = Some(raw);
                        TranscribeOutcome::Failed(FailureKind::EmptyResult)
                    }
                    Some(raw) => {
                        let model_has_vocabulary = output.custom_vocabulary;
                        let opts = FormatOptions {
                            remove_fillers: settings.remove_fillers,
                            add_final_punctuation: settings.add_final_punctuation,
                            apply_dictionary: !model_has_vocabulary,
                        };
                        let formatted =
                            text::format(&raw.text, ctx.language, &opts, &settings.dictionary);
                        ctx.raw = Some(raw);
                        if formatted.is_empty() {
                            TranscribeOutcome::Failed(FailureKind::EmptyResult)
                        } else {
                            ctx.text = Some(formatted.clone());
                            TranscribeOutcome::Text(TranscriptText(formatted))
                        }
                    }
                }
            }
            Err(InferenceError::Cancelled) => return None,
            Err(InferenceError::ModelMissing) => {
                TranscribeOutcome::Failed(FailureKind::ModelMissing)
            }
            Err(InferenceError::ModelLoad(error)) => {
                tracing::warn!(session = session.0, %error, "model load failed");
                TranscribeOutcome::Failed(FailureKind::ModelLoadFailed)
            }
            Err(InferenceError::Audio(error)) => {
                tracing::warn!(session = session.0, %error, "audio unavailable");
                TranscribeOutcome::Failed(FailureKind::Audio)
            }
            Err(error) => {
                tracing::warn!(session = session.0, %error, "transcription failed");
                TranscribeOutcome::Failed(FailureKind::Engine)
            }
        };
        Some(Event::Transcribed { session, outcome })
    }

    fn execute(&mut self, effect: Effect) {
        self.cue(&effect);
        match effect {
            Effect::StartCapture { session } => self.start_capture(session),
            Effect::PauseCapture { session } => self.set_paused(session, true),
            Effect::ResumeCapture { session } => self.set_paused(session, false),
            Effect::StopCapture { session } => {
                if let Some(ctx) = self.sessions.get_mut(&session) {
                    ctx.t_stop = Some(Instant::now());
                    Self::stop_recorder(ctx, self.tx.clone(), session);
                }
            }
            Effect::Transcribe { session } => self.transcribe(session),
            Effect::CancelTranscription { session } => {
                if let Some(ctx) = self.sessions.get(&session) {
                    ctx.cancel.cancel();
                    self.cancel_stream(ctx);
                }
            }
            Effect::Deliver { session, text } => self.deliver(session, text),
            Effect::SaveSuccess { session, outcome } => self.save_success(session, outcome),
            Effect::SaveFailure { session, failure } => self.save_failure(session, failure),
            Effect::DropAudio { session } => self.drop_audio(session),
            Effect::SetEscCapture(on) => {
                if let Some(Err(error)) = self.services.hotkeys().map(|h| h.set_esc(on)) {
                    tracing::warn!(%error, on, "esc hotkey registration failed");
                }
            }
            Effect::ArmTimer { id, after_ms } => {
                let tx = self.tx.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(after_ms)).await;
                    let _ = tx.send(Msg::Core(Event::TimerFired(id)));
                });
            }
            // Annotation effects: nothing sends `Event::AnnotationsHotkey` yet, so these arms
            // are never reached. The platform ticket gives them their adapters.
            Effect::StartScreen { .. }
            | Effect::PauseScreen { .. }
            | Effect::ResumeScreen { .. }
            | Effect::StopScreen { .. }
            | Effect::ShowOverlay
            | Effect::HideOverlay
            | Effect::PlanKeyframes { .. }
            | Effect::TranscribeSegments { .. }
            | Effect::Compose { .. }
            | Effect::DeliverBundle { .. }
            | Effect::DeliverText { .. }
            | Effect::SaveAnnotation { .. }
            | Effect::DropAll { .. } => {}
        }
    }

    /// Start and stop sounds, read from settings at the moment they play.
    fn cue(&self, effect: &Effect) {
        let cue = match effect {
            Effect::StartCapture { .. } => Cue::Start,
            Effect::Transcribe { .. } => Cue::Stop,
            Effect::DropAudio { .. } => Cue::Cancel,
            Effect::SaveFailure { .. } => Cue::Error,
            _ => return,
        };
        if self.services.settings().sounds {
            sounds::play(cue);
        }
    }

    fn start_capture(&mut self, session: SessionId) {
        let settings = self.services.settings();
        let language = self.language_override.take().unwrap_or(settings.language);
        self.last_language = language;
        let spool_path = self
            .services
            .paths()
            .spool
            .join(format!("{}.pcm", uuid::Uuid::new_v4()));
        let (audio_tx, audio_rx) = watch::channel(None);
        let cancel = CancelToken::new();
        // Transcribe closed chunks while recording, when the model and the VAD are installed.
        // The dictionary is read here, once per recording, so every chunk uses the same words.
        let stream = self.services.model_specs(language).map(|(model, vad)| {
            // Warm before the stream starts, overlapping the cold engine work with the user's speech.
            self.services.inference().warm(model.clone(), vad.clone());
            let id = StreamId(session.0);
            let vocabulary = settings
                .dictionary
                .iter()
                .filter(|e| e.scope.includes(language))
                .cloned()
                .collect();
            self.services.inference().stream_start(
                id,
                StreamRequest {
                    language,
                    model,
                    vad,
                    vocabulary,
                    cancel: cancel.clone(),
                },
            );
            id
        });
        let tap = stream.map(|id| self.services.inference().stream_tap(id));
        self.sessions.insert(
            session,
            SessionCtx {
                language,
                spool_path: spool_path.clone(),
                t_hotkey: Instant::now(),
                t_stop: None,
                recorder: None,
                paused: false,
                stop_requested: false,
                audio_tx: Some(audio_tx),
                audio_rx,
                meta: None,
                cancel,
                stream,
                raw: None,
                text: None,
                asr_ms: None,
                model_id: None,
                latency_ms: None,
            },
        );
        // The model may still be loading; recording starts anyway.
        if stream.is_none() {
            self.services.ensure_model_loaded(language);
        }

        let tx = self.tx.clone();
        let app = self.app.clone();
        let device = settings.microphone.clone();
        tauri::async_runtime::spawn(async move {
            let result = open_microphone(app, tx.clone(), session, device, spool_path, tap).await;
            let _ = tx.send(Msg::CaptureOpened { session, result });
        });
    }

    /// Drops the partial work of the session's stream, if it has one.
    fn cancel_stream(&self, ctx: &SessionCtx) {
        if let Some(id) = ctx.stream {
            self.services.inference().stream_cancel(id);
        }
    }

    fn set_paused(&mut self, session: SessionId, paused: bool) {
        if let Some(ctx) = self.sessions.get_mut(&session) {
            ctx.paused = paused;
            if let Some(recorder) = &ctx.recorder {
                recorder.set_paused(paused);
            }
        }
    }

    /// Stops the recorder on a blocking thread and publishes the audio to `audio_rx`.
    fn stop_recorder(ctx: &mut SessionCtx, tx: mpsc::UnboundedSender<Msg>, session: SessionId) {
        let Some(recorder) = ctx.recorder.take() else {
            // Still opening: stop as soon as it opens (`on_capture_opened`).
            ctx.stop_requested = true;
            return;
        };
        let Some(audio_tx) = ctx.audio_tx.take() else {
            return;
        };
        let spool_path = ctx.spool_path.clone();
        tauri::async_runtime::spawn(async move {
            let stopped = tauri::async_runtime::spawn_blocking(move || recorder.stop()).await;
            let captured = match stopped {
                Ok(Ok(CapturedAudio {
                    pcm,
                    spool_path,
                    duration_ms,
                })) => Captured {
                    meta: CapturedMeta {
                        duration_ms,
                        spool_path,
                    },
                    pcm: Some(Arc::new(pcm)),
                },
                Ok(Err(error)) => {
                    tracing::warn!(session = session.0, %error, "stopping the recorder failed");
                    // Whatever reached the spool is still on disk; a retry can read it.
                    Captured {
                        meta: CapturedMeta {
                            duration_ms: 0,
                            spool_path,
                        },
                        pcm: None,
                    }
                }
                Err(error) => {
                    tracing::warn!(session = session.0, %error, "the stop task failed");
                    Captured {
                        meta: CapturedMeta {
                            duration_ms: 0,
                            spool_path,
                        },
                        pcm: None,
                    }
                }
            };
            let _ = tx.send(Msg::AudioReady {
                session,
                result: Ok(captured.meta.clone()),
            });
            let _ = audio_tx.send(Some(captured));
        });
    }

    fn transcribe(&mut self, session: SessionId) {
        let Some(ctx) = self.sessions.get(&session) else {
            return;
        };
        let mut audio_rx = ctx.audio_rx.clone();
        let language = ctx.language;
        let cancel = ctx.cancel.clone();
        let stream = ctx.stream;
        let services = self.services.clone();
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
            // Wait for the recorder to finish (bounded: stopping is a flush and a join).
            let captured: Option<Captured> = {
                let ready =
                    tokio::time::timeout(CAPTURE_READY_TIMEOUT, audio_rx.wait_for(Option::is_some))
                        .await;
                match ready {
                    Ok(Ok(guard)) => guard.clone(),
                    _ => None,
                }
            };
            let pcm = match captured {
                Some(Captured { pcm: Some(pcm), .. }) => Some(pcm),
                // Stopping failed: read back what reached the spool file.
                Some(Captured { pcm: None, meta }) => {
                    let path = meta.spool_path.clone();
                    tauri::async_runtime::spawn_blocking(move || audio::read_spool(&path))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .map(Arc::new)
                }
                None => None,
            };
            let result = match pcm {
                Some(pcm) => {
                    let settings = services.settings();
                    let vocabulary = settings
                        .dictionary
                        .iter()
                        .filter(|e| e.scope.includes(language))
                        .cloned()
                        .collect();
                    match services.model_specs(language) {
                        Some((model, vad)) => {
                            let loading =
                                services.inference().loaded_model() != Some(model.entry.id);
                            if loading {
                                let _ = tx.send(Msg::Core(Event::ModelLoading { session }));
                            }
                            let budget = backstop_timeout(pcm.len(), loading);
                            let request = TranscribeRequest {
                                pcm,
                                language,
                                model,
                                vad,
                                vocabulary,
                                cancel: cancel.clone(),
                            };
                            let inference = services.inference().clone();
                            let job = async move {
                                match stream {
                                    // Only the tail is left to transcribe.
                                    Some(id) => inference.stream_finish(id, request).await,
                                    None => inference.transcribe(request).await,
                                }
                            };
                            match tokio::time::timeout(budget, job).await {
                                Ok(result) => result,
                                Err(_) => {
                                    // The machine has timed out already; free the inference thread.
                                    cancel.cancel();
                                    Err(InferenceError::Cancelled)
                                }
                            }
                        }
                        None => Err(InferenceError::ModelMissing),
                    }
                }
                None => Err(InferenceError::Audio("captured audio unavailable".into())),
            };
            let _ = tx.send(Msg::Inferred { session, result });
        });
    }

    fn deliver(&mut self, session: SessionId, text: TranscriptText) {
        let settings = self.services.settings();
        let opts = DeliverOptions {
            auto_paste: settings.auto_paste,
            restore_clipboard: settings.restore_clipboard,
        };
        let t_stop = self.sessions.get(&session).and_then(|c| c.t_stop);
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
            let job = tauri::async_runtime::spawn_blocking(move || output::deliver(&text.0, opts));
            let outcome = match tokio::time::timeout(DELIVER_TIMEOUT, job).await {
                Ok(Ok(outcome)) => outcome,
                _ => DeliveryOutcome::Unconfirmed,
            };
            if let Some(t_stop) = t_stop {
                tracing::info!(
                    session = session.0,
                    latency_ms = t_stop.elapsed().as_millis() as u64,
                    ?outcome,
                    "delivered"
                );
            }
            let _ = tx.send(Msg::Core(Event::Delivered { session, outcome }));
        });
        if let (Some(ctx), Some(t_stop)) = (self.sessions.get_mut(&session), t_stop) {
            // Delivery is dispatched; the clipboard write is the first thing it does.
            ctx.latency_ms = Some(t_stop.elapsed().as_millis() as u64);
        }
    }

    fn record(ctx: &SessionCtx, failure: Option<FailureKind>) -> Record {
        Record {
            language: ctx.language,
            text: ctx.text.clone(),
            raw_text: ctx.raw.as_ref().map(|r| r.text.clone()),
            duration_ms: ctx.meta.as_ref().map_or(0, |m| m.duration_ms),
            model_id: ctx.model_id,
            latency_ms: ctx.latency_ms,
            asr_ms: ctx.asr_ms,
            failure,
        }
    }

    fn save_success(&mut self, session: SessionId, outcome: DeliveryOutcome) {
        let Some(ctx) = self.sessions.remove(&session) else {
            return;
        };
        tracing::info!(session = session.0, ?outcome, "session saved");
        if let Some(text) = &ctx.text {
            self.services.remember_last(text.clone());
        }
        let record = Self::record(&ctx, None);
        history::save_success(
            self.app.clone(),
            self.services.store().clone(),
            record,
            ctx.spool_path,
        );
    }

    fn save_failure(&mut self, session: SessionId, failure: FailureKind) {
        let Some(ctx) = self.sessions.remove(&session) else {
            return;
        };
        self.cancel_stream(&ctx);
        let record = Self::record(&ctx, Some(failure));
        let app = self.app.clone();
        let store = self.services.store().clone();
        let mut rx = ctx.audio_rx;
        let spool = ctx.spool_path;
        tauri::async_runtime::spawn(async move {
            // The audio must be complete on disk before the row points at it.
            let _ = tokio::time::timeout(OPEN_TIMEOUT, rx.wait_for(Option::is_some)).await;
            history::save_failure(app, store, record, spool);
        });
    }

    fn drop_audio(&mut self, session: SessionId) {
        let Some(mut ctx) = self.sessions.remove(&session) else {
            return;
        };
        self.cancel_stream(&ctx);
        if ctx.recorder.is_some() || ctx.audio_tx.is_some() {
            Self::stop_recorder(&mut ctx, self.tx.clone(), session);
        }
        let mut rx = ctx.audio_rx;
        let path = ctx.spool_path;
        tauri::async_runtime::spawn(async move {
            // Delete only after the recorder has flushed, or the file would come back.
            let _ = tokio::time::timeout(OPEN_TIMEOUT, rx.wait_for(Option::is_some)).await;
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(%error, "could not delete spool file");
            }
        });
    }

    /// Emits the pill state and shows or hides the pill window.
    fn publish(&mut self) {
        let view = self.machine.view(self.now());
        let annotation = (self.machine.active_kind() == Some(SessionKind::Annotation)).then(|| {
            AnnotationPill {
                // The picture count arrives with the screen adapter (M3).
                images: 0,
                screen_lost: self.machine.screen_lost(),
            }
        });
        let state = PillState {
            view,
            language: self.last_language,
            annotation,
        };
        self.services.set_pill_state(state.clone());
        if let Err(error) = state.emit(&self.app) {
            tracing::warn!(%error, "pill state emit failed");
        }
        let visible = !matches!(view, PillView::Hidden);
        if visible != self.pill_visible {
            self.pill_visible = visible;
            crate::windows::set_pill_visible(&self.app, visible);
        }
    }

    fn shutdown(&mut self) {
        for (_, ctx) in self.sessions.drain() {
            ctx.cancel.cancel();
            if let Some(id) = ctx.stream {
                self.services.inference().stream_cancel(id);
            }
            if let Some(recorder) = ctx.recorder {
                // Keep the spool: recovery picks it up on the next start.
                let _ = recorder.stop();
            }
        }
    }
}

/// A hard bound for one transcription task, above the machine's own timeout (which reports the
/// failure). It only guarantees that the task and its channel do not live forever (G5).
pub fn backstop_timeout(samples: usize, loading: bool) -> Duration {
    let recorded_ms = samples as u64 * 1000 / u64::from(murmur_core::SAMPLE_RATE_HZ);
    let mut ms = session::TRANSCRIBE_TIMEOUT_BASE_MS + recorded_ms + 5_000;
    if loading {
        ms += session::MODEL_LOAD_TIMEOUT_MS;
    }
    Duration::from_millis(ms)
}

/// Checks the microphone permission (G8), then opens the device within `OPEN_TIMEOUT`.
async fn open_microphone(
    app: AppHandle,
    tx: mpsc::UnboundedSender<Msg>,
    session: SessionId,
    device: Option<String>,
    spool_path: PathBuf,
    tap: Option<audio::PcmTap>,
) -> Result<Recorder, CaptureError> {
    // The debug WAV device is a file, not the microphone: no permission is involved.
    let wav_device =
        cfg!(debug_assertions) && device.as_deref().is_some_and(|d| d.starts_with("wav:"));
    let permission = match macos::microphone_permission() {
        _ if wav_device => Permission::Granted,
        Permission::NotDetermined => {
            tokio::time::timeout(PERMISSION_PROMPT_TIMEOUT, macos::request_microphone())
                .await
                .unwrap_or(Permission::NotDetermined)
        }
        other => other,
    };
    if permission != Permission::Granted {
        return Err(CaptureError::PermissionDenied);
    }
    let on_event: Box<dyn Fn(AudioEvent) + Send + Sync> = Box::new(move |event| match event {
        AudioEvent::Level(level) => {
            let _ = AudioLevel { level }.emit_to(&app, "pill");
        }
        AudioEvent::Disconnected => {
            let _ = tx.send(Msg::Core(Event::MicDisconnected { session }));
        }
    });
    let cfg = AudioConfig {
        device,
        spool_path,
        tap,
    };
    let job = tauri::async_runtime::spawn_blocking(move || Recorder::start(cfg, on_event));
    match tokio::time::timeout(OPEN_TIMEOUT + Duration::from_millis(500), job).await {
        Ok(Ok(Ok(recorder))) => Ok(recorder),
        Ok(Ok(Err(AudioError::NoDevice))) => Err(CaptureError::NoDevice),
        Ok(Ok(Err(AudioError::OpenTimeout))) | Err(_) => Err(CaptureError::OpenTimeout),
        Ok(Ok(Err(_))) | Ok(Err(_)) => Err(CaptureError::DeviceError),
    }
}
