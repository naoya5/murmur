//! The inference thread: one OS thread that owns the speech engine, the loaded model and the VAD.
//!
//! PLAN 2.6: one inference thread; jobs arrive on a channel; one request at a time. The model stays
//! loaded between recordings (speed is the first principle). This file depends only on the core
//! ports, so any engine adapter plugs in through the two factory functions given to `spawn`.
//!
//! A transcription: VAD regions -> `core::pipeline::plan` -> the engine on each chunk (cancel is
//! checked between chunks and inside the engine) -> `pipeline::join_texts` -> hallucination filter.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use murmur_core::Language;
use murmur_core::catalog::Family;
use murmur_core::pipeline::{self, Plan};
use murmur_core::ports::{
    CancelToken, EngineError, ModelSpec, RawTranscript, SpeechEngine, TranscribeOptions,
    VoiceDetector,
};
use murmur_core::settings::DictionaryEntry;
use murmur_core::text;
use tokio::sync::oneshot;

/// Creates an engine for a model family. Called on the inference thread.
pub type EngineFactory = fn(Family) -> Box<dyn SpeechEngine>;
/// Loads a voice detector from its model file. Called on the inference thread.
pub type VadFactory = fn(&ModelSpec) -> Result<Box<dyn VoiceDetector>, EngineError>;

pub struct TranscribeRequest {
    pub pcm: Arc<Vec<f32>>,
    pub language: Language,
    pub model: ModelSpec,
    pub vad: ModelSpec,
    pub vocabulary: Vec<DictionaryEntry>,
    pub cancel: CancelToken,
}

#[derive(Debug)]
pub struct InferenceOutput {
    /// `None` when there was no speech (VAD found none, or the text was a known hallucination).
    pub raw: Option<RawTranscript>,
    pub asr_ms: u64,
    pub model_id: &'static str,
    /// The model applied the dictionary itself; the text rules then skip replacements.
    pub custom_vocabulary: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum InferenceError {
    #[error("the model for this language is not downloaded")]
    ModelMissing,
    #[error("model load failed: {0}")]
    ModelLoad(EngineError),
    #[error("engine failed: {0}")]
    Engine(EngineError),
    #[error("audio unavailable: {0}")]
    Audio(String),
    #[error("cancelled")]
    Cancelled,
    #[error("the inference thread has stopped")]
    Shutdown,
}

enum Job {
    Preload {
        model: ModelSpec,
        vad: ModelSpec,
    },
    Transcribe {
        req: TranscribeRequest,
        reply: oneshot::Sender<Result<InferenceOutput, InferenceError>>,
    },
    Unload,
    /// Free everything and acknowledge (process exit: Metal aborts if a model is still loaded).
    Shutdown(std::sync::mpsc::Sender<()>),
}

/// Handle to the inference thread. Cheap to clone.
#[derive(Clone)]
pub struct Inference {
    tx: std::sync::mpsc::Sender<Job>,
    loaded: Arc<Mutex<Option<&'static str>>>,
}

impl Inference {
    pub fn spawn(engines: EngineFactory, vads: VadFactory) -> Inference {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let loaded = Arc::new(Mutex::new(None));
        let worker_loaded = loaded.clone();
        let spawned = thread::Builder::new()
            .name("inference".into())
            .spawn(move || {
                let mut worker = Worker {
                    engines,
                    vads,
                    engine: None,
                    vad: None,
                    loaded: worker_loaded,
                };
                while let Ok(job) = rx.recv() {
                    if let Job::Shutdown(ack) = job {
                        worker.unload_all();
                        let _ = ack.send(());
                        return;
                    }
                    worker.handle(job);
                }
                worker.unload_all();
            });
        if let Err(error) = spawned {
            // Without the thread every request fails with `Shutdown`; the app stays usable.
            tracing::error!(%error, "could not start the inference thread");
        }
        Inference { tx, loaded }
    }

    /// Loads the model and the VAD in the background (no-op if already loaded).
    pub fn preload(&self, model: ModelSpec, vad: ModelSpec) {
        let _ = self.tx.send(Job::Preload { model, vad });
    }

