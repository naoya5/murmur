//! The inference thread: one OS thread that owns the speech engine, the loaded model and the VAD.
//!
//! PLAN 2.6: one inference thread; jobs arrive on a channel; one request at a time. The model stays
//! loaded between recordings (speed is the first principle). This file depends only on the core
//! ports, so any engine adapter plugs in through the two factory functions given to `spawn`.
//!
//! A transcription: VAD regions -> `core::pipeline::plan` -> the engine on each chunk (cancel is
//! checked between chunks and inside the engine) -> `pipeline::join_texts` -> hallucination filter.
//!
//! S1, streaming: while a recording runs, its audio is pushed here piece by piece (`stream_tap`).
//! The streaming VAD makes regions final as the silences arrive, `pipeline::IncrementalPlan` packs
//! them into the same chunks the offline plan would cut, and each closed chunk is transcribed at
//! once, between the pushes. At stop, `stream_finish` transcribes only the tail. A recording that
//! fits in one chunk (30 s) discards the partial work and goes through the offline path exactly as
//! before, so short dictations keep their full-context accuracy. Any streaming error falls back to
//! the offline path too.

use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use murmur_core::Language;
use murmur_core::catalog::Family;
use murmur_core::pipeline::{self, Finish, IncrementalPlan, Plan};
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

/// One streaming transcription (S1); the controller uses the session id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(pub u64);

/// What a stream needs before the first push. The vocabulary is read once here and used for
/// every chunk of the recording, the tail included.
pub struct StreamRequest {
    pub language: Language,
    pub model: ModelSpec,
    pub vad: ModelSpec,
    pub vocabulary: Vec<DictionaryEntry>,
    pub cancel: CancelToken,
}

/// Samples appended to a recording, called on the audio converter thread.
pub type PcmTap = Box<dyn Fn(&[f32]) + Send + Sync>;

