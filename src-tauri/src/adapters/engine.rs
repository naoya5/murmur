//! Adapter: the speech engine, speech.cpp 0.10.0 through `speech_sys`. The only user of
//! `speech_sys` (guardrail G2).
//!
//! Rules of the C API that this file keeps (speech.cpp `docs/c-api.md`):
//! - Every request is freed before its model and every model before the process exits; on Metal a
//!   process that exits with a model loaded aborts. The RAII wrappers below do the freeing, and
//!   `inference.rs` drops the engine on shutdown.
//! - A model serves one request at a time. The engine lives on the inference thread, which runs
//!   one job at a time, so no two requests overlap.
//! - `speech_last_error()` is per thread and valid until the next call on that thread, so a status
//!   becomes an `EngineError` right where the call returns.
//! - The library's log lines go to `tracing`. Nothing in this file logs recognized text (G7).
//!
//! Devices: a model is loaded on the GPU (Metal) and falls back to the CPU when no GPU starts, as
//! on CI runners. Warm-up runs the GPU's kernel compilation during the load instead of the first
//! request.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::Once;
use std::time::Instant;

use murmur_core::catalog::{Family, Role};
use murmur_core::ports::{
    CancelToken, EngineCapabilities, EngineError, ModelSpec, RawTranscript, SpeechEngine,
    SpeechRegion, TranscribeOptions, VoiceDetector,
};
use murmur_core::settings::DictionaryEntry;
use murmur_core::{Language, SAMPLE_RATE_HZ};
use speech_sys as ffi;

pub const ENGINE_NAME: &str = "speech.cpp 0.10.0";

/// FastConformer refuses audio shorter than two mel frames (20 ms); such a chunk has no words.
const MIN_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 50;

/// Silero VAD options for the whole-recording detection: the OpenAI `server_vad` values that
/// the speech.cpp CLI uses for `--vad`, which gave sentence-sized regions in the Phase 0 probe.
const VAD_THRESHOLD: f64 = 0.5;
const VAD_SPEECH_PAD_MS: i64 = 300;
const VAD_MIN_SILENCE_MS: i64 = 500;
/// A region longer than this is cut at its longest silence, so that `pipeline::plan` (30 s
/// chunks with 200 ms of padding) never has to split a region in the middle of a word.
const VAD_MAX_SPEECH_S: f64 = 25.0;

/// Creates the engine for a model family. Every family of the catalog runs through speech.cpp,
/// which chooses the family from the model file itself; `load` refuses a file of another task.
pub fn create_engine(_family: Family) -> Box<dyn SpeechEngine> {
    Box::new(SpeechCppEngine { loaded: None })
}

/// Loads Silero VAD on the CPU, where it is faster than on Metal for short pieces.
pub fn create_vad(spec: &ModelSpec) -> Result<Box<dyn VoiceDetector>, EngineError> {
    if spec.entry.role != Role::VoiceDetection {
        return Err(EngineError::Unsupported(format!(
            "{} is not a voice detection model",
            spec.entry.id
        )));
    }
    install_logging();
    check_api_version()?;
    let path = c_path(&spec.path)?;
    let started = Instant::now();
    let model = Model::load(&path, "cpu", false)?;
    let info = model.info()?;
    if info.task() != ffi::SPEECH_TASK_DETECTION {
        return Err(EngineError::Unsupported(format!(
            "{} is not a detection model file",
            spec.entry.id
        )));
    }
    tracing::info!(
        model = spec.entry.id,
        device = %info.device(),
        load_ms = started.elapsed().as_millis() as u64,
        "voice detector loaded"
    );
    Ok(Box::new(SileroVad {
        stream: None,
        model,
    }))
}

// ---------------------------------------------------------------------------------------------
// The recognition engine

/// speech.cpp with one recognition model loaded at a time.
pub struct SpeechCppEngine {
    loaded: Option<LoadedModel>,
}

struct LoadedModel {
    id: &'static str,
    model: Model,
    takes: Takes,
}

/// The request options the loaded model declares, read from its file once.
#[derive(Debug, Clone, Copy, Default)]
struct Takes {
    language: bool,
    keywords: bool,
    prompt: bool,
}

impl SpeechEngine for SpeechCppEngine {
    fn capabilities(&self) -> EngineCapabilities {
        let takes = self.loaded.as_ref().map(|l| l.takes).unwrap_or_default();
        EngineCapabilities {
            custom_vocabulary: takes.keywords,
            prompt_vocabulary: takes.prompt,
            streaming: false,
        }
    }

