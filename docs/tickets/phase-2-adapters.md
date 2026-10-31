# Sprint 2b (PLAN Phase 2, adapters)

Rules: [RULES.md](./RULES.md). Architecture: [../ARCHITECTURE.md](../ARCHITECTURE.md).
The controller (T2-8, orchestrator) codes against the interfaces below. Keep the public signatures; if one must
change, say so in the report. Internal design is yours.

| Ticket | Owner | Status |
|---|---|---|
| T2-5 `adapters/audio.rs` (capture, resample, level, spool, debug WAV device) | audio-dev | todo |
| T2-6 `crates/speech-sys` in repo + `adapters/engine.rs` + `inference.rs` | speech-sys-dev | todo |
| T2-7 `platform/macos.rs` permissions + `adapters/output.rs` + `adapters/hotkeys.rs` | platform-dev | review |
| T2-8 `controller.rs`, `windows.rs`, wiring, minimal pill, debug triggers, E2E | orchestrator | todo |

## T2-5 audio

```rust
pub struct AudioConfig { pub device: Option<String>, pub spool_path: std::path::PathBuf }
pub enum AudioEvent { Level(f32), Disconnected }          // Level is 0.0..=1.0, about 30 per second
pub struct Recorder { /* Send + Sync handle; the cpal stream lives on its own thread */ }
impl Recorder {
    /// Opens the device (or the system default; falls back to default if the named one is gone) and starts.
    /// Returns within 3 s (`AudioError::OpenTimeout`). Does NOT check permissions (the controller does, G8).
    pub fn start(cfg: AudioConfig, on_event: Box<dyn Fn(AudioEvent) + Send + Sync>) -> Result<Recorder, AudioError>;
    pub fn set_paused(&self, paused: bool);                  // paused audio is dropped, stream stays open
    pub fn stop(self) -> Result<CapturedAudio, AudioError>;  // closes the stream; flushes the spool
}
pub struct CapturedAudio { pub pcm: Vec<f32>, pub spool_path: std::path::PathBuf, pub duration_ms: u64 }
pub struct InputDevice { pub name: String, pub is_default: bool }   // derive Serialize + specta::Type
pub fn input_devices() -> Vec<InputDevice>;
pub fn read_spool(path: &std::path::Path) -> Result<Vec<f32>, AudioError>;
#[derive(thiserror::Error)] pub enum AudioError { NoDevice, OpenTimeout, Device(String), Io(String) }
```

Acceptance:
- cpal 0.18 + rubato 5 (or an equally good resampler) -> 16 kHz mono f32. Downmix by averaging channels.
- The cpal callback does no locking and no allocation: it writes into a pre-allocated lock-free ring buffer
  (`ringbuf` or equivalent). A converter thread resamples, appends to the PCM buffer, appends s16le to the
  spool file (flush at least every 250 ms so a crash loses at most that much), and emits levels (RMS mapped
  to 0..1 with a perceptual curve) at ~30 fps.
- Device errors/disconnect while recording -> `AudioEvent::Disconnected` once; `stop` still returns the audio
  captured so far.
- Debug builds only (`cfg(debug_assertions)`): `device = Some("wav:<path>")` plays that WAV (any rate/channels,
  use `hound`) in real time through the same converter path, then silence until stopped. Release builds treat
  it as an unknown device.
- Tests: resample accuracy (a 1 kHz sine at 48 kHz -> 16 kHz keeps frequency/energy), downmix, spool round
  trip via `read_spool`, the WAV device end to end (start, wait, stop, compare duration and content). Real
  microphone test `#[ignore]`.
- Measure and report the time from `start` to the first callback with the built-in microphone (PLAN 2.14 goal:
  hotkey -> recording start <= 150 ms). If the terminal has no microphone permission, report that instead.

## T2-6 engine + inference

