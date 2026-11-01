//! SOT: ports
//! What: the traits that adapters implement for third-party speech tech: recognition engines,
//!       voice activity detection and (future) text refiners such as a local LLM.
//! Why: engines change (speech.cpp today; Apple SpeechAnalyzer, cloud models with the user's own
//!      key later). The app depends on these traits, never on an SDK (guardrail G2).
//! Where: `src-tauri/src/adapters/engine.rs` implements `SpeechEngine` and `VoiceDetector` with
//!        speech.cpp. To add an engine: implement the traits in a new adapter, add its models to
//!        `catalog.rs`, add one arm to the engine factory. `src-tauri/src/inference.rs` drives
//!        the streaming detection (S1) while recording; the dictionary is read once when the
//!        stream starts, so every chunk of one recording uses the same vocabulary (a PLAN 2.9
//!        deviation: a dictionary edit applies from the next recording).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use specta::Type;
use thiserror::Error;

use crate::Language;
use crate::catalog::ModelEntry;
use crate::settings::DictionaryEntry;

/// Cooperative cancellation shared between the controller and an engine call in flight.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What an engine can do with the loaded model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub struct EngineCapabilities {
    pub custom_vocabulary: bool,
    pub prompt_vocabulary: bool,
    pub streaming: bool,
}

/// A model to load: its catalog entry and the verified file on disk.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub entry: &'static ModelEntry,
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub struct TranscribeOptions {
    pub language: Language,
    /// Dictionary entries for this language. Engines that cannot use them ignore them; the text
    /// rules still apply replacements for English.
    pub vocabulary: Vec<DictionaryEntry>,
    pub cancel: CancelToken,
}

/// The model output before Murmur's text rules. `Debug` hides the content (G7).
#[derive(Clone, PartialEq, Eq)]
pub struct RawTranscript {
    pub text: String,
    /// Languages the model reported (BCP 47), when it reports any.
    pub languages: Vec<String>,
}

impl std::fmt::Debug for RawTranscript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawTranscript")
            .field(
                "text",
                &format_args!("<redacted {} chars>", self.text.chars().count()),
            )
            .field("languages", &self.languages)
            .finish()
    }
}

/// A region of speech, in samples at `SAMPLE_RATE_HZ`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechRegion {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EngineError {
    #[error("no model is loaded")]
    NotLoaded,
    #[error("model file is unusable: {0}")]
    ModelFile(String),
    #[error("device error: {0}")]
    Device(String),
    #[error("out of memory")]
    OutOfMemory,
    #[error("cancelled")]
    Cancelled,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("engine library version mismatch: {0}")]
    Version(String),
    #[error("engine error: {0}")]
    Other(String),
}

/// A speech recognition engine. One instance lives on the inference thread.
pub trait SpeechEngine: Send {
    fn capabilities(&self) -> EngineCapabilities;
    /// The catalog id of the loaded model, if any.
    fn loaded(&self) -> Option<&'static str>;
    fn load(&mut self, model: &ModelSpec) -> Result<(), EngineError>;
    fn unload(&mut self);
    /// Recognizes mono PCM at `SAMPLE_RATE_HZ`. Must return `Err(Cancelled)` soon after
    /// `opts.cancel` is set.
    fn transcribe(
        &mut self,
        pcm: &[f32],
        opts: &TranscribeOptions,
    ) -> Result<RawTranscript, EngineError>;
}

/// Voice activity detection over a whole buffer, and over audio that arrives while recording.
///
/// The streaming methods feed the audio a piece at a time. A region is returned once it is
/// final and never changes; after `stream_finish` the regions returned over the whole stream are
/// exactly what `regions` returns for the same audio in one piece. One stream at a time: a new
/// `stream_start` replaces the old one. Offsets are samples at `SAMPLE_RATE_HZ` from the first
/// sample pushed.
pub trait VoiceDetector: Send {
    fn regions(&mut self, pcm: &[f32]) -> Result<Vec<SpeechRegion>, EngineError>;
    /// Starts a stream with the same options as `regions`.
    fn stream_start(&mut self) -> Result<(), EngineError>;
    /// Feeds the next samples; returns the regions that became final with them, in order.
    fn stream_push(&mut self, pcm: &[f32]) -> Result<Vec<SpeechRegion>, EngineError>;
    /// Ends the audio; returns the regions left. The stream is gone afterwards.
    fn stream_finish(&mut self) -> Result<Vec<SpeechRegion>, EngineError>;
    /// Drops the stream without a result. A no-op when none is active.
    fn stream_cancel(&mut self);
}

/// Context for a text refiner.
#[derive(Debug, Clone)]
pub struct RefineContext {
    pub language: Language,
}

/// Rewrites the formatted text (future: a local LLM, opt-in, v1.1). The MVP uses the rules in
/// `text.rs` only, so no adapter implements this yet; the port exists so that adding one does not
/// touch the pipeline.
pub trait TextRefiner: Send + Sync {
    fn refine(&self, text: &str, ctx: &RefineContext) -> String;
}
