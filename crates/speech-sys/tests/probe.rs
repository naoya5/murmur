//! Probe of speech.cpp through the raw bindings.
//!
//! `cargo test` runs the version test alone; the model tests are `#[ignore]` and need the models
//! and the WAVs:
//!
//! ```text
//! MURMUR_TEST_MODELS=~/.murmur-dev-models MURMUR_TEST_WAVS=/path/to/wav \
//! MURMUR_SPEECH_CLI=/path/to/speech \
//! cargo test --release --test probe -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `--test-threads=1` matters: a model serves one request at a time, the first call that touches a
//! device sets an environment variable, and the machine's memory holds only so many models.
//! `MURMUR_SPEECH_CLI` is optional; with it, library texts are compared to the CLI's.
//!
//! Every model, request and detection is a guard that frees itself, so that a failed assertion does
//! not leave a model loaded when the process ends, which aborts on Metal.

// Test code: clippy's test allowances cover `#[test]` functions, not the helpers around them.
#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use speech_sys::*;

const HAAS: &str = "Haas-616M-ja-v1.0-Q8_0.gguf";
const PARAKEET: &str = "parakeet-tdt-0.6B-v3-Q8_0.gguf";
const SILERO: &str = "silero-vad-309K-v6.2-F32.gguf";
const RATE: u32 = 16_000;

// ---------------------------------------------------------------------------------------------
// Environment

struct TestEnv {
    models: PathBuf,
    wavs: PathBuf,
    cli: Option<PathBuf>,
}

