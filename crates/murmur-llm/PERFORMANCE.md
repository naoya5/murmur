# Generation performance investigation

## Outcome

M2 completed with **no runtime change**. No tested configuration demonstrated a reliable speedup
worth adopting. The approximately 11 ms/token Qwen target was not reached. The original M1 source,
crates.io dependencies and staged executable are retained. This is an investigation result, not a
claim that the generation-speed gap is resolved.

A newer native-runtime experiment was reviewed but **not executed**. Dependency integration was
blocked, and the user then explicitly chose to skip the experiment. Stay on official crates.io
0.1.160 and revisit the comparison after an approved upstream release. No fork build script was
run, and no fork dependency was fetched by Cargo, built or staged.

## Measurement method

Machine: Apple M4, 10 CPU cores, 16 GB, macOS 26.5.2, arm64. Release executable, Metal, all GPU
layers requested, context 2048, CPU threads 4, batch 256, microbatch 64, automatic flash attention,
default F16 KV cache. The benchmark uses the existing read-only request set supplied for this
investigation, with its exact fixed messages, item order, temperature zero and token budgets.

For each model: one persistent process, one load, one fixed-prefix warm, ten passes over ten
items (100 actual warm requests), plus one separate fresh-process load/warm/first-generation case.
The caller times each JSON Lines request/response. `gen_ms` is the sidecar's generation interval;
ms/token is calculated for each sample as gen_ms / generated_tokens, then summarized. It excludes
the EOS token and includes output conversion, matching the sidecar's existing timing definition.
p95 is nearest rank, sample 95 of 100. Per-process peak RSS comes from macOS wait4, in bytes;
the tables convert to MiB. Cold means a new process, not purged filesystem caches.

The orchestrator paused other agents' builds for the full baseline and comparison windows. Normal
interactive desktop processes remained running. Measurements were sequential, not interleaved;
the variability and ordering limit attribution of individual causes. Short tuning probes were
exploratory only and are not substitutes for the 100-sample measurements.

Prompts and generated text were not written to result files. Only numeric metrics, model/build
identifiers and an input-set checksum are retained. Output token counts were stable, but that
alone does not establish output-text equivalence or quality. This is not an app end-to-end test.

## Full results

All durations are milliseconds. Each timing cell is median / p95.

| Build | Model | Generate wall | Generation interval | ms/token | Peak RSS MiB |
| --- | --- | ---: | ---: | ---: | ---: |
| M1 baseline | Qwen3.5-0.8B | 604.81 / 964.97 | 576.39 / 927.22 | 20.30 / 29.12 | 700.83 |
| Rejected candidate | Qwen3.5-0.8B | 885.28 / 1306.06 | 792.57 / 1175.67 | 30.37 / 44.54 | 697.55 |
| M1 baseline recheck | Qwen3.5-0.8B | 628.15 / 937.15 | 595.85 / 910.15 | 21.10 / 32.79 | 740.94 |
| M1 baseline | LFM2-350M-ENJP-MT | 260.91 / 568.32 | 256.12 / 560.76 | 7.73 / 12.26 | 355.25 |
| Rejected candidate | LFM2-350M-ENJP-MT | 393.73 / 865.40 | 384.80 / 857.35 | 11.39 / 19.73 | 349.48 |
| M1 baseline recheck | LFM2-350M-ENJP-MT | 238.47 / 575.58 | 234.94 / 572.95 | 7.04 / 10.62 | 352.06 |

The rejected candidate combined a context-owned backend greedy sampler, a reusable native decode
batch, a CPU sampler chain as fallback, output-vector reservation and removal of a redundant
accept call. Other runtime parameters remained unchanged. It was slower than both baseline
measurements and was fully reverted, not shipped. These combined measurements do not isolate
which change caused the regression.

All six warm/cold pairs exited successfully with zero stderr bytes. Each Qwen run reached EOS in
100/100 requests. Each ENJP run reached EOS in 90/100: item 40 hit its unchanged 83-token budget
in all ten repetitions. Capped runs remain in the statistics and must not be counted as complete
translations. The existing app-level incomplete-output rejection remains necessary.