```rust
// src-tauri/src/inference.rs (assembly layer, uses only core ports)
pub struct TranscribeRequest {
    pub pcm: std::sync::Arc<Vec<f32>>, pub language: Language, pub model: ModelSpec, pub vad: ModelSpec,
    pub vocabulary: Vec<DictionaryEntry>, pub cancel: CancelToken,
}
pub struct InferenceOutput {
    pub raw: Option<RawTranscript> /* None = no speech */, pub asr_ms: u64, pub model_id: &'static str,
    pub custom_vocabulary: bool, // the model used the dictionary itself (Haas keywords); the text rules then skip replacements
}
#[derive(Debug, thiserror::Error)]
pub enum InferenceError { ModelMissing, ModelLoad(EngineError), Engine(EngineError), Audio(String), Cancelled, Shutdown }
#[derive(Clone)] pub struct Inference { /* channel to the inference thread */ }
impl Inference {
    pub fn spawn(factory: fn(Family) -> Box<dyn SpeechEngine>, vad: fn() -> Box<dyn VoiceDetector>) -> Inference;
    pub fn preload(&self, model: ModelSpec, vad: ModelSpec);          // non-blocking, background load + warmup
    pub async fn transcribe(&self, req: TranscribeRequest) -> Result<InferenceOutput, InferenceError>;
    pub fn unload(&self);                                              // S5
    pub fn loaded_model(&self) -> Option<&'static str>;
}
```

Acceptance:
- `crates/speech-sys` from the Phase 0 probe, with speech.cpp v0.10.0 as a git submodule at
  `crates/speech-sys/speech.cpp` (`git submodule add`), added to the workspace. CI-safe CPU flags.
- `adapters/engine.rs` is the only user of `speech_sys` (G2): a safe wrapper (RAII for model/request,
  `Send` justified in a SAFETY comment), `SpeechCppEngine: SpeechEngine` (device "gpu" with fallback to "cpu" if
  the GPU fails to start, warmup on load, Haas `keywords` from the vocabulary, cancel via the token through the
  progress callback and/or `speech_request_cancel`), `SileroVad: VoiceDetector`. Checks
  `speech_api_version_major()` at load. Routes speech.cpp logs to `tracing` without transcript text.
- `inference.rs`: one OS thread. A transcribe job loads the model if needed (60 s timeout), runs VAD, calls
  `core::pipeline::plan`, transcribes each chunk (checking the cancel token between chunks), joins with
  `pipeline::join_texts`, measures `asr_ms`. Hallucination filter (`text::is_hallucination`) -> treated as no
  speech. Region found but empty text -> `Ok(raw: Some(empty))` and the controller maps it to `EmptyResult`.
  Switching model frees the old one first (memory).
- Engine tests (`#[ignore]`, models from `MURMUR_TEST_MODELS`): the Phase 0 WAVs through `Inference`, cancel
  latency, no-speech on silence, a 77 s file through the chunk path. A unit test of the worker with a fake
  engine (not ignored).

## T2-7 platform + output + hotkeys

```rust
// platform/macos.rs (the ONE place per permission, G8)
#[derive(Serialize, specta::Type)] pub enum Permission { Granted, Denied, NotDetermined }
pub fn microphone_permission() -> Permission;
pub async fn request_microphone() -> Permission;          // prompts only when NotDetermined
pub fn accessibility_trusted() -> bool;
pub fn request_accessibility();                            // AXIsProcessTrustedWithOptions(prompt = true)
pub fn secure_input_enabled() -> bool;                     // IsSecureEventInputEnabled
pub enum PrivacyPane { Microphone, Accessibility }
pub fn open_privacy_pane(pane: PrivacyPane);

// adapters/output.rs
pub struct DeliverOptions { pub auto_paste: bool, pub restore_clipboard: bool }
pub fn deliver(text: &str, opts: DeliverOptions) -> DeliveryOutcome;   // blocking, <= 1 s, never panics
pub fn copy_text(text: &str) -> Result<(), OutputError>;

// adapters/hotkeys.rs (tauri-plugin-global-shortcut)
pub enum HotkeyEvent { MainDown, MainUp, AlternateDown, AlternateUp, Esc }
pub struct Hotkeys { /* Send + Sync */ }
impl Hotkeys {
    pub fn new(app: &tauri::AppHandle, on_event: std::sync::Arc<dyn Fn(HotkeyEvent) + Send + Sync>) -> Hotkeys;
    pub fn set_main(&self, accelerator: &str) -> Result<(), HotkeyError>;  // on failure the old key stays registered
    pub fn set_alternate(&self, accelerator: Option<&str>) -> Result<(), HotkeyError>;
    pub fn set_esc(&self, on: bool) -> Result<(), HotkeyError>;            // plain Esc, no modifiers
    pub fn suspend(&self) -> Result<(), HotkeyError>;   // while the KeyRecorder listens
    pub fn resume(&self) -> Result<(), HotkeyError>;
}
```

