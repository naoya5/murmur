#!/usr/bin/env bash
# End-to-end test: a REAL dictation in the debug app, without a real voice or real keys.
#
# How: the debug build accepts a microphone named "wav:<path>" (plays a WAV file as the microphone) and
# SIGUSR1 / SIGUSR2 as the hotkey / Esc (docs/ARCHITECTURE.md, "Recorded decisions"). The script builds the app once, runs it on a
# scratch data dir (MURMUR_DATA_DIR) with auto_paste off (nothing is typed anywhere; the result is only copied),
# and checks the clipboard (pbpaste) and the history database (sqlite3).
#
# Cases: ja, en, silence (no row, clipboard untouched), cancel (Esc), undo (Esc twice), crash (kill -9, spool
# recovery at the next start), long (77 s Japanese file through the chunk path), memory (the app's footprint with the
# model loaded stays under MEMORY_LIMIT_MB).
# Annotations (experimental, opt-in group `annotations`): the screen is a folder of generated PNGs
# (MURMUR_DEBUG_SCREEN=png-dir:), Shift and the strokes come from a JSON script (MURMUR_DEBUG_POINTER=), and
# SIGUSR1 is the annotations hotkey (MURMUR_DEBUG_HOTKEY=annotations). Set E2E_KEEP_DIR to keep the normal and the
# marked bundle folders for a look.
# Several Murmur instances may run at once (a debug build with MURMUR_DATA_DIR skips single-instance); the script
#        only ever signals the PID it started.
# Needs: the speech models in $MURMUR_TEST_MODELS (default ~/.murmur-dev-models), macOS tools pbcopy/pbpaste,
#        sqlite3, python3. The user's text clipboard is saved and restored (images and other types are lost).
#
# Usage: scripts/e2e.sh [case ...]     cases: ja en silence cancel undo crash long memory   (default: all of these)
#        opt-in: mic (makes sound: real microphone + afplay through the speakers), tenmin (about 12 minutes),
#                idle (the model unloads N idle minutes after its last use, about 6 minutes),
#                annotations (the annotation group, about 3 minutes; or one of annot annot-marks
#                annot-nospeech-marks annot-nospeech-nomarks annot-cancel annot-undo annot-disable annot-screenfail
#                annot-compose-timeout annot-nomodel annot-words), and annot-target (pastes into TextEdit on the
#                user's screen: a person must be at the Mac, nothing else may hold the keyboard for ~15 s)
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
MEMORY_LIMIT_MB=200  # the app's footprint (Activity Monitor's "Memory") with the model loaded

SCRATCH=""
DATA=""
LOG=""
APP_PID=""
CLIP_SAVE=""
VOLUME_SAVE=""
AFPLAY_PID=""
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
  if [ -n "$AFPLAY_PID" ]; then kill "$AFPLAY_PID" 2> /dev/null; fi
  if [ -n "$VOLUME_SAVE" ]; then osascript -e "set volume output volume $VOLUME_SAVE" > /dev/null 2>&1; fi
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
TMP_ROOT="${TMPDIR:-/tmp}"
TMP_ROOT="${TMP_ROOT%/}" # a trailing slash would show up twice in every path
SCRATCH="$(mktemp -d "$TMP_ROOT/murmur-e2e.XXXXXX")" || die "mktemp failed"
DATA="$SCRATCH/data"
mkdir -p "$DATA/models"
CLIP_SAVE="$SCRATCH/clipboard.saved"
pbpaste > "$CLIP_SAVE"

say "== build"
# The app build needs the staged sidecar (Tauri externalBin), so build that first. A failed build
# stops here: an older binary left in target/ would otherwise run as if it were the current code.
BUILD_LOG="$SCRATCH/build.log" # removed with the scratch dir, so a failure prints its tail
bash scripts/build-sidecar.sh > "$BUILD_LOG" 2>&1 || { tail -20 "$BUILD_LOG" >&2; die "sidecar build failed"; }
cargo build -p murmur >> "$BUILD_LOG" 2>&1 || { tail -20 "$BUILD_LOG" >&2; die "build failed"; }
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
import os
settings = {
    "schema_version": 1,
    "onboarding_completed": True,
    "microphone": ("wav:" + wav) if wav else None,  # empty = the system default microphone
    "language": language,
    "auto_paste": os.environ.get("E2E_AUTO_PASTE") == "1",
    "restore_clipboard": False,
    "annotations_enabled": os.environ.get("E2E_ANNOTATIONS") == "1",
}
if os.environ.get("E2E_UNLOAD_MIN"):
    settings["unload_model_after_min"] = int(os.environ["E2E_UNLOAD_MIN"])
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

now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# Epoch milliseconds of the newest log line matching the regex (the app logs UTC timestamps), or
# nothing when no line matches or its timestamp cannot be read.
log_time_ms() { # <regex>
  plain_log | grep -E "$1" | tail -1 | python3 -c '
import datetime, re, sys
m = re.match(r"\s*(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?)Z", sys.stdin.read())
if m:
    t = datetime.datetime.fromisoformat(m.group(1)).replace(tzinfo=datetime.timezone.utc)
    print(int(t.timestamp() * 1000))
'
}

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
  grep -E 'could not start|model load failed|audio unavailable|transcription failed|stopping the recorder failed' "$LOG" 2> /dev/null | head -3
}

set_sentinel() { SENTINEL="E2E-SENTINEL-$RANDOM-$RANDOM"; printf '%s' "$SENTINEL" | pbcopy; }

# Starts a recording and waits until the (WAV) microphone is open.
begin_recording() {
  local seen
  seen="$(grep -c 'recording started' "$LOG")" # earlier recordings of this app instance do not count
  hotkey
  if ! wait_for_log 'recording started' 20 $((seen + 1)); then
    fail "recording did not start"
    failure_lines | sed 's/^/    log: /'
    return 1
  fi
  local open_ms
  open_ms="$(plain_log | grep -E 'recording started' | tail -1 | sed -E 's/.*open_ms=([0-9]+).*/\1/')"
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
  # The expected words of the fixture, except "hotkey": the debug trigger line names the key.
  local -a private_words=()
  local w
  for w in "$@"; do [ "$w" = hotkey ] || private_words+=("$w"); done
  assert_log_private 公園 コーヒー "${private_words[@]}"
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
  local kill_ms started_ms
  kill_ms="$(now_ms)"
  kill -9 "$APP_PID" 2> /dev/null
  wait "$APP_PID" 2> /dev/null # reap it, so the shell does not print "Killed: 9"
  APP_PID=""
  started_ms="$(log_time_ms 'recording started')"
  [ -n "$(ls -A "$DATA/spool" 2> /dev/null)" ] && pass "spool file survived the crash" || fail "no spool file after the crash"
  start_app ja "$wav" || return
  wait_for_rows $((before + 1)) 10 && pass "one new history row after restart" || fail "no recovered row"
  local row status duration path
  row="$(sqlite3 -separator '|' "$DATA/murmur.db" \
    "SELECT status, duration_ms, coalesce(audio_path,'') FROM transcripts ORDER BY created_at DESC, id DESC LIMIT 1")"
  IFS='|' read -r status duration path <<< "$row"
  [ "$status" = "recovered" ] && pass "row status recovered" || fail "row status is '$status'"
  [ -n "$path" ] && [ -f "$path" ] && pass "audio_path exists" || fail "audio_path missing: '$path'"
  # The WAV microphone runs on a real clock from its open, so the spool holds at most the wall time
  # from the open to the kill (the log line comes a little after the open: up to 150 ms more), and
  # at least that minus the flush interval (250 ms) and some scheduling (150 ms). The window follows
  # the measured time, so a slow machine does not fail it and a missing or extra second still does.
  local expected
  if [ -z "$started_ms" ]; then
    fail "no timestamp on the 'recording started' log line"
  else
    expected=$((kill_ms - started_ms))
    if [ -n "$duration" ] && [ "$duration" -ge $((expected - 400)) ] && [ "$duration" -le $((expected + 150)) ]; then
      pass "duration ${duration} ms matches the ${expected} ms from start to kill"
    else
      fail "duration is '$duration' ms, expected about ${expected} ms (start to kill)"
    fi
    note "recovered audio: ${duration} ms; start to kill ${expected} ms (flush interval loses at most 250 ms)"
  fi
  stop_app
}