    fn loaded(&self) -> Option<&'static str> {
        self.loaded.as_ref().map(|l| l.id)
    }

    fn load(&mut self, spec: &ModelSpec) -> Result<(), EngineError> {
        if spec.entry.role != Role::Recognition {
            return Err(EngineError::Unsupported(format!(
                "{} is not a recognition model",
                spec.entry.id
            )));
        }
        install_logging();
        check_api_version()?;
        // Free the old model before the new one takes memory.
        self.loaded = None;
        let path = c_path(&spec.path)?;
        let started = Instant::now();
        let model = Model::load_gpu_or_cpu(&path, true)?;
        let info = model.info()?;
        if info.task() != ffi::SPEECH_TASK_RECOGNITION {
            return Err(EngineError::Unsupported(format!(
                "{} is not a recognition model file",
                spec.entry.id
            )));
        }
        let takes = Takes {
            language: info.takes(ffi::SPEECH_OPT_LANGUAGE),
            keywords: info.takes(ffi::SPEECH_OPT_KEYWORDS),
            prompt: info.takes(ffi::SPEECH_OPT_PROMPT),
        };
        tracing::info!(
            model = spec.entry.id,
            device = %info.device(),
            threads = info.threads(),
            load_ms = started.elapsed().as_millis() as u64,
            ?takes,
            "speech model loaded"
        );
        self.loaded = Some(LoadedModel {
            id: spec.entry.id,
            model,
            takes,
        });
        Ok(())
    }

    fn unload(&mut self) {
        self.loaded = None;
    }

    fn transcribe(
        &mut self,
        pcm: &[f32],
        opts: &TranscribeOptions,
    ) -> Result<RawTranscript, EngineError> {
        let loaded = self.loaded.as_ref().ok_or(EngineError::NotLoaded)?;
        if opts.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        if pcm.len() < MIN_SAMPLES {
            return Ok(RawTranscript {
                text: String::new(),
                languages: Vec::new(),
            });
        }
        let request = Request::new(&loaded.model)?;
        request.set_audio(pcm)?;
        if loaded.takes.language {
            // FastConformer only checks the language against the model's; Qwen3-ASR steers by it.
            // A model that refuses the language still recognizes with `auto`.
            if let Err(error) = request.set_string(ffi::SPEECH_OPT_LANGUAGE, opts.language.code()) {
                tracing::warn!(%error, language = opts.language.code(), "language refused; using auto");
            }
        }
        if loaded.takes.keywords {
            let keywords = keywords_text(&opts.vocabulary);
            if !keywords.is_empty() {
                request.set_string(ffi::SPEECH_OPT_KEYWORDS, &keywords)?;
            }
        } else if loaded.takes.prompt {
            let prompt = prompt_text(&opts.vocabulary, opts.language);
            if !prompt.is_empty() {
                request.set_string(ffi::SPEECH_OPT_PROMPT, &prompt)?;
            }
        }
        request.set_cancel(&opts.cancel)?;
        // SAFETY: a valid request owned by this thread; the cancel token outlives the call.
        let status = unsafe { ffi::speech_transcribe(request.as_ptr()) };
        if status == ffi::SPEECH_CANCELLED {
            return Err(EngineError::Cancelled);
        }
        check("transcribe", status)?;
        request.transcript()
    }
}

/// Haas `keywords`: one keyword a line, the written form, then `=` and the readings separated by
/// commas; a keyword the model already writes needs no reading. The delimiters are removed from
/// the entries so that one entry cannot become two.
fn keywords_text(vocabulary: &[DictionaryEntry]) -> String {
    let mut lines = Vec::with_capacity(vocabulary.len());
    for entry in vocabulary {
        let written = clean_keyword(&entry.written);
        if written.is_empty() {
            continue;
        }
        let readings: Vec<String> = entry
            .readings
            .iter()
            .map(|reading| clean_keyword(reading))
            .filter(|reading| !reading.is_empty())
            .collect();
        if readings.is_empty() {
            lines.push(written);
        } else {
            lines.push(format!("{written}={}", readings.join(",")));
        }
    }
    lines.join("\n")
}

fn clean_keyword(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '=' | ','))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Qwen3-ASR `prompt`: the written forms as context text, in the language's list style.
fn prompt_text(vocabulary: &[DictionaryEntry], language: Language) -> String {
    let separator = match language {
        Language::Ja => "、",
        Language::En => ", ",
    };
    vocabulary
        .iter()
        .map(|entry| entry.written.trim())
        .filter(|written| !written.is_empty())
        .collect::<Vec<_>>()
        .join(separator)
}

// ---------------------------------------------------------------------------------------------
// Voice activity detection

/// Silero VAD over a whole recording, or over audio that arrives while recording (S1).
pub struct SileroVad {
    /// Declared before `model`: Rust drops fields in order, and the C API frees a detection
    /// before its model.
    stream: Option<Detection>,
    model: Model,
}

