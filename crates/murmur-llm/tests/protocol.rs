//! Black-box protocol checks that do not require model files.

use serde_json::{Value, json};
use std::{
    error::Error,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: Receiver<Result<Value, String>>,
}

impl Client {
    fn start() -> TestResult<Self> {
        Self::start_from(Path::new(env!("CARGO_BIN_EXE_murmur-llm")))
    }

    fn start_from(binary: &Path) -> TestResult<Self> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("missing stdin")?;
        let stdout = child.stdout.take().ok_or("missing stdout")?;
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let result = line
                    .map_err(|_| "stdout read failed".to_owned())
                    .and_then(|line| {
                        serde_json::from_str(&line).map_err(|_| "invalid response JSON".to_owned())
                    });
                if tx.send(result).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            replies,
        })
    }

    fn request(&mut self, line: &str) -> TestResult<Value> {
        let stdin = self.stdin.as_mut().ok_or("stdin closed")?;
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(self.replies.recv_timeout(Duration::from_secs(65))??)
    }

    fn close(&mut self) -> TestResult {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait()? {
                assert!(status.success());
                let mut stderr = Vec::new();
                self.child
                    .stderr
                    .take()
                    .ok_or("missing stderr")?
                    .read_to_end(&mut stderr)?;
                assert!(stderr.is_empty(), "native stderr must be silent");
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("stdin EOF teardown exceeded deadline".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Also terminate failed tests instead of leaving a model worker behind.
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn invalid_lines_do_not_poison_following_requests() -> TestResult {
    let mut client = Client::start()?;
    for line in ["not json", r#"{"id":1,"op":"network"}"#] {
        let reply = client.request(line)?;
        assert_eq!(reply["id"], Value::Null);
        assert_eq!(reply["error"], "invalid_request");
    }
    let reply = client.request(r#"{"id":-1,"op":"cancel","target":1}"#)?;
    assert_eq!(reply["error"], "invalid_id");
    let reply = client.request(r#"{"id":"cancel","op":"cancel","target":1}"#)?;
    assert_eq!(
        reply,
        json!({"id":"cancel","ok":true,"error":null,"cancel_requested":false})
    );
    client.close()
}

#[test]
fn loading_failures_leave_worker_available_and_no_model_loaded() -> TestResult {
    let mut client = Client::start()?;
    for (line, error) in [
        (r#"{"id":1,"op":"warm","system":"s"}"#, "model_not_loaded"),
        (
            r#"{"id":2,"op":"load","model":"/nonexistent/murmur.gguf","ctx":0}"#,
            "invalid_ctx",
        ),
        (
            r#"{"id":3,"op":"load","model":"/nonexistent/murmur.gguf"}"#,
            "model_load_failed",
        ),
        (
            r#"{"id":4,"op":"generate","system":"s","user":"u","max_tokens":8}"#,
            "model_not_loaded",
        ),
    ] {
        let reply = client.request(line)?;
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], error);
        assert!(reply["total_ms"].as_f64().is_some_and(|ms| ms >= 0.0));
    }
    client.close()
}

#[test]
fn idle_stdin_eof_terminates_without_a_request() -> TestResult {
    Client::start()?.close()
}

struct LibraryFixture {
    root: PathBuf,
    binary: PathBuf,
    model: PathBuf,
}

impl LibraryFixture {
    fn new(case: &str) -> TestResult<Self> {
        let root =
            std::env::temp_dir().join(format!("murmur-metallib-{}-{case}", std::process::id()));
        std::fs::create_dir(&root)?;
        let binary = root.join("murmur-llm");
        std::fs::copy(env!("CARGO_BIN_EXE_murmur-llm"), &binary)?;
        // A regular file passes the path precheck. No GGUF is parsed unless Metal setup
        // incorrectly succeeds, in which case the error assertion below fails.
        let model = root.join("fixture.gguf");
        std::fs::write(&model, [])?;
        Ok(Self {
            root,
            binary,
            model,
        })
    }

    fn reject_load(&self, error: &str) -> TestResult {
        let mut client = Client::start_from(&self.binary)?;
        let reply = client.request(&json!({"id":1,"op":"load","model":self.model}).to_string())?;
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], error);
        // Failure is correlated and the reader/worker stay usable without a model.
        let reply = client.request(r#"{"id":2,"op":"warm","system":"s"}"#)?;
        assert_eq!(reply["error"], "model_not_loaded");
        client.close()
    }
}

impl Drop for LibraryFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn missing_default_metallib_fails_before_native_initialization() -> TestResult {
    let fixture = LibraryFixture::new("missing-default")?;
    std::fs::write(fixture.root.join("ggml-tensor.metallib"), b"fixture")?;
    fixture.reject_load("metal_library_missing")
}

#[test]
fn missing_tensor_metallib_is_an_error_even_on_a_non_tensor_gpu() -> TestResult {
    let fixture = LibraryFixture::new("missing-tensor")?;
    std::fs::write(fixture.root.join("default.metallib"), b"fixture")?;
    fixture.reject_load("metal_library_missing")
}

#[test]
fn corrupt_default_metallib_does_not_fall_back_to_cpu_or_source() -> TestResult {
    let fixture = LibraryFixture::new("corrupt-default")?;
    for name in ["default.metallib", "ggml-tensor.metallib"] {
        std::fs::write(fixture.root.join(name), b"not a Metal library")?;
    }
    fixture.reject_load("metal_library_load_failed")
}
