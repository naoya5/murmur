//! Adapter: the local LLM of the refine step (experimental), through the murmur-llm sidecar.
//!
//! One sidecar process per step (cleanup, translation), each holding one model
//! (`catalog::refine_model`). A process starts when a recording first needs it (the warm-up at
//! recording start), serves later recordings, and stops after `REFINER_IDLE_STOP_MS` unused
//! (D3), when the feature is switched off, and at exit. The wire protocol is the sidecar's SOT
//! (`crates/murmur-llm/src/main.rs`): JSON Lines over stdin and stdout. Its stderr is discarded.
//!
//! Every wait has a timeout (G5): the start (a cancel with no target answers at once, since the
//! protocol has no greeting), the model load, the warm-up, the generation (the step's budget), and
//! the reply to a cancel, after which the process is killed. A process that ends by itself is a
//! crash; `refine::RestartBudget` decides whether it may start again. No prompt or answer text
//! reaches the log (G7): only ids, error codes, token counts and times.
//!
//! Only transport lives here. Prompts, validation and the order of the steps are in
//! `murmur_core::refine`; `crate::refine_session` makes the calls.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use murmur_core::Millis;
use murmur_core::ports::{CancelToken, GenerateRequest, Generated, RefineError, TextRefiner};
use murmur_core::refine::{self, ChatMessage, RefineKind, RestartBudget};
use serde_json::{Map, Value, json};

/// The sidecar's file name next to the app executable (Tauri `bundle.externalBin`).
pub const SIDECAR_NAME: &str = "murmur-llm";

/// How often a wait looks at the cancel token.
const POLL: Duration = Duration::from_millis(20);
/// A stopped sidecar exits within 0.5 s of the end of its input; then it is killed.
const STOP_GRACE: Duration = Duration::from_millis(1_000);
/// The longest a call waits for the process slot that another call holds: a warm-up that has
/// to start the process and load the model holds it that long at most.
const SLOT_WAIT: Duration = Duration::from_millis(
    refine::REFINER_START_TIMEOUT_MS
        + refine::REFINER_LOAD_TIMEOUT_MS
        + refine::REFINER_WARM_TIMEOUT_MS,
);

/// How long the exit waits for a busy process slot.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(1_500);

/// Finds the model file of a step; `None` while it is not downloaded.
pub type ModelPaths = Box<dyn Fn(RefineKind) -> Option<PathBuf> + Send + Sync>;

/// The sidecar bundled next to the app executable, if it is there.
pub fn bundled_sidecar() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join(SIDECAR_NAME);
    path.is_file().then_some(path)
}

fn ms(millis: Millis) -> Duration {
    Duration::from_millis(millis)
}

/// The refine models behind `ports::TextRefiner`. Cheap to clone; all clones share the processes.
#[derive(Clone)]
pub struct Refiner {
    inner: Arc<Inner>,
}

struct Inner {
    binary: Option<PathBuf>,
    model_paths: ModelPaths,
    origin: Instant,
    clean: Sidecar,
    translate: Sidecar,
}

#[derive(Default)]
struct Sidecar {
    slot: Mutex<Slot>,
    /// The process runs with its model loaded. Read without the lock (`ready`).
    loaded: AtomicBool,
}

struct Slot {
    process: Option<Process>,
    /// The prefix the sidecar holds as its warm snapshot.
    warmed: Option<Vec<ChatMessage>>,
    restarts: RestartBudget,
    /// The last process ended by itself: the next start is a restart.
    crashed: bool,
    last_used: Instant,
}

impl Default for Slot {
    fn default() -> Self {
        Slot {
            process: None,
            warmed: None,
            restarts: RestartBudget::default(),
            crashed: false,
            last_used: Instant::now(),
        }
    }
}

/// Why a call ended without its response.
#[derive(Debug)]
enum CallError {
    /// The process ended (or its pipes broke) by itself.
    Exited,
    /// The cancel token was set; the sidecar answered the cancel.
    Cancelled,
    /// The deadline passed; the sidecar answered the cancel.
    Deadline,
    /// The sidecar did not answer the cancel in time and was killed.
    Killed { cancelled: bool },
}