impl SileroVad {
    /// A request with the detection options; `speech_detect` and `speech_detection_start` read
    /// the same four.
    fn vad_request(&self) -> Result<Request, EngineError> {
        let request = Request::new(&self.model)?;
        request.set_float(ffi::SPEECH_OPT_THRESHOLD, VAD_THRESHOLD)?;
        request.set_int(ffi::SPEECH_OPT_SPEECH_PAD_MS, VAD_SPEECH_PAD_MS)?;
        request.set_int(ffi::SPEECH_OPT_MIN_SILENCE_DURATION_MS, VAD_MIN_SILENCE_MS)?;
        request.set_float(ffi::SPEECH_OPT_MAX_SPEECH_DURATION_S, VAD_MAX_SPEECH_S)?;
        Ok(request)
    }
}

impl VoiceDetector for SileroVad {
    fn regions(&mut self, pcm: &[f32]) -> Result<Vec<SpeechRegion>, EngineError> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        let request = self.vad_request()?;
        request.set_audio(pcm)?;
        // SAFETY: a valid request owned by this thread.
        let status = unsafe { ffi::speech_detect(request.as_ptr()) };
        if status == ffi::SPEECH_CANCELLED {
            return Err(EngineError::Cancelled);
        }
        check("detect", status)?;
        request.regions(pcm.len())
    }

    fn stream_start(&mut self) -> Result<(), EngineError> {
        self.stream = None;
        let request = self.vad_request()?;
        let rate = c_int::try_from(SAMPLE_RATE_HZ)
            .map_err(|_| EngineError::Other("sample rate out of range".into()))?;
        let mut raw = ptr::null_mut();
        // SAFETY: a valid request without audio (its options are copied) and a valid out pointer.
        check("detection start", unsafe {
            ffi::speech_detection_start(request.as_ptr().cast_const(), rate, &mut raw)
        })?;
        let raw = NonNull::new(raw)
            .ok_or_else(|| EngineError::Other("detection start returned nothing".into()))?;
        self.stream = Some(Detection { raw, given: 0 });
        Ok(())
    }

    fn stream_push(&mut self, pcm: &[f32]) -> Result<Vec<SpeechRegion>, EngineError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(EngineError::Other("no detection stream".into()));
        };
        // SAFETY: a valid detection owned by this thread; the samples are copied by the library.
        let status =
            unsafe { ffi::speech_detection_push(stream.raw.as_ptr(), pcm.as_ptr(), pcm.len()) };
        if let Err(error) = check("detection push", status) {
            // The library takes no more audio after a failed push.
            self.stream = None;
            return Err(error);
        }
        stream.new_regions()
    }

    fn stream_finish(&mut self) -> Result<Vec<SpeechRegion>, EngineError> {
        let Some(mut stream) = self.stream.take() else {
            return Err(EngineError::Other("no detection stream".into()));
        };
        // SAFETY: a valid detection owned by this thread.
        check("detection end", unsafe {
            ffi::speech_detection_end(stream.raw.as_ptr())
        })?;
        stream.new_regions()
    }

    fn stream_cancel(&mut self) {
        self.stream = None;
    }
}

/// One streaming detection. Freed on drop, before the model it was made for.
struct Detection {
    raw: NonNull<ffi::speech_detection>,
    /// Regions already handed out.
    given: usize,
}

// SAFETY: a detection is heap state owned by the library and tied to no thread; the C API allows
// its use from one thread at a time, and it lives on the inference thread with its model.
unsafe impl Send for Detection {}

impl Drop for Detection {
    fn drop(&mut self) {
        // SAFETY: the pointer came from speech_detection_start and is freed once.
        unsafe { ffi::speech_detection_free(self.raw.as_ptr()) };
    }
}

impl Detection {
    /// The regions given since the last call, as sample ranges from the first sample pushed.
    /// Not clamped: the caller knows the audio length (`IncrementalPlan::finish`).
    fn new_regions(&mut self) -> Result<Vec<SpeechRegion>, EngineError> {
        let rate = f64::from(SAMPLE_RATE_HZ);
        // SAFETY: a valid detection owned by this thread; every index is below the count.
        let count = unsafe { ffi::speech_detection_region_count(self.raw.as_ptr()) };
        let mut regions = Vec::with_capacity(count.saturating_sub(self.given));
        for index in self.given..count {
            let (mut start, mut end) = (0.0_f64, 0.0_f64);
            // SAFETY: as above.
            check("detection region", unsafe {
                ffi::speech_detection_region(self.raw.as_ptr(), index, &mut start, &mut end)
            })?;
            let start = (start * rate).round().max(0.0) as usize;
            let end = (end * rate).round().max(0.0) as usize;
            if end > start {
                regions.push(SpeechRegion { start, end });
            }
        }
        self.given = count;
        Ok(regions)
    }
}

// ---------------------------------------------------------------------------------------------
// RAII wrappers over the C handles

/// A loaded model. Freed after every request made for it.
struct Model(NonNull<ffi::speech_model>);

