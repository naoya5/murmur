//! Adapter: output. Puts text on the clipboard and pastes it with Cmd+V. For an annotation
//! bundle: the picture first (PNG on the pasteboard, Cmd+V), then the text, or the text alone
//! for a terminal (the picture path is inside it). A bundle goes only into the app that was in
//! front at the hotkey (`frontmost::FrontApp`): it is brought back first, and it must still be
//! in front right before every Cmd+V, or the text stays on the clipboard.
//!
//! Guardrails: the accessibility and secure-input checks come from `platform::macos` (G8). The
//! transcript text is never logged (G7). Nothing here panics (G6).

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use murmur_core::annotations::PasteTarget;
use murmur_core::session::{CopyReason, DeliveryOutcome};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};
use objc2_core_graphics::{CGEventFlags, CGEventSource, CGEventSourceStateID};
use objc2_foundation::NSData;

use crate::adapters::frontmost::{BRING_TO_FRONT_TIMEOUT, FrontApp};
use crate::platform::macos;

/// Upper bound for the whole `deliver` call.
const DELIVER_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound for a bundle: bringing the app back, the modifier wait, two pastes and the gap
/// between them.
pub const BUNDLE_TIMEOUT: Duration = Duration::from_millis(2_500);
/// Between the picture paste and the text paste, so the receiving app keeps both.
pub const IMAGE_TO_TEXT_GAP: Duration = Duration::from_millis(160);
/// The hotkey's modifiers must be up before Cmd+V, or it becomes another shortcut.
pub const MODIFIER_RELEASE_TIMEOUT: Duration = Duration::from_millis(350);
/// How long after the paste the old clipboard text comes back.
const RESTORE_DELAY: Duration = Duration::from_millis(400);
/// Virtual key code of "V" on every ANSI/ISO/JIS layout (kVK_ANSI_V).
const KEY_V: u16 = 9;

#[derive(Debug, Clone, Copy)]
pub struct DeliverOptions {
    pub auto_paste: bool,
    pub restore_clipboard: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("clipboard unavailable: {0}")]
    Clipboard(String),
    #[error("could not post the paste keystroke")]
    Paste,
}

pub fn copy_text(text: &str) -> Result<(), OutputError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| OutputError::Clipboard(e.to_string()))?;
    clipboard
        .set_text(text)
        .map_err(|e| OutputError::Clipboard(e.to_string()))
}

/// Copy the text and, when allowed, paste it into the focused app.
///
/// Blocking; returns within `DELIVER_TIMEOUT` (`Unconfirmed` on timeout). Safe to call from any
/// thread. If the clipboard itself cannot be written there is no better outcome in the shared
/// type, so it also reports `Unconfirmed`.
pub fn deliver(text: &str, opts: DeliverOptions) -> DeliveryOutcome {
    let (tx, rx) = mpsc::channel();
    let text = text.to_owned();
    let spawned = std::thread::Builder::new()
        .name("deliver".into())
        .spawn(move || {
            let _ = tx.send(deliver_blocking(&text, opts));
        });
    if spawned.is_err() {
        return DeliveryOutcome::Unconfirmed;
    }
    rx.recv_timeout(DELIVER_TIMEOUT)
        .unwrap_or(DeliveryOutcome::Unconfirmed)
}

fn deliver_blocking(text: &str, opts: DeliverOptions) -> DeliveryOutcome {
    // Decide first, so the previous clipboard is only read when we will paste.
    let skip_reason = if !opts.auto_paste {
        Some(CopyReason::AutoPasteOff)
    } else if !macos::accessibility_trusted() {
        Some(CopyReason::NoAccessibility)
    } else if macos::secure_input_enabled() {
        Some(CopyReason::SecureInput)
    } else {
        None
    };

    let previous = if skip_reason.is_none() && opts.restore_clipboard {
        read_clipboard_text()
    } else {
        None
    };

    if copy_text(text).is_err() {
        return DeliveryOutcome::Unconfirmed;
    }
    if let Some(reason) = skip_reason {
        return DeliveryOutcome::Copied { reason };
    }
    if post_cmd_v().is_err() {
        return DeliveryOutcome::Copied {
            reason: CopyReason::PasteFailed,
        };
    }
    if let Some(previous) = previous {
        schedule_restore(text.to_owned(), previous);
    }
    DeliveryOutcome::Pasted
}

