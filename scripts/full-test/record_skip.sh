#!/usr/bin/env bash
# Append a skipped/rate_limited JSON record to the staging file. Used by
# the orchestrator when a phase can't run for a deterministic reason
# (missing toolchain, prereq fail, rate-limit flag) so the phase still
# appears in the final report instead of vanishing.
#
# Usage: record_skip.sh <phase-name> <status> <reason...>
#   status must be one of: skipped, rate_limited
#
# wall_s is 0 and exit is 0 -- the phase didn't run.

set -u

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <phase-name> <status> <reason...>" >&2
  exit 64
fi

phase="$1"
status="$2"
shift 2
reason="$*"

case "$status" in
  skipped|rate_limited) ;;
  *) echo "record_skip: status must be skipped or rate_limited (got: $status)" >&2; exit 64 ;;
esac

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
mkdir -p "$runs_dir"
current="$runs_dir/.current.jsonl"
lockfile="$runs_dir/.lock"

rec_json=$(python3 - "$phase" "$status" "$reason" <<'PY'
import json, sys
phase, status, reason = sys.argv[1:4]
print(json.dumps({
    "phase": phase,
    "wall_s": 0.0,
    "exit": 0,
    "status": status,
    "reason": reason,
}))
PY
)

(
  flock 9
  echo "$rec_json" >> "$current"
) 9>"$lockfile"

printf '[%s] %s recorded (%s)\n' "$status" "$phase" "$reason" >&2
