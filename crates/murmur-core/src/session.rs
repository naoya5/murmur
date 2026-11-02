//! SOT: session-state-machine
//! What: the state of one dictation (record -> transcribe -> deliver) and every transition.
//! Why: Esc, timeouts, stale results and stale timers are decided in one pure place that
//!      unit tests cover completely (PLAN 2.4, guardrails G5 and G9).
//! Where: `src-tauri/src/controller.rs` feeds events into `Machine::step` and executes the
//!        returned effects. Tests live at the end of this file.
//!
//! Transition table (the code below implements exactly this):
//!
//! | State                      | Event                          | Next / effects                                   |
//! |----------------------------|--------------------------------|--------------------------------------------------|
//! | Idle                       | HotkeyDown                     | Recording; StartCapture, SetEscCapture(true), max timer |
//! | Recording                  | HotkeyDown (toggle) / HotkeyUp (push-to-talk) / PillClicked / max timer / MicDisconnected | Transcribing; StopCapture, Transcribe, timeout timer |
//! | Recording                  | Esc                            | CancelPending(from Recording); PauseCapture, 3 s timer |
//! | Recording / CancelPending(Rec) | CaptureFailed              | Idle + notice; DropAudio, SetEscCapture(false)   |
//! | Transcribing / CancelPending(Tx) | CaptureFailed            | Idle + notice; CancelTranscription, DropAudio, SetEscCapture(false) |
//! | CancelPending(Rec)         | Esc / PillClicked              | Recording; ResumeCapture, max timer re-armed with the time left |
//! | CancelPending(Rec)         | HotkeyDown                     | Transcribing (audio heard during the pause is dropped) |
//! | CancelPending(Rec)         | HotkeyUp                       | ignored: releasing a push-to-talk key after Esc is not a decision; the countdown or a press decides |
//! | CancelPending(Rec)         | MicDisconnected                | stays, marks the microphone lost; Esc / PillClicked then transcribe what was recorded |
//! | CancelPending(any)         | 3 s timer                      | Idle; audio and result discarded, nothing saved  |
//! | Transcribing               | Transcribed(Text)              | Delivering; SetEscCapture(false), Deliver, delivery timer |
//! | Transcribing               | Transcribed(NoSpeech)          | Idle + notice; DropAudio                         |
//! | Transcribing               | Transcribed(Failed) / timeout  | Idle + notice; SaveFailure (audio kept), engine cancelled on timeout |
//! | Transcribing               | Esc                            | CancelPending(from Transcribing); engine keeps running |
//! | Transcribing / CancelPending(Tx) | ModelLoading             | timeout re-armed with the 60 s model load budget |
//! | CancelPending(Tx)          | Transcribed / timeout          | stays; the outcome is held                       |
//! | CancelPending(Tx)          | Esc / PillClicked              | the held outcome is handled, or back to Transcribing |
//! | Delivering                 | Delivered / delivery timer     | Idle + notice; SaveSuccess (after the paste, G9) |
//! | Idle with notice           | notice timer                   | Idle (pill hides)                                |
//! | Idle with notice           | HotkeyDown                     | Recording (the notice is replaced by the pill)   |
//! | Transcribing / Delivering  | HotkeyDown / HotkeyUp          | ignored                                          |
//!
//! Every other (state, event) pair is ignored. In push-to-talk, `HotkeyDown` while recording is
//! key repeat and is ignored; after a resume with the key already up, press and release stops. A result or timer that carries an id other than
//! the current one is stale and ignored. Esc is captured exactly while the state is Recording,
//! CancelPending, Transcribing or Composing, so Esc reaches other apps the rest of the time.
//!
//! Annotation sessions (experimental; `SessionKind::Annotation`) record the screen next to the
//! microphone and end with a composed bundle instead of plain text. Their rows:
//!
//! | State                      | Event                          | Next / effects                                   |
//! |----------------------------|--------------------------------|--------------------------------------------------|
//! | Idle                       | AnnotationsHotkey              | Recording(Annotation); StartCapture, StartScreen, ShowOverlay, SetEscCapture(true), 3 min timer |
//! | Idle                       | AnnotationsRefused             | Idle + notice; nothing is recorded               |
//! | Recording(Annotation)      | AnnotationsHotkey / PillClicked / max timer / MicDisconnected | Transcribing(Annotation); StopCapture, StopScreen, HideOverlay, PlanKeyframes, TranscribeSegments, timeout timer |
//! | Recording(Annotation)      | ScreenFailed                   | stays, marks the screen lost; the pictures so far are used at stop |
//! | Recording(Annotation)      | Esc                            | CancelPending; PauseCapture, PauseScreen, 3 s timer |
//! | Recording(Annotation)      | HotkeyDown / HotkeyUp          | ignored (one recording at a time)                |
//! | Recording(Dictation) / Transcribing / Composing / Delivering | AnnotationsHotkey | ignored                           |
//! | Recording(Annotation) / CancelPending(Rec) | CaptureFailed  | Idle + notice; StopScreen, HideOverlay, DropAll, SetEscCapture(false) |
//! | CancelPending(Rec, Annotation) | Esc / PillClicked          | Recording; ResumeCapture, ResumeScreen, max timer re-armed |
//! | CancelPending(Rec, Annotation) | AnnotationsHotkey          | Transcribing(Annotation), as a stop               |
//! | CancelPending(Rec, Annotation) | 3 s timer                  | Idle; StopCapture, StopScreen, HideOverlay, DropAll |
//! | Transcribing(Annotation)   | Planned                        | stays; remembers how many keyframes and marks there are |
//! | Transcribing(Annotation)   | Transcribed(Text)              | Composing; Compose(words), 5 s timer             |
//! | Transcribing(Annotation)   | Transcribed(NoSpeech)          | Composing without words when there is a mark (or the plan is unknown); else Idle + notice, DropAll |
//! | Transcribing(Annotation)   | Transcribed(Failed) / timeout  | Composing; Compose(failed words); the audio is kept |
//! | Transcribing(Annotation) / CancelPending(Tx) | CaptureFailed | Idle + notice; CancelTranscription, DropAll     |
//! | Composing                  | Composed(Bundle)               | Delivering; SetEscCapture(false), DeliverBundle, delivery timer |
//! | Composing                  | Composed(Failed) / 5 s timer   | Delivering; SetEscCapture(false), DeliverText (words only, no path) |
//! | Composing                  | Esc                            | CancelPending; the compose keeps running and its outcome is held |
//! | CancelPending(Compose)     | Esc / PillClicked              | the held outcome is handled, or back to Composing |
//! | CancelPending(Compose/Tx, Annotation) | 3 s timer           | Idle; DropAll                                    |
//! | Delivering(Annotation)     | Delivered / delivery timer     | Idle + notice; SaveAnnotation (after the paste, G9) |
//! | any annotation state but Delivering | AnnotationsDisabled    | Idle, no notice; the screen and the overlay are released, the engine cancelled, DropAll |
//!
//! Every dictation row above is unchanged by the annotation rows.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::Millis;
use crate::settings::RecordMode;

pub const CANCEL_WINDOW_MS: Millis = 3_000;
/// The longest recording. At this length the recording stops and is transcribed.
pub const MAX_RECORDING_MS: Millis = 10 * 60 * 1_000;
/// The longest annotation recording (screen pictures are costly to keep).
pub const MAX_ANNOTATION_RECORDING_MS: Millis = 3 * 60 * 1_000;
/// Budget for drawing the keyframes and the contact sheet. Past it, the words go out alone.
pub const COMPOSE_TIMEOUT_MS: Millis = 5_000;
/// Transcription timeout = base + factor x recorded length.
pub const TRANSCRIBE_TIMEOUT_BASE_MS: Millis = 15_000;
pub const TRANSCRIBE_TIMEOUT_PER_RECORDED_MS: f64 = 0.5;
/// Safety net if the output adapter never reports back (its own paste timeout is 1 s).
pub const DELIVERY_TIMEOUT_MS: Millis = 2_500;
/// Extra budget when the model must be loaded before transcribing (model load 60 s).
pub const MODEL_LOAD_TIMEOUT_MS: Millis = 60_000;
/// How long the pill shows a result before it hides.
pub const NOTICE_OK_MS: Millis = 800;
pub const NOTICE_ERROR_MS: Millis = 2_800;

/// One recording, from hotkey to delivery. Results that carry another id are stale.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Type,
)]
pub struct SessionId(pub u64);

/// A timer armed by `Effect::ArmTimer`. A fired timer whose id the state no longer holds is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct TimerId(pub u64);

/// Final text of a transcription. `Debug` never prints the content (guardrail G7).
#[derive(Clone, PartialEq, Eq)]
pub struct TranscriptText(pub String);

impl std::fmt::Debug for TranscriptText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted {} chars>", self.0.chars().count())
    }
}

/// Why the microphone could not record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CaptureError {
    PermissionDenied,
    NoDevice,
    OpenTimeout,
    DeviceError,
}

/// Why a transcription failed. The audio is kept and the history shows a retry button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The engine did not finish within the transcription timeout.
    Timeout,
    /// Speech was detected but the model returned no text.
    EmptyResult,
    /// The model for the language is not downloaded.
    ModelMissing,
    /// The model file could not be loaded (corrupt, out of memory, device error).
    ModelLoadFailed,
    /// Any other engine error.
    Engine,
    /// The captured audio could not be read back.
    Audio,
}

/// What the engine produced for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscribeOutcome {
    Text(TranscriptText),
    /// Voice activity detection found no speech. Nothing is saved.
    NoSpeech,
    Failed(FailureKind),
}

/// Why the text was only copied and not pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CopyReason {
    AutoPasteOff,
    NoAccessibility,
    SecureInput,
    PasteFailed,
    /// The app the annotation was started in is not in front (it quit, or macOS refused to
    /// bring it back), so nothing was pasted anywhere else.
    TargetNotInFront,
}

/// What the output adapter did with the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Pasted,
    Copied {
        reason: CopyReason,
    },
    /// The output adapter did not answer in time. The text is most likely on the clipboard.
    Unconfirmed,
}

/// What a session records and delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// Microphone only; the text is pasted.
    Dictation,
    /// Microphone and screen; a bundle of pictures and words is pasted (experimental).
    Annotation,
}

/// Why an annotation recording could not start. Decided by the controller before the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationsRefusal {
    ScreenRecordingDenied,
    MicrophoneDenied,
}

/// What `Effect::Compose` writes as the words of the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeWords {
    /// The transcribed segments held by the controller.
    Transcribed,
    /// Nothing was said; the bundle has pictures and marks only.
    None,
    /// The transcription failed; the summary says so in place of the words.
    Failed,
}

/// What the compose step produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeOutcome {
    /// Pictures and summary are ready. `images` is how many keyframes were drawn.
    Bundle { images: u32 },
    /// No picture could be made; only the words can be delivered.
    Failed,
}

/// What `Effect::PlanKeyframes` found, reported back with `Event::Planned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanCounts {
    pub keyframes: u32,
    pub marks: u32,
}

/// Short feedback the pill shows before it hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    Delivered {
        outcome: DeliveryOutcome,
    },
    NoSpeech,
    /// The audio is saved in the history and can be retried.
    Failed {
        failure: FailureKind,
    },
    CaptureFailed {
        error: CaptureError,
    },
    /// An annotation bundle went out. `images` is 0 when only the words were pasted.
    AnnotationDelivered {
        outcome: DeliveryOutcome,
        images: u32,
    },
    /// An annotation recording had neither speech nor marks; nothing is kept.
    AnnotationEmpty,
    /// An annotation recording could not start (a permission is missing).
    AnnotationsRefused {
        reason: AnnotationsRefusal,
    },
}

impl Notice {
    pub fn duration_ms(self) -> Millis {
        match self {
            Notice::Delivered {
                outcome: DeliveryOutcome::Pasted,
            }
            | Notice::AnnotationDelivered {
                outcome: DeliveryOutcome::Pasted,
                ..
            } => NOTICE_OK_MS,
            Notice::Delivered {
                outcome: DeliveryOutcome::Copied { .. },
            }
            | Notice::AnnotationDelivered {
                outcome: DeliveryOutcome::Copied { .. },
                ..
            } => NOTICE_ERROR_MS,
            Notice::Delivered {
                outcome: DeliveryOutcome::Unconfirmed,
            }
            | Notice::AnnotationDelivered {
                outcome: DeliveryOutcome::Unconfirmed,
                ..
            } => NOTICE_ERROR_MS,
            Notice::NoSpeech | Notice::AnnotationEmpty => NOTICE_OK_MS * 2,
            Notice::Failed { .. }
            | Notice::CaptureFailed { .. }
            | Notice::AnnotationsRefused { .. } => NOTICE_ERROR_MS,
        }
    }
}

/// Inputs to the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The main hotkey went down. `mode` is read from settings at the moment of the press.
    HotkeyDown {
        mode: RecordMode,
    },
    /// The main hotkey went up (only push-to-talk uses it).
    HotkeyUp,
    PillClicked,
    Esc,
    TimerFired(TimerId),
    CaptureFailed {
        session: SessionId,
        error: CaptureError,
    },
    MicDisconnected {
        session: SessionId,
    },
    Transcribed {
        session: SessionId,
        outcome: TranscribeOutcome,
    },
    Delivered {
        session: SessionId,
        outcome: DeliveryOutcome,
    },
    /// The model for this session is still loading: the transcription timeout is re-armed once
    /// with the model load budget added, so a normal first load is not reported as a failure.
    ModelLoading {
        session: SessionId,
    },
    /// The annotations hotkey was pressed and the controller found the permissions in place.
    /// Press to start, press again to stop; there is no push-to-talk for annotations.
    AnnotationsHotkey,
    /// The annotations hotkey was pressed but a permission is missing.
    AnnotationsRefused {
        reason: AnnotationsRefusal,
    },
    /// The screen capture stopped working. The recording goes on without new pictures.
    ScreenFailed {
        session: SessionId,
    },
    /// `Effect::PlanKeyframes` is done.
    Planned {
        session: SessionId,
        counts: PlanCounts,
    },
    /// `Effect::Compose` is done.
    Composed {
        session: SessionId,
        outcome: ComposeOutcome,
    },
    /// The user switched the feature off: an annotation session in progress ends at once and
    /// keeps nothing. A delivery in flight finishes. Dictation sessions ignore it.
    AnnotationsDisabled,
}

