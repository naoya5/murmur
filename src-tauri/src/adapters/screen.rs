//! Adapter: pictures of the screen for the annotations, through ScreenCaptureKit. This is the
//! only file that uses ScreenCaptureKit (guardrail G2).
//!
//! Captures the display under the cursor at 2 fps with the long edge at 1568 px, cursor shown,
//! Murmur's own windows excluded. Each picture becomes numbers for the core (the 16 x 9 cell
//! brightness) at once; the pixels of the last four pictures stay in memory so that the
//! controller can keep one as a candidate PNG when the core asks for it (`keep_frame`). The
//! screen sends no picture while nothing changes, so every frame carries its arrival time.
//! Debug builds can play a folder of PNGs instead of the screen (`ScreenSource::PngDir`), through
//! the same picture path. Pictures never reach the log (G7). Every wait has a timeout (G5).
//! Nothing panics (G6).

use std::collections::VecDeque;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use murmur_core::annotations::{GRID_CELLS, GRID_COLUMNS, GRID_ROWS};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayPixelsWide, CGGetDisplaysWithPoint,
    CGMainDisplayID,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use super::image::{self, Bitmap};
use super::pointer::DisplayRect;

/// Long edge of a captured picture, in pixels (the size the contact sheet uses as is).
pub const LONG_EDGE_PX: usize = 1568;
pub const FRAMES_PER_SECOND: i32 = 2;
/// Pictures kept in memory: two seconds, enough to reach back to "the picture before the click".
pub const RING_FRAMES: usize = 4;
/// Starting the capture (content list plus first frame) must finish within this (spec 9.12).
pub const START_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
/// Mode of the candidates folder.
const FOLDER_MODE: u32 = 0o700;

#[derive(Debug, thiserror::Error)]
pub enum ScreenError {
    #[error("screen recording permission is missing")]
    PermissionDenied,
    #[error("no display to capture")]
    NoDisplay,
    #[error("screen capture did not start in time")]
    StartTimeout,
    #[error("screen capture failed: {0}")]
    Capture(String),
    #[error("picture {0} is no longer in memory")]
    FrameGone(usize),
    #[error("could not write a candidate picture: {0}")]
    Write(String),
}

/// Where the pictures come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenSource {
    /// The display under the cursor, through ScreenCaptureKit.
    Display,
    /// A folder of PNGs of one size, played in name order at 2 fps (debug builds, tests).
    PngDir(PathBuf),
}

/// What to capture and where candidate pictures go.
#[derive(Debug, Clone)]
pub struct ScreenConfig {
    pub source: ScreenSource,
    /// Murmur's own bundle id, so that its windows are left out of the pictures.
    pub exclude_bundle_id: Option<String>,
    /// Where the cursor was when the hotkey went down (CoreGraphics global points).
    pub cursor: (f64, f64),
    /// `<bundle>/candidates`, created with mode 0700 at the first kept picture.
    pub candidates_dir: PathBuf,
    /// Recording time zero.
    pub origin: Instant,
}

/// The captured display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInfo {
    pub id: u32,
    /// In points, CoreGraphics global coordinates (for the pointer adapter).
    pub rect: DisplayRect,
    /// Size of the pictures in pixels.
    pub width_px: usize,
    pub height_px: usize,
    /// Native pixels per point of the display.
    pub scale: f64,
}

/// One picture, reduced to numbers for the core.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// Counts from 0 in arrival order; also the candidate file name.
    pub index: usize,
    pub t_ms: u64,
    pub cells: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScreenEvent {
    Frame(Frame),
    /// The stream stopped on its own; the recording goes on without new pictures.
    Failed(String),
}

type Callback = Arc<dyn Fn(ScreenEvent) + Send + Sync>;

/// What `stop` returns.
pub struct Stopped {
    /// Every frame's numbers, in order.
    pub frames: Vec<Frame>,
    /// Candidate pictures written, by frame index.
    pub kept: Vec<(usize, PathBuf)>,
    /// The last pictures still in memory (for the "end" keyframe), oldest first.
    pub recent: Vec<(usize, Bitmap)>,
}