case_long() {
  if [ ! -f "$FIXTURES/ja60.wav" ]; then
    say "== long (skipped: no $FIXTURES/ja60.wav)"
    return
  fi
  case_dictation "long (77 s Japanese)" ja ja60
}

# Opt-in (it makes sound): the REAL microphone path. The fixture is played through the speakers with afplay and
# picked up by the default microphone. Room acoustics matter, so only one expected word per language is required.
mic_one() { # <name> <language> <wav name> <word>...
  local name="$1" language="$2" wav="$FIXTURES/$3.wav"
  shift 3
  say "== mic: $name (real microphone, played through the speakers)"
  local before
  before="$(rows)"
  set_sentinel
  start_app "$language" "" || return
  begin_recording || { stop_app; return; }
  afplay "$wav" &
  AFPLAY_PID=$!
  wait "$AFPLAY_PID" 2> /dev/null
  AFPLAY_PID=""
  sleep 0.7
  hotkey
  if wait_for_log 'session saved' "$RESULT_TIMEOUT"; then
    pass "session saved"
  else
    fail "no 'session saved' within ${RESULT_TIMEOUT}s"
    failure_lines | sed 's/^/    log: /'
    note "clipboard: $(pbpaste)"
    stop_app
    return
  fi
  wait_for_rows $((before + 1)) 10 && pass "one new history row" || fail "no new history row"
  local clip found="" word row status latency asr duration model
  clip="$(pbpaste)"
  for word in "$@"; do
    if printf '%s' "$clip" | grep -q -F -- "$word"; then found="$found $word"; fi
  done
  if [ -n "$found" ]; then
    pass "clipboard contains at least one expected word:$found"
  else
    fail "clipboard has none of: $*"
  fi
  note "clipboard text (our own fixture, not user data): $clip"
  row="$(last_row)"
  IFS='|' read -r status latency asr duration model <<< "$row"
  [ "$status" = "ok" ] && pass "row status ok" || fail "row status is '$status'"
  note "latency (stop -> clipboard): ${latency} ms, asr: ${asr} ms, audio: ${duration} ms, model: ${model}"
  # The expected words of the fixture, except "hotkey": the debug trigger line names the key.
  local -a private_words=()
  local w
  for w in "$@"; do [ "$w" = hotkey ] || private_words+=("$w"); done
  assert_log_private 公園 コーヒー "${private_words[@]}"
  stop_app
}

case_mic() {
  command -v afplay > /dev/null 2>&1 || die "afplay not found"
  VOLUME_SAVE="$(osascript -e 'output volume of (get volume settings)' 2> /dev/null)"
  case "$VOLUME_SAVE" in
    '' | *[!0-9]*) VOLUME_SAVE="" ;; # "missing value": a device without a volume control
  esac
  osascript -e 'set volume output volume 45' > /dev/null 2>&1
  note "output volume: ${VOLUME_SAVE:-unknown} -> 45 (restored at the end)"
  mic_one "ja" ja ja 公園 コーヒー
  mic_one "en" en en settings hotkey
  if [ -n "$VOLUME_SAVE" ]; then
    osascript -e "set volume output volume $VOLUME_SAVE" > /dev/null 2>&1
    VOLUME_SAVE=""
  fi
}

# ---------------------------------------------------------------------------------------------
# Hardening cases (PLAN phase 8)
# ---------------------------------------------------------------------------------------------

# A clean data dir with all three models installed. The app must be stopped.
fresh_data() {
  stop_app
  rm -r "$DATA"
  mkdir -p "$DATA/models"
  install_models > /dev/null || die "could not install models"
}

remove_recognition_models() {
  find "$DATA/models" \( -name 'Haas*' -o -name 'parakeet*' -o -name 'Qwen*' \) -delete
}

# Plays the fixture and stops: one complete dictation on the running app.
dictate_once() { # <wav name>
  local length
  length="$(wav_seconds "$FIXTURES/$1.wav")"
  begin_recording || return 1
  sleep "$(python3 -c "print($length + 0.5)")"
  hotkey
}

# Spool files that no failed/recovered row refers to (a leftover, or an ok row that kept its audio).
spool_orphans() {
  local f
  for f in "$DATA"/spool/*.pcm; do
    [ -e "$f" ] || continue
    [ -n "$(sqlite3 "$DATA/murmur.db" "SELECT 1 FROM transcripts WHERE audio_path='$f' AND status IN ('failed','recovered') LIMIT 1" 2> /dev/null)" ] || echo "$f"
  done
}

# Rows that are neither a complete ok row nor a failed/recovered row with audio.
broken_rows() {
  sqlite3 -separator '|' "$DATA/murmur.db" "SELECT id, status, coalesce(error,''), coalesce(audio_path,'') FROM transcripts WHERE NOT ((status = 'ok' AND text IS NOT NULL AND latency_ms IS NOT NULL AND audio_path IS NULL) OR (status IN ('failed','recovered') AND audio_path IS NOT NULL))" 2> /dev/null
}

no_panic() {
  if grep -q -i 'panicked' "$LOG" 2> /dev/null; then
    fail "the log contains a panic"
    grep -i 'panicked' "$LOG" | head -3 | sed 's/^/    log: /'
  else
    pass "no panic in the log"
  fi
}

# G7: the log must never carry what the user said or saw: spoken words, app names, window titles.
# Checks the given substrings plus the name of the app in front during the run (the real one).
assert_log_private() { # <substring>...
  local hit="" s front
  front="$(front_app_name)"
  # Murmur itself may be in front (the dev app restarts when its sources change): its own name is in every log.
  case "$front" in [Mm]urmur*) front="" ;; esac
  for s in "$@" "$front"; do
    [ -n "$s" ] || continue
    if grep -qF -- "$s" "$LOG" 2> /dev/null; then hit="$s"; break; fi
  done
  if [ -n "$hit" ]; then
    fail "G7: the log contains private text ($hit)"
  else
    pass "G7: no spoken words, app names or window titles in the log"
  fi
}

front_bundle_id() {
  osascript -e 'tell application "System Events" to get bundle identifier of first process whose frontmost is true' 2> /dev/null
}

front_app_name() {
  lsappinfo info -only name "$(lsappinfo front 2> /dev/null)" 2> /dev/null | sed -E 's/.*"LSDisplayName"="([^"]*)".*/\1/'
}

