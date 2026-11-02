//! Windows: the recording pill (a non-activating NSPanel with Liquid Glass) and the main window.
//!
//! The pill must never take focus from the app the user is typing in: it is ordered front with
//! `orderFrontRegardless` and never made key. AppKit calls run on the main thread (`run_on_main_thread`).
//! The app is an `Accessory` (no Dock icon, no Cmd+Tab entry) except while the main window is open.
//!
//! The pill window has one fixed size: the widest capsule plus a transparent margin on every side
//! (`PILL_MARGIN`), where the transcribing glow is drawn. The page centers the capsule in it; the
//! native glass is cut to the capsule (`set_pill_width`), and everywhere else the window lets clicks
//! through to the app below.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2::runtime::{AnyClass, NSObjectProtocol};
use tauri::window::{Color, Effect, EffectState, EffectsBuilder};
use tauri::{ActivationPolicy, AppHandle, Manager, WindowEvent};
use tauri_nspanel::objc2_app_kit::{NSEvent, NSScreen, NSWindow, NSWindowStyleMask};
use tauri_nspanel::objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
use tauri_nspanel::{CollectionBehavior, ManagerExt, PanelLevel, WebviewWindowExt};

use self::pill_class::PillPanel;

pub const PILL_LABEL: &str = "pill";
pub const MAIN_LABEL: &str = "main";
/// Capsule height in points. The corner radius is half of it.
const PILL_HEIGHT: f64 = 40.0;
const PILL_MIN_WIDTH: f64 = 60.0;
/// The widest capsule; the `set_pill_width` command in ipc.rs clamps to the same bound.
const PILL_MAX_WIDTH: f64 = 480.0;
/// Gap between the capsule and the bottom of the visible screen area.
const PILL_BOTTOM_MARGIN: f64 = 20.0;
/// Transparent space around the capsule, on every side, for the transcribing glow. The page pads
/// its root by the same amount (`--pill-margin` in src/styles.css); keep the two equal.
const PILL_MARGIN: f64 = 20.0;
/// The pill window, in points. It never changes size, so the webview never relayouts while the
/// capsule changes width (a late relayout would leave the glass and the page's capsule apart).
const PILL_WINDOW: (f64, f64) = (
    PILL_MAX_WIDTH + 2.0 * PILL_MARGIN,
    PILL_HEIGHT + 2.0 * PILL_MARGIN,
);
/// How often the pointer is sampled to decide whether the window takes mouse events.
const CLICK_THROUGH_TICK: Duration = Duration::from_millis(40);
/// Upper bound for one visible period of the pill (G5): longer than a 10 minute recording, its
/// transcription and the notice that follows.
const CLICK_THROUGH_MAX: Duration = Duration::from_secs(30 * 60);
/// Bumped on every show and hide; a pointer loop runs only while it holds its own generation.
static CLICK_THROUGH_GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The capsule's current width (main thread only): it sizes the glass and the clickable area.
    /// Zero (glass hidden, nothing clickable) until the page sends one.
    static CAPSULE_WIDTH: Cell<f64> = const { Cell::new(0.0) };
}

/// The macro brings its own imports, so the class lives in its own module.
mod pill_class {
    use tauri_nspanel::tauri_panel;

