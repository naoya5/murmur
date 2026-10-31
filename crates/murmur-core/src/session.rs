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
//! | CancelPending(Rec)         | Esc / PillClicked              | Recording; ResumeCapture, max timer re-armed with the time left |
//! | CancelPending(Rec)         | HotkeyDown                     | Transcribing (audio heard during the pause is dropped) |
//! | CancelPending(any)         | 3 s timer                      | Idle; audio and result discarded, nothing saved  |
//! | Transcribing               | Transcribed(Text)              | Delivering; SetEscCapture(false), Deliver, delivery timer |
//! | Transcribing               | Transcribed(NoSpeech)          | Idle + notice; DropAudio                         |
//! | Transcribing               | Transcribed(Failed) / timeout  | Idle + notice; SaveFailure (audio kept), engine cancelled on timeout |
//! | Transcribing               | Esc                            | CancelPending(from Transcribing); engine keeps running |
//! | CancelPending(Tx)          | Transcribed / timeout          | stays; the outcome is held                       |
//! | CancelPending(Tx)          | Esc / PillClicked              | the held outcome is handled, or back to Transcribing |
//! | Delivering                 | Delivered / delivery timer     | Idle + notice; SaveSuccess (after the paste, G9) |
//! | Idle with notice           | notice timer                   | Idle (pill hides)                                |
//!
//! Every other (state, event) pair is ignored. A result or timer that carries an id other than
//! the current one is stale and ignored. Esc is captured exactly while the state is Recording,
//! CancelPending or Transcribing, so Esc reaches other apps the rest of the time.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::Millis;
use crate::settings::RecordMode;

/// How long the cancel countdown lasts.
pub const CANCEL_WINDOW_MS: Millis = 3_000;
/// The longest recording. At this length the recording stops and is transcribed.
pub const MAX_RECORDING_MS: Millis = 10 * 60 * 1_000;
/// Transcription timeout = base + factor x recorded length (PLAN 2.13).
pub const TRANSCRIBE_TIMEOUT_BASE_MS: Millis = 15_000;
pub const TRANSCRIBE_TIMEOUT_PER_RECORDED_MS: f64 = 0.5;
/// Safety net if the output adapter never reports back (its own paste timeout is 1 s).
pub const DELIVERY_TIMEOUT_MS: Millis = 2_500;
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
}

