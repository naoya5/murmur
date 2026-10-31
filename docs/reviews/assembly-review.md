# Assembly layer review

Scope: `src-tauri/src/{controller,inference,history,settings_apply,app,lib,debug}.rs` and the adapters
they drive. Read-only review against the `session.rs` transition table, PLAN 2.13 (timeouts), and the
guardrails. Reviewer: core-dev. No code was changed.

Severity: High = can lose a user's result, abort the process, or break a core guarantee in normal use.
Medium = wrong behavior or a hang reachable by ordinary actions. Low = narrow or cosmetic.

---

## F1. Model load has no budget of its own and is not cancellable (High)

PLAN 2.13 gives model loading its own 60 s timeout, separate from transcription (15 s + 0.5 x recorded).
The assembly layer does not implement that separation.

- `inference.rs:252` (`transcribe` -> `ensure_model`) and `engine.rs` `load` (around `engine.rs:119-135`)
  load the model synchronously with no timeout and no cancel poll during the load. The cancel token is
  only checked before and after the load (`inference.rs:249`, `inference.rs:254`).
- `controller.rs:493` awaits `inference().transcribe(req)` with no `tokio::time::timeout`. The only bound
  is the machine's `timeout_timer`, which starts at `StopCapture -> Transcribe` and already includes the
  load time.

Failure scenarios:

1. First dictation after launch, after a model switch, or after S5 idle-unload (`lib.rs:252-275`) can land
   while the model is still loading. A short clip's timer (for example 17.5 s on a 5 s recording) fires
   first, the machine records `Failed(Timeout)` and keeps the audio, even though loading is normal. The
   `apply_at_start` / `ensure_model_loaded` preload (`settings_apply.rs:97`, `app.rs:148-154`) hides this
   only when the user waits for the preload to finish.
2. G5 / leak: if the engine stalls and never polls `poll_cancel` (`engine.rs:521`), the controller's
   transcribe task (`controller.rs:451-501`) has no timeout of its own, so it awaits forever, holding the
   `oneshot` and the inference thread. Every later transcription queues behind it for the process lifetime.
3. Exit during a load: `shutdown` (`lib.rs:277-284`) sends `Msg::Shutdown` then calls
   `inference().shutdown(3 s)`. `Job::Shutdown` is serialized behind the in-flight load, which does not
   observe cancel, so a 60 s load outruns the 3 s wait; the process then exits with a model resident and
   Metal aborts (the exact case `inference.rs:75` warns about).

Suggested fix (needs a core change in `murmur-core`, so coordinate): add a `ModelLoading { session }`
event that re-arms `timeout_timer` once with a 60 s load budget, emitted by the inference path when a load
starts. Give `controller::transcribe` a hard `tokio::time::timeout` as a backstop and cancel the token on
expiry. For exit, have `lib.rs:280-282` wait on a controller Shutdown ack (so the per-session cancel tokens
in `controller.rs:618-626` are set) before `Job::Shutdown`, and extend the inference shutdown wait when a
load is in progress.

## F2. `language_override` leaks to a later recording (Medium)

`controller.rs:191-196` sets `self.language_override = Some(language)` in `translate` for
`Msg::HotkeyDownWithLanguage`, but it is only consumed by `.take()` inside `start_capture`
(`controller.rs:337`). If the resulting `HotkeyDown` is ignored by the machine (any non-Idle state, for
example the alternate hotkey pressed while a recording or transcription is already running), no session
starts, the override is never taken, and the next ordinary main-hotkey recording silently records in the
stale alternate language.

Scenario: recording in Japanese, the alternate (English) hotkey fires once by accident -> ignored -> the
next normal recording comes out as English.

Suggested fix: set the override only when the machine actually enters `StartCapture` (clear it on any step
that does not start a session), or carry the language on the event into the machine rather than as
controller side state.

## F3. Main-thread blocking on the single SQLite mutex (Medium)

Tauri 2 runs non-`async` commands on the main (UI) thread. Several main-thread paths take the one
`Mutex<Connection>` (`store.rs:149`, 2 s `busy_timeout`) and can contend with blocking-pool writers:

- `update_settings` (`ipc.rs:127-135`, non-async) -> `settings_apply::update` -> `settings_file::save`
  (disk write) and `refresh_tray` (`settings_apply.rs:78`) -> `tray_model` -> `store().list("",5,0)`
  (`lib.rs:159-167`).
- `on_tray` `CopyRecent` -> `store().get` and `CopyLast` -> `last_saved_text` -> `store().list`
  (`lib.rs:216-241`), both on the tray callback (main thread).