struct State {
    paused: bool,
    next_index: usize,
    frames: Vec<Frame>,
    ring: VecDeque<(usize, u64, Arc<Bitmap>)>,
    kept: Vec<(usize, PathBuf)>,
}

struct Shared {
    origin: Instant,
    on_event: Callback,
    candidates_dir: PathBuf,
    state: Mutex<State>,
}

struct WriteJob {
    index: usize,
    bitmap: Arc<Bitmap>,
}

/// Single background thread that writes candidate PNGs in FIFO order for one capture session.
struct CandidateWriter {
    tx: Option<mpsc::Sender<WriteJob>>,
    done_rx: mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CandidateWriter {
    fn start(shared: Arc<Shared>) -> Result<Self, ScreenError> {
        let (tx, rx) = mpsc::channel::<WriteJob>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("screen-candidate".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let result = ensure_folder(&shared.candidates_dir).and_then(|()| {
                        image::write_png(
                            &job.bitmap,
                            &shared.candidates_dir.join(format!("{}.png", job.index)),
                        )
                        .map_err(|e| ScreenError::Write(e.to_string()))
                    });
                    if let Err(error) = result {
                        tracing::warn!(%error, index = job.index, "candidate picture not written");
                        shared.lock().kept.retain(|(i, _)| *i != job.index);
                    }
                }
                let _ = done_tx.send(());
            })
            .map_err(|_| ScreenError::Write("no thread".into()))?;
        Ok(Self {
            tx: Some(tx),
            done_rx,
            thread: Some(thread),
        })
    }

    fn send(&self, job: WriteJob) -> Result<(), ScreenError> {
        let Some(tx) = self.tx.as_ref() else {
            return Err(ScreenError::Write("writer closed".into()));
        };
        tx.send(job)
            .map_err(|_| ScreenError::Write("writer stopped".into()))
    }

    fn finish(mut self, timeout: Duration) {
        // Closing `tx` lets the worker drain queued writes and then exit cleanly.
        drop(self.tx.take());
        if self.done_rx.recv_timeout(timeout).is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            tracing::warn!("a candidate picture was still being written at stop");
        }
    }
}

impl Drop for CandidateWriter {
    fn drop(&mut self) {
        drop(self.tx.take());
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One picture arrived on the capture queue.
    fn on_sample(&self, sample: &CMSampleBuffer) {
        // SAFETY: the sample buffer is valid for the duration of the callback; the pixel buffer
        // is locked read-only while its bytes are read and unlocked right after.
        let picture = unsafe {
            let Some(buffer) = sample.image_buffer() else {
                return;
            };
            let pixel: &CVPixelBuffer =
                &*(CFRetained::as_ptr(&buffer).as_ptr() as *const CVPixelBuffer);
            if CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return;
            }
            let width = CVPixelBufferGetWidth(pixel);
            let height = CVPixelBufferGetHeight(pixel);
            let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel);
            let base = CVPixelBufferGetBaseAddress(pixel) as *const u8;
            let picture = if base.is_null() || width == 0 || height == 0 {
                None
            } else {
                let data = std::slice::from_raw_parts(base, bytes_per_row * height);
                let cells = cells_of(data, width, height, bytes_per_row);
                Bitmap::from_rows(width, height, bytes_per_row, data)
                    .ok()
                    .map(|bitmap| (cells, bitmap))
            };
            CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly);
            picture
        };
        let Some((cells, bitmap)) = picture else {
            return;
        };
        self.push_picture(cells, bitmap);
    }

    /// One picture, from any source: numbers out, pixels into the ring, unless paused.
    fn push_picture(&self, cells: Vec<f32>, bitmap: Bitmap) {
        let t_ms = self.origin.elapsed().as_millis() as u64;
        let frame = {
            let mut state = self.lock();
            if state.paused {
                return;
            }
            let index = state.next_index;
            state.next_index += 1;
            let frame = Frame { index, t_ms, cells };
            state.frames.push(frame.clone());
            state.ring.push_back((index, t_ms, Arc::new(bitmap)));
            while state.ring.len() > RING_FRAMES {
                state.ring.pop_front();
            }
            frame
        };
        (self.on_event)(ScreenEvent::Frame(frame));
    }
}