#[derive(Debug)]
pub struct InferenceOutput {
    /// `None` when there was no speech (VAD found none, or the text was a known hallucination).
    pub raw: Option<RawTranscript>,
    /// Engine time the caller waited for: the whole transcription offline, the tail for a stream.
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

type Reply = oneshot::Sender<Result<InferenceOutput, InferenceError>>;

/// Audio that may wait while the worker is busy (an engine call, or a cold model load at the
/// start of a recording, which can take several seconds). 30 s is about 1.9 MB, so memory stays
/// bounded; beyond it the stream gives up and the stop uses the offline path.
const MAX_PENDING_SAMPLES: usize = murmur_core::SAMPLE_RATE_HZ as usize * 30;
const MAX_STREAM_SAMPLES: usize =
    murmur_core::SAMPLE_RATE_HZ as usize * murmur_core::session::MAX_RECORDING_MS as usize / 1000;

#[derive(Default)]
struct StreamBuffer {
    pcm: Vec<f32>,
    abandoned: bool,
}

impl StreamBuffer {
    fn append(&mut self, pcm: &[f32]) {
        if self.abandoned {
            return;
        }
        if pcm.len() > MAX_PENDING_SAMPLES.saturating_sub(self.pcm.len()) {
            self.abandoned = true;
            self.pcm = Vec::new();
        } else {
            self.pcm.extend_from_slice(pcm);
        }
    }
}

type SharedStreamBuffer = Arc<Mutex<StreamBuffer>>;

enum Job {
    Preload {
        model: ModelSpec,
        vad: ModelSpec,
    },
    Transcribe {
        req: TranscribeRequest,
        reply: Reply,
    },
    StreamStart {
        id: StreamId,
        req: StreamRequest,
        buffer: SharedStreamBuffer,
    },
    /// The recording stopped: `req.pcm` is the complete audio, sample for sample what was pushed.
    StreamFinish {
        id: StreamId,
        req: TranscribeRequest,
        reply: Reply,
    },
    StreamCancel {
        id: StreamId,
    },
    Unload,
    /// Answers once every job before it is done (tests and measurements).
    Sync(std::sync::mpsc::Sender<()>),
    /// Free everything and acknowledge (process exit: Metal aborts if a model is still loaded).
    Shutdown(std::sync::mpsc::Sender<()>),
}

/// Handle to the inference thread. Cheap to clone.
#[derive(Clone)]
pub struct Inference {
    tx: std::sync::mpsc::Sender<Job>,
    loaded: Arc<Mutex<Option<&'static str>>>,
    stream_buffer: Arc<Mutex<Option<(StreamId, SharedStreamBuffer)>>>,
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
                    stream: None,
                    loaded: worker_loaded,
                };
                loop {
                    let poll = if worker.stream.as_ref().is_some_and(|s| !s.dead) {
                        Duration::from_millis(10)
                    } else {
                        Duration::from_secs(1)
                    };
                    let job = match rx.recv_timeout(poll) {
                        Ok(job) => job,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            worker.poll_stream();
                            continue;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if let Job::Shutdown(ack) = job {
                        worker.unload_all();
                        let _ = ack.send(());
                        return;
                    }
                    worker.handle(job);
                    worker.poll_stream();
                }
                worker.unload_all();
            });
        if let Err(error) = spawned {
            // Without the thread every request fails with `Shutdown`; the app stays usable.
            tracing::error!(%error, "could not start the inference thread");
        }
        Inference {
            tx,
            loaded,
            stream_buffer: Arc::new(Mutex::new(None)),
        }
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

    /// Starts a stream for a recording that is about to begin. Replaces an active stream.
    pub fn stream_start(&self, id: StreamId, req: StreamRequest) {
        let buffer = Arc::new(Mutex::new(StreamBuffer::default()));
        if let Ok(mut current) = self.stream_buffer.lock() {
            *current = Some((id, buffer.clone()));
        }
        let _ = self.tx.send(Job::StreamStart { id, req, buffer });
    }

    /// The tap to give the audio adapter: every call pushes the samples to the stream.
    pub fn stream_tap(&self, id: StreamId) -> PcmTap {
        let buffer = self.stream_buffer.lock().ok().and_then(|current| {
            current
                .as_ref()
                .filter(|(stream, _)| *stream == id)
                .map(|(_, buffer)| buffer.clone())
        });
        Box::new(move |pcm: &[f32]| {
            if let Some(buffer) = &buffer
                && let Ok(mut buffer) = buffer.lock()
            {
                buffer.append(pcm);
            }
        })
    }

    /// Ends the stream with the complete audio and returns the whole text. Every push made before
    /// this call is coalesced in the stream buffer. An unknown or broken stream, and a
    /// recording that fits in one chunk, go through `transcribe` instead.
    pub async fn stream_finish(
        &self,
        id: StreamId,
        req: TranscribeRequest,
    ) -> Result<InferenceOutput, InferenceError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job::StreamFinish { id, req, reply })
            .map_err(|_| InferenceError::Shutdown)?;
        rx.await.map_err(|_| InferenceError::Shutdown)?
    }

    /// Drops a stream and its partial work. A no-op for an unknown id.
    pub fn stream_cancel(&self, id: StreamId) {
        let _ = self.tx.send(Job::StreamCancel { id });
    }

    /// Frees the model memory (S5). The next request loads it again.
    pub fn unload(&self) {
        let _ = self.tx.send(Job::Unload);
    }

    /// Waits until every job sent before this call is done. False on timeout.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let (ack, done) = std::sync::mpsc::channel();
        self.tx.send(Job::Sync(ack)).is_ok() && done.recv_timeout(timeout).is_ok()
    }

    /// Frees the model and the VAD and waits (bounded) until they are gone. Call before exit.
    pub fn shutdown(&self, timeout: Duration) {
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

/// A recording being transcribed while it is still running.
struct Stream {
    id: StreamId,
    buffer: SharedStreamBuffer,
    model: ModelSpec,
    opts: TranscribeOptions,
    plan: IncrementalPlan,
    /// The audio pushed so far: the chunks are cut from it. Equals the recorder's buffer.
    pcm: Vec<f32>,
    texts: Vec<String>,
    languages: Vec<String>,
    /// Engine time spent while recording, for the log.
    ahead_ms: u64,
    /// Streaming failed; `finish` runs the offline path.
    dead: bool,
}

struct Worker {
    engines: EngineFactory,
    vads: VadFactory,
    engine: Option<LoadedEngine>,
    vad: Option<LoadedVad>,
    stream: Option<Stream>,
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
            Job::StreamStart { id, req, buffer } => self.stream_start(id, req, buffer),
            Job::StreamFinish { id, req, reply } => {
                let result = self.stream_finish(id, req);
                let _ = reply.send(result);
            }
            Job::StreamCancel { id } => {
                if self.stream.as_ref().is_some_and(|s| s.id == id) {
                    self.drop_stream();
                }
            }
            Job::Unload => self.unload_all(),
            Job::Sync(ack) => {
                self.poll_stream();
                let _ = ack.send(());
            }
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
        // A new detector cannot continue the old one's stream.
        self.drop_stream();
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
        self.drop_stream();
        if let Some(mut old) = self.engine.take() {
            old.engine.unload();
        }
        self.vad = None;
        self.set_loaded(None);
    }

    /// Forgets the stream and its partial text; the detector drops its state.
    fn drop_stream(&mut self) {
        if self.stream.take().is_some()
            && let Some(vad) = self.vad.as_mut()
        {
            vad.detector.stream_cancel();
        }
    }

    /// Transcribes the chunks of `pcm` in order. Texts and the languages heard, deduplicated.
    fn transcribe_chunks(
        engine: &mut LoadedEngine,
        pcm: &[f32],
        chunks: &[Range<usize>],
        opts: &TranscribeOptions,
        texts: &mut Vec<String>,
        languages: &mut Vec<String>,
    ) -> Result<(), InferenceError> {
        for range in chunks {
            if opts.cancel.is_cancelled() {
                return Err(InferenceError::Cancelled);
            }
            let Some(slice) = pcm.get(range.clone()) else {
                continue;
            };
            match engine.engine.transcribe(slice, opts) {
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
        Ok(())
    }

    /// VAD found nothing: the model is not needed, loaded or not.
    fn no_speech(&self, model: &ModelSpec, asr_ms: u64) -> InferenceOutput {
        InferenceOutput {
            raw: None,
            asr_ms,
            model_id: model.entry.id,
            custom_vocabulary: self.engine.as_ref().is_some_and(|e| e.custom_vocabulary),
        }
    }

    fn output(
        engine: &LoadedEngine,
        texts: &[String],
        languages: Vec<String>,
        asr_ms: u64,
    ) -> InferenceOutput {
        let joined = pipeline::join_texts(texts);
        let raw = if text::is_hallucination(&joined) {
            None
        } else {
            Some(RawTranscript {
                text: joined,
                languages,
            })
        };
        InferenceOutput {
            raw,
            asr_ms,
            model_id: engine.id,
            custom_vocabulary: engine.custom_vocabulary,
        }
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

        let chunks = match plan {
            Plan::NoSpeech => {
                return Ok(self.no_speech(&req.model, started.elapsed().as_millis() as u64));
            }
            Plan::Chunks(chunks) => chunks,
        };
        let Some(engine) = self.engine.as_mut() else {
            return Err(InferenceError::ModelLoad(EngineError::NotLoaded));
        };
        let opts = TranscribeOptions {
            language: req.language,
            vocabulary: req.vocabulary,
            cancel: req.cancel.clone(),
        };
        let mut texts = Vec::with_capacity(chunks.len());
        let mut languages = Vec::new();
        Self::transcribe_chunks(engine, pcm, &chunks, &opts, &mut texts, &mut languages)?;
        Ok(Self::output(
            engine,
            &texts,
            languages,
            started.elapsed().as_millis() as u64,
        ))
    }

    // --- streaming (S1) ---

    fn stream_start(&mut self, id: StreamId, req: StreamRequest, buffer: SharedStreamBuffer) {
        self.drop_stream();
        let started = match self.ensure_vad(&req.vad) {
            Ok(()) => self
                .vad
                .as_mut()
                .map_or(Err(EngineError::NotLoaded), |v| v.detector.stream_start()),
            Err(error) => Err(error),
        };
        if let Err(error) = started {
            // No stream: the recording is transcribed offline at stop, as before.
            tracing::warn!(stream = id.0, %error, "streaming detection unavailable");
            return;
        }
        self.stream = Some(Stream {
            id,
            buffer,
            model: req.model,
            opts: TranscribeOptions {
                language: req.language,
                vocabulary: req.vocabulary,
                cancel: req.cancel,
            },
            plan: IncrementalPlan::new(),
            pcm: Vec::with_capacity(murmur_core::SAMPLE_RATE_HZ as usize * 120),
            texts: Vec::new(),
            languages: Vec::new(),
            ahead_ms: 0,
            dead: false,
        });
    }

    fn poll_stream(&mut self) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };
        if stream.dead {
            self.stream = Some(stream);
            return;
        }
        let pending = match stream.buffer.lock() {
            Ok(mut buffer) if !buffer.abandoned => Some(std::mem::take(&mut buffer.pcm)),
            _ => None,
        };
        match pending {
            None => {
                stream.dead = true;
                stream.pcm = Vec::new();
                stream.texts.clear();
                stream.languages.clear();
                self.drop_stream_detector();
            }
            Some(pcm) if !pcm.is_empty() => {
                if !self.push_into(&mut stream, &pcm) {
                    self.drop_stream_detector();
                    return;
                }
            }
            Some(_) => {}
        }
        self.stream = Some(stream);
    }

    /// Feeds the samples and transcribes the chunks that closed. False once the stream is
    /// cancelled; an error only marks it dead (offline at stop).
    fn push_into(&mut self, stream: &mut Stream, pcm: &[f32]) -> bool {
        if stream.dead {
            return true;
        }
        if stream.opts.cancel.is_cancelled() {
            return false;
        }
        if pcm.len() > MAX_STREAM_SAMPLES.saturating_sub(stream.pcm.len()) {
            stream.dead = true;
            stream.pcm = Vec::new();
            return true;
        }
        stream.pcm.extend_from_slice(pcm);
        let regions = match self.vad.as_mut() {
            Some(vad) => vad.detector.stream_push(pcm),
            None => Err(EngineError::NotLoaded),
        };
        let regions = match regions {
            Ok(regions) => regions,
            Err(error) => {
                tracing::warn!(stream = stream.id.0, %error, "streaming detection failed; offline at stop");
                stream.dead = true;
                return true;
            }
        };
        for region in regions {
            stream.plan.push(region);
        }
        let ready = stream.plan.take_ready(stream.pcm.len());
        if ready.is_empty() {
            return true;
        }
        // The first chunk may load the model: that happens while the user still speaks.
        if let Err(error) = self.ensure_model(&stream.model) {
            tracing::warn!(stream = stream.id.0, %error, "model unavailable for the stream; offline at stop");
            stream.dead = true;
            return true;
        }
        let Some(engine) = self.engine.as_mut() else {
            stream.dead = true;
            return true;
        };
        let started = Instant::now();
        let result = Self::transcribe_chunks(
            engine,
            &stream.pcm,
            &ready,
            &stream.opts,
            &mut stream.texts,
            &mut stream.languages,
        );
        stream.ahead_ms += started.elapsed().as_millis() as u64;
        match result {
            Ok(()) => true,
            Err(InferenceError::Cancelled) => false,
            Err(error) => {
                tracing::warn!(stream = stream.id.0, %error, "chunk transcription failed; offline at stop");
                stream.dead = true;
                true
            }
        }
    }

    fn stream_finish(
        &mut self,
        id: StreamId,
        req: TranscribeRequest,
    ) -> Result<InferenceOutput, InferenceError> {
        self.poll_stream();
        let Some(stream) = self.stream.take_if(|s| s.id == id) else {
            return self.transcribe(req);
        };
        if stream.dead {
            self.drop_stream_detector();
            return self.transcribe(req);
        }
        let tail = match self.vad.as_mut() {
            Some(vad) => vad.detector.stream_finish(),
            None => Err(EngineError::NotLoaded),
        };
        let tail = match tail {
            Ok(tail) => tail,
            Err(error) => {
                tracing::warn!(stream = id.0, %error, "ending the detection failed; offline");
                return self.transcribe(req);
            }
        };
        if stream.pcm.len() != req.pcm.len() {
            // The stream did not see exactly the recording: trust the recording.
            tracing::warn!(
                stream = id.0,
                pushed = stream.pcm.len(),
                recorded = req.pcm.len(),
                "stream and recording differ; offline"
            );
            return self.transcribe(req);
        }
        if req.cancel.is_cancelled() {
            return Err(InferenceError::Cancelled);
        }
        let started = Instant::now();
        let total = req.pcm.len();
        let chunks = match stream.plan.finish(total, &tail) {
            Finish::WholeBuffer => {
                // Short recording: full context in one piece, exactly the offline path.
                return self.transcribe(req);
            }
            Finish::NoSpeech => return Ok(self.no_speech(&stream.model, 0)),
            Finish::Tail(chunks) => chunks,
        };
        self.ensure_model(&stream.model)
            .map_err(InferenceError::ModelLoad)?;
        let Some(engine) = self.engine.as_mut() else {
            return Err(InferenceError::ModelLoad(EngineError::NotLoaded));
        };
        let Stream {
            opts,
            mut texts,
            mut languages,
            ahead_ms,
            ..
        } = stream;
        let ahead = texts.len();
        Self::transcribe_chunks(engine, &req.pcm, &chunks, &opts, &mut texts, &mut languages)?;
        let tail_ms = started.elapsed().as_millis() as u64;
        tracing::info!(
            stream = id.0,
            chunks_ahead = ahead,
            chunks_tail = chunks.len(),
            ahead_ms,
            tail_ms,
            "stream finished"
        );
        Ok(Self::output(engine, &texts, languages, tail_ms))
    }

    /// Drops the detector's stream state when the worker's own stream is already gone.
    fn drop_stream_detector(&mut self) {
        if let Some(vad) = self.vad.as_mut() {
            vad.detector.stream_cancel();
        }
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
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    type EngineGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
    static ENGINE_GATE: Mutex<Option<EngineGate>> = Mutex::new(None);
    /// The counters are global, so every test of this module runs alone.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

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
            let gate = ENGINE_GATE.lock().unwrap().take();
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            CALLS.fetch_add(1, Ordering::SeqCst);
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
    fn detect(pcm: &[f32]) -> Vec<SpeechRegion> {
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
        out
    }

    /// Streaming: a region is final once a silent sample ended it.
    #[derive(Default)]
    struct FakeVad {
        buf: Vec<f32>,
        given: usize,
        active: bool,
    }

    impl VoiceDetector for FakeVad {
        fn regions(&mut self, pcm: &[f32]) -> Result<Vec<SpeechRegion>, EngineError> {
            Ok(detect(pcm))
        }
        fn stream_start(&mut self) -> Result<(), EngineError> {
            self.buf.clear();
            self.given = 0;
            self.active = true;
            Ok(())
        }
        fn stream_push(&mut self, pcm: &[f32]) -> Result<Vec<SpeechRegion>, EngineError> {
            if !self.active {
                return Err(EngineError::Other("no stream".into()));
            }
            self.buf.extend_from_slice(pcm);
            let all = detect(&self.buf);
            let closed = all.iter().take_while(|r| r.end < self.buf.len()).count();
            let fresh = all[self.given.min(closed)..closed].to_vec();
            self.given = closed;
            Ok(fresh)
        }
        fn stream_finish(&mut self) -> Result<Vec<SpeechRegion>, EngineError> {
            if !self.active {
                return Err(EngineError::Other("no stream".into()));
            }
            let all = detect(&self.buf);
            let rest = all[self.given.min(all.len())..].to_vec();
            self.stream_cancel();
            Ok(rest)
        }
        fn stream_cancel(&mut self) {
            self.active = false;
            self.buf = Vec::new();
            self.given = 0;
        }
    }

    fn engines(_: Family) -> Box<dyn SpeechEngine> {
        Box::new(FakeEngine { loaded: None })
    }

    fn vads(_: &ModelSpec) -> Result<Box<dyn VoiceDetector>, EngineError> {
        Ok(Box::new(FakeVad::default()))
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

    fn stream_request(model: &str, cancel: CancelToken) -> StreamRequest {
        StreamRequest {
            language: Language::Ja,
            model: spec(model),
            vad: spec(catalog::VAD_MODEL),
            vocabulary: vec![],
            cancel,
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// 70 s: speech at 1-20, 25-45 and 50-65 s.
    fn long_pcm() -> Vec<f32> {
        let rate = 16_000;
        let mut pcm = vec![0.0; 70 * rate];
        pcm[rate..20 * rate].fill(0.5);
        pcm[25 * rate..45 * rate].fill(0.5);
        pcm[50 * rate..65 * rate].fill(0.5);
        pcm
    }

    fn push_all(inference: &Inference, id: StreamId, pcm: &[f32]) {
        let tap = inference.stream_tap(id);
        for (i, piece) in pcm.chunks(4_000).enumerate() {
            tap(piece);
            if i % 4 == 3 {
                assert!(inference.wait_idle(Duration::from_secs(10)));
            }
        }
        assert!(inference.wait_idle(Duration::from_secs(10)));
    }

    #[test]
    fn silence_is_no_speech() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let out = rt()
            .block_on(inference.transcribe(request(vec![0.0; 16_000], catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert!(out.raw.is_none());
        assert_eq!(out.model_id, catalog::DEFAULT_JA_MODEL);
    }

    #[test]
    fn short_speech_is_one_chunk_over_the_whole_buffer() {
        let _serial = serial();
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
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let out = rt()
            .block_on(inference.transcribe(request(long_pcm(), catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        let text = out.raw.unwrap().text;
        assert!(
            text.split(' ').count() >= 2,
            "expected several chunks, got {text}"
        );
    }

    #[test]
    fn hallucination_becomes_no_speech() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let out = rt()
            .block_on(inference.transcribe(request(vec![0.95; 16_000], catalog::DEFAULT_EN_MODEL)))
            .unwrap();
        assert!(out.raw.is_none());
    }

    #[test]
    fn cancelled_request_returns_cancelled() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let req = request(vec![0.5; 16_000], catalog::DEFAULT_JA_MODEL);
        req.cancel.cancel();
        let result = rt().block_on(inference.transcribe(req));
        assert!(matches!(result, Err(InferenceError::Cancelled)));
    }

    #[test]
    fn switching_models_unloads_the_old_one_first() {
        let _serial = serial();
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

    // --- streaming ---

    #[test]
    fn ten_minute_backlog_is_bounded_and_finish_uses_offline_text() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let rt = rt();
        let mut pcm = long_pcm();
        pcm.resize(600 * 16000, 0.0);
        let offline = rt
            .block_on(inference.transcribe(request(pcm.clone(), catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        *ENGINE_GATE.lock().unwrap() = Some((entered_tx, release));
        let (reply, reply_rx) = oneshot::channel();
        inference
            .tx
            .send(Job::Transcribe {
                req: request(vec![0.5; 16000], catalog::DEFAULT_JA_MODEL),
                reply,
            })
            .unwrap();
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let id = StreamId(77);
        inference.stream_start(
            id,
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        let tap = inference.stream_tap(id);
        let buffer = inference
            .stream_buffer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .1
            .clone();
        // No inference work can run while ten minutes of audio arrive through the real tap.
        for piece in pcm.chunks(4000) {
            tap(piece);
            assert!(buffer.lock().unwrap().pcm.len() <= MAX_PENDING_SAMPLES);
        }
        assert!(buffer.lock().unwrap().abandoned);
        assert!(buffer.lock().unwrap().pcm.is_empty());
        release_tx.send(()).unwrap();
        let out = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                reply_rx.await.unwrap().unwrap();
                inference
                    .stream_finish(id, request(pcm, catalog::DEFAULT_JA_MODEL))
                    .await
                    .unwrap()
            })
            .await
            .unwrap()
        });
        assert_eq!(out.raw.unwrap().text, offline.raw.unwrap().text);
    }

    #[test]
    fn stream_gives_the_offline_text_and_works_ahead() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let rt = rt();
        let pcm = long_pcm();
        let offline = rt
            .block_on(inference.transcribe(request(pcm.clone(), catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        let offline_text = offline.raw.unwrap().text;
        assert_eq!(offline_text.split(' ').count(), 3, "{offline_text}");

        let id = StreamId(7);
        inference.stream_start(
            id,
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        let calls_before = CALLS.load(Ordering::SeqCst);
        push_all(&inference, id, &pcm);
        let ahead = CALLS.load(Ordering::SeqCst) - calls_before;
        assert_eq!(
            ahead, 2,
            "two chunks close while recording, the third is the tail"
        );

        let out = rt
            .block_on(inference.stream_finish(id, request(pcm, catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert_eq!(out.raw.unwrap().text, offline_text);
        assert_eq!(CALLS.load(Ordering::SeqCst) - calls_before, 3);
    }

    #[test]
    fn stream_short_recording_discards_partials_and_runs_whole() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let rate = 16_000;
        let mut pcm = vec![0.0; 20 * rate];
        pcm[rate..5 * rate].fill(0.5);
        pcm[8 * rate..19 * rate].fill(0.5);
        let id = StreamId(8);
        inference.stream_start(
            id,
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        push_all(&inference, id, &pcm);
        let out = rt()
            .block_on(inference.stream_finish(id, request(pcm, catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert_eq!(out.raw.unwrap().text, "w320000");
    }

    #[test]
    fn stream_without_speech_is_no_speech() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let pcm = vec![0.0; 70 * 16_000];
        let id = StreamId(9);
        inference.stream_start(
            id,
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        push_all(&inference, id, &pcm);
        let out = rt()
            .block_on(inference.stream_finish(id, request(pcm, catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert!(out.raw.is_none());
    }

    #[test]
    fn stream_cancelled_token_is_cancelled() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let pcm = long_pcm();
        let cancel = CancelToken::new();
        let id = StreamId(10);
        inference.stream_start(
            id,
            stream_request(catalog::DEFAULT_JA_MODEL, cancel.clone()),
        );
        push_all(&inference, id, &pcm);
        cancel.cancel();
        let mut req = request(pcm, catalog::DEFAULT_JA_MODEL);
        req.cancel = cancel;
        let result = rt().block_on(inference.stream_finish(id, req));
        assert!(
            matches!(result, Err(InferenceError::Cancelled)),
            "{result:?}"
        );
    }

    #[test]
    fn stream_cancel_and_unknown_id_fall_back_to_offline() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let rt = rt();
        let pcm = long_pcm();
        let offline = rt
            .block_on(inference.transcribe(request(pcm.clone(), catalog::DEFAULT_JA_MODEL)))
            .unwrap()
            .raw
            .unwrap()
            .text;

        let id = StreamId(11);
        inference.stream_start(
            id,
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        push_all(&inference, id, &pcm[..pcm.len() / 2]);
        inference.stream_cancel(id);
        let out = rt
            .block_on(inference.stream_finish(id, request(pcm.clone(), catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert_eq!(out.raw.unwrap().text, offline);

        let out = rt
            .block_on(
                inference.stream_finish(StreamId(99), request(pcm, catalog::DEFAULT_JA_MODEL)),
            )
            .unwrap();
        assert_eq!(out.raw.unwrap().text, offline);
    }

    #[test]
    fn stream_length_mismatch_falls_back_to_offline() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let rt = rt();
        let pcm = long_pcm();
        let id = StreamId(12);
        inference.stream_start(
            id,
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        push_all(&inference, id, &pcm);
        // The recorder reports one sample less than was pushed: the recording wins.
        let shorter = pcm[..pcm.len() - 1].to_vec();
        let offline = rt
            .block_on(inference.transcribe(request(shorter.clone(), catalog::DEFAULT_JA_MODEL)))
            .unwrap()
            .raw
            .unwrap()
            .text;
        let out = rt
            .block_on(inference.stream_finish(id, request(shorter, catalog::DEFAULT_JA_MODEL)))
            .unwrap();
        assert_eq!(out.raw.unwrap().text, offline);
    }

    #[test]
    fn a_new_stream_replaces_the_old_one() {
        let _serial = serial();
        let inference = Inference::spawn(engines, vads);
        let pcm = long_pcm();
        inference.stream_start(
            StreamId(13),
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        push_all(&inference, StreamId(13), &pcm[..pcm.len() / 2]);
        inference.stream_start(
            StreamId(14),
            stream_request(catalog::DEFAULT_JA_MODEL, CancelToken::new()),
        );
        push_all(&inference, StreamId(14), &pcm);
        // Pushes for the replaced stream are ignored.
        push_all(&inference, StreamId(13), &pcm);
        let out = rt()
            .block_on(
                inference.stream_finish(StreamId(14), request(pcm, catalog::DEFAULT_JA_MODEL)),
            )
            .unwrap();
        assert_eq!(out.raw.unwrap().text.split(' ').count(), 3);
    }
}
