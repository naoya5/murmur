//! Adapter: audio.
//!
//! Microphone -> 16 kHz mono f32 PCM, level meter and a crash-safe spool file.
//!
//! Threads per recording:
//! - owner: opens the device and keeps the cpal stream alive (the stream is not `Send`). The cpal callback
//!   only converts the sample format and writes into a pre-allocated lock-free ring buffer.
//! - converter: drains the ring, downmixes, resamples, appends to the PCM buffer and the spool file, and
//!   emits level events at about 30 fps.
//!
//! Debug builds accept the device name `wav:<path>`: the owner thread then feeds that WAV file into the same
//! ring buffer in real time (then silence) instead of opening a microphone.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use serde::Serialize;

/// Output sample rate of every recording (what the speech engine expects).
const TARGET_RATE: u32 = 16_000;
/// Longest time `Recorder::start` may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest time `Recorder::stop` waits for the owner thread to drop the stream.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// Ring buffer length in seconds of source audio. The converter drains every few ms, so this is slack.
const RING_SECONDS: usize = 4;
/// Spool flush interval: a crash loses at most this much audio.
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);
/// Converter poll interval when the ring is empty.
const CONVERTER_POLL: Duration = Duration::from_millis(8);
/// Level events per second.
const LEVEL_RATE: u32 = 30;
/// Resampler input chunk (frames at the source rate).
const RESAMPLE_CHUNK: usize = 1024;
/// Level meter range in dBFS that maps to 0.0..=1.0.
const LEVEL_FLOOR_DB: f32 = -55.0;
const LEVEL_CEIL_DB: f32 = -5.0;

/// Receives the samples appended to the recording, on the converter thread.
pub type PcmTap = Box<dyn Fn(&[f32]) + Send + Sync>;
/// A borrowed tap, as the converter thread holds it.
type TapRef<'a> = Option<&'a (dyn Fn(&[f32]) + Send + Sync)>;

pub struct AudioConfig {
    pub device: Option<String>,
    pub spool_path: PathBuf,
    /// Called with exactly the 16 kHz samples appended to `CapturedAudio::pcm` (and the spool),
    /// in order, as they are appended; paused audio never reaches it. An offset in the stream of
    /// calls equals the same offset in the final buffer (S1 streaming transcription).
    pub tap: Option<PcmTap>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioEvent {
    /// 0.0..=1.0, about 30 per second.
    Level(f32),
    /// The device failed or went away while recording. Sent once.
    Disconnected,
}

pub struct CapturedAudio {
    pub pcm: Vec<f32>,
    pub spool_path: PathBuf,
    pub duration_ms: u64,
}

impl std::fmt::Debug for CapturedAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedAudio")
            .field("samples", &self.pcm.len())
            .field("spool_path", &self.spool_path)
            .field("duration_ms", &self.duration_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct InputDevice {
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio input device")]
    NoDevice,
    #[error("opening the audio device timed out")]
    OpenTimeout,
    #[error("audio device error: {0}")]
    Device(String),
    #[error("audio file error: {0}")]
    Io(String),
}

fn device_err(e: &cpal::Error) -> AudioError {
    match e.kind() {
        cpal::ErrorKind::DeviceNotAvailable => AudioError::NoDevice,
        _ => AudioError::Device(e.to_string()),
    }
}

/// State shared between the Recorder handle, the cpal callbacks and the threads.
struct Shared {
    pcm: Mutex<Option<Vec<f32>>>,
    paused: AtomicBool,
    stop_owner: AtomicBool,
    stop_converter: AtomicBool,
    /// Set by the stream error callback (or never, for the WAV device).
    failed: AtomicBool,
    /// Microseconds from `start` to the first callback; `u64::MAX` until it happens.
    first_callback_us: AtomicU64,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            pcm: Mutex::new(Some(Vec::new())),
            paused: AtomicBool::new(false),
            stop_owner: AtomicBool::new(false),
            stop_converter: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            first_callback_us: AtomicU64::new(u64::MAX),
        }
    }

    /// Lock-free and allocation-free: safe to call from the audio callback.
    fn mark_first_callback(&self, start: Instant) {
        if self.first_callback_us.load(Ordering::Relaxed) == u64::MAX {
            let us = start.elapsed().as_micros() as u64;
            self.first_callback_us.store(us, Ordering::Relaxed);
        }
    }
}

/// What the owner thread reports once the source is running.
struct Opened {
    cons: HeapCons<f32>,
    rate: u32,
    channels: usize,
}

/// Handle to a running recording. `Send + Sync`: the cpal stream lives on the owner thread.
pub struct Recorder {
    shared: Arc<Shared>,
    owner: Option<JoinHandle<()>>,
    owner_done: mpsc::Receiver<()>,
    converter_done: mpsc::Receiver<Result<(), AudioError>>,
    spool_path: PathBuf,
}

