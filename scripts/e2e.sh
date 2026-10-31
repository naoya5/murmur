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
# Usage: scripts/e2e.sh [case ...]     cases: ja en silence cancel undo crash long   (default: all of these)
#        opt-in: mic (makes sound: real microphone + afplay through the speakers), tenmin (about 12 minutes)
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
    "microphone": ("wav:" + wav) if wav else None,  # empty = the system default microphone
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
  note "latency (stop -> clipboard): ${latency} ms, asr: ${asr} ms, audio: ${duration} ms, clipboard: ${#SENTINEL} (sentinel) -> $(pbpaste | wc -c | tr -d ' ') bytes"
  stop_app
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
    mic) case_mic ;;
    nomodel) case_nomodel ;;
    corrupt) case_corrupt ;;
    mash) case_mash ;;
    esc-mash) case_escmash ;;
    readonly) case_readonly ;;
    tenmin) case_tenmin ;;
    *) die "unknown case: $1 (ja en silence cancel undo crash long mic nomodel corrupt mash esc-mash readonly tenmin)" ;;
  esac
}

if [ "$#" -gt 0 ]; then
  CASES=("$@")
else
  CASES=(ja en silence cancel undo crash long nomodel corrupt mash esc-mash readonly)
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