// SAFETY: a model is heap state owned by the library and tied to no thread; the C API allows
// its use from any thread as long as one request runs at a time, which the inference thread
// guarantees by running one job at a time.
unsafe impl Send for Model {}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: the pointer came from speech_model_load and is freed once; every request made
        // for it has been dropped (requests never outlive a method call of this file).
        unsafe { ffi::speech_model_free(self.0.as_ptr()) };
    }
}

impl Model {
    /// Loads on the GPU; when no GPU is there or starts, on the CPU.
    fn load_gpu_or_cpu(path: &CStr, warmup: bool) -> Result<Model, EngineError> {
        match Model::load(path, "gpu", warmup) {
            Err(EngineError::Device(detail)) => {
                tracing::warn!(%detail, "no GPU for the speech model; loading on the CPU");
                Model::load(path, "cpu", warmup)
            }
            other => other,
        }
    }

    fn load(path: &CStr, device: &str, warmup: bool) -> Result<Model, EngineError> {
        let params = LoadParams::new(device, warmup)?;
        let mut raw = ptr::null_mut();
        // SAFETY: valid NUL-terminated path, valid params, valid out pointer.
        let status = unsafe { ffi::speech_model_load(path.as_ptr(), params.0.as_ptr(), &mut raw) };
        check("model load", status)?;
        NonNull::new(raw)
            .map(Model)
            .ok_or_else(|| EngineError::Other("model load returned no model".into()))
    }

    fn info(&self) -> Result<ModelInfo, EngineError> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid model and out pointer.
        let status = unsafe { ffi::speech_model_get_info(self.0.as_ptr(), &mut raw) };
        check("model info", status)?;
        NonNull::new(raw)
            .map(ModelInfo)
            .ok_or_else(|| EngineError::Other("model info returned nothing".into()))
    }
}

struct LoadParams(NonNull<ffi::speech_load_params>);

impl Drop for LoadParams {
    fn drop(&mut self) {
        // SAFETY: the pointer came from speech_load_params_new and is freed once.
        unsafe { ffi::speech_load_params_free(self.0.as_ptr()) };
    }
}

impl LoadParams {
    fn new(device: &str, warmup: bool) -> Result<LoadParams, EngineError> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid out pointer.
        check("load params", unsafe {
            ffi::speech_load_params_new(&mut raw)
        })?;
        let params = NonNull::new(raw)
            .map(LoadParams)
            .ok_or_else(|| EngineError::Other("load params returned nothing".into()))?;
        let device = CString::new(device)
            .map_err(|_| EngineError::Other("device name holds a NUL".into()))?;
        // SAFETY: valid params and NUL-terminated device name, copied by the library.
        check("device", unsafe {
            ffi::speech_load_params_set_device(params.0.as_ptr(), device.as_ptr())
        })?;
        // SAFETY: valid params.
        check("warmup", unsafe {
            ffi::speech_load_params_set_warmup(params.0.as_ptr(), c_int::from(warmup))
        })?;
        Ok(params)
    }
}

struct ModelInfo(NonNull<ffi::speech_model_info>);

impl Drop for ModelInfo {
    fn drop(&mut self) {
        // SAFETY: the pointer came from speech_model_get_info and is freed once.
        unsafe { ffi::speech_model_info_free(self.0.as_ptr()) };
    }
}

impl ModelInfo {
    fn task(&self) -> ffi::speech_task {
        // SAFETY: valid info.
        unsafe { ffi::speech_model_info_task(self.0.as_ptr()) }
    }

    fn takes(&self, option: ffi::speech_option) -> bool {
        // SAFETY: valid info.
        unsafe { ffi::speech_model_info_takes(self.0.as_ptr(), option) != 0 }
    }

    fn device(&self) -> String {
        // SAFETY: valid info; the string lives as long as the info.
        unsafe { c_string(ffi::speech_model_info_device(self.0.as_ptr())) }
    }

    fn threads(&self) -> i32 {
        // SAFETY: valid info.
        unsafe { ffi::speech_model_info_threads(self.0.as_ptr()) }
    }
}

/// One request on a model. Used by one thread and dropped before the method returns, so the
/// model it was made for outlives it.
struct Request(NonNull<ffi::speech_request>);

impl Drop for Request {
    fn drop(&mut self) {
        // SAFETY: the pointer came from speech_request_new and is freed once.
        unsafe { ffi::speech_request_free(self.0.as_ptr()) };
    }
}