/// Pastes an annotation bundle into `front`, the app that was in front at the hotkey.
///
/// The app is brought back to the front first (`BRING_TO_FRONT_TIMEOUT`). Terminal: the text
/// alone, once. Other apps: the PNG, Cmd+V, `IMAGE_TO_TEXT_GAP`, the text, Cmd+V. Before the
/// first Cmd+V the held modifiers may take `MODIFIER_RELEASE_TIMEOUT` to go up; right before
/// each Cmd+V the app must still be in front. Blocking; returns within `BUNDLE_TIMEOUT`
/// (`Unconfirmed` on timeout). When the paste is not allowed, or the app is not in front, the
/// text goes to the clipboard and the outcome says why. No `front` (Murmur itself was in
/// front at the hotkey): the text is copied only.
pub fn deliver_bundle(
    png: Option<&Path>,
    text: &str,
    opts: DeliverOptions,
    target: PasteTarget,
    front: Option<FrontApp>,
) -> DeliveryOutcome {
    let (tx, rx) = mpsc::channel();
    let text = text.to_owned();
    let png = png.map(Path::to_path_buf);
    let spawned = std::thread::Builder::new()
        .name("deliver-bundle".into())
        .spawn(move || {
            let _ = tx.send(deliver_bundle_blocking(
                png.as_deref(),
                &text,
                opts,
                target,
                front.as_ref(),
            ));
        });
    if spawned.is_err() {
        return DeliveryOutcome::Unconfirmed;
    }
    rx.recv_timeout(BUNDLE_TIMEOUT)
        .unwrap_or(DeliveryOutcome::Unconfirmed)
}

fn deliver_bundle_blocking(
    png: Option<&Path>,
    text: &str,
    opts: DeliverOptions,
    target: PasteTarget,
    front: Option<&FrontApp>,
) -> DeliveryOutcome {
    let skip_reason = if !opts.auto_paste {
        Some(CopyReason::AutoPasteOff)
    } else if !macos::accessibility_trusted() {
        Some(CopyReason::NoAccessibility)
    } else if macos::secure_input_enabled() {
        Some(CopyReason::SecureInput)
    } else if front.is_none() {
        Some(CopyReason::TargetNotInFront)
    } else {
        None
    };
    if let Some(reason) = skip_reason {
        if copy_text(text).is_err() {
            return DeliveryOutcome::Unconfirmed;
        }
        return DeliveryOutcome::Copied { reason };
    }
    let Some(front) = front else {
        return DeliveryOutcome::Unconfirmed;
    };
    if !front.bring_to_front(BRING_TO_FRONT_TIMEOUT) {
        if copy_text(text).is_err() {
            return DeliveryOutcome::Unconfirmed;
        }
        return DeliveryOutcome::Copied {
            reason: CopyReason::TargetNotInFront,
        };
    }
    let previous = if opts.restore_clipboard {
        read_clipboard_text()
    } else {
        None
    };
    wait_modifiers_released(MODIFIER_RELEASE_TIMEOUT);

    // The picture first, for apps that take one.
    if let (PasteTarget::Other, Some(png)) = (target, png) {
        match set_clipboard_png(png) {
            Ok(()) => {
                if let Err(reason) = paste_into(front) {
                    let _ = copy_text(text);
                    return DeliveryOutcome::Copied { reason };
                }
                std::thread::sleep(IMAGE_TO_TEXT_GAP);
            }
            Err(error) => tracing::warn!(%error, "picture not put on the pasteboard"),
        }
    }
    if copy_text(text).is_err() {
        return DeliveryOutcome::Unconfirmed;
    }
    if let Err(reason) = paste_into(front) {
        return DeliveryOutcome::Copied { reason };
    }
    if let Some(previous) = previous {
        schedule_restore(text.to_owned(), previous);
    }
    DeliveryOutcome::Pasted
}

/// Cmd+V into `front`, only if it is the app in front right now.
fn paste_into(front: &FrontApp) -> Result<(), CopyReason> {
    if !front.is_in_front() {
        tracing::info!("another app came to the front: paste stopped");
        return Err(CopyReason::TargetNotInFront);
    }
    post_cmd_v().map_err(|_| CopyReason::PasteFailed)
}