impl Recorder {
    /// Opens the device (or the system default; falls back to default if the named one is gone) and starts.
    /// Returns within 3 s (`AudioError::OpenTimeout`). Does NOT check permissions (the controller does, G8).
    pub fn start(
        cfg: AudioConfig,
        on_event: Box<dyn Fn(AudioEvent) + Send + Sync>,
    ) -> Result<Recorder, AudioError> {
        let started = Instant::now();
        let AudioConfig {
            device,
            spool_path,
            tap,
        } = cfg;
        if let Some(parent) = spool_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AudioError::Io(e.to_string()))?;
        }
        let spool = File::create(&spool_path).map_err(|e| AudioError::Io(e.to_string()))?;

        let shared = Arc::new(Shared::new());
        let (open_tx, open_rx) = mpsc::channel::<Result<Opened, AudioError>>();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let owner = {
            let shared = Arc::clone(&shared);
            let device = device.clone();
            thread::Builder::new()
                .name("murmur-audio-owner".into())
                .spawn(move || {
                    owner_main(device, &shared, started, &open_tx);
                    let _ = done_tx.send(());
                })
                .map_err(|e| AudioError::Device(e.to_string()))?
        };

        let opened = match open_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(opened)) => opened,
            Ok(Err(e)) => {
                let _ = std::fs::remove_file(&spool_path);
                return Err(e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The owner thread may still be inside the OS call. Tell it to quit when it returns.
                shared.stop_owner.store(true, Ordering::Release);
                owner.thread().unpark();
                let _ = std::fs::remove_file(&spool_path);
                return Err(AudioError::OpenTimeout);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = std::fs::remove_file(&spool_path);
                return Err(AudioError::Device("audio thread ended during open".into()));
            }
        };

        let (converter_tx, converter_done) = mpsc::channel();
        {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("murmur-audio-converter".into())
                .spawn(move || {
                    let result =
                        converter_main(opened, &shared, spool, on_event.as_ref(), tap.as_deref());
                    let _ = converter_tx.send(result);
                })
                .map_err(|e| AudioError::Device(e.to_string()))?
        };

        Ok(Recorder {
            shared,
            owner: Some(owner),
            owner_done: done_rx,
            converter_done,
            spool_path,
        })
    }

    /// Paused audio is dropped (not in the PCM, not in the spool). The stream stays open.
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    /// Microseconds from `start` to the first audio callback, once it happened.
    pub fn first_callback_micros(&self) -> Option<u64> {
        match self.shared.first_callback_us.load(Ordering::Relaxed) {
            u64::MAX => None,
            us => Some(us),
        }
    }

    /// Closes the stream and flushes the spool. Returns what was captured, also after a disconnect.
    pub fn stop(mut self) -> Result<CapturedAudio, AudioError> {
        self.shared.stop_owner.store(true, Ordering::Release);
        if let Some(owner) = self.owner.take() {
            owner.thread().unpark();
            // Join only when the owner confirmed; a stuck OS call must not hang the caller (G5).
            if self.owner_done.recv_timeout(CLOSE_TIMEOUT).is_ok() {
                let _ = owner.join();
            }
        }
        // Everything the stream produced is in the ring now; the converter drains it and finishes.
        self.shared.stop_converter.store(true, Ordering::Release);
        let pcm = finish_converter(&self.shared, &self.converter_done, CLOSE_TIMEOUT)?;
        let duration_ms = pcm.len() as u64 * 1000 / u64::from(TARGET_RATE);
        Ok(CapturedAudio {
            pcm,
            spool_path: self.spool_path.clone(),
            duration_ms,
        })
    }
}

/// A timed-out converter retains only its own file handle. Taking the buffer fences all later
/// appends and callbacks; it cannot publish into this recording or a subsequent session.
fn finish_converter(
    shared: &Shared,
    done: &mpsc::Receiver<Result<(), AudioError>>,
    timeout: Duration,
) -> Result<Vec<f32>, AudioError> {
    match done.recv_timeout(timeout) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!("audio converter did not stop in time; using captured audio");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(AudioError::Device("audio converter thread ended".into()));
        }
    }
    Ok(shared
        .pcm
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_default())
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Dropped without `stop`: let both threads wind down by themselves.
        self.shared.stop_owner.store(true, Ordering::Release);
        self.shared.stop_converter.store(true, Ordering::Release);
        if let Some(owner) = &self.owner {
            owner.thread().unpark();
        }
    }
}

/// Lists input devices by name. Empty when the host cannot enumerate them.
pub fn input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.description().ok().map(|d| d.name().to_owned()));
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut out: Vec<InputDevice> = Vec::new();
    for device in devices {
        let Ok(desc) = device.description() else {
            continue;
        };
        let name = desc.name().to_owned();
        if out.iter().any(|d| d.name == name) {
            continue;
        }
        let is_default = default_name.as_deref() == Some(name.as_str());
        out.push(InputDevice { name, is_default });
    }
    out
}

/// Reads a spool file (headerless 16 kHz mono s16le) back into f32 samples.
pub fn read_spool(path: &Path) -> Result<Vec<f32>, AudioError> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| AudioError::Io(e.to_string()))?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Owner thread: device or WAV source.
// ---------------------------------------------------------------------------------------------