/// Mean brightness (0 to 1) of each of the `GRID_COLUMNS x GRID_ROWS` cells of a BGRA picture,
/// row by row. Every fourth pixel in both directions is read; that is plenty for a 16 x 9 grid.
pub fn cells_of(bgra: &[u8], width: usize, height: usize, bytes_per_row: usize) -> Vec<f32> {
    let mut cells = vec![0.0f32; GRID_CELLS];
    if width == 0 || height == 0 || bytes_per_row < width * 4 {
        return cells;
    }
    let step = 4;
    for (ci, cell) in cells.iter_mut().enumerate() {
        let (col, row) = (ci % GRID_COLUMNS, ci / GRID_COLUMNS);
        let x0 = col * width / GRID_COLUMNS;
        let x1 = ((col + 1) * width / GRID_COLUMNS).max(x0 + 1);
        let y0 = row * height / GRID_ROWS;
        let y1 = ((row + 1) * height / GRID_ROWS).max(y0 + 1);
        let mut sum = 0u64;
        let mut count = 0u32;
        let mut y = y0;
        while y < y1 {
            let row_start = y * bytes_per_row;
            if let Some(row_slice) = bgra.get(row_start + x0 * 4..row_start + x1 * 4) {
                for px in row_slice.chunks(step * 4) {
                    if let [b, g, r, ..] = px {
                        sum += 114 * u64::from(*b) + 587 * u64::from(*g) + 299 * u64::from(*r);
                        count += 1;
                    }
                }
            } else {
                let mut x = x0;
                while x < x1 {
                    let i = row_start + x * 4;
                    if let Some(&[b, g, r]) = bgra.get(i..i + 3) {
                        sum += 114 * u64::from(b) + 587 * u64::from(g) + 299 * u64::from(r);
                        count += 1;
                    }
                    x += step;
                }
            }
            y += step;
        }
        if count > 0 {
            *cell = (sum as f64 / (255_000.0 * f64::from(count))) as f32;
        }
    }
    cells
}

struct Ivars {
    shared: Arc<Shared>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "MurmurScreenOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)] // the protocol method name
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.ivars().shared.on_sample(sample_buffer);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        #[allow(non_snake_case)] // the protocol method name
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            let message = error.localizedDescription().to_string();
            (self.ivars().shared.on_event)(ScreenEvent::Failed(message));
        }
    }
);

impl Output {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { shared });
        // SAFETY: NSObject's plain `init` on a freshly allocated instance with its ivars set.
        unsafe { msg_send![super(this), init] }
    }
}

/// A retained Objective-C object that is used from several threads.
struct Shareable<T: ?Sized>(Retained<T>);

// SAFETY: `SCStream` is documented as usable from any thread (its completion handlers run on
// arbitrary queues), and `Output` holds only an `Arc<Shared>`, which is `Send + Sync`.
unsafe impl<T: ?Sized> Send for Shareable<T> {}
unsafe impl<T: ?Sized> Sync for Shareable<T> {}