impl TestEnv {
    /// Reads every variable once, before any thread exists.
    fn load() -> Self {
        let home = std::env::var("HOME").expect("HOME");
        let models = std::env::var("MURMUR_TEST_MODELS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(&home).join(".murmur-dev-models"));
        let wavs = PathBuf::from(
            std::env::var("MURMUR_TEST_WAVS").expect("MURMUR_TEST_WAVS must point at the WAV dir"),
        );
        let cli = std::env::var("MURMUR_SPEECH_CLI").ok().map(PathBuf::from);
        Self { models, wavs, cli }
    }

    fn model(&self, file: &str) -> PathBuf {
        let path = self.models.join(file);
        assert!(path.is_file(), "model missing: {}", path.display());
        path
    }

    fn wav(&self, file: &str) -> PathBuf {
        let path = self.wavs.join(file);
        assert!(path.is_file(), "wav missing: {}", path.display());
        path
    }

    /// The CLI's text for one file, or None without a CLI.
    fn cli_text(&self, model: &Path, wav: &Path, extra: &[&str]) -> Option<String> {
        let cli = self.cli.as_ref()?;
        let output = Command::new(cli)
            .arg("asr")
            .arg(model)
            .args(extra)
            .arg(wav)
            .output()
            .expect("run speech CLI");
        assert!(
            output.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

// ---------------------------------------------------------------------------------------------
// FFI helpers

fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the library returns NUL-terminated UTF-8 or NULL, checked above.
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

fn status_name(status: speech_status) -> String {
    // SAFETY: a plain lookup of a process-lifetime string.
    cstr(unsafe { speech_status_name(status) })
}

fn last_error() -> String {
    // SAFETY: both read the calling thread's last error.
    let (message, option) = unsafe { (speech_last_error(), speech_last_error_option()) };
    format!("{} (option {})", cstr(message), cstr(option))
}

/// Panics with the library's message when a call failed.
fn check(what: &str, status: speech_status) -> speech_status {
    assert!(
        status >= 0,
        "{what}: {} - {}",
        status_name(status),
        last_error()
    );
    status
}

fn c_string(text: &str) -> CString {
    CString::new(text).expect("no NUL in text")
}

struct Model(*mut speech_model);

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: every request and detection of this model has been dropped first (declared
        // after it).
        unsafe { speech_model_free(self.0) };
    }
}

struct Request(*mut speech_request);

impl Drop for Request {
    fn drop(&mut self) {
        // SAFETY: owned pointer, freed once.
        unsafe { speech_request_free(self.0) };
    }
}

struct Detection(*mut speech_detection);

impl Drop for Detection {
    fn drop(&mut self) {
        // SAFETY: owned pointer, freed once.
        unsafe { speech_detection_free(self.0) };
    }
}

/// A raw request pointer sent to the cancelling thread. `speech_request_cancel` is the one call the
/// header allows from any thread while the request exists.
struct SendRequest(*mut speech_request);
// SAFETY: see above; the receiving thread calls nothing but speech_request_cancel.
unsafe impl Send for SendRequest {}

/// Loads a model on a device, with or without the warm-up request.
fn load(path: &Path, device: &str, warmup: bool) -> (Model, Duration) {
    let path = c_string(path.to_str().expect("utf-8 path"));
    let device = c_string(device);
    let mut params = std::ptr::null_mut();
    let mut model = std::ptr::null_mut();
    let started = Instant::now();
    // SAFETY: valid NUL-terminated strings and out pointers; params freed after the load.
    unsafe {
        check("load_params_new", speech_load_params_new(&mut params));
        check(
            "set_device",
            speech_load_params_set_device(params, device.as_ptr()),
        );
        check(
            "set_warmup",
            speech_load_params_set_warmup(params, c_int::from(warmup)),
        );
        let status = speech_model_load(path.as_ptr(), params, &mut model);
        speech_load_params_free(params);
        check("model_load", status);
    }
    (Model(model), started.elapsed())
}

/// The device and thread count the loaded model reports.
fn model_placement(model: &Model) -> (String, i32) {
    let mut info = std::ptr::null_mut();
    // SAFETY: valid model; the info is freed before returning.
    unsafe {
        check("model_get_info", speech_model_get_info(model.0, &mut info));
        let device = cstr(speech_model_info_device(info));
        let threads = speech_model_info_threads(info);
        speech_model_info_free(info);
        (device, threads)
    }
}

/// Whether the loaded model takes an option, and whether it steers.
fn option_support(model: &Model, option: speech_option) -> (bool, bool) {
    let mut info = std::ptr::null_mut();
    // SAFETY: valid model; the info is freed before returning.
    unsafe {
        check("model_get_info", speech_model_get_info(model.0, &mut info));
        let takes = speech_model_info_takes(info, option) != 0;
        let steers = speech_model_info_option_steers(info, option) != 0;
        speech_model_info_free(info);
        (takes, steers)
    }
}

fn new_request(model: &Model) -> Request {
    let mut request = std::ptr::null_mut();
    // SAFETY: valid model and out pointer.
    unsafe { check("request_new", speech_request_new(model.0, &mut request)) };
    Request(request)
}

fn set_audio(request: &Request, samples: &[f32]) {
    // SAFETY: the slice is valid for the call; the library copies it.
    unsafe {
        check(
            "set_audio",
            speech_request_set_audio(request.0, samples.as_ptr(), samples.len(), RATE as c_int),
        )
    };
}

fn set_string(request: &Request, option: speech_option, value: &str) -> speech_status {
    let value = c_string(value);
    // SAFETY: valid request and string.
    unsafe { speech_request_set_string(request.0, option, value.as_ptr()) }
}

fn set_int(request: &Request, option: speech_option, value: i64) {
    // SAFETY: valid request.
    unsafe { check("set_int", speech_request_set_int(request.0, option, value)) };
}

fn set_float(request: &Request, option: speech_option, value: f64) {
    // SAFETY: valid request.
    unsafe {
        check(
            "set_float",
            speech_request_set_float(request.0, option, value),
        )
    };
}

fn result_text(request: &Request) -> String {
    // SAFETY: the request returned OK or CANCELLED, so the result exists until the request is freed.
    unsafe {
        let result = speech_request_result(request.0);
        assert!(!result.is_null(), "no result");
        cstr(speech_result_text(result))
    }
}

fn result_stop(request: &Request) -> String {
    // SAFETY: as result_text.
    unsafe {
        let result = speech_request_result(request.0);
        assert!(!result.is_null(), "no result");
        cstr(speech_stop_name(speech_result_stop(result)))
    }
}

/// The segments of a detection result as (start, end) seconds.
fn result_regions(request: &Request) -> Vec<(f64, f64)> {
    // SAFETY: as result_text; every index is below the count.
    unsafe {
        let result = speech_request_result(request.0);
        assert!(!result.is_null(), "no result");
        let count = speech_result_segment_count(result);
        (0..count)
            .map(|index| {
                let (mut start, mut end) = (0.0, 0.0);
                check(
                    "result_segment",
                    speech_result_segment(
                        result,
                        index,
                        &mut start,
                        &mut end,
                        std::ptr::null_mut(),
                    ),
                );
                (start, end)
            })
            .collect()
    }
}

/// Transcribes samples with the options `configure` sets, returning the text and the time of
/// `speech_transcribe` alone.
fn transcribe(
    model: &Model,
    samples: &[f32],
    configure: impl FnOnce(&Request),
) -> (String, Duration) {
    let request = new_request(model);
    set_audio(&request, samples);
    configure(&request);
    let started = Instant::now();
    // SAFETY: valid request, used by this thread alone.
    let status = unsafe { speech_transcribe(request.0) };
    let elapsed = started.elapsed();
    check("transcribe", status);
    (result_text(&request), elapsed)
}

/// Silero regions of samples with the CLI's `--vad` defaults, and the detection time.
fn detect(
    vad: &Model,
    samples: &[f32],
    configure: impl FnOnce(&Request),
) -> (Vec<(f64, f64)>, Duration) {
    let request = new_request(vad);
    set_audio(&request, samples);
    configure(&request);
    let started = Instant::now();
    // SAFETY: valid request, used by this thread alone.
    let status = unsafe { speech_detect(request.0) };
    let elapsed = started.elapsed();
    check("detect", status);
    (result_regions(&request), elapsed)
}

/// The CLI's `--vad` defaults, so that library and CLI regions compare.
fn cli_vad_options(request: &Request) {
    set_float(request, SPEECH_OPT_THRESHOLD, 0.5);
    set_int(request, SPEECH_OPT_SPEECH_PAD_MS, 300);
    set_int(request, SPEECH_OPT_MIN_SILENCE_DURATION_MS, 500);
    set_float(request, SPEECH_OPT_MAX_SPEECH_DURATION_S, 10.0);
}

// ---------------------------------------------------------------------------------------------
// Audio and measurement helpers

/// Reads a 16 kHz mono 16-bit WAV as f32 samples in [-1, 1).
fn read_wav(path: &Path) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "{}: mono expected", path.display());
    assert_eq!(
        spec.sample_rate,
        RATE,
        "{}: 16 kHz expected",
        path.display()
    );
    assert_eq!(spec.bits_per_sample, 16, "{}: s16 expected", path.display());
    reader
        .samples::<i16>()
        .map(|sample| f32::from(sample.expect("wav sample")) / 32768.0)
        .collect()
}

fn seconds(samples: &[f32]) -> f64 {
    samples.len() as f64 / f64::from(RATE)
}

fn slice_seconds(samples: &[f32], start: f64, end: f64) -> &[f32] {
    let rate = f64::from(RATE);
    let from = ((start * rate).round() as usize).min(samples.len());
    let to = ((end * rate).round() as usize).clamp(from, samples.len());
    &samples[from..to]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(durations: &[Duration], fraction: f64) -> Duration {
    let mut sorted = durations.to_vec();
    sorted.sort();
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Peak resident set size of the process so far, in MiB.
fn peak_rss_mib() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer for the calling process.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage");
    // macOS reports ru_maxrss in bytes.
    usage.ru_maxrss as f64 / (1024.0 * 1024.0)
}

fn has_japanese_punctuation(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, '。' | '、' | '？' | '！' | '?' | '!' | ','))
}