fn owner_main(
    device: Option<String>,
    shared: &Arc<Shared>,
    started: Instant,
    open_tx: &mpsc::Sender<Result<Opened, AudioError>>,
) {
    #[cfg(debug_assertions)]
    if let Some(path) = device.as_deref().and_then(|d| d.strip_prefix("wav:")) {
        wav_source_main(Path::new(path), shared, started, open_tx);
        return;
    }
    match open_device(device.as_deref(), shared, started) {
        Ok((stream, opened)) => {
            if open_tx.send(Ok(opened)).is_err() {
                return; // `start` gave up (timeout); drop the stream.
            }
            park_until_stopped(shared);
            drop(stream);
        }
        Err(e) => {
            let _ = open_tx.send(Err(e));
        }
    }
}

fn park_until_stopped(shared: &Shared) {
    while !shared.stop_owner.load(Ordering::Acquire) {
        thread::park_timeout(Duration::from_millis(50));
    }
}

fn select_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device, AudioError> {
    if let Some(name) = name
        && let Ok(devices) = host.input_devices()
    {
        for device in devices {
            if device.description().is_ok_and(|d| d.name() == name) {
                return Ok(device);
            }
        }
        // The named device is gone: fall back to the default.
    }
    host.default_input_device().ok_or(AudioError::NoDevice)
}

fn open_device(
    name: Option<&str>,
    shared: &Arc<Shared>,
    started: Instant,
) -> Result<(Stream, Opened), AudioError> {
    let host = cpal::default_host();
    let device = select_device(&host, name)?;
    let supported = device.default_input_config().map_err(|e| device_err(&e))?;
    let format = supported.sample_format();
    let rate = supported.sample_rate();
    let channels = usize::from(supported.channels());
    if channels == 0 || rate == 0 {
        return Err(AudioError::Device("device reports no channels".into()));
    }
    let config = supported.config();
    let (prod, cons) = HeapRb::<f32>::new(rate as usize * channels * RING_SECONDS).split();
    let shared = Arc::clone(shared);
    let stream = match format {
        SampleFormat::F32 => build_stream::<f32>(&device, config, prod, shared, started, channels),
        SampleFormat::I16 => build_stream::<i16>(&device, config, prod, shared, started, channels),
        SampleFormat::I32 => build_stream::<i32>(&device, config, prod, shared, started, channels),
        SampleFormat::U16 => build_stream::<u16>(&device, config, prod, shared, started, channels),
        SampleFormat::I8 => build_stream::<i8>(&device, config, prod, shared, started, channels),
        SampleFormat::U8 => build_stream::<u8>(&device, config, prod, shared, started, channels),
        other => {
            return Err(AudioError::Device(format!(
                "unsupported sample format {other:?}"
            )));
        }
    }
    .map_err(|e| device_err(&e))?;
    stream.play().map_err(|e| device_err(&e))?;
    Ok((
        stream,
        Opened {
            cons,
            rate,
            channels,
        },
    ))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut prod: HeapProd<f32>,
    shared: Arc<Shared>,
    started: Instant,
    channels: usize,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let err_shared = Arc::clone(&shared);
    device.build_input_stream(
        config,
        // Real-time callback: no locks, no allocation, no logging.
        move |data: &[T], _| {
            shared.mark_first_callback(started);
            if shared.paused.load(Ordering::Relaxed) {
                return;
            }
            push_converted(&mut prod, data, channels);
        },
        move |e: cpal::Error| {
            if e.kind() != cpal::ErrorKind::DeviceChanged {
                err_shared.failed.store(true, Ordering::Release);
            }
        },
        Some(Duration::from_secs(2)),
    )
}

/// Converts to f32 through a stack buffer and pushes whole frames only. Drops audio if the ring is full.
fn push_converted<T>(prod: &mut HeapProd<f32>, data: &[T], channels: usize)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut buf = [0.0_f32; 1024];
    let step = (buf.len() / channels).max(1) * channels;
    if step > buf.len() {
        return; // More channels than the stack buffer holds; not a real device.
    }
    for chunk in data.chunks(step) {
        for (dst, src) in buf.iter_mut().zip(chunk) {
            *dst = f32::from_sample(*src);
        }
        push_frames(prod, &buf[..chunk.len()], channels);
    }
}

fn push_frames(prod: &mut HeapProd<f32>, data: &[f32], channels: usize) {
    let n = data.len().min(prod.vacant_len());
    let n = n - n % channels;
    if n > 0 {
        prod.push_slice(&data[..n]);
    }
}

