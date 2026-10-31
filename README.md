# Murmur

Press a hotkey, speak, press it again. Your words are pasted where your cursor is, in well under a second.
Everything runs on your Mac. No account, no subscription, no limits, no audio or text ever leaves the machine.

- Japanese with [Haas-ja-v1.0](https://huggingface.co/third-intelligence/Haas-ja-v1.0), English with
  [NVIDIA Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3), both running on the Apple GPU
  through [speech.cpp](https://github.com/nyosegawa/speech.cpp).
- A small frosted-glass pill shows that you are recording. It never takes focus from the app you type in.
- Every result is kept in a local history. If anything fails after recording, the audio is kept and can be
  transcribed again. Press Esc to cancel (press Esc again within 3 seconds to undo).
- A dashboard shows how much time you saved, your speaking rate, latency and streak.
- A dictionary teaches it names and terms ("Claude Code", "Tauri").

Requirements: macOS 14 or later on Apple Silicon (M1 or newer). About 0.7 GB of disk per language model.

## Install

1. Download `Murmur_<version>_aarch64.dmg` from the Releases page and drag Murmur to Applications.
2. Open Murmur. macOS says it cannot verify the developer (the app is not notarized, to keep it free).
   Open **System Settings > Privacy & Security**, scroll down and click **Open Anyway**. You only do this once;
   in-app updates do not ask again.
3. The welcome window walks you through four steps: microphone permission, accessibility permission (for
   automatic paste), the model download for your language, and a test recording.

## Use

| Action | Default |
|---|---|
| Start / stop recording | `Option + Space` (change it in Settings) |
| Cancel | `Esc` while recording or transcribing; `Esc` again within 3 s to undo |
| Switch language | menu bar icon > Language, or Settings |
| Copy a past result | menu bar icon > Recent, or the History window |

If a text field does not accept the paste (password fields, secure input), Murmur copies the text and tells you
to press `Cmd + V`.

## Privacy

Murmur makes exactly two kinds of network requests: downloading a model you chose from Hugging Face (pinned
revision, verified with SHA-256), and checking GitHub Releases for updates (can be turned off). There is no
telemetry and no crash reporting. Data lives in `~/Library/Application Support/io.github.naoya5.murmur/`.

## Build from source

```sh
git clone --recurse-submodules https://github.com/naoya5/murmur.git
cd murmur
brew install cmake          # speech.cpp builds with CMake
pnpm install
pnpm tauri dev              # development
pnpm check                  # lint, types, tests, guardrails
CI=true pnpm tauri build    # release .app and .dmg (CI=true skips the Finder styling of the DMG)
```

Release builds sign the bundle ad-hoc with the hardened runtime. Set `APPLE_SIGNING_IDENTITY` to a stable
identity (for example the one `scripts/make-signing-cert.sh` creates) so that macOS keeps the microphone and
accessibility permissions across updates. Updater artifacts are signed when `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` are set; the public key is in `src-tauri/tauri.conf.json`.

End-to-end tests drive the real debug app with a WAV file as the microphone and signals as the hotkey:

```sh
MURMUR_TEST_MODELS=~/.murmur-dev-models scripts/e2e.sh            # dictation, Esc, crash recovery, failures
MURMUR_TEST_MODELS=~/.murmur-dev-models cargo test -p murmur with_models -- --ignored --test-threads=1
```

Architecture: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Rules for contributors and agents: [CLAUDE.md](CLAUDE.md).

## Licenses and credits

Murmur is MIT licensed. It bundles or downloads:

| Component | License |
|---|---|
| [speech.cpp](https://github.com/nyosegawa/speech.cpp) and its ggml fork | MIT |
| [Haas-ja-v1.0](https://huggingface.co/third-intelligence/Haas-ja-v1.0) by Third Intelligence | Apache-2.0 |
| [parakeet-tdt-0.6b-v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) by NVIDIA | CC-BY-4.0 |
| [Qwen3-ASR](https://huggingface.co/Qwen) by the Qwen team (optional) | Apache-2.0 |
| [Silero VAD](https://github.com/snakers4/silero-vad) | MIT |

GGUF conversions by [sakasegawa](https://huggingface.co/sakasegawa). Parakeet TDT 0.6B v3 is used under
CC-BY-4.0; attribution: NVIDIA. The full list is in the app under Settings > About.