/// The dispatch queue the pictures arrive on, kept alive with the stream.
struct SharedQueue(#[allow(dead_code)] dispatch2::DispatchRetained<DispatchQueue>);

// SAFETY: a dispatch queue is thread-safe by design; the handle is only kept alive here.
unsafe impl Send for SharedQueue {}
unsafe impl Sync for SharedQueue {}

enum Backend {
    Sck {
        stream: Shareable<SCStream>,
        /// Kept alive for the stream: SCK holds them weakly.
        #[allow(dead_code)]
        output: Shareable<Output>,
        #[allow(dead_code)]
        queue: SharedQueue,
    },
    /// A thread that plays PNGs (debug builds).
    Pictures {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    },
}

pub struct ScreenCapture {
    backend: Backend,
    shared: Arc<Shared>,
    writer: Mutex<Option<CandidateWriter>>,
}

/// Starts capturing. Blocking for up to `START_TIMEOUT` (call it off the controller loop). The
/// caller has checked the Screen Recording permission (G8).
pub fn start(
    config: ScreenConfig,
    on_event: Callback,
) -> Result<(ScreenCapture, DisplayInfo), ScreenError> {
    match config.source.clone() {
        ScreenSource::Display => start_display(config, on_event),
        ScreenSource::PngDir(dir) => start_pictures(config, on_event, &dir),
    }
}

fn new_shared(config: &ScreenConfig, on_event: Callback) -> Arc<Shared> {
    Arc::new(Shared {
        origin: config.origin,
        on_event,
        candidates_dir: config.candidates_dir.clone(),
        state: Mutex::new(State {
            paused: false,
            next_index: 0,
            frames: Vec::new(),
            ring: VecDeque::with_capacity(RING_FRAMES + 1),
            kept: Vec::new(),
        }),
    })
}

/// Plays `dir/*.png` (name order) at 2 fps as if it were the screen. The pictures must share
/// one size; the first one decides the display size (points = pixels, scale 1). Once the last
/// picture is shown nothing more arrives, as with a screen that stopped changing.
fn start_pictures(
    config: ScreenConfig,
    on_event: Callback,
    dir: &std::path::Path,
) -> Result<(ScreenCapture, DisplayInfo), ScreenError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| ScreenError::Capture(format!("{}: {e}", dir.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
        .collect();
    files.sort();
    let first = files
        .first()
        .ok_or_else(|| ScreenError::Capture("no PNG in the folder".into()))?;
    let picture = image::read_png(first).map_err(|e| ScreenError::Capture(e.to_string()))?;
    let info = DisplayInfo {
        id: 0,
        rect: DisplayRect {
            x: 0.0,
            y: 0.0,
            width: picture.width as f64,
            height: picture.height as f64,
        },
        width_px: picture.width,
        height_px: picture.height,
        scale: 1.0,
    };
    let shared = new_shared(&config, on_event);
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let player = shared.clone();
    let interval = Duration::from_millis(1_000 / FRAMES_PER_SECOND as u64);
    let thread = std::thread::Builder::new()
        .name("screen-pictures".into())
        .spawn(move || {
            let mut next = Instant::now();
            for path in files {
                while Instant::now() < next {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                next += interval;
                let Ok(bitmap) = image::read_png(&path) else {
                    continue;
                };
                let cells = cells_of(&bitmap.bgra, bitmap.width, bitmap.height, bitmap.width * 4);
                player.push_picture(cells, bitmap);
            }
        })
        .map_err(|e| ScreenError::Capture(e.to_string()))?;
    tracing::info!(
        width_px = info.width_px,
        height_px = info.height_px,
        "screen capture started (pictures from a folder)"
    );
    Ok((
        ScreenCapture {
            backend: Backend::Pictures {
                stop,
                thread: Some(thread),
            },
            shared,
            writer: Mutex::new(None),
        },
        info,
    ))
}

fn start_display(
    config: ScreenConfig,
    on_event: Callback,
) -> Result<(ScreenCapture, DisplayInfo), ScreenError> {
    let deadline = Instant::now() + START_TIMEOUT;
    let content = shareable_content(deadline)?;
    let (display, info) = pick_display(&content, config.cursor)?;
    let excluded = excluded_apps(&content, config.exclude_bundle_id.as_deref());

    // SAFETY: plain constructors and setters on freshly created SCK objects; all arguments
    // outlive the calls.
    let (filter, stream_config) = unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            &display,
            &excluded,
            &NSArray::new(),
        );
        let stream_config = SCStreamConfiguration::new();
        stream_config.setWidth(info.width_px);
        stream_config.setHeight(info.height_px);
        stream_config.setMinimumFrameInterval(CMTime::new(1, FRAMES_PER_SECOND));
        stream_config.setShowsCursor(true);
        stream_config.setQueueDepth(3);
        stream_config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
        (filter, stream_config)
    };

    let shared = new_shared(&config, on_event);
    let output = Output::new(shared.clone());
    let queue = DispatchQueue::new("io.github.naoya5.murmur.screen", None);
    // SAFETY: the stream keeps the filter and the configuration; the output object and the
    // queue are retained by this `ScreenCapture` for the life of the stream.
    let stream = unsafe {
        let delegate: &ProtocolObject<dyn SCStreamDelegate> = ProtocolObject::from_ref(&*output);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &stream_config,
            Some(delegate),
        );
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|e| ScreenError::Capture(e.localizedDescription().to_string()))?;
        stream
    };

    let (tx, rx) = mpsc::channel::<Option<String>>();
    let tx = Mutex::new(Some(tx));
    let on_started = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: SCK passes a valid error object or null.
        let error: Option<&NSError> = unsafe { error.as_ref() };
        if let Ok(mut guard) = tx.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(error.map(|e| e.localizedDescription().to_string()));
        }
    });
    // SAFETY: the completion block is copied by SCK and owns only a `Sender`.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&on_started)) };
    let left = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(left) {
        Ok(None) => {}
        Ok(Some(message)) => return Err(ScreenError::Capture(message)),
        Err(_) => return Err(ScreenError::StartTimeout),
    }
    tracing::info!(
        width_px = info.width_px,
        height_px = info.height_px,
        "screen capture started"
    );
    Ok((
        ScreenCapture {
            backend: Backend::Sck {
                stream: Shareable(stream),
                output: Shareable(output),
                queue: SharedQueue(queue),
            },
            shared,
            writer: Mutex::new(None),
        },
        info,
    ))
}