/// Instructions for the controller. Executed in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Open the microphone and the spool file. Read language and device from settings now.
    StartCapture {
        session: SessionId,
    },
    /// Keep the stream open but drop incoming audio (cancel countdown).
    PauseCapture {
        session: SessionId,
    },
    ResumeCapture {
        session: SessionId,
    },
    /// Close the microphone. The audio stays in memory and in the spool until a later effect.
    StopCapture {
        session: SessionId,
    },
    /// Run the engine on the session audio and report `Event::Transcribed`.
    Transcribe {
        session: SessionId,
    },
    /// Ask the engine to stop work for this session. Any late result is stale.
    CancelTranscription {
        session: SessionId,
    },
    /// Put the text on the clipboard and paste it, then report `Event::Delivered`.
    Deliver {
        session: SessionId,
        text: TranscriptText,
    },
    /// After delivery: write history and daily stats, delete the spool file.
    SaveSuccess {
        session: SessionId,
        outcome: DeliveryOutcome,
    },
    /// Write a failed history row and keep the audio for a retry.
    SaveFailure {
        session: SessionId,
        failure: FailureKind,
    },
    /// Delete the audio and forget the session. Nothing goes to the history.
    DropAudio {
        session: SessionId,
    },
    /// Register (true) or unregister (false) the global Esc hotkey.
    SetEscCapture(bool),
    ArmTimer {
        id: TimerId,
        after_ms: Millis,
    },
    // --- annotation sessions ---
    /// Start taking pictures of the screen under the mouse; report `Event::ScreenFailed` on error.
    StartScreen {
        session: SessionId,
    },
    /// Keep the screen stream but drop its pictures (cancel countdown).
    PauseScreen {
        session: SessionId,
    },
    ResumeScreen {
        session: SessionId,
    },
    /// Stop taking pictures. The candidates stay on disk until a later effect.
    StopScreen {
        session: SessionId,
    },
    /// Bring the drawing layer to the front (the window exists already).
    ShowOverlay,
    HideOverlay,
    /// Choose the keyframes and assign the marks from the signals recorded so far, then report
    /// `Event::Planned`. Pure work in `annotations.rs`; runs after `StopScreen` is complete.
    PlanKeyframes {
        session: SessionId,
    },
    /// Cut the audio at the keyframe boundaries and run the engine on each segment, then report
    /// `Event::Transcribed` (text means at least one segment has words).
    TranscribeSegments {
        session: SessionId,
    },
    /// Draw the keyframes and the contact sheet and write the summary, then report
    /// `Event::Composed`. Must finish within `COMPOSE_TIMEOUT_MS`.
    Compose {
        session: SessionId,
        words: ComposeWords,
    },
    /// Paste the bundle (pictures, then the summary with the path), then report `Event::Delivered`.
    DeliverBundle {
        session: SessionId,
    },
    /// Paste the summary without a picture path (the compose failed), then report
    /// `Event::Delivered`.
    DeliverText {
        session: SessionId,
    },
    /// After delivery: write the annotation row, delete the unused candidates, run the retention
    /// cleanup.
    SaveAnnotation {
        session: SessionId,
        outcome: DeliveryOutcome,
    },
    /// Delete the audio, the pictures and the folder; forget the session. Nothing is saved.
    DropAll {
        session: SessionId,
    },
}

/// What the pill shows. Derived from the state; the controller emits it after every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PillView {
    Hidden,
    Recording {
        elapsed_ms: Millis,
        mode: RecordMode,
    },
    CancelPending {
        remaining_ms: Millis,
        from_recording: bool,
    },
    Transcribing,
    Delivering,
    Notice {
        notice: Notice,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub session: SessionId,
    pub kind: SessionKind,
    pub mode: RecordMode,
    /// Recorded time before the last resume.
    pub active_before_ms: Millis,
    pub resumed_at: Millis,
    pub max_timer: TimerId,
    /// The microphone went away during the cancel countdown; resuming transcribes instead.
    pub mic_lost: bool,
    /// Annotation only: the screen capture failed; the recording goes on without new pictures.
    pub screen_failed: bool,
}

impl Recording {
    fn recorded_ms(&self, now: Millis) -> Millis {
        self.active_before_ms + now.saturating_sub(self.resumed_at)
    }
}

