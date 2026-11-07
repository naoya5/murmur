# Cleanup quality verification, 2026-11-07

The implementation keeps Qwen3.5-2B Q4_K_M and LFM2-350M-ENJP-MT unchanged.
No weights, model downloads, runtime dependencies, settings or IPC types changed.

## Behavior

- Short corrections containing `やっぱり` / `やはり` and numeric enumerations such as `1つ目` reach cleanup, including translation pre-clean.
- Four additional cached examples demonstrate short self-corrections, preserved conditions and quantities, and numeric enumeration. They use different values from the regression cases.
- The minimum output length discounts a retracted clause or enumeration labels instead of rejecting a valid short result. The original upper expansion limit and answer guards remain.
- A new reverse sentence-coverage guard rejects a substantially omitted sentence in uncorrected prose. Corrections and explicitly restructured lists are excluded; this is not a general semantic completeness guarantee.
- Newly generated lists require the lists setting and explicit enumeration. Unrequested list conversion falls back to the original text, preserving the request and its details.

## Real-model comparison

`crates/murmur-core/testdata/refine_quality.json` contains 56 synthetic cases: the existing 40 calibration inputs plus 16 correction, enumeration, detail-preservation and negative-control cases. These are development regression examples used during candidate selection, not an independent held-out benchmark.

The numbers below compare the actual delivered text, including the product gate and rejection fallback, against the expected text. Exact match is stricter than semantic correctness; punctuation and equivalent wording count as mismatches.

| Lists | Before | After |
| --- | --- | --- |
| On | 37 / 56 | 40 / 56 |
| Off | 41 / 56 | 44 / 56 |

The initial evaluator input did not label calibration cases 9, 10 and 12 as lists. The lists-off totals above were recomputed from the recorded deliveries with their correct lists-off expectation (original prose). The committed fixture has the corrected labels. No generated outputs were changed.

Observed examples:

- `月曜にしよう、いややっぱり木曜日にしよう。`: before retained both choices; after delivered `木曜にしよう。`.
- `月曜にしよう。やっぱり木曜日にしよう。`: before skipped cleanup; after delivered `木曜日にしよう。`.
- `資料は月曜、いや火曜、やっぱり木曜に送ります。`: before selected Tuesday; after selected Thursday.
- `1つ目は資料、2つ目は見積書です。`: before skipped cleanup; after delivered two bullets with both items.
- A budget/deadline/assignee/request paragraph triggered an unrequested list that changed the request. The new guard rejected it and delivered the original paragraph.

Limits observed: some complex corrections still survive unchanged. Filler removal and punctuation vary; the final prompt retained a filler in calibration case 1 and inserted a comma in case 30. Lists-on case 9 added a redundant introductory phrase. These are not claims of universal accuracy improvement.

The existing 150-character limit remains. Spoken instructions such as asking someone to make a list remain transcript content; this change does not add a voice-command mode. The sentence coverage guard cannot detect every lost detail or incorrect number.

Warm generation p95 was 1.39 s before and 1.59 s after with lists on, and 1.56 s before and 1.51 s after with lists off. Builds ran during some measurements, so these are diagnostic samples, not a controlled latency benchmark. Prefix warm-up increased from 0.78 s to 1.44 s with lists on; it runs at recording start. No timeout budgets were increased.

## Verification

- `pnpm check`: passed, including formatting, Clippy, workspace tests, guardrails, TypeScript, lint, frontend tests/build, and sidecar checks.
- Pure regression tests verify valid short corrections/lists, omitted-condition rejection, unrequested-list rejection, original length limits, and failed-result propagation.
- `cargo test -p murmur with_the_real_sidecar -- --ignored --nocapture --test-threads=1`: passed. Cleanup exercises the real sidecar through the production state machine and delivery decision with lists both on and off.
- `cargo test -p murmur translates_with_the_real_sidecar -- --ignored --nocapture`: passed after adding an assertion that the final translation contains Thursday and not Monday.
- Native debug app, isolated `MURMUR_DATA_DIR`, synthetic Kyoko audio, real Haas ASR, real cleanup model, debug hotkey signals, auto-paste disabled: passed. The saved row had cleanup status `ok`; only the corrected weekday remained; clipboard matched the saved row. Stop-to-delivery was 281 ms for this one warm, streamed sample. This does not measure live microphone accuracy or general latency.
- The test app was stopped and the prior text clipboard restored. A release bundle was also built and verified with the existing signing identity; startup and speech-model loading succeeded.

Reproduce the quality run after staging the sidecar and installing the existing default model:

```sh
MURMUR_REFINE_EVALSET="$PWD/crates/murmur-core/testdata/refine_quality.json" \
MURMUR_REFINE_EVAL_LISTS=both MURMUR_REFINE_EVAL_REPEATS=1 \
cargo test -p murmur evaluates_light_cleanup -- --ignored --nocapture
```

The evaluator writes synthetic per-case results under `~/.murmur-e2e/eval-stage2/models/` with private permissions. Baseline filenames contain `baseline`; final filenames contain `refine_quality`. Native test artifacts remain under `target/refine-quality/`.