impl Request {
    fn new(model: &Model) -> Result<Request, EngineError> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid model and out pointer.
        check("request", unsafe {
            ffi::speech_request_new(model.0.as_ptr(), &mut raw)
        })?;
        NonNull::new(raw)
            .map(Request)
            .ok_or_else(|| EngineError::Other("request returned nothing".into()))
    }

    fn as_ptr(&self) -> *mut ffi::speech_request {
        self.0.as_ptr()
    }

    fn set_audio(&self, pcm: &[f32]) -> Result<(), EngineError> {
        let rate = c_int::try_from(SAMPLE_RATE_HZ)
            .map_err(|_| EngineError::Other("sample rate out of range".into()))?;
        // SAFETY: the slice is valid for the call and the library copies it.
        check("audio", unsafe {
            ffi::speech_request_set_audio(self.as_ptr(), pcm.as_ptr(), pcm.len(), rate)
        })
    }

    fn set_string(&self, option: ffi::speech_option, value: &str) -> Result<(), EngineError> {
        let value = CString::new(value)
            .map_err(|_| EngineError::Other(format!("{} holds a NUL", option_name(option))))?;
        // SAFETY: valid request and NUL-terminated string, copied by the library.
        check(option_name(option), unsafe {
            ffi::speech_request_set_string(self.as_ptr(), option, value.as_ptr())
        })
    }

    fn set_int(&self, option: ffi::speech_option, value: i64) -> Result<(), EngineError> {
        // SAFETY: valid request.
        check(option_name(option), unsafe {
            ffi::speech_request_set_int(self.as_ptr(), option, value)
        })
    }

    fn set_float(&self, option: ffi::speech_option, value: f64) -> Result<(), EngineError> {
        // SAFETY: valid request.
        check(option_name(option), unsafe {
            ffi::speech_request_set_float(self.as_ptr(), option, value)
        })
    }

    /// Lets the library poll the cancel token at every step, so a cancelled request returns
    /// `SPEECH_CANCELLED` at the next point its work can stop.
    fn set_cancel(&self, cancel: &CancelToken) -> Result<(), EngineError> {
        let user_data = ptr::from_ref(cancel).cast_mut().cast::<c_void>();
        // SAFETY: valid request; the token lives in the caller's options for the whole call, and
        // the callback runs on the thread that runs the request, before the call returns.
        check("progress", unsafe {
            ffi::speech_request_set_progress(self.as_ptr(), Some(poll_cancel), user_data)
        })
    }

    fn result(&self) -> Result<*const ffi::speech_result, EngineError> {
        // SAFETY: valid request that returned OK; the result lives until the request is freed.
        let result = unsafe { ffi::speech_request_result(self.as_ptr()) };
        if result.is_null() {
            return Err(EngineError::Other("request has no result".into()));
        }
        Ok(result)
    }

    fn transcript(&self) -> Result<RawTranscript, EngineError> {
        let result = self.result()?;
        // SAFETY: valid result owned by this request; every index is below the count.
        unsafe {
            let text = c_string(ffi::speech_result_text(result));
            let count = ffi::speech_result_language_count(result);
            let languages = (0..count)
                .map(|index| c_string(ffi::speech_result_language(result, index)))
                .filter(|language| !language.is_empty())
                .collect();
            Ok(RawTranscript { text, languages })
        }
    }

    /// The segments of a detection as sample ranges, clamped to the audio.
    fn regions(&self, total_samples: usize) -> Result<Vec<SpeechRegion>, EngineError> {
        let result = self.result()?;
        let rate = f64::from(SAMPLE_RATE_HZ);
        // SAFETY: valid result owned by this request; every index is below the count.
        let count = unsafe { ffi::speech_result_segment_count(result) };
        let mut regions = Vec::with_capacity(count);
        for index in 0..count {
            let (mut start, mut end) = (0.0_f64, 0.0_f64);
            // SAFETY: as above; the text is not needed and may be NULL.
            check("segment", unsafe {
                ffi::speech_result_segment(result, index, &mut start, &mut end, ptr::null_mut())
            })?;
            let start = ((start * rate).round().max(0.0) as usize).min(total_samples);
            let end = ((end * rate).round().max(0.0) as usize).clamp(start, total_samples);
            if end > start {
                regions.push(SpeechRegion { start, end });
            }
        }
        Ok(regions)
    }
}

/// The progress callback: returns nonzero, which stops the request, once the token is set.
unsafe extern "C" fn poll_cancel(_done: f64, user_data: *mut c_void) -> c_int {
    // SAFETY: `user_data` is the `CancelToken` that `set_cancel` registered, alive for the call.
    let cancel = unsafe { &*user_data.cast_const().cast::<CancelToken>() };
    c_int::from(cancel.is_cancelled())
}

// ---------------------------------------------------------------------------------------------
// Errors, logging and versions

