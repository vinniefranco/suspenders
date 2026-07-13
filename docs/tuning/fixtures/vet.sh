#!/usr/bin/env bash
# vet.sh RUN_DIR — compact summary of one suspenders headless run directory.
# Reads: stdout.txt, exit_code, duration_secs, diff.patch, cargo_test_full.txt,
# and the session-log *.jsonl (entry codec: src/session/log.rs).
set -u
dir="${1:?usage: vet.sh RUN_DIR}"
dir="${dir%/}"
[ -d "$dir" ] || { echo "not a directory: $dir" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq required" >&2; exit 1; }

stdout="$dir/stdout.txt"
cargo="$dir/cargo_test_full.txt"
jsonl=$(cat "$dir"/*.jsonl 2>/dev/null)

exit_code=$(cat "$dir/exit_code" 2>/dev/null || echo "?")
dur=$(cat "$dir/duration_secs" 2>/dev/null || echo "?")

# --- passes per turn + stop reasons (stdout: "-- pass N", "== turn_finished") ---
turns_line=$(awk '
  /^-- pass /       { p++ }
  /^== turn_finished/ { for (i=1;i<=NF;i++) if ($i ~ /^stop_reason=/) { sub("stop_reason=","",$i); r=$i }
                        n++; printf "%sT%d:%dp/%s", (n>1?" ":""), n, p, r; p=0 }
  /^== TURN ERROR/    { n++; printf "%sT%d:%dp/ERROR", (n>1?" ":""), n, p; p=0 }
  /^== turn_cancelled/{ n++; printf "%sT%d:%dp/cancelled", (n>1?" ":""), n, p; p=0 }
  END { if (n==0) printf "none"; if (p>0) printf " (+%dp unfinished)", p; print "" }
' "$stdout" 2>/dev/null)
n_turns=$(grep -c '^== turn_finished\|^== TURN ERROR\|^== turn_cancelled' "$stdout" 2>/dev/null); n_turns=${n_turns:-0}

# --- settlement + recovery (jsonl: settled / recovery entries) ---
settled=$(jq -r 'select(.e=="settled") | "\(.outcome)/\(.stop_reason)"' <<<"$jsonl" | paste -sd' ' -)
recov=$(jq -r 'select(.e=="recovery") | .shape' <<<"$jsonl" | sort | uniq -c | awk '{printf "%s%s x%d",(NR>1?", ":""),$2,$1} END{if(NR==0) printf "none"}')
handoffs=$(jq -r 'select(.e=="handoff") | 1' <<<"$jsonl" | wc -l)

# --- cargo test result + compile status ---
if [ -f "$cargo" ]; then
  testres=$(grep '^test result:' "$cargo" | sed 's/^test result: //' | paste -sd'|' - | sed 's/|/  ||  /g')
  [ -n "$testres" ] || testres="(no test result line)"
  if grep -q "could not compile\|^error\[E" "$cargo"; then compiled="NO (compile errors)"; else compiled="yes"; fi
else testres="(missing)"; compiled="?"; fi

# --- eviction / supersession traces (marker strings from voice.rs; eviction is
#     request-time only, so nonzero counts here mean the text leaked into logged
#     entries — count both jsonl and stdout for visibility) ---
cnt() { { grep -oF "$1" <<<"$jsonl"; grep -oF "$1" "$stdout" 2>/dev/null; } | wc -l; }
elided=$(cnt '[result elided - re-run the tool if needed]')
sup_cmd=$(cnt '[superseded by a newer run of this command below]')
sup_read=$(cnt '[superseded by a newer read of this file below]')
husked=$(cnt '[edit body elided - the file on disk holds the result]')
anch_elide=$(cnt '[stale anchor elided - a fresher anchor is below]')
compactions=$(grep -c '## COMPACTION' "$stdout" 2>/dev/null); compactions=${compactions:-0}
waves=$(grep -c "## EVICTION wave" "$stdout" 2>/dev/null); waves=${waves:-0}
compacted_entries=$(jq -r 'select(.e=="compacted") | 1' <<<"$jsonl" | wc -l)

# --- riders + stale-plan line (jsonl rider entries; voice.rs stale_plan_line) ---
riders=$(jq -r 'select(.e=="rider") | .tag' <<<"$jsonl" | sort | uniq -c | awk '{printf "%s%s x%d",(NR>1?", ":""),$2,$1} END{if(NR==0) printf "none"}')
stale=$(grep -oF 'this plan has not changed in' <<<"$jsonl" | wc -l)

# --- diff stats ---
if [ -s "$dir/diff.patch" ]; then
  dstats=$(awk '/^diff --git/{f++} /^\+/&&!/^\+\+\+/{i++} /^-/&&!/^---/{d++} END{printf "%d file(s), +%d/-%d", f, i, d}' "$dir/diff.patch")
else dstats="(empty diff)"; fi

echo "== vet: $(basename "$dir")  exit=$exit_code  duration=${dur}s"
echo "turns($n_turns): ${turns_line:-none}"
echo "settled: ${settled:-none}"
echo "recovery: $recov  (handoff entries: $handoffs)"
echo "cargo: compiled=$compiled"
echo "  $testres"
echo "eviction: result-elided=$elided cmd-superseded=$sup_cmd read-superseded=$sup_read edit-husked=$husked anchor-elided=$anch_elide"
echo "compaction: stdout=$compactions jsonl-compacted=$compacted_entries eviction-waves(stdout)=$waves"
echo "riders: $riders  stale-plan-lines: $stale"
echo "diff: $dstats"
