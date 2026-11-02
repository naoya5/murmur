# Murmur architecture

This is the map of the code. The full rationale is in `PLAN.md` (Japanese, kept local and not in the public repository). Rules for changes are in
`/CLAUDE.md` and `docs/tickets/RULES.md`.

## One path through the code

Read in this order. Each step names where to go next.

1. `src-tauri/src/main.rs` -> `lib.rs`: assembly only. Plugins, managed state, windows, tray, the controller task.
2. `adapters/hotkeys.rs`: the global hotkey and Esc. Turns key events into `session::Event`s for the controller.
3. `controller.rs`: owns the session `Machine`. Feeds events in, executes the effects that come out, emits the
   pill state after every step.
4. `crates/murmur-core/src/session.rs`: the state machine (pure). The transition table is the spec.
5. The adapters the effects call:
   - `adapters/audio.rs`: microphone -> 16 kHz mono PCM, level meter, spool file.
   - `inference.rs`: the single inference thread. VAD -> `core/pipeline.rs` plan -> engine per chunk.
   - `adapters/engine.rs`: speech.cpp behind the `SpeechEngine` / `VoiceDetector` ports.
   - `core/text.rs`: formatting rules on the raw text.
   - `adapters/output.rs`: clipboard and Cmd+V, with secure-input and accessibility checks.
   - `adapters/store.rs`: SQLite history and per-day totals, after delivery.
6. `ipc.rs`: every command and event; generates `src/ipc/bindings.ts`.
7. `src/pill/Pill.tsx` and `src/views/*`: the two windows, built from the shared parts in `src/components/*`.
   `src/overlay/` is the drawing layer of the annotations (a third page, `overlay.html`).

## Layers and dependency direction

| Layer | Where | May depend on | Must not |
|---|---|---|---|
| Core | `crates/murmur-core` | std, serde, specta, unicode-segmentation | Tauri, files, network, OS APIs, SDKs |
| Speech FFI | `crates/speech-sys` | speech.cpp (git submodule, pinned tag) | anything else |
| Adapters | `src-tauri/src/adapters`, `src-tauri/src/platform` | core, one SDK each | other adapters' internals |
| Assembly | `src-tauri/src/{lib,controller,inference,ipc,windows,tray,settings_apply,annotation_session,bundles}.rs` | core, adapters | business rules |
| UI | `src/` | the backend through `src/ipc/` only | `invoke` directly, business rules |

## Threads

| Thread | Owns | Rule |
|---|---|---|
| Main (AppKit) | windows, NSPanel, tray | AppKit calls only here (`run_on_main_thread`) |
| Controller (tokio task) | the session `Machine`, per-session context | events arrive on one mpsc channel; timers are tokio sleeps that send `TimerFired` |
| Audio owner (OS thread per recording) | the cpal stream (not `Send`) | the cpal callback only writes to a lock-free ring buffer |
| Audio converter (OS thread per recording) | resampler, spool writer, level meter | 30 fps level events; appends 16 kHz PCM |
| Inference (one OS thread) | engine, loaded model, VAD | jobs arrive on a channel; one request at a time |
| Blocking pool | SQLite, file cleanup, paste | runs after delivery |

## Dictation flow and latency budget

```
hotkey -> controller: Machine::step(HotkeyDown)
  -> StartCapture (mic permission check, open device <= 3 s) ; pill shows (pre-created, just ordered front)
  ... speech ...
hotkey -> StopCapture -> Transcribe
  -> inference thread: VAD regions -> plan -> engine (model is resident) -> raw text
  -> core::text::format (< 2 ms)
  -> Deliver: clipboard + Cmd+V (accessibility + secure input checked first)
  -> Delivered -> SaveSuccess: history row + day totals + spool delete   (after the paste, G9)
```

Each session records `latency_ms` (stop -> clipboard) and `asr_ms` (engine time) in the history.

## Data on disk

`~/Library/Application Support/io.github.naoya5.murmur/`: `murmur.db` (SQLite, WAL), `settings.json`,
`models/`, `spool/` (raw 16 kHz mono s16le, headerless, survives crashes). Logs: `~/Library/Logs/Murmur/`
(no transcript text).