/// Turns a status into an error, reading the thread's last error message right away.
fn check(what: &str, status: ffi::speech_status) -> Result<(), EngineError> {
    if status >= 0 {
        return Ok(());
    }
    // SAFETY: plain reads of the calling thread's last error, valid until the next call.
    let (message, option) = unsafe {
        (
            c_string(ffi::speech_last_error()),
            c_string(ffi::speech_last_error_option()),
        )
    };
    let detail = if option.is_empty() {
        format!("{what}: {message}")
    } else {
        format!("{what}: {message} (option {option})")
    };
    Err(match status {
        ffi::SPEECH_ERROR_MODEL_FILE | ffi::SPEECH_ERROR_IO => EngineError::ModelFile(detail),
        ffi::SPEECH_ERROR_DEVICE => EngineError::Device(detail),
        ffi::SPEECH_ERROR_OUT_OF_MEMORY => EngineError::OutOfMemory,
        ffi::SPEECH_ERROR_UNSUPPORTED | ffi::SPEECH_ERROR_OUT_OF_RANGE => {
            EngineError::Unsupported(detail)
        }
        _ => EngineError::Other(detail),
    })
}

/// The library must have the API major version these bindings were generated from.
fn check_api_version() -> Result<(), EngineError> {
    // SAFETY: a plain query.
    let major = unsafe { ffi::speech_api_version_major() };
    if i64::from(major) == i64::from(ffi::SPEECH_API_VERSION_MAJOR) {
        return Ok(());
    }
    // SAFETY: a process-lifetime string.
    let version = unsafe { c_string(ffi::speech_version()) };
    Err(EngineError::Version(format!(
        "speech.cpp {version} speaks API {major}.x, the app was built for {}.x",
        ffi::SPEECH_API_VERSION_MAJOR
    )))
}

fn option_name(option: ffi::speech_option) -> &'static str {
    // SAFETY: a process-lifetime string, or NULL for an unknown option.
    let name = unsafe { ffi::speech_option_name(option) };
    if name.is_null() {
        return "option";
    }
    // SAFETY: non-NULL, NUL-terminated, lives as long as the process.
    unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("option")
}

/// Copies a C string the library returned; NULL becomes "".
///
/// # Safety
/// `text` is NULL or a NUL-terminated string valid for the call.
unsafe fn c_string(text: *const c_char) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: as documented above.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

fn c_path(path: &Path) -> Result<CString, EngineError> {
    let text = path
        .to_str()
        .ok_or_else(|| EngineError::ModelFile("model path is not UTF-8".into()))?;
    CString::new(text).map_err(|_| EngineError::ModelFile("model path holds a NUL".into()))
}

/// Sends the library's and ggml's log lines to `tracing`, once per process. The library writes
/// device, memory and model-file messages, never recognized text.
fn install_logging() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        // SAFETY: process-wide setting with a callback that is safe to call from any thread.
        unsafe { ffi::speech_log_set(Some(forward_log), ptr::null_mut()) };
    });
}

