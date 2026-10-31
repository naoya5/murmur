# Ticket rules (apply to every ticket)

Every ticket in `docs/tickets/` links here. A ticket is done only when all of these hold.
The guardrail ids (G1 to G13) refer to PLAN.md 2.15.

| # | Rule | How it is checked |
|---|---|---|
| R1 | Code, comments, README and commit messages are in English. Plan documents in `docs/` stay Japanese. | review |
| R2 | Never use the em dash character (U+2014). Use "-". | `scripts/guardrails.sh` |
| R3 | No `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!` in production code (G6). Tests may use them. | clippy workspace lints |
| R4 | `crates/murmur-core` stays pure: no Tauri, files, network, OS APIs, speech SDK (G1). | `cargo tree -p murmur-core` in guardrails |
| R5 | A third-party SDK is used from exactly one adapter file (G2): `speech_sys` only in `src-tauri/src/adapters/engine.rs`; HTTP only in `src-tauri/src/adapters/models.rs` (G11). | guardrails grep |
| R6 | Every wait has a timeout (G5, PLAN 2.13). | review + state machine tests |
| R7 | Never log or `Debug`-print transcript text (G7). Use the redacting types. | review |
| R8 | Permissions are checked right before use, through the single function per permission in `platform/macos.rs` (G8). | review |
| R9 | Nothing slow runs before the paste. History, stats and file cleanup run after delivery (G9). | review + latency log |
| R10 | A file that is a source of truth starts with `//! SOT:` + What / Why / Where (G10). | `rg "SOT:"` |
| R11 | No telemetry, no network except model download and update check (G11). CSP `default-src 'self'`. | guardrails + config review |
| R12 | `pnpm check` passes (fmt, clippy -D warnings, cargo test, guardrails, bindings diff, tsc, biome, vitest, vite build) (G12). Fix any failing lint or test you see, even if unrelated. | `pnpm check` |
| R13 | Do not call a feature done until it runs end to end in the app (G13). Say exactly what was and was not verified. | ticket report |
| R14 | Never hand-edit generated files: `src/ipc/bindings.ts`, lockfiles, CHANGELOG. | guardrails bindings diff |
| R15 | Never commit secrets, keys or personal data. | review |
| R16 | Never delete or move `docs/`. Never run a scaffolding generator in the repo root. | review |
| R17 | Do not `git commit`; the orchestrator reviews and commits. Do not touch files owned by another open ticket unless the ticket says so. | review |
| R18 | Use the SOT, do not copy it: settings from `core/settings.rs`, models from `core/catalog.rs`, UI strings from `src/i18n.ts`, design tokens from `src/styles.css`, IPC from `src-tauri/src/ipc.rs`. | review |
