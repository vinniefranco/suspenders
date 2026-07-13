#!/usr/bin/env bash
# drive.sh FIXTURE TAG [N] — N headless runs of /tmp/FIXTURE, artifacts in /tmp/fixture-logs/TAG-runN/
# Arm config via env: SUSPENDERS_RECOVERY_LIMIT, SUSPENDERS_RECOVERY_SHAPE, etc.
set -u
FIXTURE=$1; TAG=$2; N=${3:-5}
BIN=/home/vinnie/Projects/suspenders/suspenders/target/release/suspenders
SESS_DIR=${XDG_DATA_HOME:-$HOME/.local/share}/suspenders/sessions
ROOT=/tmp/$FIXTURE
mkdir -p /tmp/fixture-logs "$SESS_DIR"

for i in $(seq 1 "$N"); do
  OUT=/tmp/fixture-logs/$TAG-run$i
  mkdir -p "$OUT"
  git -C "$ROOT" reset --hard -q
  git -C "$ROOT" clean -fdq
  PROMPT=$(cat "$ROOT/PROMPT.txt")
  ls "$SESS_DIR" > "$OUT/.sess_before"

  START=$(date +%s)
  "$BIN" --headless --root "$ROOT" "$PROMPT" > "$OUT/stdout.txt" 2>&1
  echo $? > "$OUT/exit_code"
  echo $(( $(date +%s) - START )) > "$OUT/duration_secs"

  ls "$SESS_DIR" > "$OUT/.sess_after"
  comm -13 "$OUT/.sess_before" "$OUT/.sess_after" | while read -r f; do
    [ -n "$f" ] && cp "$SESS_DIR/$f" "$OUT/"
  done

  # diff including untracked files, then FULL cargo test output (c006 lesson)
  git -C "$ROOT" add -A
  git -C "$ROOT" diff --cached > "$OUT/diff.patch"
  git -C "$ROOT" status --short > "$OUT/git_status.txt"
  (cd "$ROOT" && timeout 180 cargo test 2>&1) > "$OUT/cargo_test_full.txt"

  git -C "$ROOT" reset --hard -q
  git -C "$ROOT" clean -fdq
  echo "== $TAG-run$i done: exit=$(cat "$OUT/exit_code") dur=$(cat "$OUT/duration_secs")s test=$(grep -m1 'test result' "$OUT/cargo_test_full.txt" || echo n/a)"
done