impl ScreenCapture {
    /// While paused, arriving pictures are dropped (the cancel countdown).
    pub fn set_paused(&self, paused: bool) {
        self.shared.lock().paused = paused;
    }

    /// Writes the picture `index` to `candidates/<index>.png` in the background, if it is still
    /// one of the last `RING_FRAMES`.
    pub fn keep_frame(&self, index: usize) -> Result<(), ScreenError> {
        let bitmap = {
            let mut state = self.shared.lock();
            if state.kept.iter().any(|(i, _)| *i == index) {
                return Ok(());
            }
            let bitmap = state
                .ring
                .iter()
                .find(|(i, _, _)| *i == index)
                .map(|(_, _, b)| Arc::clone(b))
                .ok_or(ScreenError::FrameGone(index))?;
            let path = self.shared.candidates_dir.join(format!("{index}.png"));
            state.kept.push((index, path));
            bitmap
        };
        let send_result = {
            let mut writer_guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            if writer_guard.is_none() {
                match CandidateWriter::start(self.shared.clone()) {
                    Ok(writer) => *writer_guard = Some(writer),
                    Err(error) => {
                        drop(writer_guard);
                        self.shared.lock().kept.retain(|(i, _)| *i != index);
                        return Err(error);
                    }
                }
            }
            match writer_guard.as_ref() {
                Some(writer) => writer.send(WriteJob { index, bitmap }),
                None => Err(ScreenError::Write("writer missing".into())),
            }
        };
        if let Err(error) = send_result {
            self.shared.lock().kept.retain(|(i, _)| *i != index);
            return Err(error);
        }
        Ok(())
    }

    /// Frames so far (numbers only), for the controller's candidate decisions.
    pub fn frames(&self) -> Vec<Frame> {
        self.shared.lock().frames.clone()
    }

