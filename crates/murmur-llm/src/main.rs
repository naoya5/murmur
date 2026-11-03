//! SOT: llm-sidecar-protocol
//! What: JSON Lines contract for local model loading, prefix warming, generation and cancellation.
//! Why: Keep llama.cpp and its ggml out of Murmur's speech.cpp process, without sockets or text logs.
//! Where: This standalone executable; the app's adapters/refiner.rs is its sole caller.
//!
//! # Transport and lifetime
//! Each UTF-8 stdin line is one JSON object, at most 1,048,576 bytes including the newline.
//! Each stdout line is one response, flushed immediately. No startup handshake is emitted.
//! `id` is a nonnegative u64 or string, echoed unchanged. Use distinct IDs for outstanding calls.
//! Unknown JSON fields are ignored. Invalid JSON, missing required fields, unknown operations or
//! roles and invalid field types produce `invalid_request` with a null ID. Other reader errors
//! echo the ID when available. Never interpret stderr as protocol; native logs are silenced.
//! One worker owns one model/context and a single warm snapshot in memory. Non-cancel operations
//! are rejected with `busy` while a request is active; requests are not queued. Cancel replies can
//! precede or follow the target's response. Correlate by ID, not response order.
//! Each worker request has a 60-second cooperative deadline, checked between native calls and
//! prompt chunks (at most 64 tokens). Loading uses a progress callback too. The caller must impose
//! a hard timeout and kill the process if a native call does not return. Idle stdin is allowed;
//! the app owns the five-minute idle-stop policy. EOF cancels work and allows 500 ms for worker
//! teardown, then calls `_exit(0)` to avoid unbounded native global finalizers. Broken stdout
//! also uses this immediate exit.
//!
//! # Request fields and successful responses
//! Every request requires `id` and `op`. Every response contains `id` and `ok` (boolean).
//! Worker responses additionally contain `total_ms`, the wall time from acceptance until reply,
//! including parsing-independent scheduling, native work and output conversion but not writing.
//! Success has `error:null`; failure has a fixed `error` string and no partial generated text.
//! All durations below are floating-point milliseconds, not timestamps.
//!
//! - `load`: required `model` (local GGUF path string), optional `ctx` (u32, default 2048,
//!   inclusive range 128..=32768). Replaces the previous model and clears its warm state. A failed
//!   replacement leaves no model loaded. Both `default.metallib` and `ggml-tensor.metallib`
//!   must be readable in ggml's bundle-resource/argv-directory lookup before native init.
//!   A Metal backend must initialize successfully; missing or unusable libraries never select
//!   CPU or source compilation. Tensor use remains ggml's device decision; a corrupt tensor
//!   library on a tensor-capable GPU can still trigger ggml's own tensor-disable path.
//!   Success: `load_ms` (acceptance through model/context
//!   creation), `ctx` (effective context size), `gpu_offload_supported` (backend capability bool,
//!   not a claim that every layer was placed on GPU). Requests all layers on Metal.
//! - `warm`: chat input as below, ending in a system or assistant turn. Clears the old snapshot,
//!   evaluates the rendered fixed conversation without a generation header, and snapshots the
//!   whole sequence state, including LFM2 recurrent state, in memory only. Success: `prompt_ms`
//!   (chat validation/rendering, evaluation and snapshot), `prompt_tokens` and `cached_tokens`
//!   (both the fixed prefix's token count), `generated_tokens:0`.
//! - `generate`: chat input ending in a user turn; required `max_tokens` (u32, 1..=8192), optional
//!   `temperature` (f32, default 0, only 0 supported), optional `no_think` (bool or null). Defaults
//!   to true for GGUF architectures starting with `qwen3`, false otherwise. Explicit true on
//!   other architectures is rejected. The Qwen assistant header contains an evaluated empty
//!   think block; generated text is never filtered. A warm snapshot is restored only when every
//!   fixed message matches exactly and its tokens prefix the complete rendered prompt. Otherwise
//!   generation starts with a clear cache. The final user and its result are never cached across
//!   requests. Rendered prompt tokens plus max_tokens must fit ctx.
//!   Success: `text` (raw UTF-8 output, no postprocessing), `prompt_ms` (validation/rendering,
//!   cache restoration and remaining prompt evaluation), `gen_ms` (sampling, decoding and output
//!   conversion), `prompt_tokens` (complete prompt count), `evaluated_prompt_tokens` (uncached
//!   count), `cached_tokens` (restored count or 0), `generated_tokens` (excludes end-of-generation
//!   token), `stop_reason` (`eos` or `max_tokens`), and `no_think` (effective bool). A capped
//!   response is still ok:true; the caller must reject incomplete task outputs when appropriate.
//! - `cancel`: required `target` (JSON value, compared exactly with the active request ID).
//!   Success: `cancel_requested` (true if an active ID matched, false otherwise). A match sets
//!   the cooperative flag; the target normally returns `cancelled`. This ack has no total_ms.
//!
//! # Chat input
//! Prefer `messages:[{"role":"system","content":"..."}, ...]`. Roles are `system`, `user`,
//! `assistant`; content is a string. An optional leading system is followed by alternating
//! user/assistant turns. Warm needs at least a system or a completed user/assistant pair;
//! generate needs a final user. The final user is already in messages, never appended again.
//! Legacy input: `system` string for warm, `system` plus `user` strings for generate. Mixing
//! either legacy field with messages is rejected. Legacy and messages normalize identically
//! and can reuse the same warm snapshot. Embedded NUL content is rejected by chat rendering.
//!
//! # Fixed errors
//! Reader/startup errors (no total_ms): `invalid_request` (schema/JSON), `invalid_id` (not u64 or
//! string), `request_too_large` (line limit, then teardown), `stdin_read_failed` (read/UTF-8,
//! then teardown), `worker_start_failed` (thread spawn, then exit),
//! `worker_unavailable` (lock/channel failure), `busy`.
//! Worker errors (with total_ms): `model_not_loaded` (warm/generate before successful load),
//! `invalid_ctx` (range), `model_load_failed` (file/load/progress abort), `context_create_failed`,
//! `backend_init_failed` (native initialization),
//! `metal_library_missing` (a required metallib is absent/unreadable),
//! `metal_library_load_failed` (the GPU backend could not initialize, including corrupt default),
//! `cancelled`, `timeout`, `ambiguous_chat_input`, `missing_system_or_messages`,
//! `invalid_user_turn` (legacy fields inconsistent with operation), `empty_messages`,
//! `invalid_message_order`, `invalid_final_role`, `only_temperature_zero_supported`,
//! `invalid_max_tokens`, `missing_architecture` (GGUF metadata), `no_think_unsupported_model`,
//! `missing_chat_template`, `invalid_message_content` (NUL), `chat_template_failed`,
//! `missing_assistant_prefix`, `unsupported_assistant_prefix` (unrecognized Qwen template),
//! `invalid_prompt_length` (empty/oversized warm), `context_overflow` (generation budget/position),
//! `batch_failed`, `decode_failed`, `cache_save_failed`, `cache_restore_failed`,
//! `invalid_output_utf8`, `unsupported_worker_operation` (internal dispatch guard).
//! Native error messages and request/model text are never included in errors.