app_alive() {
  [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2> /dev/null && pass "$1: the app is still running" || fail "$1: the app died"
}

# Asserts the newest row is a failed one with the given error and an existing audio file.
assert_failed_row() { # <error> <label>
  local row status error path
  row="$(sqlite3 -separator '|' "$DATA/murmur.db" \
    "SELECT status, coalesce(error,''), coalesce(audio_path,'') FROM transcripts ORDER BY created_at DESC, id DESC LIMIT 1")"
  IFS='|' read -r status error path <<< "$row"
  [ "$status" = "failed" ] && pass "$2: row status failed" || fail "$2: row status is '$status'"
  [ "$error" = "$1" ] && pass "$2: error is $1" || fail "$2: error is '$error'"
  [ -n "$path" ] && [ -f "$path" ] && pass "$2: audio file kept" || fail "$2: audio file missing ('$path')"
}

case_nomodel() {
  say "== nomodel (VAD only, no recognition model)"
  fresh_data
  remove_recognition_models
  set_sentinel
  start_app ja "$FIXTURES/ja.wav" || return
  local round
  for round in 1 2; do
    dictate_once ja || { stop_app; return; }
    if wait_for_rows "$round" 30; then
      pass "dictation $round: a row was written"
      assert_failed_row model_missing "dictation $round"
    else
      fail "dictation $round: no row within 30 s"
      failure_lines | sed 's/^/    log: /'
      break
    fi
  done
  [ "$(pbpaste)" = "$SENTINEL" ] && pass "clipboard unchanged" || fail "clipboard changed"
  app_alive nomodel
  no_panic
  stop_app
}

case_corrupt() {
  say "== corrupt (recognition model present, marker does not match)"
  local variant
  for variant in size sha; do
    fresh_data
    python3 - "$DATA/models/Haas-616M-ja-v1.0-Q8_0.gguf.verified" "$variant" <<'PY'
import json, sys
path, variant = sys.argv[1:3]
marker = json.load(open(path))
if variant == "size":
    marker["size"] -= 1
else:
    marker["sha256"] = "0" * 64
json.dump(marker, open(path, "w"))
PY
    start_app ja "$FIXTURES/ja.wav" || return
    dictate_once ja || { stop_app; return; }
    if wait_for_rows 1 30; then
      assert_failed_row model_missing "marker with wrong $variant"
    else
      fail "marker with wrong $variant: no row within 30 s (the file was trusted?)"
      failure_lines | sed 's/^/    log: /'
    fi
    no_panic
    stop_app
  done
}

case_mash() {
  say "== mash (12 hotkeys within 1.5 s)"
  fresh_data
  start_app ja "$FIXTURES/ja.wav" || return
  local i
  for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
    hotkey
    sleep 0.125
  done
  sleep 10
  app_alive mash
  no_panic
  local started saved
  started="$(grep -c 'recording started' "$LOG")"
  saved="$(grep -c 'session saved' "$LOG")"
  note "recordings started: $started, sessions saved: $saved, rows: $(rows)"
  local bad
  bad="$(broken_rows)"
  [ -z "$bad" ] && pass "every row is complete (ok with text, or failed/recovered with audio)" || { fail "incomplete rows:"; printf '%s\n' "$bad" | sed 's/^/    row: /'; }
  # A recording may still be open (an odd number of toggles got through): it shows as an unreferenced spool file.
  bad="$(spool_orphans)"
  if [ -n "$bad" ]; then
    note "a recording was still open after the mash; closing it with one more hotkey"
    hotkey
    sleep 8
    bad="$(spool_orphans)"
  else
    note "no recording was open after the mash"
  fi
  [ -z "$bad" ] && pass "spool holds only files of failed/recovered rows" || { fail "orphan spool files:"; printf '%s\n' "$bad" | sed 's/^/    file: /'; }
  # The app still answers a normal dictation (up to two attempts: the state after the mash is not known).
  local before attempt ok=""
  before="$(rows)"
  set_sentinel
  for attempt in 1 2; do
    dictate_once ja || break
    if wait_for_rows $((before + 1)) 25; then ok=1; break; fi
    hotkey # it may have been a "stop" of a recording that was still open
    sleep 3
    before="$(rows)"
  done
  if [ -n "$ok" ]; then
    pass "a normal dictation works afterwards (attempt $attempt)"
    local clip
    clip="$(pbpaste)"
    if printf '%s' "$clip" | grep -q -e 公園 -e コーヒー; then pass "and its text is right"; else fail "wrong text afterwards: $clip"; fi
  else
    fail "no normal dictation possible after the mash"
    failure_lines | sed 's/^/    log: /'
  fi
  stop_app
}

case_escmash() {
  say "== esc-mash (6 Esc within 1 s)"
  fresh_data
  start_app ja "$FIXTURES/ja.wav" || return
  begin_recording || { stop_app; return; }
  local i
  for i in 1 2 3 4 5 6; do
    esc
    sleep 0.17
  done
  sleep 6
  app_alive esc-mash
  no_panic
  local n
  n="$(rows)"
  [ "$n" -le 1 ] && pass "at most one row ($n)" || fail "$n rows"
  local bad
  bad="$(broken_rows)"
  [ -z "$bad" ] && pass "every row is complete" || { fail "incomplete rows:"; printf '%s\n' "$bad" | sed 's/^/    row: /'; }
  # Quit and start again: whatever was still being recorded comes back through the spool recovery.
  stop_app
  start_app ja "$FIXTURES/ja.wav" || return
  sleep 1
  bad="$(spool_orphans)"
  [ -z "$bad" ] && pass "after restart the spool holds only files of failed/recovered rows" || { fail "orphan spool files:"; printf '%s\n' "$bad" | sed 's/^/    file: /'; }
  note "rows after restart: $(rows) (statuses: $(sqlite3 "$DATA/murmur.db" "SELECT group_concat(status) FROM transcripts"))"
  stop_app
}

case_readonly() {
  say "== readonly (the database cannot be written)"
  fresh_data
  # Create the database, then close the app so no WAL files remain.
  start_app ja "$FIXTURES/ja.wav" || return
  stop_app
  chmod 444 "$DATA/murmur.db"
  set_sentinel
  start_app ja "$FIXTURES/ja.wav" || { chmod 644 "$DATA/murmur.db"; return; }
  dictate_once ja || { chmod 644 "$DATA/murmur.db"; stop_app; return; }
  if wait_for_log 'session saved' "$RESULT_TIMEOUT"; then
    pass "session saved (delivery does not depend on the database)"
  else
    fail "no 'session saved'"
    failure_lines | sed 's/^/    log: /'
  fi
  sleep 2
  local clip
  clip="$(pbpaste)"
  if printf '%s' "$clip" | grep -q -e 公園 -e コーヒー; then pass "clipboard has the text"; else fail "clipboard: '$clip'"; fi
  grep -E 'could not save|readonly|read-only' "$LOG" | head -2 | sed 's/^/    log: /'
  [ "$(rows)" = "0" ] && pass "no row could be written" || fail "a row was written into a read-only database?"
  local files
  files="$(ls -A "$DATA/spool" | wc -l | tr -d ' ')"
  [ "$files" = "1" ] && pass "the spool file was kept" || fail "spool has $files file(s)"
  app_alive readonly
  stop_app
  chmod 644 "$DATA/murmur.db"
  start_app ja "$FIXTURES/ja.wav" || return
  wait_for_rows 1 10 && pass "after restart: one row" || fail "after restart: no row"
  local row status path duration
  IFS='|' read -r status duration path <<< "$(sqlite3 -separator '|' "$DATA/murmur.db" "SELECT status, duration_ms, coalesce(audio_path,'') FROM transcripts LIMIT 1")"
  [ "$status" = "recovered" ] && pass "the audio came back as a recovered row" || fail "row status is '$status'"
  [ -n "$path" ] && [ -f "$path" ] && pass "its audio file exists (${duration} ms)" || fail "audio missing"
  stop_app
}

# Opt-in (slow, about 12 minutes): the recording stops by itself at 10 minutes.
case_tenmin() {
  say "== tenmin (10 min 30 s of audio; the recorder must stop at 10 min)"
  fresh_data
  local wav="$SCRATCH/ten-minutes.wav"
  python3 - "$FIXTURES/ja.wav" "$wav" 630 <<'PY'
import sys, wave
src, dst, seconds = sys.argv[1], sys.argv[2], int(sys.argv[3])
with wave.open(src) as w:
    params, frames = w.getparams(), w.readframes(w.getnframes())
out = wave.open(dst, "wb")
out.setparams(params)
need = seconds * params.framerate * params.sampwidth * params.nchannels
buf = b""
while len(buf) < need:
    buf += frames
out.writeframes(buf[:need])
out.close()
PY
  start_app ja "$wav" || return
  begin_recording || { stop_app; return; }
  note "recording; waiting for the automatic stop at 10 min (no hotkey is sent)"
  if wait_for_log 'session saved' 900; then
    pass "session saved without a second hotkey"
  else
    fail "no 'session saved' within 900 s"
    failure_lines | sed 's/^/    log: /'
    stop_app
    return
  fi
  wait_for_rows 1 20 && pass "one row" || fail "no row"
  local status latency asr duration model
  IFS='|' read -r status latency asr duration model <<< "$(last_row)"
  [ "$status" = "ok" ] && pass "row status ok" || fail "row status is '$status'"
  if [ -n "$duration" ] && [ "$duration" -ge 598000 ] && [ "$duration" -le 602000 ]; then
    pass "duration about 600000 ms"
  else
    fail "duration is '$duration' ms"
  fi
  note "latency (stop -> clipboard): ${latency} ms, asr: ${asr} ms, audio: ${duration} ms, clipboard: $(pbpaste | wc -c | tr -d ' ') bytes"
  stop_app
}

# ---------------------------------------------------------------------------------------------
# Annotations (experimental)
# ---------------------------------------------------------------------------------------------

ANNOT_FIX=""

# 12 PNG frames (320 x 180): a bright block that moves after frame 6, so a "change" keyframe exists;
# and a pointer script with a click, a stroke and a dwell (cursor still for 0.9 s while the WAV speaks).
make_annotation_fixtures() {
  [ -n "$ANNOT_FIX" ] && return 0
  ANNOT_FIX="$SCRATCH/annot"
  mkdir -p "$ANNOT_FIX/frames"
  python3 - "$ANNOT_FIX" <<'PY'
import json, os, struct, sys, zlib
root = sys.argv[1]
w, h = 320, 180
def png(path, block_x):
    rows = []
    for y in range(h):
        row = bytearray(b"\x00")
        for x in range(w):
            inside = block_x <= x < block_x + 120 and 50 <= y < 130
            row += bytes((230, 230, 230) if inside else (40, 40, 40))
        rows.append(bytes(row))
    raw = b"".join(rows)
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    data += chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(data)
for i in range(12):
    png(os.path.join(root, "frames", f"{i:02d}.png"), 20 if i < 6 else 180)
# Shift goes down, one stroke is drawn (two strokes within 1.5 s form one group), Shift goes up.
events = [
    {"kind": "shift", "t_ms": 1800, "held": True},
    {"kind": "stroke", "t0_ms": 2000, "t1_ms": 2600, "points": [[0.2, 0.2], [0.3, 0.3], [0.4, 0.35]]},
    {"kind": "stroke", "t0_ms": 2900, "t1_ms": 3200, "points": [[0.5, 0.5], [0.6, 0.55]]},
    {"kind": "shift", "t_ms": 3300, "held": False},
]
with open(os.path.join(root, "marks.json"), "w", encoding="utf-8") as f:
    json.dump({"events": events}, f)
# No strokes, but still a script: the real Shift key is not watched, so a person at the Mac
# cannot change a result.
with open(os.path.join(root, "noop.json"), "w", encoding="utf-8") as f:
    json.dump({"events": []}, f)
# One stroke at 10 s and nothing before it: the words spoken earlier must still be pasted.
late = [
    {"kind": "shift", "t_ms": 9800, "held": True},
    {"kind": "stroke", "t0_ms": 10000, "t1_ms": 10600, "points": [[0.2, 0.2], [0.3, 0.3], [0.4, 0.35]]},
    {"kind": "shift", "t_ms": 10700, "held": False},
]
with open(os.path.join(root, "late-stroke.json"), "w", encoding="utf-8") as f:
    json.dump({"events": late}, f)
# Strokes at 4 s: after the Esc countdown of the undo case, whatever the microphone's open time
# (a stroke that starts or ends inside the pause is dropped, by design).
undo = [
    {"kind": "shift", "t_ms": 4000, "held": True},
    {"kind": "stroke", "t0_ms": 4200, "t1_ms": 4600, "points": [[0.2, 0.2], [0.3, 0.3]]},
    {"kind": "shift", "t_ms": 4700, "held": False},
]
with open(os.path.join(root, "undo.json"), "w", encoding="utf-8") as f:
    json.dump({"events": undo}, f)
PY
}

annot_env() { # <png-dir|missing> [script]   (no script: the no-op one, never the real pointer)
  export E2E_ANNOTATIONS=1 MURMUR_DEBUG_HOTKEY=annotations
  export MURMUR_DEBUG_SCREEN="png-dir:$1"
  export MURMUR_DEBUG_POINTER="${2:-$ANNOT_FIX/noop.json}"
}
annot_unenv() { unset E2E_ANNOTATIONS E2E_AUTO_PASTE MURMUR_DEBUG_HOTKEY MURMUR_DEBUG_SCREEN MURMUR_DEBUG_POINTER MURMUR_DEBUG_COMPOSE_DELAY_MS; }
switch_off() { kill -INFO "$APP_PID"; }
annot_rows() { sqlite3 "$DATA/murmur.db" "SELECT count(*) FROM annotations" 2> /dev/null || echo 0; }
last_annot_row() { # status|keyframes|marks|latency_ms|asr_ms|compose_ms|error|folder
  sqlite3 -separator '|' "$DATA/murmur.db" \
    "SELECT status, keyframes, marks, coalesce(latency_ms,''), coalesce(asr_ms,''), coalesce(compose_ms,''), coalesce(error,''), folder FROM annotations ORDER BY created_at DESC, id DESC LIMIT 1"
}
bundle_count() { ls -d "$DATA"/annotations/*/ 2> /dev/null | wc -l | tr -d ' '; }
newest_bundle() { ls -d "$DATA"/annotations/*/ 2> /dev/null | sort | tail -1 | sed 's#/$##'; }
spool_count() { ls -A "$DATA/spool" 2> /dev/null | wc -l | tr -d ' '; }

# Records the WAV through the annotations hotkey and waits for the bundle to be saved. The
# recording lasts the WAV plus half a second, or the seconds given (the WAV, then silence).
annot_record() { # <wav> [seconds] -> 0 when "annotation saved" appeared
  local length
  length="${2:-$(python3 -c "print($(wav_seconds "$1") + 0.5)")}"
  begin_recording || return 1
  sleep "$length"
  hotkey
  if wait_for_log 'annotation saved' "$RESULT_TIMEOUT"; then
    pass "annotation saved"
    note "stop -> paste: $(plain_log | grep -E 'bundle delivered' | tail -1 | sed -E 's/.*latency_ms=([0-9]+).*/\1/') ms"
    return 0
  fi
  fail "no 'annotation saved' within ${RESULT_TIMEOUT}s"
  failure_lines | sed 's/^/    log: /'
  return 1
}

# Checks the newest bundle: row status, files, summary lines, clipboard, spool.
expect_bundle() { # <label> <status ok|text_only|failed> <pictures yes|no> <words yes|no|failed> [marks regex]
  local label="$1" status="$2" pictures="$3" words="$4" marks="${5:-}"
  local row dir summary
  row="$(last_annot_row)"
  local rstatus rkeyframes rmarks rlatency rasr rcompose rerror rfolder
  IFS='|' read -r rstatus rkeyframes rmarks rlatency rasr rcompose rerror rfolder <<< "$row"
  [ "$rstatus" = "$status" ] && pass "$label: row status $status" || fail "$label: row status is '$rstatus' (want $status)"
  dir="$DATA/annotations/$rfolder"
  [ -d "$dir" ] && pass "$label: bundle folder exists" || fail "$label: bundle folder missing ($rfolder)"
  [ -f "$dir/summary.md" ] && pass "$label: summary.md" || fail "$label: summary.md missing"
  [ -f "$dir/annotation.json" ] && pass "$label: annotation.json" || fail "$label: annotation.json missing"
  [ -d "$dir/candidates" ] && fail "$label: candidates/ was not removed" || pass "$label: candidates/ removed"
  [ -f "$dir/contact-sheet.png" ] && fail "$label: a contact sheet was written" || pass "$label: no contact sheet"
  if [ "$pictures" = yes ]; then
    [ -f "$dir/frames/1.png" ] && pass "$label: frames/1.png" || fail "$label: pictures missing"
    [ "$rkeyframes" -ge 1 ] 2> /dev/null && pass "$label: $rkeyframes picture(s)" || fail "$label: no pictures in the row"
  else
    # A compose that finished late (status text_only) may still have written frames/.
    if [ "$status" = text_only ]; then
      pass "$label: pictures not counted (late compose may leave frames/)"
    else
      [ -d "$dir/frames" ] && fail "$label: pictures exist for a bundle without ink" || pass "$label: no pictures, as expected"
    fi
    [ "$rkeyframes" = 0 ] && pass "$label: 0 pictures in the row" || fail "$label: the row counts $rkeyframes picture(s)"
  fi
  summary="$(cat "$dir/summary.md")"
  case "$summary" in *"[Murmur Annotations]"*|*"読み方"*|*"画像: "*) fail "$label: the summary still has a header" ;; *) pass "$label: no header in the summary" ;; esac
  local words_only
  words_only="$(printf '%s' "$summary" | sed -E 's/\[(画像|image) [0-9]+[^]]*\]//g; s/\(文字起こしに失敗しました\)//g' | tr -d '[:space:]')"
  case "$pictures" in no) case "$summary" in *"[画像"*|*"[image"*) fail "$label: a mark in the text of a bundle without pictures" ;; *) pass "$label: no mark in the text" ;; esac ;; esac
  case "$words" in
    yes) case "$summary" in *公園*) pass "$label: the words are in the summary" ;; *) fail "$label: expected word missing from the summary" ;; esac ;;
    any) [ -n "$words_only" ] && pass "$label: words in the summary" || fail "$label: no words in the summary" ;;
    no) [ -z "$words_only" ] && pass "$label: no words, as expected" || fail "$label: words in a summary that should have none" ;;
    failed) case "$summary" in *"文字起こしに失敗"*) pass "$label: the failure note is in the summary" ;; *) fail "$label: failure note missing" ;; esac ;;
  esac
  python3 - "$dir/annotation.json" "$pictures" <<'PY' && pass "$label: annotation.json is version 2" || fail "$label: annotation.json is not the version 2 shape"
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
ok = d.get("version") == 2 and "pictures" in d and "marks" in d and "chunks" in d and "target_bundle_id" in d and "speech" in d
ok = ok and all(set(s) == {"start_ms", "end_ms"} for s in d["speech"])
ok = ok and all(set(p) == {"n", "t_ms", "file"} for p in d["pictures"])
ok = ok and all(set(m) == {"picture", "label", "t0_ms", "t1_ms", "strokes"} for m in d["marks"])
ok = ok and (len(d["pictures"]) >= 1) == (sys.argv[2] == "yes")
sys.exit(0 if ok else 1)
PY
  if [ -n "$marks" ]; then
    python3 - "$dir/summary.md" "$marks" <<'PY' && pass "$label: marks $marks in the summary" || fail "$label: marks $marks missing from the summary"
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
sys.exit(0 if all(re.search(p, text) for p in sys.argv[2].split(",")) else 1)
PY
    [ "$rmarks" -ge 1 ] 2> /dev/null && pass "$label: $rmarks mark(s) in the row" || fail "$label: no marks in the row"
  fi
  # The paste target is the app in front at the hotkey, a terminal when the script runs in one in front. A terminal
  # gets the picture paths inside the marks; any other app gets the summary as it is.
  local target
  target="$(python3 - "$dir/annotation.json" "$ROOT/crates/murmur-core/src/annotations.rs" <<'PY'