impl CallError {
    fn into_refine_error(self) -> RefineError {
        match self {
            CallError::Exited => RefineError::Unavailable("sidecar exited".to_owned()),
            CallError::Cancelled | CallError::Killed { cancelled: true } => RefineError::Cancelled,
            CallError::Deadline | CallError::Killed { cancelled: false } => RefineError::Timeout,
        }
    }
}

/// One running sidecar.
struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Process {
    fn spawn(binary: &Path) -> Result<Process, RefineError> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| RefineError::Unavailable(format!("could not start: {}", e.kind())))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            kill_and_reap(&mut child);
            return Err(RefineError::Unavailable("no pipes".to_owned()));
        };
        let (tx, lines) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("murmur-llm-reader".to_owned())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if tx.send(line.trim_end().to_owned()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        if reader.is_err() {
            kill_and_reap(&mut child);
            return Err(RefineError::Unavailable("no reader thread".to_owned()));
        }
        Ok(Process {
            child,
            stdin: Some(stdin),
            lines,
            next_id: 1,
        })
    }

    /// Writes one request line. Requests are far below the pipe buffer, and the sidecar reads its
    /// input all the time (to see cancels), so the write does not block.
    fn send(&mut self, body: &Value) -> Result<(), CallError> {
        let stdin = self.stdin.as_mut().ok_or(CallError::Exited)?;
        let mut line = body.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|_| CallError::Exited)
    }

    /// Sends `op` with `fields` and waits for its response until `deadline`, or until `cancel` is
    /// set. Either of those cancels the request on the sidecar.
    fn call(
        &mut self,
        op: &str,
        fields: Map<String, Value>,
        deadline: Instant,
        cancel: Option<&CancelToken>,
    ) -> Result<Map<String, Value>, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut body = fields;
        body.insert("id".to_owned(), json!(id));
        body.insert("op".to_owned(), json!(op));
        self.send(&Value::Object(body))?;
        loop {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(self.abort(id, true));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(self.abort(id, false));
            }
            match self.lines.recv_timeout((deadline - now).min(POLL)) {
                Ok(line) => {
                    if let Some(response) = response_to(&line, id) {
                        return Ok(response);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(CallError::Exited),
            }
        }
    }

    /// Cancels request `id` and waits `REFINER_CANCEL_REPLY_MS` for its response; a sidecar that
    /// does not answer is killed.
    fn abort(&mut self, id: u64, cancelled: bool) -> CallError {
        let cancel_id = self.next_id;
        self.next_id += 1;
        let answered = self
            .send(&json!({"id": cancel_id, "op": "cancel", "target": id}))
            .is_ok()
            && {
                let until = Instant::now() + ms(refine::REFINER_CANCEL_REPLY_MS);
                loop {
                    let now = Instant::now();
                    if now >= until {
                        break false;
                    }
                    match self.lines.recv_timeout(until - now) {
                        Ok(line) if response_to(&line, id).is_some() => break true,
                        Ok(_) => {}
                        Err(_) => break false,
                    }
                }
            };
        if answered {
            if cancelled {
                CallError::Cancelled
            } else {
                CallError::Deadline
            }
        } else {
            self.kill();
            CallError::Killed { cancelled }
        }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn kill(&mut self) {
        self.stdin = None;
        kill_and_reap(&mut self.child);
    }

    /// Closes the input (the sidecar cancels its work and exits) and waits a little for the
    /// exit, then kills it.
    fn stop(mut self) {
        self.stdin = None;
        let until = Instant::now() + STOP_GRACE;
        while Instant::now() < until {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(POLL);
        }
        kill_and_reap(&mut self.child);
    }
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    // After a kill the wait returns at once; it reaps the process (no zombie).
    let _ = child.wait();
}

/// The response to request `id` in one output line, if it is one.
fn response_to(line: &str, id: u64) -> Option<Map<String, Value>> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    (map.get("id").and_then(Value::as_u64) == Some(id)).then_some(map)
}

