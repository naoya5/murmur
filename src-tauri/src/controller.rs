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
use murmur_core::annotations as ann;
use murmur_core::catalog;
use murmur_core::ports::{CancelToken, RawTranscript, RefineError};
use murmur_core::refine::{self, Glossary, RefineKind, RefinePlan, RefineStatus, RunReport};
use murmur_core::session::{
    self, AnnotationsRefusal, CaptureError, ComposeOutcome, ComposeWords, CopyReason,
    DeliveryOutcome, Effect, Event, FailureKind, Machine, Notice, Paused, PillView, PlanCounts,
    RefineOutcome, SessionId, SessionKind, State, TranscribeOutcome, TranscriptText,
};
use murmur_core::text::{self, FormatOptions};
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event as _;
use tokio::sync::{mpsc, watch};

use crate::adapters::audio::{self, AudioConfig, AudioError, AudioEvent, CapturedAudio, Recorder};
use crate::adapters::frontmost::{self, FrontApp};
use crate::adapters::output::{self, DeliverOptions};
use crate::adapters::pointer::{self, PointerEvent, PointerFeed};
use crate::adapters::screen::{
    self, DisplayInfo, ScreenCapture, ScreenConfig, ScreenEvent, ScreenSource,
};
use crate::adapters::sounds::{self, Cue};
use crate::adapters::store::AnnotationRow;
use crate::annotation_session::{self as ann_session, AnnotationCtx, PlanInput, Planned};
use crate::app::App;
use crate::bundles::{self, RecordingClock};
use crate::debug;
use crate::history::{self, Record, RefineRecord};
use crate::inference::{
    InferenceError, InferenceOutput, RangesRequest, RegionsRequest, StreamId, StreamRequest,
    TranscribeRequest,
};
use crate::ipc::{InkPoint, OverlayState};
use crate::platform::macos::{self, Permission, PrivacyPane};

type ScreenDone = Option<Arc<screen::Stopped>>;

/// Paste must finish within this time; the output adapter has its own 1 s limit.
const DELIVER_TIMEOUT: Duration = Duration::from_millis(1_500);
/// A bundle paste (picture, gap, text) must finish within this; the adapter stops at 2.0 s and
/// the machine's delivery timer at 2.5 s.
const BUNDLE_DELIVER_TIMEOUT: Duration = Duration::from_millis(4_500);
/// Stopping the screen capture (stream stop plus candidate writes) is bounded by the adapter at
/// 6 s; the plan waits a little longer for it.
const SCREEN_STOP_TIMEOUT: Duration = Duration::from_secs(8);
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// Covers a pending open (3.5 s), owner close (3 s), and converter close (3 s).
const CAPTURE_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// The microphone permission prompt waits for the user; give them time, but not forever.
const PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
/// A hard bound for one refine task, above the machine's refine timer: the adapter may wait for a
/// warm-up to finish (start, load and warm) and then start and load a cold model itself.
const REFINE_BACKSTOP_EXTRA_MS: u64 = refine::REFINER_START_TIMEOUT_MS * 2
    + refine::REFINER_LOAD_TIMEOUT_MS * 2
    + refine::REFINER_WARM_TIMEOUT_MS
    + 5_000;

