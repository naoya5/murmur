# murmur-llm

Local JSON Lines sidecar. This is a standalone Cargo workspace, intentionally excluded from the
app workspace so llama.cpp's ggml cannot be linked into the speech.cpp process. The wire contract
is documented in `src/main.rs` under `SOT: llm-sidecar-protocol`.

## Build and stage

From the repository root:

```sh
bash scripts/build-sidecar.sh
```

This requires an Xcode 26 or newer macOS SDK and the Metal Toolchain. If the toolchain is missing:

```sh
xcodebuild -downloadComponent MetalToolchain
```

The root `.cargo/config.toml` sets `GGML_METAL_EMBED_LIBRARY=OFF` and the macOS 14 deployment
target for every Cargo invocation from the repository. ggml compiles Metal shaders at build time,
not in each running sidecar. The script builds release for the Rust host and stages these files:

- `src-tauri/binaries/murmur-llm-<host-target-triple>`
- `src-tauri/binaries/default.metallib` (minimum macOS 14)
- `src-tauri/binaries/ggml-tensor.metallib` (minimum macOS 26, used only on suitable devices)

Tauri's `externalBin` entry is `binaries/murmur-llm`, without the suffix. The resources map must
place each metallib at its Resources-root filename: dev copies them next to the executable in
`target/debug/`, while the app bundle uses `Contents/Resources/`. Never put data files in
`Contents/MacOS`. Both files are required even when the current GPU does not use tensor kernels.
Missing/unreadable libraries cause `metal_library_missing`; a failed GPU backend initialization
(including a corrupt default library) causes `metal_library_load_failed`. There is no CPU or
source-compile fallback. ggml can still disable tensor kernels if a present tensor library cannot
load on a tensor-capable GPU; see the performance report for this known limit.

Bundling/signing is owned by the app build. The sidecar does not need microphone or accessibility
permissions. Only changed staged files are replaced, so an unchanged build does not trigger the
dev watcher. This script builds only the host architecture; universal distribution needs separate
architecture builds and packaging.

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
  --model "$HOME/.murmur-dev-models/llm/Qwen3.5-2B-Q4_K_M.gguf"
```

The default translation model is
`~/.murmur-dev-models/llm/LFM2-350M-ENJP-MT-Q4_K_M.gguf`; `--model` overrides it.
Model-free Rust process tests also cover missing default/tensor metallibs and a corrupt default
library. The opt-in checks cover malformed JSON, pre-load requests, failed load, invalid budgets,
cached/cold output equivalence, recurrent-state isolation, exact fixed-message matching, Qwen
non-thinking mode, busy rejection, cancellation, stdin EOF, silent stderr and absence of sockets
(`lsof` on macOS).
Prompts, output and stderr stay in memory. `--output` persists only numeric check/timing metadata;
put local results under `target/`, not in tracked files. Production task validation and idle-stop
policy belong to the app/core, not this transport executable.

## Performance investigation

See [PERFORMANCE.md](PERFORMANCE.md) for the 100-sample baseline, rejected tuning candidates,
CLI comparison, declined native-runtime experiment and the later precompiled-Metal change.
`benchmark.py` repeats the fixed-prefix 10-item x 10-run method against an explicit read-only
request-set JSON. `metallib_bench.py` measures four simultaneous residents, three solo loads per
model, and first prefill using synthetic text and the current core cleanup prompt. Both persist
numeric measurements only. The historical Python cleanup-prefix reader in `metallib_bench.py`
predates the lists-on/off example variants; current cleanup acceptance measurements use the ignored
Rust tests in `src-tauri/src/adapters/refiner.rs`, which call the production prompt directly.