use llama_cpp_2::{
    context::{
        LlamaContext,
        params::LlamaContextParams,
        session::{LlamaStateSeqFlags, SeqState},
    },
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{LlamaChatMessage, LlamaModel, params::LlamaModelParams},
    sampling::LlamaSampler,
    token::LlamaToken,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, &'static str>;
type Active = Arc<Mutex<Option<(Value, Arc<AtomicBool>)>>>;

#[derive(Deserialize)]
struct Request {
    id: Value,
    #[serde(flatten)]
    op: Op,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Op {
    Load {
        model: String,
        #[serde(default = "default_ctx")]
        ctx: u32,
    },
    Warm {
        #[serde(flatten)]
        input: ChatInput,
    },
    Generate {
        #[serde(flatten)]
        input: ChatInput,
        #[serde(default)]
        no_think: Option<bool>,
        max_tokens: u32,
        #[serde(default)]
        temperature: f32,
    },
    Cancel {
        target: Value,
    },
}

fn default_ctx() -> u32 {
    2048
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Message {
    role: Role,
    content: String,
}

#[derive(Default, Deserialize)]
struct ChatInput {
    messages: Option<Vec<Message>>,
    system: Option<String>,
    user: Option<String>,
}

impl ChatInput {
    fn resolve(&self, generation: bool) -> Result<Vec<Message>> {
        let messages = if let Some(messages) = &self.messages {
            if self.system.is_some() || self.user.is_some() {
                return Err("ambiguous_chat_input");
            }
            messages.clone()
        } else {
            let mut messages = vec![Message {
                role: Role::System,
                content: self.system.clone().ok_or("missing_system_or_messages")?,
            }];
            match (&self.user, generation) {
                (Some(user), true) => messages.push(Message {
                    role: Role::User,
                    content: user.clone(),
                }),
                (None, false) => {}
                _ => return Err("invalid_user_turn"),
            }
            messages
        };
        validate_messages(&messages, generation)?;
        Ok(messages)
    }
}

fn validate_messages(messages: &[Message], generation: bool) -> Result<()> {
    if messages.is_empty() {
        return Err("empty_messages");
    }
    let conversation = if messages
        .first()
        .is_some_and(|message| message.role == Role::System)
    {
        &messages[1..]
    } else {
        messages
    };
    for (index, message) in conversation.iter().enumerate() {
        let expected = if index % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        if message.role != expected {
            return Err("invalid_message_order");
        }
    }
    let ends_in_user = messages
        .last()
        .is_some_and(|message| message.role == Role::User);
    if ends_in_user != generation {
        return Err("invalid_final_role");
    }
    Ok(())
}

struct Job {
    req: Request,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

impl Job {
    fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err("cancelled");
        }
        if self.started.elapsed() > Duration::from_secs(60) {
            return Err("timeout");
        }
        Ok(())
    }
}

struct Prefix {
    messages: Vec<Message>,
    tokens: Vec<LlamaToken>,
    state: SeqState,
}

fn emit(value: Value) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if serde_json::to_writer(&mut out, &value).is_err()
        || out.write_all(b"\n").is_err()
        || out.flush().is_err()
    {
        terminate();
    }
}

fn finish(job: &Job, result: Result<Value>, active: &Active) {
    // Clear busy before replying, so a client can immediately send the next request.
    if let Ok(mut guard) = active.lock() {
        *guard = None;
    }
    let mut response = match result {
        Ok(value) => value,
        Err(error) => json!({"ok":false,"error":error}),
    };
    response["id"] = job.req.id.clone();
    response["total_ms"] = json!(job.started.elapsed().as_secs_f64() * 1000.0);
    emit(response);
}

unsafe extern "C" fn drop_log(
    _level: llama_cpp_sys_2::ggml_log_level,
    _text: *const std::ffi::c_char,
    _data: *mut std::ffi::c_void,
) {
}

const METAL_LIBRARIES: &[&str] = &["default.metallib", "ggml-tensor.metallib"];

/// Match ggml's lookup: the app bundle's Resources first, then argv[0]'s directory. Do not
/// consult source-kernel paths or the build machine's output directory at runtime.
fn metallib_roots(executable: &Path, argv0: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(macos) = executable.parent()
        && macos.file_name().is_some_and(|name| name == "MacOS")
        && let Some(contents) = macos.parent()
        && contents.file_name().is_some_and(|name| name == "Contents")
    {
        roots.push(contents.join("Resources"));
    }
    if let Some(directory) = argv0.parent() {
        roots.push(directory.to_path_buf());
    }
    roots
}

fn require_metallibs(roots: &[PathBuf]) -> Result<()> {
    for name in METAL_LIBRARIES {
        if !roots.iter().any(|root| {
            let path = root.join(name);
            path.is_file() && std::fs::File::open(path).is_ok()
        }) {
            return Err("metal_library_missing");
        }
    }
    Ok(())
}

fn initialize_backend() -> Result<LlamaBackend> {
    let executable = std::env::current_exe().map_err(|_| "metal_library_missing")?;
    let argv0 = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .ok_or("metal_library_missing")?;
    require_metallibs(&metallib_roots(&executable, &argv0))?;
    let backend = LlamaBackend::init().map_err(|_| "backend_init_failed")?;
    // GPU registration/support alone does not imply its library loaded. Creating a backend
    // exercises ggml_metal_init; failure must not silently leave model loading on the CPU.
    let metal = unsafe {
        llama_cpp_sys_2::ggml_backend_init_by_type(
            llama_cpp_sys_2::GGML_BACKEND_DEVICE_TYPE_GPU,
            std::ptr::null(),
        )
    };
    if metal.is_null() {
        return Err("metal_library_load_failed");
    }
    // SAFETY: a successful init returns one owned backend; the device's shared library stays
    // cached while this temporary backend context is released before loading any model.
    unsafe { llama_cpp_sys_2::ggml_backend_free(metal) };
    Ok(backend)
}

fn worker(rx: mpsc::Receiver<Job>, active: Active) {
    // Install callbacks before backend initialization, including ggml's independent logger.
    unsafe {
        llama_cpp_sys_2::llama_log_set(Some(drop_log), std::ptr::null_mut());
        llama_cpp_sys_2::ggml_log_set(Some(drop_log), std::ptr::null_mut());
    }
    // Keep initialization errors for the load response so the app receives a fixed, correlated
    // error rather than an unsolicited failure followed by a dead worker.
    let backend_result = initialize_backend();
    let mut pending = None;
    loop {
        let job = match pending.take() {
            Some(job) => job,
            None => match rx.recv_timeout(Duration::from_secs(300)) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            },
        };
        let (path, ctx_size) = match &job.req.op {
            Op::Load { model, ctx } if (128..=32768).contains(ctx) => (model, *ctx),
            Op::Load { .. } => {
                finish(&job, Err("invalid_ctx"), &active);
                continue;
            }
            _ => {
                finish(&job, Err("model_not_loaded"), &active);
                continue;
            }
        };
        // The binding debug-asserts path existence before returning its load error. Validate
        // here so a missing model is a protocol failure in debug builds too, not a dead worker.
        if !std::path::Path::new(path).is_file() {
            finish(&job, Err("model_load_failed"), &active);
            continue;
        }
        let backend = match &backend_result {
            Ok(backend) => backend,
            Err(error) => {
                finish(&job, Err(error), &active);
                continue;
            }
        };
        let started = job.started;
        let cancel = job.cancel.clone();
        let params = LlamaModelParams::default()
            .with_n_gpu_layers(999)
            .with_progress_callback(move |_| {
                !cancel.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(60)
            });
        let model = match LlamaModel::load_from_file(backend, path, &params) {
            Ok(model) => model,
            Err(_) => {
                finish(&job, Err("model_load_failed"), &active);
                continue;
            }
        };
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(ctx_size))
            .with_n_batch(256)
            .with_n_ubatch(64)
            .with_n_threads(4)
            .with_n_threads_batch(4);
        let mut context = match model.new_context(backend, ctx_params) {
            Ok(context) => context,
            Err(_) => {
                finish(&job, Err("context_create_failed"), &active);
                continue;
            }
        };
        let result = job.check().map(|()| {
            json!({"ok":true,"error":null,
            "load_ms":started.elapsed().as_secs_f64()*1000.0,
            "gpu_offload_supported":backend.supports_gpu_offload(),"ctx":ctx_size})
        });
        finish(&job, result, &active);
        let mut prefix = None;
        loop {
            let next = match rx.recv_timeout(Duration::from_secs(300)) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            if matches!(next.req.op, Op::Load { .. }) {
                pending = Some(next);
                break;
            }
            let result = handle(&model, &mut context, &mut prefix, &next);
            finish(&next, result, &active);
        }
    }
}

