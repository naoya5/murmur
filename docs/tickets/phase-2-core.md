# Sprint 2a (PLAN Phase 2, pure core)

Rules: [RULES.md](./RULES.md). Owner: core-dev. Files: `crates/murmur-core/**` only.

| Ticket | Status |
|---|---|
| T2-1 Session state machine: exhaustive transition tests | todo |
| T2-2 `text.rs`: implement formatting rules + tests | todo |
| T2-3 `metrics.rs`: median, dates, dashboard + tests | todo |
| T2-4 `pipeline.rs`, `settings.rs`, `catalog.rs`: tests | todo |

## T2-1 Session state machine tests

`session.rs` is implemented from the transition table in its header. Write table-driven tests with a fake clock
that cover every row of the table, plus:
- Esc twice (resume) keeps the elapsed time and re-arms the max timer with the time left.
- Countdown expiry from Recording and from Transcribing (engine cancelled only if no outcome is held).
- A result held during the countdown is delivered on undo; a held failure is saved on undo.
- Stale timers and stale session ids are ignored in every state.
- Push-to-talk: HotkeyUp stops; key repeat (HotkeyDown while recording) is ignored.
- Invariant over random event sequences (a small deterministic generator, no new dependency):
  `SetEscCapture` effects always leave Esc registered exactly when `wants_esc()` is true; `Deliver` is always
  before `SaveSuccess`; every session that started ends in exactly one of SaveSuccess / SaveFailure / DropAudio.
If a test reveals a defect in `session.rs`, fix the code (keep the table as the spec) and say so in the report.

## T2-2 text.rs

Implement `is_hallucination` and `format` exactly as documented in the file. Tests: English fillers with commas,
Japanese fillers, "the the", sentence capitals, CJK spacing ("今日 は" -> "今日は", "Claude Code で" keeps the
space between Latin and CJK as written by the model), final punctuation on/off for both languages, dictionary
replacement (case-insensitive, word boundaries, longest first), hallucination phrases. Benchmark: a test asserts
`format` of a 2,000-character text takes < 2 ms in release (`#[ignore]` perf test is fine).

## T2-3 metrics.rs

Implement `median`, `day_to_iso` (and the inverse `iso_to_day` if useful), `dashboard`. Tests for counting
(English, Japanese, mixed, numbers, punctuation), typing and saved time, streak (today empty -> counts from
yesterday; gaps), heatmap 140 and last-14 lengths with zero-filled days, per-language speaking rates.

## T2-4 pipeline, settings, catalog tests

- pipeline: no regions, short buffer, long buffer packing, single region over the limit, clamping, `join_texts`
  for Japanese and English.
- settings: defaults validate; `diff` reports each key (one test per key is fine via a table); `migrate` from
  `{}` (version 0), from a file with unknown fields, refuses a future version; JSON round trip.
- catalog: ids unique, `VAD_MODEL`/`DEFAULT_*` exist with the right role and languages, SHA-256 is 64 hex chars,
  URLs pin a 40-hex revision.
