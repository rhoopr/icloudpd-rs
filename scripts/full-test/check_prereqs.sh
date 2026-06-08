#!/usr/bin/env bash
# Verify prerequisites for the live test phases. Emits structured pass/skip
# lines on stdout. On any failure, writes the reasons to
# the full-test run state dir so downstream live phases self-skip.
#
# Output format (one line per check):
#   <check>: pass
#   <check>: skip <reason>
#
# Exit code is always 0 -- callers consult the flag file (not the exit
# code), which makes this safe under `set -e` orchestrators.
#
# Checks:
#   env      .env present at the repo root (lib.sh + cargo tests need it).
#   cookies  Every entry in .test-cookies/ is owned by the current user.
#            Docker mounts can chown the cookie dir to root; per
#            feedback_no_sudo.md we don't auto-fix -- we surface it and skip.
#   tools    Reports optional local script/full-test helpers. Missing optional
#            tools do not skip live phases, but the report makes local gate
#            trust gaps visible before a long run starts.

set -u

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
mkdir -p "$runs_dir"
flag="$runs_dir/.live-skipped"

reasons=()

report_tool() {
  local cmd="$1"
  local label="${2:-$1}"
  local optional="${3:-0}"
  if command -v "$cmd" >/dev/null 2>&1; then
    echo "$label: pass"
  elif [[ "$optional" == "1" ]]; then
    echo "$label: optional-missing $cmd not found"
  else
    local reason="$cmd not found"
    echo "$label: skip $reason"
    reasons+=("$label: $reason")
  fi
}

# --- .env present ---------------------------------------------------------
env_path="$repo_root/.env"
if [[ -f "$env_path" ]]; then
  echo "env: pass"
else
  reason=".env missing at $env_path"
  echo "env: skip $reason"
  reasons+=("env: $reason")
fi

# --- cookies owned by current user ---------------------------------------
cookies_dir="$repo_root/.test-cookies"
me=$(id -un)
if [[ ! -d "$cookies_dir" ]]; then
  reason="$cookies_dir does not exist (run live tests once to create it)"
  echo "cookies: skip $reason"
  reasons+=("cookies: $reason")
else
  bad=$(find "$cookies_dir" -mindepth 1 -maxdepth 1 -exec stat -c '%U %n' {} \; 2>/dev/null \
        | awk -v me="$me" '$1 != me { print $0 }')
  if [[ -z "$bad" ]]; then
    echo "cookies: pass"
  else
    reason="non-$me-owned files in $cookies_dir (Docker chown? run 'chown -R $me $cookies_dir' as root to fix)"
    echo "cookies: skip $reason"
    {
      echo "  offending entries:"
      printf '%s\n' "$bad" | sed 's/^/    /'
    } >&2
    reasons+=("cookies: $reason")
  fi
fi

# --- local tooling visibility --------------------------------------------
report_tool shellcheck shellcheck 1
report_tool shfmt shfmt 1
report_tool ruff ruff 1
report_tool actionlint actionlint 1
report_tool cargo-bloat cargo-bloat 1

# --- summary + flag -------------------------------------------------------
if [[ ${#reasons[@]} -gt 0 ]]; then
  printf '%s\n' "${reasons[@]}" > "$flag"
  echo
  echo "live-phases: SKIP (${#reasons[@]} prereq failure(s)); flag=$flag" >&2
else
  rm -f "$flag"
fi