If a `save_success` / `save_failure` write holds the connection, the UI thread stalls up to the busy
timeout. Suggested fix: make `update_settings` an `async` command and move the `store` read inside
`refresh_tray` (and the tray copy actions) to `spawn_blocking`.

## F4. `history::retry` awaits the engine with no timeout and no cancel (Medium, G5)

`history.rs:183-190` awaits `inference().transcribe(request)` with a fresh `CancelToken` that nothing ever
cancels and no surrounding timeout. A stalled or very long engine call makes the `retry_history_item` IPC
call (`ipc.rs:220-228`) hang with no way to abort. Every wait must have a timeout (G5). Suggested fix:
wrap in `tokio::time::timeout(15 s + 0.5 x duration_ms)` and call `cancel.cancel()` on expiry.

## F5. Orphan spool when a session is cancelled before the mic opens (Medium)

In `drop_audio` (`controller.rs:580-598`), when the recorder never opened, `stop_recorder`
(`controller.rs:388-393`) returns early on `recorder = None` without taking `audio_tx`, so `audio_tx`
stays inside `ctx`. `ctx` is dropped at the end of `drop_audio`, the watch channel closes, the spawned
task's `rx.wait_for(Option::is_some)` returns `Err` at once, and `remove_file` runs immediately, before any
file exists. Then `Recorder::start` (`audio.rs:164`) creates the spool, the device opens,
`on_capture_opened` (`controller.rs:216-222`) finds no session and calls `stop()` but does not delete the
file. On the next launch `recover_spool` (`history.rs:249-307`) turns it into a phantom "recovered" row.

Trigger: Esc (then countdown expiry) or a cancel while the microphone permission prompt is up or the device
is slow to open. Suggested fix: in the orphan branch of `on_capture_opened`, delete `spool_path` after
`stop()` (the returned `CapturedAudio` carries the path).

## F6. `SetEscCapture(true)` failure is swallowed (Low)

`controller.rs:320-324` logs and continues when `set_esc` fails. `set_esc(false)` cannot fail
(`hotkeys.rs:159-173`: `unregister` ignores its result and `state.esc = on` always runs), so there is no
"Esc left registered globally" path. The only real divergence is `set_esc(true)` failing: the machine then
believes Esc is captured (`wants_esc()` true) while the OS has not registered it, so the user cannot cancel
a recording with Esc. Rare (registering plain Esc almost always succeeds). Suggested fix: surface this as a
pill notice, or retry once.

## F7. `AutoCheckUpdates` setting has no effect (Low-Medium)

`updater.rs` is not declared in the module tree (`lib.rs:8-19` has no `mod updater;`), the updater plugin
is not registered in the builder (`lib.rs:48-66`), and `updater::schedule` is never called. The `apply`
arm comment at `settings_apply.rs:174-175` ("Read by the update scheduler before each check") describes
code that does not run, and the `auto_check_updates` setting is inert. This may be deferred to Phase 7; if
so, note it, but the setting is already user-visible. Suggested fix: wire `mod updater;`, register the
plugin, and start `updater::schedule(handle, || services.settings().auto_check_updates)` in `start`.

---

## Verified clean

- G7 (no transcript text in logs): every log line checked. `on_inferred`, `deliver`, `save_*` and history
  log ids, counts, outcomes and `%error` only; `RawTranscript` and `TranscriptText` `Debug` impls redact
  the content. No transcript text reaches `tracing`.
- G9 (nothing slow before the paste): `SaveSuccess`/`SaveFailure` run only after `Delivered` or the delivery
  timer; history, totals and spool delete are on the blocking pool after delivery. `format` runs on the
  controller loop but is < 2 ms (perf-tested in core).
- Stale results and timers: `on_inferred` (`controller.rs:249`) and `translate` drop results for unknown
  sessions; `InferenceError::Cancelled` returns `None`; the machine ignores stale `TimerId`s. A late result
  after a timeout is discarded.
- `SetEscCapture(false)` is emitted on every exit from Recording/CancelPending/Transcribing (confirmed
  against the machine's effects), so Esc is released whenever the session ends.
- Nested delivery timeouts are ordered correctly: output adapter 1 s (`output.rs:15`) < controller
  `DELIVER_TIMEOUT` 1.5 s (`controller.rs:38`) < machine `DELIVERY_TIMEOUT_MS` 2.5 s, so the inner layer
  reports before the outer gives up.
- Pause requested before the mic opens is honored: `ctx.paused` is applied in `on_capture_opened`
  (`controller.rs:230-232`); audio heard during a cancel countdown is dropped by `set_paused`, not by
  closing the stream (matches PLAN 2.4).
- Model switch frees the old model before loading the new one (`inference.rs:217-219`), and `unload_all`
  runs on `Job::Shutdown` and `Job::Unload`.