fn report_equal(label: &str, library: &str, cli: Option<String>) {
    match cli {
        Some(cli) => println!(
            "[probe] {label}: library == CLI: {}\n        library: {library}\n        cli:     {cli}",
            library == cli
        ),
        None => println!(
            "[probe] {label}: {library}\n        (no MURMUR_SPEECH_CLI, CLI comparison skipped)"
        ),
    }
}

/// Load timings and the median of `runs` transcriptions of `clip` on one device.
fn measure(label: &str, model_path: &Path, device: &str, clip: &[f32], runs: usize) {
    let (cold, cold_load) = load(model_path, device, false);
    let (placement, threads) = model_placement(&cold);
    // The first request after a load without warm-up pays for the GPU's kernel compilation.
    let (_, cold_first) = transcribe(&cold, clip, |_| {});
    let (_, cold_second) = transcribe(&cold, clip, |_| {});
    drop(cold);
    let (model, warm_load) = load(model_path, device, true);
    let mut times = Vec::with_capacity(runs);
    let mut text = String::new();
    for _ in 0..runs {
        let (got, elapsed) = transcribe(&model, clip, |_| {});
        times.push(elapsed);
        text = got;
    }
    let median = percentile(&times, 0.5);
    let p95 = percentile(&times, 0.95);
    println!(
        "[probe] {label} device={placement} threads={threads} clip={:.2}s\n        \
         load(no warmup)={:.0}ms then first request={:.0}ms second={:.0}ms; load(warmup)={:.0}ms\n        \
         transcribe first={:.0}ms median={:.0}ms p95={:.0}ms RTF={:.3} peakRSS={:.0}MiB\n        \
         text: {text}",
        seconds(clip),
        ms(cold_load),
        ms(cold_first),
        ms(cold_second),
        ms(warm_load),
        ms(times[0]),
        ms(median),
        ms(p95),
        median.as_secs_f64() / seconds(clip),
        peak_rss_mib()
    );
}

