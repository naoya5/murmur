#!/usr/bin/env bash
# End-to-end test: a REAL dictation in the debug app, without a real voice or real keys.
#
# How: the debug build accepts a microphone named "wav:<path>" (plays a WAV file as the microphone) and
# SIGUSR1 / SIGUSR2 as the hotkey / Esc (ARCHITECTURE, deviations). The script builds the app once, runs it on a
# scratch data dir (MURMUR_DATA_DIR) with auto_paste off (nothing is typed anywhere; the result is only copied),
# and checks the clipboard (pbpaste) and the history database (sqlite3).
#
# Cases: ja, en, silence (no row, clipboard untouched), cancel (Esc), undo (Esc twice), crash (kill -9, spool
# recovery at the next start), long (77 s Japanese file through the chunk path).
# Several Murmur instances may run at once (a debug build with MURMUR_DATA_DIR skips single-instance); the script
#        only ever signals the PID it started.
# Needs: the speech models in $MURMUR_TEST_MODELS (default ~/.murmur-dev-models), macOS tools pbcopy/pbpaste,
#        sqlite3, python3. The user's text clipboard is saved and restored (images and other types are lost).
#
# Usage: scripts/e2e.sh [case ...]     cases: ja en silence cancel undo crash long   (default: all)
# Exit code: 0 all passed, 1 an assertion failed, 2 the environment is not ready.

set -u -o pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 2

MODELS="${MURMUR_TEST_MODELS:-$HOME/.murmur-dev-models}"
BIN="$ROOT/target/debug/murmur"
FIXTURES="${E2E_FIXTURES:-$ROOT/src-tauri/tests/fixtures}"
START_TIMEOUT=90     # seconds: app start (the model is preloaded in the background)
RESULT_TIMEOUT=30    # seconds: stop -> "session saved"
SILENCE_WAIT=8       # seconds to wait before declaring "nothing happened"

SCRATCH=""
DATA=""
LOG=""
APP_PID=""
CLIP_SAVE=""
FAILS=0
CHECKS=0