/// The longest recording for a kind of session.
pub fn max_recording_ms(kind: SessionKind) -> Millis {
    match kind {
        SessionKind::Dictation => MAX_RECORDING_MS,
        SessionKind::Annotation => MAX_ANNOTATION_RECORDING_MS,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcribing {
    pub session: SessionId,
    pub kind: SessionKind,
    pub recorded_ms: Millis,
    pub timeout_timer: TimerId,
    /// Annotation only: what `PlanKeyframes` found, once `Event::Planned` arrived.
    pub plan: Option<PlanCounts>,
}

/// An annotation session whose pictures and summary are being drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composing {
    pub session: SessionId,
    pub timeout_timer: TimerId,
}

/// What `Delivering` hands over, and which save follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Text,
    /// `images` is 0 when the compose failed and only the words go out.
    Bundle {
        images: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paused {
    /// `rec.active_before_ms` already holds the time recorded until the pause.
    Recording(Recording),
    Transcribing {
        tx: Transcribing,
        held: Option<TranscribeOutcome>,
    },
    Composing {
        composing: Composing,
        held: Option<ComposeOutcome>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle {
        notice: Option<(Notice, TimerId)>,
    },
    Recording(Recording),
    CancelPending {
        session: SessionId,
        timer: TimerId,
        deadline: Millis,
        paused: Paused,
    },
    Transcribing(Transcribing),
    Composing(Composing),
    Delivering {
        session: SessionId,
        timer: TimerId,
        delivery: Delivery,
    },
}

/// The session state machine. Deterministic: same events and times give same effects.
#[derive(Debug, Clone)]
pub struct Machine {
    state: State,
    next_session: u64,
    next_timer: u64,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub fn new() -> Self {
        Self {
            state: State::Idle { notice: None },
            next_session: 1,
            next_timer: 1,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// The session that owns the microphone or the engine now, if any.
    pub fn active_session(&self) -> Option<SessionId> {
        match &self.state {
            State::Idle { .. } => None,
            State::Recording(rec) => Some(rec.session),
            State::CancelPending { session, .. } | State::Delivering { session, .. } => {
                Some(*session)
            }
            State::Transcribing(tx) => Some(tx.session),
            State::Composing(composing) => Some(composing.session),
        }
    }

    /// The kind of the active session, if any.
    pub fn active_kind(&self) -> Option<SessionKind> {
        match &self.state {
            State::Idle { .. } => None,
            State::Recording(rec) => Some(rec.kind),
            State::Transcribing(tx) => Some(tx.kind),
            State::Composing(_) => Some(SessionKind::Annotation),
            State::Delivering { delivery, .. } => Some(match delivery {
                Delivery::Text => SessionKind::Dictation,
                Delivery::Bundle { .. } => SessionKind::Annotation,
            }),
            State::CancelPending { paused, .. } => Some(paused_kind(paused)),
        }
    }

    /// Whether the active annotation recording lost its screen capture (`Event::ScreenFailed`).
    pub fn screen_lost(&self) -> bool {
        match &self.state {
            State::Recording(rec) => rec.screen_failed,
            State::CancelPending {
                paused: Paused::Recording(rec),
                ..
            } => rec.screen_failed,
            _ => false,
        }
    }

    /// Whether the global Esc hotkey must be registered in the current state.
    pub fn wants_esc(&self) -> bool {
        matches!(
            self.state,
            State::Recording(_)
                | State::CancelPending { .. }
                | State::Transcribing(_)
                | State::Composing(_)
        )
    }

    pub fn view(&self, now: Millis) -> PillView {
        match &self.state {
            State::Idle { notice: None } => PillView::Hidden,
            State::Idle {
                notice: Some((notice, _)),
            } => PillView::Notice { notice: *notice },
            State::Recording(rec) => PillView::Recording {
                elapsed_ms: rec.recorded_ms(now),
                mode: rec.mode,
            },
            State::CancelPending {
                deadline, paused, ..
            } => PillView::CancelPending {
                remaining_ms: deadline.saturating_sub(now),
                from_recording: matches!(paused, Paused::Recording(_)),
            },
            // The compose step shows the same busy animation as the transcription.
            State::Transcribing(_) | State::Composing(_) => PillView::Transcribing,
            State::Delivering { .. } => PillView::Delivering,
        }
    }

    pub fn step(&mut self, event: Event, now: Millis) -> Vec<Effect> {
        let mut fx = Vec::new();
        let current = std::mem::replace(&mut self.state, State::Idle { notice: None });
        self.state = self.transition(current, event, now, &mut fx);
        fx
    }

    fn timer(&mut self, after_ms: Millis, fx: &mut Vec<Effect>) -> TimerId {
        let id = TimerId(self.next_timer);
        self.next_timer += 1;
        fx.push(Effect::ArmTimer { id, after_ms });
        id
    }

    fn idle_with(&mut self, notice: Option<Notice>, fx: &mut Vec<Effect>) -> State {
        match notice {
            None => State::Idle { notice: None },
            Some(n) => {
                let timer = self.timer(n.duration_ms(), fx);
                State::Idle {
                    notice: Some((n, timer)),
                }
            }
        }
    }

    fn start_recording(
        &mut self,
        kind: SessionKind,
        mode: RecordMode,
        now: Millis,
        fx: &mut Vec<Effect>,
    ) -> State {
        let session = SessionId(self.next_session);
        self.next_session += 1;
        fx.push(Effect::StartCapture { session });
        if kind == SessionKind::Annotation {
            fx.push(Effect::StartScreen { session });
            fx.push(Effect::ShowOverlay);
        }
        fx.push(Effect::SetEscCapture(true));
        let max_timer = self.timer(max_recording_ms(kind), fx);
        State::Recording(Recording {
            session,
            kind,
            mode,
            active_before_ms: 0,
            resumed_at: now,
            max_timer,
            mic_lost: false,
            screen_failed: false,
        })
    }

    fn stop_and_transcribe(
        &mut self,
        session: SessionId,
        kind: SessionKind,
        recorded_ms: Millis,
        fx: &mut Vec<Effect>,
    ) -> State {
        fx.push(Effect::StopCapture { session });
        match kind {
            SessionKind::Dictation => fx.push(Effect::Transcribe { session }),
            SessionKind::Annotation => {
                fx.push(Effect::StopScreen { session });
                fx.push(Effect::HideOverlay);
                fx.push(Effect::PlanKeyframes { session });
                fx.push(Effect::TranscribeSegments { session });
            }
        }
        let timeout = TRANSCRIBE_TIMEOUT_BASE_MS
            + (recorded_ms as f64 * TRANSCRIBE_TIMEOUT_PER_RECORDED_MS) as Millis;
        let timeout_timer = self.timer(timeout, fx);
        State::Transcribing(Transcribing {
            session,
            kind,
            recorded_ms,
            timeout_timer,
            plan: None,
        })
    }

    /// Replaces the transcription timer with one that also covers a model load.
    fn extend_for_model_load(&mut self, tx: Transcribing, fx: &mut Vec<Effect>) -> Transcribing {
        let timeout = MODEL_LOAD_TIMEOUT_MS
            + TRANSCRIBE_TIMEOUT_BASE_MS
            + (tx.recorded_ms as f64 * TRANSCRIBE_TIMEOUT_PER_RECORDED_MS) as Millis;
        let timeout_timer = self.timer(timeout, fx);
        Transcribing {
            timeout_timer,
            ..tx
        }
    }

    fn pause(
        &mut self,
        session: SessionId,
        paused: Paused,
        now: Millis,
        fx: &mut Vec<Effect>,
    ) -> State {
        let timer = self.timer(CANCEL_WINDOW_MS, fx);
        State::CancelPending {
            session,
            timer,
            deadline: now + CANCEL_WINDOW_MS,
            paused,
        }
    }

    /// Ends a paused session without saving anything (countdown expiry, feature switched off).
    fn abandon_paused(session: SessionId, paused: Paused, fx: &mut Vec<Effect>) -> State {
        let kind = paused_kind(&paused);
        match paused {
            Paused::Recording(_) => {
                fx.push(Effect::StopCapture { session });
                Self::release_screen(session, kind, fx);
            }
            Paused::Transcribing { held: None, .. } => {
                fx.push(Effect::CancelTranscription { session })
            }
            Paused::Transcribing { held: Some(_), .. } | Paused::Composing { .. } => {}
        }
        fx.push(Effect::SetEscCapture(false));
        Self::drop_session(session, kind, fx);
        State::Idle { notice: None }
    }

    /// Ends a session without saving anything: the audio for a dictation, everything for an
    /// annotation.
    fn drop_session(session: SessionId, kind: SessionKind, fx: &mut Vec<Effect>) {
        match kind {
            SessionKind::Dictation => fx.push(Effect::DropAudio { session }),
            SessionKind::Annotation => fx.push(Effect::DropAll { session }),
        }
    }

    /// Releases the screen and the drawing layer of an annotation recording that ends early.
    fn release_screen(session: SessionId, kind: SessionKind, fx: &mut Vec<Effect>) {
        if kind == SessionKind::Annotation {
            fx.push(Effect::StopScreen { session });
            fx.push(Effect::HideOverlay);
        }
    }

    /// Handles a transcription outcome while transcribing (or when a held one is released).
    fn finish_transcription(
        &mut self,
        session: SessionId,
        kind: SessionKind,
        plan: Option<PlanCounts>,
        outcome: TranscribeOutcome,
        fx: &mut Vec<Effect>,
    ) -> State {
        match kind {
            SessionKind::Dictation => match outcome {
                TranscribeOutcome::Text(text) => {
                    fx.push(Effect::SetEscCapture(false));
                    fx.push(Effect::Deliver { session, text });
                    let timer = self.timer(DELIVERY_TIMEOUT_MS, fx);
                    State::Delivering {
                        session,
                        timer,
                        delivery: Delivery::Text,
                    }
                }
                TranscribeOutcome::NoSpeech => {
                    fx.push(Effect::SetEscCapture(false));
                    fx.push(Effect::DropAudio { session });
                    self.idle_with(Some(Notice::NoSpeech), fx)
                }
                TranscribeOutcome::Failed(failure) => {
                    fx.push(Effect::SetEscCapture(false));
                    fx.push(Effect::SaveFailure { session, failure });
                    self.idle_with(Some(Notice::Failed { failure }), fx)
                }
            },
            SessionKind::Annotation => match outcome {
                TranscribeOutcome::Text(_) => self.compose(session, ComposeWords::Transcribed, fx),
                // Without words the pictures still carry the marks. When the plan has not been
                // reported yet the marks are unknown, and keeping them wins over dropping them.
                TranscribeOutcome::NoSpeech if plan.is_none_or(|p| p.marks > 0) => {
                    self.compose(session, ComposeWords::None, fx)
                }
                TranscribeOutcome::NoSpeech => {
                    fx.push(Effect::SetEscCapture(false));
                    fx.push(Effect::DropAll { session });
                    self.idle_with(Some(Notice::AnnotationEmpty), fx)
                }
                // The pictures go out with a note in place of the words; the audio is kept.
                TranscribeOutcome::Failed(_) => self.compose(session, ComposeWords::Failed, fx),
            },
        }
    }

    fn compose(&mut self, session: SessionId, words: ComposeWords, fx: &mut Vec<Effect>) -> State {
        fx.push(Effect::Compose { session, words });
        let timeout_timer = self.timer(COMPOSE_TIMEOUT_MS, fx);
        State::Composing(Composing {
            session,
            timeout_timer,
        })
    }

    /// Handles a compose outcome while composing (or when a held one is released).
    fn finish_compose(
        &mut self,
        session: SessionId,
        outcome: ComposeOutcome,
        fx: &mut Vec<Effect>,
    ) -> State {
        fx.push(Effect::SetEscCapture(false));
        let delivery = match outcome {
            ComposeOutcome::Bundle { images } => {
                fx.push(Effect::DeliverBundle { session });
                Delivery::Bundle { images }
            }
            ComposeOutcome::Failed => {
                fx.push(Effect::DeliverText { session });
                Delivery::Bundle { images: 0 }
            }
        };
        let timer = self.timer(DELIVERY_TIMEOUT_MS, fx);
        State::Delivering {
            session,
            timer,
            delivery,
        }
    }

    fn finish_delivery(
        &mut self,
        session: SessionId,
        delivery: Delivery,
        outcome: DeliveryOutcome,
        fx: &mut Vec<Effect>,
    ) -> State {
        match delivery {
            Delivery::Text => {
                fx.push(Effect::SaveSuccess { session, outcome });
                self.idle_with(Some(Notice::Delivered { outcome }), fx)
            }
            Delivery::Bundle { images } => {
                fx.push(Effect::SaveAnnotation { session, outcome });
                self.idle_with(Some(Notice::AnnotationDelivered { outcome, images }), fx)
            }
        }
    }

    fn transition(
        &mut self,
        state: State,
        event: Event,
        now: Millis,
        fx: &mut Vec<Effect>,
    ) -> State {
        match state {
            State::Idle { notice } => match event {
                Event::HotkeyDown { mode } => {
                    self.start_recording(SessionKind::Dictation, mode, now, fx)
                }
                Event::AnnotationsHotkey => {
                    self.start_recording(SessionKind::Annotation, RecordMode::Toggle, now, fx)
                }
                Event::AnnotationsRefused { reason } => {
                    self.idle_with(Some(Notice::AnnotationsRefused { reason }), fx)
                }
                Event::TimerFired(id) if notice.is_some_and(|(_, t)| t == id) => {
                    State::Idle { notice: None }
                }
                _ => State::Idle { notice },
            },

            State::Recording(rec) => match event {
                Event::HotkeyDown { .. }
                    if rec.kind == SessionKind::Dictation && rec.mode == RecordMode::Toggle =>
                {
                    self.stop_and_transcribe(rec.session, rec.kind, rec.recorded_ms(now), fx)
                }
                Event::HotkeyUp
                    if rec.kind == SessionKind::Dictation && rec.mode == RecordMode::PushToTalk =>
                {
                    self.stop_and_transcribe(rec.session, rec.kind, rec.recorded_ms(now), fx)
                }
                Event::AnnotationsHotkey if rec.kind == SessionKind::Annotation => {
                    self.stop_and_transcribe(rec.session, rec.kind, rec.recorded_ms(now), fx)
                }
                Event::PillClicked => {
                    self.stop_and_transcribe(rec.session, rec.kind, rec.recorded_ms(now), fx)
                }
                Event::TimerFired(id) if id == rec.max_timer => {
                    self.stop_and_transcribe(rec.session, rec.kind, rec.recorded_ms(now), fx)
                }
                Event::MicDisconnected { session } if session == rec.session => {
                    self.stop_and_transcribe(rec.session, rec.kind, rec.recorded_ms(now), fx)
                }
                Event::ScreenFailed { session }
                    if session == rec.session && rec.kind == SessionKind::Annotation =>
                {
                    State::Recording(Recording {
                        screen_failed: true,
                        ..rec
                    })
                }
                Event::Esc => {
                    fx.push(Effect::PauseCapture {
                        session: rec.session,
                    });
                    if rec.kind == SessionKind::Annotation {
                        fx.push(Effect::PauseScreen {
                            session: rec.session,
                        });
                    }
                    let session = rec.session;
                    let frozen = Recording {
                        active_before_ms: rec.recorded_ms(now),
                        resumed_at: now,
                        ..rec
                    };
                    self.pause(session, Paused::Recording(frozen), now, fx)
                }
                Event::CaptureFailed { session, error } if session == rec.session => {
                    fx.push(Effect::SetEscCapture(false));
                    Self::release_screen(session, rec.kind, fx);
                    Self::drop_session(session, rec.kind, fx);
                    self.idle_with(Some(Notice::CaptureFailed { error }), fx)
                }
                Event::AnnotationsDisabled if rec.kind == SessionKind::Annotation => {
                    Self::abandon_paused(rec.session, Paused::Recording(rec), fx)
                }
                _ => State::Recording(rec),
            },

            State::CancelPending {
                session,
                timer,
                deadline,
                paused,
            } => self.on_cancel_pending(session, timer, deadline, paused, event, now, fx),

            State::Transcribing(tx) => match event {
                Event::CaptureFailed { session, error } if session == tx.session => {
                    fx.push(Effect::CancelTranscription { session });
                    fx.push(Effect::SetEscCapture(false));
                    Self::drop_session(session, tx.kind, fx);
                    self.idle_with(Some(Notice::CaptureFailed { error }), fx)
                }
                Event::Transcribed { session, outcome } if session == tx.session => {
                    self.finish_transcription(session, tx.kind, tx.plan, outcome, fx)
                }
                Event::TimerFired(id) if id == tx.timeout_timer => {
                    fx.push(Effect::CancelTranscription {
                        session: tx.session,
                    });
                    self.finish_transcription(
                        tx.session,
                        tx.kind,
                        tx.plan,
                        TranscribeOutcome::Failed(FailureKind::Timeout),
                        fx,
                    )
                }
                Event::Esc => {
                    let session = tx.session;
                    self.pause(session, Paused::Transcribing { tx, held: None }, now, fx)
                }
                Event::ModelLoading { session } if session == tx.session => {
                    State::Transcribing(self.extend_for_model_load(tx, fx))
                }
                Event::Planned { session, counts }
                    if session == tx.session && tx.kind == SessionKind::Annotation =>
                {
                    State::Transcribing(Transcribing {
                        plan: Some(counts),
                        ..tx
                    })
                }
                Event::AnnotationsDisabled if tx.kind == SessionKind::Annotation => {
                    Self::abandon_paused(tx.session, Paused::Transcribing { tx, held: None }, fx)
                }
                _ => State::Transcribing(tx),
            },

            State::Composing(composing) => match event {
                Event::Composed { session, outcome } if session == composing.session => {
                    self.finish_compose(session, outcome, fx)
                }
                Event::TimerFired(id) if id == composing.timeout_timer => {
                    self.finish_compose(composing.session, ComposeOutcome::Failed, fx)
                }
                Event::Esc => {
                    let session = composing.session;
                    self.pause(
                        session,
                        Paused::Composing {
                            composing,
                            held: None,
                        },
                        now,
                        fx,
                    )
                }
                Event::AnnotationsDisabled => Self::abandon_paused(
                    composing.session,
                    Paused::Composing {
                        composing,
                        held: None,
                    },
                    fx,
                ),
                _ => State::Composing(composing),
            },

            State::Delivering {
                session,
                timer,
                delivery,
            } => match event {
                Event::Delivered {
                    session: s,
                    outcome,
                } if s == session => self.finish_delivery(session, delivery, outcome, fx),
                Event::TimerFired(id) if id == timer => {
                    self.finish_delivery(session, delivery, DeliveryOutcome::Unconfirmed, fx)
                }
                _ => State::Delivering {
                    session,
                    timer,
                    delivery,
                },
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_cancel_pending(
        &mut self,
        session: SessionId,
        timer: TimerId,
        deadline: Millis,
        paused: Paused,
        event: Event,
        now: Millis,
        fx: &mut Vec<Effect>,
    ) -> State {
        let same = |s: SessionId| s == session;
        match (paused, event) {
            // The countdown ran out: discard everything, save nothing.
            (paused, Event::TimerFired(id)) if id == timer => {
                Self::abandon_paused(session, paused, fx)
            }
            // The feature was switched off under a paused annotation session: the same.
            (paused, Event::AnnotationsDisabled)
                if paused_kind(&paused) == SessionKind::Annotation =>
            {
                Self::abandon_paused(session, paused, fx)
            }

            // Undo while recording: resume, or transcribe if the microphone went away.
            (Paused::Recording(rec), Event::Esc | Event::PillClicked) => {
                if rec.mic_lost {
                    return self.stop_and_transcribe(session, rec.kind, rec.active_before_ms, fx);
                }
                fx.push(Effect::ResumeCapture { session });
                if rec.kind == SessionKind::Annotation {
                    fx.push(Effect::ResumeScreen { session });
                }
                let left = max_recording_ms(rec.kind).saturating_sub(rec.active_before_ms);
                let max_timer = self.timer(left, fx);
                State::Recording(Recording {
                    resumed_at: now,
                    max_timer,
                    ..rec
                })
            }
            (Paused::Recording(rec), Event::HotkeyDown { .. })
                if rec.kind == SessionKind::Dictation =>
            {
                self.stop_and_transcribe(session, rec.kind, rec.active_before_ms, fx)
            }
            (Paused::Recording(rec), Event::AnnotationsHotkey)
                if rec.kind == SessionKind::Annotation =>
            {
                self.stop_and_transcribe(session, rec.kind, rec.active_before_ms, fx)
            }
            (Paused::Recording(rec), Event::MicDisconnected { session: s }) if same(s) => {
                State::CancelPending {
                    session,
                    timer,
                    deadline,
                    paused: Paused::Recording(Recording {
                        mic_lost: true,
                        ..rec
                    }),
                }
            }
            (Paused::Recording(rec), Event::ScreenFailed { session: s })
                if same(s) && rec.kind == SessionKind::Annotation =>
            {
                State::CancelPending {
                    session,
                    timer,
                    deadline,
                    paused: Paused::Recording(Recording {
                        screen_failed: true,
                        ..rec
                    }),
                }
            }
            (Paused::Recording(rec), Event::CaptureFailed { session: s, error }) if same(s) => {
                fx.push(Effect::SetEscCapture(false));
                Self::release_screen(session, rec.kind, fx);
                Self::drop_session(session, rec.kind, fx);
                self.idle_with(Some(Notice::CaptureFailed { error }), fx)
            }

            (Paused::Transcribing { tx, .. }, Event::CaptureFailed { session: s, error })
                if same(s) =>
            {
                fx.push(Effect::CancelTranscription { session });
                fx.push(Effect::SetEscCapture(false));
                Self::drop_session(session, tx.kind, fx);
                self.idle_with(Some(Notice::CaptureFailed { error }), fx)
            }

            // While transcribing: hold the outcome until the countdown ends or is undone.
            (
                Paused::Transcribing { tx, held: None },
                Event::Transcribed {
                    session: s,
                    outcome,
                },
            ) if same(s) => State::CancelPending {
                session,
                timer,
                deadline,
                paused: Paused::Transcribing {
                    tx,
                    held: Some(outcome),
                },
            },
            (Paused::Transcribing { tx, held: None }, Event::TimerFired(id))
                if id == tx.timeout_timer =>
            {
                fx.push(Effect::CancelTranscription { session });
                State::CancelPending {
                    session,
                    timer,
                    deadline,
                    paused: Paused::Transcribing {
                        tx,
                        held: Some(TranscribeOutcome::Failed(FailureKind::Timeout)),
                    },
                }
            }
            (Paused::Transcribing { tx, held: None }, Event::ModelLoading { session: s })
                if same(s) =>
            {
                let tx = self.extend_for_model_load(tx, fx);
                State::CancelPending {
                    session,
                    timer,
                    deadline,
                    paused: Paused::Transcribing { tx, held: None },
                }
            }
            (Paused::Transcribing { tx, held }, Event::Planned { session: s, counts })
                if same(s) && tx.kind == SessionKind::Annotation =>
            {
                State::CancelPending {
                    session,
                    timer,
                    deadline,
                    paused: Paused::Transcribing {
                        tx: Transcribing {
                            plan: Some(counts),
                            ..tx
                        },
                        held,
                    },
                }
            }
            (Paused::Transcribing { tx, held }, Event::Esc | Event::PillClicked) => match held {
                Some(outcome) => self.finish_transcription(session, tx.kind, tx.plan, outcome, fx),
                None => State::Transcribing(tx),
            },

            // While composing: the same, with the compose outcome held.
            (
                Paused::Composing {
                    composing,
                    held: None,
                },
                Event::Composed {
                    session: s,
                    outcome,
                },
            ) if same(s) => State::CancelPending {
                session,
                timer,
                deadline,
                paused: Paused::Composing {
                    composing,
                    held: Some(outcome),
                },
            },
            (
                Paused::Composing {
                    composing,
                    held: None,
                },
                Event::TimerFired(id),
            ) if id == composing.timeout_timer => State::CancelPending {
                session,
                timer,
                deadline,
                paused: Paused::Composing {
                    composing,
                    held: Some(ComposeOutcome::Failed),
                },
            },
            (Paused::Composing { composing, held }, Event::Esc | Event::PillClicked) => {
                match held {
                    Some(outcome) => self.finish_compose(session, outcome, fx),
                    None => State::Composing(composing),
                }
            }

            (paused, _) => State::CancelPending {
                session,
                timer,
                deadline,
                paused,
            },
        }
    }
}

fn paused_kind(paused: &Paused) -> SessionKind {
    match paused {
        Paused::Recording(rec) => rec.kind,
        Paused::Transcribing { tx, .. } => tx.kind,
        Paused::Composing { .. } => SessionKind::Annotation,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    const S1: SessionId = SessionId(1);
    const S2: SessionId = SessionId(2);
    const STALE_SESSION: SessionId = SessionId(9_999);
    const STALE_TIMER: TimerId = TimerId(9_999);
    const TOGGLE: Event = Event::HotkeyDown {
        mode: RecordMode::Toggle,
    };
    const PTT: Event = Event::HotkeyDown {
        mode: RecordMode::PushToTalk,
    };

    fn text(s: &str) -> TranscriptText {
        TranscriptText(s.to_owned())
    }

    fn transcribed(session: SessionId, outcome: TranscribeOutcome) -> Event {
        Event::Transcribed { session, outcome }
    }

    fn delivered(session: SessionId, outcome: DeliveryOutcome) -> Event {
        Event::Delivered { session, outcome }
    }

    fn arm(id: TimerId, after_ms: Millis) -> Effect {
        Effect::ArmTimer { id, after_ms }
    }

    /// The timers armed by a batch of effects.
    fn armed(fx: &[Effect]) -> Vec<(TimerId, Millis)> {
        fx.iter()
            .filter_map(|f| match f {
                Effect::ArmTimer { id, after_ms } => Some((*id, *after_ms)),
                _ => None,
            })
            .collect()
    }

    /// The single timer armed by a batch, asserting its duration.
    fn timer_for(fx: &[Effect], after_ms: Millis) -> TimerId {
        let timers = armed(fx);
        assert_eq!(timers.len(), 1, "expected one timer in {fx:?}");
        assert_eq!(timers[0].1, after_ms, "timer duration in {fx:?}");
        timers[0].0
    }

    /// A fake clock plus the timer bookkeeping that the controller will do.
    struct Harness {
        m: Machine,
        now: Millis,
        /// Armed timers with their due time; fired ones are removed.
        timers: Vec<(TimerId, Millis)>,
        fired: Vec<TimerId>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                m: Machine::new(),
                now: 1_000_000,
                timers: Vec::new(),
                fired: Vec::new(),
            }
        }

        fn step(&mut self, event: Event) -> Vec<Effect> {
            let fx = self.m.step(event, self.now);
            for (id, after_ms) in armed(&fx) {
                self.timers.push((id, self.now + after_ms));
            }
            fx
        }

        fn advance(&mut self, ms: Millis) {
            self.now += ms;
        }

        /// Fires one armed timer by id, moving the clock to its due time if it is earlier.
        fn fire(&mut self, id: TimerId) -> Vec<Effect> {
            let i = self
                .timers
                .iter()
                .position(|(t, _)| *t == id)
                .unwrap_or_else(|| panic!("{id:?} is not armed"));
            let (_, due) = self.timers.remove(i);
            self.now = self.now.max(due);
            self.fired.push(id);
            self.step(Event::TimerFired(id))
        }

        /// Fires the earliest armed timer.
        fn fire_next(&mut self) -> Vec<Effect> {
            let id = self
                .timers
                .iter()
                .min_by_key(|(_, due)| *due)
                .map(|(id, _)| *id)
                .expect("an armed timer");
            self.fire(id)
        }

        fn state(&self) -> &State {
            self.m.state()
        }

        fn view(&self) -> PillView {
            self.m.view(self.now)
        }
    }

    // --- state builders ---

    fn recording(mode: RecordMode) -> Harness {
        let mut h = Harness::new();
        let fx = h.step(Event::HotkeyDown { mode });
        assert!(matches!(h.state(), State::Recording(_)), "{fx:?}");
        h
    }

    /// Recording for `recorded_ms`, then Esc.
    fn cancel_rec(mode: RecordMode, recorded_ms: Millis) -> Harness {
        let mut h = recording(mode);
        h.advance(recorded_ms);
        h.step(Event::Esc);
        assert!(matches!(
            h.state(),
            State::CancelPending {
                paused: Paused::Recording(_),
                ..
            }
        ));
        h
    }

    fn transcribing(recorded_ms: Millis) -> Harness {
        let mut h = recording(RecordMode::Toggle);
        h.advance(recorded_ms);
        h.step(TOGGLE);
        assert!(matches!(h.state(), State::Transcribing(_)));
        h
    }

    fn cancel_tx() -> Harness {
        let mut h = transcribing(5_000);
        h.advance(300);
        h.step(Event::Esc);
        assert!(matches!(
            h.state(),
            State::CancelPending {
                paused: Paused::Transcribing { held: None, .. },
                ..
            }
        ));
        h
    }

    fn delivering() -> Harness {
        let mut h = transcribing(5_000);
        h.advance(700);
        h.step(transcribed(S1, TranscribeOutcome::Text(text("hello"))));
        assert!(matches!(h.state(), State::Delivering { .. }));
        h
    }

    fn idle_with_notice() -> Harness {
        let mut h = delivering();
        h.advance(100);
        h.step(delivered(S1, DeliveryOutcome::Pasted));
        assert!(matches!(h.state(), State::Idle { notice: Some(_) }));
        h
    }

    fn timeout_for(recorded_ms: Millis) -> Millis {
        TRANSCRIBE_TIMEOUT_BASE_MS
            + (recorded_ms as f64 * TRANSCRIBE_TIMEOUT_PER_RECORDED_MS) as Millis
    }

    // --- Model loading ---

    #[test]
    fn model_loading_extends_the_transcription_timeout_once() {
        let mut h = transcribing(5_000);
        let State::Transcribing(before) = h.state().clone() else {
            panic!()
        };
        let fx = h.step(Event::ModelLoading { session: S1 });
        let extended = MODEL_LOAD_TIMEOUT_MS + timeout_for(5_000);
        let id = timer_for(&fx, extended);
        let State::Transcribing(after) = h.state().clone() else {
            panic!()
        };
        assert_eq!(after.timeout_timer, id);
        // The old timer is stale now; firing it changes nothing.
        let fx = h.step(Event::TimerFired(before.timeout_timer));
        assert!(fx.is_empty());
        assert!(matches!(h.state(), State::Transcribing(_)));
        // The new timer still times out with a saved failure.
        let fx = h.fire(id);
        assert!(fx.contains(&Effect::SaveFailure {
            session: S1,
            failure: FailureKind::Timeout
        }));
    }

    #[test]
    fn model_loading_for_another_session_is_ignored() {
        let mut h = transcribing(5_000);
        let fx = h.step(Event::ModelLoading {
            session: SessionId(99),
        });
        assert!(fx.is_empty());
    }

    #[test]
    fn model_loading_during_the_countdown_extends_the_held_transcription() {
        let mut h = cancel_tx();
        let fx = h.step(Event::ModelLoading { session: S1 });
        let id = timer_for(&fx, MODEL_LOAD_TIMEOUT_MS + timeout_for(5_000));
        let State::CancelPending {
            paused: Paused::Transcribing { tx, held: None },
            ..
        } = h.state().clone()
        else {
            panic!()
        };
        assert_eq!(tx.timeout_timer, id);
    }

    // --- Idle ---

    #[test]
    fn new_machine_is_hidden_and_wants_no_esc() {
        let h = Harness::new();
        assert_eq!(h.state(), &State::Idle { notice: None });
        assert_eq!(h.view(), PillView::Hidden);
        assert!(!h.m.wants_esc());
        assert_eq!(h.m.active_session(), None);
        assert_eq!(Machine::default().state(), &State::Idle { notice: None });
    }

    #[test]
    fn idle_hotkey_starts_recording() {
        let mut h = Harness::new();
        let fx = h.step(TOGGLE);
        let max = timer_for(&fx, MAX_RECORDING_MS);
        assert_eq!(
            fx,
            vec![
                Effect::StartCapture { session: S1 },
                Effect::SetEscCapture(true),
                arm(max, MAX_RECORDING_MS),
            ]
        );
        assert!(h.m.wants_esc());
        assert_eq!(h.m.active_session(), Some(S1));
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 0,
                mode: RecordMode::Toggle
            }
        );
        h.advance(1_234);
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 1_234,
                mode: RecordMode::Toggle
            }
        );
    }

    #[test]
    fn idle_ignores_everything_but_the_hotkey() {
        let mut h = Harness::new();
        for e in [
            Event::HotkeyUp,
            Event::Esc,
            Event::PillClicked,
            Event::TimerFired(STALE_TIMER),
            Event::CaptureFailed {
                session: S1,
                error: CaptureError::NoDevice,
            },
            Event::MicDisconnected { session: S1 },
            transcribed(S1, TranscribeOutcome::NoSpeech),
            delivered(S1, DeliveryOutcome::Pasted),
        ] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
            assert_eq!(h.state(), &State::Idle { notice: None });
        }
    }

    #[test]
    fn session_ids_increase_per_recording() {
        let mut h = Harness::new();
        h.step(TOGGLE);
        h.step(TOGGLE);
        let fx = h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        assert!(fx.contains(&Effect::DropAudio { session: S1 }));
        let fx = h.step(TOGGLE);
        assert!(fx.contains(&Effect::StartCapture { session: S2 }));
        assert_eq!(h.m.active_session(), Some(S2));
    }

    // --- Recording ---

    #[test]
    fn recording_toggle_hotkey_down_transcribes() {
        let mut h = recording(RecordMode::Toggle);
        h.advance(10_000);
        let fx = h.step(TOGGLE);
        let timeout = timer_for(&fx, 20_000);
        assert_eq!(
            fx,
            vec![
                Effect::StopCapture { session: S1 },
                Effect::Transcribe { session: S1 },
                arm(timeout, 20_000),
            ]
        );
        assert_eq!(
            h.state(),
            &State::Transcribing(Transcribing {
                session: S1,
                kind: SessionKind::Dictation,
                recorded_ms: 10_000,
                timeout_timer: timeout,
                plan: None,
            })
        );
        assert_eq!(h.view(), PillView::Transcribing);
        assert!(h.m.wants_esc(), "Esc still cancels while transcribing");
    }

    #[test]
    fn transcription_timeout_grows_with_the_recording() {
        for recorded in [0, 1_000, 60_000, MAX_RECORDING_MS] {
            let mut h = recording(RecordMode::Toggle);
            h.advance(recorded);
            let fx = h.step(Event::PillClicked);
            timer_for(&fx, timeout_for(recorded));
        }
        assert_eq!(timeout_for(MAX_RECORDING_MS), 315_000);
    }

    #[test]
    fn recording_toggle_ignores_hotkey_up() {
        let mut h = recording(RecordMode::Toggle);
        assert!(h.step(Event::HotkeyUp).is_empty());
        assert!(matches!(h.state(), State::Recording(_)));
    }

    #[test]
    fn recording_push_to_talk_hotkey_up_transcribes() {
        let mut h = recording(RecordMode::PushToTalk);
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 0,
                mode: RecordMode::PushToTalk
            }
        );
        h.advance(3_000);
        let fx = h.step(Event::HotkeyUp);
        assert_eq!(fx[0], Effect::StopCapture { session: S1 });
        assert_eq!(fx[1], Effect::Transcribe { session: S1 });
        timer_for(&fx, timeout_for(3_000));
    }

    #[test]
    fn recording_push_to_talk_ignores_key_repeat() {
        let mut h = recording(RecordMode::PushToTalk);
        h.advance(500);
        for _ in 0..3 {
            assert!(h.step(PTT).is_empty());
            // The mode in the event does not matter: the recording keeps its own.
            assert!(h.step(TOGGLE).is_empty());
        }
        assert!(matches!(h.state(), State::Recording(_)));
        assert_eq!(h.m.active_session(), Some(S1));
    }

    #[test]
    fn recording_stops_on_pill_click_max_timer_and_mic_loss() {
        let stop = |h: &mut Harness, fx: Vec<Effect>| {
            assert_eq!(fx[0], Effect::StopCapture { session: S1 });
            assert_eq!(fx[1], Effect::Transcribe { session: S1 });
            assert_eq!(armed(&fx).len(), 1);
            assert!(matches!(h.state(), State::Transcribing(_)));
        };

        let mut h = recording(RecordMode::Toggle);
        let fx = h.step(Event::PillClicked);
        stop(&mut h, fx);

        let mut h = recording(RecordMode::PushToTalk);
        let max = h.timers[0].0;
        let fx = h.fire(max);
        assert_eq!(h.now, 1_000_000 + MAX_RECORDING_MS);
        stop(&mut h, fx);
        let State::Transcribing(tx) = h.state() else {
            panic!()
        };
        assert_eq!(tx.recorded_ms, MAX_RECORDING_MS);

        let mut h = recording(RecordMode::Toggle);
        h.advance(2_000);
        let fx = h.step(Event::MicDisconnected { session: S1 });
        stop(&mut h, fx);
    }

    #[test]
    fn recording_esc_pauses_with_a_countdown() {
        let mut h = recording(RecordMode::Toggle);
        h.advance(4_000);
        let fx = h.step(Event::Esc);
        let timer = timer_for(&fx, CANCEL_WINDOW_MS);
        assert_eq!(
            fx,
            vec![
                Effect::PauseCapture { session: S1 },
                arm(timer, CANCEL_WINDOW_MS)
            ]
        );
        assert_eq!(
            h.view(),
            PillView::CancelPending {
                remaining_ms: 3_000,
                from_recording: true
            }
        );
        h.advance(1_000);
        assert_eq!(
            h.view(),
            PillView::CancelPending {
                remaining_ms: 2_000,
                from_recording: true
            }
        );
        h.advance(5_000);
        assert_eq!(
            h.view(),
            PillView::CancelPending {
                remaining_ms: 0,
                from_recording: true
            }
        );
        assert!(h.m.wants_esc());
        assert_eq!(h.m.active_session(), Some(S1));
    }

    #[test]
    fn capture_failure_while_recording_drops_the_audio() {
        let mut h = recording(RecordMode::Toggle);
        let fx = h.step(Event::CaptureFailed {
            session: S1,
            error: CaptureError::PermissionDenied,
        });
        let notice = timer_for(&fx, NOTICE_ERROR_MS);
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::DropAudio { session: S1 },
                arm(notice, NOTICE_ERROR_MS),
            ]
        );
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::CaptureFailed {
                    error: CaptureError::PermissionDenied
                }
            }
        );
        assert!(!h.m.wants_esc());
        assert_eq!(h.m.active_session(), None);
    }

    #[test]
    fn capture_failure_during_the_countdown_drops_the_audio() {
        let mut h = cancel_rec(RecordMode::Toggle, 2_000);
        let fx = h.step(Event::CaptureFailed {
            session: S1,
            error: CaptureError::DeviceError,
        });
        assert_eq!(fx[0], Effect::SetEscCapture(false));
        assert_eq!(fx[1], Effect::DropAudio { session: S1 });
        timer_for(&fx, NOTICE_ERROR_MS);
        assert!(matches!(h.state(), State::Idle { notice: Some(_) }));
    }

    // --- CancelPending from Recording ---

    #[test]
    fn esc_twice_resumes_and_keeps_the_elapsed_time() {
        let mut h = cancel_rec(RecordMode::Toggle, 5_000);
        let old_max = h.timers[0].0;
        h.advance(2_000);
        let fx = h.step(Event::Esc);
        let new_max = timer_for(&fx, MAX_RECORDING_MS - 5_000);
        assert_eq!(
            fx,
            vec![
                Effect::ResumeCapture { session: S1 },
                arm(new_max, MAX_RECORDING_MS - 5_000),
            ]
        );
        assert_ne!(old_max, new_max);
        // The pause did not count.
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 5_000,
                mode: RecordMode::Toggle
            }
        );
        h.advance(1_000);
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 6_000,
                mode: RecordMode::Toggle
            }
        );
        // The old max timer is stale now.
        assert!(h.fire(old_max).is_empty());
        assert!(matches!(h.state(), State::Recording(_)));
        // The new one stops the recording at exactly the maximum.
        let fx = h.fire(new_max);
        assert_eq!(fx[0], Effect::StopCapture { session: S1 });
        let State::Transcribing(tx) = h.state() else {
            panic!()
        };
        assert_eq!(tx.recorded_ms, MAX_RECORDING_MS);
    }

    #[test]
    fn pill_click_during_the_countdown_resumes_too() {
        let mut h = cancel_rec(RecordMode::PushToTalk, 1_000);
        let fx = h.step(Event::PillClicked);
        assert_eq!(fx[0], Effect::ResumeCapture { session: S1 });
        assert!(matches!(h.state(), State::Recording(_)));
    }

    #[test]
    fn pause_resume_twice_accumulates_only_recorded_time() {
        let mut h = cancel_rec(RecordMode::Toggle, 3_000);
        h.advance(2_500);
        h.step(Event::Esc); // resume at 3 s recorded
        h.advance(4_000); // 7 s recorded
        h.step(Event::Esc);
        h.advance(1_000);
        let fx = h.step(Event::Esc);
        timer_for(&fx, MAX_RECORDING_MS - 7_000);
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 7_000,
                mode: RecordMode::Toggle
            }
        );
        h.step(TOGGLE);
        let State::Transcribing(tx) = h.state() else {
            panic!()
        };
        assert_eq!(tx.recorded_ms, 7_000);
    }

    #[test]
    fn hotkey_during_the_countdown_transcribes_what_was_recorded() {
        let mut h = cancel_rec(RecordMode::Toggle, 6_000);
        h.advance(1_500);
        let fx = h.step(TOGGLE);
        let timeout = timer_for(&fx, timeout_for(6_000));
        assert_eq!(
            fx,
            vec![
                Effect::StopCapture { session: S1 },
                Effect::Transcribe { session: S1 },
                arm(timeout, timeout_for(6_000)),
            ]
        );
        let State::Transcribing(tx) = h.state() else {
            panic!()
        };
        assert_eq!(tx.recorded_ms, 6_000, "the pause is not recorded time");
    }

    #[test]
    fn push_to_talk_release_during_the_countdown_is_ignored() {
        let mut h = cancel_rec(RecordMode::PushToTalk, 2_000);
        assert!(h.step(Event::HotkeyUp).is_empty());
        assert!(matches!(h.state(), State::CancelPending { .. }));
        // A new press decides.
        let fx = h.step(PTT);
        assert_eq!(fx[0], Effect::StopCapture { session: S1 });
        assert!(matches!(h.state(), State::Transcribing(_)));
    }

    #[test]
    fn countdown_expiry_while_recording_discards_everything() {
        let mut h = cancel_rec(RecordMode::Toggle, 2_000);
        let cancel = h.timers[1].0;
        let fx = h.fire(cancel);
        assert_eq!(
            fx,
            vec![
                Effect::StopCapture { session: S1 },
                Effect::SetEscCapture(false),
                Effect::DropAudio { session: S1 },
            ]
        );
        assert_eq!(h.state(), &State::Idle { notice: None });
        assert_eq!(h.view(), PillView::Hidden);
        assert!(!h.m.wants_esc());
    }

    #[test]
    fn mic_lost_during_the_countdown_transcribes_on_undo() {
        let mut h = cancel_rec(RecordMode::Toggle, 4_000);
        assert!(h.step(Event::MicDisconnected { session: S1 }).is_empty());
        let State::CancelPending {
            paused: Paused::Recording(rec),
            ..
        } = h.state()
        else {
            panic!()
        };
        assert!(rec.mic_lost);
        let fx = h.step(Event::Esc);
        assert_eq!(fx[0], Effect::StopCapture { session: S1 });
        assert_eq!(fx[1], Effect::Transcribe { session: S1 });
        timer_for(&fx, timeout_for(4_000));
        assert!(matches!(h.state(), State::Transcribing(_)));
    }

    #[test]
    fn mic_lost_during_the_countdown_still_expires_quietly() {
        let mut h = cancel_rec(RecordMode::Toggle, 4_000);
        h.step(Event::MicDisconnected { session: S1 });
        let fx = h.fire_next();
        assert!(fx.contains(&Effect::DropAudio { session: S1 }));
        assert_eq!(h.state(), &State::Idle { notice: None });
    }

    // --- Transcribing ---

    #[test]
    fn transcribed_text_is_delivered() {
        let mut h = transcribing(5_000);
        let fx = h.step(transcribed(S1, TranscribeOutcome::Text(text("hello"))));
        let delivery = timer_for(&fx, DELIVERY_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::Deliver {
                    session: S1,
                    text: text("hello"),
                },
                arm(delivery, DELIVERY_TIMEOUT_MS),
            ]
        );
        assert_eq!(
            h.state(),
            &State::Delivering {
                session: S1,
                timer: delivery,
                delivery: Delivery::Text,
            }
        );
        assert_eq!(h.view(), PillView::Delivering);
        assert!(
            !h.m.wants_esc(),
            "Esc goes back to other apps while pasting"
        );
        assert_eq!(h.m.active_session(), Some(S1));
    }

    #[test]
    fn transcribed_no_speech_drops_the_audio() {
        let mut h = transcribing(5_000);
        let fx = h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        let notice = timer_for(&fx, NOTICE_OK_MS * 2);
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::DropAudio { session: S1 },
                arm(notice, NOTICE_OK_MS * 2),
            ]
        );
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::NoSpeech
            }
        );
    }

    #[test]
    fn transcribed_failure_is_saved_for_retry() {
        for failure in [
            FailureKind::EmptyResult,
            FailureKind::ModelMissing,
            FailureKind::ModelLoadFailed,
            FailureKind::Engine,
            FailureKind::Audio,
        ] {
            let mut h = transcribing(5_000);
            let fx = h.step(transcribed(S1, TranscribeOutcome::Failed(failure)));
            let notice = timer_for(&fx, NOTICE_ERROR_MS);
            assert_eq!(
                fx,
                vec![
                    Effect::SetEscCapture(false),
                    Effect::SaveFailure {
                        session: S1,
                        failure
                    },
                    arm(notice, NOTICE_ERROR_MS),
                ]
            );
            assert_eq!(
                h.view(),
                PillView::Notice {
                    notice: Notice::Failed { failure }
                }
            );
        }
    }

    #[test]
    fn transcription_timeout_cancels_the_engine_and_saves_a_failure() {
        let mut h = transcribing(10_000);
        let timeout = h.timers[1].0;
        let fx = h.fire(timeout);
        assert_eq!(h.now, 1_000_000 + 10_000 + 20_000);
        let notice = timer_for(&fx, NOTICE_ERROR_MS);
        assert_eq!(
            fx,
            vec![
                Effect::CancelTranscription { session: S1 },
                Effect::SetEscCapture(false),
                Effect::SaveFailure {
                    session: S1,
                    failure: FailureKind::Timeout
                },
                arm(notice, NOTICE_ERROR_MS),
            ]
        );
        // A late result from the cancelled engine is stale.
        assert!(
            h.step(transcribed(S1, TranscribeOutcome::Text(text("late"))))
                .is_empty()
        );
        assert!(matches!(h.state(), State::Idle { notice: Some(_) }));
    }

    #[test]
    fn capture_failure_after_stop_preserves_cause_without_saving_history() {
        for paused in [false, true] {
            let mut h = transcribing(0);
            if paused {
                h.step(Event::Esc);
            }
            let fx = h.step(Event::CaptureFailed {
                session: S1,
                error: CaptureError::PermissionDenied,
            });
            assert_eq!(
                h.view(),
                PillView::Notice {
                    notice: Notice::CaptureFailed {
                        error: CaptureError::PermissionDenied
                    }
                }
            );
            assert!(fx.contains(&Effect::CancelTranscription { session: S1 }));
            assert!(fx.contains(&Effect::DropAudio { session: S1 }));
            assert!(
                !fx.iter()
                    .any(|effect| matches!(effect, Effect::SaveFailure { .. }))
            );
            assert!(
                h.step(transcribed(
                    S1,
                    TranscribeOutcome::Failed(FailureKind::Audio)
                ))
                .is_empty()
            );
        }
    }

    #[test]
    fn transcribing_ignores_hotkeys_mic_and_delivery() {
        let mut h = transcribing(5_000);
        for e in [
            TOGGLE,
            PTT,
            Event::HotkeyUp,
            Event::MicDisconnected { session: S1 },
            delivered(S1, DeliveryOutcome::Pasted),
        ] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
            assert!(matches!(h.state(), State::Transcribing(_)));
        }
    }

    #[test]
    fn transcribing_esc_starts_a_countdown_and_keeps_the_engine_running() {
        let mut h = transcribing(5_000);
        let fx = h.step(Event::Esc);
        let timer = timer_for(&fx, CANCEL_WINDOW_MS);
        assert_eq!(fx, vec![arm(timer, CANCEL_WINDOW_MS)]);
        assert_eq!(
            h.view(),
            PillView::CancelPending {
                remaining_ms: 3_000,
                from_recording: false
            }
        );
        assert!(h.m.wants_esc());
    }

    // --- CancelPending from Transcribing ---

    #[test]
    fn held_result_is_delivered_on_undo() {
        let mut h = cancel_tx();
        assert!(
            h.step(transcribed(S1, TranscribeOutcome::Text(text("kept"))))
                .is_empty()
        );
        assert!(matches!(
            h.state(),
            State::CancelPending {
                paused: Paused::Transcribing {
                    held: Some(TranscribeOutcome::Text(_)),
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            h.view(),
            PillView::CancelPending {
                remaining_ms: 3_000,
                from_recording: false
            }
        );
        let fx = h.step(Event::Esc);
        let delivery = timer_for(&fx, DELIVERY_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::Deliver {
                    session: S1,
                    text: text("kept"),
                },
                arm(delivery, DELIVERY_TIMEOUT_MS),
            ]
        );
    }

    #[test]
    fn held_failure_is_saved_on_undo() {
        let mut h = cancel_tx();
        h.step(transcribed(
            S1,
            TranscribeOutcome::Failed(FailureKind::Engine),
        ));
        let fx = h.step(Event::PillClicked);
        assert_eq!(fx[0], Effect::SetEscCapture(false));
        assert_eq!(
            fx[1],
            Effect::SaveFailure {
                session: S1,
                failure: FailureKind::Engine
            }
        );
        timer_for(&fx, NOTICE_ERROR_MS);
    }

    #[test]
    fn held_no_speech_is_dropped_on_undo() {
        let mut h = cancel_tx();
        h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        let fx = h.step(Event::Esc);
        assert_eq!(fx[0], Effect::SetEscCapture(false));
        assert_eq!(fx[1], Effect::DropAudio { session: S1 });
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::NoSpeech
            }
        );
    }

    #[test]
    fn timeout_during_the_countdown_cancels_the_engine_once_and_holds_the_failure() {
        let mut h = cancel_tx();
        let timeout = h.timers[1].0;
        h.advance(100);
        let fx = h.step(Event::TimerFired(timeout));
        assert_eq!(fx, vec![Effect::CancelTranscription { session: S1 }]);
        assert!(matches!(
            h.state(),
            State::CancelPending {
                paused: Paused::Transcribing {
                    held: Some(TranscribeOutcome::Failed(FailureKind::Timeout)),
                    ..
                },
                ..
            }
        ));
        // A result that arrives after the timeout is ignored; the failure stays held.
        assert!(
            h.step(transcribed(S1, TranscribeOutcome::Text(text("late"))))
                .is_empty()
        );
        let fx = h.step(Event::Esc);
        assert_eq!(
            fx[..2],
            [
                Effect::SetEscCapture(false),
                Effect::SaveFailure {
                    session: S1,
                    failure: FailureKind::Timeout
                },
            ]
        );
        assert!(!fx.contains(&Effect::CancelTranscription { session: S1 }));
    }

    #[test]
    fn countdown_expiry_while_transcribing_cancels_the_engine() {
        let mut h = cancel_tx();
        let cancel = h.timers[2].0;
        let fx = h.fire(cancel);
        assert_eq!(
            fx,
            vec![
                Effect::CancelTranscription { session: S1 },
                Effect::SetEscCapture(false),
                Effect::DropAudio { session: S1 },
            ]
        );
        assert_eq!(h.state(), &State::Idle { notice: None });
        // The engine may still answer; the answer is stale.
        assert!(
            h.step(transcribed(S1, TranscribeOutcome::Text(text("late"))))
                .is_empty()
        );
    }

    #[test]
    fn countdown_expiry_with_a_held_result_only_discards() {
        for held in [
            TranscribeOutcome::Text(text("kept")),
            TranscribeOutcome::Failed(FailureKind::Engine),
            TranscribeOutcome::NoSpeech,
        ] {
            let mut h = cancel_tx();
            h.step(transcribed(S1, held));
            let cancel = h.timers[2].0;
            let fx = h.fire(cancel);
            assert_eq!(
                fx,
                vec![
                    Effect::SetEscCapture(false),
                    Effect::DropAudio { session: S1 }
                ],
                "nothing is saved and the engine is already done"
            );
            assert_eq!(h.state(), &State::Idle { notice: None });
        }
    }

    #[test]
    fn undo_without_a_result_returns_to_transcribing() {
        let mut h = cancel_tx();
        let timeout = h.timers[1].0;
        h.advance(1_000);
        assert!(h.step(Event::Esc).is_empty());
        assert_eq!(
            h.state(),
            &State::Transcribing(Transcribing {
                session: S1,
                kind: SessionKind::Dictation,
                recorded_ms: 5_000,
                timeout_timer: timeout,
                plan: None,
            })
        );
        // The original timeout timer still works after the detour.
        let fx = h.fire(timeout);
        assert_eq!(fx[0], Effect::CancelTranscription { session: S1 });
        assert!(matches!(h.state(), State::Idle { notice: Some(_) }));
    }

    #[test]
    fn countdown_from_transcribing_ignores_hotkeys_and_mic_events() {
        let mut h = cancel_tx();
        for e in [
            TOGGLE,
            PTT,
            Event::HotkeyUp,
            Event::MicDisconnected { session: S1 },
            delivered(S1, DeliveryOutcome::Pasted),
        ] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
            assert!(matches!(
                h.state(),
                State::CancelPending {
                    paused: Paused::Transcribing { held: None, .. },
                    ..
                }
            ));
        }
    }

    // --- Delivering ---

    #[test]
    fn delivered_saves_after_the_paste() {
        let cases = [
            (DeliveryOutcome::Pasted, NOTICE_OK_MS),
            (
                DeliveryOutcome::Copied {
                    reason: CopyReason::SecureInput,
                },
                NOTICE_ERROR_MS,
            ),
            (
                DeliveryOutcome::Copied {
                    reason: CopyReason::AutoPasteOff,
                },
                NOTICE_ERROR_MS,
            ),
            (DeliveryOutcome::Unconfirmed, NOTICE_ERROR_MS),
        ];
        for (outcome, notice_ms) in cases {
            let mut h = delivering();
            let fx = h.step(delivered(S1, outcome));
            let notice = timer_for(&fx, notice_ms);
            assert_eq!(
                fx,
                vec![
                    Effect::SaveSuccess {
                        session: S1,
                        outcome
                    },
                    arm(notice, notice_ms),
                ],
                "{outcome:?}"
            );
            assert_eq!(
                h.view(),
                PillView::Notice {
                    notice: Notice::Delivered { outcome }
                }
            );
            assert_eq!(h.m.active_session(), None);
        }
    }

    #[test]
    fn delivery_timeout_saves_as_unconfirmed() {
        let mut h = delivering();
        let delivery = h.timers[2].0;
        let fx = h.fire(delivery);
        assert_eq!(
            fx[0],
            Effect::SaveSuccess {
                session: S1,
                outcome: DeliveryOutcome::Unconfirmed
            }
        );
        timer_for(&fx, NOTICE_ERROR_MS);
        // The late confirmation is stale.
        assert!(h.step(delivered(S1, DeliveryOutcome::Pasted)).is_empty());
    }

    #[test]
    fn delivering_ignores_user_input_and_engine_events() {
        let mut h = delivering();
        for e in [
            TOGGLE,
            Event::HotkeyUp,
            Event::Esc,
            Event::PillClicked,
            Event::MicDisconnected { session: S1 },
            transcribed(S1, TranscribeOutcome::Text(text("again"))),
        ] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
            assert!(matches!(h.state(), State::Delivering { .. }));
        }
    }

    // --- Idle with notice ---

    #[test]
    fn notice_hides_when_its_timer_fires() {
        let mut h = idle_with_notice();
        assert!(matches!(h.view(), PillView::Notice { .. }));
        let notice = h.timers.last().expect("notice timer").0;
        assert!(h.fire(notice).is_empty());
        assert_eq!(h.state(), &State::Idle { notice: None });
        assert_eq!(h.view(), PillView::Hidden);
    }

    #[test]
    fn notice_durations() {
        assert_eq!(
            Notice::Delivered {
                outcome: DeliveryOutcome::Pasted
            }
            .duration_ms(),
            800
        );
        assert_eq!(Notice::NoSpeech.duration_ms(), 1_600);
        assert_eq!(
            Notice::Failed {
                failure: FailureKind::Engine
            }
            .duration_ms(),
            2_800
        );
        assert_eq!(
            Notice::CaptureFailed {
                error: CaptureError::NoDevice
            }
            .duration_ms(),
            2_800
        );
    }

    #[test]
    fn hotkey_during_a_notice_starts_a_new_recording() {
        let mut h = idle_with_notice();
        let notice = h.timers.last().expect("notice timer").0;
        let fx = h.step(TOGGLE);
        assert_eq!(fx[0], Effect::StartCapture { session: S2 });
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 0,
                mode: RecordMode::Toggle
            }
        );
        // The old notice timer is stale now.
        assert!(h.fire(notice).is_empty());
        assert!(matches!(h.state(), State::Recording(_)));
    }

    #[test]
    fn notice_ignores_other_input() {
        let mut h = idle_with_notice();
        for e in [
            Event::HotkeyUp,
            Event::Esc,
            Event::PillClicked,
            Event::TimerFired(STALE_TIMER),
            delivered(S1, DeliveryOutcome::Pasted),
        ] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
            assert!(matches!(h.state(), State::Idle { notice: Some(_) }));
        }
    }

    // --- stale events in every state ---

    fn stale_events() -> Vec<Event> {
        vec![
            Event::TimerFired(STALE_TIMER),
            Event::TimerFired(TimerId(0)),
            Event::CaptureFailed {
                session: STALE_SESSION,
                error: CaptureError::DeviceError,
            },
            Event::MicDisconnected {
                session: STALE_SESSION,
            },
            transcribed(STALE_SESSION, TranscribeOutcome::Text(text("stale"))),
            transcribed(STALE_SESSION, TranscribeOutcome::NoSpeech),
            transcribed(
                STALE_SESSION,
                TranscribeOutcome::Failed(FailureKind::Engine),
            ),
            delivered(STALE_SESSION, DeliveryOutcome::Pasted),
            Event::ScreenFailed {
                session: STALE_SESSION,
            },
            Event::Planned {
                session: STALE_SESSION,
                counts: PlanCounts {
                    keyframes: 2,
                    marks: 1,
                },
            },
            Event::Composed {
                session: STALE_SESSION,
                outcome: ComposeOutcome::Bundle { images: 2 },
            },
            Event::Composed {
                session: STALE_SESSION,
                outcome: ComposeOutcome::Failed,
            },
        ]
    }

    type Builder = (&'static str, fn() -> Harness);

    #[test]
    fn stale_timers_and_sessions_are_ignored_in_every_state() {
        let builders: Vec<Builder> = vec![
            ("idle", Harness::new),
            ("recording", || recording(RecordMode::Toggle)),
            ("cancel from recording", || {
                cancel_rec(RecordMode::Toggle, 1_000)
            }),
            ("transcribing", || transcribing(1_000)),
            ("cancel from transcribing", cancel_tx),
            ("cancel with held result", || {
                let mut h = cancel_tx();
                h.step(transcribed(S1, TranscribeOutcome::Text(text("kept"))));
                h
            }),
            ("delivering", delivering),
            ("idle with notice", idle_with_notice),
            ("annotation recording", recording_annotation),
            ("annotation cancel from recording", || {
                cancel_rec_annotation(1_000)
            }),
            ("annotation transcribing", || transcribing_annotation(1_000)),
            ("annotation cancel from transcribing", cancel_tx_annotation),
            ("composing", composing),
            ("cancel from composing", cancel_compose),
            ("cancel with held bundle", || {
                let mut h = cancel_compose();
                h.step(composed(S1, ComposeOutcome::Bundle { images: 2 }));
                h
            }),
            ("delivering bundle", || delivering_bundle(2)),
        ];
        for (name, build) in builders {
            let mut h = build();
            let before = h.state().clone();
            let view = h.view();
            for e in stale_events() {
                assert!(h.step(e.clone()).is_empty(), "{name}: {e:?}");
                assert_eq!(h.state(), &before, "{name}: {e:?}");
                assert_eq!(h.view(), view, "{name}: {e:?}");
            }
        }
    }

    #[test]
    fn previous_session_events_are_stale_in_the_next_session() {
        let mut h = idle_with_notice();
        h.step(TOGGLE); // session 2
        for e in [
            transcribed(S1, TranscribeOutcome::Text(text("old"))),
            delivered(S1, DeliveryOutcome::Pasted),
            Event::MicDisconnected { session: S1 },
            Event::CaptureFailed {
                session: S1,
                error: CaptureError::DeviceError,
            },
        ] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
        }
        assert!(matches!(h.state(), State::Recording(_)));
    }

    // --- full scenarios ---

    #[test]
    fn happy_path_toggle() {
        let mut h = Harness::new();
        h.step(TOGGLE);
        h.advance(3_000);
        h.step(TOGGLE);
        h.advance(600);
        h.step(transcribed(
            S1,
            TranscribeOutcome::Text(text("今日は晴れです。")),
        ));
        h.advance(80);
        let fx = h.step(delivered(S1, DeliveryOutcome::Pasted));
        assert!(matches!(fx[0], Effect::SaveSuccess { session: S1, .. }));
        assert!(matches!(h.view(), PillView::Notice { .. }));
        h.fire_next();
        assert_eq!(h.view(), PillView::Hidden);
        assert!(
            h.timers
                .iter()
                .all(|(id, _)| h.fired.iter().all(|f| f != id))
        );
    }

    #[test]
    fn esc_then_nothing_cancels_without_a_trace() {
        let mut h = cancel_rec(RecordMode::Toggle, 2_000);
        let fx = h.fire_next();
        assert!(
            !fx.iter()
                .any(|f| matches!(f, Effect::SaveSuccess { .. } | Effect::SaveFailure { .. }))
        );
        assert!(fx.contains(&Effect::DropAudio { session: S1 }));
        assert_eq!(h.view(), PillView::Hidden);
    }

    // --- annotation sessions ---

    fn composed(session: SessionId, outcome: ComposeOutcome) -> Event {
        Event::Composed { session, outcome }
    }

    fn planned(session: SessionId, keyframes: u32, marks: u32) -> Event {
        Event::Planned {
            session,
            counts: PlanCounts { keyframes, marks },
        }
    }

    fn recording_annotation() -> Harness {
        let mut h = Harness::new();
        let fx = h.step(Event::AnnotationsHotkey);
        assert!(
            matches!(
                h.state(),
                State::Recording(Recording {
                    kind: SessionKind::Annotation,
                    ..
                })
            ),
            "{fx:?}"
        );
        h
    }

    fn cancel_rec_annotation(recorded_ms: Millis) -> Harness {
        let mut h = recording_annotation();
        h.advance(recorded_ms);
        h.step(Event::Esc);
        assert!(matches!(
            h.state(),
            State::CancelPending {
                paused: Paused::Recording(Recording {
                    kind: SessionKind::Annotation,
                    ..
                }),
                ..
            }
        ));
        h
    }

    fn transcribing_annotation(recorded_ms: Millis) -> Harness {
        let mut h = recording_annotation();
        h.advance(recorded_ms);
        h.step(Event::AnnotationsHotkey);
        assert!(matches!(
            h.state(),
            State::Transcribing(Transcribing {
                kind: SessionKind::Annotation,
                ..
            })
        ));
        h
    }

    fn cancel_tx_annotation() -> Harness {
        let mut h = transcribing_annotation(5_000);
        h.advance(300);
        h.step(Event::Esc);
        assert!(matches!(
            h.state(),
            State::CancelPending {
                paused: Paused::Transcribing { held: None, .. },
                ..
            }
        ));
        h
    }

    /// Transcribing with a plan of 2 keyframes and 1 mark, then text: Composing.
    fn composing() -> Harness {
        let mut h = transcribing_annotation(5_000);
        h.step(planned(S1, 2, 1));
        h.advance(400);
        h.step(transcribed(S1, TranscribeOutcome::Text(text("hello"))));
        assert!(matches!(h.state(), State::Composing(_)));
        h
    }

    fn cancel_compose() -> Harness {
        let mut h = composing();
        h.advance(200);
        h.step(Event::Esc);
        assert!(matches!(
            h.state(),
            State::CancelPending {
                paused: Paused::Composing { held: None, .. },
                ..
            }
        ));
        h
    }

    fn delivering_bundle(images: u32) -> Harness {
        let mut h = composing();
        h.advance(900);
        h.step(composed(S1, ComposeOutcome::Bundle { images }));
        assert!(matches!(
            h.state(),
            State::Delivering {
                delivery: Delivery::Bundle { .. },
                ..
            }
        ));
        h
    }

    fn stop_fx(session: SessionId, timeout: TimerId, recorded_ms: Millis) -> Vec<Effect> {
        vec![
            Effect::StopCapture { session },
            Effect::StopScreen { session },
            Effect::HideOverlay,
            Effect::PlanKeyframes { session },
            Effect::TranscribeSegments { session },
            arm(timeout, timeout_for(recorded_ms)),
        ]
    }

    #[test]
    fn annotation_constants() {
        assert_eq!(MAX_ANNOTATION_RECORDING_MS, 180_000);
        assert_eq!(COMPOSE_TIMEOUT_MS, 5_000);
        assert_eq!(max_recording_ms(SessionKind::Dictation), MAX_RECORDING_MS);
        assert_eq!(
            max_recording_ms(SessionKind::Annotation),
            MAX_ANNOTATION_RECORDING_MS
        );
    }

    #[test]
    fn annotations_hotkey_starts_screen_microphone_and_overlay() {
        let mut h = Harness::new();
        let fx = h.step(Event::AnnotationsHotkey);
        let max = timer_for(&fx, MAX_ANNOTATION_RECORDING_MS);
        assert_eq!(
            fx,
            vec![
                Effect::StartCapture { session: S1 },
                Effect::StartScreen { session: S1 },
                Effect::ShowOverlay,
                Effect::SetEscCapture(true),
                arm(max, MAX_ANNOTATION_RECORDING_MS),
            ]
        );
        assert_eq!(h.m.active_session(), Some(S1));
        assert_eq!(h.m.active_kind(), Some(SessionKind::Annotation));
        assert!(h.m.wants_esc());
        h.advance(1_500);
        assert_eq!(
            h.view(),
            PillView::Recording {
                elapsed_ms: 1_500,
                mode: RecordMode::Toggle
            }
        );
        // A notice is replaced by the recording, as for dictation.
        let mut h = idle_with_notice();
        let fx = h.step(Event::AnnotationsHotkey);
        assert!(fx.contains(&Effect::StartScreen { session: S2 }));
    }

    #[test]
    fn annotations_refused_shows_a_notice_and_records_nothing() {
        let mut h = Harness::new();
        let reason = AnnotationsRefusal::ScreenRecordingDenied;
        let fx = h.step(Event::AnnotationsRefused { reason });
        let t = timer_for(&fx, NOTICE_ERROR_MS);
        assert_eq!(fx, vec![arm(t, NOTICE_ERROR_MS)]);
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::AnnotationsRefused { reason }
            }
        );
        assert_eq!(h.m.active_session(), None);
        assert!(!h.m.wants_esc());
        assert!(h.fire(t).is_empty());
        assert_eq!(h.view(), PillView::Hidden);
        // While another notice shows, the refusal replaces it; the old timer is stale.
        let mut h = idle_with_notice();
        let State::Idle {
            notice: Some((_, old)),
        } = h.state().clone()
        else {
            panic!()
        };
        let fx = h.step(Event::AnnotationsRefused {
            reason: AnnotationsRefusal::MicrophoneDenied,
        });
        let t = timer_for(&fx, NOTICE_ERROR_MS);
        assert!(h.step(Event::TimerFired(old)).is_empty());
        assert!(matches!(h.state(), State::Idle { notice: Some(_) }));
        h.fire(t);
        assert_eq!(h.view(), PillView::Hidden);
        // The refusal is ignored outside Idle.
        let mut h = recording_annotation();
        assert!(h.step(Event::AnnotationsRefused { reason }).is_empty());
    }

    #[test]
    fn annotation_recording_stops_on_hotkey_pill_max_timer_and_mic_loss() {
        let stops: Vec<(&str, Event)> = vec![
            ("hotkey", Event::AnnotationsHotkey),
            ("pill", Event::PillClicked),
            ("mic", Event::MicDisconnected { session: S1 }),
        ];
        for (name, event) in stops {
            let mut h = recording_annotation();
            h.advance(10_000);
            let fx = h.step(event);
            let timeout = timer_for(&fx, timeout_for(10_000));
            assert_eq!(fx, stop_fx(S1, timeout, 10_000), "{name}");
            assert_eq!(
                h.state(),
                &State::Transcribing(Transcribing {
                    session: S1,
                    kind: SessionKind::Annotation,
                    recorded_ms: 10_000,
                    timeout_timer: timeout,
                    plan: None,
                }),
                "{name}"
            );
            assert_eq!(h.view(), PillView::Transcribing);
            assert!(h.m.wants_esc());
        }
        // The 3 minute limit stops it too.
        let mut h = recording_annotation();
        let fx = h.fire_next();
        let timeout = timer_for(&fx, timeout_for(MAX_ANNOTATION_RECORDING_MS));
        assert_eq!(fx, stop_fx(S1, timeout, MAX_ANNOTATION_RECORDING_MS));
    }

    #[test]
    fn annotation_recording_ignores_the_dictation_hotkeys_and_vice_versa() {
        let mut h = recording_annotation();
        for e in [TOGGLE, PTT, Event::HotkeyUp] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
            assert!(matches!(h.state(), State::Recording(_)));
        }
        for mode in [RecordMode::Toggle, RecordMode::PushToTalk] {
            let mut h = recording(mode);
            assert!(h.step(Event::AnnotationsHotkey).is_empty());
            assert!(matches!(h.state(), State::Recording(_)));
            assert!(h.step(Event::ScreenFailed { session: S1 }).is_empty());
        }
        // Busy states ignore the annotations hotkey as they ignore the dictation hotkey.
        let builders: Vec<Builder> = vec![
            ("transcribing", || transcribing(1_000)),
            ("annotation transcribing", || transcribing_annotation(1_000)),
            ("composing", composing),
            ("delivering", delivering),
            ("delivering bundle", || delivering_bundle(1)),
            ("cancel from transcribing", cancel_tx),
            ("cancel from composing", cancel_compose),
            ("cancel from dictation recording", || {
                cancel_rec(RecordMode::Toggle, 1_000)
            }),
        ];
        for (name, build) in builders {
            let mut h = build();
            let before = h.state().clone();
            assert!(h.step(Event::AnnotationsHotkey).is_empty(), "{name}");
            assert_eq!(h.state(), &before, "{name}");
        }
    }

    #[test]
    fn screen_failure_marks_the_recording_and_the_stop_is_unchanged() {
        let mut h = recording_annotation();
        assert!(h.step(Event::ScreenFailed { session: S1 }).is_empty());
        let State::Recording(rec) = h.state().clone() else {
            panic!()
        };
        assert!(rec.screen_failed);
        assert!(h.m.screen_lost());
        assert_eq!(h.m.active_kind(), Some(SessionKind::Annotation));
        h.advance(2_000);
        let fx = h.step(Event::AnnotationsHotkey);
        let timeout = timer_for(&fx, timeout_for(2_000));
        assert_eq!(fx, stop_fx(S1, timeout, 2_000));
        // During the countdown the flag is kept too.
        let mut h = cancel_rec_annotation(1_000);
        assert!(h.step(Event::ScreenFailed { session: S1 }).is_empty());
        let State::CancelPending {
            paused: Paused::Recording(rec),
            ..
        } = h.state().clone()
        else {
            panic!()
        };
        assert!(rec.screen_failed);
        assert!(h.m.screen_lost());
        // Any other state reports no loss.
        assert!(!recording_annotation().m.screen_lost());
        assert!(!transcribing_annotation(1_000).m.screen_lost());
        assert!(!Harness::new().m.screen_lost());
    }

    #[test]
    fn annotation_esc_pauses_screen_and_microphone() {
        let mut h = recording_annotation();
        h.advance(4_000);
        let fx = h.step(Event::Esc);
        let t = timer_for(&fx, CANCEL_WINDOW_MS);
        assert_eq!(
            fx,
            vec![
                Effect::PauseCapture { session: S1 },
                Effect::PauseScreen { session: S1 },
                arm(t, CANCEL_WINDOW_MS),
            ]
        );
        assert_eq!(
            h.view(),
            PillView::CancelPending {
                remaining_ms: CANCEL_WINDOW_MS,
                from_recording: true
            }
        );
        assert!(h.m.wants_esc());
        assert_eq!(h.m.active_kind(), Some(SessionKind::Annotation));
    }

    #[test]
    fn annotation_countdown_undo_resumes_screen_and_microphone() {
        for undo in [Event::Esc, Event::PillClicked] {
            let mut h = cancel_rec_annotation(4_000);
            h.advance(1_000);
            let fx = h.step(undo.clone());
            let left = MAX_ANNOTATION_RECORDING_MS - 4_000;
            let max = timer_for(&fx, left);
            assert_eq!(
                fx,
                vec![
                    Effect::ResumeCapture { session: S1 },
                    Effect::ResumeScreen { session: S1 },
                    arm(max, left),
                ],
                "{undo:?}"
            );
            // The paused second is not counted.
            assert_eq!(
                h.view(),
                PillView::Recording {
                    elapsed_ms: 4_000,
                    mode: RecordMode::Toggle
                }
            );
            // The new limit fires at 3 minutes of recorded time.
            let fx = h.fire(max);
            assert!(fx.contains(&Effect::StopScreen { session: S1 }));
            let State::Transcribing(tx) = h.state() else {
                panic!()
            };
            assert_eq!(tx.recorded_ms, MAX_ANNOTATION_RECORDING_MS);
        }
    }

    #[test]
    fn annotation_countdown_hotkey_stops_while_the_dictation_hotkey_is_ignored() {
        let mut h = cancel_rec_annotation(4_000);
        assert!(h.step(TOGGLE).is_empty());
        assert!(h.step(Event::HotkeyUp).is_empty());
        assert!(matches!(h.state(), State::CancelPending { .. }));
        h.advance(1_000);
        let fx = h.step(Event::AnnotationsHotkey);
        let timeout = timer_for(&fx, timeout_for(4_000));
        assert_eq!(fx, stop_fx(S1, timeout, 4_000));
        // The dictation countdown ignores the annotations hotkey (tested above) and the
        // dictation hotkey still stops it.
        let mut h = cancel_rec(RecordMode::Toggle, 4_000);
        let fx = h.step(TOGGLE);
        assert!(fx.contains(&Effect::Transcribe { session: S1 }));
    }

    #[test]
    fn annotation_countdown_expiry_drops_everything() {
        let mut h = cancel_rec_annotation(4_000);
        let fx = h.fire_next();
        assert_eq!(
            fx,
            vec![
                Effect::StopCapture { session: S1 },
                Effect::StopScreen { session: S1 },
                Effect::HideOverlay,
                Effect::SetEscCapture(false),
                Effect::DropAll { session: S1 },
            ]
        );
        assert_eq!(h.state(), &State::Idle { notice: None });
        assert_eq!(h.view(), PillView::Hidden);
        assert!(!h.m.wants_esc());
    }

    #[test]
    fn annotation_mic_lost_during_the_countdown_transcribes_on_undo() {
        let mut h = cancel_rec_annotation(4_000);
        assert!(h.step(Event::MicDisconnected { session: S1 }).is_empty());
        h.advance(500);
        let fx = h.step(Event::Esc);
        let timeout = timer_for(&fx, timeout_for(4_000));
        assert_eq!(fx, stop_fx(S1, timeout, 4_000));
    }

    #[test]
    fn annotation_capture_failure_releases_the_screen_everywhere() {
        let error = CaptureError::DeviceError;
        let failed = Event::CaptureFailed { session: S1, error };
        let notice = Notice::CaptureFailed { error };

        let mut h = recording_annotation();
        let fx = h.step(failed.clone());
        let t = timer_for(&fx, notice.duration_ms());
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::StopScreen { session: S1 },
                Effect::HideOverlay,
                Effect::DropAll { session: S1 },
                arm(t, notice.duration_ms()),
            ]
        );
        assert_eq!(h.view(), PillView::Notice { notice });

        let mut h = cancel_rec_annotation(1_000);
        let fx = h.step(failed.clone());
        let t = timer_for(&fx, notice.duration_ms());
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::StopScreen { session: S1 },
                Effect::HideOverlay,
                Effect::DropAll { session: S1 },
                arm(t, notice.duration_ms()),
            ]
        );

        // After the stop the screen is already released: only the engine is cancelled.
        for build in [transcribing_annotation as fn(Millis) -> Harness, |ms| {
            let mut h = transcribing_annotation(ms);
            h.step(Event::Esc);
            h
        }] {
            let mut h = build(1_000);
            let fx = h.step(failed.clone());
            let t = timer_for(&fx, notice.duration_ms());
            assert_eq!(
                fx,
                vec![
                    Effect::CancelTranscription { session: S1 },
                    Effect::SetEscCapture(false),
                    Effect::DropAll { session: S1 },
                    arm(t, notice.duration_ms()),
                ]
            );
        }
        // Composing and delivering ignore it (the microphone is long closed).
        for build in [composing as fn() -> Harness, || delivering_bundle(1)] {
            let mut h = build();
            let before = h.state().clone();
            assert!(h.step(failed.clone()).is_empty());
            assert_eq!(h.state(), &before);
        }
    }

    #[test]
    fn planned_is_remembered_for_annotations_only() {
        let mut h = transcribing_annotation(1_000);
        assert!(h.step(planned(S1, 3, 2)).is_empty());
        let State::Transcribing(tx) = h.state() else {
            panic!()
        };
        assert_eq!(
            tx.plan,
            Some(PlanCounts {
                keyframes: 3,
                marks: 2
            })
        );
        // Also during the countdown, with or without a held result.
        let mut h = cancel_tx_annotation();
        assert!(h.step(planned(S1, 1, 0)).is_empty());
        h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        assert!(h.step(planned(S1, 1, 0)).is_empty());
        let State::CancelPending {
            paused: Paused::Transcribing { tx, held },
            ..
        } = h.state().clone()
        else {
            panic!()
        };
        assert_eq!(tx.plan.map(|p| p.marks), Some(0));
        assert_eq!(held, Some(TranscribeOutcome::NoSpeech));
        // A dictation transcription ignores it.
        let mut h = transcribing(1_000);
        let before = h.state().clone();
        assert!(h.step(planned(S1, 3, 2)).is_empty());
        assert_eq!(h.state(), &before);
    }

    #[test]
    fn annotation_text_goes_to_composing() {
        let mut h = transcribing_annotation(5_000);
        h.step(planned(S1, 2, 1));
        h.advance(400);
        let fx = h.step(transcribed(S1, TranscribeOutcome::Text(text("hello"))));
        let t = timer_for(&fx, COMPOSE_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::Compose {
                    session: S1,
                    words: ComposeWords::Transcribed
                },
                arm(t, COMPOSE_TIMEOUT_MS),
            ]
        );
        assert_eq!(
            h.state(),
            &State::Composing(Composing {
                session: S1,
                timeout_timer: t
            })
        );
        assert_eq!(h.view(), PillView::Transcribing);
        assert!(h.m.wants_esc(), "Esc still cancels while composing");
        assert_eq!(h.m.active_session(), Some(S1));
        assert_eq!(h.m.active_kind(), Some(SessionKind::Annotation));
        // The transcription timer is stale now.
        let fx = h.step(Event::TimerFired(TimerId(2)));
        assert!(fx.is_empty());
    }

    #[test]
    fn annotation_no_speech_composes_with_marks_and_drops_without() {
        // Marks: pictures and marks go out without words.
        let mut h = transcribing_annotation(5_000);
        h.step(planned(S1, 2, 1));
        let fx = h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        let t = timer_for(&fx, COMPOSE_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::Compose {
                    session: S1,
                    words: ComposeWords::None
                },
                arm(t, COMPOSE_TIMEOUT_MS),
            ]
        );
        // No marks: nothing to show.
        let mut h = transcribing_annotation(5_000);
        h.step(planned(S1, 2, 0));
        let fx = h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        let t = timer_for(&fx, Notice::AnnotationEmpty.duration_ms());
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::DropAll { session: S1 },
                arm(t, NOTICE_OK_MS * 2),
            ]
        );
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::AnnotationEmpty
            }
        );
        assert!(!h.m.wants_esc());
        // Plan not reported yet: the marks are unknown, so they are kept.
        let mut h = transcribing_annotation(5_000);
        let fx = h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        assert!(fx.contains(&Effect::Compose {
            session: S1,
            words: ComposeWords::None
        }));
    }

    #[test]
    fn annotation_failure_and_timeout_compose_with_a_note_and_keep_the_audio() {
        let mut h = transcribing_annotation(5_000);
        h.step(planned(S1, 2, 0));
        let fx = h.step(transcribed(
            S1,
            TranscribeOutcome::Failed(FailureKind::Engine),
        ));
        let t = timer_for(&fx, COMPOSE_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::Compose {
                    session: S1,
                    words: ComposeWords::Failed
                },
                arm(t, COMPOSE_TIMEOUT_MS),
            ]
        );
        assert!(!fx.iter().any(|f| matches!(f, Effect::SaveFailure { .. })));

        let mut h = transcribing_annotation(5_000);
        let State::Transcribing(tx) = h.state().clone() else {
            panic!()
        };
        let fx = h.fire(tx.timeout_timer);
        let t = timer_for(&fx, COMPOSE_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::CancelTranscription { session: S1 },
                Effect::Compose {
                    session: S1,
                    words: ComposeWords::Failed
                },
                arm(t, COMPOSE_TIMEOUT_MS),
            ]
        );
        // The model load budget applies to annotations too.
        let mut h = transcribing_annotation(5_000);
        let fx = h.step(Event::ModelLoading { session: S1 });
        timer_for(&fx, MODEL_LOAD_TIMEOUT_MS + timeout_for(5_000));
    }

    #[test]
    fn composed_bundle_is_delivered_then_saved_after_the_paste() {
        let mut h = composing();
        h.advance(900);
        let fx = h.step(composed(S1, ComposeOutcome::Bundle { images: 3 }));
        let delivery = timer_for(&fx, DELIVERY_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::DeliverBundle { session: S1 },
                arm(delivery, DELIVERY_TIMEOUT_MS),
            ]
        );
        assert_eq!(
            h.state(),
            &State::Delivering {
                session: S1,
                timer: delivery,
                delivery: Delivery::Bundle { images: 3 },
            }
        );
        assert_eq!(h.view(), PillView::Delivering);
        assert!(!h.m.wants_esc());
        assert_eq!(h.m.active_kind(), Some(SessionKind::Annotation));
        // The compose timer is stale now.
        assert!(h.step(Event::TimerFired(TimerId(3))).is_empty());

        h.advance(100);
        let outcome = DeliveryOutcome::Pasted;
        let fx = h.step(delivered(S1, outcome));
        let notice = Notice::AnnotationDelivered { outcome, images: 3 };
        let t = timer_for(&fx, NOTICE_OK_MS);
        assert_eq!(
            fx,
            vec![
                Effect::SaveAnnotation {
                    session: S1,
                    outcome
                },
                arm(t, NOTICE_OK_MS),
            ]
        );
        assert_eq!(h.view(), PillView::Notice { notice });
        assert_eq!(h.m.active_session(), None);
        h.fire(t);
        assert_eq!(h.view(), PillView::Hidden);
    }

    #[test]
    fn compose_failure_or_timeout_delivers_the_words_alone() {
        let mut h = composing();
        let fx = h.step(composed(S1, ComposeOutcome::Failed));
        let delivery = timer_for(&fx, DELIVERY_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::DeliverText { session: S1 },
                arm(delivery, DELIVERY_TIMEOUT_MS),
            ]
        );
        let outcome = DeliveryOutcome::Copied {
            reason: CopyReason::PasteFailed,
        };
        let fx = h.step(delivered(S1, outcome));
        assert!(fx.contains(&Effect::SaveAnnotation {
            session: S1,
            outcome
        }));
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::AnnotationDelivered { outcome, images: 0 }
            }
        );

        let mut h = composing();
        let fx = h.fire_next();
        assert_eq!(fx[0], Effect::SetEscCapture(false));
        assert_eq!(fx[1], Effect::DeliverText { session: S1 });
        assert!(matches!(
            h.state(),
            State::Delivering {
                delivery: Delivery::Bundle { images: 0 },
                ..
            }
        ));
    }

    #[test]
    fn bundle_delivery_timeout_saves_as_unconfirmed() {
        let mut h = delivering_bundle(2);
        let fx = h.fire_next();
        let outcome = DeliveryOutcome::Unconfirmed;
        assert_eq!(
            fx[0],
            Effect::SaveAnnotation {
                session: S1,
                outcome
            }
        );
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::AnnotationDelivered { outcome, images: 2 }
            }
        );
        // A late delivery report is stale.
        assert!(h.step(delivered(S1, DeliveryOutcome::Pasted)).is_empty());
    }

    #[test]
    fn composing_ignores_input_and_old_results() {
        let mut h = composing();
        let before = h.state().clone();
        for e in [
            TOGGLE,
            Event::HotkeyUp,
            Event::PillClicked,
            Event::AnnotationsHotkey,
            Event::MicDisconnected { session: S1 },
            Event::ModelLoading { session: S1 },
            planned(S1, 1, 1),
            transcribed(S1, TranscribeOutcome::Text(text("late"))),
            delivered(S1, DeliveryOutcome::Pasted),
            Event::ScreenFailed { session: S1 },
        ] {
            assert!(h.step(e.clone()).is_empty(), "{e:?}");
            assert_eq!(h.state(), &before, "{e:?}");
        }
    }

    #[test]
    fn composing_esc_holds_the_outcome_until_undo() {
        let mut h = composing();
        h.advance(200);
        let fx = h.step(Event::Esc);
        let countdown = timer_for(&fx, CANCEL_WINDOW_MS);
        assert_eq!(fx, vec![arm(countdown, CANCEL_WINDOW_MS)]);
        assert_eq!(
            h.view(),
            PillView::CancelPending {
                remaining_ms: CANCEL_WINDOW_MS,
                from_recording: false
            }
        );
        assert!(h.m.wants_esc());
        // The compose finishes meanwhile: held, nothing happens.
        assert!(
            h.step(composed(S1, ComposeOutcome::Bundle { images: 2 }))
                .is_empty()
        );
        // Undo: the held bundle is delivered.
        h.advance(500);
        let fx = h.step(Event::Esc);
        let delivery = timer_for(&fx, DELIVERY_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::DeliverBundle { session: S1 },
                arm(delivery, DELIVERY_TIMEOUT_MS),
            ]
        );
        // The countdown timer is stale.
        assert!(h.step(Event::TimerFired(countdown)).is_empty());
        assert!(matches!(h.state(), State::Delivering { .. }));
    }

    #[test]
    fn compose_timeout_during_the_countdown_is_held_as_a_failure() {
        let mut h = cancel_compose();
        let State::CancelPending {
            paused: Paused::Composing { composing, .. },
            ..
        } = h.state().clone()
        else {
            panic!()
        };
        assert!(h.fire(composing.timeout_timer).is_empty());
        let fx = h.step(Event::PillClicked);
        assert_eq!(fx[1], Effect::DeliverText { session: S1 });
        assert!(matches!(
            h.state(),
            State::Delivering {
                delivery: Delivery::Bundle { images: 0 },
                ..
            }
        ));
    }

    #[test]
    fn undo_without_a_compose_result_returns_to_composing() {
        let mut h = cancel_compose();
        let State::CancelPending {
            paused: Paused::Composing { composing, .. },
            ..
        } = h.state().clone()
        else {
            panic!()
        };
        h.advance(1_000);
        assert!(h.step(Event::Esc).is_empty());
        assert_eq!(h.state(), &State::Composing(composing.clone()));
        // The original compose timer still works after the detour.
        let fx = h.fire(composing.timeout_timer);
        assert_eq!(fx[1], Effect::DeliverText { session: S1 });
    }

    #[test]
    fn compose_countdown_expiry_drops_everything() {
        for held in [false, true] {
            let mut h = cancel_compose();
            if held {
                h.step(composed(S1, ComposeOutcome::Bundle { images: 1 }));
            }
            let fx = h.fire_next();
            assert_eq!(
                fx,
                vec![
                    Effect::SetEscCapture(false),
                    Effect::DropAll { session: S1 }
                ],
                "held {held}"
            );
            assert_eq!(h.state(), &State::Idle { notice: None });
            assert_eq!(h.m.active_session(), None);
        }
    }

    #[test]
    fn annotation_transcribing_countdown_expiry_drops_everything() {
        let mut h = cancel_tx_annotation();
        let fx = h.fire_next();
        assert_eq!(
            fx,
            vec![
                Effect::CancelTranscription { session: S1 },
                Effect::SetEscCapture(false),
                Effect::DropAll { session: S1 },
            ]
        );
        assert_eq!(h.state(), &State::Idle { notice: None });
        // With a held result the engine needs no cancel.
        let mut h = cancel_tx_annotation();
        h.step(transcribed(S1, TranscribeOutcome::Text(text("held"))));
        let fx = h.fire_next();
        assert_eq!(
            fx,
            vec![
                Effect::SetEscCapture(false),
                Effect::DropAll { session: S1 }
            ]
        );
    }

    #[test]
    fn held_annotation_result_composes_on_undo() {
        let mut h = cancel_tx_annotation();
        h.step(planned(S1, 2, 0));
        assert!(
            h.step(transcribed(S1, TranscribeOutcome::Text(text("held"))))
                .is_empty()
        );
        let fx = h.step(Event::Esc);
        let t = timer_for(&fx, COMPOSE_TIMEOUT_MS);
        assert_eq!(
            fx,
            vec![
                Effect::Compose {
                    session: S1,
                    words: ComposeWords::Transcribed
                },
                arm(t, COMPOSE_TIMEOUT_MS),
            ]
        );
        // A held no-speech result without marks ends quietly with a notice.
        let mut h = cancel_tx_annotation();
        h.step(planned(S1, 2, 0));
        h.step(transcribed(S1, TranscribeOutcome::NoSpeech));
        let fx = h.step(Event::PillClicked);
        assert!(fx.contains(&Effect::DropAll { session: S1 }));
        assert_eq!(
            h.view(),
            PillView::Notice {
                notice: Notice::AnnotationEmpty
            }
        );
    }

    #[test]
    fn disabling_the_feature_ends_an_annotation_session_at_once() {
        let drop_all = [
            Effect::SetEscCapture(false),
            Effect::DropAll { session: S1 },
        ];
        type Case = (&'static str, fn() -> Harness, Vec<Effect>);
        let cases: Vec<Case> = vec![
            (
                "recording",
                recording_annotation,
                vec![
                    Effect::StopCapture { session: S1 },
                    Effect::StopScreen { session: S1 },
                    Effect::HideOverlay,
                    Effect::SetEscCapture(false),
                    Effect::DropAll { session: S1 },
                ],
            ),
            (
                "cancel from recording",
                || cancel_rec_annotation(1_000),
                vec![
                    Effect::StopCapture { session: S1 },
                    Effect::StopScreen { session: S1 },
                    Effect::HideOverlay,
                    Effect::SetEscCapture(false),
                    Effect::DropAll { session: S1 },
                ],
            ),
            (
                "transcribing",
                || transcribing_annotation(1_000),
                vec![
                    Effect::CancelTranscription { session: S1 },
                    Effect::SetEscCapture(false),
                    Effect::DropAll { session: S1 },
                ],
            ),
            (
                "cancel from transcribing",
                cancel_tx_annotation,
                vec![
                    Effect::CancelTranscription { session: S1 },
                    Effect::SetEscCapture(false),
                    Effect::DropAll { session: S1 },
                ],
            ),
            (
                "cancel with held result",
                || {
                    let mut h = cancel_tx_annotation();
                    h.step(transcribed(S1, TranscribeOutcome::Text(text("held"))));
                    h
                },
                drop_all.to_vec(),
            ),
            ("composing", composing, drop_all.to_vec()),
            ("cancel from composing", cancel_compose, drop_all.to_vec()),
            (
                "cancel with held bundle",
                || {
                    let mut h = cancel_compose();
                    h.step(composed(S1, ComposeOutcome::Bundle { images: 1 }));
                    h
                },
                drop_all.to_vec(),
            ),
        ];
        for (name, build, expected) in cases {
            let mut h = build();
            let fx = h.step(Event::AnnotationsDisabled);
            assert_eq!(fx, expected, "{name}");
            assert_eq!(h.state(), &State::Idle { notice: None }, "{name}");
            assert!(!h.m.wants_esc(), "{name}");
        }
        // A paste in flight finishes; dictation sessions and idle ignore it.
        let builders: Vec<Builder> = vec![
            ("delivering bundle", || delivering_bundle(1)),
            ("idle", Harness::new),
            ("idle with notice", idle_with_notice),
            ("recording", || recording(RecordMode::Toggle)),
            ("cancel from recording", || {
                cancel_rec(RecordMode::Toggle, 1_000)
            }),
            ("transcribing", || transcribing(1_000)),
            ("cancel from transcribing", cancel_tx),
            ("delivering", delivering),
        ];
        for (name, build) in builders {
            let mut h = build();
            let before = h.state().clone();
            assert!(h.step(Event::AnnotationsDisabled).is_empty(), "{name}");
            assert_eq!(h.state(), &before, "{name}");
        }
    }

    #[test]
    fn annotation_notice_durations() {
        for outcome in [
            DeliveryOutcome::Pasted,
            DeliveryOutcome::Copied {
                reason: CopyReason::SecureInput,
            },
            DeliveryOutcome::Unconfirmed,
        ] {
            assert_eq!(
                Notice::AnnotationDelivered { outcome, images: 2 }.duration_ms(),
                Notice::Delivered { outcome }.duration_ms()
            );
        }
        assert_eq!(
            Notice::AnnotationEmpty.duration_ms(),
            Notice::NoSpeech.duration_ms()
        );
        assert_eq!(
            Notice::AnnotationsRefused {
                reason: AnnotationsRefusal::MicrophoneDenied
            }
            .duration_ms(),
            NOTICE_ERROR_MS
        );
    }

    #[test]
    fn happy_path_annotation() {
        let mut h = Harness::new();
        let mut all = h.step(Event::AnnotationsHotkey);
        h.advance(30_000);
        all.extend(h.step(Event::AnnotationsHotkey));
        all.extend(h.step(planned(S1, 4, 3)));
        h.advance(900);
        all.extend(h.step(transcribed(S1, TranscribeOutcome::Text(text("words")))));
        h.advance(600);
        all.extend(h.step(composed(S1, ComposeOutcome::Bundle { images: 4 })));
        h.advance(200);
        all.extend(h.step(delivered(S1, DeliveryOutcome::Pasted)));
        let names: Vec<String> = all
            .iter()
            .filter(|f| !matches!(f, Effect::ArmTimer { .. }))
            .map(|f| format!("{f:?}"))
            .map(|s| s.split([' ', '(']).next().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "StartCapture",
                "StartScreen",
                "ShowOverlay",
                "SetEscCapture",
                "StopCapture",
                "StopScreen",
                "HideOverlay",
                "PlanKeyframes",
                "TranscribeSegments",
                "Compose",
                "SetEscCapture",
                "DeliverBundle",
                "SaveAnnotation",
            ]
        );
        assert_eq!(h.m.active_session(), None);
        // The next session is a fresh id, whatever its kind.
        let fx = h.step(TOGGLE);
        assert!(fx.contains(&Effect::StartCapture { session: S2 }));
    }

    // --- invariants over random event sequences ---

    /// xorshift64*: small, deterministic, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn pick<T: Clone>(&mut self, items: &[T]) -> T {
            items[self.below(items.len() as u64) as usize].clone()
        }
    }

    /// What the controller would have done so far, checked against the machine.
    #[derive(Default)]
    struct Ledger {
        esc_registered: bool,
        overlay_shown: bool,
        started: BTreeSet<u64>,
        /// Annotation sessions: the screen stream was started / stopped.
        screen_started: BTreeSet<u64>,
        screen_stopped: BTreeSet<u64>,
        planned: BTreeSet<u64>,
        transcribe_count: BTreeMap<u64, u32>,
        composed: BTreeSet<u64>,
        delivered: BTreeSet<u64>,
        terminals: BTreeMap<u64, Vec<String>>,
    }

    impl Ledger {
        fn record(&mut self, fx: &[Effect]) {
            for f in fx {
                match f {
                    Effect::StartCapture { session } => {
                        assert!(self.started.insert(session.0), "{session:?} started twice");
                    }
                    Effect::StartScreen { session } => {
                        assert!(
                            self.started.contains(&session.0),
                            "{f:?} before StartCapture"
                        );
                        assert!(
                            self.screen_started.insert(session.0),
                            "{session:?} screen started twice"
                        );
                    }
                    Effect::StopScreen { session } => {
                        assert!(
                            self.screen_started.contains(&session.0),
                            "{f:?} never started"
                        );
                        assert!(
                            self.screen_stopped.insert(session.0),
                            "{session:?} screen stopped twice"
                        );
                    }
                    Effect::PauseScreen { session } | Effect::ResumeScreen { session } => {
                        assert!(
                            self.screen_started.contains(&session.0),
                            "{f:?} never started"
                        );
                        assert!(
                            !self.screen_stopped.contains(&session.0),
                            "{f:?} after StopScreen"
                        );
                    }
                    Effect::ShowOverlay => {
                        assert!(!self.overlay_shown, "overlay shown twice");
                        self.overlay_shown = true;
                    }
                    Effect::HideOverlay => {
                        assert!(self.overlay_shown, "overlay hidden while hidden");
                        self.overlay_shown = false;
                    }
                    Effect::PlanKeyframes { session } => {
                        assert!(
                            self.screen_stopped.contains(&session.0),
                            "{f:?} before StopScreen"
                        );
                        assert!(self.planned.insert(session.0), "{session:?} planned twice");
                    }
                    Effect::Transcribe { session } | Effect::TranscribeSegments { session } => {
                        assert!(self.started.contains(&session.0));
                        if matches!(f, Effect::TranscribeSegments { .. }) {
                            assert!(self.planned.contains(&session.0), "{f:?} before the plan");
                        }
                        let n = self.transcribe_count.entry(session.0).or_default();
                        *n += 1;
                        assert_eq!(*n, 1, "{session:?} transcribed twice");
                    }
                    Effect::Compose { session, .. } => {
                        assert_eq!(self.transcribe_count.get(&session.0), Some(&1));
                        assert!(
                            self.composed.insert(session.0),
                            "{session:?} composed twice"
                        );
                    }
                    Effect::Deliver { session, .. } => {
                        assert_eq!(self.transcribe_count.get(&session.0), Some(&1));
                        assert!(
                            self.delivered.insert(session.0),
                            "{session:?} delivered twice"
                        );
                    }
                    Effect::DeliverBundle { session } | Effect::DeliverText { session } => {
                        assert!(self.composed.contains(&session.0), "{f:?} before Compose");
                        assert!(
                            self.delivered.insert(session.0),
                            "{session:?} delivered twice"
                        );
                    }
                    Effect::SaveSuccess { session, .. } => {
                        assert!(
                            self.delivered.contains(&session.0),
                            "SaveSuccess before Deliver for {session:?}"
                        );
                        self.terminal(*session, "SaveSuccess");
                    }
                    Effect::SaveAnnotation { session, .. } => {
                        assert!(
                            self.delivered.contains(&session.0),
                            "SaveAnnotation before a delivery for {session:?}"
                        );
                        self.terminal(*session, "SaveAnnotation");
                    }
                    Effect::SaveFailure { session, .. } => self.terminal(*session, "SaveFailure"),
                    Effect::DropAudio { session } => self.terminal(*session, "DropAudio"),
                    Effect::DropAll { session } => self.terminal(*session, "DropAll"),
                    Effect::PauseCapture { session }
                    | Effect::ResumeCapture { session }
                    | Effect::StopCapture { session }
                    | Effect::CancelTranscription { session } => {
                        assert!(
                            self.started.contains(&session.0),
                            "{f:?} for unknown session"
                        );
                        assert!(
                            !self.terminals.contains_key(&session.0),
                            "{f:?} after the session ended"
                        );
                    }
                    Effect::SetEscCapture(on) => self.esc_registered = *on,
                    Effect::ArmTimer { .. } => {}
                }
            }
        }

        fn terminal(&mut self, session: SessionId, what: &str) {
            assert!(
                self.started.contains(&session.0),
                "{what} for unknown {session:?}"
            );
            // An annotation session must have released the screen before it ends.
            if self.screen_started.contains(&session.0) {
                assert!(
                    self.screen_stopped.contains(&session.0),
                    "{what} for {session:?} while its screen stream is open"
                );
                assert!(
                    !self.overlay_shown,
                    "{what} for {session:?} with the overlay up"
                );
            }
            let list = self.terminals.entry(session.0).or_default();
            list.push(what.to_owned());
            assert_eq!(list.len(), 1, "{session:?} ended twice: {list:?}");
        }

        fn assert_all_sessions_ended(&self) {
            for s in &self.started {
                assert_eq!(
                    self.terminals.get(s).map(Vec::len),
                    Some(1),
                    "session {s} did not end exactly once: {:?}",
                    self.terminals.get(s)
                );
            }
            assert_eq!(
                self.screen_started, self.screen_stopped,
                "screen streams left open"
            );
            assert!(!self.overlay_shown, "overlay left up");
        }
    }

    fn random_event(rng: &mut Rng, h: &Harness) -> Option<Event> {
        let active = h.m.active_session();
        let session = |rng: &mut Rng| {
            if rng.below(100) < 80 {
                active.unwrap_or(STALE_SESSION)
            } else {
                SessionId(rng.below(6))
            }
        };
        let event = match rng.below(18) {
            0 => Event::HotkeyDown {
                mode: rng.pick(&[RecordMode::Toggle, RecordMode::PushToTalk]),
            },
            1 => Event::HotkeyUp,
            2 | 3 => Event::Esc,
            4 => Event::PillClicked,
            12 | 13 => Event::AnnotationsHotkey,
            14 => match rng.below(4) {
                0 => Event::AnnotationsRefused {
                    reason: AnnotationsRefusal::ScreenRecordingDenied,
                },
                _ => Event::ScreenFailed {
                    session: session(rng),
                },
            },
            15 => Event::Planned {
                session: session(rng),
                counts: PlanCounts {
                    keyframes: rng.below(7) as u32,
                    marks: rng.below(3) as u32,
                },
            },
            16 => Event::Composed {
                session: session(rng),
                outcome: if rng.below(4) == 0 {
                    ComposeOutcome::Failed
                } else {
                    ComposeOutcome::Bundle {
                        images: 1 + rng.below(6) as u32,
                    }
                },
            },
            17 => Event::AnnotationsDisabled,
            5 | 6 => {
                // The earliest armed timer, fired at its due time.
                let (id, _) = *h.timers.iter().min_by_key(|(_, due)| *due)?;
                Event::TimerFired(id)
            }
            7 => {
                // A stale timer: already fired, or never armed.
                if h.fired.is_empty() || rng.below(2) == 0 {
                    Event::TimerFired(TimerId(1_000_000 + rng.below(50)))
                } else {
                    Event::TimerFired(rng.pick(&h.fired))
                }
            }
            8 | 9 => {
                let outcome = match rng.below(5) {
                    0 => TranscribeOutcome::NoSpeech,
                    1 => TranscribeOutcome::Failed(rng.pick(&[
                        FailureKind::Engine,
                        FailureKind::EmptyResult,
                        FailureKind::ModelMissing,
                    ])),
                    _ => TranscribeOutcome::Text(text("random")),
                };
                transcribed(session(rng), outcome)
            }
            10 => delivered(
                session(rng),
                rng.pick(&[
                    DeliveryOutcome::Pasted,
                    DeliveryOutcome::Copied {
                        reason: CopyReason::PasteFailed,
                    },
                ]),
            ),
            _ => {
                if rng.below(2) == 0 {
                    Event::MicDisconnected {
                        session: session(rng),
                    }
                } else {
                    Event::CaptureFailed {
                        session: session(rng),
                        error: CaptureError::DeviceError,
                    }
                }
            }
        };
        Some(event)
    }

    #[test]
    fn invariants_hold_over_random_event_sequences() {
        let mut endings: BTreeMap<String, u32> = BTreeMap::new();
        for seed in 1..=400u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut h = Harness::new();
            let mut ledger = Ledger::default();
            let mut trace: Vec<Event> = Vec::new();

            for _ in 0..80 {
                h.advance(rng.below(4_000));
                let Some(event) = random_event(&mut rng, &h) else {
                    continue;
                };
                trace.push(event.clone());
                let fx = match &event {
                    Event::TimerFired(id) if h.timers.iter().any(|(t, _)| t == id) => h.fire(*id),
                    _ => h.step(event.clone()),
                };
                ledger.record(&fx);
                assert_eq!(
                    ledger.esc_registered,
                    h.m.wants_esc(),
                    "seed {seed}: Esc registration drifted after {trace:?}\nstate {:?}",
                    h.state()
                );
                if matches!(h.state(), State::Idle { .. }) {
                    assert_eq!(h.m.active_session(), None);
                    ledger.assert_all_sessions_ended();
                } else {
                    let active =
                        h.m.active_session()
                            .expect("a non-idle state has a session");
                    assert!(ledger.started.contains(&active.0));
                    assert!(
                        !ledger.terminals.contains_key(&active.0),
                        "seed {seed}: active session already ended"
                    );
                }
            }

            // Drain: firing every armed timer must bring the machine to rest.
            let mut rounds = 0;
            while !h.timers.is_empty() {
                let fx = h.fire_next();
                ledger.record(&fx);
                assert_eq!(ledger.esc_registered, h.m.wants_esc(), "seed {seed}");
                rounds += 1;
                assert!(rounds < 1_000, "seed {seed}: the drain does not converge");
            }
            assert_eq!(
                h.state(),
                &State::Idle { notice: None },
                "seed {seed}: not at rest after {trace:?}"
            );
            assert!(!ledger.esc_registered);
            ledger.assert_all_sessions_ended();
            for what in ledger.terminals.values().flatten() {
                *endings.entry(what.clone()).or_default() += 1;
            }
        }
        // The generator reached every kind of ending many times.
        for what in [
            "SaveSuccess",
            "SaveFailure",
            "DropAudio",
            "SaveAnnotation",
            "DropAll",
        ] {
            assert!(
                endings.get(what).is_some_and(|n| *n >= 50),
                "{what} was rarely reached: {endings:?}"
            );
        }
    }
}