/// The sidecar's error code of a failed response (`None` when it succeeded).
fn error_code(response: &Map<String, Value>) -> Option<String> {
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    Some(
        response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
    )
}

fn number(response: &Map<String, Value>, key: &str) -> Option<f64> {
    response.get(key).and_then(Value::as_f64)
}

fn messages_json(messages: &[ChatMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|m| json!({"role": m.role.as_str(), "content": m.content}))
            .collect(),
    )
}

impl Refiner {
    /// `binary` is the sidecar (`bundled_sidecar`); `model_paths` finds each step's model file.
    pub fn new(binary: Option<PathBuf>, model_paths: ModelPaths) -> Refiner {
        Refiner {
            inner: Arc::new(Inner {
                binary,
                model_paths,
                origin: Instant::now(),
                clean: Sidecar::default(),
                translate: Sidecar::default(),
            }),
        }
    }

    fn sidecar(&self, step: RefineKind) -> &Sidecar {
        match step {
            RefineKind::Clean => &self.inner.clean,
            RefineKind::Translate => &self.inner.translate,
        }
    }

    fn now_ms(&self) -> Millis {
        self.inner.origin.elapsed().as_millis() as Millis
    }

    /// Waits for the step's process slot until `deadline` (or the cancel).
    fn lock(
        &self,
        step: RefineKind,
        deadline: Instant,
        cancel: Option<&CancelToken>,
    ) -> Result<MutexGuard<'_, Slot>, RefineError> {
        let slot = &self.sidecar(step).slot;
        loop {
            match slot.try_lock() {
                Ok(guard) => return Ok(guard),
                // A panic while holding the slot leaves at worst a dead process, which the next
                // call notices.
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(RefineError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(RefineError::Timeout);
            }
            thread::sleep(POLL);
        }
    }

    /// Drops the slot's process. `crashed`: it ended by itself, so the next start is a restart.
    fn drop_process(&self, step: RefineKind, slot: &mut Slot, crashed: bool) {
        if let Some(mut process) = slot.process.take() {
            if crashed {
                tracing::warn!(step = step.code(), "refine sidecar ended unexpectedly");
                process.kill();
            } else {
                process.stop();
            }
        }
        slot.warmed = None;
        slot.crashed |= crashed;
        self.sidecar(step).loaded.store(false, Ordering::Release);
    }

    /// Settles a failed call: a process that ended is dropped as a crash, a killed one as stopped.
    fn after_call_error(&self, step: RefineKind, slot: &mut Slot, error: CallError) -> RefineError {
        match error {
            CallError::Exited => self.drop_process(step, slot, true),
            CallError::Killed { .. } => self.drop_process(step, slot, false),
            CallError::Cancelled | CallError::Deadline => {}
        }
        error.into_refine_error()
    }