    pub async fn transcribe(
        &self,
        req: TranscribeRequest,
    ) -> Result<InferenceOutput, InferenceError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::Transcribe { req, reply })
            .map_err(|_| InferenceError::Shutdown)?;
        rx.await.map_err(|_| InferenceError::Shutdown)?
    }

    /// Frees the model memory (S5). The next request loads it again.
    pub fn unload(&self) {
        let _ = self.tx.send(Job::Unload);
    }

    /// Frees the model and the VAD and waits (bounded) until they are gone. Call before exit.
    pub fn shutdown(&self, timeout: std::time::Duration) {
        let (ack, done) = std::sync::mpsc::channel();
        if self.tx.send(Job::Shutdown(ack)).is_ok() && done.recv_timeout(timeout).is_err() {
            tracing::warn!("the inference thread did not stop in time");
        }
    }

    pub fn loaded_model(&self) -> Option<&'static str> {
        self.loaded.lock().ok().and_then(|guard| *guard)
    }
}

struct LoadedEngine {
    id: &'static str,
    engine: Box<dyn SpeechEngine>,
    custom_vocabulary: bool,
}

struct LoadedVad {
    id: &'static str,
    detector: Box<dyn VoiceDetector>,
}

struct Worker {
    engines: EngineFactory,
    vads: VadFactory,
    engine: Option<LoadedEngine>,
    vad: Option<LoadedVad>,
    loaded: Arc<Mutex<Option<&'static str>>>,
}

impl Worker {
    fn handle(&mut self, job: Job) {
        match job {
            Job::Preload { model, vad } => {
                if let Err(error) = self.ensure_vad(&vad) {
                    tracing::warn!(%error, "VAD preload failed");
                }
                if let Err(error) = self.ensure_model(&model) {
                    tracing::warn!(%error, model = model.entry.id, "model preload failed");
                }
            }
            Job::Transcribe { req, reply } => {
                let result = self.transcribe(req);
                let _ = reply.send(result);
            }
            Job::Unload => self.unload_all(),
            Job::Shutdown(ack) => {
                self.unload_all();
                let _ = ack.send(());
            }
        }
    }

    fn set_loaded(&self, id: Option<&'static str>) {
        if let Ok(mut guard) = self.loaded.lock() {
            *guard = id;
        }
    }

    fn ensure_vad(&mut self, spec: &ModelSpec) -> Result<(), EngineError> {
        if self.vad.as_ref().is_some_and(|v| v.id == spec.entry.id) {
            return Ok(());
        }
        self.vad = None;
        let detector = (self.vads)(spec)?;
        self.vad = Some(LoadedVad {
            id: spec.entry.id,
            detector,
        });
        Ok(())
    }

    fn ensure_model(&mut self, spec: &ModelSpec) -> Result<(), EngineError> {
        if self.engine.as_ref().is_some_and(|e| e.id == spec.entry.id) {
            return Ok(());
        }
        // Free the old model before loading the new one (memory, PLAN 2.7).
        if let Some(mut old) = self.engine.take() {
            old.engine.unload();
        }
        self.set_loaded(None);
        let started = Instant::now();
        let mut engine = (self.engines)(spec.entry.family);
        engine.load(spec)?;
        let custom_vocabulary = engine.capabilities().custom_vocabulary;
        tracing::info!(
            model = spec.entry.id,
            load_ms = started.elapsed().as_millis() as u64,
            "model loaded"
        );
        self.engine = Some(LoadedEngine {
            id: spec.entry.id,
            engine,
            custom_vocabulary,
        });
        self.set_loaded(Some(spec.entry.id));
        Ok(())
    }

    fn unload_all(&mut self) {
        if let Some(mut old) = self.engine.take() {
            old.engine.unload();
        }
        self.vad = None;
        self.set_loaded(None);
    }