Annotations (experimental) add an `annotations` table in `murmur.db` (counts and times, no words) and the folder
`annotations/<YYYYMMDD-HHMMSS-xxxx>/` with `frames/<n>.png` (one per drawn screen, absent when nothing was
drawn), `summary.md` (the pasted text) and `annotation.json` (version 2: pictures, ink groups, spoken segments,
the target app's bundle id). The folder is private (0700, files 0600). It is created by the first bundle, never at
start-up.
Retention is a number of days (1 to 90, default 7) plus a 1 GiB cap on the whole folder; the cleanup runs on
blocking threads after a paste and when the setting changes.

## Annotations (experimental)

Off by default (`annotations_enabled`). While it is off nothing of it runs: no hotkey, no overlay window, no
Screen Recording check, no folder. One hotkey (default Option+Shift+S) records the voice and the screen together.
Holding Shift and dragging draws on the screen. At the stop, every screen that was drawn on becomes one picture
with the lines on it, and the words are pasted with a mark such as `[画像 1 ①]` in front of the sentence spoken
while drawing. Nothing drawn: the words alone, as a dictation. The bundle goes to the app that was in front when
the hotkey was pressed, not to whatever is in front at the stop.

New parts:

| Where | Role |
|---|---|
| `crates/murmur-core/src/annotations.rs` | Pure rules: strokes into groups (1.5 s apart, same screen), one picture per group (the screen right before the first stroke; a group on an unchanged screen joins the previous picture; at most 6), marks numbered in order, the pasted text (ja/en, with absolute picture paths for a terminal), terminal bundle ids, all tuning constants. |
| `crates/murmur-core/src/{session,pipeline,settings}.rs` | The annotation rows of the state machine (`Composing`, words-only delivery, the longer bundle delivery timer), speech segments grouped by picture (never cut inside a speech region; everything before the first picture belongs to it, so no word is lost), the three `Annotations*` settings. |
| `src-tauri/src/annotation_session.rs` | Per-session work beside the controller: screen samples and strokes in recording time, which pictures to keep (the newest one when Shift goes down, the one before each group), the plan at stop, the compose step, the bundle folder. |
| `src-tauri/src/bundles.rs` | Folder and file modes, recording clock, `summary.md` / `annotation.json`, retention and the size cap. |
| `adapters/screen.rs` | ScreenCaptureKit (the only file that uses it): 2 fps, long edge 1568 px, Murmur's windows excluded; the last four pictures stay in memory for `keep_frame`. |
| `adapters/pointer.rs` | The Shift key (CoreGraphics, 30 Hz) and the cursor position at the start (which display). No clicks, no tracking, no app names. |
| `adapters/frontmost.rs` | The app in front at the hotkey (pid, bundle id, its focused element through accessibility), brought back before the paste; checked again right before every Cmd+V. |
| `adapters/draw.rs`, `adapters/image.rs` | Draws each picture at its captured size with its ink groups and number badges (CoreGraphics, CoreText, ImageIO). |
| `adapters/output.rs` | `deliver_bundle`: each picture (PNG on the pasteboard, Cmd+V, 160 ms), then the text; a terminal gets the text alone. |
| `windows.rs`, `capabilities/overlay.json` | The `overlay` panel: transparent, no focus, all Spaces, click-through until Shift is held, never captured. Its capability allows `submit_ink_stroke` only. |
| `src/overlay/` | The drawing layer (`overlay.html`): strokes, 0.65 s hold, 0.85 s fade, sent as 0..1 points. |

Flow at a glance:

```
annotations hotkey -> remember the app in front -> Screen Recording check
  -> StartCapture (mic) + StartScreen + overlay shown
  ... speech, Shift+drag strokes ; the pictures before each stroke group kept in the bundle folder ...
hotkey -> StopCapture + StopScreen + overlay hidden -> PlanKeyframes (core: pictures, marks)
  -> speech regions grouped by picture -> one transcription per group (inference thread)
  -> Composing (<= 5 s): draw the pictures, write summary.md and annotation.json
  -> Deliver: bring the app back, pictures one by one, then the text (a terminal: the text with paths)
  -> Delivered -> SaveAnnotation: row + cleanup                                  (after the paste, G9)
```

Esc during recording or processing behaves like dictation (3 s undo, nothing is saved on cancel). If the screen
capture fails nothing can be drawn, so the words go out alone. If a picture was drawn on but could not be made,
the words go out alone with a notice. If the remembered app cannot be brought back to the front, nothing is
pasted anywhere: the text goes to the clipboard and the pill says why.

## How to extend

| Task | Touch |
|---|---|
| Add a model | `core/catalog.rs` only |
| Add an engine | a new adapter implementing `core/ports.rs` + catalog entries + one factory arm in `inference.rs` |
| Add a setting | `core/settings.rs` -> the compiler asks for a `SettingKey` and an `apply` arm -> `views/Settings.tsx` |
| Add a text rule | `core/text.rs` + its tests |
| Add an IPC command | `ipc.rs`; bindings regenerate |
| Add a screen | `src/views/*.tsx` + the sidebar entry |

## Deviations from PLAN.md (recorded decisions)

- `daily_stats` is keyed by (day, language) and does not store saved time; saved time is derived when read so
  that a typing-speed change applies to the whole history.
- Debug builds accept a device name `wav:<path>` that plays a WAV file as the microphone, and SIGUSR1 / SIGUSR2
  as hotkey / Esc, for end-to-end tests without real keys or a real voice.
- Debug builds also read the annotation inputs in `debug.rs`: `MURMUR_DEBUG_HOTKEY=annotations` makes SIGUSR1
  the annotations hotkey; `MURMUR_DEBUG_SCREEN=png-dir:<dir>` plays PNGs as the screen; `MURMUR_DEBUG_POINTER=<json>`
  plays Shift presses and strokes from a script instead of the real keys;
  `MURMUR_DEBUG_COMPOSE_DELAY_MS=<n>` delays the compose step to hit its timeout; SIGINFO flips the
  `annotations_enabled` setting. Release builds ignore all of them.
- S1 (stream transcription): for recordings over 30 s, closed speech regions are transcribed while recording;
  at stop only the tail is processed. Recordings up to 30 s keep the whole-buffer path. Any streaming error
  falls back to the offline path. The dictionary is read once at the start of a recording, so an edit applies
  from the next recording.

