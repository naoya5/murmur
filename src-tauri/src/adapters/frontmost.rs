//! Adapter: the app in front. An annotation is pasted into the app that was in front when its
//! hotkey was pressed, not into whatever is in front at the stop (the user clicks around while
//! recording). `capture` remembers that app (pid and bundle id at once; the element that had
//! the focus a moment later, on its own thread, so the recording does not wait for an
//! accessibility call); `bring_to_front` activates it again, waits for it with a timeout (G5)
//! and puts the focus back; `is_in_front` is checked right before every paste keystroke, so a
//! keystroke never lands in another app. "In front" means the app is the active one: the one
//! the keyboard goes to, from `NSWorkspace`, which the main run loop keeps fresh. (The window
//! list is not consulted: quick-entry panels and floating inputs are not ordinary windows, yet
//! they take a paste.)
//!
//! Accessibility is checked right before its first use, through `platform::macos` (G8). Bundle
//! ids and element names never reach the log (G7). Nothing here panics (G6).

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
use objc2_core_foundation::CFString;

use crate::platform::macos;

/// How long the remembered app may take to come back to the front.
pub const BRING_TO_FRONT_TIMEOUT: Duration = Duration::from_millis(500);
/// An accessibility call into an app that does not answer gives up after this long.
const AX_MESSAGE_TIMEOUT_S: f32 = 0.2;
/// `kAXErrorSuccess`.
const AX_SUCCESS: i32 = 0;

// SAFETY (all declarations below): plain C functions of the system frameworks named in `link`,
// with the signatures of the Apple headers (HIServices/AXUIElement.h, CFBase.h). They are
// thread-safe to call.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// `AXUIElementRef AXUIElementCreateApplication(pid_t pid)` (a +1 reference)
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    /// `AXError AXUIElementCopyAttributeValue(AXUIElementRef, CFStringRef, CFTypeRef *)`
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    /// `AXError AXUIElementSetAttributeValue(AXUIElementRef, CFStringRef, CFTypeRef)`
    fn AXUIElementSetAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *const c_void,
    ) -> i32;
    /// `AXError AXUIElementSetMessagingTimeout(AXUIElementRef, float seconds)`
    fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    /// `void CFRelease(CFTypeRef)`
    fn CFRelease(cf: *const c_void);
    /// `CFTypeRef CFRetain(CFTypeRef)`
    fn CFRetain(cf: *const c_void) -> *const c_void;
}

/// A retained accessibility element (`AXUIElementRef`), released on drop.
struct AxElement(*const c_void);

// SAFETY: an AXUIElementRef is an immutable CoreFoundation object; retain, release and the AX
// calls on it may happen from any thread.
unsafe impl Send for AxElement {}
unsafe impl Sync for AxElement {}

impl Drop for AxElement {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a +1 AX call and is released exactly once.
        unsafe { CFRelease(self.0) };
    }
}

impl std::fmt::Debug for AxElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AxElement")
    }
}

/// The app that was in front at the hotkey: where the annotation goes.
#[derive(Debug)]
pub struct FrontApp {
    pub pid: i32,
    pub bundle_id: Option<String>,
    /// Filled by the `front-focus` thread shortly after `capture`; empty until then, or when
    /// accessibility is not granted or the app has no focused element.
    focused: Arc<Mutex<Option<AxElement>>>,
}

/// The pid of the active app: the one the keyboard goes to (`NSWorkspace`, which the main run
/// loop keeps up to date; on a thread of a process without a running main loop it may lag).
pub fn active_pid() -> Option<i32> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    Some(app.processIdentifier())
}

/// Remembers the app in front. `None` when it is Murmur itself (there is nowhere to paste) or
/// when no app is in front.
pub fn capture() -> Option<FrontApp> {
    let pid = active_pid()?;
    if pid == std::process::id() as i32 {
        return None;
    }
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
    let focused: Arc<Mutex<Option<AxElement>>> = Arc::new(Mutex::new(None));
    // The focused element is read on its own thread (an accessibility call can take up to
    // `AX_MESSAGE_TIMEOUT_S`), only with the permission in hand (G8); without it the app still
    // comes back to the front, and restores its own focus.
    let slot = focused.clone();
    let spawned = std::thread::Builder::new()
        .name("front-focus".into())
        .spawn(move || {
            let element = macos::accessibility_trusted()
                .then(|| focused_element(pid))
                .flatten();
            if let Ok(mut slot) = slot.lock() {
                *slot = element;
            }
        });
    if spawned.is_err() {
        tracing::debug!("focus of the app in front not read");
    }
    Some(FrontApp {
        pid,
        bundle_id,
        focused,
    })
}

fn ax_string(name: &'static str) -> objc2_core_foundation::CFRetained<CFString> {
    CFString::from_static_str(name)
}