import json, re, sys
bundle = (json.load(open(sys.argv[1], encoding="utf-8")).get("target_bundle_id") or "").lower()
ids = re.search(r"TERMINAL_BUNDLE_IDS: \[&str; \d+\] = \[(.*?)\];", open(sys.argv[2], encoding="utf-8").read(), re.S)
if not ids:
    sys.exit("TERMINAL_BUNDLE_IDS not found in annotations.rs")
print("terminal" if bundle in {i.lower() for i in re.findall(r'"([^"]+)"', ids.group(1))} else "app")
PY
)"
  if [ "$target" = terminal ] && [ "$pictures" = yes ]; then
    case "$(pbpaste)" in *"/frames/"*) pass "$label: picture paths in the text for a terminal" ;; *) fail "$label: no picture path in the text for a terminal" ;; esac
  elif [ -n "$target" ]; then
    [ "$(pbpaste)" = "$summary" ] && pass "$label: clipboard holds the summary" || fail "$label: clipboard does not hold the summary"
    case "$(pbpaste)" in *"/frames/"*) fail "$label: a picture path in the text for a GUI app" ;; *) pass "$label: no picture path in the text" ;; esac
  else
    fail "$label: the paste target could not be read"
  fi
  if [ "$status" = failed ]; then
    [ "$(spool_count)" -ge 1 ] && pass "$label: audio kept for a retry" || fail "$label: audio was deleted after a failure"
  else
    [ "$(spool_count)" = 0 ] && pass "$label: spool empty" || fail "$label: spool not empty"
  fi
  note "row: keyframes=$rkeyframes marks=$rmarks latency=${rlatency}ms asr=${rasr}ms compose=${rcompose}ms error='$rerror'"
}