impl Notice {
    pub fn duration_ms(self) -> Millis {
        match self {
            Notice::Delivered {
                outcome: DeliveryOutcome::Pasted,
            } => NOTICE_OK_MS,
            Notice::Delivered {
                outcome: DeliveryOutcome::Copied { .. },
            } => NOTICE_ERROR_MS,
            Notice::Delivered {
                outcome: DeliveryOutcome::Unconfirmed,
            } => NOTICE_ERROR_MS,
            Notice::NoSpeech => NOTICE_OK_MS * 2,
            Notice::Failed { .. } | Notice::CaptureFailed { .. } => NOTICE_ERROR_MS,
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
    pub mode: RecordMode,
    /// Recorded time before the last resume.
    pub active_before_ms: Millis,
    pub resumed_at: Millis,
    pub max_timer: TimerId,
    /// The microphone went away during the cancel countdown; resuming transcribes instead.
    pub mic_lost: bool,
}

impl Recording {
    fn recorded_ms(&self, now: Millis) -> Millis {
        self.active_before_ms + now.saturating_sub(self.resumed_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcribing {
    pub session: SessionId,
    pub recorded_ms: Millis,
    pub timeout_timer: TimerId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paused {
    /// `rec.active_before_ms` already holds the time recorded until the pause.
    Recording(Recording),
    Transcribing {
        tx: Transcribing,
        held: Option<TranscribeOutcome>,
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
    Delivering {
        session: SessionId,
        timer: TimerId,
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
        }
    }

    /// Whether the global Esc hotkey must be registered in the current state.
    pub fn wants_esc(&self) -> bool {
        matches!(
            self.state,
            State::Recording(_) | State::CancelPending { .. } | State::Transcribing(_)
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
            State::Transcribing(_) => PillView::Transcribing,
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

    fn start_recording(&mut self, mode: RecordMode, now: Millis, fx: &mut Vec<Effect>) -> State {
        let session = SessionId(self.next_session);
        self.next_session += 1;
        fx.push(Effect::StartCapture { session });
        fx.push(Effect::SetEscCapture(true));
        let max_timer = self.timer(MAX_RECORDING_MS, fx);
        State::Recording(Recording {
            session,
            mode,
            active_before_ms: 0,
            resumed_at: now,
            max_timer,
            mic_lost: false,
        })
    }

    fn stop_and_transcribe(
        &mut self,
        session: SessionId,
        recorded_ms: Millis,
        fx: &mut Vec<Effect>,
    ) -> State {
        fx.push(Effect::StopCapture { session });
        fx.push(Effect::Transcribe { session });
        let timeout = TRANSCRIBE_TIMEOUT_BASE_MS
            + (recorded_ms as f64 * TRANSCRIBE_TIMEOUT_PER_RECORDED_MS) as Millis;
        let timeout_timer = self.timer(timeout, fx);
        State::Transcribing(Transcribing {
            session,
            recorded_ms,
            timeout_timer,
        })
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

    /// Handles a transcription outcome while transcribing (or when a held one is released).
    fn finish_transcription(
        &mut self,
        session: SessionId,
        outcome: TranscribeOutcome,
        fx: &mut Vec<Effect>,
    ) -> State {
        match outcome {
            TranscribeOutcome::Text(text) => {
                fx.push(Effect::SetEscCapture(false));
                fx.push(Effect::Deliver { session, text });
                let timer = self.timer(DELIVERY_TIMEOUT_MS, fx);
                State::Delivering { session, timer }
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
                Event::HotkeyDown { mode } => self.start_recording(mode, now, fx),
                Event::TimerFired(id) if notice.is_some_and(|(_, t)| t == id) => {
                    State::Idle { notice: None }
                }
                _ => State::Idle { notice },
            },

            State::Recording(rec) => match event {
                Event::HotkeyDown { .. } if rec.mode == RecordMode::Toggle => {
                    self.stop_and_transcribe(rec.session, rec.recorded_ms(now), fx)
                }
                Event::HotkeyUp if rec.mode == RecordMode::PushToTalk => {
                    self.stop_and_transcribe(rec.session, rec.recorded_ms(now), fx)
                }
                Event::PillClicked => {
                    self.stop_and_transcribe(rec.session, rec.recorded_ms(now), fx)
                }
                Event::TimerFired(id) if id == rec.max_timer => {
                    self.stop_and_transcribe(rec.session, rec.recorded_ms(now), fx)
                }
                Event::MicDisconnected { session } if session == rec.session => {
                    self.stop_and_transcribe(rec.session, rec.recorded_ms(now), fx)
                }
                Event::Esc => {
                    fx.push(Effect::PauseCapture {
                        session: rec.session,
                    });
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
                    fx.push(Effect::DropAudio { session });
                    self.idle_with(Some(Notice::CaptureFailed { error }), fx)
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
                Event::Transcribed { session, outcome } if session == tx.session => {
                    self.finish_transcription(session, outcome, fx)
                }
                Event::TimerFired(id) if id == tx.timeout_timer => {
                    fx.push(Effect::CancelTranscription {
                        session: tx.session,
                    });
                    self.finish_transcription(
                        tx.session,
                        TranscribeOutcome::Failed(FailureKind::Timeout),
                        fx,
                    )
                }
                Event::Esc => {
                    let session = tx.session;
                    self.pause(session, Paused::Transcribing { tx, held: None }, now, fx)
                }
                _ => State::Transcribing(tx),
            },

            State::Delivering { session, timer } => match event {
                Event::Delivered {
                    session: s,
                    outcome,
                } if s == session => {
                    fx.push(Effect::SaveSuccess { session, outcome });
                    self.idle_with(Some(Notice::Delivered { outcome }), fx)
                }
                Event::TimerFired(id) if id == timer => {
                    let outcome = DeliveryOutcome::Unconfirmed;
                    fx.push(Effect::SaveSuccess { session, outcome });
                    self.idle_with(Some(Notice::Delivered { outcome }), fx)
                }
                _ => State::Delivering { session, timer },
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
                match paused {
                    Paused::Recording(_) => fx.push(Effect::StopCapture { session }),
                    Paused::Transcribing { held: None, .. } => {
                        fx.push(Effect::CancelTranscription { session })
                    }
                    Paused::Transcribing { held: Some(_), .. } => {}
                }
                fx.push(Effect::SetEscCapture(false));
                fx.push(Effect::DropAudio { session });
                State::Idle { notice: None }
            }

            // Undo while recording: resume, or transcribe if the microphone went away.
            (Paused::Recording(rec), Event::Esc | Event::PillClicked) => {
                if rec.mic_lost {
                    return self.stop_and_transcribe(session, rec.active_before_ms, fx);
                }
                fx.push(Effect::ResumeCapture { session });
                let left = MAX_RECORDING_MS.saturating_sub(rec.active_before_ms);
                let max_timer = self.timer(left, fx);
                State::Recording(Recording {
                    resumed_at: now,
                    max_timer,
                    ..rec
                })
            }
            (Paused::Recording(rec), Event::HotkeyDown { .. }) => {
                self.stop_and_transcribe(session, rec.active_before_ms, fx)
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
            (Paused::Recording(_), Event::CaptureFailed { session: s, error }) if same(s) => {
                fx.push(Effect::SetEscCapture(false));
                fx.push(Effect::DropAudio { session });
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
            (Paused::Transcribing { tx, held }, Event::Esc | Event::PillClicked) => match held {
                Some(outcome) => self.finish_transcription(session, outcome, fx),
                None => State::Transcribing(tx),
            },

            (paused, _) => State::CancelPending {
                session,
                timer,
                deadline,
                paused,
            },
        }
    }
}
