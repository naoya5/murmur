# Sprint 6 (PLAN Phase 6: speed for long dictations)

Rules: [RULES.md](./RULES.md). Architecture: [../ARCHITECTURE.md](../ARCHITECTURE.md).

| Ticket | Owner | Status |
|---|---|---|
| T6-1 S1: transcribe finished speech regions while recording | core-dev | done (77 s: 850 -> 278 ms stop to clipboard; e2e 33/33) |

Status of the other Should items: S2 push-to-talk (core + hotkeys + settings: done), S3 extra models (catalog:
done), S4 sounds (done), S5 idle unload (done), S6 copy last / recent from the tray (done), S7 (a) alternate
language hotkey (done; S7 (b) auto language detection is not planned for v1: it needs Qwen3-ASR as a third
resident model).

## T6-1 S1 stream transcription

Measured today (M4, Metal, Haas Q8_0, stop -> clipboard): 6 s -> 90-150 ms, 77 s -> 850 ms, 10 min -> 7.3 s.
PLAN 2.14 asks for 60 s <= 700 ms median after S1. Goal: any length -> the cost of the last region only
(target: 77 s <= 300 ms), with no loss of accuracy for short dictations.

Policy (keeps accuracy where it matters):
- Recordings up to `pipeline::MAX_CHUNK_SAMPLES` (30 s) are transcribed exactly as today (whole buffer, full
  context). Anything pre-transcribed is discarded for them.
- Longer recordings: while recording, Silero streaming VAD (`speech_detection_start/push/end`, regions are final
  once given and identical to `speech_detect`) finds regions; closed regions are packed with the same rules as
  `pipeline::plan` (chunks <= 30 s, cut in silences, 200 ms pad) and each full chunk is transcribed at once in the
  background. At stop, only the audio after the last committed chunk is transcribed; texts are joined with
  `pipeline::join_texts`. Result text must equal the offline chunk path for the same regions (test it).

Design (files you may edit for this ticket):
- `crates/murmur-core/src/ports.rs`: add streaming detection to `VoiceDetector` (e.g. `stream_start`,
  `stream_push(&[f32]) -> Vec<SpeechRegion>` with only newly final regions, `stream_finish`). Pure packing logic
  for incremental chunks belongs in `core/pipeline.rs` with tests (an `IncrementalPlan` that takes final regions
  and yields chunks ready to transcribe, then `finish(total)` for the tail).
- `src-tauri/src/adapters/engine.rs`: implement the streaming methods on `SileroVad` with
  `speech_detection_*` (one active detection; RAII).
- `src-tauri/src/adapters/audio.rs`: an optional PCM tap in `AudioConfig` called from the converter thread with
  exactly the samples appended to the final buffer (never paused audio), so offsets in the stream equal offsets
  in `CapturedAudio::pcm`.
- `src-tauri/src/inference.rs`: stream jobs (start, push, finish with the full PCM, cancel) on the same single
  thread; pushes are cheap, chunk transcriptions run between them; cancel tokens respected.
- `src-tauri/src/controller.rs`: start a stream on `StartCapture` (only when the model and VAD are installed),
  use finish on `Transcribe`, cancel on `DropAudio`/`CancelTranscription`/failure. Pause/resume keep working
  (the tap is silent while paused).
- Tests: core packing tests; inference worker tests with the fake engine (stream == offline for the same audio,
  short recordings ignore partials, cancel); `#[ignore]` real-engine test on ja60.wav comparing stream vs offline
  text and timing; run `scripts/e2e.sh long` and report stop -> clipboard before/after.
- Keep G5 (timeouts), G7 (no text in logs), G9 (nothing slow before the paste). `pnpm check` must pass.