Local numeric evidence is preserved under the ignored directory:

- `target/m2-before.json`: original M1 baseline.
- `target/m2-after.json`: rejected candidate, not the retained runtime.
- `target/m2-before-recheck.json`: restored M1 comparison.
- `target/m2-cli-probe.json`: stock CLI microbenchmark.

These files are local measurement artifacts, not required build inputs.

## Other probes and source inspection

- Backend greedy sampling alone looked modestly faster in a short probe (Qwen about 18.43
  ms/token), but the complete candidate did not preserve that apparent gain. It is not adopted.
- One generation CPU thread versus four showed no convincing improvement (about 18.72 ms/token
  in a short GPU-sampler probe). Four threads are retained.
- Disabling flash attention did not consistently help the sidecar (about 19.83 ms/token in that
  short probe). Automatic selection is retained.
- A combined 512/512 batch/microbatch plus allocation-reuse probe was worse (Qwen about 30.20
  ms/token). Batch 256 / microbatch 64 is retained. This does not prove batch size alone caused it.
- CPU-chain/allocation reuse without backend sampling also failed to improve a short retest
  (Qwen about 30.76 ms/token). All such edits were reverted to avoid shipping unproven tuning.
- GPU-layer offload was already requested for all layers. F16 K/V and automatic flash attention
  were already native defaults. Quantized caches were not tested: they would change numerical
  behavior, and there was no evidence that this small-context workload was KV-bandwidth-bound.
- Source inspection found that a bare greedy sampler creates a local full-vocabulary candidate
  vector per token; a chain can reuse that vector. `sample()` already calls accept, so the
  existing second accept is redundant for stateless greedy. These are concrete overhead sources,
  but their contribution to the end-to-end gap was not established by the measured candidates.
- The Rust loop already buffers output and detokenizes once after generation. It performs no
  per-token JSON, stdout writes or transcript logging. IPC is outside the measured `gen_ms`,
  so it cannot explain the generation-interval gap by itself.

## Stock CLI comparison

Installed Homebrew llama.cpp is 0.7.0, build b11669, commit
`620d2225cc436ab0b001e3200e590e3b286ef9ab`. A current numeric-only microbenchmark used Qwen,
32 generated tokens after depth 495, batch 256 / microbatch 64, F16 KV, all default GPU placement,
three repetitions per setting:

| Threads | Flash attention | Mean tokens/s | Approximate ms/token |
| ---: | --- | ---: | ---: |
| 1 | on | 61.92 | 16.15 |
| 4 | on | 55.39 | 18.05 |
| 1 | off | 78.60 | 12.72 |
| 4 | off | 71.29 | 14.03 |

Command (from the repository root):

```sh
llama-bench --offline \
  -m "$HOME/.murmur-dev-models/llm/Qwen3.5-0.8B-Q4_K_M.gguf" \
  -p 0 -n 32 -d 495 -r 3 -b 256 -ub 64 -t 1,4 -fa on,off -o json
```

These are synthetic decoder throughput measurements, not the item-specific JSON Lines workload.
They confirm that the current CLI can be faster, but do not reproduce a universal 11 ms/token
number. Different output tokens, context handling, runtime versions and build flags prevent
attributing the entire gap to the Rust loop or a particular option.

## New-runtime experiment review, declined before execution

The published `llama-cpp-2` / `llama-cpp-sys-2` release is already 0.1.160. Its tag resolves to
binding commit `0dd5814c9b061930965d144c1653cd4d05054873`, bundling official llama.cpp 0.6.0
`d81235049384534c167caea52b85a694f6103d14`. Official binding main, checked at
`e7af607ebd5ec2932709ef918c17cca2f9b0fce4`, still points to the same native commit. The installed
CLI is 240 upstream native commits ahead. This is a plausible investigation lead, not a measured
causal explanation.

Open upstream PR 1175 offers a measurement candidate:

- Repository: `https://github.com/willsheldon/llama-cpp-rs`.
- Binding revision: `424d1ffe6c1c4212d5da8e6075f473e33e35fb7f`, package versions 0.1.161.
- Its sole submodule still uses `https://github.com/ggml-org/llama.cpp` and points exactly to
  `620d2225cc436ab0b001e3200e590e3b286ef9ab`, matching the installed CLI.