say() { printf '%s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
pass() { CHECKS=$((CHECKS + 1)); printf '  PASS  %s\n' "$*"; }
fail() { CHECKS=$((CHECKS + 1)); FAILS=$((FAILS + 1)); printf '  FAIL  %s\n' "$*"; }
die() { printf 'e2e: %s\n' "$*" >&2; exit 2; }

cleanup() {
  # An early exit can happen before the app helpers are defined.
  if type stop_app > /dev/null 2>&1; then stop_app; fi
  if [ -n "$CLIP_SAVE" ] && [ -f "$CLIP_SAVE" ]; then
    pbcopy < "$CLIP_SAVE"
  fi
  if [ -n "$SCRATCH" ] && [ -d "$SCRATCH" ]; then
    rm -r "$SCRATCH"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---------------------------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------------------------

for tool in pbcopy pbpaste sqlite3 python3 cargo; do
  command -v "$tool" > /dev/null 2>&1 || die "missing tool: $tool"
done
for wav in ja en silence; do
  [ -f "$FIXTURES/$wav.wav" ] || die "missing fixture $FIXTURES/$wav.wav"
done
for model in Haas-616M-ja-v1.0-Q8_0.gguf parakeet-tdt-0.6B-v3-Q8_0.gguf silero-vad-309K-v6.2-F32.gguf; do
  [ -f "$MODELS/$model" ] || die "missing model $MODELS/$model (set MURMUR_TEST_MODELS)"
done
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/murmur-e2e.XXXXXX")" || die "mktemp failed"
DATA="$SCRATCH/data"
mkdir -p "$DATA/models"
CLIP_SAVE="$SCRATCH/clipboard.saved"
pbpaste > "$CLIP_SAVE"

say "== build"
cargo build -p murmur 2>&1 | tail -3
[ -x "$BIN" ] || die "build failed: $BIN not found"

# ---------------------------------------------------------------------------------------------
# Models: symlinks plus valid .verified markers (see adapters/models.rs: path_if_installed)
# ---------------------------------------------------------------------------------------------

install_models() {
  python3 - "$ROOT/crates/murmur-core/src/catalog.rs" "$MODELS" "$DATA/models" <<'PY'
import json, os, re, sys
catalog, src_dir, dst_dir = sys.argv[1:4]
text = open(catalog, encoding="utf-8").read()
entries = re.findall(r'file:\s*"([^"]+)",\s*bytes:\s*([\d_]+),\s*sha256:\s*"([0-9a-f]{64})"', text)
installed = 0
for name, size, sha in entries:
    src = os.path.join(src_dir, name)
    if not os.path.isfile(src):
        continue
    size = int(size.replace("_", ""))
    st = os.stat(src)  # follows symlinks, like the app's metadata() on the link
    if st.st_size != size:
        print(f"size mismatch for {name}: {st.st_size} != {size}", file=sys.stderr)
        sys.exit(1)
    dst = os.path.join(dst_dir, name)
    if os.path.lexists(dst):
        os.remove(dst)
    os.symlink(src, dst)
    marker = {"size": size, "sha256": sha, "mtime_ms": st.st_mtime_ns // 1_000_000}
    with open(dst + ".verified", "w", encoding="utf-8") as f:
        json.dump(marker, f)
    installed += 1
print(f"installed {installed} model(s)")
PY
}
install_models || die "could not install models"

# ---------------------------------------------------------------------------------------------
# App control
# ---------------------------------------------------------------------------------------------

write_settings() { # <language> <wav>
  python3 - "$DATA/settings.json" "$1" "$2" <<'PY'
import json, sys
path, language, wav = sys.argv[1:4]
settings = {
    "schema_version": 1,
    "onboarding_completed": True,
    "microphone": "wav:" + wav,
    "language": language,
    "auto_paste": False,
    "restore_clipboard": False,
}
with open(path, "w", encoding="utf-8") as f:
    json.dump(settings, f)
PY
}

wait_for_log() { # <regex> <timeout s> [min count]   -> 0 when found
  local pattern="$1" timeout="$2" want="${3:-1}" waited=0
  while [ "$waited" -lt $((timeout * 10)) ]; do
    if [ "$(grep -c -E "$pattern" "$LOG" 2> /dev/null)" -ge "$want" ]; then
      return 0
    fi
    if [ -n "$APP_PID" ] && ! kill -0 "$APP_PID" 2> /dev/null; then
      return 2
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  return 1
}

start_app() { # <language> <wav>
  write_settings "$1" "$2"
  LOG="$SCRATCH/app-$(date +%s)-$RANDOM.log"
  NO_COLOR=1 MURMUR_DATA_DIR="$DATA" MURMUR_LOG=info "$BIN" > /dev/null 2> "$LOG" &
  APP_PID=$!
  if ! wait_for_log 'debug triggers' "$START_TIMEOUT"; then
    fail "app did not reach 'debug triggers' within ${START_TIMEOUT}s"
    sed 's/^/    log: /' "$LOG" | tail -15
    stop_app
    return 1
  fi
  local logged_pid
  logged_pid="$(plain_log | grep -E 'debug triggers' | head -1 | sed -E 's/.*pid=([0-9]+).*/\1/')"
  if [ -n "$logged_pid" ] && [ "$logged_pid" != "$APP_PID" ]; then
    APP_PID="$logged_pid"
  fi
  return 0
}

stop_app() {
  [ -n "$APP_PID" ] || return 0
  if kill -0 "$APP_PID" 2> /dev/null; then
    kill "$APP_PID" 2> /dev/null
    local i=0
    while kill -0 "$APP_PID" 2> /dev/null && [ "$i" -lt 50 ]; do
      sleep 0.1
      i=$((i + 1))
    done
    kill -9 "$APP_PID" 2> /dev/null
  fi
  APP_PID=""
}

# The log without ANSI colors (NO_COLOR is set, this is a second line of defense).
plain_log() { sed $'s/\033\\[[0-9;]*m//g' "$LOG"; }

hotkey() { kill -USR1 "$APP_PID"; }
esc() { kill -USR2 "$APP_PID"; }

wav_seconds() {
  python3 -c "import sys, wave; w = wave.open(sys.argv[1]); print(w.getnframes() / w.getframerate())" "$1"
}

rows() { sqlite3 "$DATA/murmur.db" "SELECT count(*) FROM transcripts" 2> /dev/null || echo 0; }

# The history row is written after delivery (G9), so poll for it.
wait_for_rows() { # <expected count> <timeout s>
  local waited=0
  while [ "$waited" -lt $(($2 * 10)) ]; do
    [ "$(rows)" -ge "$1" ] && return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  return 1
}

last_row() { # status|latency_ms|asr_ms|duration_ms|model_id of the newest row
  sqlite3 -separator '|' "$DATA/murmur.db" \
    "SELECT status, coalesce(latency_ms,''), coalesce(asr_ms,''), duration_ms, coalesce(model_id,'') FROM transcripts ORDER BY created_at DESC, id DESC LIMIT 1"
}

failure_lines() {
  grep -E 'model load failed|audio unavailable|transcription failed|stopping the recorder failed' "$LOG" 2> /dev/null | head -3
}

set_sentinel() { SENTINEL="E2E-SENTINEL-$RANDOM-$RANDOM"; printf '%s' "$SENTINEL" | pbcopy; }

# Starts a recording and waits until the (WAV) microphone is open.
begin_recording() {
  hotkey
  if ! wait_for_log 'recording started' 20; then
    fail "recording did not start"
    failure_lines | sed 's/^/    log: /'
    return 1
  fi
  local open_ms
  open_ms="$(plain_log | grep -E 'recording started' | head -1 | sed -E 's/.*open_ms=([0-9]+).*/\1/')"
  note "hotkey -> recording started: ${open_ms} ms"
  return 0
}

# ---------------------------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------------------------

case_dictation() { # <name> <language> <wav name> <expected word>...
  local name="$1" language="$2" wav="$FIXTURES/$3.wav"
  shift 3
  say "== $name"
  local before length
  before="$(rows)"
  length="$(wav_seconds "$wav")"
  set_sentinel
  start_app "$language" "$wav" || return
  begin_recording || { stop_app; return; }
  sleep "$(python3 -c "print($length + 0.5)")"
  hotkey
  if wait_for_log 'session saved' "$RESULT_TIMEOUT"; then
    pass "session saved"
  else
    fail "no 'session saved' within ${RESULT_TIMEOUT}s"
    failure_lines | sed 's/^/    log: /'
    stop_app
    return
  fi
  if wait_for_rows $((before + 1)) 10; then
    pass "one new history row"
  else
    fail "no new history row"
  fi
  local clip row status latency asr duration model
  clip="$(pbpaste)"
  for word in "$@"; do
    if printf '%s' "$clip" | grep -q -F -- "$word"; then
      pass "clipboard contains: $word"
    else
      fail "clipboard lacks: $word"
    fi
  done
  note "clipboard length: ${#clip} characters"
  row="$(last_row)"
  IFS='|' read -r status latency asr duration model <<< "$row"
  [ "$status" = "ok" ] && pass "row status ok" || fail "row status is '$status'"
  [ -n "$latency" ] && pass "latency_ms recorded" || fail "latency_ms missing"
  [ -n "$asr" ] && pass "asr_ms recorded" || fail "asr_ms missing"
  note "latency (stop -> clipboard): ${latency} ms, asr: ${asr} ms, audio: ${duration} ms, model: ${model}"
  stop_app
}

case_silence() {
  say "== silence"
  local before wav="$FIXTURES/silence.wav" length
  before="$(rows)"
  length="$(wav_seconds "$wav")"
  set_sentinel
  start_app ja "$wav" || return
  begin_recording || { stop_app; return; }
  sleep "$(python3 -c "print($length + 0.5)")"
  hotkey
  sleep "$SILENCE_WAIT"
  [ "$(rows)" = "$before" ] && pass "no new row" || fail "a row was written for silence"
  [ "$(pbpaste)" = "$SENTINEL" ] && pass "clipboard unchanged" || fail "clipboard changed"
  if grep -q 'session saved' "$LOG"; then fail "session was saved"; else pass "no 'session saved'"; fi
  failure_lines | sed 's/^/    log: /'
  stop_app
}

case_cancel() {
  say "== Esc cancel"
  local before wav="$FIXTURES/ja.wav"
  before="$(rows)"
  set_sentinel
  start_app ja "$wav" || return
  begin_recording || { stop_app; return; }
  sleep 1
  esc
  sleep 4 # the 3 s countdown ends and the recording is dropped
  [ "$(rows)" = "$before" ] && pass "no new row" || fail "a row was written after cancel"
  local left
  left="$(ls -A "$DATA/spool" 2> /dev/null | wc -l | tr -d ' ')"
  [ "$left" = "0" ] && pass "spool is empty" || fail "spool has $left file(s)"
  [ "$(pbpaste)" = "$SENTINEL" ] && pass "clipboard unchanged" || fail "clipboard changed"
  stop_app
}

case_undo() {
  say "== Esc undo"
  local before wav="$FIXTURES/ja.wav" length
  before="$(rows)"
  length="$(wav_seconds "$wav")"
  set_sentinel
  start_app ja "$wav" || return
  begin_recording || { stop_app; return; }
  sleep 0.3
  esc # cancel pending: capture is paused
  sleep 0.5
  esc # undo within 3 s: recording resumes
  sleep "$(python3 -c "print($length + 0.3)")"
  hotkey
  if wait_for_log 'session saved' "$RESULT_TIMEOUT"; then
    pass "session saved after undo"
  else
    fail "no 'session saved' after undo"
    failure_lines | sed 's/^/    log: /'
    stop_app
    return
  fi
  wait_for_rows $((before + 1)) 10 && pass "one new history row" || fail "no new history row"
  local status latency asr duration model
  IFS='|' read -r status latency asr duration model <<< "$(last_row)"
  [ "$status" = "ok" ] && pass "row status ok" || fail "row status is '$status'"
  note "latency: ${latency} ms, asr: ${asr} ms, audio kept: ${duration} ms"
  stop_app
}

case_crash() {
  say "== crash (kill -9 while recording, recovery at the next start)"
  local before wav="$FIXTURES/ja.wav"
  before="$(rows)"
  set_sentinel
  start_app ja "$wav" || return
  begin_recording || { stop_app; return; }
  sleep 2
  kill -9 "$APP_PID" 2> /dev/null
  wait "$APP_PID" 2> /dev/null # reap it, so the shell does not print "Killed: 9"
  APP_PID=""
  [ -n "$(ls -A "$DATA/spool" 2> /dev/null)" ] && pass "spool file survived the crash" || fail "no spool file after the crash"
  start_app ja "$wav" || return
  wait_for_rows $((before + 1)) 10 && pass "one new history row after restart" || fail "no recovered row"
  local row status duration path
  row="$(sqlite3 -separator '|' "$DATA/murmur.db" \
    "SELECT status, duration_ms, coalesce(audio_path,'') FROM transcripts ORDER BY created_at DESC, id DESC LIMIT 1")"
  IFS='|' read -r status duration path <<< "$row"
  [ "$status" = "recovered" ] && pass "row status recovered" || fail "row status is '$status'"
  [ -n "$path" ] && [ -f "$path" ] && pass "audio_path exists" || fail "audio_path missing: '$path'"
  if [ -n "$duration" ] && [ "$duration" -ge 1500 ] && [ "$duration" -le 2300 ]; then
    pass "duration about 2 s"
  else
    fail "duration is '$duration' ms"
  fi
  note "recovered audio: ${duration} ms (flush interval loses at most 250 ms)"
  stop_app
}

case_long() {
  if [ ! -f "$FIXTURES/ja60.wav" ]; then
    say "== long (skipped: no $FIXTURES/ja60.wav)"
    return
  fi
  case_dictation "long (77 s Japanese)" ja ja60
}

run_case() {
  case "$1" in
    ja) case_dictation "ja" ja ja 公園 コーヒー ;;
    en) case_dictation "en" en en settings hotkey ;;
    silence) case_silence ;;
    cancel) case_cancel ;;
    undo) case_undo ;;
    crash) case_crash ;;
    long) case_long ;;
    *) die "unknown case: $1 (ja en silence cancel undo crash long)" ;;
  esac
}

if [ "$#" -gt 0 ]; then
  CASES=("$@")
else
  CASES=(ja en silence cancel undo crash long)
fi
for c in "${CASES[@]}"; do
  run_case "$c"
done

say ""
if [ "$FAILS" -eq 0 ]; then
  say "e2e: all $CHECKS checks passed"
  exit 0
fi
say "e2e: $FAILS of $CHECKS checks FAILED"
exit 1
