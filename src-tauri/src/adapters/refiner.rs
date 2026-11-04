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
//! the reply to a cancel, after which the process is killed. Requests go through a writer thread,
//! so a sidecar that stops reading cannot hold a caller past its deadline. A process that ends by
//! itself is a crash; `refine::RestartBudget` decides whether it may start again. Killed and
//! dropped processes are reaped within a short wait, or later on a reaper thread. No prompt or
//! answer text reaches the log (G7): only ids, error codes, token counts and times.
//!
//! Lifecycle: `Refiner::handle` gives a `RefinerHandle` (the `ports::TextRefiner`) bound to the
//! current lifecycle of each model. `stop(step)` retires only that model; `stop_all` and
//! `shutdown` retire both. Old handles cannot restart a retired model, and its calls in progress
//! stop promptly. The other model remains usable, so translation and cleanup can share cleanup's
//! process. Later recordings take fresh lifecycle tokens.
//!
//! Only transport lives here. Prompts, validation and the order of the steps are in
//! `murmur_core::refine`; `crate::refine_session` makes the calls.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use murmur_core::Millis;
use murmur_core::ports::{CancelToken, GenerateRequest, Generated, RefineError, TextRefiner};
use murmur_core::refine::{self, ChatMessage, RefineKind, RestartBudget};
use serde_json::{Map, Value, json};

/// The sidecar's file name next to the app executable (Tauri `bundle.externalBin`).
pub const SIDECAR_NAME: &str = "murmur-llm";

/// How often a wait looks at the cancel tokens.
const POLL: Duration = Duration::from_millis(20);
/// A stopped sidecar exits within 0.5 s of the end of its input; then it is killed.
const STOP_GRACE: Duration = Duration::from_millis(1_000);
/// How long a kill waits to reap the process before a reaper thread takes over.
const REAP_WAIT: Duration = Duration::from_millis(200);
/// How long a reaper thread keeps trying before it gives up (and logs).
const REAPER_LIMIT: Duration = Duration::from_secs(30);
/// The longest a call waits for the process slot that another call holds: a warm-up that has
/// to start the process and load the model holds it that long at most.
const SLOT_WAIT: Duration = Duration::from_millis(
    refine::REFINER_START_TIMEOUT_MS
        + refine::REFINER_LOAD_TIMEOUT_MS
        + refine::REFINER_WARM_TIMEOUT_MS,
);
/// How long `stop_all` waits for a busy slot. The ended lifecycle stops its calls within a cancel
/// reply and a reap, so this is a safety margin; past it the process is killed directly.
const STOP_WAIT: Duration = Duration::from_millis(3_000);
/// The same for the exit.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(1_500);
/// The longest output line accepted. A generation is capped at 8192 tokens, far below this; a
/// longer line means the process is broken, and it is stopped.
const MAX_LINE_BYTES: usize = 1_048_576;
/// Output lines waiting for the caller; the reader waits when they are not taken.
const LINE_QUEUE: usize = 64;
/// The error a call gets when its lifecycle ended (`stop_all`, `shutdown`).
const STOPPED: &str = "stopped";

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

fn lock_ignoring_poison<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The refine models: their processes, lifecycle and status. Cheap to clone; all clones share the
/// processes. Calls go through a `RefinerHandle`.
#[derive(Clone)]
pub struct Refiner {
    inner: Arc<Inner>,
}

/// The `ports::TextRefiner` of one lifecycle (see the module comment).
#[derive(Clone)]
pub struct RefinerHandle {
    inner: Arc<Inner>,
    clean_lifecycle: CancelToken,
    translate_lifecycle: CancelToken,
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
    /// Each model has its own lifecycle: stopping translation must not end shared cleanup calls.
    lifecycle: Mutex<CancelToken>,
    /// The process runs with its model loaded. Read without the slot (`ready`).
    loaded: AtomicBool,
    /// Why the last start or model load failed, until a start succeeds (`status`).
    last_error: Mutex<Option<String>>,
    /// The running process, reachable without the slot, so a stop can kill it while a call
    /// holds the slot.
    child: Mutex<Option<SharedChild>>,
}

/// What the settings show about the sidecars: the last failure to start a step's sidecar or load
/// its model, until a start succeeds. A missing model is not listed (the Models screen shows it).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, specta::Type)]
pub struct RefinerStatus {
    pub translate_error: Option<String>,
    pub clean_error: Option<String>,
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

/// Why a call stopped waiting for its response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// The session's cancel token.
    Cancelled,
    /// The call's deadline.
    Deadline,
    /// The lifecycle ended.
    Stopped,
}

/// Why a call ended without its response.
#[derive(Debug, PartialEq, Eq)]
enum CallError {
    /// The process ended (or its pipes broke).
    Exited,
    /// The sidecar reported an error that no request id carries (`worker_start_failed`, or
    /// `invalid_request` for a line it could not read). Backend and Metal failures are not this:
    /// they come back as the load request's own error.
    Fatal(String),
    /// The sidecar answered the cancel; the process is still usable.
    Interrupted(Why),
    /// The sidecar did not answer the cancel in time and was killed.
    Killed(Why),
}

impl CallError {
    fn into_refine_error(self) -> RefineError {
        match self {
            CallError::Exited => RefineError::Unavailable("sidecar_exited".to_owned()),
            CallError::Fatal(code) => RefineError::Unavailable(code),
            CallError::Interrupted(why) | CallError::Killed(why) => match why {
                Why::Cancelled => RefineError::Cancelled,
                Why::Deadline => RefineError::Timeout,
                Why::Stopped => RefineError::Unavailable(STOPPED.to_owned()),
            },
        }
    }
}

// --- the child process ---

/// What reaping needs from a child process; a test double injects kill and wait failures.
trait Reapable: Send {
    fn kill(&mut self) -> std::io::Result<()>;
    /// Whether the exit status has been collected (the process is gone, not a zombie).
    fn try_wait(&mut self) -> std::io::Result<bool>;
}

impl Reapable for Child {
    fn kill(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }

    fn try_wait(&mut self) -> std::io::Result<bool> {
        Child::try_wait(self).map(|status| status.is_some())
    }
}

/// A child process shared by its owner and the stop path. The lock is held only for a kill or a
/// non-blocking wait.
type SharedChild = Arc<Mutex<Box<dyn Reapable>>>;

/// Polls the child until its exit status is collected, at most `wait`.
fn poll_reaped(child: &SharedChild, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    loop {
        match lock_ignoring_poison(child).try_wait() {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => {
                // No such child any more: nothing is left to reap.
                tracing::warn!(kind = ?error.kind(), "waiting for a refine sidecar failed");
                return true;
            }
        }
        let now = Instant::now();
        if now >= until {
            return false;
        }
        thread::sleep(POLL.min(until - now));
    }
}

/// Reaps the child on its own thread, so no caller waits for it.
fn reap_later(child: SharedChild, grace: Duration) {
    let spawned = thread::Builder::new()
        .name("murmur-llm-reaper".to_owned())
        .spawn(move || {
            if poll_reaped(&child, grace) {
                return;
            }
            if let Err(error) = lock_ignoring_poison(&child).kill() {
                tracing::warn!(kind = ?error.kind(), "could not kill a refine sidecar");
            }
            if !poll_reaped(&child, REAPER_LIMIT) {
                tracing::warn!("a refine sidecar could not be reaped");
            }
        });
    if spawned.is_err() {
        tracing::warn!("no reaper thread for a refine sidecar");
    }
}

/// Kills the child and reaps it within `wait`; past that a reaper thread finishes the job.
/// Returns whether it was reaped here.
fn kill_and_reap(child: &SharedChild, wait: Duration) -> bool {
    if let Err(error) = lock_ignoring_poison(child).kill() {
        // Also when it has exited already; the wait below tells.
        tracing::info!(kind = ?error.kind(), "kill of a refine sidecar failed");
    }
    if poll_reaped(child, wait) {
        return true;
    }
    reap_later(Arc::clone(child), Duration::ZERO);
    false
}

/// How reading one output line ended.
#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    Line,
    Eof,
    TooLong,
}

/// Reads one line without its newline into `buf`, at most `max` bytes.
fn read_line_bounded(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<LineRead> {
    buf.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line
            });
        }
        let (take, end) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (i, true),
            None => (available.len(), false),
        };
        if buf.len() + take > max {
            return Ok(LineRead::TooLong);
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(if end { take + 1 } else { take });
        if end {
            return Ok(LineRead::Line);
        }
    }
}

/// One running sidecar.
struct Process {
    child: SharedChild,
    pid: u32,
    /// Request lines for the writer thread; `None` once the input is closed.
    writer: Option<Sender<Vec<u8>>>,
    lines: Receiver<String>,
    next_id: u64,
    /// Stopped or killed and reaped (or handed to a reaper): nothing left for `Drop`.
    done: bool,
}

/// How an output line relates to a request.
enum Reply {
    /// The response to the request.
    Response(Map<String, Value>),
    /// A fatal error without a request id.
    Fatal(String),
    Other,
}