/// What the pill window receives after every step.
#[derive(Debug, Clone, Serialize, Type, tauri_specta::Event)]
pub struct PillState {
    pub view: PillView,
    pub language: Language,
    /// Present while the session has a model step (the pill shows "JA -> EN" for a translation).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub refine: Option<RefineKind>,
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

/// What the compose step came back with.
pub enum ComposeResult {
    /// The pictures are drawn and the files written; the time it took.
    Rendered(crate::adapters::draw::Rendered, u64),
    /// Nothing was drawn: no picture was wanted, the words go out alone.
    WordsOnly,
    /// A picture was wanted but could not be made.
    Failed(String),
}

/// Messages into the controller loop.
pub enum Msg {
    /// A core event from hotkeys, the pill, timers or debug triggers.
    Core(Event),
    /// The main hotkey with a language override (alternate hotkey).
    HotkeyDownWithLanguage(Language),
    /// The translation hotkey went down (experimental). Its release is a plain `HotkeyUp`.
    TranslateHotkey,
    /// The annotations hotkey. The permissions are checked here, right before use (G8).
    AnnotationsHotkey,
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
    /// The model step ended (or was cancelled).
    Refined {
        session: SessionId,
        report: RunReport,
    },
    /// A stroke from the drawing layer (epoch milliseconds).
    InkStroke {
        points: Vec<InkPoint>,
        started_ms: f64,
        ended_ms: f64,
    },
    ScreenStarted {
        session: SessionId,
        result: Result<(ScreenCapture, DisplayInfo), String>,
    },
    Screen {
        session: SessionId,
        event: ScreenEvent,
    },
    Pointer {
        session: SessionId,
        event: PointerEvent,
    },
    /// The keyframes are chosen (the stop pipeline, first half).
    Planned {
        session: SessionId,
        planned: Planned,
    },
    /// The segments are transcribed (the stop pipeline, second half).
    Segments {
        session: SessionId,
        result: Result<SegmentsOutcome, InferenceError>,
    },
    /// The pictures and the summary are written (or not).
    Composed {
        session: SessionId,
        result: ComposeResult,
    },
    /// The drawing layer's page has mounted and wants the current state.
    OverlayReady,
    Shutdown,
}

/// What the segment transcription produced.
pub enum SegmentsOutcome {
    NoSpeech,
    Chunks {
        chunks: Vec<ann::Chunk>,
        asr_ms: u64,
        model_id: &'static str,
    },
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
    /// Annotation sessions only.
    annotation: Option<AnnotationCtx>,
    /// Dictations with a model step only.
    refine: Option<RefineCtx>,
    /// Resolves once the screen capture has stopped (an empty result when there was none).
    screen_done: Option<(watch::Sender<ScreenDone>, watch::Receiver<ScreenDone>)>,
}

/// The model step of a dictation (experimental).
struct RefineCtx {
    kind: RefineKind,
    /// Built from the dictionary once per recording, so the warm-up and the call share a prefix.
    glossary: Glossary,
    cancel: CancelToken,
    /// The rule-formatted text that went to the model (a translation's Japanese).
    pre_refine_text: Option<String>,
    /// The steps planned for the text (`refine::steps`), for the history's model ids.
    steps: Vec<RefineKind>,
    started: Option<Instant>,
    /// From the refine request to the delivery.
    refine_ms: Option<u64>,
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
    /// The app in front at the annotations hotkey, until its session starts the screen.
    front_at_hotkey: Option<FrontApp>,
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
        front_at_hotkey: None,
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
            Msg::Core(mut event) => {
                if let Event::HotkeyDown { refine, .. } = &mut event {
                    let settings = self.services.settings();
                    self.language_override = Some(settings.language);
                    *refine = settings.dictation_refine(settings.language);
                }
                // A click on the "Screen Recording is needed" notice opens the settings pane.
                // This lives here, not in the core: it is a window action, not a transition.
                if event == Event::PillClicked
                    && let State::Idle {
                        notice: Some((Notice::AnnotationsRefused { reason }, _)),
                    } = self.machine.state()
                    && *reason == AnnotationsRefusal::ScreenRecordingDenied
                {
                    macos::open_privacy_pane(PrivacyPane::ScreenCapture);
                }
                Some(event)
            }
            Msg::HotkeyDownWithLanguage(language) => {
                self.language_override = Some(language);
                let settings = self.services.settings();
                Some(Event::HotkeyDown {
                    mode: settings.record_mode,
                    refine: settings.dictation_refine(language),
                })
            }
            Msg::TranslateHotkey => {
                let settings = self.services.settings();
                // The key may fire once after the feature was switched off.
                if !settings.translate_enabled {
                    return None;
                }
                // The language is fixed for the recording: translation records Japanese.
                self.language_override = Some(refine::TRANSLATE_FROM);
                Some(Event::HotkeyDown {
                    mode: settings.record_mode,
                    refine: Some(RefinePlan::new(RefineKind::Translate, true)),
                })
            }
            Msg::Refined { session, report } => self.on_refined(session, report),
            Msg::AnnotationsHotkey => self.on_annotations_hotkey(),
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
            Msg::InkStroke {
                points,
                started_ms,
                ended_ms,
            } => {
                let session = self.machine.active_session()?;
                let ann = self.sessions.get_mut(&session)?.annotation.as_mut()?;
                if ann.on_stroke(points, started_ms, ended_ms) {
                    self.publish();
                }
                None
            }
            Msg::ScreenStarted { session, result } => self.on_screen_started(session, result),
            Msg::Screen { session, event } => self.on_screen_event(session, event),
            Msg::Pointer { session, event } => {
                let ann = self.sessions.get_mut(&session)?.annotation.as_mut()?;
                let shift_before = ann.shift_held;
                let changed = ann.on_pointer(event);
                if ann.shift_held != shift_before {
                    crate::windows::set_overlay_interactive(&self.app, ann.shift_held);
                }
                if changed {
                    self.publish();
                }
                None
            }
            Msg::Planned { session, planned } => self.on_planned(session, planned),
            Msg::Segments { session, result } => self.on_segments(session, result),
            Msg::Composed { session, result } => self.on_composed(session, result),
            Msg::OverlayReady => {
                self.emit_overlay_state();
                None
            }
            Msg::Shutdown => None,
        }
    }

    /// Screen Recording and the microphone are checked only when a recording would start; a
    /// press that stops one needs no permission. A press after the feature was switched off
    /// (the key may fire once before it is unregistered) does nothing.
    fn on_annotations_hotkey(&mut self) -> Option<Event> {
        if !self.services.settings().annotations_enabled {
            return None;
        }
        if self.machine.active_session().is_some() {
            return Some(Event::AnnotationsHotkey);
        }
        // The app in front now receives the bundle, whatever is in front at the stop (R1).
        self.front_at_hotkey = frontmost::capture();
        if !macos::screen_recording_granted() && !macos::request_screen_recording() {
            return Some(Event::AnnotationsRefused {
                reason: AnnotationsRefusal::ScreenRecordingDenied,
            });
        }
        // `NotDetermined` is fine: opening the microphone shows the system prompt, as for dictation.
        if macos::microphone_permission() == Permission::Denied {
            return Some(Event::AnnotationsRefused {
                reason: AnnotationsRefusal::MicrophoneDenied,
            });
        }
        Some(Event::AnnotationsHotkey)
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

    /// Turns the model step's report into the machine's outcome. A cancel needs no event: the
    /// machine cancelled it.
    fn on_refined(&mut self, session: SessionId, report: RunReport) -> Option<Event> {
        let rejected = match &report.result {
            Err(RefineError::Rejected(rules)) => Some(rules.clone()),
            _ => None,
        };
        let (pre_clean, pre_clean_error) = match &report.pre_clean {
            refine::PreClean::NotNeeded => ("not_needed", None),
            refine::PreClean::Used => ("used", None),
            refine::PreClean::Failed(error) => ("failed", Some(error.to_string())),
        };
        tracing::info!(
            session = session.0,
            steps = ?report.steps,
            ok = report.result.is_ok(),
            error = report.result.as_ref().err().map(|e| e.to_string()),
            ?rejected,
            pre_clean,
            pre_clean_error,
            "refine finished"
        );
        let outcome = match report.result {
            Ok(text) => RefineOutcome::Text(text),
            Err(RefineError::Cancelled) => return None,
            Err(RefineError::Timeout) => RefineOutcome::TimedOut,
            Err(_) => RefineOutcome::Failed,
        };
        Some(Event::Refined { session, outcome })
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
            Effect::Deliver {
                session,
                text,
                copy_only,
            } => self.deliver(session, text, copy_only),
            Effect::SaveSuccess {
                session,
                outcome,
                refine,
            } => self.save_success(session, outcome, refine),
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
            // --- annotation sessions ---
            Effect::StartScreen { session } => self.start_screen(session),
            Effect::PauseScreen { session } => self.set_screen_paused(session, true),
            Effect::ResumeScreen { session } => self.set_screen_paused(session, false),
            Effect::StopScreen { session } => self.stop_screen(session),
            Effect::ShowOverlay => self.show_overlay(),
            Effect::HideOverlay => self.hide_overlay(),
            Effect::PlanKeyframes { session } => self.plan_keyframes(session),
            // The plan task carries on into the segment transcription: both wait for the same
            // audio, and the segments need the plan first.
            Effect::TranscribeSegments { .. } => {}
            Effect::Compose { session, words } => self.compose(session, words),
            Effect::DeliverBundle { session } => self.deliver_annotation(session, true),
            Effect::DeliverText { session } => self.deliver_annotation(session, false),
            Effect::SaveAnnotation { session, outcome } => self.save_annotation(session, outcome),
            Effect::DropAll { session } => self.drop_all(session),
            // --- the model step (experimental) ---
            Effect::WarmRefiner { session, plan } => self.warm_refiner(session, plan),
            Effect::Refine {
                session,
                plan,
                text,
            } => self.refine(session, plan, text),
            Effect::CancelRefine { session } => {
                if let Some(refine) = self.sessions.get(&session).and_then(|c| c.refine.as_ref()) {
                    refine.cancel.cancel();
                }
            }
        }
    }

    /// Starts and warms the models a session of `kind` may need, while the user speaks.
    fn warm_refiner(&mut self, session: SessionId, plan: RefinePlan) {
        let kind = plan.kind;
        // Take the lifecycle before checking settings: an off transition either retires this
        // handle or is already visible below. A delayed warm effect cannot restart an off model.
        let mut refiner = self.services.refiner().handle();
        let settings = self.services.settings();
        let Some(ctx) = self.sessions.get_mut(&session) else {
            return;
        };
        let glossary = Glossary::new(&settings.dictionary);
        ctx.refine = Some(RefineCtx {
            kind,
            glossary: glossary.clone(),
            cancel: CancelToken::new(),
            pre_refine_text: None,
            steps: Vec::new(),
            started: None,
            refine_ms: None,
        });
        let enabled = match kind {
            RefineKind::Clean => settings.refine_enabled,
            RefineKind::Translate => settings.translate_enabled,
        };
        if enabled {
            tauri::async_runtime::spawn_blocking(move || {
                crate::refine_session::warm(plan, &glossary, &mut refiner);
            });
        }
    }

    /// Sends the rule-formatted text through the model step on a blocking thread.
    fn refine(&mut self, session: SessionId, plan: RefinePlan, text: TranscriptText) {
        let kind = plan.kind;
        let mut refiner = self.services.refiner().handle();
        let settings = self.services.settings();
        let dictionary = settings.dictionary;
        let Some(ctx) = self.sessions.get_mut(&session) else {
            return;
        };
        let refine = ctx.refine.get_or_insert_with(|| RefineCtx {
            kind,
            glossary: Glossary::new(&dictionary),
            cancel: CancelToken::new(),
            pre_refine_text: None,
            steps: Vec::new(),
            started: None,
            refine_ms: None,
        });
        refine.pre_refine_text = Some(text.0.clone());
        refine.steps = refine::steps(kind, &text.0).to_vec();
        refine.started = Some(Instant::now());
        let glossary = refine.glossary.clone();
        let cancel = refine.cancel.clone();
        let backstop = Duration::from_millis(
            refine::timeout_ms(kind, &text.0).saturating_add(REFINE_BACKSTOP_EXTRA_MS),
        );
        let enabled = match kind {
            RefineKind::Clean => settings.refine_enabled,
            RefineKind::Translate => settings.translate_enabled,
        };
        if !enabled {
            let _ = self.tx.send(Msg::Core(Event::Refined {
                session,
                outcome: RefineOutcome::Failed,
            }));
            return;
        }
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
            let job_cancel = cancel.clone();
            let loading_tx = tx.clone();
            let job = tauri::async_runtime::spawn_blocking(move || {
                crate::refine_session::drive(
                    plan,
                    &text.0,
                    glossary,
                    &mut refiner,
                    &job_cancel,
                    &mut || {
                        let _ = loading_tx.send(Msg::Core(Event::RefinerLoading { session }));
                    },
                )
            });
            let report = match tokio::time::timeout(backstop, job).await {
                Ok(Ok(report)) => report,
                Ok(Err(error)) => {
                    tracing::error!(session = session.0, %error, "the refine task failed");
                    RunReport {
                        result: Err(RefineError::Failed("task".to_owned())),
                        pre_clean: refine::PreClean::NotNeeded,
                        steps: Vec::new(),
                    }
                }
                Err(_) => {
                    // The machine has given up already; free the blocking thread.
                    cancel.cancel();
                    return;
                }
            };
            let _ = tx.send(Msg::Refined { session, report });
        });
    }

    /// Start and stop sounds, read from settings at the moment they play.
    fn cue(&self, effect: &Effect) {
        let cue = match effect {
            Effect::StartCapture { .. } => Cue::Start,
            Effect::Transcribe { .. } | Effect::TranscribeSegments { .. } => Cue::Stop,
            Effect::DropAudio { .. } | Effect::DropAll { .. } => Cue::Cancel,
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
        // An annotation session is transcribed per segment at stop instead; it only warms the
        // model here.
        let annotation = self.machine.active_kind() == Some(SessionKind::Annotation);
        if annotation && let Some((model, vad)) = self.services.model_specs(language) {
            self.services.inference().warm(model, vad);
        }
        let specs = (!annotation)
            .then(|| self.services.model_specs(language))
            .flatten();
        let stream = specs.map(|(model, vad)| {
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
                annotation: None,
                refine: None,
                screen_done: None,
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

    fn deliver(&mut self, session: SessionId, text: TranscriptText, copy_only: Option<CopyReason>) {
        let settings = self.services.settings();
        let opts = DeliverOptions {
            auto_paste: settings.auto_paste,
            restore_clipboard: settings.restore_clipboard,
        };
        let t_stop = self.sessions.get(&session).and_then(|c| c.t_stop);
        if let Some(ctx) = self.sessions.get_mut(&session) {
            // The history keeps what went out: the English of a translation, the Japanese when
            // the translation failed.
            ctx.text = Some(text.0.clone());
            if let Some(refine) = ctx.refine.as_mut() {
                refine.refine_ms = refine.started.map(|t| t.elapsed().as_millis() as u64);
            }
        }
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
            let job = tauri::async_runtime::spawn_blocking(move || match copy_only {
                Some(reason) => output::copy_only(&text.0, reason),
                None => output::deliver(&text.0, opts),
            });
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
            refine: None,
        }
    }

    fn save_success(
        &mut self,
        session: SessionId,
        outcome: DeliveryOutcome,
        refine_status: Option<RefineStatus>,
    ) {
        let Some(ctx) = self.sessions.remove(&session) else {
            return;
        };
        tracing::info!(
            session = session.0,
            ?outcome,
            ?refine_status,
            "session saved"
        );
        if let Some(text) = &ctx.text {
            self.services.remember_last(text.clone());
        }
        let mut record = Self::record(&ctx, None);
        if let (Some(status), Some(refine)) = (refine_status, ctx.refine.as_ref()) {
            record.refine = Some(RefineRecord {
                kind: refine.kind,
                status,
                // Without a model call the delivered text is the rule text.
                pre_refine_text: refine.pre_refine_text.clone().or_else(|| ctx.text.clone()),
                model_id: (!refine.steps.is_empty())
                    .then(|| catalog::refine_model_ids(&refine.steps)),
                refine_ms: refine.refine_ms,
            });
        }
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

    // --- annotation sessions ---

    fn start_screen(&mut self, session: SessionId) {
        let Some(ctx) = self.sessions.get_mut(&session) else {
            return;
        };
        let now_epoch = chrono::Utc::now().timestamp_millis();
        let origin_epoch = now_epoch as f64 - ctx.t_hotkey.elapsed().as_millis() as f64;
        let clock = RecordingClock::new(ctx.t_hotkey, origin_epoch);
        let root = self.services.paths().annotations.clone();
        let mut annotation = match AnnotationCtx::new(&root, now_epoch, session.0, clock) {
            Ok(annotation) => annotation,
            Err(error) => {
                tracing::warn!(session = session.0, %error, "bundle folder not created");
                let _ = self.tx.send(Msg::Core(Event::ScreenFailed { session }));
                return;
            }
        };
        let config = ScreenConfig {
            source: debug::screen_png_dir().map_or(ScreenSource::Display, ScreenSource::PngDir),
            exclude_bundle_id: Some(self.app.config().identifier.clone()),
            cursor: pointer::cursor_position().unwrap_or((0.0, 0.0)),
            candidates_dir: annotation.candidates_dir.clone(),
            origin: ctx.t_hotkey,
        };
        annotation.front = self.front_at_hotkey.take();
        annotation.target_bundle_id = annotation.front.as_ref().and_then(|f| f.bundle_id.clone());
        ctx.annotation = Some(annotation);
        ctx.screen_done = Some(watch::channel(None));
        let tx = self.tx.clone();
        let events = self.tx.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let on_event: Arc<dyn Fn(ScreenEvent) + Send + Sync> = Arc::new(move |event| {
                let _ = events.send(Msg::Screen { session, event });
            });
            let result = screen::start(config, on_event).map_err(|e| e.to_string());
            let _ = tx.send(Msg::ScreenStarted { session, result });
        });
    }

    fn on_screen_started(
        &mut self,
        session: SessionId,
        result: Result<(ScreenCapture, DisplayInfo), String>,
    ) -> Option<Event> {
        let Some(ctx) = self.sessions.get_mut(&session) else {
            // The session ended while the capture was starting: release it.
            if let Ok((capture, _)) = result {
                tauri::async_runtime::spawn_blocking(move || {
                    let _ = capture.stop();
                });
            }
            return None;
        };
        let annotation = ctx.annotation.as_mut()?;
        match result {
            Ok((capture, display)) => {
                let tx = self.tx.clone();
                let on_event: Arc<dyn Fn(PointerEvent) + Send + Sync> = Arc::new(move |event| {
                    let _ = tx.send(Msg::Pointer { session, event });
                });
                // A test script replaces the real pointer, so nobody's clicks leak into a test.
                annotation.pointer = match debug::pointer_script() {
                    Some(script) => {
                        let strokes = self.tx.clone();
                        let on_stroke: Arc<dyn Fn(Vec<InkPoint>, f64, f64) + Send + Sync> =
                            Arc::new(move |points, started_ms, ended_ms| {
                                let _ = strokes.send(Msg::InkStroke {
                                    points,
                                    started_ms,
                                    ended_ms,
                                });
                            });
                        match debug::play_pointer_script(
                            &script,
                            annotation.clock.origin(),
                            annotation.clock.origin_epoch_ms(),
                            on_event,
                            on_stroke,
                        ) {
                            Ok(player) => Some(Box::new(player) as Box<dyn PointerFeed>),
                            Err(error) => {
                                tracing::warn!(%error, "pointer script not played");
                                None
                            }
                        }
                    }
                    None => Some(Box::new(pointer::start(on_event))),
                };
                annotation.screen = Some(capture);
                annotation.display = Some(display);
                None
            }
            Err(error) => {
                tracing::warn!(session = session.0, %error, "screen capture did not start");
                annotation.screen_failed = true;
                Some(Event::ScreenFailed { session })
            }
        }
    }

    fn on_screen_event(&mut self, session: SessionId, event: ScreenEvent) -> Option<Event> {
        let annotation = self.sessions.get_mut(&session)?.annotation.as_mut()?;
        match event {
            ScreenEvent::Frame(frame) => {
                if annotation.on_frame(frame) {
                    self.publish();
                }
                None
            }
            ScreenEvent::Failed(error) => {
                tracing::warn!(session = session.0, %error, "screen capture stopped on its own");
                annotation.screen_failed = true;
                Some(Event::ScreenFailed { session })
            }
        }
    }

    fn set_screen_paused(&mut self, session: SessionId, paused: bool) {
        let Some(annotation) = self
            .sessions
            .get_mut(&session)
            .and_then(|c| c.annotation.as_mut())
        else {
            return;
        };
        let wall = annotation.clock.wall_now();
        if paused {
            annotation.clock.pause_at(wall);
        } else {
            annotation.clock.resume_at(wall);
        }
        if let Some(screen) = &annotation.screen {
            screen.set_paused(paused);
        }
    }

    /// Stops the pointer and the screen on a blocking thread; the plan waits for the result.
    fn stop_screen(&mut self, session: SessionId) {
        let Some(ctx) = self.sessions.get_mut(&session) else {
            return;
        };
        let Some(annotation) = ctx.annotation.as_mut() else {
            return;
        };
        let pointer = annotation.pointer.take();
        let capture = annotation.screen.take();
        let done = ctx.screen_done.as_ref().map(|(tx, _)| tx.clone());
        tauri::async_runtime::spawn_blocking(move || {
            if let Some(pointer) = pointer {
                pointer.stop();
            }
            let stopped = match capture.map(ScreenCapture::stop) {
                Some(Ok(stopped)) => stopped,
                Some(Err(error)) => {
                    tracing::warn!(session = session.0, %error, "screen capture stop failed");
                    empty_stopped()
                }
                None => empty_stopped(),
            };
            if let Some(done) = done {
                let _ = done.send(Some(Arc::new(stopped)));
            }
        });
    }

    fn show_overlay(&mut self) {
        crate::windows::show_overlay(&self.app);
        self.emit_overlay_state();
    }

    /// Tells the drawing layer whether to draw: shown while an annotation is being recorded
    /// (also during its cancel countdown), hidden otherwise. Sent on every change and again when
    /// the layer's page asks (`Msg::OverlayReady`), so a page that mounted late catches up.
    fn emit_overlay_state(&self) {
        let recording = match self.machine.state() {
            State::Recording(rec) => rec.kind == SessionKind::Annotation,
            State::CancelPending {
                paused: Paused::Recording(rec),
                ..
            } => rec.kind == SessionKind::Annotation,
            _ => false,
        };
        let state = if recording {
            let started = self
                .machine
                .active_session()
                .and_then(|s| self.sessions.get(&s))
                .and_then(|c| c.annotation.as_ref())
                .map_or_else(
                    || chrono::Utc::now().timestamp_millis() as f64,
                    |a| a.clock.origin_epoch_ms(),
                );
            OverlayState::Shown {
                recording_started_ms: started,
            }
        } else {
            OverlayState::Hidden
        };
        if let Err(error) = state.emit(&self.app) {
            tracing::warn!(%error, "overlay state emit failed");
        }
    }

    fn hide_overlay(&mut self) {
        crate::windows::set_overlay_interactive(&self.app, false);
        crate::windows::hide_overlay(&self.app);
        self.emit_overlay_state();
    }

    /// The stop pipeline: audio and screen done, VAD, keyframes (`Msg::Planned`), segments,
    /// transcription (`Msg::Segments`). One task, because every step needs the previous one.
    fn plan_keyframes(&mut self, session: SessionId) {
        let Some(ctx) = self.sessions.get_mut(&session) else {
            return;
        };
        let Some(annotation) = ctx.annotation.as_mut() else {
            return;
        };
        let input = PlanInput {
            signals: annotation.signals.clone(),
            frame_of_sample: annotation.frame_of_sample.clone(),
            kept: annotation.kept.clone(),
            candidates_dir: annotation.candidates_dir.clone(),
            stopped_ms: annotation.clock.recording_now(),
        };
        let mut audio_rx = ctx.audio_rx.clone();
        let screen_rx = ctx.screen_done.as_ref().map(|(_, rx)| rx.clone());
        let language = ctx.language;
        let cancel = ctx.cancel.clone();
        let services = self.services.clone();
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
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
            let stopped = match screen_rx {
                Some(mut rx) => {
                    match tokio::time::timeout(SCREEN_STOP_TIMEOUT, rx.wait_for(Option::is_some))
                        .await
                    {
                        Ok(Ok(guard)) => guard.clone(),
                        _ => None,
                    }
                }
                None => None,
            };
            let specs = services.model_specs(language);
            let regions = match (&pcm, &specs) {
                (Some(pcm), Some((_, vad))) if !pcm.is_empty() => {
                    match services
                        .inference()
                        .detect_regions(RegionsRequest {
                            pcm: pcm.clone(),
                            vad: vad.clone(),
                            cancel: cancel.clone(),
                        })
                        .await
                    {
                        Ok(regions) => regions,
                        Err(error) => {
                            tracing::warn!(session = session.0, %error, "VAD failed; no speech spans");
                            Vec::new()
                        }
                    }
                }
                _ => Vec::new(),
            };
            // The adapter's list of written candidates is the truth: a write may have failed.
            let mut input = input;
            if let Some(stopped) = &stopped {
                input.kept = stopped.kept.iter().map(|(index, _)| *index).collect();
            }
            let recent = stopped.map(|s| s.recent.clone()).unwrap_or_default();
            let total = pcm.as_ref().map_or(0, |p| p.len());
            let regions_for_plan = regions.clone();
            let planned = tauri::async_runtime::spawn_blocking(move || {
                ann_session::plan(input, &regions_for_plan, total, &recent)
            })
            .await;
            let planned = match planned {
                Ok(planned) => planned,
                Err(error) => {
                    tracing::warn!(session = session.0, %error, "the plan task failed");
                    let _ = tx.send(Msg::Segments {
                        session,
                        result: Err(InferenceError::Audio("plan failed".into())),
                    });
                    return;
                }
            };
            let segments = ann_session::segments(&planned);
            let _ = tx.send(Msg::Planned {
                session,
                planned: planned.clone(),
            });
            // The pictures are planned either way; the words need the audio and the model.
            let Some(pcm) = pcm else {
                let _ = tx.send(Msg::Segments {
                    session,
                    result: Err(InferenceError::Audio("captured audio unavailable".into())),
                });
                return;
            };
            let Some((model, _)) = specs else {
                let _ = tx.send(Msg::Segments {
                    session,
                    result: Err(InferenceError::ModelMissing),
                });
                return;
            };
            if segments.is_empty() {
                let _ = tx.send(Msg::Segments {
                    session,
                    result: Ok(SegmentsOutcome::NoSpeech),
                });
                return;
            }
            let settings = services.settings();
            let vocabulary = settings
                .dictionary
                .iter()
                .filter(|e| e.scope.includes(language))
                .cloned()
                .collect();
            let loading = services.inference().loaded_model() != Some(model.entry.id);
            if loading {
                let _ = tx.send(Msg::Core(Event::ModelLoading { session }));
            }
            let budget = backstop_timeout(pcm.len(), loading);
            let request = RangesRequest {
                pcm,
                ranges: segments.iter().map(|s| s.audio.clone()).collect(),
                language,
                model,
                vocabulary,
                cancel: cancel.clone(),
            };
            let inference = services.inference().clone();
            let result =
                match tokio::time::timeout(budget, inference.transcribe_ranges(request)).await {
                    Ok(result) => result,
                    Err(_) => {
                        cancel.cancel();
                        Err(InferenceError::Cancelled)
                    }
                };
            let result = result.map(|output| {
                let opts = FormatOptions {
                    remove_fillers: settings.remove_fillers,
                    add_final_punctuation: settings.add_final_punctuation,
                    apply_dictionary: !output.custom_vocabulary,
                };
                let chunks = segments
                    .iter()
                    .zip(output.texts)
                    .filter_map(|(segment, raw)| {
                        let formatted =
                            text::format(&raw.text, language, &opts, &settings.dictionary);
                        (!formatted.is_empty()).then(|| ann_session::chunk_of(segment, formatted))
                    })
                    .collect();
                SegmentsOutcome::Chunks {
                    chunks,
                    asr_ms: output.asr_ms,
                    model_id: output.model_id,
                }
            });
            let _ = tx.send(Msg::Segments { session, result });
        });
    }

    fn on_planned(&mut self, session: SessionId, planned: Planned) -> Option<Event> {
        let annotation = self.sessions.get_mut(&session)?.annotation.as_mut()?;
        let counts = PlanCounts {
            keyframes: planned.selection.keyframes.len() as u32,
            marks: planned.selection.marks.len() as u32,
        };
        tracing::info!(
            session = session.0,
            keyframes = counts.keyframes,
            marks = counts.marks,
            candidates = annotation.kept.len(),
            "keyframes planned"
        );
        annotation.plan = Some(planned);
        Some(Event::Planned { session, counts })
    }

    fn on_segments(
        &mut self,
        session: SessionId,
        result: Result<SegmentsOutcome, InferenceError>,
    ) -> Option<Event> {
        let transcribing = match self.machine.state() {
            State::Transcribing(tx) => tx.session == session,
            State::CancelPending {
                session: s,
                paused: Paused::Transcribing { .. },
                ..
            } => *s == session,
            _ => false,
        };
        if !transcribing {
            tracing::warn!(
                session = session.0,
                "segments finished late; result dropped"
            );
            return None;
        }
        let ctx = self.sessions.get_mut(&session)?;
        let outcome = match result {
            Ok(SegmentsOutcome::NoSpeech) => TranscribeOutcome::NoSpeech,
            Ok(SegmentsOutcome::Chunks {
                chunks,
                asr_ms,
                model_id,
            }) => {
                ctx.asr_ms = Some(asr_ms);
                ctx.model_id = Some(model_id);
                if chunks.is_empty() {
                    TranscribeOutcome::NoSpeech
                } else {
                    let joined: Vec<String> = chunks.iter().map(|c| c.text.0.clone()).collect();
                    let joined = murmur_core::pipeline::join_texts(&joined);
                    if let Some(annotation) = ctx.annotation.as_mut() {
                        annotation.chunks = chunks;
                    }
                    ctx.text = Some(joined.clone());
                    TranscribeOutcome::Text(TranscriptText(joined))
                }
            }
            Err(InferenceError::Cancelled) => return None,
            Err(error) => {
                let failure = match &error {
                    InferenceError::ModelMissing => FailureKind::ModelMissing,
                    InferenceError::ModelLoad(_) => FailureKind::ModelLoadFailed,
                    InferenceError::Audio(_) => FailureKind::Audio,
                    _ => FailureKind::Engine,
                };
                tracing::warn!(session = session.0, %error, "segment transcription failed");
                if let Some(annotation) = ctx.annotation.as_mut() {
                    annotation.failure = Some(failure);
                }
                TranscribeOutcome::Failed(failure)
            }
        };
        Some(Event::Transcribed { session, outcome })
    }

    fn compose(&mut self, session: SessionId, words: ComposeWords) {
        let Some(ctx) = self.sessions.get_mut(&session) else {
            return;
        };
        let Some(annotation) = ctx.annotation.as_mut() else {
            return;
        };
        annotation.words = Some(words);
        let tx = self.tx.clone();
        let Some(planned) = annotation.plan.clone() else {
            let _ = tx.send(Msg::Composed {
                session,
                result: ComposeResult::Failed("no plan".into()),
            });
            return;
        };
        if planned.selection.keyframes.is_empty() {
            // Nothing drawn: the words go out as a dictation. Drawn but no picture: a failure.
            let result = if planned.pictures_wanted() {
                ComposeResult::Failed("no picture for the ink".into())
            } else {
                ComposeResult::WordsOnly
            };
            let _ = tx.send(Msg::Composed { session, result });
            return;
        }
        let chunks = annotation.chunks.clone();
        let language = ctx.language;
        let duration_ms = ctx
            .meta
            .as_ref()
            .map_or_else(|| annotation.clock.recording_now(), |m| m.duration_ms);
        let created_at_ms = annotation.created_at_ms;
        let model_id = ctx.model_id.map(str::to_owned);
        let display = ann_session::display_facts(annotation.display.as_ref());
        let bundle_dir = annotation.bundle_dir.clone();
        let target_bundle_id = annotation.target_bundle_id.clone();
        let delay = debug::compose_delay();
        tauri::async_runtime::spawn_blocking(move || {
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            let started = Instant::now();
            let result = ann_session::compose(
                &bundle_dir,
                &planned,
                &chunks,
                words,
                language,
                duration_ms,
                created_at_ms,
                model_id.as_deref(),
                display,
                target_bundle_id.as_deref(),
            )
            .map_or_else(ComposeResult::Failed, |c| {
                ComposeResult::Rendered(c.rendered, started.elapsed().as_millis() as u64)
            });
            let _ = tx.send(Msg::Composed { session, result });
        });
    }

    fn on_composed(&mut self, session: SessionId, result: ComposeResult) -> Option<Event> {
        // After the compose timer the words went out alone; a late result must not turn the
        // saved row into "ok".
        let composing = match self.machine.state() {
            State::Composing(c) => c.session == session,
            State::CancelPending {
                session: s,
                paused: Paused::Composing { .. },
                ..
            } => *s == session,
            _ => false,
        };
        if !composing {
            tracing::warn!(session = session.0, "compose finished late; result dropped");
            return None;
        }
        let annotation = self.sessions.get_mut(&session)?.annotation.as_mut()?;
        let outcome = match result {
            ComposeResult::Rendered(rendered, compose_ms) => {
                tracing::info!(
                    session = session.0,
                    images = rendered.frames.len(),
                    compose_ms,
                    draw_ms = rendered.draw_ms,
                    encode_ms = rendered.encode_ms,
                    "bundle composed"
                );
                let images = rendered.frames.len() as u32;
                annotation.compose_ms = Some(compose_ms);
                annotation.rendered = Some(rendered);
                ComposeOutcome::Bundle { images }
            }
            ComposeResult::WordsOnly => {
                tracing::info!(session = session.0, "nothing drawn: words only");
                ComposeOutcome::WordsOnly
            }
            ComposeResult::Failed(error) => {
                tracing::warn!(session = session.0, %error, "compose failed; words only");
                ComposeOutcome::Failed
            }
        };
        Some(Event::Composed { session, outcome })
    }

    /// Pastes the bundle (`with_pictures`) or the words alone, then reports `Delivered`.
    fn deliver_annotation(&mut self, session: SessionId, with_pictures: bool) {
        let settings = self.services.settings();
        let opts = DeliverOptions {
            auto_paste: settings.auto_paste,
            restore_clipboard: settings.restore_clipboard,
        };
        let Some(ctx) = self.sessions.get_mut(&session) else {
            return;
        };
        let Some(annotation) = ctx.annotation.as_mut() else {
            return;
        };
        let target = ann::paste_target(annotation.target_bundle_id.as_deref());
        let pictures: Vec<PathBuf> = with_pictures
            .then(|| annotation.rendered.as_ref().map(|r| r.frames.clone()))
            .flatten()
            .unwrap_or_default();
        // Without pictures the text carries no marks: they would name files that do not exist.
        let planned = with_pictures
            .then(|| annotation.plan.clone())
            .flatten()
            .unwrap_or_else(|| Planned {
                selection: ann::Selection::default(),
                pictures: Vec::new(),
                regions: Vec::new(),
                total_samples: 0,
            });
        let words = annotation.words.unwrap_or(ComposeWords::Transcribed);
        let text = ann_session::summary_text(
            ctx.language,
            target,
            &annotation.bundle_dir,
            &planned,
            &annotation.chunks,
            words,
        );
        ctx.text = Some(text.clone());
        let front = annotation.front.take();
        let t_stop = ctx.t_stop;
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
            let job = tauri::async_runtime::spawn_blocking(move || {
                output::deliver_bundle(&pictures, &text, opts, target, front)
            });
            let outcome = match tokio::time::timeout(BUNDLE_DELIVER_TIMEOUT, job).await {
                Ok(Ok(outcome)) => outcome,
                _ => DeliveryOutcome::Unconfirmed,
            };
            if let Some(t_stop) = t_stop {
                tracing::info!(
                    session = session.0,
                    latency_ms = t_stop.elapsed().as_millis() as u64,
                    ?outcome,
                    ?target,
                    with_pictures,
                    "bundle delivered"
                );
            }
            let _ = tx.send(Msg::Core(Event::Delivered { session, outcome }));
        });
        if let Some(t_stop) = t_stop {
            ctx.latency_ms = Some(t_stop.elapsed().as_millis() as u64);
        }
    }

    /// After the paste (G9): the row, the candidate cleanup, the retention cleanup, the spool.
    fn save_annotation(&mut self, session: SessionId, outcome: DeliveryOutcome) {
        let Some(mut ctx) = self.sessions.remove(&session) else {
            return;
        };
        let Some(annotation) = ctx.annotation.take() else {
            return;
        };
        if let Some(text) = &ctx.text {
            self.services.remember_last(text.clone());
        }
        let words = annotation.words.unwrap_or(ComposeWords::Transcribed);
        let pictures_wanted = annotation
            .plan
            .as_ref()
            .is_some_and(Planned::pictures_wanted);
        let status = ann_session::status_of(words, pictures_wanted, annotation.rendered.is_some());
        // The row counts what went out: nothing when the pictures could not be made.
        let (keyframes, marks) = match (&annotation.rendered, &annotation.plan) {
            (Some(rendered), Some(plan)) => (
                rendered.frames.len() as u32,
                plan.selection.marks.len() as u32,
            ),
            _ => (0, 0),
        };
        tracing::info!(
            session = session.0,
            ?outcome,
            ?status,
            keyframes,
            marks,
            latency_ms = ctx.latency_ms,
            asr_ms = ctx.asr_ms,
            compose_ms = annotation.compose_ms,
            "annotation saved"
        );
        let failure = annotation.failure.or(match words {
            ComposeWords::Failed => Some(FailureKind::Timeout),
            _ => None,
        });
        let row = AnnotationRow {
            id: uuid::Uuid::new_v4().to_string(),
            created_at_ms: annotation.created_at_ms,
            status,
            folder: annotation.folder.clone(),
            duration_ms: ctx.meta.as_ref().map_or(0, |m| m.duration_ms),
            keyframes,
            marks,
            language: Some(ctx.language),
            model_id: ctx.model_id.map(str::to_owned),
            latency_ms: ctx.latency_ms,
            asr_ms: ctx.asr_ms,
            compose_ms: annotation.compose_ms,
            error: failure.map(|f| history::failure_text(f).to_owned()),
        };
        // A bundle without pictures still gets its summary and record on disk.
        let text_only_files = if annotation.rendered.is_none() {
            // No pictures on disk, so no picture and no mark in the record either.
            let planned = Planned {
                selection: ann::Selection::default(),
                pictures: Vec::new(),
                regions: Vec::new(),
                total_samples: 0,
            };
            let duration_ms = ctx.meta.as_ref().map_or(0, |m| m.duration_ms);
            let summary = ann_session::summary_text(
                ctx.language,
                ann::PasteTarget::Other,
                &annotation.bundle_dir,
                &planned,
                &annotation.chunks,
                words,
            );
            let json = bundles::annotation_json(
                annotation.created_at_ms,
                duration_ms,
                ctx.language.code(),
                ctx.model_id,
                ann_session::display_facts(annotation.display.as_ref()),
                annotation.target_bundle_id.as_deref(),
                &planned.selection.keyframes,
                &planned.selection.marks,
                &annotation.chunks,
                &planned.speech_spans(),
                status,
            );
            Some((annotation.bundle_dir.clone(), summary, json))
        } else {
            None
        };
        let failed_record = failure.map(|f| Self::record(&ctx, Some(f)));
        let app = self.app.clone();
        let store = self.services.store().clone();
        let root = self.services.paths().annotations.clone();
        let retention = self.services.settings().annotations_retention_days;
        let candidates_dir = annotation.candidates_dir;
        let spool = ctx.spool_path;
        let mut rx = ctx.audio_rx;
        tauri::async_runtime::spawn(async move {
            // The audio must be complete on disk before a failed row points at it.
            let _ = tokio::time::timeout(OPEN_TIMEOUT, rx.wait_for(Option::is_some)).await;
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(error) = store.insert_annotation(&row) {
                    tracing::error!(%error, "could not save the annotation row");
                }
                if let Some((dir, summary, json)) = text_only_files
                    && let Err(error) = bundles::write_bundle_files(&dir, &summary, &json)
                {
                    tracing::warn!(%error, "could not write the bundle files");
                }
                ann_session::remove_candidates(&candidates_dir);
                let now = chrono::Utc::now().timestamp_millis();
                bundles::cleanup(&store, &root, retention, now);
                match failed_record {
                    // The words failed: the audio stays, with a history row that can retry it.
                    Some(record) => history::save_failure(app, store, record, spool),
                    None => {
                        if let Err(error) = std::fs::remove_file(&spool)
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            tracing::warn!(%error, "could not delete spool file");
                        }
                    }
                }
            });
        });
    }

    /// Ends an annotation session without a trace: screen, pointer, engine, audio, folder.
    fn drop_all(&mut self, session: SessionId) {
        let Some(mut ctx) = self.sessions.remove(&session) else {
            return;
        };
        self.cancel_stream(&ctx);
        ctx.cancel.cancel();
        if ctx.recorder.is_some() || ctx.audio_tx.is_some() {
            Self::stop_recorder(&mut ctx, self.tx.clone(), session);
        }
        let root = self.services.paths().annotations.clone();
        let annotation = ctx.annotation.take();
        let screen_rx = ctx.screen_done.as_ref().map(|(_, rx)| rx.clone());
        let mut rx = ctx.audio_rx;
        let spool = ctx.spool_path;
        tauri::async_runtime::spawn(async move {
            let _ = tokio::time::timeout(OPEN_TIMEOUT, rx.wait_for(Option::is_some)).await;
            // The screen stop (and its candidate writes) must be over before the folder goes.
            if let Some(mut screen_rx) = screen_rx {
                let _ =
                    tokio::time::timeout(SCREEN_STOP_TIMEOUT, screen_rx.wait_for(Option::is_some))
                        .await;
            }
            tauri::async_runtime::spawn_blocking(move || {
                if let Some(annotation) = annotation {
                    if let Some(pointer) = annotation.pointer {
                        pointer.stop();
                    }
                    if let Some(screen) = annotation.screen {
                        let _ = screen.stop();
                    }
                    ann_session::remove_bundle(&root, &annotation.folder);
                }
                if let Err(error) = std::fs::remove_file(&spool)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(%error, "could not delete spool file");
                }
            });
        });
    }

    /// Emits the pill state and shows or hides the pill window.
    fn publish(&mut self) {
        let view = self.machine.view(self.now());
        let annotation = (self.machine.active_kind() == Some(SessionKind::Annotation)).then(|| {
            let images = self
                .machine
                .active_session()
                .and_then(|s| self.sessions.get(&s))
                .and_then(|c| c.annotation.as_ref())
                .map_or(0, AnnotationCtx::images);
            AnnotationPill {
                images,
                screen_lost: self.machine.screen_lost(),
            }
        });
        let state = PillState {
            view,
            language: self.last_language,
            refine: self.machine.active_refine(),
            annotation,
        };
        self.services.set_pill_state(state.clone());
        // Only the pill page listens; the main window does not pay for the event.
        if let Err(error) = state.emit_to(&self.app, crate::windows::PILL_LABEL) {
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
            if let Some(refine) = &ctx.refine {
                refine.cancel.cancel();
            }
            if let Some(annotation) = ctx.annotation {
                if let Some(pointer) = annotation.pointer {
                    pointer.stop();
                }
                if let Some(screen) = annotation.screen {
                    let _ = screen.stop();
                }
            }
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

/// What the plan sees when there was no screen capture at all.
fn empty_stopped() -> screen::Stopped {
    screen::Stopped {
        frames: Vec::new(),
        kept: Vec::new(),
        recent: Vec::new(),
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