// ---------------------------------------------------------------------------------------------
// 1. Build, link, version, devices

#[test]
fn version_and_devices() {
    // SAFETY: plain queries; the device calls set ggml up for the process on this thread.
    unsafe {
        assert_eq!(speech_api_version_major(), 4);
        assert_eq!(
            speech_api_version_major(),
            SPEECH_API_VERSION_MAJOR as c_int
        );
        assert_eq!(cstr(speech_version()), "0.10.0");
        let count = speech_device_count();
        assert!(count >= 1, "no device");
        for index in 0..count {
            let mut kind = 0;
            check("device_get_kind", speech_device_get_kind(index, &mut kind));
            let (mut total, mut free) = (0, 0);
            check(
                "device_memory",
                speech_device_memory(index, &mut total, &mut free),
            );
            println!(
                "[probe] device {index}: {} kind={kind} ({}) total={}MiB free={}MiB",
                cstr(speech_device_name(index)),
                cstr(speech_device_description(index)),
                total >> 20,
                free >> 20
            );
        }
        assert_eq!(status_name(SPEECH_OK), "ok");
        assert_eq!(status_name(SPEECH_CANCELLED), "cancelled");
        assert_eq!(cstr(speech_option_name(SPEECH_OPT_KEYWORDS)), "keywords");
    }
}

// ---------------------------------------------------------------------------------------------
// 2. Haas Q8_0 on gpu, texts against the CLI