    /// Stops the capture and waits (with timeouts) for the stream and the candidate writes.
    pub fn stop(self) -> Result<Stopped, ScreenError> {
        let ScreenCapture {
            backend,
            shared,
            writer,
        } = self;
        match backend {
            Backend::Sck { stream, .. } => {
                let (tx, rx) = mpsc::channel::<()>();
                let tx = Mutex::new(Some(tx));
                let on_stopped = RcBlock::new(move |_error: *mut NSError| {
                    if let Ok(mut guard) = tx.lock()
                        && let Some(tx) = guard.take()
                    {
                        let _ = tx.send(());
                    }
                });
                // SAFETY: the completion block is copied by SCK and owns only a `Sender`.
                unsafe { stream.0.stopCaptureWithCompletionHandler(Some(&on_stopped)) };
                if rx.recv_timeout(STOP_TIMEOUT).is_err() {
                    tracing::warn!("screen capture did not confirm its stop in time");
                }
            }
            Backend::Pictures { stop, mut thread } => {
                stop.store(true, Ordering::SeqCst);
                if let Some(thread) = thread.take() {
                    let _ = thread.join();
                }
            }
        }
        if let Some(writer) = writer.into_inner().unwrap_or_else(|e| e.into_inner()) {
            writer.finish(WRITE_TIMEOUT);
        }
        let mut state = shared.lock();
        let frames = std::mem::take(&mut state.frames);
        let kept = std::mem::take(&mut state.kept);
        let recent = state
            .ring
            .drain(..)
            .map(|(index, _, bitmap)| (index, Arc::unwrap_or_clone(bitmap)))
            .collect();
        drop(state);
        tracing::info!(
            frames = frames.len(),
            kept = kept.len(),
            "screen capture stopped"
        );
        Ok(Stopped {
            frames,
            kept,
            recent,
        })
    }
}

fn ensure_folder(path: &std::path::Path) -> Result<(), ScreenError> {
    if path.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(FOLDER_MODE)
        .create(path)
        .map_err(|e| ScreenError::Write(format!("{}: {e}", path.display())))
}

/// The list of displays and apps, within the deadline.
fn shareable_content(deadline: Instant) -> Result<Retained<SCShareableContent>, ScreenError> {
    let (tx, rx) = mpsc::channel::<Result<Retained<SCShareableContent>, String>>();
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: SCK passes either a valid content object or a valid error.
            let result = if content.is_null() {
                let error: Option<&NSError> = unsafe { error.as_ref() };
                Err(error
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "unknown error".to_owned()))
            } else {
                match unsafe { Retained::retain(content) } {
                    Some(content) => Ok(content),
                    None => Err("no content".to_owned()),
                }
            };
            if let Ok(mut guard) = tx.lock()
                && let Some(tx) = guard.take()
            {
                let _ = tx.send(result);
            }
        },
    );
    // SAFETY: the block is copied by SCK and owns only a `Sender`.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }
    let left = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(left) {
        Ok(Ok(content)) => Ok(content),
        Ok(Err(message)) if message.contains("declined") || message.contains("permission") => {
            Err(ScreenError::PermissionDenied)
        }
        Ok(Err(message)) => Err(ScreenError::Capture(message)),
        Err(_) => Err(ScreenError::StartTimeout),
    }
}

/// The display under `cursor` (else the main display) and its facts.
fn pick_display(
    content: &SCShareableContent,
    cursor: (f64, f64),
) -> Result<(Retained<SCDisplay>, DisplayInfo), ScreenError> {
    let mut id: CGDirectDisplayID = 0;
    let mut count: u32 = 0;
    // SAFETY: the out-pointers are valid for one display id and one count.
    let status =
        unsafe { CGGetDisplaysWithPoint(CGPoint::new(cursor.0, cursor.1), 1, &mut id, &mut count) };
    if status != objc2_core_graphics::CGError::Success || count == 0 {
        id = CGMainDisplayID();
    }
    // SAFETY: `displays()` and `displayID()` are plain getters.
    let displays = unsafe { content.displays() };
    let display = displays
        .iter()
        .find(|d| unsafe { d.displayID() } == id)
        .or_else(|| displays.firstObject())
        .ok_or(ScreenError::NoDisplay)?;
    // SAFETY: plain getter.
    let id = unsafe { display.displayID() };
    let bounds = CGDisplayBounds(id);
    let (w_pt, h_pt) = (bounds.size.width, bounds.size.height);
    if w_pt <= 0.0 || h_pt <= 0.0 {
        return Err(ScreenError::NoDisplay);
    }
    let (width_px, height_px) = capture_size(w_pt, h_pt);
    let scale = CGDisplayPixelsWide(id) as f64 / w_pt;
    Ok((
        display,
        DisplayInfo {
            id,
            rect: DisplayRect {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: w_pt,
                height: h_pt,
            },
            width_px,
            height_px,
            scale,
        },
    ))
}

