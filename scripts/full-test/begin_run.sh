#!/usr/bin/env bash
# Initialize state for a fresh /full-test run.
#
# - If another run is in progress (run-marker fresh, staging file
#   non-empty), refuse to start. Avoids corrupting the staging file.
# - If the marker is stale (> 1h old) or the staging file has entries
#   from a previous, crashed run, clear them.
# - Touch the run-marker so finalize_run.sh can detect a hung session.
#
# Stdout: the run id (timestamp). Phases use it for log paths.
# Exit codes: 0 ok, 65 concurrent run detected.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
mkdir -p "$runs_dir"
current="$runs_dir/.current.jsonl"
marker="$runs_dir/.run-marker"
start_file="$runs_dir/.run-started-at"
rate_flag="$runs_dir/.rate-limited"
skip_flag="$runs_dir/.live-skipped"
lockfile="$runs_dir/.lock"

now=$(date +%s)
run_id=$(date +%Y%m%dT%H%M%S)

(
  flock 9

  # Concurrency check: a fresh marker means another run is in progress, even
  # before the first phase has appended to staging.
  if [[ -f "$marker" ]]; then
    marker_age=$(( now - $(stat -c '%Y' "$marker" 2>/dev/null || echo 0) ))
    if [[ $marker_age -lt 3600 ]]; then
      echo "ERROR: another /full-test run appears to be in progress" >&2
      echo "  marker:  $marker (age ${marker_age}s)" >&2
      if [[ -s "$current" ]]; then
        echo "  staging: $current ($(wc -l <"$current") records)" >&2
      else
        echo "  staging: $current (no records yet)" >&2
      fi
      echo "If this is wrong (previous run crashed), remove the marker:" >&2
      echo "  rm $marker" >&2
      exit 65
    fi
  fi

  # Clear stale staging from a crashed previous run.
  if [[ -s "$current" ]]; then
    echo "begin_run: clearing stale staging file ($(wc -l <"$current") records)" >&2
    : > "$current"
  fi

  # Clear per-run flag files left over from a prior run. These are
  # decision inputs for downstream live phases; a stale flag would
  # silently skip everything.
  rm -f "$rate_flag" "$skip_flag"

  # Establish marker and stable run-start metadata while still holding the
  # lock. finalize_run.sh uses this instead of its own wall clock so long
  # runs record the actual start time, not finalization time.
  date -u +%s > "$marker"
  date +%Y-%m-%dT%H:%M:%S > "$start_file"
) 9>"$lockfile"

echo "$run_id"