    /// Starts the step's sidecar and loads its model, unless it is running already.
    fn ensure_running(
        &self,
        step: RefineKind,
        slot: &mut Slot,
        cancel: Option<&CancelToken>,
    ) -> Result<(), RefineError> {
        if let Some(process) = slot.process.as_mut() {
            if process.alive() {
                return Ok(());
            }
            self.drop_process(step, slot, true);
        }
        let model = (self.inner.model_paths)(step).ok_or(RefineError::ModelMissing)?;
        let binary = self
            .inner
            .binary
            .as_deref()
            .ok_or_else(|| RefineError::Unavailable("sidecar missing".to_owned()))?;
        if slot.crashed {
            if !slot.restarts.try_restart(self.now_ms()) {
                return Err(RefineError::Unavailable(
                    "paused after repeated crashes".to_owned(),
                ));
            }
            slot.crashed = false;
        }
        let started = Instant::now();
        let mut process = Process::spawn(binary)?;
        // No greeting in the protocol: a cancel with no target answers as soon as it runs.
        let handshake = process.call(
            "cancel",
            Map::from_iter([("target".to_owned(), json!("start"))]),
            started + ms(refine::REFINER_START_TIMEOUT_MS),
            cancel,
        );
        let load = handshake.and_then(|_| {
            let fields = Map::from_iter([
                (
                    "model".to_owned(),
                    json!(model.to_string_lossy().into_owned()),
                ),
                ("ctx".to_owned(), json!(refine::context_tokens(step))),
            ]);
            process.call(
                "load",
                fields,
                Instant::now() + ms(refine::REFINER_LOAD_TIMEOUT_MS),
                cancel,
            )
        });
        slot.process = Some(process);
        let response = match load {
            Ok(response) => response,
            Err(error) => {
                // A start that does not get going counts as a crash only when the process
                // itself ended; a timeout or cancel just stops it.
                let crashed = matches!(error, CallError::Exited);
                self.drop_process(step, slot, crashed);
                return Err(match error {
                    CallError::Deadline | CallError::Killed { cancelled: false } => {
                        RefineError::Unavailable("start or load timed out".to_owned())
                    }
                    other => other.into_refine_error(),
                });
            }
        };
        if let Some(code) = error_code(&response) {
            tracing::warn!(step = step.code(), error = %code, "refine model load failed");
            self.drop_process(step, slot, false);
            return Err(RefineError::Unavailable(code));
        }
        let gpu = response
            .get("gpu_offload_supported")
            .and_then(Value::as_bool);
        tracing::info!(
            step = step.code(),
            start_ms = started.elapsed().as_millis() as u64,
            load_ms = number(&response, "load_ms"),
            gpu,
            "refine sidecar ready"
        );
        self.sidecar(step).loaded.store(true, Ordering::Release);
        Ok(())
    }

    /// Stops the processes that have not been used for `REFINER_IDLE_STOP_MS` (D3). A busy
    /// process is left alone.
    pub fn stop_idle(&self) {
        for step in [RefineKind::Clean, RefineKind::Translate] {
            let Ok(mut slot) = self.sidecar(step).slot.try_lock() else {
                continue;
            };
            if slot.process.is_some()
                && slot.last_used.elapsed() >= ms(refine::REFINER_IDLE_STOP_MS)
            {
                tracing::info!(step = step.code(), "stopping an idle refine sidecar");
                self.drop_process(step, &mut slot, false);
            }
        }
    }

    /// Stops both processes (the feature was switched off). Waits for a call in progress to end
    /// first; its own deadlines bound that wait.
    pub fn stop_all(&self) {
        self.stop_processes(SLOT_WAIT);
    }

    /// Stops both processes as the app quits. A call in progress was cancelled with its session;
    /// a process still busy after a short wait exits by itself once Murmur's end closes its input.
    pub fn shutdown(&self) {
        self.stop_processes(SHUTDOWN_WAIT);
    }

    fn stop_processes(&self, wait: Duration) {
        for step in [RefineKind::Clean, RefineKind::Translate] {
            match self.lock(step, Instant::now() + wait, None) {
                Ok(mut slot) => {
                    if slot.process.is_some() {
                        self.drop_process(step, &mut slot, false);
                    }
                }
                Err(error) => {
                    tracing::warn!(step = step.code(), %error, "could not stop a refine sidecar")
                }
            }
        }
    }
}

impl TextRefiner for Refiner {
    fn warm(&mut self, step: RefineKind, prefix: &[ChatMessage]) -> Result<(), RefineError> {
        let mut slot = self.lock(step, Instant::now() + SLOT_WAIT, None)?;
        slot.last_used = Instant::now();
        self.ensure_running(step, &mut slot, None)?;
        if slot.warmed.as_deref() == Some(prefix) {
            return Ok(());
        }
        let deadline = Instant::now() + ms(refine::REFINER_WARM_TIMEOUT_MS);
        let fields = Map::from_iter([("messages".to_owned(), messages_json(prefix))]);
        let result = match slot.process.as_mut() {
            Some(process) => process.call("warm", fields, deadline, None),
            None => Err(CallError::Exited),
        };
        slot.last_used = Instant::now();
        let response = result.map_err(|error| self.after_call_error(step, &mut slot, error))?;
        if let Some(code) = error_code(&response) {
            tracing::warn!(step = step.code(), error = %code, "refine warm-up failed");
            return Err(RefineError::Failed(code));
        }
        tracing::info!(
            step = step.code(),
            prompt_ms = number(&response, "prompt_ms"),
            prompt_tokens = number(&response, "prompt_tokens"),
            "refine prefix warmed"
        );
        slot.warmed = Some(prefix.to_vec());
        Ok(())
    }