Acceptance:
- `deliver`: write clipboard (arboard); if `auto_paste` and `accessibility_trusted()` and not
  `secure_input_enabled()`, post Cmd+V (CGEvent via a small dependency or enigo; must work when called from a
  non-main thread, verify). Outcomes map to `DeliveryOutcome` / `CopyReason`. `restore_clipboard`: restore the
  previous text about 400 ms after the paste, on a background thread, only if the clipboard still holds our text.
- Verify `deliver` end to end on this Mac: write a tiny `#[ignore]` test or example that delivers into TextEdit
  (`open -a TextEdit`, new document), then read the document text back with AppleScript; report the result. If
  the process lacks accessibility permission, report the `Copied { NoAccessibility }` outcome instead and
  confirm `pbpaste` shows the text.
- Hotkeys: confirm that plain `Escape` registers with global-hotkey on macOS (PLAN risk table). Key release
  events must arrive (`ShortcutState::Released`) for push-to-talk.
- Unit tests where possible (accelerator parsing/validation).

## T2-9 windows + tray (owner: audio-dev)

```rust
// src-tauri/src/windows.rs
pub fn setup(app: &tauri::AppHandle) -> Result<(), WindowError>;   // once at startup, on the main thread
pub fn set_pill_visible(app: &tauri::AppHandle, visible: bool);     // any thread; hops to the main thread
pub fn set_pill_width(app: &tauri::AppHandle, width: f64);          // any thread; animated, keeps the center
pub fn show_main(app: &tauri::AppHandle);                           // Regular activation policy, show, focus
// tray.rs
#[derive(Clone)] pub struct TrayModel { pub recording: bool, pub language: Language, pub recent: Vec<(String, String)> /* (id, preview <= 40 chars) */, pub ui_ja: bool }
pub enum TrayAction { OpenMain, ToggleRecording, SetLanguage(Language), CopyLast, CopyRecent(String), CheckUpdates, Quit }
pub fn create(app: &tauri::AppHandle, model: &TrayModel, on_action: std::sync::Arc<dyn Fn(TrayAction) + Send + Sync>) -> Result<(), TrayError>;
pub fn update(app: &tauri::AppHandle, model: &TrayModel);           // rebuilds the menu
```

Acceptance:
- Pill = non-activating NSPanel via `tauri-nspanel` 2.1 (PLAN 2.12): `NonactivatingPanel` style, `hidesOnDeactivate = NO`,
  `becomesKeyOnlyIfNeeded = YES`, collection behavior `canJoinAllSpaces | fullScreenAuxiliary | stationary | ignoresCycle`,
  level above floating (status bar level is fine). Never steals focus: showing uses `orderFrontRegardless`, not `makeKey`.
- Native frosted glass: NSVisualEffectView material `hudWindow`, state `active` (so it stays frosted when another app is
  front), corner radius = height / 2 (capsule). Main window: vibrancy `sidebar` (or `underWindowBackground`), state active.
  Use Tauri `windowEffects` if it can do this; otherwise objc2 directly. The web content stays transparent.
- Position: bottom center of the `visibleFrame` of the screen that holds the mouse, 20 px above the bottom, recomputed
  on every show. Height 40 px (+ shadow margin if needed).
- `set_pill_width`: `setFrame:display:animate:` on the main thread, centered on the same x.
- Main window: hide instead of close (red button and Cmd+W), then switch the activation policy back to `Accessory`;
  `show_main` switches to `Regular` so it appears in Cmd+Tab. App starts as `Accessory` (no Dock icon).
- Tray: template icon (monochrome, generated from a simple waveform glyph as PNG in `src-tauri/icons/tray*.png`),
  menu items in the UI language (English and Japanese strings kept in tray.rs, short), Recent submenu, Language radio items.
- Verify by running the app (`pnpm tauri dev`): the pill shows over another app without stealing focus (check with
  `osascript -e 'tell application "System Events" to get name of first process whose frontmost is true'` before and
  after showing it via a temporary debug call), the tray menu appears. Report what you could and could not verify.