impl Process {
    fn spawn(binary: &Path) -> Result<Process, RefineError> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                tracing::warn!(kind = ?e.kind(), "could not start the refine sidecar");
                RefineError::Unavailable("spawn_failed".to_owned())
            })?;
        let pid = child.id();
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let shared: SharedChild = Arc::new(Mutex::new(Box::new(child)));
        let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
            kill_and_reap(&shared, REAP_WAIT);
            return Err(RefineError::Unavailable("no_pipes".to_owned()));
        };
        let (line_tx, lines) = mpsc::sync_channel(LINE_QUEUE);
        let reader = thread::Builder::new()
            .name("murmur-llm-reader".to_owned())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    match read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES) {
                        Ok(LineRead::Line) => {
                            // A line that is not UTF-8 is not a response; skip it.
                            let Ok(line) = String::from_utf8(std::mem::take(&mut buf)) else {
                                continue;
                            };
                            if line_tx.send(line).is_err() {
                                break;
                            }
                        }
                        Ok(LineRead::TooLong) => {
                            tracing::warn!("refine sidecar wrote an overlong line");
                            break;
                        }
                        Ok(LineRead::Eof) | Err(_) => break,
                    }
                }
            });
        let (writer, requests) = mpsc::channel::<Vec<u8>>();
        let writer_thread = thread::Builder::new()
            .name("murmur-llm-writer".to_owned())
            .spawn(move || {
                // Ends when the owner closes the input, or when the pipe breaks (the process
                // ended or was killed); dropping stdin then gives the sidecar its end of input.
                for line in requests {
                    if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                        break;
                    }
                }
            });
        if reader.is_err() || writer_thread.is_err() {
            kill_and_reap(&shared, REAP_WAIT);
            return Err(RefineError::Unavailable("no_pipe_threads".to_owned()));
        }
        Ok(Process {
            child: shared,
            pid,
            writer: Some(writer),
            lines,
            next_id: 1,
            done: false,
        })
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Hands one request line to the writer thread. Never blocks.
    fn send(&mut self, body: &Value) -> Result<(), CallError> {
        let writer = self.writer.as_ref().ok_or(CallError::Exited)?;
        let mut line = body.to_string().into_bytes();
        line.push(b'\n');
        writer.send(line).map_err(|_| CallError::Exited)
    }

    fn reply(line: &str, id: u64) -> Reply {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) else {
            return Reply::Other;
        };
        match map.get("id") {
            Some(Value::Null) if error_code(&map).is_some() => {
                Reply::Fatal(error_code(&map).unwrap_or_default())
            }
            Some(value) if value.as_u64() == Some(id) => Reply::Response(map),
            _ => Reply::Other,
        }
    }

    /// Sends `op` with `fields` and waits for its response until `deadline`, the session's cancel
    /// or the end of the lifecycle. Each of those cancels the request on the sidecar.
    fn call(
        &mut self,
        op: &str,
        fields: Map<String, Value>,
        deadline: Instant,
        session: Option<&CancelToken>,
        lifecycle: &CancelToken,
    ) -> Result<Map<String, Value>, CallError> {
        let id = self.take_id();
        let mut body = fields;
        body.insert("id".to_owned(), json!(id));
        body.insert("op".to_owned(), json!(op));
        self.send(&Value::Object(body))?;
        loop {
            let now = Instant::now();
            let why = if session.is_some_and(CancelToken::is_cancelled) {
                Some(Why::Cancelled)
            } else if lifecycle.is_cancelled() {
                Some(Why::Stopped)
            } else if now >= deadline {
                Some(Why::Deadline)
            } else {
                None
            };
            if let Some(why) = why {
                return Err(self.abort(id, why));
            }
            match self.lines.recv_timeout((deadline - now).min(POLL)) {
                Ok(line) => match Self::reply(&line, id) {
                    Reply::Response(response) => return Ok(response),
                    Reply::Fatal(code) => return Err(CallError::Fatal(code)),
                    Reply::Other => {}
                },
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(CallError::Exited),
            }
        }
    }

    /// Cancels request `id` and waits for its response, `REFINER_CANCEL_REPLY_MS` in all
    /// (sending included); a sidecar that does not answer is killed.
    fn abort(&mut self, id: u64, why: Why) -> CallError {
        let until = Instant::now() + ms(refine::REFINER_CANCEL_REPLY_MS);
        let cancel_id = self.take_id();
        let answered = self
            .send(&json!({"id": cancel_id, "op": "cancel", "target": id}))
            .is_ok()
            && loop {
                let now = Instant::now();
                if now >= until {
                    break false;
                }
                match self.lines.recv_timeout(until - now) {
                    Ok(line) => match Self::reply(&line, id) {
                        Reply::Response(_) => break true,
                        Reply::Fatal(_) => break false,
                        Reply::Other => {}
                    },
                    Err(_) => break false,
                }
            };
        if answered {
            CallError::Interrupted(why)
        } else {
            self.kill();
            CallError::Killed(why)
        }
    }

    fn alive(&self) -> bool {
        matches!(lock_ignoring_poison(&self.child).try_wait(), Ok(false))
    }

    /// Kills the process now; it is reaped within `REAP_WAIT` or by a reaper thread.
    fn kill(&mut self) {
        self.writer = None;
        kill_and_reap(&self.child, REAP_WAIT);
        self.done = true;
    }

    /// Closes the input (the sidecar cancels its work and exits), waits `STOP_GRACE` for the
    /// exit, then kills it.
    fn stop(mut self) {
        if self.done {
            return;
        }
        self.writer = None;
        if !poll_reaped(&self.child, STOP_GRACE) {
            kill_and_reap(&self.child, REAP_WAIT);
        }
        self.done = true;
    }
}

impl Drop for Process {
    /// A process dropped without `stop` or `kill` (its last owner went away, an early return, a
    /// panic): the input closes, and a reaper thread gives it the stop grace, then kills and reaps
    /// it. Never blocks.
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.writer = None;
        reap_later(Arc::clone(&self.child), STOP_GRACE);
    }
}

/// The sidecar's error code of a failed response (`None` when it succeeded). Only plain codes are
/// kept; anything else becomes `unknown` (the log never carries free text).
fn error_code(response: &Map<String, Value>) -> Option<String> {
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let code = response.get("error").and_then(Value::as_str);
    Some(match code {
        Some(code) if is_plain_code(code) => code.to_owned(),
        _ => "unknown".to_owned(),
    })
}