/// Picture size with the long edge at `LONG_EDGE_PX`, keeping the display's aspect.
pub fn capture_size(width_pt: f64, height_pt: f64) -> (usize, usize) {
    let long = LONG_EDGE_PX as f64;
    if width_pt >= height_pt {
        (
            LONG_EDGE_PX,
            ((long * height_pt / width_pt).round() as usize).max(1),
        )
    } else {
        (
            ((long * width_pt / height_pt).round() as usize).max(1),
            LONG_EDGE_PX,
        )
    }
}

/// The running apps with `bundle_id` (Murmur itself), to leave out of the pictures.
fn excluded_apps(
    content: &SCShareableContent,
    bundle_id: Option<&str>,
) -> Retained<NSArray<SCRunningApplication>> {
    let Some(bundle_id) = bundle_id else {
        return NSArray::new();
    };
    // SAFETY: plain getters on the content object and its apps.
    let apps = unsafe { content.applications() };
    let excluded: Vec<Retained<SCRunningApplication>> = apps
        .iter()
        .filter(|app| unsafe { app.bundleIdentifier() }.to_string() == bundle_id)
        .collect();
    NSArray::from_retained_slice(&excluded)
}

#[cfg(test)]
pub(crate) struct TestPictureSink(Arc<Shared>);

#[cfg(test)]
impl TestPictureSink {
    pub(crate) fn push(&self, cells: Vec<f32>, bitmap: Bitmap) {
        self.0.push_picture(cells, bitmap);
    }
}

