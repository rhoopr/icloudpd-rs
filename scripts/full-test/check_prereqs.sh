#!/usr/bin/env bash
# Verify prerequisites for the live test phases. Emits structured pass/skip
# lines on stdout. On any failure, writes the reasons to
# .scratch/test-runs/.live-skipped so downstream live phases self-skip.
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

set -u

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
runs_dir="$repo_root/.scratch/test-runs"
mkdir -p "$runs_dir"
flag="$runs_dir/.live-skipped"

reasons=()

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

# --- summary + flag -------------------------------------------------------
if [[ ${#reasons[@]} -gt 0 ]]; then
  printf '%s\n' "${reasons[@]}" > "$flag"
  echo
  echo "live-phases: SKIP (${#reasons[@]} prereq failure(s)); flag=$flag" >&2
else
  rm -f "$flag"
fi