fn render(
    model: &LlamaModel,
    messages: &[Message],
    generation: bool,
    no_think: bool,
) -> Result<Vec<LlamaToken>> {
    let template = model
        .chat_template(None)
        .map_err(|_| "missing_chat_template")?;
    let chat = messages
        .iter()
        .map(|message| {
            LlamaChatMessage::new(message.role.as_str().into(), message.content.clone())
                .map_err(|_| "invalid_message_content")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut text = model
        .apply_chat_template(&template, &chat, generation)
        .map_err(|_| "chat_template_failed")?;
    if generation && no_think {
        text = disable_qwen_thinking(text)?;
    }
    // Templates include their own BOS, if required. Parse control tokens but do not add BOS twice.
    Ok(model.vocab().tokenize(text.as_bytes(), false, true))
}

fn no_think_mode(architecture: &str, requested: Option<bool>) -> Result<bool> {
    let qwen_thinking = architecture.starts_with("qwen3");
    let enabled = requested.unwrap_or(qwen_thinking);
    if enabled && !qwen_thinking {
        return Err("no_think_unsupported_model");
    }
    Ok(enabled)
}

fn disable_qwen_thinking(mut text: String) -> Result<String> {
    const ASSISTANT: &str = "<|im_start|>assistant\n";
    const EMPTY_THINK: &str = "<think>\n\n</think>\n\n";
    let start = text.rfind(ASSISTANT).ok_or("missing_assistant_prefix")? + ASSISTANT.len();
    match &text[start..] {
        "" | "<think>" | "<think>\n" | "<think>\n\n</think>\n\n" => {}
        _ => return Err("unsupported_assistant_prefix"),
    }
    // The empty block belongs in the evaluated assistant prefix, not a post-generation filter.
    text.truncate(start);
    text.push_str(EMPTY_THINK);
    Ok(text)
}

fn decode(
    context: &mut LlamaContext<'_>,
    tokens: &[LlamaToken],
    offset: usize,
    job: &Job,
) -> Result<()> {
    for (chunk_index, chunk) in tokens.chunks(64).enumerate() {
        job.check()?;
        let mut batch = LlamaBatch::new(64, 1);
        for (index, token) in chunk.iter().enumerate() {
            let position = offset + chunk_index * 64 + index;
            let position = i32::try_from(position).map_err(|_| "context_overflow")?;
            batch
                .add(*token, position, &[0], index + 1 == chunk.len())
                .map_err(|_| "batch_failed")?;
        }
        context.decode(&mut batch).map_err(|_| "decode_failed")?;
    }
    job.check()
}

fn warm(
    model: &LlamaModel,
    context: &mut LlamaContext<'_>,
    prefix: &mut Option<Prefix>,
    messages: &[Message],
    job: &Job,
) -> Result<usize> {
    let tokens = render(model, messages, false, false)?;
    if tokens.is_empty() || tokens.len() >= context.n_ctx() as usize {
        return Err("invalid_prompt_length");
    }
    *prefix = None;
    context.clear_kv_cache();
    decode(context, &tokens, 0, job)?;
    // Save an in-memory snapshot, not only positions. Hybrid LFM2 recurrent state cannot be truncated.
    let state = context
        .state_seq_get(0, LlamaStateSeqFlags::empty())
        .map_err(|_| "cache_save_failed")?;
    let count = tokens.len();
    *prefix = Some(Prefix {
        messages: messages.to_vec(),
        tokens,
        state,
    });
    Ok(count)
}

fn handle(
    model: &LlamaModel,
    context: &mut LlamaContext<'_>,
    prefix: &mut Option<Prefix>,
    job: &Job,
) -> Result<Value> {
    job.check()?;
    match &job.req.op {
        Op::Warm { input } => {
            let started = Instant::now();
            let messages = input.resolve(false)?;
            let count = warm(model, context, prefix, &messages, job)?;
            Ok(
                json!({"ok":true,"error":null,"prompt_ms":started.elapsed().as_secs_f64()*1000.0,
                "prompt_tokens":count,"cached_tokens":count,"generated_tokens":0}),
            )
        }
        Op::Generate {
            input,
            no_think,
            max_tokens,
            temperature,
        } => {
            if *temperature != 0.0 {
                return Err("only_temperature_zero_supported");
            }
            if *max_tokens == 0 || *max_tokens > 8192 {
                return Err("invalid_max_tokens");
            }
            let started = Instant::now();
            let messages = input.resolve(true)?;
            let architecture = model
                .meta_val_str("general.architecture")
                .map_err(|_| "missing_architecture")?;
            let no_think = no_think_mode(&architecture, *no_think)?;
            let tokens = render(model, &messages, true, no_think)?;
            if tokens.is_empty() || tokens.len() + *max_tokens as usize > context.n_ctx() as usize {
                return Err("context_overflow");
            }
            let mut cached = 0;
            if let Some(saved) = prefix.as_ref()
                && saved.messages == messages[..messages.len() - 1]
                && tokens.starts_with(&saved.tokens)
            {
                context.clear_kv_cache();
                context
                    .state_seq_set(&saved.state, 0)
                    .map_err(|_| "cache_restore_failed")?;
                cached = saved.tokens.len();
            }
            if cached == 0 {
                context.clear_kv_cache();
            }
            decode(context, &tokens[cached..], cached, job)?;
            let prompt_ms = started.elapsed().as_secs_f64() * 1000.0;
            let generation = Instant::now();
            let mut sampler = LlamaSampler::greedy();
            let mut generated = Vec::new();
            let mut stop_reason = "max_tokens";
            for index in 0..*max_tokens as usize {
                job.check()?;
                let token = sampler.sample(context, -1);
                if model.vocab().is_eog(token) {
                    stop_reason = "eos";
                    break;
                }
                sampler.accept(token);
                generated.push(token);
                if index + 1 < *max_tokens as usize {
                    decode(context, &[token], tokens.len() + index, job)?;
                }
            }
            job.check()?;
            let text = String::from_utf8(model.vocab().detokenize(&generated, false, false))
                .map_err(|_| "invalid_output_utf8")?;
            Ok(
                json!({"ok":true,"error":null,"text":text,"prompt_ms":prompt_ms,
                "gen_ms":generation.elapsed().as_secs_f64()*1000.0,"prompt_tokens":tokens.len(),
                "evaluated_prompt_tokens":tokens.len()-cached,"cached_tokens":cached,
                "generated_tokens":generated.len(),"stop_reason":stop_reason,"no_think":no_think}),
            )
        }
        _ => Err("unsupported_worker_operation"),
    }
}

unsafe extern "C" {
    fn _exit(status: i32) -> !;
}

fn terminate() -> ! {
    // Emergency teardown must skip native global destructors while Metal resources are live.
    unsafe { _exit(0) }
}

fn main() {
    let active: Active = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let worker_active = active.clone();
    if std::thread::Builder::new()
        .name("llm-worker".into())
        .spawn(move || {
            worker(rx, worker_active);
            let _ = done_tx.try_send(());
        })
        .is_err()
    {
        emit(json!({"id":null,"ok":false,"error":"worker_start_failed"}));
        return;
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        let mut line = String::new();
        match input.by_ref().take(1_048_577).read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => {
                emit(json!({"id":null,"ok":false,"error":"stdin_read_failed"}));
                break;
            }
        }
        if line.len() > 1_048_576 {
            emit(json!({"id":null,"ok":false,"error":"request_too_large"}));
            break;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(_) => {
                emit(json!({"id":null,"ok":false,"error":"invalid_request"}));
                continue;
            }
        };
        if !req.id.is_u64() && !req.id.is_string() {
            emit(json!({"id":req.id,"ok":false,"error":"invalid_id"}));
            continue;
        }
        let mut guard = match active.lock() {
            Ok(guard) => guard,
            Err(_) => {
                emit(json!({"id":req.id,"ok":false,"error":"worker_unavailable"}));
                break;
            }
        };
        if let Op::Cancel { target } = &req.op {
            let found = if let Some((id, flag)) = guard.as_ref() {
                if id == target {
                    flag.store(true, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            emit(json!({"id":req.id,"ok":true,"error":null,"cancel_requested":found}));
            continue;
        }
        if guard.is_some() {
            emit(json!({"id":req.id,"ok":false,"error":"busy"}));
            continue;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *guard = Some((req.id.clone(), cancel.clone()));
        let id = req.id.clone();
        let job = Job {
            req,
            cancel,
            started: Instant::now(),
        };
        if tx.try_send(job).is_err() {
            *guard = None;
            emit(json!({"id":id,"ok":false,"error":"worker_unavailable"}));
        }
    }
    if let Ok(guard) = active.lock()
        && let Some((_, cancel)) = guard.as_ref()
    {
        cancel.store(true, Ordering::Relaxed);
    }
    drop(tx);
    let _ = done_rx.recv_timeout(Duration::from_millis(500));
    // Worker-owned resources already had their grace period. Native global finalizers can
    // otherwise block after main returns, outside the bounded worker teardown above.
    terminate();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
        }
    }

    #[test]
    fn messages_accept_fixed_shots_and_final_user() {
        let fixed = vec![
            message(Role::System, "s"),
            message(Role::User, "example"),
            message(Role::Assistant, "answer"),
        ];
        assert!(validate_messages(&fixed, false).is_ok());
        let mut full = fixed.clone();
        full.push(message(Role::User, "final"));
        let input = ChatInput {
            messages: Some(full.clone()),
            ..ChatInput::default()
        };
        assert_eq!(input.resolve(true), Ok(full));
        let warm = ChatInput {
            messages: Some(fixed.clone()),
            ..ChatInput::default()
        };
        assert_eq!(warm.resolve(false), Ok(fixed));
    }

    #[test]
    fn legacy_fields_resolve_to_equivalent_messages() {
        let input = ChatInput {
            system: Some("s".into()),
            user: Some("u".into()),
            messages: None,
        };
        assert_eq!(
            input.resolve(true),
            Ok(vec![message(Role::System, "s"), message(Role::User, "u")])
        );
        assert!(input.resolve(false).is_err());
    }

    #[test]
    fn messages_reject_ambiguous_empty_or_invalid_turns() {
        let input = ChatInput {
            messages: Some(vec![message(Role::User, "u")]),
            system: Some("s".into()),
            user: None,
        };
        assert_eq!(input.resolve(true), Err("ambiguous_chat_input"));
        assert!(validate_messages(&[], true).is_err());
        assert!(validate_messages(&[message(Role::Assistant, "a")], false).is_err());
        assert!(
            validate_messages(
                &[message(Role::User, "u"), message(Role::System, "s")],
                true
            )
            .is_err()
        );
        assert!(validate_messages(&[message(Role::User, "u")], false).is_err());
        assert!(validate_messages(&[message(Role::System, "s")], true).is_err());
        assert!(serde_json::from_str::<Message>(r#"{"role":"tool","content":"u"}"#).is_err());
    }

    #[test]
    fn qwen_non_thinking_is_automatic_and_can_be_disabled() {
        for architecture in ["qwen3", "qwen3moe", "qwen35", "qwen35moe"] {
            assert_eq!(no_think_mode(architecture, None), Ok(true));
            assert_eq!(no_think_mode(architecture, Some(false)), Ok(false));
            assert_eq!(no_think_mode(architecture, Some(true)), Ok(true));
        }
        assert_eq!(no_think_mode("lfm2", None), Ok(false));
        assert_eq!(
            no_think_mode("qwen2", Some(true)),
            Err("no_think_unsupported_model")
        );
    }

    #[test]
    fn non_thinking_prefix_is_empty_and_idempotent() {
        let head = "<|im_start|>user\nKeep literal <think>data</think>.<|im_end|>\n<|im_start|>assistant\n";
        let expected = format!("{head}<think>\n\n</think>\n\n");
        for suffix in ["", "<think>", "<think>\n", "<think>\n\n</think>\n\n"] {
            assert_eq!(
                disable_qwen_thinking(format!("{head}{suffix}")),
                Ok(expected.clone())
            );
        }
    }

    #[test]
    fn non_thinking_rejects_unrecognized_prefix_without_stripping_text() {
        assert!(disable_qwen_thinking("no assistant prefix".into()).is_err());
        assert!(disable_qwen_thinking("<|im_start|>assistant\nactual content".into()).is_err());
    }

    #[test]
    fn metallib_lookup_matches_dev_and_bundle_layouts() {
        let dev = Path::new("/work/target/debug/murmur-llm");
        assert_eq!(
            metallib_roots(dev, dev),
            vec![PathBuf::from("/work/target/debug")]
        );
        let bundled = Path::new("/Applications/Murmur.app/Contents/MacOS/murmur-llm");
        assert_eq!(
            metallib_roots(bundled, bundled),
            vec![
                PathBuf::from("/Applications/Murmur.app/Contents/Resources"),
                PathBuf::from("/Applications/Murmur.app/Contents/MacOS"),
            ]
        );
    }

    #[test]
    fn load_defaults_context() {
        let request = serde_json::from_str::<Request>(r#"{"id":1,"op":"load","model":"m.gguf"}"#);
        assert!(matches!(
            request,
            Ok(Request {
                op: Op::Load { ctx: 2048, .. },
                ..
            })
        ));
    }

    #[test]
    fn generate_defaults_temperature_zero() {
        let request = serde_json::from_str::<Request>(
            r#"{"id":"test","op":"generate","system":"s","user":"u","max_tokens":10}"#,
        );
        assert!(matches!(
            request,
            Ok(Request {
                op: Op::Generate {
                    temperature: 0.0,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn unknown_operations_and_malformed_requests_are_rejected() {
        for line in [
            "not json",
            r#"{"id":1,"op":"network"}"#,
            r#"{"id":1,"op":"generate","system":"s"}"#,
        ] {
            assert!(serde_json::from_str::<Request>(line).is_err());
        }
    }

    #[test]
    fn cancellation_and_deadline_are_explicit_errors() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut job = Job {
            req: Request {
                id: json!(1),
                op: Op::Warm {
                    input: ChatInput {
                        system: Some(String::new()),
                        ..ChatInput::default()
                    },
                },
            },
            cancel: flag.clone(),
            started: Instant::now(),
        };
        assert!(job.check().is_ok());
        flag.store(true, Ordering::Relaxed);
        assert_eq!(job.check(), Err("cancelled"));
        flag.store(false, Ordering::Relaxed);
        job.started = Instant::now() - Duration::from_secs(61);
        assert_eq!(job.check(), Err("timeout"));
    }
}