/// Whether any of Option, Shift, Command or Control is down in these flags.
pub fn modifiers_held(flags: CGEventFlags) -> bool {
    flags.intersects(
        CGEventFlags::MaskAlternate
            | CGEventFlags::MaskShift
            | CGEventFlags::MaskCommand
            | CGEventFlags::MaskControl,
    )
}

/// Waits until no modifier is held, at most `limit`.
fn wait_modifiers_released(limit: Duration) {
    let deadline = Instant::now() + limit;
    while modifiers_held(CGEventSource::flags_state(
        CGEventSourceStateID::CombinedSessionState,
    )) && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Puts a PNG file on the general pasteboard as the only item.
fn set_clipboard_png(path: &Path) -> Result<(), OutputError> {
    let bytes = std::fs::read(path).map_err(|e| OutputError::Clipboard(e.to_string()))?;
    let data = NSData::with_bytes(&bytes);
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: `NSPasteboardTypePNG` is an immortal AppKit constant; the data lives for the call.
    let ok = unsafe { pasteboard.setData_forType(Some(&data), NSPasteboardTypePNG) };
    if ok {
        Ok(())
    } else {
        Err(OutputError::Clipboard(
            "pasteboard refused the picture".into(),
        ))
    }
}

fn read_clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// After `RESTORE_DELAY`, put `previous` back, but only if the clipboard still holds `ours`
/// (the user may have copied something else in the meantime).
fn schedule_restore(ours: String, previous: String) {
    let _ = std::thread::Builder::new()
        .name("clipboard-restore".into())
        .spawn(move || {
            std::thread::sleep(RESTORE_DELAY);
            if read_clipboard_text().as_deref() == Some(ours.as_str()) {
                let _ = copy_text(&previous);
            }
        });
}

/// Post Cmd+V (key down, key up) to the HID event stream. CGEventPost is thread-safe.
fn post_cmd_v() -> Result<(), OutputError> {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|()| OutputError::Paste)?;
    for key_down in [true, false] {
        let event = CGEvent::new_keyboard_event(source.clone(), KEY_V, key_down)
            .map_err(|()| OutputError::Paste)?;
        event.set_flags(CGEventFlags::CGEventFlagCommand);
        event.post(CGEventTapLocation::HID);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_predicate_covers_the_hotkey_keys_only() {
        assert!(!modifiers_held(CGEventFlags::empty()));
        assert!(modifiers_held(CGEventFlags::MaskShift));
        assert!(modifiers_held(
            CGEventFlags::MaskAlternate | CGEventFlags::MaskShift
        ));
        assert!(modifiers_held(CGEventFlags::MaskCommand));
        assert!(modifiers_held(CGEventFlags::MaskControl));
        // Caps Lock and the function key do not block a paste.
        assert!(!modifiers_held(
            CGEventFlags::MaskAlphaShift | CGEventFlags::MaskSecondaryFn
        ));
        assert_eq!(IMAGE_TO_TEXT_GAP, Duration::from_millis(160));
        assert_eq!(MODIFIER_RELEASE_TIMEOUT, Duration::from_millis(350));
        assert!(
            BUNDLE_TIMEOUT > BRING_TO_FRONT_TIMEOUT + MODIFIER_RELEASE_TIMEOUT + IMAGE_TO_TEXT_GAP
        );
        assert!(BUNDLE_TIMEOUT < Duration::from_millis(3_000));
    }

    /// Writes to the real clipboard, so it is opt-in: `cargo test -p murmur -- --ignored png`.
    #[test]
    #[ignore]
    fn png_reaches_the_pasteboard() {
        use crate::adapters::image::{Bitmap, write_png};
        let dir = std::env::temp_dir().join(format!("murmur-png-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.png");
        write_png(&Bitmap::new(8, 8).unwrap(), &path).unwrap();
        let previous = read_clipboard_text();
        set_clipboard_png(&path).unwrap();
        let pasteboard = NSPasteboard::generalPasteboard();
        // SAFETY: an immortal AppKit constant.
        let data = unsafe { pasteboard.dataForType(NSPasteboardTypePNG) };
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(data.map(|d| d.len()), Some(bytes.len()));
        if let Some(previous) = previous {
            copy_text(&previous).unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Writes to the real clipboard, so it is opt-in: `cargo test -p murmur -- --ignored`.
    #[test]
    #[ignore = "uses the real clipboard"]
    fn copy_text_reaches_the_clipboard() {
        copy_text("murmur clipboard test").unwrap();
        assert_eq!(
            read_clipboard_text().as_deref(),
            Some("murmur clipboard test")
        );
    }

    /// Runs an AppleScript and returns what it printed.
    fn osa(script: &str) -> String {
        let out = std::process::Command::new("osascript")
            .args(["-e", script])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A plain 320 x 180 PNG in a fresh scratch folder (a composer may refuse a tiny picture).
    fn scratch_png() -> std::path::PathBuf {
        use crate::adapters::image::{Bitmap, write_png};
        let dir = std::env::temp_dir().join(format!("murmur-bundle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("picture.png");
        write_png(&Bitmap::new(320, 180).unwrap(), &path).unwrap();
        path
    }

    /// Where the screen pictures of the GUI tests go (`MURMUR_TEST_SHOTS`, else the temp dir).
    fn shots_dir() -> std::path::PathBuf {
        let dir = std::env::var_os("MURMUR_TEST_SHOTS")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn screenshot(name: &str) -> std::path::PathBuf {
        let path = shots_dir().join(name);
        let _ = std::process::Command::new("screencapture")
            .args(["-x", &path.to_string_lossy()])
            .status();
        path
    }

    const PASTE_OPTS: DeliverOptions = DeliverOptions {
        auto_paste: true,
        restore_clipboard: false,
    };

    /// Activates the app and remembers it; fails (without pasting anything) when another app
    /// is in front afterwards, so a GUI test never pastes into the wrong window.
    fn front_app(app: &str, bundle_id: &str) -> FrontApp {
        osa(&format!(r#"tell application "{app}" to activate"#));
        std::thread::sleep(Duration::from_millis(1500));
        let front = crate::adapters::frontmost::capture().expect("an app in front");
        assert_eq!(
            front.bundle_id.as_deref(),
            Some(bundle_id),
            "{app} is not in front: nothing pasted"
        );
        front
    }

    /// End to end into TextEdit. Reports the outcome; the document text is read back with
    /// AppleScript. Needs accessibility permission for the host terminal to paste.
    #[test]
    #[ignore = "drives TextEdit and the real clipboard"]
    fn deliver_into_textedit() {
        osa(r#"tell application "TextEdit" to activate
tell application "TextEdit" to make new document"#);
        std::thread::sleep(Duration::from_millis(1500));
        let started = std::time::Instant::now();
        let outcome = deliver(
            "murmur delivery test",
            DeliverOptions {
                auto_paste: true,
                restore_clipboard: false,
            },
        );
        println!("outcome = {outcome:?} in {:?}", started.elapsed());
        std::thread::sleep(Duration::from_millis(600));
        let doc = osa(r#"tell application "TextEdit" to get text of front document"#);
        println!("document = {doc:?}");
        println!("clipboard = {:?}", read_clipboard_text());
        osa(r#"tell application "TextEdit" to close front document saving no"#);
        match outcome {
            DeliveryOutcome::Pasted => assert_eq!(doc, "murmur delivery test"),
            _ => assert_eq!(
                read_clipboard_text().as_deref(),
                Some("murmur delivery test")
            ),
        }
    }

    /// A picture plus the words into TextEdit (`deliver_bundle`, `PasteTarget::Other`): both
    /// pastes must land, so the document text must hold the words after the picture.
    #[test]
    #[ignore = "drives TextEdit and the real clipboard"]
    fn deliver_bundle_into_textedit() {
        let png = scratch_png();
        osa(r#"tell application "TextEdit" to make new document"#);
        let front = front_app("TextEdit", "com.apple.TextEdit");
        let started = std::time::Instant::now();
        let outcome = deliver_bundle(
            Some(&png),
            "murmur bundle test words",
            PASTE_OPTS,
            PasteTarget::Other,
            Some(front),
        );
        println!("outcome = {outcome:?} in {:?}", started.elapsed());
        std::thread::sleep(Duration::from_millis(800));
        let doc = osa(r#"tell application "TextEdit" to get text of front document"#);
        println!("document = {doc:?}");
        osa(r#"tell application "TextEdit" to close front document saving no"#);
        let _ = std::fs::remove_dir_all(png.parent().unwrap());
        assert_eq!(outcome, DeliveryOutcome::Pasted);
        assert!(
            doc.contains("murmur bundle test words"),
            "the words did not land"
        );
        // A pasted picture shows up in the document text as the attachment character.
        assert!(doc.contains('\u{FFFC}'), "the picture did not land");
    }

    /// A picture plus the words into the composer of Claude.app. The result is a screen picture
    /// (`MURMUR_TEST_SHOTS`) for a person to look at; the composer text is cleared afterwards
    /// with Cmd+A and Delete (never Enter: nothing is sent). A picture chip may stay and needs a
    /// click by hand.
    #[test]
    #[ignore = "drives Claude.app on the user's screen"]
    fn deliver_bundle_into_claude_app() {
        let png = scratch_png();
        let front = front_app("Claude", "com.anthropic.claudefordesktop");
        let started = std::time::Instant::now();
        let outcome = deliver_bundle(
            Some(&png),
            "murmur bundle test words",
            PASTE_OPTS,
            PasteTarget::Other,
            Some(front),
        );
        println!("outcome = {outcome:?} in {:?}", started.elapsed());
        std::thread::sleep(Duration::from_millis(1500));
        let after = screenshot("claude-after-paste.png");
        osa(r#"tell application "System Events" to tell process "Claude"
keystroke "a" using command down
key code 51
end tell"#);
        std::thread::sleep(Duration::from_millis(500));
        let cleared = screenshot("claude-after-clear.png");
        println!("pictures: {} and {}", after.display(), cleared.display());
        let _ = std::fs::remove_dir_all(png.parent().unwrap());
        assert_eq!(outcome, DeliveryOutcome::Pasted);
    }

    /// The bundle goes back to TextEdit although Finder is in front at delivery time (the
    /// user clicked elsewhere while recording). The test process is in the background, like
    /// Murmur, so this also shows whether macOS lets a background app activate another one.
    #[test]
    #[ignore = "drives TextEdit and Finder on the user's screen"]
    fn deliver_bundle_returns_to_textedit_from_behind_finder() {
        let png = scratch_png();
        osa(r#"tell application "TextEdit" to make new document"#);
        let front = front_app("TextEdit", "com.apple.TextEdit");
        osa(r#"tell application "Finder" to activate"#);
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!front.is_in_front(), "Finder did not come to the front");
        let started = std::time::Instant::now();
        let outcome = deliver_bundle(
            Some(&png),
            "murmur bundle test words",
            PASTE_OPTS,
            PasteTarget::Other,
            Some(front),
        );
        println!("outcome = {outcome:?} in {:?}", started.elapsed());
        std::thread::sleep(Duration::from_millis(800));
        let doc = osa(r#"tell application "TextEdit" to get text of front document"#);
        println!("document = {doc:?}");
        osa(r#"tell application "TextEdit" to close front document saving no"#);
        let _ = std::fs::remove_dir_all(png.parent().unwrap());
        assert_eq!(outcome, DeliveryOutcome::Pasted);
        assert!(
            doc.contains("murmur bundle test words"),
            "the words did not land"
        );
        assert!(doc.contains('\u{FFFC}'), "the picture did not land");
    }

    /// Without a remembered app nothing is pasted: the words go to the clipboard.
    #[test]
    #[ignore = "uses the real clipboard"]
    fn deliver_bundle_without_an_app_only_copies() {
        let outcome = deliver_bundle(
            None,
            "murmur bundle copy only",
            PASTE_OPTS,
            PasteTarget::Other,
            None,
        );
        assert_eq!(
            outcome,
            DeliveryOutcome::Copied {
                reason: CopyReason::TargetNotInFront
            }
        );
        assert_eq!(
            read_clipboard_text().as_deref(),
            Some("murmur bundle copy only")
        );
    }
}