    tauri_panel! {
        panel!(PillPanel {
            config: {
                can_become_key_window: false,
                can_become_main_window: false,
                is_floating_panel: true
            }
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    #[error("window {0} does not exist")]
    Missing(&'static str),
    #[error("window error: {0}")]
    Tauri(String),
}

impl From<tauri::Error> for WindowError {
    fn from(e: tauri::Error) -> Self {
        WindowError::Tauri(e.to_string())
    }
}

/// Once at startup, on the main thread (the `setup` closure).
pub fn setup(app: &AppHandle) -> Result<(), WindowError> {
    app.set_activation_policy(ActivationPolicy::Accessory)?;
    setup_pill(app)?;
    setup_main(app)?;
    Ok(())
}

fn setup_pill(app: &AppHandle) -> Result<(), WindowError> {
    let window = app
        .get_webview_window(PILL_LABEL)
        .ok_or(WindowError::Missing(PILL_LABEL))?;
    // The same Clear Liquid Glass as the main window (macOS 26), cut to the capsule. The pill floats
    // over whatever app the user types in, often a white page, so its warm dark tint is denser than
    // the main window's; the capsule's CSS fill completes it. macOS 14 and 15 fall back to the
    // frosted HUD material, kept active so it stays frosted while another app is in front.
    window.set_effects(
        EffectsBuilder::new()
            .effect(Effect::LiquidGlassClear)
            .effect(Effect::HudWindow)
            .state(EffectState::Active)
            .radius(PILL_HEIGHT / 2.0)
            .color(Color(20, 14, 10, 96))
            .build(),
    )?;
    let panel = window.to_panel::<PillPanel>()?;
    panel.set_level(PanelLevel::Status.value());
    panel.set_hides_on_deactivate(false);
    panel.set_becomes_key_only_if_needed(true);
    panel.set_floating_panel(true);
    // AppKit would shadow every painted pixel, the glow in the margin included; the page draws the
    // capsule's shadow itself, inside the margin.
    panel.set_has_shadow(false);
    panel.set_released_when_closed(false);
    panel.set_collection_behavior(
        CollectionBehavior::new()
            .can_join_all_spaces()
            .full_screen_auxiliary()
            .stationary()
            .ignores_cycle()
            .value(),
    );
    panel
        .add_style_mask(NSWindowStyleMask::NonactivatingPanel)
        .map_err(|e| WindowError::Tauri(format!("{e:?}")))?;
    panel.set_content_size(PILL_WINDOW.0, PILL_WINDOW.1);
    fit_glass_to_capsule(panel.as_panel());
    Ok(())
}

fn setup_main(app: &AppHandle) -> Result<(), WindowError> {
    let window = app
        .get_webview_window(MAIN_LABEL)
        .ok_or(WindowError::Missing(MAIN_LABEL))?;
    // Clear Liquid Glass from macOS 26 (the most see-through style), with a light warm dark
    // tint that the page's thin scene tint completes; macOS 14 and 15 fall back to the frosted
    // sidebar material, kept active so it does not turn gray while another app is in front.
    window.set_effects(
        EffectsBuilder::new()
            .effect(Effect::LiquidGlassClear)
            .effect(Effect::Sidebar)
            .state(EffectState::Active)
            .color(Color(20, 14, 10, 72))
            .build(),
    )?;
    // The red button and Cmd+W hide the window; the app keeps running in the menu bar.
    let handle = app.clone();
    let hidden = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = hidden.hide();
            let _ = handle.set_activation_policy(ActivationPolicy::Accessory);
        }
    });
    Ok(())
}

/// Shows or hides the pill without taking focus. Any thread.
pub fn set_pill_visible(app: &AppHandle, visible: bool) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Ok(panel) = handle.get_webview_panel(PILL_LABEL) else {
            return;
        };
        if visible {
            // The glass waits for the page (hidden): it would show alone, empty, until the page
            // paints. The page sends the capsule's width right after its first paint.
            CAPSULE_WIDTH.with(|w| w.set(0.0));
            place_pill(&panel);
            fit_glass_to_capsule(panel.as_panel());
            follow_pointer(panel.as_panel());
            panel.order_front_regardless();
            start_click_through(&handle);
        } else {
            stop_click_through();
            panel.hide();
        }
    });
}

/// Sets the capsule to `width` points, centered in the window. Any thread.
///
/// The window keeps its size; the page lays the capsule out itself, so only the native glass is
/// cut to the new width here.
pub fn set_pill_width(app: &AppHandle, width: f64) {
    let width = width.clamp(PILL_MIN_WIDTH, PILL_MAX_WIDTH);
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        CAPSULE_WIDTH.with(|w| w.set(width));
        if let Ok(panel) = handle.get_webview_panel(PILL_LABEL) {
            fit_glass_to_capsule(panel.as_panel());
        }
    });
}

/// The capsule `capsule_width` points wide, centered in a pill window frame (AppKit coordinates).
fn capsule_rect(window: Rect, capsule_width: f64) -> Rect {
    let width = capsule_width.min(window.2).max(0.0);
    (
        window.0 + (window.2 - width) / 2.0,
        window.1 + PILL_MARGIN,
        width,
        PILL_HEIGHT,
    )
}

/// Cuts the native glass (Liquid Glass, or the frosted fallback before macOS 26) to the capsule, so
/// the rest of the window stays clear. The effect view is added by `set_effects`, possibly after
/// setup, so every show calls this too. Main thread only; does nothing when the glass already fits.
fn fit_glass_to_capsule(window: &NSWindow) {
    let Some(content) = window.contentView() else {
        return;
    };
    let glass_classes: Vec<&AnyClass> = [c"NSGlassEffectView", c"NSVisualEffectView"]
        .into_iter()
        .filter_map(AnyClass::get)
        .collect();
    let bounds = content.bounds();
    let (x, y, w, h) = capsule_rect(
        (0.0, 0.0, bounds.size.width, bounds.size.height),
        CAPSULE_WIDTH.with(Cell::get),
    );
    let target = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
    // Without a width the glass is hidden, never cut to zero: the glass view has a minimum width,
    // and its autoresizing constraints would widen the window to keep it.
    let hidden = w < PILL_MIN_WIDTH;
    for view in content.subviews().iter() {
        if !glass_classes.iter().any(|class| view.isKindOfClass(class)) {
            continue;
        }
        let frame = view.frame();
        if !hidden && (frame.origin != target.origin || frame.size != target.size) {
            view.setFrame(target);
        }
        if view.isHidden() != hidden {
            view.setHidden(hidden);
        }
    }
}