/// Debug-only source: plays a WAV file into the ring in real time, then silence until stopped.
#[cfg(debug_assertions)]
fn wav_source_main(
    path: &Path,
    shared: &Arc<Shared>,
    started: Instant,
    open_tx: &mpsc::Sender<Result<Opened, AudioError>>,
) {
    let (samples, rate, channels) = match read_wav(path) {
        Ok(wav) => wav,
        Err(e) => {
            let _ = open_tx.send(Err(e));
            return;
        }
    };
    let (mut prod, cons) = HeapRb::<f32>::new(rate as usize * channels * RING_SECONDS).split();
    if open_tx
        .send(Ok(Opened {
            cons,
            rate,
            channels,
        }))
        .is_err()
    {
        return;
    }
    let total_frames = samples.len() / channels;
    let silence = vec![0.0_f32; rate as usize / 10 * channels];
    let clock = Instant::now();
    let mut delivered: usize = 0; // frames handed out so far (file, then silence)
    while !shared.stop_owner.load(Ordering::Acquire) {
        thread::park_timeout(Duration::from_millis(10));
        shared.mark_first_callback(started);
        let due = (clock.elapsed().as_secs_f64() * f64::from(rate)) as usize;
        while delivered < due {
            if shared.stop_owner.load(Ordering::Acquire) {
                return;
            }
            let remaining = due - delivered;
            if delivered < total_frames {
                let n = remaining.min(total_frames - delivered);
                if !shared.paused.load(Ordering::Relaxed) {
                    push_frames(
                        &mut prod,
                        &samples[delivered * channels..(delivered + n) * channels],
                        channels,
                    );
                }
                delivered += n;
            } else {
                let n = remaining.min(silence.len() / channels);
                if !shared.paused.load(Ordering::Relaxed) {
                    push_frames(&mut prod, &silence[..n * channels], channels);
                }
                delivered += n;
            }
        }
    }
}

/// Reads a WAV of any rate / channel count / sample type as interleaved f32.
#[cfg(debug_assertions)]
fn read_wav(path: &Path) -> Result<(Vec<f32>, u32, usize), AudioError> {
    let io = |e: hound::Error| AudioError::Io(e.to_string());
    let mut reader = hound::WavReader::open(path).map_err(io)?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels);
    if channels == 0 || spec.sample_rate == 0 {
        return Err(AudioError::Io("invalid WAV header".into()));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(io)?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1_u64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()
                .map_err(io)?
        }
    };
    Ok((samples, spec.sample_rate, channels))
}

// ---------------------------------------------------------------------------------------------
// Converter thread.
// ---------------------------------------------------------------------------------------------

fn converter_main(
    opened: Opened,
    shared: &Shared,
    spool: File,
    on_event: &(dyn Fn(AudioEvent) + Send + Sync),
    tap: TapRef<'_>,
) -> Result<(), AudioError> {
    let Opened {
        mut cons,
        rate,
        channels,
    } = opened;
    let mut converter = Converter::new(rate, channels)?;
    let mut meter = LevelMeter::new();
    let mut spool = BufWriter::new(spool);
    let mut raw = vec![0.0_f32; 8192 - 8192 % channels];
    let mut converted: Vec<f32> = Vec::with_capacity(4096);
    let mut last_flush = Instant::now();
    let mut disconnected_sent = false;
    let mut first_error: Option<AudioError> = None;

    loop {
        if shared
            .pcm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
        {
            return Ok(());
        }
        // Read the flag before draining: the owner is already gone when it is set, so one more drain is final.
        let stopping = shared.stop_converter.load(Ordering::Acquire);
        loop {
            let available = cons.occupied_len();
            let n = available.min(raw.len());
            let n = n - n % channels;
            if n == 0 {
                break;
            }
            let popped = cons.pop_slice(&mut raw[..n]);
            converted.clear();
            if let Err(e) = converter.push(&raw[..popped], &mut converted) {
                first_error.get_or_insert(e);
            }
            append(
                &converted,
                shared,
                &mut spool,
                &mut meter,
                on_event,
                tap,
                &mut first_error,
            );
        }
        if shared.failed.load(Ordering::Acquire)
            && !disconnected_sent
            && shared
                .pcm
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        {
            disconnected_sent = true;
            on_event(AudioEvent::Disconnected);
        }
        if last_flush.elapsed() >= FLUSH_INTERVAL {
            if let Err(e) = spool.flush() {
                first_error.get_or_insert(AudioError::Io(e.to_string()));
            }
            last_flush = Instant::now();
        }
        if stopping {
            break;
        }
        thread::sleep(CONVERTER_POLL);
    }

    converted.clear();
    if let Err(e) = converter.finish(&mut converted) {
        first_error.get_or_insert(e);
    }
    append(
        &converted,
        shared,
        &mut spool,
        &mut meter,
        on_event,
        tap,
        &mut first_error,
    );
    if let Err(e) = spool.flush() {
        first_error.get_or_insert(AudioError::Io(e.to_string()));
    }
    let pcm = shared
        .pcm
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match first_error {
        // Keep the audio captured so far when only the spool failed; report hard errors otherwise.
        Some(e) if pcm.as_ref().is_none_or(Vec::is_empty) => Err(e),
        _ => Ok(()),
    }
}

