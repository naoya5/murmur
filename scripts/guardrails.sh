#!/usr/bin/env bash
# Guardrail checks that clippy and the compiler cannot do (PLAN 2.15, RULES.md).
# Runs from any directory. Exits non-zero if any check fails and reports all failures.
# Works on macOS bash 3.2 (no mapfile, no associative arrays).
set -u
cd "$(dirname "$0")/.." || exit 1

# R10 / G10: every source of truth that must exist now.
# Later tickets append their ids here (db-schema, ui-strings, design-tokens).
REQUIRED_SOT_IDS=(
  session-state-machine
  settings
  model-catalog
  text-rules
  metrics
  ports
  recognition-plan
  app-version
  db-schema
  ipc
  ui-strings
  design-tokens
  annotations
)

# G2: the only file that may use the speech SDK crate.
SPEECH_SYS_ALLOWED="src-tauri/src/adapters/engine.rs"
# G11: the only file that may use an HTTP client.
HTTP_ALLOWED="src-tauri/src/adapters/models.rs"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# Tracked files plus new untracked files that are not ignored. The speech-sys crate and
# docs are excluded from the code greps; lockfiles are generated.
all_files() { git ls-files --cached --others --exclude-standard | sort -u; }
code_files() {
  all_files | grep -E '\.(rs|toml)$' | grep -v -E '^(crates/speech-sys/|docs/)' | grep -v '^Cargo\.lock$'
}

echo "== G1: murmur-core has no Tauri, HTTP or speech dependency"
core_tree=$(cargo tree -p murmur-core -e normal --prefix none 2>&1) || {
  fail "cargo tree -p murmur-core failed: $core_tree"
  core_tree=""
}
bad=$(printf '%s\n' "$core_tree" | grep -E '^(tauri|wry|tao|speech-sys|reqwest|ureq|hyper)([ -]|$)' || true)
[ -z "$bad" ] || fail "murmur-core depends on forbidden crates:
$bad"

echo "== G2: speech_sys is used only in $SPEECH_SYS_ALLOWED"
# Rust paths use `speech_sys`; manifests use `speech-sys` (allowed in src-tauri/Cargo.toml only).
hits=$(code_files | grep '\.rs$' | grep -v -x "$SPEECH_SYS_ALLOWED" | xargs grep -n -E '\bspeech_sys\b' 2>/dev/null || true)
[ -z "$hits" ] || fail "speech_sys used outside $SPEECH_SYS_ALLOWED:
$hits"
hits=$(code_files | grep '\.toml$' | grep -v -x 'src-tauri/Cargo.toml' | xargs grep -n -E '^\s*speech-sys\b' 2>/dev/null || true)
[ -z "$hits" ] || fail "speech-sys dependency declared outside src-tauri/Cargo.toml:
$hits"

echo "== G11: HTTP clients are used only in $HTTP_ALLOWED"
hits=$(code_files | grep '\.rs$' | grep -v -x "$HTTP_ALLOWED" | xargs grep -n -E '\b(reqwest|ureq|hyper)\b' 2>/dev/null || true)
[ -z "$hits" ] || fail "HTTP client used outside $HTTP_ALLOWED:
$hits"
hits=$(code_files | grep '\.toml$' | grep -v -x 'src-tauri/Cargo.toml' | xargs grep -n -E '^\s*(reqwest|ureq|hyper)\b' 2>/dev/null || true)
[ -z "$hits" ] || fail "HTTP client dependency declared outside src-tauri/Cargo.toml:
$hits"

echo "== R2: no em dash (U+2014) in text files"
# LC_ALL=C makes grep match the raw UTF-8 bytes; -I skips binary files.
hits=$(all_files | while IFS= read -r f; do
  [ -f "$f" ] && LC_ALL=C grep -I -n -H $'\xe2\x80\x94' "$f"
done || true)
[ -z "$hits" ] || fail "em dash found (use '-'):
$hits"

echo "== R10: required SOT ids are declared"
sot_hits=$(all_files | grep -E '\.(rs|toml|ts|tsx|md|css)$' | grep -v -E '^(docs/|scripts/)' | xargs grep -n -E 'SOT: [a-z0-9-]+' 2>/dev/null || true)
for id in "${REQUIRED_SOT_IDS[@]}"; do
  printf '%s\n' "$sot_hits" | grep -q -E "SOT: ${id}([^a-z0-9-]|\$)" || fail "missing SOT declaration: $id"
done

echo "== R14: src/ipc/bindings.ts matches the Rust types"
bindings=src/ipc/bindings.ts
before=$(mktemp)
if [ -f "$bindings" ]; then cp "$bindings" "$before"; else : >"$before"; fi
if cargo test -p murmur export_bindings >/dev/null 2>&1; then
  if ! cmp -s "$before" "$bindings"; then
    fail "$bindings was out of date and has been regenerated. Review and commit it."
    git --no-pager diff --no-index --stat "$before" "$bindings" >&2 || true
  fi
else
  fail "cargo test -p murmur export_bindings failed"
fi
rm -f "$before"

if [ "$failures" -ne 0 ]; then
  echo "guardrails: $failures check(s) failed" >&2
  exit 1
fi
echo "guardrails: all checks passed"