    fn transcribe(&mut self, req: TranscribeRequest) -> Result<InferenceOutput, InferenceError> {
        if req.cancel.is_cancelled() {
            return Err(InferenceError::Cancelled);
        }
        self.ensure_model(&req.model)
            .map_err(InferenceError::ModelLoad)?;
        if req.cancel.is_cancelled() {
            return Err(InferenceError::Cancelled);
        }
        let started = Instant::now();
        let pcm: &[f32] = &req.pcm;

        // Without a working VAD, transcribe everything; the hallucination filter still applies.
        let regions = match self.ensure_vad(&req.vad) {
            Ok(()) => match self.vad.as_mut().map(|v| v.detector.regions(pcm)) {
                Some(Ok(regions)) => Some(regions),
                Some(Err(error)) => {
                    tracing::warn!(%error, "VAD failed; transcribing the whole recording");
                    None
                }
                None => None,
            },
            Err(error) => {
                tracing::warn!(%error, "VAD unavailable; transcribing the whole recording");
                None
            }
        };
        let plan = match regions {
            Some(regions) => pipeline::plan(pcm.len(), &regions),
            None if pcm.is_empty() => Plan::NoSpeech,
            None => pipeline::plan(
                pcm.len(),
                &[murmur_core::ports::SpeechRegion {
                    start: 0,
                    end: pcm.len(),
                }],
            ),
        };

        let Some(loaded) = self.engine.as_mut() else {
            return Err(InferenceError::ModelLoad(EngineError::NotLoaded));
        };
        let model_id = loaded.id;
        let custom_vocabulary = loaded.custom_vocabulary;
        let chunks = match plan {
            Plan::NoSpeech => {
                return Ok(InferenceOutput {
                    raw: None,
                    asr_ms: started.elapsed().as_millis() as u64,
                    model_id,
                    custom_vocabulary,
                });
            }
            Plan::Chunks(chunks) => chunks,
        };

        let opts = TranscribeOptions {
            language: req.language,
            vocabulary: req.vocabulary,
            cancel: req.cancel.clone(),
        };
        let mut texts = Vec::with_capacity(chunks.len());
        let mut languages = Vec::new();
        for range in chunks {
            if req.cancel.is_cancelled() {
                return Err(InferenceError::Cancelled);
            }
            let Some(slice) = pcm.get(range) else {
                continue;
            };
            match loaded.engine.transcribe(slice, &opts) {
                Ok(raw) => {
                    texts.push(raw.text);
                    for lang in raw.languages {
                        if !languages.contains(&lang) {
                            languages.push(lang);
                        }
                    }
                }
                Err(EngineError::Cancelled) => return Err(InferenceError::Cancelled),
                Err(error) => return Err(InferenceError::Engine(error)),
            }
        }
        let joined = pipeline::join_texts(&texts);
        let asr_ms = started.elapsed().as_millis() as u64;
        let raw = if text::is_hallucination(&joined) {
            None
        } else {
            Some(RawTranscript {
                text: joined,
                languages,
            })
        };
        Ok(InferenceOutput {
            raw,
            asr_ms,
            model_id,
            custom_vocabulary,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_core::catalog;
    use murmur_core::ports::{EngineCapabilities, SpeechRegion};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LOADS: AtomicUsize = AtomicUsize::new(0);
    static UNLOADS: AtomicUsize = AtomicUsize::new(0);

    /// Writes "w<len>" for each chunk, so tests can see the chunking.
    struct FakeEngine {
        loaded: Option<&'static str>,
    }

    impl SpeechEngine for FakeEngine {
        fn capabilities(&self) -> EngineCapabilities {
            EngineCapabilities {
                custom_vocabulary: false,
                prompt_vocabulary: false,
                streaming: false,
            }
        }
        fn loaded(&self) -> Option<&'static str> {
            self.loaded
        }
        fn load(&mut self, model: &ModelSpec) -> Result<(), EngineError> {
            LOADS.fetch_add(1, Ordering::SeqCst);
            self.loaded = Some(model.entry.id);
            Ok(())
        }
        fn unload(&mut self) {
            UNLOADS.fetch_add(1, Ordering::SeqCst);
            self.loaded = None;
        }
        fn transcribe(
            &mut self,
            pcm: &[f32],
            opts: &TranscribeOptions,
        ) -> Result<RawTranscript, EngineError> {
            if opts.cancel.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            if pcm.first().is_some_and(|s| *s > 0.9) {
                return Ok(RawTranscript {
                    text: "Thank you for watching.".into(),
                    languages: vec![],
                });
            }
            Ok(RawTranscript {
                text: format!("w{}", pcm.len()),
                languages: vec![],
            })
        }
    }

    /// Speech wherever the absolute sample value is above 0.1.
    struct FakeVad;

    impl VoiceDetector for FakeVad {
        fn regions(&mut self, pcm: &[f32]) -> Result<Vec<SpeechRegion>, EngineError> {
            let mut out = Vec::new();
            let mut start = None;
            for (i, s) in pcm.iter().enumerate() {
                match (s.abs() > 0.1, start) {
                    (true, None) => start = Some(i),
                    (false, Some(st)) => {
                        out.push(SpeechRegion { start: st, end: i });
                        start = None;
                    }
                    _ => {}
                }
            }
            if let Some(st) = start {
                out.push(SpeechRegion {
                    start: st,
                    end: pcm.len(),
                });
            }
            Ok(out)
        }
    }

    fn engines(_: Family) -> Box<dyn SpeechEngine> {
        Box::new(FakeEngine { loaded: None })
    }

    fn vads(_: &ModelSpec) -> Result<Box<dyn VoiceDetector>, EngineError> {
        Ok(Box::new(FakeVad))
    }

    fn spec(id: &str) -> ModelSpec {
        ModelSpec {
            entry: catalog::find(id).unwrap(),
            path: PathBuf::from("/dev/null"),
        }
    }

    fn request(pcm: Vec<f32>, model: &str) -> TranscribeRequest {
        TranscribeRequest {
            pcm: Arc::new(pcm),
            language: Language::Ja,
            model: spec(model),
            vad: spec(catalog::VAD_MODEL),
            vocabulary: vec![],
            cancel: CancelToken::new(),
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn silence_is_no_speech() {
        let inference = Inference::spawn(engines, vads);
        let out = rt()
            .block_on(inference.transcribe(request(vec![0.0; 16_000], catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert!(out.raw.is_none());
        assert_eq!(out.model_id, catalog::DEFAULT_JA_MODEL);
    }

    #[test]
    fn short_speech_is_one_chunk_over_the_whole_buffer() {
        let inference = Inference::spawn(engines, vads);
        let mut pcm = vec![0.0; 32_000];
        pcm[8_000..16_000].fill(0.5);
        let out = rt()
            .block_on(inference.transcribe(request(pcm, catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert_eq!(out.raw.unwrap().text, "w32000");
    }

    #[test]
    fn long_speech_is_chunked_and_joined() {
        let inference = Inference::spawn(engines, vads);
        let rate = 16_000;
        let mut pcm = vec![0.0; 70 * rate];
        pcm[rate..20 * rate].fill(0.5);
        pcm[25 * rate..45 * rate].fill(0.5);
        pcm[50 * rate..65 * rate].fill(0.5);
        let out = rt()
            .block_on(inference.transcribe(request(pcm, catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        let text = out.raw.unwrap().text;
        assert!(
            text.split(' ').count() >= 2,
            "expected several chunks, got {text}"
        );
    }

    #[test]
    fn hallucination_becomes_no_speech() {
        let inference = Inference::spawn(engines, vads);
        let out = rt()
            .block_on(inference.transcribe(request(vec![0.95; 16_000], catalog::DEFAULT_EN_MODEL)))
            .unwrap();
        assert!(out.raw.is_none());
    }

    #[test]
    fn cancelled_request_returns_cancelled() {
        let inference = Inference::spawn(engines, vads);
        let req = request(vec![0.5; 16_000], catalog::DEFAULT_JA_MODEL);
        req.cancel.cancel();
        let result = rt().block_on(inference.transcribe(req));
        assert!(matches!(result, Err(InferenceError::Cancelled)));
    }

    #[test]
    fn switching_models_unloads_the_old_one_first() {
        let inference = Inference::spawn(engines, vads);
        let rt = rt();
        let before_unloads = UNLOADS.load(Ordering::SeqCst);
        rt.block_on(inference.transcribe(request(vec![0.5; 16_000], catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert_eq!(inference.loaded_model(), Some(catalog::DEFAULT_JA_MODEL));
        rt.block_on(inference.transcribe(request(vec![0.5; 16_000], catalog::DEFAULT_EN_MODEL)))
            .unwrap();
        assert_eq!(inference.loaded_model(), Some(catalog::DEFAULT_EN_MODEL));
        assert!(UNLOADS.load(Ordering::SeqCst) > before_unloads);
        assert!(LOADS.load(Ordering::SeqCst) >= 2);
    }
}
