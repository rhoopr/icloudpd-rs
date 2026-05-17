#!/usr/bin/env bash
# Convert .scratch/test-runs/.current.jsonl into a finalized run record at
# .scratch/test-runs/<ISO-timestamp>.json with branch/head/rustc metadata
# and run-level metrics from collect_metrics.py.
#
# Print the path of the finalized record on stdout.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
runs_dir="$repo_root/.scratch/test-runs"
current="$runs_dir/.current.jsonl"
script_dir="$(cd "$(dirname "$0")" && pwd)"

if [[ ! -s "$current" ]]; then
  echo "no phases recorded in $current" >&2
  exit 1
fi

ts=$(date +%Y%m%dT%H%M%S)
out="$runs_dir/$ts.json"

branch=$(git branch --show-current 2>/dev/null || echo "(detached)")
head=$(git rev-parse --short HEAD 2>/dev/null || echo "(no rev)")
rustc=$(rustc -V 2>/dev/null || echo "(no rustc)")
started_at=$(date +%Y-%m-%dT%H:%M:%S)

# Run-level metrics. Failures here don't fail the finalize step -- prefer
# a partial record over a missing one.
metrics_json=$("$script_dir/collect_metrics.py" 2>/dev/null || echo "{}")

python3 - "$current" "$out" "$branch" "$head" "$rustc" "$started_at" "$metrics_json" <<'PY'
import json, sys
src, dst, branch, head, rustc, started_at, metrics_json = sys.argv[1:8]
phases = {}
with open(src) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        phase = rec.pop("phase")
        phases[phase] = rec
record = {
    "started_at": started_at,
    "branch": branch,
    "head": head,
    "rustc": rustc,
    "phases": phases,
    "metrics": json.loads(metrics_json or "{}"),
}
with open(dst, "w") as f:
    json.dump(record, f, indent=2, sort_keys=True)
PY

# Clear staging + run marker (lets the next /full-test start cleanly).
rm -f "$current" "$runs_dir/.run-marker"
echo "$out"
