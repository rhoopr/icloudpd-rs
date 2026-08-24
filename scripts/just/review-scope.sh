#!/usr/bin/env bash
set -euo pipefail

mode=scope
base_ref=origin/main
if [[ "${1:-}" == "--validation-only" ]]; then
    mode=validation
elif [[ -n "${1:-}" ]]; then
    base_ref=$1
fi
base_ref="${base_ref#BASE=}"

latest_full_test() {
    local runs_dir latest
    runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
    latest=$(find "$runs_dir" -maxdepth 1 -name '*.json' -type f -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n 1 | cut -d' ' -f2- || true)
    printf '%s' "$latest"
}

print_validation_provenance() {
    local current_branch current_head latest record
    current_branch=$(git branch --show-current 2>/dev/null || true)
    current_head=$(git rev-parse HEAD)
    latest=$(latest_full_test)

    echo "validation provenance:"
    echo "current_branch: ${current_branch:-(detached)}"
    echo "current_head: $current_head"
    if [[ -z "$latest" ]]; then
        echo "latest_full_test: (none)"
        echo "validation_status: NONE"
        return
    fi

    record=$(python3 - "$latest" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    data = json.load(handle)
print(data.get("branch", ""))
print(data.get("head", ""))
PY
)
    local record_branch record_head validation_status
    record_branch=$(sed -n '1p' <<<"$record")
    record_head=$(sed -n '2p' <<<"$record")
    if [[ -n "$record_head" && ( "$current_head" == "$record_head"* || "$record_head" == "$current_head"* ) ]]; then
        validation_status=CURRENT
    elif [[ -n "$record_branch" && "$record_branch" == "$current_branch" ]]; then
        validation_status=STALE
    else
        validation_status="OTHER BRANCH"
    fi

    echo "latest_full_test: $latest"
    echo "validation_branch: ${record_branch:-(unknown)}"
    echo "validation_head: ${record_head:-(unknown)}"
    echo "validation_status: $validation_status"
}

if [[ "$mode" == validation ]]; then
    print_validation_provenance
    exit 0
fi

base_sha=$(git rev-parse --verify "${base_ref}^{commit}")
head_sha=$(git rev-parse HEAD)
merge_base=$(git merge-base "$base_sha" "$head_sha")
commit_count=$(git rev-list --count "${merge_base}..${head_sha}")

echo "review target:"
echo "base_ref: $base_ref"
echo "base: $base_sha"
echo "merge_base: $merge_base"
echo "head: $head_sha"
echo "commits: $commit_count"
echo
echo "changed files:"
changed_files=$(git diff --name-status --find-renames "${merge_base}...${head_sha}")
if [[ -n "$changed_files" ]]; then
    printf '%s\n' "$changed_files"
else
    echo "(none)"
fi
echo
echo "changed lines:"
changed_lines=$(git diff --numstat --find-renames "${merge_base}...${head_sha}")
if [[ -n "$changed_lines" ]]; then
    printf '%s\n' "$changed_lines"
else
    echo "(none)"
fi
echo
echo "summary:"
summary=$(git diff --shortstat "${merge_base}...${head_sha}")
if [[ -n "$summary" ]]; then
    printf '%s\n' "$summary"
else
    echo "(none)"
fi
echo
echo "workspace:"
workspace=$(git status --short --untracked-files=all)
if [[ -n "$workspace" ]]; then
    printf '%s\n' "$workspace"
else
    echo "(clean)"
fi
echo
print_validation_provenance
