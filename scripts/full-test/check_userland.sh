#!/usr/bin/env bash
# Fail early when local full-test helpers run on an unsupported userland.
#
# The full-test harness is Linux-oriented and intentionally uses a few GNU
# command extensions (`find -printf`, `stat -c`, and `timeout`) plus `flock`
# for run-state locking. Check those up front so macOS/BSD hosts get one clear
# error instead of a later confusing phase failure.

set -euo pipefail

errors=()

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    errors+=("$cmd not found")
  fi
}

require_cmd find
require_cmd stat
require_cmd flock
require_cmd timeout

if command -v find >/dev/null 2>&1 && ! find . -maxdepth 0 -printf '' >/dev/null 2>&1; then
  errors+=("find does not support GNU -printf")
fi

if command -v stat >/dev/null 2>&1 && ! stat -c '%Y' . >/dev/null 2>&1; then
  errors+=("stat does not support GNU -c")
fi

if command -v timeout >/dev/null 2>&1 && ! timeout 1 true >/dev/null 2>&1; then
  errors+=("timeout command is present but failed a basic smoke test")
fi

if [[ ${#errors[@]} -gt 0 ]]; then
  {
    echo "full-test: unsupported local userland"
    echo
    echo "This harness expects GNU/Linux userland tools. Missing checks:"
    for error in "${errors[@]}"; do
      echo "  - $error"
    done
    echo
    echo "Run on Linux, or install GNU coreutils/findutils/flock-compatible tools"
    echo "and put them before the BSD/macOS tools in PATH."
  } >&2
  exit 64
fi
