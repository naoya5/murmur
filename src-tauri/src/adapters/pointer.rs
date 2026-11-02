//! Adapter: the pointer during an annotation recording. Clicks in other apps (an `NSEvent`
//! global monitor: events sent to Murmur's own windows never reach it, so drawing on the overlay
//! is not a click), the cursor position and the Shift key (CoreGraphics, read at 30 Hz from a
//! sampler thread), and the frontmost app's name (NSWorkspace, 2 Hz, name only, never a window
//! title).
//!
//! A `CGEventTap` would need the Input Monitoring permission, so it is not used (stage 0, 7).
//! Positions are reported as shares of the recorded display (0 to 1); times as milliseconds
//! since the `origin` the caller passes. Names never reach the log (G7). Nothing panics (G6).
#![allow(dead_code)] // wired into the controller by the next milestone

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventType, NSWorkspace};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGMainDisplayID,
};
use tauri::AppHandle;

/// How often the sampler reads the cursor and the Shift key.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(33);
/// A cursor position goes out every this many samples (about 10 Hz).
const CURSOR_EVERY: u32 = 3;
/// The frontmost app is read every this many samples (about 2 Hz).
const APP_EVERY: u32 = 15;
/// Waiting for the main thread to add or remove the monitor (G5).
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(2);

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

#[derive(Debug, Clone, PartialEq)]
pub enum PointerEvent {
    /// A mouse button went down in another app, inside the recorded display.
    Click { t_ms: u64, at: (f32, f32) },
    /// Where the cursor is (about 10 Hz), inside the recorded display only.
    Cursor { t_ms: u64, at: (f32, f32) },
    /// The Shift key went down (true) or up (false).
    Shift(bool),
    /// The frontmost app changed (its localized name).
    App { t_ms: u64, name: String },
}

type Callback = Arc<dyn Fn(PointerEvent) + Send + Sync>;

/// The global monitor object. It is created and removed on the main thread only.
struct Monitor(Retained<AnyObject>);

// SAFETY: the object is only touched on the main thread (`run_on_main_thread`); this wrapper
// exists so the handle can be stored in a `Pointer` that lives on another thread.
unsafe impl Send for Monitor {}

pub struct Pointer {
    app: AppHandle,
    stop: Arc<AtomicBool>,
    monitor: Arc<Mutex<Option<Monitor>>>,
    sampler: Option<JoinHandle<()>>,
}

/// Starts watching. Returns at once; the monitor is registered on the main thread.
pub fn start(
    app: &AppHandle,
    display: DisplayRect,
    origin: Instant,
    on_event: Callback,
) -> Pointer {
    let stop = Arc::new(AtomicBool::new(false));
    let monitor: Arc<Mutex<Option<Monitor>>> = Arc::new(Mutex::new(None));

    let (handler_events, monitor_slot) = (on_event.clone(), monitor.clone());
    let _ = app.run_on_main_thread(move || {
        let block = RcBlock::new(move |event: NonNull<NSEvent>| {
            // SAFETY: AppKit hands a live event to the handler for the duration of the call.
            let event = unsafe { event.as_ref() };
            let (kind, location) = (event.r#type(), event.locationInWindow());
            if kind != NSEventType::LeftMouseDown && kind != NSEventType::RightMouseDown {
                return;
            }
            // A global monitor reports screen coordinates (AppKit, bottom-left origin).
            let main_height = CGDisplayBounds(CGMainDisplayID()).size.height;
            let (x, y) = appkit_to_cg((location.x, location.y), main_height);
            if let Some(at) = normalize((x, y), &display) {
                handler_events(PointerEvent::Click {
                    t_ms: origin.elapsed().as_millis() as u64,
                    at,
                });
            }
        });
        let mask = NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown;
        let object = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block);
        match object {
            Some(object) => {
                if let Ok(mut slot) = monitor_slot.lock() {
                    *slot = Some(Monitor(object));
                }
            }
            None => tracing::warn!("the click monitor could not be added"),
        }
    });

    let sampler = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("pointer-sampler".into())
            .spawn(move || sample_loop(display, origin, on_event, stop))
            .ok()
    };
    Pointer {
        app: app.clone(),
        stop,
        monitor,
        sampler,
    }
}