fn is_plain_code(code: &str) -> bool {
    (1..=48).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
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

// --- the service ---

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

    /// A `TextRefiner` for the current lifecycle. Take one per recording.
    pub fn handle(&self) -> RefinerHandle {
        RefinerHandle {
            inner: Arc::clone(&self.inner),
            clean_lifecycle: lock_ignoring_poison(&self.inner.clean.lifecycle).clone(),
            translate_lifecycle: lock_ignoring_poison(&self.inner.translate.lifecycle).clone(),
        }
    }

    /// The last start failures of both steps.
    pub fn status(&self) -> RefinerStatus {
        let error = |step| lock_ignoring_poison(&self.inner.sidecar(step).last_error).clone();
        RefinerStatus {
            translate_error: error(RefineKind::Translate),
            clean_error: error(RefineKind::Clean),
        }
    }

    /// Stops the processes that have not been used for `REFINER_IDLE_STOP_MS` (D3). A busy
    /// process is left alone.
    pub fn stop_idle(&self) {
        for step in [RefineKind::Clean, RefineKind::Translate] {
            let Ok(mut slot) = self.inner.sidecar(step).slot.try_lock() else {
                continue;
            };
            if slot.process.is_some()
                && slot.last_used.elapsed() >= ms(refine::REFINER_IDLE_STOP_MS)
            {
                tracing::info!(step = step.code(), "stopping an idle refine sidecar");
                self.inner.drop_process(step, &mut slot, false);
            }
        }
    }

    /// Ends both model lifecycles and stops both processes (for example, on model deletion).
    pub fn stop_all(&self) {
        self.stop_processes(&[RefineKind::Clean, RefineKind::Translate], STOP_WAIT);
    }

    /// The same as the app quits, with a shorter wait. A process left busy also exits by itself
    /// once Murmur's end closes its input.
    pub fn shutdown(&self) {
        self.stop_processes(&[RefineKind::Clean, RefineKind::Translate], SHUTDOWN_WAIT);
    }

    /// Stops just one model, without invalidating the other model's handles or calls.
    pub fn stop(&self, step: RefineKind) {
        self.stop_processes(&[step], STOP_WAIT);
    }

    fn stop_processes(&self, steps: &[RefineKind], wait: Duration) {
        // Invalidate every affected handle before waiting for any busy process slot.
        for &step in steps {
            let ended = std::mem::take(&mut *lock_ignoring_poison(
                &self.inner.sidecar(step).lifecycle,
            ));
            ended.cancel();
        }
        let deadline = Instant::now() + wait;
        for &step in steps {
            let sidecar = self.inner.sidecar(step);
            match lock_slot(&sidecar.slot, deadline, None, None) {
                Ok(mut slot) => {
                    if slot.process.is_some() {
                        self.inner.drop_process(step, &mut slot, false);
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        step = step.code(),
                        "a refine sidecar stayed busy; killing it"
                    );
                    if let Some(child) = lock_ignoring_poison(&sidecar.child).clone() {
                        kill_and_reap(&child, REAP_WAIT);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn pid(&self, step: RefineKind) -> Option<u32> {
        lock_ignoring_poison(&self.inner.sidecar(step).slot)
            .process
            .as_ref()
            .map(|p| p.pid)
    }
}

/// Waits for a process slot until `deadline`, the session's cancel or the end of the lifecycle.
fn lock_slot<'a>(
    slot: &'a Mutex<Slot>,
    deadline: Instant,
    session: Option<&CancelToken>,
    lifecycle: Option<&CancelToken>,
) -> Result<MutexGuard<'a, Slot>, RefineError> {
    loop {
        match slot.try_lock() {
            Ok(guard) => return Ok(guard),
            // A panic while holding the slot leaves at worst a dead process, which the next call
            // notices.
            Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        if session.is_some_and(CancelToken::is_cancelled) {
            return Err(RefineError::Cancelled);
        }
        if lifecycle.is_some_and(CancelToken::is_cancelled) {
            return Err(RefineError::Unavailable(STOPPED.to_owned()));
        }
        if Instant::now() >= deadline {
            return Err(RefineError::Timeout);
        }
        thread::sleep(POLL);
    }
}

impl Inner {
    fn sidecar(&self, step: RefineKind) -> &Sidecar {
        match step {
            RefineKind::Clean => &self.clean,
            RefineKind::Translate => &self.translate,
        }
    }

    fn now_ms(&self) -> Millis {
        self.origin.elapsed().as_millis() as Millis
    }

    /// Drops the slot's process. `crashed`: it ended by itself, so the next start is a restart.
    fn drop_process(&self, step: RefineKind, slot: &mut Slot, crashed: bool) {
        let sidecar = self.sidecar(step);
        *lock_ignoring_poison(&sidecar.child) = None;
        if let Some(mut process) = slot.process.take() {
            if crashed {
                tracing::warn!(
                    step = step.code(),
                    pid = process.pid,
                    "refine sidecar ended unexpectedly"
                );
                process.kill();
            } else {
                process.stop();
            }
        }
        slot.warmed = None;
        slot.crashed |= crashed;
        sidecar.loaded.store(false, Ordering::Release);
    }
}

// --- the calls of one lifecycle ---

impl RefinerHandle {
    fn lifecycle(&self, step: RefineKind) -> &CancelToken {
        match step {
            RefineKind::Clean => &self.clean_lifecycle,
            RefineKind::Translate => &self.translate_lifecycle,
        }
    }

    fn sidecar(&self, step: RefineKind) -> &Sidecar {
        self.inner.sidecar(step)
    }

    /// Takes the step's slot for a call of this lifecycle.
    fn slot(
        &self,
        step: RefineKind,
        session: Option<&CancelToken>,
    ) -> Result<MutexGuard<'_, Slot>, RefineError> {
        let slot = lock_slot(
            &self.sidecar(step).slot,
            Instant::now() + SLOT_WAIT,
            session,
            Some(self.lifecycle(step)),
        )?;
        // Checked under the slot: a stop sweep ends the lifecycle before it takes any slot, so
        // nothing of an ended lifecycle starts after the sweep has passed.
        if self.lifecycle(step).is_cancelled() {
            return Err(RefineError::Unavailable(STOPPED.to_owned()));
        }
        Ok(slot)
    }

    /// Settles a failed call: a process that ended by itself is dropped as a crash; a killed one,
    /// one that reported a fatal error, or one that ended after its lifecycle, as stopped.
    fn after_call_error(&self, step: RefineKind, slot: &mut Slot, error: CallError) -> RefineError {
        match &error {
            CallError::Exited => {
                let crashed = !self.lifecycle(step).is_cancelled();
                self.inner.drop_process(step, slot, crashed);
            }
            CallError::Fatal(_) | CallError::Killed(_) => {
                self.inner.drop_process(step, slot, false)
            }
            CallError::Interrupted(_) => {}
        }
        error.into_refine_error()
    }

    /// Starts the step's sidecar and loads its model, unless it is running already, and keeps the
    /// outcome for `status`.
    fn ensure_running(
        &self,
        step: RefineKind,
        slot: &mut Slot,
        session: Option<&CancelToken>,
    ) -> Result<(), RefineError> {
        let result = self.start_if_needed(step, slot, session);
        let error = match &result {
            Err(RefineError::Unavailable(code)) if code != STOPPED => Some(code.clone()),
            Ok(()) | Err(RefineError::ModelMissing) => None,
            // A cancelled or stopped call says nothing about the sidecar.
            Err(_) => return result,
        };
        *lock_ignoring_poison(&self.sidecar(step).last_error) = error;
        result
    }

    fn start_if_needed(
        &self,
        step: RefineKind,
        slot: &mut Slot,
        session: Option<&CancelToken>,
    ) -> Result<(), RefineError> {
        if let Some(process) = slot.process.as_ref() {
            if process.alive() {
                return Ok(());
            }
            self.inner.drop_process(step, slot, true);
        }
        let model = (self.inner.model_paths)(step).ok_or(RefineError::ModelMissing)?;
        let binary = self
            .inner
            .binary
            .as_deref()
            .ok_or_else(|| RefineError::Unavailable("sidecar_missing".to_owned()))?;
        if slot.crashed {
            if !slot.restarts.try_restart(self.inner.now_ms()) {
                return Err(RefineError::Unavailable("paused_after_crashes".to_owned()));
            }
            slot.crashed = false;
        }
        let started = Instant::now();
        let mut process = Process::spawn(binary)?;
        *lock_ignoring_poison(&self.sidecar(step).child) = Some(Arc::clone(&process.child));
        // No greeting in the protocol: a cancel with no target answers as soon as it runs.
        let handshake = process
            .call(
                "cancel",
                Map::from_iter([("target".to_owned(), json!("start"))]),
                started + ms(refine::REFINER_START_TIMEOUT_MS),
                session,
                self.lifecycle(step),
            )
            .and_then(|response| match error_code(&response) {
                Some(code) => Err(CallError::Fatal(code)),
                None => Ok(()),
            });
        let load = handshake.and_then(|()| {
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
                session,
                self.lifecycle(step),
            )
        });
        slot.process = Some(process);
        let response = match load {
            Ok(response) => response,
            Err(error) => {
                // A start that does not get going counts as a crash only when the process
                // itself ended; a timeout, a cancel, a fatal report or a stop just stops it.
                let crashed = error == CallError::Exited && !self.lifecycle(step).is_cancelled();
                self.inner.drop_process(step, slot, crashed);
                return Err(match error {
                    CallError::Interrupted(Why::Deadline) | CallError::Killed(Why::Deadline) => {
                        RefineError::Unavailable("start_timeout".to_owned())
                    }
                    other => other.into_refine_error(),
                });
            }
        };
        if let Some(code) = error_code(&response) {
            tracing::warn!(step = step.code(), error = %code, "refine model load failed");
            self.inner.drop_process(step, slot, false);
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
}

impl TextRefiner for RefinerHandle {
    fn warm(&mut self, step: RefineKind, prefix: &[ChatMessage]) -> Result<(), RefineError> {
        let mut slot = self.slot(step, None)?;
        slot.last_used = Instant::now();
        self.ensure_running(step, &mut slot, None)?;
        if slot.warmed.as_deref() == Some(prefix) {
            return Ok(());
        }
        // The sidecar drops its old snapshot when a warm-up starts, whatever the outcome.
        slot.warmed = None;
        let deadline = Instant::now() + ms(refine::REFINER_WARM_TIMEOUT_MS);
        let fields = Map::from_iter([("messages".to_owned(), messages_json(prefix))]);
        let result = match slot.process.as_mut() {
            Some(process) => process.call("warm", fields, deadline, None, self.lifecycle(step)),
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
        let mut slot = self.slot(step, Some(cancel))?;
        slot.last_used = Instant::now();
        self.ensure_running(step, &mut slot, Some(cancel))?;
        // The budget starts once the model is ready.
        let deadline = Instant::now() + ms(request.budget_ms);
        let fields = Map::from_iter([
            ("messages".to_owned(), messages_json(&request.messages)),
            ("max_tokens".to_owned(), json!(request.max_tokens)),
        ]);
        let result = match slot.process.as_mut() {
            Some(process) => process.call(
                "generate",
                fields,
                deadline,
                Some(cancel),
                self.lifecycle(step),
            ),
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
                    self.inner.drop_process(step, &mut slot, false);
                    RefineError::Unavailable(code)
                }
                _ => RefineError::Failed(code),
            });
        }
        // Only the two stop reasons of the protocol: anything else could be an incomplete answer
        // passed off as complete.
        let truncated = match response.get("stop_reason").and_then(Value::as_str) {
            Some("eos") => false,
            Some("max_tokens") => true,
            _ => {
                tracing::warn!(
                    step = step.code(),
                    "refine answer without a known stop reason"
                );
                return Err(RefineError::Failed("bad_stop_reason".to_owned()));
            }
        };
        tracing::info!(
            step = step.code(),
            prompt_ms = number(&response, "prompt_ms"),
            gen_ms = number(&response, "gen_ms"),
            prompt_tokens = number(&response, "prompt_tokens"),
            cached_tokens = number(&response, "cached_tokens"),
            generated_tokens = number(&response, "generated_tokens"),
            truncated,
            "refine generated"
        );
        let text = response
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| RefineError::Failed("no_text".to_owned()))?;
        Ok(Generated {
            text: text.to_owned(),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;

    use murmur_core::refine::Glossary;

    use super::*;

    /// A stand-in sidecar: a shell script that speaks the protocol. `mode` decides how it answers:
    /// `ok`, `runaway` (stops at max_tokens), `nostop` / `badstop` (no or an unknown stop reason),
    /// `slow` (answers a generation only when cancelled), `hang` (ignores the cancel too),
    /// `crash` (exits on a generation), `deaf` (stops reading its input after a warm-up),
    /// `initfail` (reports `worker_start_failed` without an id at startup, then exits), `nometal`
    /// (the load fails with its id and `metal_library_missing`), `slowload` (the load waits for
    /// `<log>.go`, reading nothing meanwhile). Every start (with a `pid` line), load, warm-up and
    /// generation is appended to `log`; while `<log>.fail` exists the load fails, while
    /// `<log>.warmfail` exists the warm-up fails.
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
echo "pid $$" >> "$LOG"
if [ "$MODE" = initfail ]; then
  printf '{{"id":null,"ok":false,"error":"worker_start_failed"}}\n'
  exit 0
fi
# Sets n to the number after a key, without starting a process per line.
number() {{
  n=
  case "$1" in
    *"\"$2\":"[0-9]*) n=${{1#*\"$2\":}}; n=${{n%%[!0-9]*}} ;;
  esac
}}
while IFS= read -r line; do
  number "$line" id; id=$n
  case "$line" in
    *'"op":"cancel"'*)
      printf '{{"id":%s,"ok":true,"error":null,"cancel_requested":false}}\n' "$id"
      number "$line" target; target=$n
      if [ -n "$target" ] && [ "$MODE" = slow ]; then
        printf '{{"id":%s,"ok":false,"error":"cancelled","total_ms":1}}\n' "$target"
      fi ;;
    *'"op":"load"'*)
      echo load >> "$LOG"
      if [ "$MODE" = nometal ]; then
        printf '{{"id":%s,"ok":false,"error":"metal_library_missing","total_ms":1}}\n' "$id"
        continue
      fi
      if [ "$MODE" = slowload ]; then
        while [ ! -f "$LOG.go" ]; do sleep 0.05; done
      fi
      if [ -f "$LOG.fail" ]; then
        printf '{{"id":%s,"ok":false,"error":"model_load_failed","total_ms":1}}\n' "$id"
      else
        printf '{{"id":%s,"ok":true,"error":null,"load_ms":1,"ctx":2048,"gpu_offload_supported":true,"total_ms":1}}\n' "$id"
      fi
      ;;
    *'"op":"warm"'*)
      echo warm >> "$LOG"
      if [ -f "$LOG.warmfail" ]; then
        printf '{{"id":%s,"ok":false,"error":"decode_failed","total_ms":1}}\n' "$id"
      else
        printf '{{"id":%s,"ok":true,"error":null,"prompt_ms":1,"prompt_tokens":9,"cached_tokens":9,"generated_tokens":0,"total_ms":1}}\n' "$id"
      fi
      if [ "$MODE" = deaf ]; then
        exec sleep 30
      fi ;;
    *'"op":"generate"'*)
      echo generate >> "$LOG"
      case "$MODE" in
        ok) printf '{{"id":%s,"ok":true,"error":null,"text":"The meeting is at four.","stop_reason":"eos","total_ms":1}}\n' "$id" ;;
        runaway) printf '{{"id":%s,"ok":true,"error":null,"text":"The the the","stop_reason":"max_tokens","total_ms":1}}\n' "$id" ;;
        nostop) printf '{{"id":%s,"ok":true,"error":null,"text":"The meeting","total_ms":1}}\n' "$id" ;;
        badstop) printf '{{"id":%s,"ok":true,"error":null,"text":"The meeting","stop_reason":"length","total_ms":1}}\n' "$id" ;;
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

    fn flag(log: &Path, suffix: &str) -> PathBuf {
        PathBuf::from(format!("{}.{suffix}", log.display()))
    }

    fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    /// The process is gone and reaped (not even a zombie).
    fn reaped(pid: u32) -> bool {
        let out = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().is_empty()
    }

    /// The process ids of every start of the fake.
    fn pids(log: &Path) -> Vec<u32> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("pid ")?.parse().ok())
            .collect()
    }

    /// Collects the log output of the test thread.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
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
            true,
        );
        GenerateRequest {
            budget_ms,
            ..call.request
        }
    }

    fn generate(h: &mut RefinerHandle, budget_ms: Millis) -> Result<Generated, RefineError> {
        h.generate(
            RefineKind::Translate,
            &request(budget_ms),
            &CancelToken::new(),
        )
    }

    #[test]
    fn warms_once_per_prefix_and_generates() {
        let (script, log) = fake_sidecar("ok", "ok");
        let r = refiner(script);
        let mut h = r.handle();
        let prefix = refine::prefix(RefineKind::Translate, &Glossary::default(), true);
        assert!(!h.ready(RefineKind::Translate));
        h.warm(RefineKind::Translate, &prefix).unwrap();
        assert!(h.ready(RefineKind::Translate));
        assert!(!h.ready(RefineKind::Clean));
        h.warm(RefineKind::Translate, &prefix).unwrap();
        assert_eq!(count(&log, "warm"), 1, "the same prefix is warmed once");
        let answer = generate(&mut h, 2_000).unwrap();
        assert_eq!(answer.text, "The meeting is at four.");
        assert!(!answer.truncated);
        assert_eq!(count(&log, "start"), 1);
        r.stop_all();
        assert!(!h.ready(RefineKind::Translate));
    }

    #[test]
    fn an_answer_cut_at_max_tokens_is_marked_truncated() {
        let (script, _) = fake_sidecar("runaway", "runaway");
        let answer = generate(&mut refiner(script).handle(), 2_000).unwrap();
        assert!(answer.truncated);
    }

    #[test]
    fn a_missing_or_unknown_stop_reason_is_a_protocol_error() {
        for mode in ["nostop", "badstop"] {
            let (script, _) = fake_sidecar(mode, mode);
            let r = refiner(script);
            assert_eq!(
                generate(&mut r.handle(), 2_000),
                Err(RefineError::Failed("bad_stop_reason".to_owned())),
                "{mode}"
            );
            r.stop_all();
        }
    }

    #[test]
    fn the_budget_and_the_cancel_stop_a_generation_and_keep_the_process() {
        let (script, log) = fake_sidecar("slow", "slow");
        let r = refiner(script);
        let mut h = r.handle();
        // Start the process first: only the budget phase is timed.
        h.warm(RefineKind::Translate, &[]).unwrap();
        let started = Instant::now();
        assert_eq!(generate(&mut h, 150), Err(RefineError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(2));
        let cancel = CancelToken::new();
        let later = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            later.cancel();
        });
        assert_eq!(
            h.generate(RefineKind::Translate, &request(5_000), &cancel),
            Err(RefineError::Cancelled)
        );
        canceller.join().unwrap();
        // The sidecar answered both cancels: still the first process.
        assert!(h.ready(RefineKind::Translate));
        assert_eq!(count(&log, "start"), 1);
        r.stop_all();
    }

    #[test]
    fn a_sidecar_that_ignores_the_cancel_is_killed_and_started_again_freely() {
        let (script, log) = fake_sidecar("hang", "hang");
        let r = refiner(script);
        let mut h = r.handle();
        // One more kill than the restart budget allows after crashes.
        for _ in 0..=refine::REFINER_MAX_RESTARTS {
            h.warm(RefineKind::Translate, &[]).ok();
            let pid = r.pid(RefineKind::Translate).unwrap();
            let started = Instant::now();
            assert_eq!(generate(&mut h, 100), Err(RefineError::Timeout));
            // Budget plus the cancel reply time, with room for a slow machine.
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(!h.ready(RefineKind::Translate));
            assert!(
                wait_until(Duration::from_secs(2), || reaped(pid)),
                "pid {pid}"
            );
        }
        // A kill is not a crash: no restart budget is used.
        assert_eq!(count(&log, "start"), refine::REFINER_MAX_RESTARTS + 1);
    }

    #[test]
    fn a_sidecar_that_stops_reading_cannot_hold_a_call_past_its_deadline() {
        let (script, _) = fake_sidecar("deaf", "deaf");
        let r = refiner(script);
        let mut h = r.handle();
        h.warm(RefineKind::Translate, &[]).ok();
        let pid = r.pid(RefineKind::Translate).unwrap();
        // Far more than a pipe holds, to a process that reads nothing: the write blocks.
        let mut big = request(200);
        if let Some(last) = big.messages.last_mut() {
            last.content = "あ".repeat(300_000);
        }
        let started = Instant::now();
        assert_eq!(
            h.generate(RefineKind::Translate, &big, &CancelToken::new()),
            Err(RefineError::Timeout)
        );
        // The budget plus the cancel reply, sending included, then a kill.
        assert!(
            started.elapsed() < Duration::from_millis(1_500),
            "{:?}",
            started.elapsed()
        );
        assert!(
            wait_until(Duration::from_secs(2), || reaped(pid)),
            "pid {pid}"
        );
        // The slot is free again: a stop does not wait for it.
        let started = Instant::now();
        r.stop_all();
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn crashes_get_three_restarts_then_the_step_pauses() {
        let (script, log) = fake_sidecar("crash", "crash");
        let r = refiner(script);
        let mut h = r.handle();
        // The first start and three restarts each crash on the generation.
        for _ in 0..4 {
            assert!(matches!(
                generate(&mut h, 2_000),
                Err(RefineError::Unavailable(_))
            ));
        }
        assert_eq!(count(&log, "start"), 4);
        // The fourth crash is refused a restart.
        assert_eq!(
            generate(&mut h, 2_000),
            Err(RefineError::Unavailable("paused_after_crashes".to_owned()))
        );
        assert_eq!(count(&log, "start"), 4);
        // The other step has its own budget.
        assert!(matches!(
            h.generate(RefineKind::Clean, &request(2_000), &CancelToken::new()),
            Err(RefineError::Unavailable(_))
        ));
        assert_eq!(count(&log, "start"), 5);
    }

    #[test]
    fn a_missing_model_or_sidecar_fails_without_starting() {
        let (script, log) = fake_sidecar("missing", "ok");
        let r = Refiner::new(Some(script), Box::new(|_| None));
        assert_eq!(
            generate(&mut r.handle(), 2_000),
            Err(RefineError::ModelMissing)
        );
        assert_eq!(count(&log, "start"), 0);
        assert_eq!(
            r.status(),
            RefinerStatus::default(),
            "a missing model is not listed"
        );
        let r = Refiner::new(None, Box::new(|_| Some(PathBuf::from("/dev/null"))));
        assert!(matches!(
            r.handle().warm(RefineKind::Translate, &[]),
            Err(RefineError::Unavailable(_))
        ));
        assert_eq!(
            r.status(),
            RefinerStatus {
                translate_error: Some("sidecar_missing".to_owned()),
                clean_error: None,
            }
        );
    }

    #[test]
    fn a_failed_load_is_shown_until_a_start_succeeds() {
        let (script, log) = fake_sidecar("status", "ok");
        std::fs::write(flag(&log, "fail"), "").unwrap();
        let r = refiner(script);
        let mut h = r.handle();
        let prefix = refine::prefix(RefineKind::Clean, &Glossary::default(), true);
        assert_eq!(
            h.warm(RefineKind::Clean, &prefix),
            Err(RefineError::Unavailable("model_load_failed".to_owned()))
        );
        assert_eq!(
            r.status(),
            RefinerStatus {
                translate_error: None,
                clean_error: Some("model_load_failed".to_owned()),
            }
        );
        // A failed load is not a crash: the next warm-up starts again and clears it.
        std::fs::remove_file(flag(&log, "fail")).unwrap();
        h.warm(RefineKind::Clean, &prefix).unwrap();
        assert_eq!(r.status(), RefinerStatus::default());
        assert_eq!(count(&log, "start"), 2);
        r.stop_all();
    }

    #[test]
    fn a_fatal_report_without_an_id_fails_at_once() {
        let (script, _) = fake_sidecar("initfail", "initfail");
        let r = refiner(script);
        let started = Instant::now();
        assert_eq!(
            r.handle().warm(RefineKind::Translate, &[]),
            Err(RefineError::Unavailable("worker_start_failed".to_owned()))
        );
        // At once: not after the start timeout, and with its own code, not as an exit.
        assert!(started.elapsed() < ms(refine::REFINER_START_TIMEOUT_MS));
        assert_eq!(
            r.status().translate_error.as_deref(),
            Some("worker_start_failed")
        );
        assert_eq!(r.pid(RefineKind::Translate), None);
    }

    #[test]
    fn a_metal_library_load_error_is_unavailable_and_tried_once_per_call() {
        let (script, log) = fake_sidecar("nometal", "nometal");
        let r = refiner(script);
        let mut h = r.handle();
        let refused = || Err(RefineError::Unavailable("metal_library_missing".to_owned()));
        // More calls than the restart budget allows after crashes.
        let calls = refine::REFINER_MAX_RESTARTS + 2;
        let started = Instant::now();
        // The first call is not captured. It registers this path's log call sites before the
        // test subscriber exists, so creating the subscriber includes them. A call site that a
        // parallel test registers first, while this is the only subscriber, can stay disabled.
        assert_eq!(generate(&mut h, 2_000), refused());
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer({
                let logs = logs.clone();
                move || logs.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for _ in 1..calls {
                assert_eq!(generate(&mut h, 2_000), refused());
            }
        });
        // Each call fails at once (not after the 60 s load budget) with one start and one load:
        // no retry inside a call. A load error is not a crash, so it does not use the restart
        // budget, and nothing starts the sidecar again without a call.
        assert!(started.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(200));
        assert_eq!(count(&log, "start"), calls);
        assert_eq!(count(&log, "load"), calls);
        assert_eq!(count(&log, "generate"), 0, "the text is never sent");
        assert_eq!(
            r.status(),
            RefinerStatus {
                translate_error: Some("metal_library_missing".to_owned()),
                clean_error: None,
            }
        );
        assert_eq!(r.pid(RefineKind::Translate), None);
        let started = pids(&log);
        assert_eq!(started.len(), calls);
        for pid in started {
            assert!(
                wait_until(Duration::from_secs(2), || reaped(pid)),
                "pid {pid}"
            );
        }
        // The code is logged; the text to translate is not.
        let logged = logs.text();
        assert!(logged.contains("metal_library_missing"), "{logged}");
        assert!(!logged.contains("会議"), "{logged}");
    }

    #[test]
    fn a_failed_warm_up_forgets_the_old_snapshot() {
        let (script, log) = fake_sidecar("warmfail", "ok");
        let r = refiner(script);
        let mut h = r.handle();
        let plain = refine::prefix(RefineKind::Clean, &Glossary::default(), true);
        let other = refine::prefix(
            RefineKind::Clean,
            &Glossary::new(&[murmur_core::settings::DictionaryEntry {
                id: "e1".to_owned(),
                written: "Murmur".to_owned(),
                readings: vec!["マーマー".to_owned()],
                scope: murmur_core::settings::DictionaryScope::Ja,
            }]),
            true,
        );
        h.warm(RefineKind::Clean, &plain).unwrap();
        std::fs::write(flag(&log, "warmfail"), "").unwrap();
        assert!(h.warm(RefineKind::Clean, &other).is_err());
        std::fs::remove_file(flag(&log, "warmfail")).unwrap();
        // The failed warm-up may have dropped the first snapshot: it is warmed again.
        h.warm(RefineKind::Clean, &plain).unwrap();
        assert_eq!(count(&log, "warm"), 3);
        r.stop_all();
    }

    #[test]
    fn switching_off_during_a_warm_up_starts_nothing_more() {
        let (script, log) = fake_sidecar("race", "slowload");
        let r = refiner(script);
        let mut warming = r.handle();
        let old = r.handle();
        let task = thread::spawn(move || {
            crate::refine_session::warm(
                refine::RefinePlan::new(RefineKind::Translate, true),
                &Glossary::default(),
                &mut warming,
            )
        });
        // Barrier: the translation model is loading (the cleanup one comes next).
        let loading = wait_until(ms(refine::REFINER_START_TIMEOUT_MS + 1_000), || {
            count(&log, "load") == 1
        });
        if !loading {
            // Do not strand the blocked fake if a slow test host misses the startup barrier.
            r.stop_all();
            task.join().unwrap();
            panic!("the fake sidecar did not reach the load barrier");
        }
        let started = Instant::now();
        r.stop_all();
        // The cancel reply and a kill (0.7 s), not the slot wait of a load (75 s), with room for
        // a busy machine.
        assert!(started.elapsed() < STOP_WAIT, "{:?}", started.elapsed());
        task.join().unwrap();
        // Only the translation sidecar ever started, and nothing runs now.
        assert_eq!(count(&log, "start"), 1);
        assert_eq!(r.pid(RefineKind::Translate), None);
        assert_eq!(r.pid(RefineKind::Clean), None);
        assert!(!old.ready(RefineKind::Translate) && !old.ready(RefineKind::Clean));
        // A stop is not a start failure.
        assert_eq!(r.status(), RefinerStatus::default());
        // A handle of the ended lifecycle starts nothing.
        let mut old = old;
        assert_eq!(
            old.warm(RefineKind::Clean, &[]),
            Err(RefineError::Unavailable(STOPPED.to_owned()))
        );
        assert_eq!(count(&log, "start"), 1);
        // A later recording takes a handle of the next lifecycle.
        std::fs::write(flag(&log, "go"), "").unwrap();
        r.handle().warm(RefineKind::Clean, &[]).ok();
        assert!(r.handle().ready(RefineKind::Clean));
        assert_eq!(count(&log, "start"), 2);
        r.stop_all();
    }

    #[test]
    fn stopping_one_model_keeps_the_other_models_lifecycle_and_process() {
        for stopped in [RefineKind::Clean, RefineKind::Translate] {
            let (script, log) = fake_sidecar("shared", "ok");
            let r = refiner(script);
            let mut old = r.handle();
            for step in [RefineKind::Clean, RefineKind::Translate] {
                old.warm(step, &[]).unwrap();
            }
            let kept = match stopped {
                RefineKind::Clean => RefineKind::Translate,
                RefineKind::Translate => RefineKind::Clean,
            };
            let stopped_pid = r.pid(stopped).unwrap();
            let kept_pid = r.pid(kept);
            r.stop(stopped);
            assert_eq!(r.pid(stopped), None);
            assert_eq!(r.pid(kept), kept_pid);
            assert!(wait_until(Duration::from_secs(2), || reaped(stopped_pid)));
            assert_eq!(
                old.warm(stopped, &[]),
                Err(RefineError::Unavailable(STOPPED.to_owned()))
            );
            old.generate(kept, &request(2_000), &CancelToken::new())
                .unwrap();
            assert_eq!(count(&log, "start"), 2);
            r.handle().warm(stopped, &[]).unwrap();
            assert_eq!(count(&log, "start"), 3);
            r.stop_all();
        }
    }

    #[test]
    fn changing_lists_rewarms_only_when_the_prefix_changes() {
        let (script, log) = fake_sidecar("lists", "ok");
        let r = refiner(script);
        let mut h = r.handle();
        for lists in [true, true, false, false, true] {
            let prefix = refine::prefix(RefineKind::Clean, &Glossary::default(), lists);
            h.warm(RefineKind::Clean, &prefix).unwrap();
        }
        assert_eq!(count(&log, "start"), 1);
        assert_eq!(count(&log, "warm"), 3);
        r.stop_all();
    }

    #[test]
    fn dropping_the_last_owner_stops_and_reaps_the_process() {
        let (script, _) = fake_sidecar("drop", "ok");
        let r = refiner(script);
        let mut h = r.handle();
        h.warm(RefineKind::Translate, &[]).ok();
        let pid = r.pid(RefineKind::Translate).unwrap();
        assert!(!reaped(pid));
        let started = Instant::now();
        drop(h);
        drop(r);
        // Dropping does not wait for the process.
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(
            wait_until(Duration::from_secs(3), || reaped(pid)),
            "pid {pid}"
        );
    }

    /// A child whose kill fails and which exits only when told.
    struct Stuck {
        exited: Arc<AtomicBool>,
        collected: Arc<AtomicUsize>,
    }

    impl Reapable for Stuck {
        fn kill(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }

        fn try_wait(&mut self) -> std::io::Result<bool> {
            let exited = self.exited.load(Ordering::SeqCst);
            if exited {
                self.collected.fetch_add(1, Ordering::SeqCst);
            }
            Ok(exited)
        }
    }

    #[test]
    fn a_kill_that_fails_does_not_hold_the_caller_and_is_reaped_later() {
        let exited = Arc::new(AtomicBool::new(false));
        let collected = Arc::new(AtomicUsize::new(0));
        let child: SharedChild = Arc::new(Mutex::new(Box::new(Stuck {
            exited: Arc::clone(&exited),
            collected: Arc::clone(&collected),
        })));
        let started = Instant::now();
        assert!(!kill_and_reap(&child, Duration::from_millis(100)));
        assert!(started.elapsed() < Duration::from_millis(400));
        // The reaper keeps trying off the caller's path and collects the exit when it comes.
        thread::sleep(Duration::from_millis(100));
        assert_eq!(collected.load(Ordering::SeqCst), 0);
        exited.store(true, Ordering::SeqCst);
        assert!(wait_until(Duration::from_secs(2), || collected
            .load(Ordering::SeqCst)
            > 0));
    }

    #[test]
    fn output_lines_are_bounded() {
        let mut input = Cursor::new(b"{\"id\":1}\nxxxxxxxxxxxx\ntail".to_vec());
        let mut buf = Vec::new();
        assert_eq!(
            read_line_bounded(&mut input, &mut buf, 10).unwrap(),
            LineRead::Line
        );
        assert_eq!(buf, b"{\"id\":1}");
        assert_eq!(
            read_line_bounded(&mut input, &mut buf, 10).unwrap(),
            LineRead::TooLong
        );
        let mut input = Cursor::new(b"tail".to_vec());
        assert_eq!(
            read_line_bounded(&mut input, &mut buf, 10).unwrap(),
            LineRead::Line
        );
        assert_eq!(buf, b"tail");
        assert_eq!(
            read_line_bounded(&mut input, &mut buf, 10).unwrap(),
            LineRead::Eof
        );
        assert!(is_plain_code("backend_init_failed"));
        assert!(!is_plain_code("Bad code: 会議"));
    }

    fn real_refiner() -> Refiner {
        real_refiner_with_clean_model(None)
    }

    fn real_refiner_with_clean_model(clean_model: Option<PathBuf>) -> Refiner {
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
        Refiner::new(
            Some(binary),
            Box::new(move |step| {
                if step == RefineKind::Clean
                    && let Some(model) = &clean_model
                {
                    return Some(model.clone());
                }
                Some(models.join(murmur_core::catalog::refine_model(step).file))
            }),
        )
    }

    /// Private synthetic evaluation. Item text is written only to the dedicated external folder;
    /// stdout/stderr from this test contain numeric CSV summaries, never inputs or model text.
    #[test]
    #[ignore = "needs MURMUR_REFINE_EVALSET, staged sidecar and local cleanup model"]
    fn evaluates_light_cleanup() {
        use murmur_core::settings::{DictionaryEntry, DictionaryScope};
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        #[derive(serde::Deserialize)]
        struct Term {
            ja: String,
        }
        #[derive(serde::Deserialize)]
        struct Item {
            id: u32,
            cat: Vec<String>,
            input: String,
            clean: String,
        }
        #[derive(serde::Deserialize)]
        struct EvalSet {
            glossary: Vec<Term>,
            items: Vec<Item>,
        }
        fn timing(values: &[u64]) -> (u64, u64) {
            let mut sorted = values.to_vec();
            sorted.sort_unstable();
            let median = murmur_core::metrics::median(&sorted).unwrap_or(0);
            let p95 = sorted
                .get((sorted.len() * 95).div_ceil(100).saturating_sub(1))
                .copied()
                .unwrap_or(0);
            (median, p95)
        }

        let path =
            std::env::var_os("MURMUR_REFINE_EVALSET").expect("MURMUR_REFINE_EVALSET is required");
        let eval: EvalSet =
            serde_json::from_reader(std::fs::File::open(&path).expect("eval input file"))
                .expect("eval JSON");
        assert!(!eval.items.is_empty(), "empty eval set");
        let set_name = Path::new(&path)
            .file_stem()
            .and_then(|name| name.to_str())
            .expect("eval set file stem")
            .to_owned();
        let model = std::env::var_os("MURMUR_REFINE_EVAL_MODEL").map(PathBuf::from);
        if let Some(model) = &model {
            assert!(model.is_file(), "eval model file missing");
        }
        let model_name = model
            .as_deref()
            .unwrap_or_else(|| {
                Path::new(murmur_core::catalog::refine_model(RefineKind::Clean).file)
            })
            .file_stem()
            .and_then(|name| name.to_str())
            .expect("eval model file stem")
            .to_owned();
        let lists_modes: &[bool] = match std::env::var("MURMUR_REFINE_EVAL_LISTS").as_deref() {
            Ok("1") => &[true],
            Ok("0") => &[false],
            Ok("both") | Err(_) => &[true, false],
            Ok(_) => panic!("MURMUR_REFINE_EVAL_LISTS must be 0, 1 or both"),
        };
        let dictionary: Vec<_> = eval
            .glossary
            .into_iter()
            .enumerate()
            .map(|(i, term)| DictionaryEntry {
                id: format!("eval-{i}"),
                written: term.ja,
                readings: Vec::new(),
                scope: DictionaryScope::Ja,
            })
            .collect();
        let glossary = Glossary::new(&dictionary);
        let long: Vec<_> = eval
            .items
            .iter()
            .filter(|item| item.cat.iter().any(|cat| cat == "long"))
            .collect();
        assert!(
            long.is_empty() || long.len() == 10,
            "benchmark needs ten long items"
        );
        let repeats = std::env::var("MURMUR_REFINE_EVAL_REPEATS")
            .ok()
            .map(|value| value.parse::<usize>().expect("numeric repeats"))
            .unwrap_or(10);
        assert!((1..=20).contains(&repeats));
        let root = PathBuf::from(std::env::var_os("HOME").expect("HOME"))
            .join(".murmur-e2e/eval-stage2/models");
        std::fs::create_dir_all(&root).expect("eval output directory");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private eval directory");
        let r = real_refiner_with_clean_model(model);
        let mut h = r.handle();
        let cold_started = Instant::now();
        {
            let mut slot = h.slot(RefineKind::Clean, None).expect("eval model slot");
            h.ensure_running(RefineKind::Clean, &mut slot, None)
                .expect("eval cold model start and load");
        }
        let cold_start_load_ms = cold_started.elapsed().as_millis() as u64;
        let model_file = (r.inner.model_paths)(RefineKind::Clean).expect("eval model path");
        let file_bytes = model_file.metadata().expect("eval model metadata").len();
        let mut summaries = Vec::new();
        for &lists in lists_modes {
            let flag = u8::from(lists);
            let prefix = refine::prefix(RefineKind::Clean, &glossary, lists);
            let warm_started = Instant::now();
            h.warm(RefineKind::Clean, &prefix)
                .expect("eval model warm-up");
            let prefix_warm_ms = warm_started.elapsed().as_millis() as u64;
            let mut rss_after_request_bytes = None;
            let mut newly_rejected = 0;
            let mut guard_rejected = 0;
            let mut expected_validation_failures = 0;
            let output_stem = format!("{model_name}-{set_name}-lists-{flag}");
            let mut output = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(root.join(format!("{output_stem}.jsonl")))
                .expect("private eval output");
            let (
                mut passed,
                mut exact,
                mut delivered_exact,
                mut question_request_rejects,
                mut short_skips,
            ) = (0, 0, 0, 0, 0);
            let mut times = Vec::new();
            for item in &eval.items {
                // With lists disabled, the three enumeration fixtures keep their spoken prose.
                let expected = if !lists && item.cat.iter().any(|cat| cat == "list") {
                    &item.input
                } else {
                    &item.clean
                };
                let expected_rejections =
                    refine::validate_clean(expected, &item.input, glossary.terms(), lists);
                expected_validation_failures += usize::from(!expected_rejections.is_empty());
                let (_, call) =
                    refine::Run::start(RefineKind::Clean, &item.input, glossary.clone(), lists);
                let started = Instant::now();
                // Generate every fixture diagnostically, even those the product gates.
                let generated = h.generate(call.step, &call.request, &CancelToken::new());
                let elapsed = started.elapsed().as_millis() as u64;
                times.push(elapsed);
                if rss_after_request_bytes.is_none() {
                    let pid = r.pid(RefineKind::Clean).expect("eval sidecar pid");
                    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
                    let size = i32::try_from(std::mem::size_of_val(&info)).expect("task info size");
                    // SAFETY: proc_pidinfo gets the correctly sized writable task-info buffer.
                    let read = unsafe {
                        libc::proc_pidinfo(
                            i32::try_from(pid).expect("sidecar pid range"),
                            libc::PROC_PIDTASKINFO,
                            0,
                            info.as_mut_ptr().cast(),
                            size,
                        )
                    };
                    assert_eq!(read, size, "eval RSS query failed");
                    // SAFETY: the full structure was initialized by the successful call above.
                    rss_after_request_bytes = Some(unsafe { info.assume_init() }.pti_resident_size);
                }
                let finished = generated.as_ref().ok().map(|generated| {
                    refine::finish(
                        RefineKind::Clean,
                        &generated.text,
                        generated.truncated,
                        &item.input,
                        &glossary,
                        lists,
                    )
                });
                let accepted = finished.as_ref().and_then(|result| result.as_ref().ok());
                let valid = accepted.is_some();
                passed += usize::from(valid);
                exact += usize::from(accepted == Some(expected));
                let gate = refine::clean_gate(&item.input);
                short_skips += usize::from(gate == refine::CleanGate::SkipShort);
                let delivered = if gate == refine::CleanGate::Run {
                    accepted.unwrap_or(&item.input)
                } else {
                    &item.input
                };
                delivered_exact += usize::from(delivered == expected);
                let question_request = item
                    .cat
                    .iter()
                    .any(|cat| cat == "question" || cat == "request");
                question_request_rejects += usize::from(question_request && !valid);
                if let Some(Err(rules)) = &finished
                    && rules.contains(&refine::Rejection::UnmarkedSelfCorrection)
                {
                    guard_rejected += 1;
                    newly_rejected += usize::from(rules.len() == 1);
                }
                let rejections = finished
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .map(|rules| {
                        rules
                            .iter()
                            .map(|rule| format!("{rule:?}"))
                            .collect::<Vec<_>>()
                    });
                let row = json!({
                    "id": item.id, "lists": lists, "categories": item.cat, "input": item.input,
                    "expected": expected, "raw": generated.as_ref().ok().map(|g| &g.text),
                    "postprocessed": generated.as_ref().ok().map(|g| refine::postprocess(&g.text, &item.input)),
                    "truncated": generated.as_ref().ok().map(|g| g.truncated), "valid": valid,
                    "rejections": rejections, "error": generated.as_ref().err().map(ToString::to_string),
                    "elapsed_ms": elapsed, "gate": format!("{gate:?}"), "delivered": delivered,
                    "exact": accepted == Some(expected), "delivered_exact": delivered == expected,
                    "answer_review": null, "correction_review": null,
                    "negative_control_review": null,
                    "expected_rejections": expected_rejections.iter().map(|rule| format!("{rule:?}")).collect::<Vec<_>>(),
                });
                serde_json::to_writer(&mut output, &row).expect("eval item write");
                writeln!(&mut output).expect("eval line write");
            }
            let mut benchmark = Vec::new();
            let mut benchmark_errors = 0;
            for _ in 0..repeats {
                for item in &long {
                    let (_, call) =
                        refine::Run::start(RefineKind::Clean, &item.input, glossary.clone(), lists);
                    let started = Instant::now();
                    let generated = h.generate(call.step, &call.request, &CancelToken::new());
                    benchmark.push(started.elapsed().as_millis() as u64);
                    benchmark_errors += usize::from(generated.is_err());
                }
            }
            let (quality_median, quality_p95) = timing(&times);
            let (benchmark_median, benchmark_p95) = timing(&benchmark);
            let summary = json!({
                "lists": lists, "items": eval.items.len(), "passed_validation": passed,
                "accepted_exact": exact, "delivered_exact": delivered_exact,
                "rejected_question_request_candidates": question_request_rejects,
                "short_skips": short_skips, "quality_median_ms": quality_median, "quality_p95_ms": quality_p95,
                "benchmark_samples": benchmark.len(), "benchmark_errors": benchmark_errors,
                "benchmark_median_ms": benchmark_median, "benchmark_p95_ms": benchmark_p95,
                "benchmark_times_ms": benchmark,
                "model": model_name, "eval_set": set_name, "file_bytes": file_bytes,
                "cold_start_load_ms": cold_start_load_ms, "prefix_warm_ms": prefix_warm_ms,
                "rss_after_request_bytes": rss_after_request_bytes,
                "guard_rejected": guard_rejected, "newly_rejected": newly_rejected,
                "expected_validation_failures": expected_validation_failures,
            });
            let summary_file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(root.join(format!("{output_stem}-summary.json")))
                .expect("eval summary file");
            serde_json::to_writer_pretty(summary_file, &summary).expect("eval summary write");
            summaries.push(summary);
            eprintln!(
                "{flag},{},{passed},{exact},{delivered_exact},{question_request_rejects},{short_skips},{quality_median},{quality_p95},{benchmark_median},{benchmark_p95},{benchmark_errors},{cold_start_load_ms},{prefix_warm_ms},{},{file_bytes},{guard_rejected},{newly_rejected},{expected_validation_failures}",
                eval.items.len(),
                rss_after_request_bytes.unwrap_or(0)
            );
        }
        let summary_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(root.join(format!("{model_name}-{set_name}-summary.json")))
            .expect("eval summary file");
        serde_json::to_writer_pretty(summary_file, &summaries).expect("eval summary write");
        r.stop_all();
    }

    /// Opt-in rollout measurements: private synthetic text stays outside the repository.
    #[test]
    #[ignore = "needs MURMUR_REFINE_TIMINGSET and the staged default models"]
    fn measures_default_cleanup_rollout() {
        use crate::refine_session;
        use murmur_core::refine::{Glossary, RefinePlan, RefineStatus};
        use murmur_core::session::{
            DeliveryOutcome, Effect, Event, Machine, Notice, PillView, RefineOutcome, State,
            TranscribeOutcome, TranscriptText,
        };
        use murmur_core::settings::{DictionaryEntry, DictionaryScope, RecordMode};
        use std::os::unix::fs::OpenOptionsExt;

        struct Timed<'a> {
            inner: &'a mut RefinerHandle,
            calls: Vec<(RefineKind, u64)>,
        }
        impl TextRefiner for Timed<'_> {
            fn warm(
                &mut self,
                step: RefineKind,
                prefix: &[ChatMessage],
            ) -> Result<(), RefineError> {
                self.inner.warm(step, prefix)
            }
            fn ready(&self, step: RefineKind) -> bool {
                self.inner.ready(step)
            }
            fn generate(
                &mut self,
                step: RefineKind,
                request: &GenerateRequest,
                cancel: &CancelToken,
            ) -> Result<Generated, RefineError> {
                let started = Instant::now();
                let result = self.inner.generate(step, request, cancel);
                self.calls
                    .push((step, started.elapsed().as_millis() as u64));
                result
            }
        }
        fn transcribed_machine(source: &str) -> Machine {
            let mut machine = Machine::new();
            let press = Event::HotkeyDown {
                mode: RecordMode::Toggle,
                refine: Some(RefinePlan::new(RefineKind::Clean, true)),
            };
            machine.step(press.clone(), 0);
            let session = machine.active_session().expect("recording session");
            machine.step(press, 50);
            machine.step(
                Event::Transcribed {
                    session,
                    outcome: TranscribeOutcome::Text(TranscriptText(source.to_owned())),
                },
                100,
            );
            machine
        }

        let path = std::env::var_os("MURMUR_REFINE_TIMINGSET").expect("timing fixture path");
        let fixture: Value =
            serde_json::from_reader(std::fs::File::open(path).expect("timing fixture"))
                .expect("timing JSON");
        let dictionary: Vec<_> = fixture["glossary"]
            .as_array()
            .expect("timing glossary")
            .iter()
            .enumerate()
            .map(|(i, term)| DictionaryEntry {
                id: format!("timing-{i}"),
                written: term["ja"].as_str().expect("timing term").to_owned(),
                readings: vec![],
                scope: DictionaryScope::Ja,
            })
            .collect();
        let glossary = Glossary::new(&dictionary);
        let r = real_refiner();
        let mut h = r.handle();
        let plan = RefinePlan::new(RefineKind::Translate, true);
        let warm_started = Instant::now();
        refine_session::warm(plan, &glossary, &mut h);
        assert!(h.ready(RefineKind::Clean) && h.ready(RefineKind::Translate));
        let combined_start_load_warm_ms = warm_started.elapsed().as_millis() as u64;
        let mut rows = Vec::new();
        for repeat in 0..5 {
            for item in fixture["items"].as_array().expect("timing items") {
                let source = item["input"].as_str().expect("timing input");
                let mut timed = Timed {
                    inner: &mut h,
                    calls: vec![],
                };
                let started = Instant::now();
                let mut loads = 0;
                let report = refine_session::drive(
                    plan,
                    source,
                    glossary.clone(),
                    &mut timed,
                    &CancelToken::new(),
                    &mut || loads += 1,
                );
                let elapsed_ms = started.elapsed().as_millis() as u64;
                assert_eq!(loads, 0);
                assert!(elapsed_ms < refine::timeout_ms(plan.kind, source));
                rows.push(json!({"id": item["id"], "repeat": repeat,
                    "chars": source.chars().count(), "elapsed_ms": elapsed_ms,
                    "pre_clean": format!("{:?}", report.pre_clean),
                    "steps": timed.calls.iter().map(|(step, elapsed)| json!({"step": step.code(), "ms": elapsed})).collect::<Vec<_>>(),
                    "ok": report.result.is_ok(), "input": source,
                    "result": report.result.as_ref().ok().map(|text| &text.0),
                    "error": report.result.as_ref().err().map(ToString::to_string)}));
            }
        }
        let pid = r.pid(RefineKind::Clean).expect("cleanup pid");
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
        let size = i32::try_from(std::mem::size_of_val(&info)).expect("task info size");
        // SAFETY: the correctly sized writable buffer is checked before it is read.
        let read = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).expect("pid range"),
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        assert_eq!(read, size);
        // SAFETY: proc_pidinfo initialized the entire structure.
        let rss_after_long_requests_bytes = unsafe { info.assume_init() }.pti_resident_size;
        r.stop_all();

        let cold = real_refiner();
        let mut foreground = cold.handle();
        let mut warming = cold.handle();
        let prefix = refine::prefix(RefineKind::Clean, &Glossary::default(), true);
        let worker = thread::spawn(move || warming.warm(RefineKind::Clean, &prefix));
        assert!(wait_until(Duration::from_secs(2), || cold
            .inner
            .clean
            .slot
            .try_lock()
            .is_err()));
        thread::sleep(Duration::from_millis(50));
        let source = "あの、準備した資料を机に置きます。";
        let mut machine = transcribed_machine(source);
        let session = machine.active_session().expect("cold session");
        let mut loads = 0;
        let started = Instant::now();
        let report = refine_session::drive(
            RefinePlan::new(RefineKind::Clean, true),
            source,
            Glossary::default(),
            &mut foreground,
            &CancelToken::new(),
            &mut || {
                loads += 1;
                machine.step(Event::RefinerLoading { session }, 100);
            },
        );
        let cold_stop_to_result_ms = started.elapsed().as_millis() as u64;
        assert!(report.result.is_ok(), "cold cleanup failed");
        assert_eq!(loads, 1);
        assert!(matches!(machine.state(), State::Refining(state) if state.load_extended));
        assert!(
            cold_stop_to_result_ms
                < refine::REFINER_LOAD_TIMEOUT_MS + refine::timeout_ms(RefineKind::Clean, source)
        );
        assert!(wait_until(SLOT_WAIT, || worker.is_finished()));
        assert!(worker.join().expect("warm thread").is_ok());

        // A short unmarked first recording never waits for a model call.
        let short_started = Instant::now();
        let short = transcribed_machine("お疲れさまです。");
        assert!(matches!(short.state(), State::Delivering { .. }));
        let short_skip_ms = short_started.elapsed().as_millis() as u64;
        // Exercise the real adapter's deadline and a fresh session's rule-text fallback together.
        let mut machine = transcribed_machine(source);
        let session = machine.active_session().expect("fallback session");
        let (_, mut call) =
            refine::Run::start(RefineKind::Clean, source, Glossary::default(), true);
        call.request.budget_ms = 1;
        let started = Instant::now();
        let timed_out = foreground.generate(RefineKind::Clean, &call.request, &CancelToken::new());
        let forced_timeout_ms = started.elapsed().as_millis() as u64;
        assert!(matches!(timed_out, Err(RefineError::Timeout)));
        assert!(forced_timeout_ms < 1_000);
        let fx = machine.step(
            Event::Refined {
                session,
                outcome: RefineOutcome::TimedOut,
            },
            200,
        );
        assert!(fx.iter().any(|effect| matches!(effect, Effect::Deliver { text, copy_only: None, .. } if text.0 == source)));
        let fx = machine.step(
            Event::Delivered {
                session,
                outcome: DeliveryOutcome::Pasted,
            },
            201,
        );
        assert!(fx.iter().any(|effect| matches!(
            effect,
            Effect::SaveSuccess {
                refine: Some(RefineStatus::Timeout),
                ..
            }
        )));
        assert!(matches!(
            machine.view(201),
            PillView::Notice {
                notice: Notice::CleanupSkipped { .. }
            }
        ));
        cold.stop_all();

        let root = PathBuf::from(std::env::var_os("HOME").expect("HOME"))
            .join(".murmur-e2e/eval-stage2/models");
        let output = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(root.join(format!(
                "rollout-m3c-limit-{}.json",
                refine::CLEAN_MAX_CHARS
            )))
            .expect("rollout output");
        serde_json::to_writer_pretty(
            output,
            &json!({"limit": refine::CLEAN_MAX_CHARS,
            "combined_start_load_warm_ms": combined_start_load_warm_ms,
            "rss_after_long_requests_bytes": rss_after_long_requests_bytes,
            "cold_recording_ms": 50, "cold_stop_to_result_ms": cold_stop_to_result_ms,
            "cold_loading_events": loads, "short_skip_ms": short_skip_ms,
            "forced_timeout_ms": forced_timeout_ms, "translation": rows}),
        )
        .expect("rollout write");
        eprintln!(
            "{},{combined_start_load_warm_ms},{cold_stop_to_result_ms},{short_skip_ms},{forced_timeout_ms},{rss_after_long_requests_bytes}",
            refine::CLEAN_MAX_CHARS
        );
    }

    /// The real sidecar and models: the staged binary (`scripts/build-sidecar.sh`, or
    /// `MURMUR_LLM_BIN`) and the GGUF files in `$MURMUR_TEST_MODELS/llm` (default
    /// `~/.murmur-dev-models/llm`). Run: `cargo test -p murmur refiner -- --ignored`.
    #[test]
    #[ignore = "needs the staged sidecar and the LLM models"]
    fn translates_with_the_real_sidecar() {
        use crate::refine_session;
        use murmur_core::refine::PreClean;

        let r = real_refiner();
        let mut h = r.handle();
        let glossary = Glossary::default();
        refine_session::warm(
            refine::RefinePlan::new(RefineKind::Translate, true),
            &glossary,
            &mut h,
        );
        assert!(h.ready(RefineKind::Translate) && h.ready(RefineKind::Clean));
        let mut loading = 0;
        for (text, cleaned_first) in [
            ("明日の会議は午後4時からです。", false),
            ("会議は3時から、いや4時からでお願いします。", true),
        ] {
            let started = Instant::now();
            let report = refine_session::drive(
                refine::RefinePlan::new(RefineKind::Translate, true),
                text,
                glossary.clone(),
                &mut h,
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
        // The cleanup feature uses both prompt variants against the same real process.
        let text = "あの、明日の会議の資料はSlackのチャンネルに上げておきました。";
        for lists in [true, false] {
            let plan = refine::RefinePlan::new(RefineKind::Clean, lists);
            refine_session::warm(plan, &glossary, &mut h);
            let prefix = refine::prefix(plan.kind, &glossary, lists);
            assert_eq!(
                lock_ignoring_poison(&r.inner.clean.slot).warmed.as_ref(),
                Some(&prefix)
            );
            let report = refine_session::drive(
                plan,
                text,
                glossary.clone(),
                &mut h,
                &CancelToken::new(),
                &mut || loading += 1,
            );
            assert!(
                report.result.is_ok(),
                "cleanup validation failed: {:?}",
                report.result.err()
            );
            assert_eq!(report.steps, vec![RefineKind::Clean]);
            assert_eq!(report.pre_clean, PreClean::NotNeeded);
        }
        assert_eq!(loading, 0, "both models were warm");
        let pids = [
            r.pid(RefineKind::Translate).unwrap(),
            r.pid(RefineKind::Clean).unwrap(),
        ];
        r.stop_all();
        assert!(!h.ready(RefineKind::Translate));
        for pid in pids {
            assert!(
                wait_until(Duration::from_secs(2), || reaped(pid)),
                "pid {pid}"
            );
        }
    }
}
