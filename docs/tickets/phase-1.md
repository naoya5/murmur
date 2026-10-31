# Sprint 1 (PLAN Phase 1: foundation)

Rules: [RULES.md](./RULES.md). Status values: todo / doing / review / done.

| Ticket | Owner | Status |
|---|---|---|
| T1-1 Cargo workspace + murmur-core skeleton (SOT files, ports, session machine) | orchestrator | done |
| T1-2 Tooling, guardrails script, `pnpm check`, CI | platform-dev | todo |
| T1-3 tauri-specta IPC, two windows, CSP, capabilities | platform-dev | todo |
| T1-4 CLAUDE.md, docs/ARCHITECTURE.md, README.md, LICENSE | orchestrator | todo |
| T1-5 `crates/speech-sys` as a submodule-backed `-sys` crate | speech-sys-dev | doing (Phase 0 probe) |

## T1-2 Tooling, guardrails, `pnpm check`, CI

Acceptance:
- Biome (lint + format) for `src/`, config in `biome.json`; `tsc --noEmit` strict; Vitest with jsdom.
- `scripts/guardrails.sh` checks: G1 (`cargo tree -p murmur-core -e normal` has no `tauri`), G2/G11 (grep:
  `speech_sys` only in `src-tauri/src/adapters/engine.rs`; `reqwest`/`ureq`/`hyper` only in
  `src-tauri/src/adapters/models.rs`), R2 (no U+2014 in tracked text files), R10 (list of required SOT ids
  present), R14 (regenerated `src/ipc/bindings.ts` has no diff).
- `pnpm check` runs, in order and failing fast: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `scripts/guardrails.sh`, `pnpm typecheck`, `pnpm lint`, `pnpm test`, `pnpm build`.
- `.github/workflows/ci.yml`: macOS arm64 runner, checkout with `submodules: recursive`, rustup stable with
  rustfmt + clippy, pnpm 10, Node 24, cargo and pnpm caches, then `pnpm check`. Engine tests run on CPU in CI.
- `rust-toolchain.toml` for CI only (local Homebrew Rust ignores it; that is expected).

## T1-3 tauri-specta IPC, two windows, CSP, capabilities

Acceptance:
- `tauri-specta =2.0.0-rc.21` + `specta =2.0.0-rc.22` + `specta-typescript =0.0.9` (Handy's proven set). If it
  does not compile with Tauri 2.12, find the nearest working exact set and pin it.
- `src-tauri/src/ipc.rs` (SOT: ipc) holds the specta builder: all commands and events in one place; exports
  `src/ipc/bindings.ts` in debug builds at startup and from a `cargo test` (`export_bindings`), so CI can diff.
- First command `app_info() -> AppInfo { version, data_dir }` to prove the round trip.
- `src/ipc/index.ts` re-exports typed commands and a `useEvent` hook. Views never call `invoke` directly.
- Two windows: `main` (index.html, 760x540, min 680x480, hidden title bar with traffic lights, hidden at start)
  and `pill` (pill.html, transparent, no decorations, always on top, skip taskbar, hidden at start).
  Vite multi-page build. `macOSPrivateApi: true`.
- CSP `default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'` (no remote origins).
- Capabilities per window: pill gets only what it needs (core events + `stop`-type commands later); main gets
  the app commands. No `opener` for the pill.
- `pnpm tauri dev` launches; the main window shows the app version from `app_info`.