Compared with the exact 0.1.160 tag, the fork changes only seven paths:

1. `Cargo.lock`: workspace version updates and cc 1.6.0 to 1.8.0.
2. `examples/embeddings/Cargo.toml`: version.
3. `examples/simple/Cargo.toml`: version.
4. `llama-cpp-2/Cargo.toml`: package/sys dependency versions.
5. `llama-cpp-2/src/timing.rs`: two new native timing counters and constructor arguments.
6. `llama-cpp-sys-2/Cargo.toml`: version.
7. `llama-cpp-sys-2/llama.cpp`: official submodule revision.

No fork build.rs, CMake or .gitmodules changes were present. The complete unchanged binding
build.rs was reviewed: local bindgen/compiler/CMake/filesystem operations, no network or download
commands. Server, tools and app targets are disabled; the binding sets LLAMA_CURL=OFF.

Review correction: `cargo tree -e features -i llama-cpp-sys-2` shows that the binding's own sys
dependency enables the sys crate's default `common` feature transitively, even though the
sidecar requests default-features=false. Therefore native LLAMA_BUILD_COMMON is ON, not OFF as
initially reported. This is also present in the published 0.1.160 baseline, not introduced by the
fork. Native common builds bundled sources including download helpers and cpp-httplib; building
those sources does not perform a download, and the sidecar does not call the networking helpers.
The native common CMake file has an optional LLGuidance git clone/build branch, guarded by
LLAMA_LLGUIDANCE, whose default is OFF. KleidiAI and Windows OpenMP fetch paths are also optional
and off. The proposed experiment would explicitly keep all three off, in addition to setting
FetchContent fully disconnected. It would be incorrect to claim that the complete native source
contains no download-capable build code; the reviewed enabled macOS build path does not invoke
those optional paths. Cargo's initial repository/dependency acquisition is separate from
executing build scripts.

The user chose to skip this measurement after the safety block, and it was not attempted.

An unmerged personal fork is not an acceptable production dependency here. Even a future vetted
pin would require provenance/reachability review of both binding and native revisions, native
API/ABI and compiler checks, protocol/cache/cancellation/privacy regressions, model-output review,
repeatable benchmarks, and a planned migration to an official release. A git pin freezes source
identity but does not remove repository-availability or native compatibility maintenance risks.
Prefer repeating this comparison after the runtime update is available from an approved source.

Review sources:

- https://crates.io/api/v1/crates/llama-cpp-2
- https://github.com/utilityai/llama-cpp-rs/pull/1175
- https://github.com/utilityai/llama-cpp-rs/compare/0dd5814c9b061930965d144c1653cd4d05054873...willsheldon:424d1ffe6c1c4212d5da8e6075f473e33e35fb7f
- https://github.com/ggml-org/llama.cpp/compare/d81235049384534c167caea52b85a694f6103d14...620d2225cc436ab0b001e3200e590e3b286ef9ab

## Reproduction and retained state

`benchmark.py` runs the fixed 10x10 method without embedding or persisting prompt text:

```sh
export MURMUR_SIDECAR_BINARY="$PWD/src-tauri/binaries/murmur-llm-$(rustc --print host-tuple)"
PYTHONDONTWRITEBYTECODE=1 python3 crates/murmur-llm/benchmark.py \
  --input /absolute/path/to/final_run.json \
  --output crates/murmur-llm/target/repeated-baseline.json \
  --label m1-baseline \
  --model Qwen3.5-0.8B-Q4_K_M.gguf \
  --model LFM2-350M-ENJP-MT-Q4_K_M.gguf
```

Final `pnpm check:sidecar` is green: fmt, warnings-denied clippy and all 13 Rust tests (10 unit,
three model-free process tests). The command ran through a login shell because hc's non-login
shell did not have pnpm on PATH. Python benchmark syntax validation also passed. The opt-in
LFM/Qwen/privacy checks passed in M1; the complete benchmark processes in M2 also had empty stderr.