    fn ready(&self, step: RefineKind) -> bool {
        self.sidecar(step).loaded.load(Ordering::Acquire)
    }

    fn generate(
        &mut self,
        step: RefineKind,
        request: &GenerateRequest,
        cancel: &CancelToken,
    ) -> Result<Generated, RefineError> {
        let mut slot = self.lock(step, Instant::now() + SLOT_WAIT, Some(cancel))?;
        slot.last_used = Instant::now();
        self.ensure_running(step, &mut slot, Some(cancel))?;
        // The budget starts once the model is ready.
        let deadline = Instant::now() + ms(request.budget_ms);
        let fields = Map::from_iter([
            ("messages".to_owned(), messages_json(&request.messages)),
            ("max_tokens".to_owned(), json!(request.max_tokens)),
        ]);
        let result = match slot.process.as_mut() {
            Some(process) => process.call("generate", fields, deadline, Some(cancel)),
            None => Err(CallError::Exited),
        };
        slot.last_used = Instant::now();
        let response = result.map_err(|error| self.after_call_error(step, &mut slot, error))?;
        if let Some(code) = error_code(&response) {
            tracing::info!(step = step.code(), error = %code, "refine generation failed");
            return Err(match code.as_str() {
                "cancelled" if cancel.is_cancelled() => RefineError::Cancelled,
                "cancelled" | "timeout" => RefineError::Timeout,
                "model_not_loaded" => {
                    self.drop_process(step, &mut slot, false);
                    RefineError::Unavailable(code)
                }
                _ => RefineError::Failed(code),
            });
        }
        let stop_reason = response.get("stop_reason").and_then(Value::as_str);
        tracing::info!(
            step = step.code(),
            prompt_ms = number(&response, "prompt_ms"),
            gen_ms = number(&response, "gen_ms"),
            prompt_tokens = number(&response, "prompt_tokens"),
            cached_tokens = number(&response, "cached_tokens"),
            generated_tokens = number(&response, "generated_tokens"),
            stop_reason,
            "refine generated"
        );
        let text = response
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| RefineError::Failed("no text".to_owned()))?;
        Ok(Generated {
            text: text.to_owned(),
            truncated: stop_reason == Some("max_tokens"),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use murmur_core::refine::Glossary;

    use super::*;

    /// A stand-in sidecar: a shell script that speaks the protocol. `mode` decides how it answers
    /// a generation: `ok`, `runaway` (stops at max_tokens), `slow` (answers only when
    /// cancelled), `hang` (ignores the cancel too), `crash` (exits). Every start and every warm-up
    /// is appended to `log`.
    fn fake_sidecar(name: &str, mode: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "murmur-refiner-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let script = dir.join("sidecar");
        let body = format!(
            r#"#!/bin/sh
MODE={mode}
LOG='{log}'
echo start >> "$LOG"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"op":"cancel"'*)
      printf '{{"id":%s,"ok":true,"error":null,"cancel_requested":false}}\n' "$id"
      target=$(printf '%s\n' "$line" | sed -n 's/.*"target":\([0-9][0-9]*\).*/\1/p')
      if [ -n "$target" ] && [ "$MODE" = slow ]; then
        printf '{{"id":%s,"ok":false,"error":"cancelled","total_ms":1}}\n' "$target"
      fi ;;
    *'"op":"load"'*)
      printf '{{"id":%s,"ok":true,"error":null,"load_ms":1,"ctx":2048,"gpu_offload_supported":true,"total_ms":1}}\n' "$id" ;;
    *'"op":"warm"'*)
      echo warm >> "$LOG"
      printf '{{"id":%s,"ok":true,"error":null,"prompt_ms":1,"prompt_tokens":9,"cached_tokens":9,"generated_tokens":0,"total_ms":1}}\n' "$id" ;;
    *'"op":"generate"'*)
      case "$MODE" in
        ok) printf '{{"id":%s,"ok":true,"error":null,"text":"The meeting is at four.","stop_reason":"eos","total_ms":1}}\n' "$id" ;;
        runaway) printf '{{"id":%s,"ok":true,"error":null,"text":"The the the","stop_reason":"max_tokens","total_ms":1}}\n' "$id" ;;
        crash) exit 3 ;;
        *) ;;
      esac ;;
  esac