unsafe extern "C" fn forward_log(
    level: ffi::speech_log_level,
    text: *const c_char,
    _user_data: *mut c_void,
) {
    // SAFETY: the library passes NULL or a string valid during the call.
    let message = unsafe { c_string(text) };
    let message = message.trim_end();
    if message.is_empty() {
        return;
    }
    match level {
        ffi::SPEECH_LOG_ERROR => tracing::error!(target: "speech.cpp", "{message}"),
        ffi::SPEECH_LOG_WARN => tracing::warn!(target: "speech.cpp", "{message}"),
        ffi::SPEECH_LOG_INFO => tracing::debug!(target: "speech.cpp", "{message}"),
        _ => tracing::trace!(target: "speech.cpp", "{message}"),
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_core::settings::DictionaryScope;

    fn entry(written: &str, readings: &[&str]) -> DictionaryEntry {
        DictionaryEntry {
            id: written.to_owned(),
            written: written.to_owned(),
            readings: readings.iter().map(|r| (*r).to_owned()).collect(),
            scope: DictionaryScope::Both,
        }
    }

    #[test]
    fn keywords_are_one_per_line_with_readings() {
        let text = keywords_text(&[
            entry("Claude Code", &["クロードコード", "クロード コード"]),
            entry("AIモデル", &[]),
            entry("  ", &["ignored"]),
            entry("a=b,c\nd", &["x,y"]),
        ]);
        assert_eq!(
            text,
            "Claude Code=クロードコード,クロード コード\nAIモデル\nabcd=xy"
        );
    }

    #[test]
    fn prompt_lists_written_forms_in_the_language_style() {
        let vocabulary = [entry("Claude Code", &[]), entry("Murmur", &["マーマー"])];
        assert_eq!(
            prompt_text(&vocabulary, Language::Ja),
            "Claude Code、Murmur"
        );
        assert_eq!(
            prompt_text(&vocabulary, Language::En),
            "Claude Code, Murmur"
        );
        assert_eq!(prompt_text(&[], Language::En), "");
    }

    #[test]
    fn api_version_matches_the_bindings() {
        assert_eq!(check_api_version(), Ok(()));
        // A submodule bump must also update ENGINE_NAME, which app_info shows.
        // SAFETY: a process-lifetime string.
        let version = unsafe { c_string(ffi::speech_version()) };
        assert_eq!(ENGINE_NAME, format!("speech.cpp {version}"));
    }

    /// Tests that need the models (`MURMUR_TEST_MODELS`, default `~/.murmur-dev-models`) and run
    /// the real engine through the inference thread. Run them one at a time:
    /// `cargo test -p murmur engine -- --ignored --test-threads=1`.
    mod with_models {
        use super::*;
        use crate::inference::{Inference, InferenceError, InferenceOutput, TranscribeRequest};
        use murmur_core::catalog;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        fn models_dir() -> PathBuf {
            std::env::var("MURMUR_TEST_MODELS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(std::env::var("HOME").unwrap()).join(".murmur-dev-models")
                })
        }

        fn spec(id: &str) -> ModelSpec {
            let entry = catalog::find(id).unwrap();
            let path = models_dir().join(entry.file);
            assert!(path.is_file(), "model missing: {}", path.display());
            ModelSpec { entry, path }
        }

        /// A 16 kHz mono s16 fixture from `tests/fixtures`.
        fn fixture(name: &str) -> Arc<Vec<f32>> {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            let mut reader = hound::WavReader::open(&path).unwrap();
            assert_eq!(reader.spec().sample_rate, SAMPLE_RATE_HZ);
            assert_eq!(reader.spec().channels, 1);
            Arc::new(
                reader
                    .samples::<i16>()
                    .map(|s| f32::from(s.unwrap()) / 32768.0)
                    .collect(),
            )
        }

        fn request(
            pcm: Arc<Vec<f32>>,
            language: Language,
            vocabulary: Vec<DictionaryEntry>,
        ) -> TranscribeRequest {
            let model = match language {
                Language::Ja => catalog::DEFAULT_JA_MODEL,
                Language::En => catalog::DEFAULT_EN_MODEL,
            };
            TranscribeRequest {
                pcm,
                language,
                model: spec(model),
                vad: spec(catalog::VAD_MODEL),
                vocabulary,
                cancel: CancelToken::new(),
            }
        }

        fn run(
            inference: &Inference,
            req: TranscribeRequest,
        ) -> Result<InferenceOutput, InferenceError> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let started = Instant::now();
            let result = runtime.block_on(inference.transcribe(req));
            println!(
                "[engine] round trip {} ms, asr {:?} ms",
                started.elapsed().as_millis(),
                result.as_ref().ok().map(|out| out.asr_ms)
            );
            result
        }

        fn text(out: &InferenceOutput) -> String {
            out.raw
                .as_ref()
                .map(|raw| raw.text.clone())
                .unwrap_or_default()
        }

        fn with_inference(test: impl FnOnce(&Inference)) {
            let inference = Inference::spawn(create_engine, create_vad);
            test(&inference);
            // Free the models before the process ends (Metal aborts otherwise).
            inference.shutdown(Duration::from_secs(30));
        }

        #[test]
        #[ignore]
        fn japanese_sentence_with_punctuation() {
            with_inference(|inference| {
                let out = run(inference, request(fixture("ja.wav"), Language::Ja, vec![])).unwrap();
                let text = text(&out);
                assert!(
                    text.contains("公園"),
                    "unexpected text of {} chars",
                    text.chars().count()
                );
                assert!(text.ends_with('。'));
                // Haas takes `keywords`, so the text rules leave replacements to the model.
                assert!(out.custom_vocabulary);
                // A second request on the resident model: the steady-state time.
                let again =
                    run(inference, request(fixture("ja.wav"), Language::Ja, vec![])).unwrap();
                assert_eq!(text, self::text(&again));
            });
        }

        #[test]
        #[ignore]
        fn english_sentence_through_parakeet() {
            with_inference(|inference| {
                let out = run(inference, request(fixture("en.wav"), Language::En, vec![])).unwrap();
                let text = text(&out);
                assert!(
                    text.contains("settings page"),
                    "unexpected text of {} chars",
                    text.len()
                );
                assert!(text.ends_with('.'));
                assert_eq!(out.model_id, catalog::DEFAULT_EN_MODEL);
                // FastConformer names no language.
                assert!(out.raw.unwrap().languages.is_empty());
            });
        }

        #[test]
        #[ignore]
        fn silence_is_no_speech() {
            with_inference(|inference| {
                let out = run(
                    inference,
                    request(fixture("silence.wav"), Language::Ja, vec![]),
                )
                .unwrap();
                assert!(out.raw.is_none());
            });
        }

        #[test]
        #[ignore]
        fn short_hai() {
            with_inference(|inference| {
                let out =
                    run(inference, request(fixture("hai.wav"), Language::Ja, vec![])).unwrap();
                assert!(text(&out).contains("はい"));
            });
        }

        #[test]
        #[ignore]
        fn haas_takes_the_dictionary_as_keywords() {
            with_inference(|inference| {
                let plain = run(
                    inference,
                    request(fixture("mixed.wav"), Language::Ja, vec![]),
                )
                .unwrap();
                let vocabulary = vec![entry("Claude Code", &["クロードコード"])];
                let boosted = run(
                    inference,
                    request(fixture("mixed.wav"), Language::Ja, vocabulary),
                )
                .unwrap();
                assert!(boosted.custom_vocabulary);
                assert!(text(&boosted).contains("Claude Code"));
                assert!(!text(&plain).contains("Claude Code"));
                println!(
                    "[engine] keywords changed the text: {}",
                    text(&plain) != text(&boosted)
                );
            });
        }

        #[test]
        #[ignore]
        fn long_recording_goes_through_the_chunk_path() {
            with_inference(|inference| {
                let pcm = fixture("ja60.wav");
                let seconds = pcm.len() as f64 / f64::from(SAMPLE_RATE_HZ);
                let out = run(inference, request(pcm, Language::Ja, vec![])).unwrap();
                let text = text(&out);
                println!(
                    "[engine] {seconds:.1} s of audio -> {} chars, asr {} ms, RTF {:.3}",
                    text.chars().count(),
                    out.asr_ms,
                    out.asr_ms as f64 / 1000.0 / seconds
                );
                assert!(text.chars().count() > 300);
                assert!(text.contains("音声入力"));
                // Once more on the resident model, for the steady-state time.
                let again = run(
                    inference,
                    request(fixture("ja60.wav"), Language::Ja, vec![]),
                )
                .unwrap();
                assert_eq!(text, self::text(&again));
            });
        }

        /// S1: the stream must give the offline text, and the tail must cost far less than the
        /// whole. Prints both times.
        #[test]
        #[ignore]
        fn stream_matches_offline_on_a_long_recording() {
            use crate::inference::{StreamId, StreamRequest};
            with_inference(|inference| {
                let pcm = fixture("ja60.wav");
                // Warm the model, then the steady-state offline time.
                run(inference, request(pcm.clone(), Language::Ja, vec![])).unwrap();
                let started = Instant::now();
                let offline = run(inference, request(pcm.clone(), Language::Ja, vec![])).unwrap();
                let offline_ms = started.elapsed().as_millis();

                let id = StreamId(1);
                inference.stream_start(
                    id,
                    StreamRequest {
                        language: Language::Ja,
                        model: spec(catalog::DEFAULT_JA_MODEL),
                        vad: spec(catalog::VAD_MODEL),
                        vocabulary: vec![],
                        cancel: CancelToken::new(),
                    },
                );
                let tap = inference.stream_tap(id);
                // 100 ms pieces, as the converter thread delivers them.
                for piece in pcm.chunks(SAMPLE_RATE_HZ as usize / 10) {
                    tap(piece);
                }
                assert!(inference.wait_idle(Duration::from_secs(120)));
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let started = Instant::now();
                let streamed = runtime
                    .block_on(inference.stream_finish(id, request(pcm, Language::Ja, vec![])))
                    .unwrap();
                let stream_ms = started.elapsed().as_millis();
                println!(
                    "[engine] stream finish {stream_ms} ms (asr {} ms) vs offline {offline_ms} ms (asr {} ms)",
                    streamed.asr_ms, offline.asr_ms
                );
                assert_eq!(text(&streamed), text(&offline));
                assert!(
                    stream_ms < offline_ms,
                    "the tail must be cheaper than the whole"
                );
            });
        }

        #[test]
        #[ignore]
        fn cancel_returns_soon_after_the_token_is_set() {
            with_inference(|inference| {
                // Warm the model first so the measurement is the cancel path alone.
                run(inference, request(fixture("hai.wav"), Language::Ja, vec![])).unwrap();
                let req = request(fixture("ja60.wav"), Language::Ja, vec![]);
                let cancel = req.cancel.clone();
                // 150 ms: after the VAD (about 50 ms) and inside the first chunk's encoder pass,
                // so the measurement is the engine's cancel path, not the check between chunks.
                let canceller = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    cancel.cancel();
                    Instant::now()
                });
                let result = run(inference, req);
                let returned = Instant::now();
                let cancelled_at = canceller.join().unwrap();
                let latency = returned.saturating_duration_since(cancelled_at);
                println!("[engine] cancel latency {} ms", latency.as_millis());
                assert!(
                    matches!(result, Err(InferenceError::Cancelled)),
                    "{result:?}"
                );
                assert!(
                    latency < Duration::from_millis(1500),
                    "cancel took {latency:?}"
                );
            });
        }
    }
}