No production source or dependency change remains from M2. The staged host executable was restored
from the M1 binary and its SHA-256 matches the M1 backup. Only this report, its README link and
the numeric-only benchmark runner are additions. Build profiles in this crate's own target were
cleaned after tuning, leaving about 176 MiB before rebuilding the final checks; the final target
footprint is about 2.0 GiB, below the 3 GiB budget. No app build, model download, commit, push or
change outside the assigned ownership was performed.

## M4: precompiled Metal libraries (2026-11-04)

### Change and build provenance

The native source remains the official crates.io 0.1.160 release; no fork or new dependency is
used. The root `.cargo/config.toml` supplies `GGML_METAL_EMBED_LIBRARY=OFF` and
`MACOSX_DEPLOYMENT_TARGET=14.0`. A forced rebuild of the native dependency in this crate's own
existing target directory confirmed the root configuration reaches `build.rs` even when Cargo
is invoked from the repository root with `--manifest-path`. The native CMake cache records:

```text
CMAKE_BUILD_TYPE:STRING=Release
CMAKE_OSX_DEPLOYMENT_TARGET:STRING=14.0
GGML_METAL_EMBED_LIBRARY:BOOL=OFF
GGML_METAL_MACOSX_VERSION_MIN:STRING=
```

Do not set GGML_METAL_MACOSX_VERSION_MIN to 14.0 with this pinned CMake version: that adds
`-mmacosx-version-min=14.0` to the tensor compile command, which also adds
`-mtargetos=macos26.0`, and Metal rejects that combination. The inherited deployment-target
environment alone correctly gives default shaders macOS 14 and lets the explicit tensor target
select macOS 26. Actual output verification with `xcrun -sdk macosx metal-vtool --show`:

| Linked library | Bytes | PlatformMajor / Minor | Minimum OS |
| --- | ---: | --- | --- |
| default.metallib | 15,094,049 | 14 / 0 | macOS 14.0 |
| ggml-tensor.metallib | 2,251,442 | 26 / 0 | macOS 26.0 |

All 37 input AIR modules were also checked: 35 ordinary modules report OS Version 14.0, and the
two tensor modules report 26.0. The compiler was Apple metal 32023.883 with macOS SDK 26.5.
This verifies compiled minimum-OS metadata, not execution on an actual macOS 14/15 machine.

The tensor loader is guarded by ggml's has_tensor device property. That property first requires
Metal family 4 support and is disabled by default on pre-M5/pre-A19 devices. Older macOS releases
do not advertise that family. A present tensor file therefore is not loaded on those systems;
its macOS 26 minimum does not force ordinary shaders to require 26. No Metal kernel-source path
or CPU fallback is used when a required library is missing or default cannot initialize.

### Before/after method

`metallib_bench.py` is tracked alongside the 10x10 benchmark runner. It uses existing E2E GGUF
files, synthetic Japanese text, and the current core cleanup system/six-shot prefix with an
empty glossary. No prompt, output or captured stderr is persisted. Both binary versions use the
same script, models, context 2048 and generation cap. OS/driver caches are not deleted.

1. Start four processes successively, alternating ENJP/Qwen, and keep all earlier processes
   alive until the fourth load completes. The embedded baseline uses a fresh executable copy;
   the precompiled case uses the newly built executable with adjacent metallibs.
2. Three new solo processes per model, each closed before the next starts.
3. A fresh process per model: load, warm the fixed prefix, then the first user prefill/generation.
   The fixture has 167 characters; the uncached prompt parts contain 113 ENJP tokens and 111
   Qwen tokens, both exceeding the requested 84-token minimum.

CPU is the largest observed aggregate `ps %cpu` across MTLCompilerService processes while a case
runs. It is a sampled system observation, not a per-child attribution; desktop activity can
contribute. All timings below are milliseconds. These are small operational samples, not new
100-request latency distributions.

### Four simultaneously resident sidecars