#[test]
#[ignore]
fn haas_gpu_matches_cli() {
    let env = TestEnv::load();
    let model_path = env.model(HAAS);
    let (model, load_time) = load(&model_path, "gpu", false);
    let (placement, threads) = model_placement(&model);
    println!(
        "[probe] Haas on {placement} threads={threads} loaded in {:.0}ms",
        ms(load_time)
    );
    for file in ["ja.wav", "mixed.wav", "hai.wav"] {
        let wav = env.wav(file);
        let samples = read_wav(&wav);
        let (text, elapsed) = transcribe(&model, &samples, |_| {});
        assert!(!text.is_empty(), "{file}: empty text");
        println!(
            "[probe] {file}: {:.0}ms for {:.2}s",
            ms(elapsed),
            seconds(&samples)
        );
        if file != "hai.wav" {
            assert!(
                has_japanese_punctuation(&text),
                "{file}: no punctuation in {text}"
            );
        }
        report_equal(file, &text, env.cli_text(&model_path, &wav, &[]));
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Parakeet v3 Q8_0 on en.wav, and how `language` behaves

#[test]
#[ignore]
fn parakeet_en_and_language_check() {
    let env = TestEnv::load();
    let model_path = env.model(PARAKEET);
    let (model, load_time) = load(&model_path, "gpu", false);
    let (placement, threads) = model_placement(&model);
    println!(
        "[probe] Parakeet on {placement} threads={threads} loaded in {:.0}ms",
        ms(load_time)
    );
    let (takes, steers) = option_support(&model, SPEECH_OPT_LANGUAGE);
    println!("[probe] Parakeet language option: takes={takes} steers={steers}");

    let wav = env.wav("en.wav");
    let samples = read_wav(&wav);
    let (text, elapsed) = transcribe(&model, &samples, |_| {});
    assert!(!text.is_empty());
    println!("[probe] en.wav (language auto): {:.0}ms", ms(elapsed));
    report_equal("en.wav", &text, env.cli_text(&model_path, &wav, &[]));

    let (text_en, _) = transcribe(&model, &samples, |request| {
        check(
            "language=en",
            set_string(request, SPEECH_OPT_LANGUAGE, "en"),
        );
    });
    println!("[probe] en.wav (language=en): {text_en}");
    assert_eq!(text, text_en, "a checked language must not change the text");

    // Japanese is not one of parakeet-v3's languages: the setter must refuse it.
    let request = new_request(&model);
    let status = set_string(&request, SPEECH_OPT_LANGUAGE, "ja");
    println!(
        "[probe] language=ja on Parakeet: {} - {}",
        status_name(status),
        last_error()
    );
    assert_eq!(status, SPEECH_ERROR_OUT_OF_RANGE);
}

// ---------------------------------------------------------------------------------------------
// 4. Timing

#[test]
#[ignore]
fn timing_haas_gpu() {
    let env = TestEnv::load();
    let ja60 = read_wav(&env.wav("ja60.wav"));
    let clip = &ja60[..(10 * RATE as usize).min(ja60.len())];
    measure("Haas Q8_0 gpu", &env.model(HAAS), "gpu", clip, 10);
}

#[test]
#[ignore]
fn timing_haas_cpu() {
    let env = TestEnv::load();
    let ja60 = read_wav(&env.wav("ja60.wav"));
    let clip = &ja60[..(10 * RATE as usize).min(ja60.len())];
    measure("Haas Q8_0 cpu", &env.model(HAAS), "cpu", clip, 10);
}

#[test]
#[ignore]
fn timing_parakeet_gpu() {
    let env = TestEnv::load();
    let en = read_wav(&env.wav("en.wav"));
    let mut clip = en.clone();
    clip.extend_from_slice(&en);
    measure(
        "Parakeet v3 Q8_0 gpu",
        &env.model(PARAKEET),
        "gpu",
        &clip,
        10,
    );
}

// ---------------------------------------------------------------------------------------------
// 5. Cancellation from another thread

#[test]
#[ignore]
fn cancel_from_another_thread() {
    let env = TestEnv::load();
    let (model, _) = load(&env.model(HAAS), "gpu", true);
    let samples = read_wav(&env.wav("ja60.wav"));
    let request = new_request(&model);
    set_audio(&request, &samples);

    let handle = SendRequest(request.0);
    let delay = Duration::from_millis(50);
    let canceller = thread::spawn(move || {
        let handle = handle;
        thread::sleep(delay);
        // SAFETY: the request exists until the main thread drops it after joining this thread.
        unsafe { speech_request_cancel(handle.0) };
        Instant::now()
    });
    let started = Instant::now();
    // SAFETY: valid request, run by this thread alone.
    let status = unsafe { speech_transcribe(request.0) };
    let returned = Instant::now();
    let cancelled_at = canceller.join().expect("canceller thread");
    println!(
        "[probe] cancel: transcribe returned {} after {:.0}ms, {:.0}ms after the cancel call; stop={}",
        status_name(status),
        ms(returned - started),
        ms(returned.saturating_duration_since(cancelled_at)),
        result_stop(&request)
    );
    assert_eq!(status, SPEECH_CANCELLED);
    assert_eq!(result_stop(&request), "cancelled");

    // A request cancelled before it runs returns at once.
    let early = new_request(&model);
    set_audio(&early, &samples[..RATE as usize]);
    // SAFETY: valid request.
    unsafe { speech_request_cancel(early.0) };
    let started = Instant::now();
    // SAFETY: valid request.
    let status = unsafe { speech_transcribe(early.0) };
    println!(
        "[probe] cancel before run: {} in {:.2}ms",
        status_name(status),
        ms(started.elapsed())
    );
    assert_eq!(status, SPEECH_CANCELLED);
}

// ---------------------------------------------------------------------------------------------
// 6. Keywords

#[test]
#[ignore]
fn keywords_on_mixed() {
    let env = TestEnv::load();
    let (model, _) = load(&env.model(HAAS), "gpu", true);
    let (takes, steers) = option_support(&model, SPEECH_OPT_KEYWORDS);
    println!("[probe] Haas keywords option: takes={takes} steers={steers}");
    assert!(takes);
    let samples = read_wav(&env.wav("mixed.wav"));
    let (plain, plain_time) = transcribe(&model, &samples, |_| {});
    let (boosted, boosted_time) = transcribe(&model, &samples, |request| {
        check(
            "keywords",
            set_string(request, SPEECH_OPT_KEYWORDS, "Claude Code=クロードコード"),
        );
    });
    // A gentler boost than the default alpha of 2, to see whether it keeps more of the sentence.
    let (gentle, _) = transcribe(&model, &samples, |request| {
        check(
            "keywords",
            set_string(request, SPEECH_OPT_KEYWORDS, "Claude Code=クロードコード"),
        );
        set_float(request, SPEECH_OPT_KEYWORDS_ALPHA, 1.0);
    });
    println!(
        "[probe] mixed.wav without keywords ({:.0}ms): {plain}\n[probe] mixed.wav with keywords, alpha 2 ({:.0}ms): {boosted}\n[probe] mixed.wav with keywords, alpha 1: {gentle}\n[probe] keywords changed the text: {}",
        ms(plain_time),
        ms(boosted_time),
        plain != boosted
    );
    assert!(!boosted.is_empty());
}

// ---------------------------------------------------------------------------------------------
// 7. ja60: one shot against VAD regions; VAD on silence and on "はい"

#[test]
#[ignore]
fn ja60_one_shot_vs_vad_regions() {
    let env = TestEnv::load();
    let haas_path = env.model(HAAS);
    let (haas, _) = load(&haas_path, "gpu", true);
    // The CLI runs its detection model on the CPU; so does this test.
    let (vad, vad_load) = load(&env.model(SILERO), "cpu", false);
    println!("[probe] Silero loaded on cpu in {:.0}ms", ms(vad_load));

    let wav = env.wav("ja60.wav");
    let samples = read_wav(&wav);
    let (one_shot, one_shot_time) = transcribe(&haas, &samples, |_| {});
    println!(
        "[probe] ja60 one shot: {:.0}ms for {:.2}s (RTF {:.3}), {} chars\n        {one_shot}",
        ms(one_shot_time),
        seconds(&samples),
        one_shot_time.as_secs_f64() / seconds(&samples),
        one_shot.chars().count()
    );

    let (regions, detect_time) = detect(&vad, &samples, cli_vad_options);
    assert!(!regions.is_empty(), "no speech found in ja60");
    let mut joined = String::new();
    let mut region_time = Duration::ZERO;
    for &(start, end) in &regions {
        let (text, elapsed) = transcribe(&haas, slice_seconds(&samples, start, end), |_| {});
        region_time += elapsed;
        joined.push_str(&text);
    }
    println!(
        "[probe] ja60 by VAD: {} regions, detect {:.0}ms, transcribe total {:.0}ms, {} chars\n        regions: {}\n        {joined}",
        regions.len(),
        ms(detect_time),
        ms(region_time),
        joined.chars().count(),
        regions
            .iter()
            .map(|(start, end)| format!("{start:.2}-{end:.2}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "[probe] ja60 one shot == VAD joined: {}",
        one_shot == joined
    );
    if let Some(cli_vad) = env.cli_text(
        &haas_path,
        &wav,
        &["--vad", env.model(SILERO).to_str().expect("utf-8")],
    ) {
        let cli_plain = cli_vad.replace(' ', "");
        println!(
            "[probe] ja60 CLI --vad text == library VAD joined (spaces ignored): {}\n        cli: {cli_vad}",
            cli_plain == joined.replace(' ', "")
        );
    }

    let silence = read_wav(&env.wav("silence.wav"));
    let (silent_regions, _) = detect(&vad, &silence, cli_vad_options);
    println!("[probe] silence.wav regions: {silent_regions:?}");
    assert!(silent_regions.is_empty(), "silence must give no region");

    let hai = read_wav(&env.wav("hai.wav"));
    let (hai_regions, _) = detect(&vad, &hai, cli_vad_options);
    let (hai_text, _) = transcribe(&haas, &hai, |_| {});
    println!("[probe] hai.wav regions: {hai_regions:?}; Haas text: {hai_text}");
    let hai_default_regions = detect(&vad, &hai, |_| {}).0;
    println!("[probe] hai.wav regions with Silero defaults: {hai_default_regions:?}");
}

// ---------------------------------------------------------------------------------------------
// 8. Streaming detection in 480-sample pieces

#[test]
#[ignore]
fn streaming_vad_480_sample_pieces() {
    let env = TestEnv::load();
    let (vad, _) = load(&env.model(SILERO), "cpu", false);
    let samples = read_wav(&env.wav("ja.wav"));

    let (whole, _) = detect(&vad, &samples, |_| {});

    let request = new_request(&vad);
    let mut detection = std::ptr::null_mut();
    // SAFETY: a request without audio, as the header requires, and a valid out pointer.
    unsafe {
        check(
            "detection_start",
            speech_detection_start(request.0, RATE as c_int, &mut detection),
        )
    };
    let detection = Detection(detection);
    let mut given = 0;
    let mut first_region_at = None;
    let mut speaking_started_at = None;
    let started = Instant::now();
    for (index, piece) in samples.chunks(480).enumerate() {
        // SAFETY: valid detection and slice.
        unsafe {
            check(
                "detection_push",
                speech_detection_push(detection.0, piece.as_ptr(), piece.len()),
            );
            let (mut start, mut kept) = (0.0, 0);
            if speaking_started_at.is_none()
                && speech_detection_speaking(detection.0, &mut start, &mut kept) != 0
                && kept != 0
            {
                speaking_started_at = Some((index, start));
            }
            let count = speech_detection_region_count(detection.0);
            if count > given && first_region_at.is_none() {
                first_region_at = Some(index);
            }
            given = count;
        }
    }
    // SAFETY: valid detection, ended once.
    unsafe { check("detection_end", speech_detection_end(detection.0)) };
    let elapsed = started.elapsed();
    // SAFETY: every index is below the count.
    let streamed: Vec<(f64, f64)> = unsafe {
        (0..speech_detection_region_count(detection.0))
            .map(|index| {
                let (mut start, mut end) = (0.0, 0.0);
                check(
                    "detection_region",
                    speech_detection_region(detection.0, index, &mut start, &mut end),
                );
                (start, end)
            })
            .collect()
    };
    let pieces = samples.len().div_ceil(480);
    println!(
        "[probe] streaming VAD: {pieces} pieces of 30ms in {:.0}ms ({:.2}ms per piece); regions {streamed:?}\n        \
         whole-file speech_detect: {whole:?}\n        first region given after piece {:?}, speaking kept after piece {:?}",
        ms(elapsed),
        ms(elapsed) / pieces as f64,
        first_region_at,
        speaking_started_at
    );
    assert!(!streamed.is_empty(), "no region from the stream");
    assert_eq!(
        streamed.len(),
        whole.len(),
        "stream and whole-file regions differ in count"
    );
    for (stream, whole) in streamed.iter().zip(&whole) {
        assert!(
            (stream.0 - whole.0).abs() < 1e-3 && (stream.1 - whole.1).abs() < 1e-3,
            "{stream:?} != {whole:?}"
        );
    }
}
