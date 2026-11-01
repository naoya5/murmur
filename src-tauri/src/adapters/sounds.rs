//! Adapter: start / stop / cancel / error cues (S4), played with the macOS system sounds.
//!
//! `play` never blocks and never fails loudly: the work is queued on the main thread (NSSound is an AppKit
//! class) and every problem, such as a missing sound, is ignored. The caller checks the `sounds` setting.

use std::ffi::c_void;

use objc2_app_kit::NSSound;
use objc2_foundation::NSString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    Start,
    Stop,
    Cancel,
    Error,
}

impl Cue {
    /// Name of a sound in `/System/Library/Sounds`.
    fn sound_name(self) -> &'static str {
        match self {
            Cue::Start => "Tink",
            Cue::Stop => "Pop",
            Cue::Cancel => "Bottle",
            Cue::Error => "Basso",
        }
    }
}

// Grand Central Dispatch, part of libSystem (always linked).
unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

extern "C" fn play_on_main(context: *mut c_void) {
    // SAFETY: `context` is the pointer from `Box::into_raw` in `play`; GCD runs this exactly once.
    let cue = unsafe { Box::from_raw(context.cast::<Cue>()) };
    play_now(*cue);
}

/// Plays the cue without waiting for it. Any thread. Never panics, never reports an error.
pub fn play(cue: Cue) {
    let context = Box::into_raw(Box::new(cue)).cast::<c_void>();
    // SAFETY: the main queue is a process-wide constant; `dispatch_async_f` only enqueues the function and
    // the boxed cue it owns from then on (freed in `play_on_main`).
    unsafe {
        dispatch_async_f(&raw const _dispatch_main_q, context, play_on_main);
    }
}

/// Main thread only. `soundNamed:` returns a cached object, so a cue that is still playing is restarted.
fn play_now(cue: Cue) {
    let name = NSString::from_str(cue.sound_name());
    if let Some(sound) = NSSound::soundNamed(&name) {
        sound.stop();
        sound.play();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cue_has_a_system_sound() {
        for cue in [Cue::Start, Cue::Stop, Cue::Cancel, Cue::Error] {
            let path = format!("/System/Library/Sounds/{}.aiff", cue.sound_name());
            assert!(std::path::Path::new(&path).exists(), "{path}");
        }
        let names: Vec<_> = [Cue::Start, Cue::Stop, Cue::Cancel, Cue::Error]
            .iter()
            .map(|c| c.sound_name())
            .collect();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "cues must sound different");
    }

    #[test]
    fn nssound_resolves_the_names() {
        for cue in [Cue::Start, Cue::Stop, Cue::Cancel, Cue::Error] {
            let name = NSString::from_str(cue.sound_name());
            assert!(NSSound::soundNamed(&name).is_some(), "{}", cue.sound_name());
        }
    }

    #[test]
    fn play_returns_immediately() {
        let started = std::time::Instant::now();
        for _ in 0..50 {
            play(Cue::Start);
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }
}