| Start | Model | Embedded load_ms | Precompiled load_ms | Peak compiler CPU before / after |
| ---: | --- | ---: | ---: | ---: |
| 1 | ENJP | 10368.95 | 407.92 | 756.7% / 0% |
| 2 | Qwen | 10345.28 | 1168.00 | 723.9% / 0% |
| 3 | ENJP | 9471.99 | 128.83 | 668.8% / 0% |
| 4 | Qwen | 9111.11 | 418.34 | 643.3% / 0% |

The repeated approximately ten-second source-compilation stall is absent from these precompiled
loads, including later processes kept alive together. Warm/solo load measurements:

| Model | Three embedded load_ms | Three precompiled load_ms | Median before / after |
| --- | --- | --- | ---: |
| ENJP | 177.05, 159.63, 152.74 | 108.40, 112.86, 120.85 | 159.63 / 112.86 |
| Qwen | 433.63, 552.87, 413.27 | 402.50, 389.08, 400.81 | 433.63 / 400.81 |

### First prefill: remaining first-use cost

| Model | Prefix warm before / after | prompt_ms before / after | gen_ms before / after | New prompt tokens |
| --- | ---: | ---: | ---: | ---: |
| ENJP | 31.49 / 653.40 | 23.43 / 328.11 | 385.01 / 413.60 | 113 |
| Qwen | 310.72 / 1066.23 | 60.08 / 56.88 | 1643.89 / 1315.02 | 111 |

Both answers reached EOS (94 ENJP output tokens, 93 Qwen tokens). The first new precompiled
library use still produced compiler activity, up to 86.3% for ENJP and 71.7% for Qwen, consistent
with first-use GPU pipeline preparation. Pipeline caches differ between the previously exercised
embedded library and the new artifact, so these n=1 values do not isolate a prefill speedup or
regression. In particular, this change does **not** establish that first prefill is always fast
or eliminate all MTLCompilerService work. It fixes the measured repeated source-compile load
stall; warming at recording start remains important.

### Required-resource failure behavior and checks

The sidecar requires the two literal filenames before native initialization. Missing or
unreadable resources yield a correlated `load` error `metal_library_missing`. A successful GPU
backend registration is not sufficient: a temporary GPU backend must actually initialize;
failure, including a corrupt default library, yields `metal_library_load_failed` rather than
continuing with a CPU model. Backend initialization failure is now also a correlated load error,
not an unsolicited null-ID message followed by a dead worker.

`pnpm check:sidecar` passed: fmt, warnings-denied clippy, 11 unit tests and six process tests.
The three new black-box cases verify missing default, corrupt default and missing tensor. The
missing-tensor case fails before backend initialization even on this non-tensor M4 setup. Error
responses retain the request ID, the worker remains responsive, and stderr stays empty.

Local checks used `LLAMA_LIB_PROFILE=Release`, verified in both check/test native CMake caches.
That compiles native test/clippy dependencies as C++ Release instead of RelWithDebInfo, saving
disk. Rust test assertions remain enabled. It does not change shipping behavior: the sidecar's
ordinary `--release` build already selects native Release. No debug native build, second target
directory or app/DMG build was made. Free disk was checked before each build and remained above
the two-GiB stop threshold.

The real-model protocol/privacy/cache suites and Qwen non-thinking checks also passed against
the new release binary. Cancellation was 13.91 ms, busy stdin-EOF exit 13.56 ms, stderr zero
bytes, and `lsof -a -p PID -i` found no sockets. Qwen produced the three exact READY answers and
the explicit thinking control still behaved differently.

Known limit approved for this change: if a tensor file is present but cannot load on a
tensor-capable device, ggml can disable its tensor path. There is no extra digest manifest or
new dependency to override that upstream behavior. Bundled resources are sealed by the app's
code signature. A future test on an M5-class device would be needed to exercise that branch.

### Resource placement and signed minimal-bundle check

Required resource mappings are:

```json
{
  "binaries/default.metallib": "default.metallib",
  "binaries/ggml-tensor.metallib": "ggml-tensor.metallib"
}
```

The script stages sources next to the suffixed binary in `src-tauri/binaries/`. Tauri places the
mapped files next to the executable in dev (`target/debug/default.metallib` and
`target/debug/ggml-tensor.metallib`) and under `Contents/Resources/` in the macOS bundle.
`Contents/MacOS/` contains code only. The native loader first searches bundle resources and then
the directory of argv[0]; the sidecar's presence check uses the same supported placements.

