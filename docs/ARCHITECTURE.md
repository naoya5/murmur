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

## Layers and dependency direction

| Layer | Where | May depend on | Must not |
|---|---|---|---|
| Core | `crates/murmur-core` | std, serde, specta, unicode-segmentation | Tauri, files, network, OS APIs, SDKs |
| Speech FFI | `crates/speech-sys` | speech.cpp (git submodule, pinned tag) | anything else |
| Adapters | `src-tauri/src/adapters`, `src-tauri/src/platform` | core, one SDK each | other adapters' internals |
| Assembly | `src-tauri/src/{lib,controller,inference,ipc,windows,tray,settings_apply}.rs` | core, adapters | business rules |
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
- S1 (stream transcription): for recordings over 30 s, closed speech regions are transcribed while recording;
  at stop only the tail is processed. Recordings up to 30 s keep the whole-buffer path. Any streaming error
  falls back to the offline path. The dictionary is read once at the start of a recording, so an edit applies
  from the next recording.

