//! macOS integration: permissions, secure input, privacy panes, activation policy, NSPanel.
//!
//! Guardrail G8: this file holds the ONE function per permission. The controller and the first-run
//! guide both call these functions, right before the permission is needed.
#![allow(dead_code)] // Wired by T2-8 (controller); remove then.

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use serde::{Deserialize, Serialize};

/// How long `request_microphone` waits for the user to answer the system prompt (G5).
const MICROPHONE_PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// State of one macOS privacy permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
pub enum Permission {
    Granted,
    Denied,
    NotDetermined,
}

/// Which pane of System Settings > Privacy & Security to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum PrivacyPane {
    Microphone,
    Accessibility,
}

impl PrivacyPane {
    fn url(self) -> &'static str {
        match self {
            PrivacyPane::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            PrivacyPane::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
        }
    }
}

// SAFETY (all declarations below): these are plain C functions and constants exported by the
// system frameworks named in `link`. The signatures match the Apple headers
// (HIServices/AXUIElement.h, HIToolbox/CarbonEventsCore.h). They are thread-safe to call.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// `Boolean AXIsProcessTrustedWithOptions(CFDictionaryRef options)`
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    /// `CFStringRef kAXTrustedCheckOptionPrompt`, an immortal constant string.
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// `Boolean IsSecureEventInputEnabled(void)`
    fn IsSecureEventInputEnabled() -> u8;
}

/// Current microphone permission. Never prompts.
pub fn microphone_permission() -> Permission {
    // SAFETY: `AVMediaTypeAudio` is an immortal extern constant that is `Some` on every macOS
    // version we support (10.14+). `authorizationStatusForMediaType` is a class method with no
    // preconditions and is safe from any thread.
    let status = unsafe {
        match AVMediaTypeAudio {
            Some(media_type) => AVCaptureDevice::authorizationStatusForMediaType(media_type),
            None => return Permission::Denied,
        }
    };
    if status == AVAuthorizationStatus::Authorized {
        Permission::Granted
    } else if status == AVAuthorizationStatus::NotDetermined {
        Permission::NotDetermined
    } else {
        // Denied or Restricted (parental controls / MDM): the user cannot grant it in the prompt.
        Permission::Denied
    }
}

/// Ask for the microphone. Shows the system prompt only when the state is `NotDetermined`.
/// Does not block the caller's thread (the wait runs on the blocking pool) and gives up after
/// `MICROPHONE_PROMPT_TIMEOUT`, returning the state at that moment.
pub async fn request_microphone() -> Permission {
    match microphone_permission() {
        Permission::NotDetermined => {}
        other => return other,
    }
    let waited = tauri::async_runtime::spawn_blocking(|| {
        let (tx, rx) = mpsc::channel::<bool>();
        let handler = RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        // SAFETY: the media type constant is valid (see `microphone_permission`). The block is
        // copied by AVFoundation, so it may outlive this stack frame; it only owns a `Sender`.
        unsafe {
            if let Some(media_type) = AVMediaTypeAudio {
                AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
            }
        }
        // The handler fires on an arbitrary queue; a timeout covers a prompt nobody answers.
        let _ = rx.recv_timeout(MICROPHONE_PROMPT_TIMEOUT);
    })
    .await;
    if waited.is_err() {
        eprintln!("microphone request task failed");
    }
    microphone_permission()
}

/// Whether this process may control the computer (needed to post Cmd+V). Never prompts.
pub fn accessibility_trusted() -> bool {
    // SAFETY: a null options dictionary is documented as "no options".
    unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) != 0 }
}

/// Show the system Accessibility prompt (once per app build) and register the app in the list.
pub fn request_accessibility() {
    // SAFETY: `kAXTrustedCheckOptionPrompt` is a CFString constant, and CFString is toll-free
    // bridged to NSString, so reading it as `&NSString` is valid for the process lifetime. The
    // NSDictionary is toll-free bridged to CFDictionary and outlives the call.
    unsafe {
        let key: &NSString = &*(kAXTrustedCheckOptionPrompt as *const NSString);
        let yes = NSNumber::new_bool(true);
        let options: objc2::rc::Retained<NSDictionary<NSString, NSNumber>> =
            NSDictionary::from_slices(&[key], &[&*yes]);
        AXIsProcessTrustedWithOptions(&*options as *const _ as *const c_void);
    }
}

/// Whether some app has enabled secure event input (a password field has focus). While it is
/// on, synthesized key events are dropped, so pasting with Cmd+V would silently do nothing.
pub fn secure_input_enabled() -> bool {
    // SAFETY: no arguments, no preconditions.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// Open the matching pane of System Settings. Failure to launch is ignored: it is a convenience.
pub fn open_privacy_pane(pane: PrivacyPane) {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(pane.url())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_urls_point_to_the_security_pane() {
        for pane in [PrivacyPane::Microphone, PrivacyPane::Accessibility] {
            assert!(
                pane.url().starts_with(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_"
                )
            );
        }
    }

    /// The queries must not crash or prompt. The values depend on the machine.
    #[test]
    fn permission_queries_do_not_crash() {
        let _ = microphone_permission();
        let _ = accessibility_trusted();
        let _ = secure_input_enabled();
    }
}
