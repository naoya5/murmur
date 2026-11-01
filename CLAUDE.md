# Murmur - rules for agents

Local, private speech-to-text for macOS. Tauri 2 + Rust + React 19. Read `docs/ARCHITECTURE.md` first.

## Hard rules (guardrails, PLAN.md 2.15)

| Id | Rule |
|---|---|
| G1 | `crates/murmur-core` is pure: no Tauri, I/O, OS APIs or SDKs. |
| G2 | An SDK is used from one adapter file only (`speech_sys` -> `adapters/engine.rs`). |
| G3 | New setting = new `SettingKey` + an arm in `settings_apply.rs` (exhaustive match, no `_`). |
| G4 | IPC types come from Rust. `src/ipc/bindings.ts` is generated; never edit it. |
| G5 | Every wait has a timeout. |
| G6 | No `unwrap` / `expect` / `panic!` in production code. |
| G7 | Never log transcript text. |
| G8 | Check a permission right before use, via `platform/macos.rs`. |
| G9 | Nothing slow before the paste; save after delivery. |
| G10 | Source-of-truth files start with `//! SOT:` + What / Why / Where. |
| G11 | Network only in `adapters/models.rs` and the updater. No telemetry. |
| G12 | `pnpm check` must pass before a change is done. |
| G13 | A feature is done only when it works end to end in the app. |

Also: English in code and comments; never the em dash (U+2014); never delete `docs/`;
never run scaffolding generators in the repo root.

Private docs: `docs/HANDOFF.md`, `docs/IDEATION.md`, `docs/PLAN.md`, `docs/PROGRESS.md` and
`docs/features/` are local only (gitignored, not in the history). Read and update them, but never
commit them, never `git add -f` them, and never copy their content into tracked files.

## Find the source of truth

```sh
rg "SOT:"            # every source of truth with its What / Why / Where
```

Key ones: session state machine `crates/murmur-core/src/session.rs`, settings `.../settings.rs`,
models `.../catalog.rs`, IPC `src-tauri/src/ipc.rs`, DB schema `src-tauri/src/adapters/store.rs`,
UI strings `src/i18n.ts`, design tokens `src/styles.css`, version root `Cargo.toml`.

## Commands

```sh
pnpm tauri dev       # run the app (permissions attach to the terminal in dev)
pnpm check           # everything CI runs
cargo test -p murmur-core
MURMUR_TEST_MODELS=~/.murmur-dev-models cargo test -p murmur -- --ignored   # engine tests
```

## Work tracking

Tickets: `docs/tickets/` (rules in `RULES.md`). Progress: `docs/PROGRESS.md` (local only).