#[cfg(test)]
pub(crate) fn test_capture_for_dir(candidates_dir: PathBuf) -> (ScreenCapture, TestPictureSink) {
    let config = ScreenConfig {
        source: ScreenSource::PngDir(candidates_dir.clone()),
        exclude_bundle_id: None,
        cursor: (0.0, 0.0),
        candidates_dir,
        origin: Instant::now(),
    };
    let shared = new_shared(&config, Arc::new(|_| {}));
    let capture = ScreenCapture {
        backend: Backend::Pictures {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        },
        shared: Arc::clone(&shared),
        writer: Mutex::new(None),
    };
    (capture, TestPictureSink(shared))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_size_keeps_the_aspect_with_the_long_edge_fixed() {
        assert_eq!(capture_size(2560.0, 1440.0), (1568, 882));
        assert_eq!(capture_size(1512.0, 982.0), (1568, 1018));
        assert_eq!(capture_size(1080.0, 1920.0), (882, 1568));
        assert_eq!(capture_size(1000.0, 1000.0), (1568, 1568));
    }

    #[test]
    fn cells_follow_the_brightness_of_the_grid() {
        // 64 x 36 pixels, 4 bytes each, 16 bytes of row padding: left half white, right half black.
        let (w, h, stride) = (64usize, 36usize, 64 * 4 + 16);
        let mut data = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * stride + x * 4;
                let v = if x < w / 2 { 255 } else { 0 };
                data[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let cells = cells_of(&data, w, h, stride);
        assert_eq!(cells.len(), GRID_CELLS);
        for (i, c) in cells.iter().enumerate() {
            let col = i % GRID_COLUMNS;
            if col < GRID_COLUMNS / 2 {
                assert!((c - 1.0).abs() < 0.01, "cell {i} = {c}");
            } else {
                assert!(c.abs() < 0.01, "cell {i} = {c}");
            }
        }
        // Colour weights: pure green is brighter than pure blue.
        let mut green = vec![0u8; 16 * 4 * 9];
        let mut blue = green.clone();
        for px in green.as_chunks_mut::<4>().0 {
            px[1] = 255;
            px[3] = 255;
        }
        for px in blue.as_chunks_mut::<4>().0 {
            px[0] = 255;
            px[3] = 255;
        }
        let g = cells_of(&green, 16, 9, 64);
        let b = cells_of(&blue, 16, 9, 64);
        assert!(g[0] > b[0]);
        assert!((g[0] - 0.587).abs() < 0.01);
        // Degenerate input gives a dark grid, never a panic.
        assert_eq!(cells_of(&[], 0, 0, 0), vec![0.0; GRID_CELLS]);
        assert_eq!(cells_of(&data[..10], 64, 36, 100), vec![0.0; GRID_CELLS]);
    }

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "murmur_screen_test_{name}_{}_{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn solid_bitmap(width: usize, height: usize, b: u8, g: u8, r: u8) -> Bitmap {
        let mut bmp = Bitmap::new(width, height).unwrap();
        for px in bmp.bgra.as_chunks_mut::<4>().0 {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        bmp
    }

    #[test]
    fn candidate_writer_writes_multiple_kept_frames_dedups_and_reports_gone() {
        let dir = ScratchDir::new("writer");
        let candidates_dir = dir.0.join("candidates");
        let (capture, shared) = test_capture_for_dir(candidates_dir.clone());

        shared.push(vec![0.1; GRID_CELLS], solid_bitmap(8, 8, 10, 20, 30));
        shared.push(vec![0.2; GRID_CELLS], solid_bitmap(8, 8, 40, 50, 60));
        shared.push(vec![0.3; GRID_CELLS], solid_bitmap(8, 8, 70, 80, 90));

        capture.keep_frame(0).unwrap();
        capture.keep_frame(0).unwrap();
        capture.keep_frame(1).unwrap();

        // Push enough new frames to evict frame 2 from the ring without keeping it.
        for i in 0..RING_FRAMES {
            shared.push(vec![0.4; GRID_CELLS], solid_bitmap(8, 8, i as u8, 100, 110));
        }
        assert!(matches!(
            capture.keep_frame(2),
            Err(ScreenError::FrameGone(2))
        ));

        let stopped = capture.stop().unwrap();
        assert_eq!(stopped.frames.len(), 3 + RING_FRAMES);
        assert_eq!(stopped.kept.len(), 2);
        assert_eq!(stopped.kept[0].0, 0);
        assert_eq!(stopped.kept[1].0, 1);
        assert_eq!(stopped.recent.len(), RING_FRAMES);

        let decoded0 = crate::adapters::image::read_png(&stopped.kept[0].1).unwrap();
        assert_eq!((decoded0.width, decoded0.height), (8, 8));
        assert_eq!(&decoded0.bgra[..4], &[10, 20, 30, 255]);

        let decoded1 = crate::adapters::image::read_png(&stopped.kept[1].1).unwrap();
        assert_eq!((decoded1.width, decoded1.height), (8, 8));
        assert_eq!(&decoded1.bgra[..4], &[40, 50, 60, 255]);
    }

    #[test]
    fn stop_without_keep_frame_writes_nothing_and_returns_recent_bitmaps() {
        let dir = ScratchDir::new("no_keep");
        let candidates_dir = dir.0.join("candidates");
        let (capture, shared) = test_capture_for_dir(candidates_dir.clone());

        shared.push(vec![0.1; GRID_CELLS], solid_bitmap(4, 4, 1, 2, 3));
        shared.push(vec![0.2; GRID_CELLS], solid_bitmap(4, 4, 4, 5, 6));

        let stopped = capture.stop().unwrap();
        assert_eq!(stopped.frames.len(), 2);
        assert!(stopped.kept.is_empty());
        assert_eq!(stopped.recent.len(), 2);
        assert!(!candidates_dir.exists());
    }
}