done
"#,
            log = log.display()
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }

    fn count(log: &Path, what: &str) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == what)
            .count()
    }

    fn refiner(script: PathBuf) -> Refiner {
        // The fake never opens the model; any existing path will do.
        Refiner::new(Some(script), Box::new(|_| Some(PathBuf::from("/dev/null"))))
    }

    fn request(budget_ms: Millis) -> GenerateRequest {
        let (_, call) = refine::Run::start(
            RefineKind::Translate,
            "会議は4時からです。",
            Glossary::default(),
        );
        GenerateRequest {
            budget_ms,
            ..call.request
        }
    }

    #[test]
    fn warms_once_per_prefix_and_generates() {
        let (script, log) = fake_sidecar("ok", "ok");
        let mut r = refiner(script);
        let prefix = refine::prefix(RefineKind::Translate, &Glossary::default());
        assert!(!r.ready(RefineKind::Translate));
        r.warm(RefineKind::Translate, &prefix).unwrap();
        assert!(r.ready(RefineKind::Translate));
        assert!(!r.ready(RefineKind::Clean));
        r.warm(RefineKind::Translate, &prefix).unwrap();
        assert_eq!(count(&log, "warm"), 1, "the same prefix is warmed once");
        let answer = r
            .generate(RefineKind::Translate, &request(2_000), &CancelToken::new())
            .unwrap();
        assert_eq!(answer.text, "The meeting is at four.");
        assert!(!answer.truncated);
        assert_eq!(count(&log, "start"), 1);
        r.stop_all();
        assert!(!r.ready(RefineKind::Translate));
    }

    #[test]
    fn an_answer_cut_at_max_tokens_is_marked_truncated() {
        let (script, _) = fake_sidecar("runaway", "runaway");
        let answer = refiner(script)
            .generate(RefineKind::Translate, &request(2_000), &CancelToken::new())
            .unwrap();
        assert!(answer.truncated);
    }

    #[test]
    fn the_budget_and_the_cancel_stop_a_generation_and_keep_the_process() {
        let (script, log) = fake_sidecar("slow", "slow");
        let mut r = refiner(script);
        let started = Instant::now();
        assert_eq!(
            r.generate(RefineKind::Translate, &request(150), &CancelToken::new()),
            Err(RefineError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let cancel = CancelToken::new();
        let later = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            later.cancel();
        });
        assert_eq!(
            r.generate(RefineKind::Translate, &request(5_000), &cancel),
            Err(RefineError::Cancelled)
        );
        canceller.join().unwrap();
        // The sidecar answered both cancels: still the first process.
        assert!(r.ready(RefineKind::Translate));
        assert_eq!(count(&log, "start"), 1);
        r.stop_all();
    }

    #[test]
    fn a_sidecar_that_ignores_the_cancel_is_killed_and_started_again_freely() {
        let (script, log) = fake_sidecar("hang", "hang");
        let mut r = refiner(script);
        // One more kill than the restart budget allows after crashes.
        for _ in 0..=refine::REFINER_MAX_RESTARTS {
            let started = Instant::now();
            assert_eq!(
                r.generate(RefineKind::Translate, &request(100), &CancelToken::new()),
                Err(RefineError::Timeout)
            );
            // Budget plus the cancel reply time, with room for a slow machine.
            assert!(started.elapsed() < Duration::from_secs(3));
            assert!(!r.ready(RefineKind::Translate));
        }
        // A kill is not a crash: no restart budget is used.
        assert_eq!(count(&log, "start"), refine::REFINER_MAX_RESTARTS + 1);
    }

    #[test]
    fn crashes_get_three_restarts_then_the_step_pauses() {
        let (script, log) = fake_sidecar("crash", "crash");
        let mut r = refiner(script);
        // The first start and three restarts each crash on the generation.
        for _ in 0..4 {
            assert!(matches!(
                r.generate(RefineKind::Translate, &request(2_000), &CancelToken::new()),
                Err(RefineError::Unavailable(_))
            ));
        }
        assert_eq!(count(&log, "start"), 4);
        // The fourth crash is refused a restart.
        assert_eq!(
            r.generate(RefineKind::Translate, &request(2_000), &CancelToken::new()),
            Err(RefineError::Unavailable(
                "paused after repeated crashes".to_owned()
            ))
        );
        assert_eq!(count(&log, "start"), 4);
        // The other step has its own budget.
        assert!(matches!(
            r.generate(RefineKind::Clean, &request(2_000), &CancelToken::new()),
            Err(RefineError::Unavailable(_))
        ));
        assert_eq!(count(&log, "start"), 5);
    }

    #[test]
    fn a_missing_model_or_sidecar_fails_without_starting() {
        let (script, log) = fake_sidecar("missing", "ok");
        let mut r = Refiner::new(Some(script), Box::new(|_| None));
        assert_eq!(
            r.generate(RefineKind::Translate, &request(2_000), &CancelToken::new()),
            Err(RefineError::ModelMissing)
        );
        assert_eq!(count(&log, "start"), 0);
        let mut r = Refiner::new(None, Box::new(|_| Some(PathBuf::from("/dev/null"))));
        assert!(matches!(
            r.warm(RefineKind::Translate, &[]),
            Err(RefineError::Unavailable(_))
        ));
    }

    /// The real sidecar and models: the staged binary (`scripts/build-sidecar.sh`, or
    /// `MURMUR_LLM_BIN`) and the GGUF files in `$MURMUR_TEST_MODELS/llm` (default
    /// `~/.murmur-dev-models/llm`). Run: `cargo test -p murmur refiner -- --ignored`.
    #[test]
    #[ignore = "needs the staged sidecar and the LLM models"]
    fn translates_with_the_real_sidecar() {
        use crate::refine_session;
        use murmur_core::catalog;
        use murmur_core::refine::PreClean;

        let binary = std::env::var("MURMUR_LLM_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "binaries/murmur-llm-{}-apple-darwin",
                    std::env::consts::ARCH
                ))
            });
        assert!(binary.is_file(), "sidecar missing: {}", binary.display());
        let models = std::env::var("MURMUR_TEST_MODELS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap()).join(".murmur-dev-models")
            })
            .join("llm");
        let mut r = Refiner::new(
            Some(binary),
            Box::new(move |step| Some(models.join(catalog::refine_model(step).file))),
        );
        let glossary = Glossary::default();
        refine_session::warm(RefineKind::Translate, &glossary, &mut r);
        assert!(r.ready(RefineKind::Translate) && r.ready(RefineKind::Clean));
        let mut loading = 0;
        for (text, cleaned_first) in [
            ("明日の会議は午後4時からです。", false),
            ("会議は3時から、いや4時からでお願いします。", true),
        ] {
            let started = Instant::now();
            let report = refine_session::drive(
                RefineKind::Translate,
                text,
                glossary.clone(),
                &mut r,
                &CancelToken::new(),
                &mut || loading += 1,
            );
            let english = report.result.expect("translation");
            assert!(
                english.0.is_ascii(),
                "not English: {} chars",
                english.0.len()
            );
            assert_eq!(report.pre_clean != PreClean::NotNeeded, cleaned_first);
            eprintln!(
                "{} chars, steps {:?}, {} ms",
                text.chars().count(),
                report.steps,
                started.elapsed().as_millis()
            );
        }
        assert_eq!(loading, 0, "both models were warm");
        r.stop_all();
        assert!(!r.ready(RefineKind::Translate));
    }
}