/// While the pill is visible, it takes mouse events only over the capsule, so the rest of the window
/// never swallows a click meant for the app below. AppKit has no per-region switch for a webview
/// window, so the pointer is sampled and `ignoresMouseEvents` follows it. Any thread.
fn start_click_through(app: &AppHandle) {
    let generation = CLICK_THROUGH_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let started = Instant::now();
        while CLICK_THROUGH_GENERATION.load(Ordering::SeqCst) == generation
            && started.elapsed() < CLICK_THROUGH_MAX
        {
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Ok(panel) = handle.get_webview_panel(PILL_LABEL)
                    && panel.is_visible()
                {
                    follow_pointer(panel.as_panel());
                }
            });
            tokio::time::sleep(CLICK_THROUGH_TICK).await;
        }
    });
}

fn stop_click_through() {
    CLICK_THROUGH_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Takes mouse events when the pointer is over the capsule, lets them through otherwise.
/// Main thread only.
fn follow_pointer(window: &NSWindow) {
    let mouse = NSEvent::mouseLocation();
    let capsule = capsule_rect(rect_of(window.frame()), CAPSULE_WIDTH.with(Cell::get));
    let over = contains(capsule, (mouse.x, mouse.y));
    if window.ignoresMouseEvents() == over {
        window.setIgnoresMouseEvents(!over);
    }
}

/// Regular activation policy (Dock icon, Cmd+Tab), show, focus. Any thread.
pub fn show_main(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let _ = handle.set_activation_policy(ActivationPolicy::Regular);
        if let Some(window) = handle.get_webview_window(MAIN_LABEL) {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
    });
}

/// Moves the pill to the bottom center of the screen that holds the mouse. Main thread only.
fn place_pill(panel: &tauri_nspanel::PanelHandle<tauri::Wry>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let mouse = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    let screen = screens
        .iter()
        .find(|s| contains(rect_of(s.frame()), (mouse.x, mouse.y)))
        .or_else(|| NSScreen::mainScreen(mtm));
    let Some(screen) = screen else {
        return;
    };
    let (x, y) = pill_origin(rect_of(screen.visibleFrame()));
    let frame = NSRect::new(
        NSPoint::new(x, y),
        NSSize::new(PILL_WINDOW.0, PILL_WINDOW.1),
    );
    panel.as_panel().setFrame_display(frame, true);
}

/// (x, y, width, height) in the AppKit global coordinate space (origin bottom left).
type Rect = (f64, f64, f64, f64);

fn rect_of(r: NSRect) -> Rect {
    (r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn contains(rect: Rect, point: (f64, f64)) -> bool {
    point.0 >= rect.0 && point.0 < rect.0 + rect.2 && point.1 >= rect.1 && point.1 < rect.1 + rect.3
}

/// Origin of the pill window: centered in `visible`, with the capsule `PILL_BOTTOM_MARGIN` above
/// its bottom edge.
fn pill_origin(visible: Rect) -> (f64, f64) {
    (
        visible.0 + (visible.2 - PILL_WINDOW.0) / 2.0,
        visible.1 + PILL_BOTTOM_MARGIN - PILL_MARGIN,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pill_is_centered_above_the_bottom_edge() {
        // A second screen to the left, with a Dock of 60 pt: the visible area starts at y = 60.
        let visible = (-1920.0, 60.0, 1920.0, 1000.0);
        let (x, y) = pill_origin(visible);
        assert_eq!(x, -1920.0 + (1920.0 - PILL_WINDOW.0) / 2.0);
        // The capsule, not the window, sits 20 pt above the visible area, centered on the screen.
        let capsule = capsule_rect((x, y, PILL_WINDOW.0, PILL_WINDOW.1), 220.0);
        assert_eq!(capsule.1, 80.0);
        assert_eq!((capsule.2, capsule.3), (220.0, PILL_HEIGHT));
        assert_eq!(capsule.0 + capsule.2 / 2.0, -1920.0 + 1920.0 / 2.0);
    }

    #[test]
    fn the_capsule_is_centered_in_the_window() {
        let window = (10.0, 20.0, PILL_WINDOW.0, PILL_WINDOW.1);
        assert_eq!(
            capsule_rect(window, 180.0),
            (
                10.0 + (PILL_WINDOW.0 - 180.0) / 2.0,
                20.0 + PILL_MARGIN,
                180.0,
                PILL_HEIGHT
            )
        );
        // The widest capsule leaves exactly the margin on both sides.
        assert_eq!(capsule_rect(window, PILL_MAX_WIDTH).0, 10.0 + PILL_MARGIN);
        // A capsule wider than the window (never sent: the width is clamped) stays inside it.
        assert_eq!(capsule_rect((0.0, 0.0, 100.0, 80.0), 300.0).2, 100.0);
    }

    #[test]
    fn screen_lookup_uses_half_open_bounds() {
        let a = (0.0, 0.0, 1440.0, 900.0);
        assert!(contains(a, (0.0, 0.0)));
        assert!(contains(a, (1439.9, 899.9)));
        assert!(!contains(a, (1440.0, 10.0)));
        assert!(!contains(a, (-1.0, 10.0)));
    }
}