fn focused_element(pid: i32) -> Option<AxElement> {
    // SAFETY: a +1 reference, released below; the attribute string lives for the call.
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let _ = AXUIElementSetMessagingTimeout(app, AX_MESSAGE_TIMEOUT_S);
        let attribute = ax_string("AXFocusedUIElement");
        let mut value: *const c_void = std::ptr::null();
        let error = AXUIElementCopyAttributeValue(
            app,
            &*attribute as *const CFString as *const c_void,
            &mut value,
        );
        CFRelease(app);
        if error == AX_SUCCESS && !value.is_null() {
            let _ = AXUIElementSetMessagingTimeout(value, AX_MESSAGE_TIMEOUT_S);
            Some(AxElement(value))
        } else {
            None
        }
    }
}

impl FrontApp {
    /// Whether the remembered app is the active one right now.
    pub fn is_in_front(&self) -> bool {
        active_pid() == Some(self.pid)
    }

    /// Brings the remembered app back to the front, waiting at most `timeout`, and gives the
    /// focus back to the element that had it. Returns whether the app is in front afterwards.
    /// It stays `false` when the app has quit or when macOS refuses the activation.
    pub fn bring_to_front(&self, timeout: Duration) -> bool {
        if !self.is_in_front() {
            let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(self.pid)
            else {
                tracing::info!("the app of the annotation is gone");
                return false;
            };
            if app.isTerminated() {
                tracing::info!("the app of the annotation has quit");
                return false;
            }
            let asked = app.activateWithOptions(NSApplicationActivationOptions::empty());
            let deadline = Instant::now() + timeout;
            while !self.is_in_front() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !self.is_in_front() {
                tracing::info!(asked, "the app of the annotation did not come to the front");
                return false;
            }
        }
        let focused = self.focused.lock().ok().and_then(|slot| {
            // SAFETY: a retained element stays valid; the copy is released on its own.
            slot.as_ref().map(|e| AxElement(unsafe { CFRetain(e.0) }))
        });
        if let Some(focused) = focused {
            let attribute = ax_string("AXFocused");
            // SAFETY: the element is retained by `self`; the attribute and the constant live for
            // the call. A failure only means the app keeps the focus it chose itself.
            let error = unsafe {
                AXUIElementSetAttributeValue(
                    focused.0,
                    &*attribute as *const CFString as *const c_void,
                    objc2_core_foundation::kCFBooleanTrue
                        .map_or(std::ptr::null(), |b| b as *const _ as *const c_void),
                )
            };
            if error != AX_SUCCESS {
                tracing::debug!(error, "focus not restored");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur_itself_is_never_a_target() {
        // The test process is in front only when run from a terminal that is; either way the
        // capture must not name this process.
        if let Some(front) = capture() {
            assert_ne!(front.pid, std::process::id() as i32);
        }
    }

    /// The active app is the one System Events names (read only: nothing is activated).
    /// Skipped where System Events does not answer (no session, no automation permission), on CI
    /// (`CI` is set: nobody uses a hosted runner's desktop, and there NSWorkspace and System
    /// Events name two different processes), while the screen is locked (NSWorkspace then names
    /// loginwindow, but System Events still names the app from before the lock), and when the
    /// front app changes between the reads.
    #[test]
    fn front_app_agrees_with_system_events() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        let truth = || {
            std::process::Command::new("osascript")
                .args(["-e", r#"tell application "System Events" to get unix id of first process whose frontmost is true"#])
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<i32>().ok())
        };
        let is_login_window = |pid: i32| {
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .and_then(|app| app.bundleIdentifier())
                .is_some_and(|id| id.to_string() == "com.apple.loginwindow")
        };
        let Some(before) = truth() else {
            return;
        };
        let active = active_pid();
        if active.is_some_and(is_login_window) || truth() != Some(before) {
            return;
        }
        assert_eq!(active, Some(before));
    }

    /// Needs the screen: TextEdit comes back although Safari was brought in front, with the
    /// test process in the background like Murmur. Run only when nobody is using the Mac.
    #[test]
    #[ignore = "activates TextEdit and Safari on the user's screen"]
    fn textedit_comes_back_to_the_front_from_behind_safari() {
        let osa = |script: &str| -> String {
            std::process::Command::new("osascript")
                .args(["-e", script])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .unwrap_or_default()
        };
        // What System Events says is in front (the truth outside this process).
        let truth = || {
            osa(
                r#"tell application "System Events" to get bundle identifier of first process whose frontmost is true"#,
            )
        };
        let open = |app: &str| {
            let _ = std::process::Command::new("open")
                .args(["-a", app])
                .status();
            std::thread::sleep(Duration::from_millis(1200));
        };
        open("TextEdit");
        let front = capture().expect("an app in front");
        assert_eq!(
            front.bundle_id.as_deref(),
            Some("com.apple.TextEdit"),
            "TextEdit did not come to the front: nothing is tried"
        );
        open("Safari");
        println!("after the switch: {}", truth());
        assert!(
            !front.is_in_front(),
            "Safari did not come to the front: nothing is tried"
        );
        let started = Instant::now();
        let back = front.bring_to_front(BRING_TO_FRONT_TIMEOUT);
        println!(
            "back = {back} in {:?}, now in front: {}",
            started.elapsed(),
            truth()
        );
        assert!(back, "macOS refused the activation from the background");
        assert_eq!(truth(), "com.apple.TextEdit");
    }
}
