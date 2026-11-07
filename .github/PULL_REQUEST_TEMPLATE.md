## What and why

<!-- What changes for someone using Murmur or working on it, and why. Link the issue: "Closes #123". -->

## How it was verified

<!-- Say what you ran and what you checked in the app. Say what you did NOT verify. -->

- [ ] `pnpm check` passes
- [ ] Checked end to end in the app (describe the steps below)
- [ ] `scripts/e2e.sh` (and the `annotations` group, if that area changed)

## Screenshots

<!-- For UI changes: before and after. Use test text, not real transcripts. -->

## Checklist

- [ ] The guardrails in `AGENTS.md` hold (pure core, one SDK per adapter, timeouts, no panics, no transcript text in logs).
- [ ] New UI text is in `src/i18n.ts` in English and Japanese.
- [ ] Generated files were regenerated, not edited (`pnpm bindings` for `src/ipc/bindings.ts`).
- [ ] Docs are updated (`README.md`, `README.ja.md`, `docs/ARCHITECTURE.md`) if behavior changed.
- [ ] No secrets, personal data, recordings or transcripts are included.
