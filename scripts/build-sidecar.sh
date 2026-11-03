#!/usr/bin/env bash
# Build the isolated llama.cpp executable and stage Tauri's host-suffixed externalBin/resources.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
if ! xcrun -sdk macosx metal --version >/dev/null 2>&1; then
  printf '%s\n' 'The Metal Toolchain is required to build murmur-llm.' \
    'Install it with: xcodebuild -downloadComponent MetalToolchain' >&2
  exit 1
fi
host=$(rustc --print host-tuple)
# The root .cargo/config.toml supplies GGML_METAL_EMBED_LIBRARY=OFF for every Cargo invocation.
# An explicit target directory keeps llama.cpp artifacts out of the app workspace.
target_dir="$root/crates/murmur-llm/target"
mkdir -p "$target_dir"
metadata=$(mktemp "$target_dir/build-sidecar.XXXXXX")
trap 'rm -f "$metadata"' EXIT
cd "$root"
cargo build --locked --release --manifest-path "$root/crates/murmur-llm/Cargo.toml" \
  --target "$host" --target-dir "$target_dir" --message-format=json-render-diagnostics >"$metadata"
# Cargo reports the active dependency output even on a cached build. Never select a stale
# metallib by globbing old native build directories.
native_bin=$(python3 - "$metadata" <<'PY'
import json
from pathlib import Path
import sys
outputs = []
for line in Path(sys.argv[1]).read_text().splitlines():
    entry = json.loads(line)
    if (entry.get("reason") == "build-script-executed"
            and "llama-cpp-sys-2@" in entry.get("package_id", "")):
        outputs.append(Path(entry["out_dir"]))
if len(outputs) != 1:
    sys.exit("Could not identify the active llama-cpp-sys-2 Metal artifacts from Cargo output")
cache = (outputs[0] / "build/CMakeCache.txt").read_text().splitlines()
if "GGML_METAL_EMBED_LIBRARY:BOOL=OFF" not in cache:
    sys.exit("murmur-llm must use GGML_METAL_EMBED_LIBRARY=OFF from .cargo/config.toml")
print(outputs[0] / "bin")
PY
)
for name in default.metallib ggml-tensor.metallib; do
  if [ ! -f "$native_bin/$name" ]; then
    printf 'Missing precompiled Metal artifact: %s\n' "$name" >&2
    printf '%s\n' 'Use an Xcode 26 or newer macOS SDK and the Metal Toolchain.' >&2
    exit 1
  fi
done
stage() {
  local source=$1 destination=$2 mode=$3
  if [ ! -f "$destination" ] || ! cmp -s "$source" "$destination" \
    || { [ "$mode" = 755 ] && [ ! -x "$destination" ]; }; then
    install -m "$mode" "$source" "$destination"
    printf 'Staged %s\n' "${destination#"$root/"}"
  fi
}
mkdir -p "$root/src-tauri/binaries"
stage "$target_dir/$host/release/murmur-llm" \
  "$root/src-tauri/binaries/murmur-llm-$host" 755
for name in default.metallib ggml-tensor.metallib; do
  stage "$native_bin/$name" "$root/src-tauri/binaries/$name" 644
done
printf 'Sidecar and precompiled Metal libraries ready for %s\n' "$host"
