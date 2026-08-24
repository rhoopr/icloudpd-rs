#!/usr/bin/env bash
# Convert the full-test staging JSONL into a finalized run record with
# branch/head/rustc metadata
# and run-level metrics from collect_metrics.py.
#
# Print the path of the finalized record on stdout.

set -euo pipefail

runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
current="$runs_dir/.current.jsonl"
start_file="$runs_dir/.run-started-at"
start_head_file="$runs_dir/.run-start-head"
script_dir="$(cd "$(dirname "$0")" && pwd)"

if [[ ! -s "$current" ]]; then
    echo "no phases recorded in $current" >&2
    exit 1
fi

ts=$(date +%Y%m%dT%H%M%S)
out="$runs_dir/$ts.json"

branch=$(git branch --show-current 2>/dev/null || echo "(detached)")
if [[ ! -s "$start_head_file" ]]; then
    echo "missing full-test start head: $start_head_file" >&2
    exit 1
fi
head=$(head -n 1 "$start_head_file")
end_head=$(git rev-parse HEAD 2>/dev/null || echo "(no rev)")
rustc=$(rustc -V 2>/dev/null || echo "(no rustc)")
if [[ -s "$start_file" ]]; then
    started_at=$(head -n 1 "$start_file")
else
    started_at=$(date +%Y-%m-%dT%H:%M:%S)
fi

# Run-level metrics. Failures here don't fail the finalize step -- prefer
# a partial record over a missing one.
metrics_json=$("$script_dir/collect_metrics.py" 2>/dev/null || echo "{}")

python3 - "$current" "$out" "$branch" "$head" "$end_head" "$rustc" "$started_at" "$metrics_json" <<'PY'
import json, sys
src, dst, branch, head, end_head, rustc, started_at, metrics_json = sys.argv[1:9]
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
    "end_head": end_head,
    "rustc": rustc,
    "phases": phases,
    "metrics": json.loads(metrics_json or "{}"),
}
with open(dst, "w") as f:
    json.dump(record, f, indent=2, sort_keys=True)
PY

# Clear staging + run marker (lets the next /full-test start cleanly).
rm -f "$current" "$runs_dir/.run-marker" "$start_file" "$start_head_file"
echo "$out"
