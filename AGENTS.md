# AGENTS.md

The working guide for coding agents (Codex, Claude Code and others) and for human contributors.
`CLAUDE.md` imports this file, so both tools read the same rules. Start with
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), then the source of truth for the area you change.

## What Murmur is

Murmur is local, private speech-to-text for macOS on Apple silicon. The user presses a hotkey,
speaks, and presses it again; the text is pasted at the cursor. Speech recognition (speech.cpp on
Metal) and the optional language-model steps (llama.cpp in a sidecar process) run on the Mac.
Audio and text never leave it.

Stack: Tauri 2, Rust (edition 2024), React 19 and TypeScript. The design is ports and adapters:
a pure core crate holds the rules, and one adapter per SDK or OS API does the I/O.

## Repository map

| Path | What |
|---|---|
| `crates/murmur-core` | The pure core: session state machine, settings, model catalog, text rules, metrics, refine and annotation rules. |
| `crates/speech-sys` | FFI to [speech.cpp](https://github.com/nyosegawa/speech.cpp) (git submodule, pinned to a commit of the fork [naoya5/speech.cpp](https://github.com/naoya5/speech.cpp); `docs/ARCHITECTURE.md` says why). |
| `crates/murmur-llm` | The LLM sidecar: llama.cpp behind JSON Lines on stdin and stdout. A separate Cargo workspace. |
| `src-tauri/` | The app: adapters, controller, inference thread, IPC, windows, tray, updater. |
| `src/` | The React UI: main window (`index.html`), recording pill (`pill.html`), annotation overlay (`overlay.html`). |
| `scripts/` | `build-sidecar.sh`, `e2e.sh`, `guardrails.sh`, `make-signing-cert.sh`. |
| `docs/` | `ARCHITECTURE.md`. |
| `.agents/skills/` | Agent skills. `.claude/skills` links here, so Codex and Claude Code share them. |

## Commands

```sh
git submodule update --init --recursive   # speech.cpp, once after cloning
pnpm install
pnpm tauri dev                            # run the app (builds the sidecar first)
pnpm check                                # everything CI runs; must pass before a change is done
cargo test -p murmur-core                 # the core alone, fast
pnpm bindings                             # regenerate src/ipc/bindings.ts after an IPC change
```

Tests that need models (not downloaded by any test):

```sh
MURMUR_TEST_MODELS=~/.murmur-dev-models cargo test -p murmur with_models -- --ignored --test-threads=1
MURMUR_TEST_MODELS=~/.murmur-dev-models bash scripts/e2e.sh                # end to end in the debug app
MURMUR_TEST_MODELS=~/.murmur-dev-models bash scripts/e2e.sh annotations    # the annotation group
```

`CONTRIBUTING.md` explains how to fill `~/.murmur-dev-models`.

In dev, macOS attaches the microphone and accessibility permissions to the terminal that runs
`pnpm tauri dev`, not to Murmur. The E2E script runs the debug app on a scratch data directory
(`MURMUR_DATA_DIR`), so the user's own history and settings stay untouched.

## Guardrails

Every change keeps these. `scripts/guardrails.sh` and the workspace clippy lints enforce the
ones marked as checked; review covers the rest.

| Id | Rule | Checked by |
|---|---|---|
| G1 | `crates/murmur-core` is pure: no Tauri, file or network I/O, OS APIs or SDKs. | `guardrails.sh` |
| G2 | An SDK is used from one adapter file only (`speech_sys` -> `src-tauri/src/adapters/engine.rs`). | `guardrails.sh` |
| G3 | A new setting is a new `SettingKey` plus an arm in `settings_apply.rs` (exhaustive match, no `_`). | compiler |
| G4 | IPC types come from Rust. `src/ipc/bindings.ts` is generated; never edit it by hand. | `guardrails.sh` |
| G5 | Every wait has a timeout. | review, state machine tests |
| G6 | No `unwrap`, `expect`, `panic!`, `todo!` or `unimplemented!` in production code. Tests may use them. | clippy |
| G7 | Never log or `Debug`-print transcript text. Use the redacting types. | review |
| G8 | Check a permission right before use, through `src-tauri/src/platform/macos.rs`. | review |
| G9 | Nothing slow runs before the paste. History, stats and cleanup run after delivery. | review, latency log |
| G10 | A source-of-truth file starts with `//! SOT:` plus What / Why / Where. | `guardrails.sh` |
| G11 | Network only in `src-tauri/src/adapters/models.rs` and the updater. No telemetry. | `guardrails.sh` |
| G12 | `pnpm check` passes before a change is done. Fix any failing lint or test you see, even if unrelated. | `pnpm check` |
| G13 | A feature is done only when it works end to end in the app. Say exactly what was and was not verified. | review |

## Sources of truth

```sh
rg "SOT:"    # every source of truth with its What / Why / Where
```

| Area | File |
|---|---|
| Session state machine | `crates/murmur-core/src/session.rs` |
| Settings | `crates/murmur-core/src/settings.rs` |
| Models | `crates/murmur-core/src/catalog.rs` |
| Text rules | `crates/murmur-core/src/text.rs` |
| Cleanup and translation rules | `crates/murmur-core/src/refine.rs` |
| Sidecar protocol | `crates/murmur-llm/src/main.rs` |
| IPC | `src-tauri/src/ipc.rs` |
| Database schema | `src-tauri/src/adapters/store.rs` |
| UI strings (English and Japanese) | `src/i18n.ts` |
| Design tokens | `src/styles.css` |
| App version | `Cargo.toml` (`[workspace.package].version`; `package.json` mirrors it) |

Use the source of truth; do not copy its values into other files.

## Conventions

- English in code, comments, docs and commit messages. `README.ja.md` is the one Japanese page.
- Never use the em dash (U+2014); write a plain `-`. `guardrails.sh` rejects it in every file.
- Never hand-edit generated files: `src/ipc/bindings.ts`, `src-tauri/permissions/autogenerated/`,
  lockfiles.
- Commits follow Conventional Commits (`feat(scope): ...`, `fix`, `perf`, `refactor`, `test`,
  `docs`, `chore`, `ci`). A body may add `intent(scope): ...`, `decision(scope): ...`,
  `constraint(scope): ...` and `learned(scope): ...` lines that record why.
- New user-facing features are opt-in experiments, off by default. While one is off, nothing of
  it runs: no hotkey, no process, no model, no folder. A new feature needs the maintainer's
  approval before work starts.
- Never commit secrets, keys, `.p12` files, personal data, recordings or transcripts.
- Never run a scaffolding generator in the repository root, and never delete `docs/`.

## Local-only files

The maintainer keeps planning notes, work tickets, internal reviews and investigation logs next to
the code. They are listed in the local-only section of `.gitignore`. Read and update them when
they exist, but never commit them, never `git add -f` them, and never copy their content into
tracked files.

## Skills

| Skill | Use |
|---|---|
| [`murmur-release`](.agents/skills/murmur-release/SKILL.md) | Cut, sign and publish a version from the PRs merged since the last tag. Maintainer only. |