keep_bundle() { # <name>
  [ -n "${E2E_KEEP_DIR:-}" ] || return 0
  local dir
  dir="$(newest_bundle)"
  [ -d "$dir" ] || return 0
  mkdir -p "$E2E_KEEP_DIR"
  rm -rf "${E2E_KEEP_DIR:?}/$1"
  cp -R "$dir" "$E2E_KEEP_DIR/$1"
  note "kept: $E2E_KEEP_DIR/$1"
}

case_annot() { # <name> <marks|""> <keep name|"">
  say "== $1"
  make_annotation_fixtures
  local script=""
  [ "$2" = marks ] && script="$ANNOT_FIX/marks.json"
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$script"
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; return; }
  if annot_record "$FIXTURES/ja.wav"; then
    if [ -n "$script" ]; then
      expect_bundle "$1" ok yes yes '\[画像 1 ①\]'
    else
      expect_bundle "$1" ok no yes
    fi
    [ -n "$3" ] && keep_bundle "$3"
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
}

case_annot_nospeech() { # <name> <marks|""> ; silence: with marks -> a bundle without words, without -> nothing kept
  say "== $1"
  make_annotation_fixtures
  local script=""
  [ "$2" = marks ] && script="$ANNOT_FIX/marks.json"
  local before_rows before_bundles
  before_rows="$(annot_rows)"; before_bundles="$(bundle_count)"
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$script"
  start_app ja "$FIXTURES/silence.wav" || { annot_unenv; return; }
  if [ -n "$script" ]; then
    if annot_record "$FIXTURES/silence.wav"; then
      expect_bundle "$1" ok yes no '\[画像 1 ①\]'
    fi
  else
    begin_recording || { stop_app; annot_unenv; return; }
    sleep "$(python3 -c "print($(wav_seconds "$FIXTURES/silence.wav") + 0.5)")"
    hotkey
    sleep "$SILENCE_WAIT"
    [ "$(annot_rows)" = "$before_rows" ] && pass "$1: no row" || fail "$1: a row was written"
    [ "$(bundle_count)" = "$before_bundles" ] && pass "$1: bundle folder removed" || fail "$1: a bundle folder was left"
    [ "$(pbpaste)" = "$SENTINEL" ] && pass "$1: clipboard unchanged" || fail "$1: clipboard changed"
    [ "$(spool_count)" = 0 ] && pass "$1: spool empty" || fail "$1: spool not empty"
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
}

