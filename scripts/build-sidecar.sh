#!/usr/bin/env bash
# Build the isolated llama.cpp executable and stage Tauri's host-suffixed externalBin.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
host=$(rustc --print host-tuple)
# An explicit target directory keeps llama.cpp artifacts out of the app workspace, including
# when the caller exports CARGO_TARGET_DIR. Cross/universal builds need one build per target.
target_dir="$root/crates/murmur-llm/target"
cargo build --locked --release --manifest-path "$root/crates/murmur-llm/Cargo.toml" \
  --target "$host" --target-dir "$target_dir"
mkdir -p "$root/src-tauri/binaries"
install -m 755 "$target_dir/$host/release/murmur-llm" \
  "$root/src-tauri/binaries/murmur-llm-$host"
printf 'Staged src-tauri/binaries/murmur-llm-%s\n' "$host"