/// Appends converted 16 kHz samples to the PCM buffer and the spool, feeds the level meter and
/// the tap.
#[allow(clippy::too_many_arguments)]
fn append(
    converted: &[f32],
    shared: &Shared,
    spool: &mut impl Write,
    meter: &mut LevelMeter,
    on_event: &(dyn Fn(AudioEvent) + Send + Sync),
    tap: TapRef<'_>,
    first_error: &mut Option<AudioError>,
) {
    if converted.is_empty() {
        return;
    }
    {
        let mut captured = shared
            .pcm
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pcm) = captured.as_mut() else {
            return;
        };
        pcm.extend_from_slice(converted);
    }
    if let Some(tap) = tap {
        tap(converted);
    }
    let bytes: Vec<u8> = converted
        .iter()
        .flat_map(|&s| to_s16(s).to_le_bytes())
        .collect();
    if let Err(e) = spool.write_all(&bytes) {
        first_error.get_or_insert(AudioError::Io(e.to_string()));
    }
    if shared
        .pcm
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
    {
        meter.feed(converted, |level| on_event(AudioEvent::Level(level)));
    }
}

fn to_s16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

// ---------------------------------------------------------------------------------------------
// Pure helpers (unit tested).
// ---------------------------------------------------------------------------------------------

/// Averages interleaved channels into mono and appends to `out`.
fn downmix_into(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels == 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let inv = 1.0 / channels as f32;
    out.extend(
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() * inv),
    );
}

/// Source rate / channels -> 16 kHz mono. Streaming: `push` any number of whole frames, then `finish`.
struct Converter {
    channels: usize,
    source_rate: u32,
    resampler: Option<Fft<f32>>,
    /// Mono samples at the source rate waiting for a full resampler chunk.
    pending: Vec<f32>,
    out_buf: Vec<f32>,
    /// Output frames still to drop (the resampler's start-up delay).
    delay_left: usize,
    in_frames: u64,
    out_frames: u64,
}

impl Converter {
    fn new(source_rate: u32, channels: usize) -> Result<Converter, AudioError> {
        if channels == 0 || source_rate == 0 {
            return Err(AudioError::Device("invalid source format".into()));
        }
        let (resampler, out_buf, delay_left) = if source_rate == TARGET_RATE {
            (None, Vec::new(), 0)
        } else {
            let rs = Fft::<f32>::new(
                source_rate as usize,
                TARGET_RATE as usize,
                RESAMPLE_CHUNK,
                1,
                FixedSync::Input,
            )
            .map_err(|e| AudioError::Device(e.to_string()))?;
            let buf = vec![0.0_f32; rs.output_frames_max()];
            let delay = rs.output_delay();
            (Some(rs), buf, delay)
        };
        Ok(Converter {
            channels,
            source_rate,
            resampler,
            pending: Vec::new(),
            out_buf,
            delay_left,
            in_frames: 0,
            out_frames: 0,
        })
    }

    /// Takes interleaved source samples (whole frames) and appends finished 16 kHz mono samples to `out`.
    fn push(&mut self, interleaved: &[f32], out: &mut Vec<f32>) -> Result<(), AudioError> {
        let frames = interleaved.len() / self.channels;
        self.in_frames += frames as u64;
        let Some(rs) = self.resampler.as_mut() else {
            downmix_into(interleaved, self.channels, out);
            self.out_frames += frames as u64;
            return Ok(());
        };
        downmix_into(interleaved, self.channels, &mut self.pending);
        loop {
            let need = rs.input_frames_next();
            if self.pending.len() < need {
                return Ok(());
            }
            let input = InterleavedSlice::new(&self.pending[..need], 1, need)
                .map_err(|e| AudioError::Device(e.to_string()))?;
            let cap = self.out_buf.len();
            let mut output = InterleavedSlice::new_mut(&mut self.out_buf, 1, cap)
                .map_err(|e| AudioError::Device(e.to_string()))?;
            let (read, written) = rs
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| AudioError::Device(e.to_string()))?;
            emit(
                &self.out_buf[..written],
                &mut self.delay_left,
                &mut self.out_frames,
                out,
            );
            self.pending.drain(..read);
        }
    }

    /// Flushes the resampler tail so the output length matches the input duration.
    fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), AudioError> {
        let Some(rs) = self.resampler.as_mut() else {
            return Ok(());
        };
        let expected = (self.in_frames * u64::from(TARGET_RATE) + u64::from(self.source_rate) / 2)
            / u64::from(self.source_rate);
        let limit = out.len() + expected.saturating_sub(self.out_frames) as usize;
        // Bounded: the tail is only a few chunks long.
        for _ in 0..8 {
            if self.out_frames >= expected {
                break;
            }
            let need = rs.input_frames_next();
            let partial = self.pending.len().min(need);
            let mut padded = vec![0.0_f32; need];
            padded[..partial].copy_from_slice(&self.pending[..partial]);
            let input = InterleavedSlice::new(&padded, 1, need)
                .map_err(|e| AudioError::Device(e.to_string()))?;
            let cap = self.out_buf.len();
            let mut output = InterleavedSlice::new_mut(&mut self.out_buf, 1, cap)
                .map_err(|e| AudioError::Device(e.to_string()))?;
            let indexing = Indexing {
                partial_len: Some(partial),
                ..Indexing::default()
            };
            let (_, written) = rs
                .process_into_buffer(&input, &mut output, Some(&indexing))
                .map_err(|e| AudioError::Device(e.to_string()))?;
            emit(
                &self.out_buf[..written],
                &mut self.delay_left,
                &mut self.out_frames,
                out,
            );
            self.pending.drain(..partial);
        }
        out.truncate(limit);
        Ok(())
    }
}