case_annot_dropped() { # <name> <cancel|disable>
  say "== $1"
  make_annotation_fixtures
  local before_rows before_bundles
  before_rows="$(annot_rows)"; before_bundles="$(bundle_count)"
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$ANNOT_FIX/marks.json"
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; return; }
  begin_recording || { stop_app; annot_unenv; return; }
  sleep 1.5
  if [ "$2" = cancel ]; then
    esc
    sleep 4.5 # the 3 s countdown ends and everything is dropped
  else
    switch_off
    wait_for_log 'annotations switch flipped' 10 || fail "$1: the switch did not flip"
    wait_for_log 'drawing layer destroyed' 5 && pass "$1: drawing layer destroyed" || fail "$1: drawing layer not destroyed"
    sleep 2
  fi
  [ "$(annot_rows)" = "$before_rows" ] && pass "$1: no row" || fail "$1: a row was written"
  [ "$(bundle_count)" = "$before_bundles" ] && pass "$1: bundle folder removed" || fail "$1: a bundle folder was left"
  [ "$(pbpaste)" = "$SENTINEL" ] && pass "$1: clipboard unchanged" || fail "$1: clipboard changed"
  [ "$(spool_count)" = 0 ] && pass "$1: spool empty" || fail "$1: spool not empty"
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
}

case_annot_undo() {
  say "== annot-undo (Esc, Esc, then stop)"
  make_annotation_fixtures
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$ANNOT_FIX/undo.json"
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; return; }
  begin_recording || { stop_app; annot_unenv; return; }
  sleep 1
  esc
  sleep 1
  esc # undo: the recording continues
  sleep "$(python3 -c "print($(wav_seconds "$FIXTURES/ja.wav") + 0.5)")"
  hotkey
  if wait_for_log 'annotation saved' "$RESULT_TIMEOUT"; then
    pass "annot-undo: annotation saved after the undo"
    # The pause cut a second out of the sentence, so only "some words" can be expected.
    expect_bundle "annot-undo" ok yes any
  else
    fail "annot-undo: no 'annotation saved'"
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
}

case_annot_screenfail() {
  say "== annot-screenfail (no screen: words only)"
  make_annotation_fixtures
  set_sentinel
  annot_env "$SCRATCH/no-such-folder" "$ANNOT_FIX/marks.json"
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; return; }
  if annot_record "$FIXTURES/ja.wav"; then
    grep -q 'screen capture did not start' "$LOG" && pass "annot-screenfail: screen failure logged" || fail "annot-screenfail: no screen failure in the log"
    # Without a screen the Shift and stroke feed never starts, so nothing is drawn: words only.
    expect_bundle "annot-screenfail" ok no yes
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
}