Source verification, pinned to the tauri-build 2.7.4 tag:

- [macOS app bundler, lines 75 and 97](https://github.com/tauri-apps/tauri/blob/tauri-build-v2.7.4/crates/tauri-bundler/src/bundle/macos/app.rs#L75-L97)
  creates Contents/Resources and passes it to settings.copy_resources.
- [resource-map copying, lines 1223 to 1228](https://github.com/tauri-apps/tauri/blob/tauri-build-v2.7.4/crates/tauri-bundler/src/bundle/settings.rs#L1223-L1228)
  joins each resource.target() to that directory, so a filename target is at its root.
- [dev copy helper](https://github.com/tauri-apps/tauri/blob/tauri-build-v2.7.4/crates/tauri-build/src/lib.rs#L171-L185)
  and [map call at line 761](https://github.com/tauri-apps/tauri/blob/tauri-build-v2.7.4/crates/tauri-build/src/lib.rs#L755-L763)
  copy mapped resources into the Cargo target profile directory.

Without running tauri build, a minimal app was assembled only under this crate's ignored target.
Its main executable was a copied /usr/bin/true named Murmur; the separately named sidecar was
Contents/MacOS/murmur-llm, matching the real app/child layout. Info.plist names Murmur as the
main executable. Both metallibs were placed only in Contents/Resources.

The nested sidecar was signed first and the enclosing app second, both ad-hoc with hardened
runtime and no new entitlements. These commands returned zero for both executable and bundle:

```sh
codesign --verify --deep --strict --verbose=2 <path>
codesign -dv --verbose=4 <path>
```

The signature descriptions included adhoc and runtime. Executing the bundled sidecar path
successfully loaded ENJP on the GPU (748.98 ms), warmed its ten-token prefix (575.75 ms) and
reached EOS with a nonempty ten-token answer (prompt 210.12 ms, generation 70.39 ms). Stderr was
empty. Removing either default or tensor resource, then re-signing the negative-case bundle,
produced the expected correlated `metal_library_missing` (0.030 ms and 0.089 ms). Restoring the
resources and re-signing returned the bundle to a valid sealed state. The main stub/app was not
launched, no permissions/keychains were changed, and no claim about TCC continuity, notarization
or the actual updater follows from this check.

The active speech.cpp build explicitly uses EMBED=ON, does not forward the new GGML environment
setting, and has no newDefaultLibrary call in its source. Its embedded branch therefore does not
consult the sidecar's default.metallib resource.

### Build/CI behavior and reproduction

The build script fails before Cargo if `xcrun metal --version` is unavailable, names
`xcodebuild -downloadComponent MetalToolchain`, validates the active native build's EMBED=OFF
cache and both expected artifacts, and uses cmp-before-install so identical staged files are not
rewritten. It reads the active dependency output directory from Cargo JSON, not stale glob hits.
CI and release workflows install the Metal Toolchain only when its version probe fails.

```sh
export MURMUR_SIDECAR_BINARY="$PWD/src-tauri/binaries/murmur-llm-$(rustc --print host-tuple)"
PYTHONDONTWRITEBYTECODE=1 python3 crates/murmur-llm/metallib_bench.py \
  --label precompiled --output crates/murmur-llm/target/metallib-loads.json
```

The benchmark requires the two existing model files under `$MURMUR_TEST_MODELS/llm` (default
`~/.murmur-dev-models/llm`, as for the app's real-sidecar test) and does not download them. Local evidence: target/m4-before.json, m4-after.json, m4-platform-versions.json,
m4-bundle-check.json and m4-{checks,messages,qwen}.json. The benchmark runner is tracked; the
numeric reports and minimal signed bundle remain ignored under target. Final src-tauri staging
and the resource-map edit are delegated to the orchestrator's explicit staging slot, not done
by this agent while the user's dev app is running. Actual dev resource copying and the user's
post-restart E2E remain to be checked at that staging step.