impl Pointer {
    /// Removes the monitor and stops the sampler. Waits briefly for both (G5).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.sampler.take() {
            let _ = handle.join();
        }
        let slot = self.monitor.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = self.app.run_on_main_thread(move || {
            if let Ok(mut guard) = slot.lock()
                && let Some(Monitor(object)) = guard.take()
            {
                // SAFETY: the object came from `addGlobalMonitor` and is removed once, here.
                unsafe { NSEvent::removeMonitor(&object) };
            }
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(MAIN_THREAD_TIMEOUT);
    }
}

impl Drop for Pointer {
    fn drop(&mut self) {
        if !self.stop.load(Ordering::SeqCst) {
            self.shutdown();
        }
    }
}

fn sample_loop(display: DisplayRect, origin: Instant, on_event: Callback, stop: Arc<AtomicBool>) {
    let mut tick: u32 = 0;
    let mut shift = false;
    let mut app_name: Option<String> = None;
    while !stop.load(Ordering::SeqCst) {
        let now = origin.elapsed().as_millis() as u64;
        let held = shift_held();
        if held != shift {
            shift = held;
            on_event(PointerEvent::Shift(held));
        }
        if tick.is_multiple_of(CURSOR_EVERY)
            && let Some((x, y)) = cursor_position()
            && let Some(at) = normalize((x, y), &display)
        {
            on_event(PointerEvent::Cursor { t_ms: now, at });
        }
        if tick.is_multiple_of(APP_EVERY) {
            let name = frontmost_app_name();
            if name != app_name {
                if let Some(n) = &name {
                    on_event(PointerEvent::App {
                        t_ms: now,
                        name: n.clone(),
                    });
                }
                app_name = name;
            }
        }
        tick = tick.wrapping_add(1);
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

/// The localized name of the frontmost app (never its window title).
fn frontmost_app_name() -> Option<String> {
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    app.localizedName().map(|n| n.to_string())
}

/// AppKit global coordinates (origin bottom left of the main display) to CoreGraphics global
/// coordinates (origin top left of the main display).
pub fn appkit_to_cg(point: (f64, f64), main_display_height: f64) -> (f64, f64) {
    (point.0, main_display_height - point.1)
}

/// A CoreGraphics point as a share of the display, or `None` outside it.
pub fn normalize(point: (f64, f64), display: &DisplayRect) -> Option<(f32, f32)> {
    if display.width <= 0.0 || display.height <= 0.0 || !display.contains(point.0, point.1) {
        return None;
    }
    Some((
        ((point.0 - display.x) / display.width) as f32,
        ((point.1 - display.y) / display.height) as f32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: DisplayRect = DisplayRect {
        x: 0.0,
        y: 0.0,
        width: 1512.0,
        height: 982.0,
    };

    #[test]
    fn appkit_points_flip_against_the_main_display() {
        // The bottom-left corner of the main display is (0, 0) in AppKit and (0, 982) in CG.
        assert_eq!(appkit_to_cg((0.0, 0.0), 982.0), (0.0, 982.0));
        assert_eq!(appkit_to_cg((100.0, 982.0), 982.0), (100.0, 0.0));
        // A display to the left and above keeps its x; y flips through the main height.
        assert_eq!(appkit_to_cg((-500.0, 1200.0), 982.0), (-500.0, -218.0));
    }

    #[test]
    fn normalization_is_a_share_of_the_display_and_none_outside() {
        assert_eq!(normalize((0.0, 0.0), &MAIN), Some((0.0, 0.0)));
        assert_eq!(normalize((756.0, 491.0), &MAIN), Some((0.5, 0.5)));
        assert_eq!(
            normalize((1512.0, 10.0), &MAIN),
            None,
            "right edge is outside"
        );
        assert_eq!(normalize((-1.0, 10.0), &MAIN), None);
        assert_eq!(normalize((10.0, 982.0), &MAIN), None);
        // A second display to the right.
        let right = DisplayRect {
            x: 1512.0,
            y: -200.0,
            width: 2560.0,
            height: 1440.0,
        };
        assert_eq!(normalize((1512.0, -200.0), &right), Some((0.0, 0.0)));
        assert_eq!(normalize((2792.0, 520.0), &right), Some((0.5, 0.5)));
        assert_eq!(normalize((100.0, 100.0), &right), None);
        let empty = DisplayRect { width: 0.0, ..MAIN };
        assert_eq!(normalize((0.0, 0.0), &empty), None);
    }

    /// The system reads must not crash or prompt; values depend on the machine.
    #[test]
    fn system_reads_do_not_crash() {
        let _ = shift_held();
        let _ = cursor_position();
        let _ = frontmost_app_name();
    }
}