case_annot_compose_timeout() {
  say "== annot-compose-timeout (compose delayed past 5 s: words only)"
  make_annotation_fixtures
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$ANNOT_FIX/marks.json"
  export MURMUR_DEBUG_COMPOSE_DELAY_MS=6000
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; return; }
  if annot_record "$FIXTURES/ja.wav"; then
    expect_bundle "annot-compose-timeout" text_only no yes
    sleep 2
    grep -q 'compose finished late' "$LOG" && pass "annot-compose-timeout: late compose dropped" || note "late compose result not seen yet"
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
}

case_annot_nomodel() {
  say "== annot-nomodel (VAD only: the words fail, the pictures go out, the audio is kept)"
  make_annotation_fixtures
  fresh_data
  remove_recognition_models
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$ANNOT_FIX/marks.json"
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; fresh_data; return; }
  if annot_record "$FIXTURES/ja.wav"; then
    expect_bundle "annot-nomodel" failed yes failed '\[画像 1 ①\]'
    assert_failed_row model_missing "annot-nomodel"
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
  fresh_data
}

# The words of the newest bundle as one string: the summary without its headers and marks (both
# the first and the revised summary shapes), so a word check does not depend on the format.
bundle_words() {
  python3 - "$(newest_bundle)/summary.md" <<'PY'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
keep = []
for line in text.splitlines():
    if re.match(r"^(\[Murmur Annotations\]|画像: |読み方: |#\d+ \(|元の大きさの画像: |Pictures?: |How to read: |Full-size)", line):
        continue
    keep.append(line)
text = "\n".join(keep)
text = re.sub(r"\[(画像|image) \d+[^\]]*\]", "", text)   # revised marks: [画像 1 ①], [image 1 ①: /path]
text = re.sub(r"[①-⑳◉◌「」]|クリック|click", "", text)     # first-shape marks and quotes
print(text)
PY
}

# Character coverage of the baseline in the candidate (longest common subsequence over the
# characters, spaces and punctuation removed) and whether the first sentence of the baseline is
# in the candidate. Prints "<coverage 0..1> <first yes|no> <baseline chars> <candidate chars>".
word_coverage() { # <baseline text> <candidate text>
  python3 - "$1" "$2" <<'PY'
import re, sys
def norm(t):
    return re.sub(r"[\s。、，．,.!?！？「」]", "", t)
a, b = norm(sys.argv[1]), norm(sys.argv[2])
if not a:
    print("0 no 0", len(b)); sys.exit(0)
prev = [0] * (len(b) + 1)
for ca in a:
    cur = [0]
    for j, cb in enumerate(b):
        cur.append(prev[j] + 1 if ca == cb else max(prev[j + 1], cur[j]))
    prev = cur
first = re.split(r"[。！？!?]", sys.argv[1].strip(), 1)[0]
first = norm(first)
print(f"{prev[-1] / len(a):.3f}", "yes" if first and first in b else "no", len(a), len(b))
PY
}

# No spoken word may be lost on the annotations path. Two recordings. (1) ja.wav, then
# silence, the only stroke at 10 s: the words all come before the first picture. (2) ja60.wav
# through dictation and through annotations with the stroke at 10 s: the annotation words must
# cover the dictation words (character coverage, LCS) and start with the same first sentence.
case_annot_words() {
  say "== annot-words (words before the first stroke; ja60 against dictation)"
  make_annotation_fixtures
  local summary
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$ANNOT_FIX/late-stroke.json"
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; return; }
  if annot_record "$FIXTURES/ja.wav" 12; then
    summary="$(bundle_words)"
    local w
    for w in 公園 コーヒー; do
      case "$summary" in *"$w"*) pass "annot-words: '$w' (spoken before the stroke) is in the summary" ;; *) fail "annot-words: '$w' (spoken before the stroke) missing from the summary" ;; esac
    done
    note "row: $(last_annot_row | cut -d'|' -f1-3)"
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
  if [ ! -f "$FIXTURES/ja60.wav" ]; then
    note "no ja60.wav: the dictation comparison is skipped"
    return
  fi
  local length baseline
  length="$(python3 -c "print($(wav_seconds "$FIXTURES/ja60.wav") + 0.5)")"
  set_sentinel
  start_app ja "$FIXTURES/ja60.wav" || return
  begin_recording || { stop_app; return; }
  sleep "$length"
  hotkey
  if wait_for_log 'session saved' "$RESULT_TIMEOUT"; then
    baseline="$(pbpaste)"
    pass "annot-words: dictation baseline (${#baseline} characters)"
  else
    fail "annot-words: no dictation baseline"
    stop_app
    return
  fi
  stop_app
  set_sentinel
  annot_env "$ANNOT_FIX/frames" "$ANNOT_FIX/late-stroke.json"
  start_app ja "$FIXTURES/ja60.wav" || { annot_unenv; return; }
  if annot_record "$FIXTURES/ja60.wav" "$length"; then
    summary="$(bundle_words)"
    local coverage first na nb
    read -r coverage first na nb <<< "$(word_coverage "$baseline" "$summary")"
    note "coverage of the dictation words: $coverage ($na -> $nb characters), first sentence: $first"
    python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) >= 0.9 else 1)" "$coverage" \
      && pass "annot-words: annotation words cover the dictation words ($coverage)" \
      || fail "annot-words: words lost on the annotations path (coverage $coverage)"
    [ "$first" = yes ] && pass "annot-words: the first sentence is in the summary" || fail "annot-words: the first sentence is missing from the summary"
    note "row: $(last_annot_row | cut -d'|' -f1-3)"
  fi
  assert_log_private 公園 コーヒー
  no_panic
  stop_app
  annot_unenv
}

# The paste goes to the app that was in front when the recording started, not to the app in
# front at stop. TextEdit is in front at the hotkey; two seconds into the recording Finder comes
# to the front (Cmd+V does nothing there); after the stop the TextEdit document must hold the
# words. Pastes for real (auto_paste on), so a person must be at the Mac and nothing else may
# use the keyboard for about 15 s. The document is closed without saving.
case_annot_target() {
  say "== annot-target (TextEdit in front at start, Finder at stop)"
  make_annotation_fixtures
  set_sentinel
  export E2E_AUTO_PASTE=1
  annot_env "$ANNOT_FIX/frames" "$ANNOT_FIX/marks.json"
  start_app ja "$FIXTURES/ja.wav" || { annot_unenv; return; }
  osascript -e 'tell application "TextEdit" to activate' -e 'tell application "TextEdit" to make new document' > /dev/null
  sleep 1.5
  local front
  front="$(front_bundle_id)"
  if [ "$front" != com.apple.TextEdit ]; then
    # Somebody is using the Mac: recording now would paste into their app. Nothing is started.
    fail "annot-target: '$front' is in front at the hotkey (want com.apple.TextEdit): not recorded"
    osascript -e 'tell application "TextEdit" to close front document saving no' > /dev/null 2>&1
    stop_app
    annot_unenv
    return
  fi
  pass "annot-target: TextEdit is in front at the hotkey"
  if begin_recording; then
    sleep 2
    osascript -e 'tell application "Finder" to activate' > /dev/null
    sleep 4.5
    hotkey
    if wait_for_log 'annotation saved' "$RESULT_TIMEOUT"; then
      pass "annot-target: annotation saved"
      sleep 1
      local doc target
      doc="$(osascript -e 'tell application "TextEdit" to get text of front document' 2> /dev/null)"
      target="$(plain_log | grep -E 'bundle delivered' | tail -1 | sed -E 's/.*target=([A-Za-z]+).*/\1/')"
      note "paste target decided: $target; TextEdit document length: ${#doc}"
      case "$doc" in *公園*) pass "annot-target: the words landed in the TextEdit document" ;; *) fail "annot-target: the TextEdit document does not hold the words" ;; esac
      case "$(pbpaste)" in *公園*) pass "annot-target: clipboard holds the words" ;; *) fail "annot-target: clipboard does not hold the words" ;; esac
    else
      fail "annot-target: no 'annotation saved' within ${RESULT_TIMEOUT}s"
      failure_lines | sed 's/^/    log: /'
    fi
  fi
  osascript -e 'tell application "TextEdit" to close front document saving no' > /dev/null 2>&1
  assert_log_private 公園 コーヒー TextEdit Finder
  no_panic
  stop_app
  annot_unenv
}

