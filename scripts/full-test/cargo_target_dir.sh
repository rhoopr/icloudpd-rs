#!/usr/bin/env bash
# Print the Cargo target directory for commands that consume build artifacts.

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-target}"

if [[ "$target_dir" != /* ]]; then
  target_dir="$repo_root/$target_dir"
fi

printf '%s\n' "$target_dir"