/// Appends resampler output after dropping the start-up delay.
fn emit(produced: &[f32], delay_left: &mut usize, out_frames: &mut u64, out: &mut Vec<f32>) {
    let skip = (*delay_left).min(produced.len());
    *delay_left -= skip;
    let kept = &produced[skip..];
    out.extend_from_slice(kept);
    *out_frames += kept.len() as u64;
}

/// RMS level meter over windows of 1/30 s of 16 kHz audio.
struct LevelMeter {
    window: usize,
    sum_sq: f64,
    count: usize,
}

impl LevelMeter {
    fn new() -> LevelMeter {
        LevelMeter {
            window: (TARGET_RATE / LEVEL_RATE) as usize,
            sum_sq: 0.0,
            count: 0,
        }
    }

    fn feed(&mut self, samples: &[f32], mut emit: impl FnMut(f32)) {
        for &s in samples {
            self.sum_sq += f64::from(s) * f64::from(s);
            self.count += 1;
            if self.count == self.window {
                let rms = (self.sum_sq / self.count as f64).sqrt() as f32;
                emit(level_from_rms(rms));
                self.sum_sq = 0.0;
                self.count = 0;
            }
        }
    }
}

/// Maps RMS to 0..=1 on a dB scale (perceptual): -55 dBFS or lower is 0, -5 dBFS or higher is 1.
fn level_from_rms(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - LEVEL_FLOOR_DB) / (LEVEL_CEIL_DB - LEVEL_FLOOR_DB)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(debug_assertions)]
    use std::sync::Mutex;

    fn sine(rate: u32, hz: f32, secs: f32, amp: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Frequency from upward zero crossings.
    fn estimate_hz(x: &[f32], rate: u32) -> f32 {
        let crossings = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        crossings as f32 * rate as f32 / x.len() as f32
    }

    fn convert_all(rate: u32, channels: usize, input: &[f32], block: usize) -> Vec<f32> {
        let mut c = Converter::new(rate, channels).unwrap();
        let mut out = Vec::new();
        for chunk in input.chunks(block * channels) {
            c.push(chunk, &mut out).unwrap();
        }
        c.finish(&mut out).unwrap();
        out
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("murmur-audio-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn converter_timeout_returns_snapshot_and_fences_late_appends() {
        struct StalledWriter {
            file: File,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Write for StalledWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(3)).unwrap();
                self.file.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.file.flush()
            }
        }
        let shared = Arc::new(Shared::new());
        let (done_tx, done) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        let (late_tx, late) = mpsc::channel();
        let worker_shared = shared.clone();
        let path = temp_dir("converter-timeout").join("rec.pcm");
        let mut spool = StalledWriter {
            file: File::create(&path).unwrap(),
            entered: entered_tx,
            release,
        };
        let worker = thread::spawn(move || {
            let mut meter = LevelMeter::new();
            let mut error = None;
            append(
                &[0.25; 1600],
                &worker_shared,
                &mut spool,
                &mut meter,
                &|_| late_tx.send(()).unwrap(),
                None,
                &mut error,
            );
            append(
                &[0.5; 1600],
                &worker_shared,
                &mut spool,
                &mut meter,
                &|_| late_tx.send(()).unwrap(),
                Some(&|_| panic!("late tap")),
                &mut error,
            );
            spool.flush().unwrap();
            done_tx.send(Ok(())).unwrap();
        });
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let started = Instant::now();
        let pcm = finish_converter(&shared, &done, Duration::from_millis(20)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(pcm, vec![0.25; 1600]);
        assert!(shared.pcm.lock().unwrap().is_none());
        // Cleanup can unlink the old spool safely even if the detached write returns later.
        std::fs::remove_file(&path).unwrap();
        release_tx.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
        worker.join().unwrap();
        assert!(late.try_recv().is_err());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn resample_keeps_frequency_and_energy() {
        for rate in [48_000_u32, 44_100] {
            let input = sine(rate, 1000.0, 2.0, 0.5);
            let out = convert_all(rate, 1, &input, 480);
            assert_eq!(out.len(), 32_000, "rate {rate}");
            let mid = &out[2000..out.len() - 2000];
            let hz = estimate_hz(mid, TARGET_RATE);
            assert!((hz - 1000.0).abs() < 10.0, "rate {rate}: {hz} Hz");
            let expected = 0.5 / 2.0_f32.sqrt();
            assert!(
                (rms(mid) - expected).abs() < expected * 0.03,
                "rate {rate}: rms {}",
                rms(mid)
            );
        }
    }

    #[test]
    fn resample_output_is_aligned_with_input() {
        // A sine keeps its phase: the first output samples must not be the resampler's start-up silence.
        let input = sine(48_000, 500.0, 0.5, 0.8);
        let out = convert_all(48_000, 1, &input, 1000);
        let expected = sine(16_000, 500.0, 0.5, 0.8);
        let n = expected.len().min(out.len());
        let err = rms(&out[200..n - 200]
            .iter()
            .zip(&expected[200..n - 200])
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>());
        assert!(err < 0.02, "alignment error {err}");
    }

    #[test]
    fn resample_is_independent_of_block_size() {
        let input = sine(48_000, 700.0, 1.0, 0.4);
        let a = convert_all(48_000, 1, &input, 480);
        let b = convert_all(48_000, 1, &input, 777);
        assert_eq!(a.len(), b.len());
        let diff = a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max);
        assert!(diff < 1e-4, "max diff {diff}");
    }

    #[test]
    fn downmix_averages_channels() {
        let mut out = Vec::new();
        downmix_into(&[1.0, -1.0, 0.5, 0.5, 0.2, 0.4], 2, &mut out);
        assert_eq!(out, vec![0.0, 0.5, 0.3]);
        let stereo16k = convert_all(16_000, 2, &[0.5, 0.1, -0.5, -0.1], 2);
        assert_eq!(stereo16k.len(), 2);
        assert!((stereo16k[0] - 0.3).abs() < 1e-6 && (stereo16k[1] + 0.3).abs() < 1e-6);
        let six = convert_all(16_000, 6, &[1.0; 12], 2);
        assert_eq!(six, vec![1.0, 1.0]);
    }

    #[test]
    fn level_curve() {
        assert_eq!(level_from_rms(0.0), 0.0);
        assert_eq!(level_from_rms(1e-5), 0.0);
        assert_eq!(level_from_rms(1.0), 1.0);
        let speech = level_from_rms(0.03); // about -30 dBFS
        assert!(speech > 0.3 && speech < 0.7, "{speech}");
        let mut meter = LevelMeter::new();
        let mut levels = Vec::new();
        meter.feed(&vec![0.5; 16_000], |l| levels.push(l));
        assert_eq!(levels.len(), 30);
    }

    #[test]
    fn spool_round_trip() {
        let dir = temp_dir("spool");
        let path = dir.join("a.pcm");
        let samples = sine(16_000, 300.0, 0.3, 0.7);
        let mut w = BufWriter::new(File::create(&path).unwrap());
        for &s in &samples {
            w.write_all(&to_s16(s).to_le_bytes()).unwrap();
        }
        w.flush().unwrap();
        let back = read_spool(&path).unwrap();
        assert_eq!(back.len(), samples.len());
        assert!(
            samples
                .iter()
                .zip(&back)
                .all(|(a, b)| (a - b).abs() < 1.0 / 16_384.0)
        );
        assert!(matches!(
            read_spool(&dir.join("missing.pcm")),
            Err(AudioError::Io(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn input_devices_does_not_panic() {
        let devices = input_devices();
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
    }

    #[cfg(debug_assertions)]
    fn write_wav(path: &Path, rate: u32, channels: u16, samples: &[f32]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample(to_s16(s)).unwrap();
        }
        w.finalize().unwrap();
    }

    #[cfg(debug_assertions)]
    fn record_wav(wav: &Path, spool: &Path, run: Duration) -> (CapturedAudio, Vec<AudioEvent>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let rec = Recorder::start(
            AudioConfig {
                device: Some(format!("wav:{}", wav.display())),
                spool_path: spool.to_path_buf(),
                tap: None,
            },
            Box::new(move |e| sink.lock().unwrap().push(e)),
        )
        .unwrap();
        thread::sleep(run);
        let audio = rec.stop().unwrap();
        let events = events.lock().unwrap().clone();
        (audio, events)
    }

    #[cfg(debug_assertions)]
    #[test]
    fn wav_device_end_to_end_stereo_48k() {
        let dir = temp_dir("wav48");
        let wav = dir.join("in.wav");
        let tone = sine(48_000, 1000.0, 1.0, 0.5);
        let stereo: Vec<f32> = tone.iter().flat_map(|&s| [s, s]).collect();
        write_wav(&wav, 48_000, 2, &stereo);

        let spool = dir.join("spool").join("rec.pcm");
        let (audio, events) = record_wav(&wav, &spool, Duration::from_millis(1500));

        assert!(
            (1400..=1650).contains(&audio.duration_ms),
            "duration {} ms",
            audio.duration_ms
        );
        assert_eq!(audio.spool_path, spool);
        // The first second holds the tone, the rest is silence.
        let tone_part = &audio.pcm[1600..14_400];
        let expected = 0.5 / 2.0_f32.sqrt();
        assert!(
            (rms(tone_part) - expected).abs() < expected * 0.05,
            "rms {}",
            rms(tone_part)
        );
        assert!((estimate_hz(tone_part, TARGET_RATE) - 1000.0).abs() < 15.0);
        assert!(rms(&audio.pcm[17_000..]) < 0.01, "tail is not silent");
        // Spool equals the PCM within s16 quantisation.
        let spooled = read_spool(&spool).unwrap();
        assert_eq!(spooled.len(), audio.pcm.len());
        assert!(
            spooled
                .iter()
                .zip(&audio.pcm)
                .all(|(a, b)| (a - b).abs() < 1.0 / 16_384.0)
        );
        // About 30 level events per second, loud while the tone plays.
        let levels: Vec<f32> = events
            .iter()
            .filter_map(|e| {
                if let AudioEvent::Level(l) = e {
                    Some(*l)
                } else {
                    None
                }
            })
            .collect();
        assert!((38..=52).contains(&levels.len()), "{} levels", levels.len());
        assert!(levels.iter().all(|l| (0.0..=1.0).contains(l)));
        assert!(levels.iter().copied().fold(0.0, f32::max) > 0.5);
        assert!(!events.contains(&AudioEvent::Disconnected));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn wav_device_16k_mono_is_passed_through_exactly() {
        let dir = temp_dir("wav16");
        let wav = dir.join("in.wav");
        let tone = sine(16_000, 440.0, 0.5, 0.6);
        write_wav(&wav, 16_000, 1, &tone);
        let (audio, _) = record_wav(&wav, &dir.join("rec.pcm"), Duration::from_millis(800));
        assert!(audio.pcm.len() >= tone.len());
        // The WAV is 16-bit, so the source itself is quantised; the converter must not change it further.
        let quantised: Vec<f32> = tone
            .iter()
            .map(|&s| f32::from(to_s16(s)) / 32768.0)
            .collect();
        assert!(
            quantised
                .iter()
                .zip(&audio.pcm)
                .all(|(a, b)| (a - b).abs() < 1.0 / 16_384.0)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn paused_audio_is_dropped() {
        let dir = temp_dir("pause");
        let wav = dir.join("in.wav");
        write_wav(&wav, 16_000, 1, &sine(16_000, 440.0, 2.0, 0.6));
        let rec = Recorder::start(
            AudioConfig {
                device: Some(format!("wav:{}", wav.display())),
                spool_path: dir.join("rec.pcm"),
                tap: None,
            },
            Box::new(|_| {}),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        rec.set_paused(true);
        thread::sleep(Duration::from_millis(600));
        rec.set_paused(false);
        thread::sleep(Duration::from_millis(300));
        let audio = rec.stop().unwrap();
        assert!(
            (450..=800).contains(&audio.duration_ms),
            "duration {} ms",
            audio.duration_ms
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// S1: the tap receives exactly the samples of the final buffer, paused audio excluded.
    #[cfg(debug_assertions)]
    #[test]
    fn tap_sees_exactly_the_final_pcm() {
        let dir = temp_dir("tap");
        let wav = dir.join("in.wav");
        write_wav(&wav, 16_000, 1, &sine(16_000, 440.0, 2.0, 0.6));
        let tapped: Arc<std::sync::Mutex<Vec<f32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&tapped);
        let rec = Recorder::start(
            AudioConfig {
                device: Some(format!("wav:{}", wav.display())),
                spool_path: dir.join("rec.pcm"),
                tap: Some(Box::new(move |pcm| {
                    sink.lock().unwrap().extend_from_slice(pcm)
                })),
            },
            Box::new(|_| {}),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        rec.set_paused(true);
        thread::sleep(Duration::from_millis(400));
        rec.set_paused(false);
        thread::sleep(Duration::from_millis(300));
        let audio = rec.stop().unwrap();
        let tapped = tapped.lock().unwrap();
        assert!(!audio.pcm.is_empty());
        assert_eq!(*tapped, audio.pcm, "tap and buffer differ");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn unreadable_wav_is_an_io_error() {
        let dir = temp_dir("badwav");
        let result = Recorder::start(
            AudioConfig {
                device: Some("wav:/nonexistent/file.wav".into()),
                spool_path: dir.join("rec.pcm"),
                tap: None,
            },
            Box::new(|_| {}),
        );
        assert!(matches!(result, Err(AudioError::Io(_))));
        assert!(!dir.join("rec.pcm").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Needs microphone permission for the terminal. Prints the start -> first callback time.
    #[test]
    #[ignore = "needs a real microphone and permission"]
    fn real_microphone() {
        let dir = temp_dir("mic");
        for attempt in 0..3 {
            let t0 = Instant::now();
            let rec = Recorder::start(
                AudioConfig {
                    device: None,
                    spool_path: dir.join("mic.pcm"),
                    tap: None,
                },
                Box::new(|_| {}),
            )
            .unwrap();
            let open = t0.elapsed();
            thread::sleep(Duration::from_millis(600));
            let first = rec.first_callback_micros();
            let audio = rec.stop().unwrap();
            println!(
                "attempt {attempt}: start() returned in {open:?}, first callback at {first:?} us, {} ms captured",
                audio.duration_ms
            );
            assert!(first.is_some(), "no audio callback arrived");
            assert!(audio.duration_ms > 300);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
