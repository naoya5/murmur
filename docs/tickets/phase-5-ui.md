# Sprint 5 (PLAN Phase 5: the UI) - owner: ui-designer

Rules: [RULES.md](./RULES.md). Read first: `docs/IDEATION.md` 5 to 8 (screens, Esc behavior, dashboard),
`docs/PLAN.md` 2.12 and 2.16, `docs/HANDOFF.md` 6 (what went wrong in the reference app, good ideas) and 8
(macOS facts: CSS `backdrop-filter` cannot blur the desktop; the native vibrancy is behind a transparent page).

Your files: everything under `src/` except `src/ipc/bindings.ts` (generated), plus `index.html`, `pill.html`,
`package.json` dependencies you add, app icons in `src-tauri/icons/` (not `tray*.png`). Ask the orchestrator
(hc send) for any backend change; do not edit `src-tauri/src/**`.

| Ticket | Status |
|---|---|
| T5-1 Design tokens (`src/styles.css`, `//`-free CSS header `SOT: design-tokens`) + UI strings (`src/i18n.ts`, `SOT: ui-strings`, en + ja, follows `settings.ui_language`, System = OS language) | done |
| T5-2 Components in `src/ui/`: Glass surfaces, Button, Toggle, Segmented, Select, TextField, KeyRecorder, ProgressBar, Charts (hand-made SVG: heatmap, bar chart, sparkline), EmptyState, Toast | done |
| T5-3 The pill (`src/pill/Pill.tsx`) | done (browser mock; not yet run against the native pill window) |
| T5-4 Main window shell + views: Home (dashboard), History, Dictionary, Models, Settings (incl. About + licenses), Onboarding | done (browser mock; not yet run against the backend) |
| T5-5 App icon (1024 master -> `pnpm tauri icon`) | done |
| T5-6 Visual QA: screenshots of every view and pill state, light and dark, minimum window size (680x480), reduced motion | done in the browser mock (chrome-devtools-axi, 760x540 and 680x480, light and dark); reduced motion checked by CSS review only |

## Design direction

Apple-made, minimal, quiet. Frosted glass (the native window is already vibrant; the page background stays
transparent; panels are subtle translucent fills with a 1 px light inner border and a soft gradient edge, not
"transparent"), generous radius (10-14 px controls, 18-20 px cards), system fonts (`-apple-system`, SF Pro,
Hiragino for Japanese), few colors, accent = macOS accent (`AccentColor` system color in WebKit; fall back to
#0A84FF), motion 150-250 ms with `prefers-reduced-motion` honored. Light and dark follow the OS. It should feel
like a small widget, not a big desktop app. Icons: `lucide-react` (ISC). Copy reference apps' structure only,
never their assets.

## T5-3 Pill

Window: 40 px tall, width set by you through `commands.setPillWidth(width)` (measure the content, call it on
every state change; the native window animates). The page is transparent; the native capsule is frosted glass
with radius 20. States come from `PillState` (`commands.getPillState()` on load, then `events.pillState`):
- recording: red dot, live waveform from `events.audioLevel` (about 30/s, 0..1; smooth it), elapsed time
  (count locally from `elapsed_ms`), language badge ("あ" for ja, "A" for en). Click anywhere = stop
  (`commands.pillClicked()`).
- cancel_pending: circular countdown (3 s, from `remaining_ms`) + "Esc to undo" / "Esc で元に戻す"; click = undo.
- transcribing / delivering: a short shimmer over the waveform.
- notice: delivered pasted -> check + "Pasted"; copied -> "Copied - press ⌘V" (reason-specific short text);
  no_speech -> "No speech"; failed -> yellow icon + reason + "Saved to history"; capture_failed -> reason
  (permission -> "Allow the microphone in Settings").
- hidden: render nothing.
Fixed-width slots so the text does not jitter while the timer runs (tabular numbers).

## T5-4 Main window

Hidden title bar with traffic lights (titleBarStyle Overlay): leave 28 px top inset and ~78 px left inset for
the traffic lights; drag region on the top bar (`data-tauri-drag-region`). Sidebar: Home, History, Dictionary,
Models, Settings. Listen to `events.navigate` (view name) to switch views.
- Home: `commands.getDashboard()`; refetch on `events.historyChanged` and `events.settingsChanged`. Time saved
  (big), sessions, words / characters, speaking rate (WPM en, chars/min ja), median latency, streak, 20-week
  heatmap, last 14 days bars. Empty state for a new user.
- History: search (debounced 150 ms) + paging (`commands.listHistory(query, limit, offset)`), newest first,
  each row: time, language, text (2 lines, expand), status badge (failed / recovered with Retry button ->
  `commands.retryHistoryItem(id)`), copy (`commands.copyText`), delete. Clear all with confirmation (an in-app
  sheet, never `window.confirm`). Auto-refresh on `events.historyChanged` (never require a manual reload).
- Dictionary: list of `settings.dictionary` entries (written form, readings chips, scope ja/en/both); add, edit,
  delete; saves through `commands.updateSettings`. Explain readings with an example: Claude Code = クロードコード.
- Models: per language, the catalog models (`commands.listModels()`), size, license, installed state, download
  with progress (`events.modelProgress`), cancel, delete, "Use for Japanese/English" (updates
  `settings.models`). Show errors (offline, disk full) in plain words. Attribution for CC-BY models.
- Settings: every field of `Settings` (none may be missing; group them: General, Recording, Output, Text,
  Dictation stats, Privacy & data, About). Hotkey via KeyRecorder: on focus call `commands.suspendHotkeys()`,
  capture a combo (show it as ⌥ Space), save via `updateSettings`, show the error if registration fails, always
  `commands.resumeHotkeys()` on blur/cancel. Microphone select from `commands.listInputDevices()`. Permissions
  status with buttons (`getPermissions`, `requestMicrophone`, `requestAccessibility`, `openPrivacyPane`).
  Reset stats with confirmation. About: version (`commands.appInfo()`), engine, licenses (speech.cpp MIT, each
  model's license and attribution from the catalog, Parakeet CC-BY-4.0 credit), "no data leaves your Mac".
  `updateSettings` returns `ApplyError` -> show it inline next to the field.
- Onboarding (when `settings.onboarding_completed` is false, full window, 4 steps): microphone permission,
  accessibility permission (explain it is for automatic paste; skippable: copy-only works without it), model
  download for the chosen language (+ VAD `silero-vad-v6.2`, both via `downloadModel`; show progress), test:
  a focused text area + "press ⌥ Space and say something"; when text appears in it, success. Finish sets
  `onboarding_completed = true`. Uses the same permission commands as Settings (G8).

## Verification (T5-6)

Use a dev-only IPC mock so every view renders in a normal browser: `src/ipc/mock.ts`, loaded only when
`import.meta.env.DEV` and `window.__TAURI_INTERNALS__` is absent (it must not end up in the production bundle;
check `dist/` after `pnpm build`). Then `pnpm dev` and screenshot each view and pill state in light and dark
with `chrome-devtools-axi` (run `chrome-devtools-axi --help`), at 760x540 and 680x480. Be picky about
alignment, spacing, truncation, Japanese line breaks. Vitest tests for components with logic (KeyRecorder,
history paging, countdown).