# The weights of a loaded model stay in the model file's mapped pages (speech.cpp), so they do not count in the app's
# footprint, which is what Activity Monitor shows as "Memory".
case_memory() {
  say "== memory (footprint with the model loaded, after a dictation)"
  fresh_data
  local wav="$FIXTURES/ja.wav" length mb
  length="$(wav_seconds "$wav")"
  start_app ja "$wav" || return
  begin_recording || { stop_app; return; }
  sleep "$(python3 -c "print($length + 0.5)")"
  hotkey
  if ! wait_for_log 'session saved' "$RESULT_TIMEOUT"; then
    fail "no 'session saved' within ${RESULT_TIMEOUT}s"
    failure_lines | sed 's/^/    log: /'
    stop_app
    return
  fi
  if grep -q 'speech model loaded' "$LOG"; then
    pass "the model is loaded"
  else
    fail "the model was not loaded"
  fi
  mb="$(footprint "$APP_PID" 2> /dev/null | python3 -c '
import re, sys
m = re.search(r"Footprint: ([0-9.]+) (KB|MB|GB)", sys.stdin.read())
if m:
    print(round(float(m.group(1)) * {"KB": 1 / 1024, "MB": 1, "GB": 1024}[m.group(2)]))
')"
  if [ -z "$mb" ]; then
    fail "footprint could not read the app's memory"
  elif [ "$mb" -le "$MEMORY_LIMIT_MB" ]; then
    pass "footprint ${mb} MB, at most ${MEMORY_LIMIT_MB} MB"
  else
    fail "footprint ${mb} MB, over ${MEMORY_LIMIT_MB} MB"
  fi
  no_panic
  stop_app
}

# "Unload the model after N minutes" counts from the model's last use. A dictation that falls
# between two of the once-a-minute idle checks must restart the count too.
case_idle() {
  say "== idle (unload 2 idle minutes after the last use; about 6 minutes)"
  fresh_data
  local wav="$FIXTURES/ja.wav" length unload='unloading the model after idle time'
  length="$(wav_seconds "$wav")"
  E2E_UNLOAD_MIN=2 start_app ja "$wav" || return
  if wait_for_log "$unload" 240; then
    pass "the preloaded model is unloaded when idle"
  else
    fail "no idle unload within 240 s of the start"
    stop_app
    return
  fi
  # Speak right after an idle check, so that the whole dictation falls before the next one.
  sleep 3
  begin_recording || { stop_app; return; }
  sleep "$(python3 -c "print($length + 0.5)")"
  hotkey
  if ! wait_for_log 'session saved' "$RESULT_TIMEOUT"; then
    fail "no 'session saved' within ${RESULT_TIMEOUT}s"
    failure_lines | sed 's/^/    log: /'
    stop_app
    return
  fi
  if [ "$(grep -c 'speech model loaded' "$LOG")" -ge 2 ]; then
    pass "the dictation loaded the model again"
  else
    fail "the dictation did not load the model again"
  fi
  if ! wait_for_log "$unload" 240 2; then
    fail "no second idle unload within 240 s of the dictation"
    stop_app
    return
  fi
  local saved_ms unloaded_ms gap_s
  saved_ms="$(log_time_ms 'session saved')"
  unloaded_ms="$(log_time_ms "$unload")"
  gap_s="$(python3 -c "print(round(($unloaded_ms - $saved_ms) / 1000))")"
  note "dictation saved -> model unloaded: ${gap_s} s (limit 2 min, idle checks once a minute)"
  if [ "$gap_s" -ge 115 ] && [ "$gap_s" -le 185 ]; then
    pass "the model stayed loaded for the 2 idle minutes after the dictation"
  else
    fail "the model was unloaded ${gap_s} s after the dictation, expected 120 to 180 s"
  fi
  no_panic
  stop_app
}

run_annotations() {
  for c in annot annot-marks annot-nospeech-marks annot-nospeech-nomarks annot-cancel annot-undo annot-disable \
    annot-screenfail annot-compose-timeout annot-nomodel annot-words; do
    run_case "$c"
  done
}

run_case() {
  case "$1" in
    annotations) run_annotations ;;
    annot) case_annot "annot (no ink: words only)" "" normal ;;
    annot-marks) case_annot "annot-marks (one ink group)" marks marks ;;
    annot-nospeech-marks) case_annot_nospeech "annot-nospeech-marks" marks ;;
    annot-nospeech-nomarks) case_annot_nospeech "annot-nospeech-nomarks" "" ;;
    annot-cancel) case_annot_dropped "annot-cancel (Esc, countdown)" cancel ;;
    annot-disable) case_annot_dropped "annot-disable (switch off while recording)" disable ;;
    annot-undo) case_annot_undo ;;
    annot-screenfail) case_annot_screenfail ;;
    annot-compose-timeout) case_annot_compose_timeout ;;
    annot-nomodel) case_annot_nomodel ;;
    annot-words) case_annot_words ;;
    annot-target) case_annot_target ;;
    ja) case_dictation "ja" ja ja 公園 コーヒー ;;
    en) case_dictation "en" en en settings hotkey ;;
    silence) case_silence ;;
    cancel) case_cancel ;;
    undo) case_undo ;;
    crash) case_crash ;;
    long) case_long ;;
    mic) case_mic ;;
    nomodel) case_nomodel ;;
    corrupt) case_corrupt ;;
    mash) case_mash ;;
    esc-mash) case_escmash ;;
    readonly) case_readonly ;;
    tenmin) case_tenmin ;;
    idle) case_idle ;;
    memory) case_memory ;;
    *) die "unknown case: $1 (ja en silence cancel undo crash long mic nomodel corrupt mash esc-mash readonly tenmin idle memory annotations annot ...)" ;;
  esac
}

if [ "$#" -gt 0 ]; then
  CASES=("$@")
else
  CASES=(ja en silence cancel undo crash long nomodel corrupt mash esc-mash readonly memory)
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
