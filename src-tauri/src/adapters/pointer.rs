//! Adapter: the small reads of the pointer during an annotation. The Shift key (CoreGraphics
//! `flags_state`, public state, no permission), read at 30 Hz from a sampler thread while
//! recording so the drawing layer knows when to take the drag, and the cursor position at the
//! start, to pick the display under it. Nothing else: no clicks, no cursor tracking, no app
//! names (a `CGEventTap` would need the Input Monitoring permission; stage 0, 7).
//!
//! Nothing panics (G6).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID};

/// How often the sampler reads the Shift key.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(33);

/// A display in CoreGraphics global coordinates (origin at the top left of the main display),
/// in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl DisplayRect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEvent {
    /// The Shift key went down (true) or up (false).
    Shift(bool),
}

type Callback = Arc<dyn Fn(PointerEvent) + Send + Sync>;

/// Anything that feeds pointer events into a recording: the real sampler, or a test script.
pub trait PointerFeed: Send {
    fn stop(self: Box<Self>);
}

/// The Shift sampler of one recording.
pub struct Pointer {
    stop: Arc<AtomicBool>,
    sampler: Option<JoinHandle<()>>,
}

impl PointerFeed for Pointer {
    fn stop(mut self: Box<Self>) {
        self.shutdown();
    }
}

/// Starts watching the Shift key. Returns at once.
pub fn start(on_event: Callback) -> Pointer {
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("pointer-sampler".into())
            .spawn(move || sample_loop(on_event, stop))
            .ok()
    };
    Pointer { stop, sampler }
}

impl Pointer {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.sampler.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Pointer {
    fn drop(&mut self) {
        if !self.stop.load(Ordering::SeqCst) {
            self.shutdown();
        }
    }
}

fn sample_loop(on_event: Callback, stop: Arc<AtomicBool>) {
    let mut shift = false;
    while !stop.load(Ordering::SeqCst) {
        let held = shift_held();
        if held != shift {
            shift = held;
            on_event(PointerEvent::Shift(held));
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
}

/// Whether Shift is down anywhere on the system. Public CoreGraphics state, no permission.
pub fn shift_held() -> bool {
    CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState)
        .contains(CGEventFlags::MaskShift)
}

/// The cursor in CoreGraphics global coordinates (points).
pub fn cursor_position() -> Option<(f64, f64)> {
    let event = CGEvent::new(None)?;
    let location = CGEvent::location(Some(&event));
    Some((location.x, location.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_contains_its_points_only() {
        let main = DisplayRect {
            x: 0.0,
            y: 0.0,
            width: 1512.0,
            height: 982.0,
        };
        assert!(main.contains(0.0, 0.0));
        assert!(main.contains(1511.9, 981.9));
        assert!(!main.contains(1512.0, 10.0));
        assert!(!main.contains(-1.0, 10.0));
    }

    /// The system reads must not crash or prompt; values depend on the machine.
    #[test]
    fn system_reads_do_not_crash() {
        let _ = shift_held();
        let _ = cursor_position();
    }

    #[test]
    fn the_sampler_stops_and_reports_only_shift() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let pointer = start(Arc::new(move |e| {
            if let Ok(mut v) = sink.lock() {
                v.push(e);
            }
        }));
        std::thread::sleep(Duration::from_millis(80));
        Box::new(pointer).stop();
        let events = seen.lock().unwrap();
        assert!(events.iter().all(|e| matches!(e, PointerEvent::Shift(_))));
    }
}
