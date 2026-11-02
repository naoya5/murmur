# murmur-llm

Local JSON Lines sidecar. This is a standalone Cargo workspace, intentionally excluded from the
app workspace so llama.cpp's ggml cannot be linked into the speech.cpp process. The wire contract
is documented in `src/main.rs` under `SOT: llm-sidecar-protocol`.

## Build and stage

From the repository root:

```sh
bash scripts/build-sidecar.sh
```

This builds release for the Rust host target and stages
`src-tauri/binaries/murmur-llm-<host-target-triple>`. Tauri's `externalBin` entry must be
`binaries/murmur-llm`, without the suffix. Bundling/signing is owned by the app build;
the sidecar does not need microphone or accessibility permissions. This script builds only the
host architecture; universal distribution requires separate architecture builds and packaging.

## Checks

```sh
cargo fmt --manifest-path crates/murmur-llm/Cargo.toml --check
cargo clippy --manifest-path crates/murmur-llm/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path crates/murmur-llm/Cargo.toml
```

These run without models. The real-model script is deliberately opt-in and never downloads:

```sh
export MURMUR_SIDECAR_BINARY="$PWD/src-tauri/binaries/murmur-llm-$(rustc --print host-tuple)"
python3 crates/murmur-llm/check.py checks
python3 crates/murmur-llm/check.py messages
python3 crates/murmur-llm/check.py qwen \
  --model "$HOME/.murmur-dev-models/llm/Qwen3.5-0.8B-Q4_K_M.gguf"
```

The default translation model is
`~/.murmur-dev-models/llm/LFM2-350M-ENJP-MT-Q4_K_M.gguf`; `--model` overrides it.
Checks cover malformed JSON, pre-load requests, failed load, invalid budgets, cached/cold output
equivalence, recurrent-state isolation, exact fixed-message matching, Qwen non-thinking mode,
busy rejection, cancellation, stdin EOF, silent stderr and absence of sockets (`lsof` on macOS).
Prompts, output and stderr stay in memory. `--output` persists only numeric check/timing metadata;
put local results under `target/`, not in tracked files. Production task validation and idle-stop
policy belong to the app/core, not this transport executable.
