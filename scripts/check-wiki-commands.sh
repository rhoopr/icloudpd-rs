#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <kei-binary> <wiki-directory>" >&2
    exit 2
fi

binary="$1"
home="$2/Home.md"

if [[ ! -x "$binary" ]]; then
    echo "wiki command check: binary is not executable: $binary" >&2
    exit 2
fi
if [[ ! -f "$home" ]]; then
    echo "wiki command check: Home.md not found under $2" >&2
    exit 2
fi

help_output=$("$binary" --help)
mapfile -t cli_commands < <(
    sed -n '/^Commands:$/,/^Options:$/s/^  \([a-z][a-z-]*\)  .*/\1/p' <<<"$help_output" |
        grep -v '^help$'
)
mapfile -t wiki_commands < <(
    awk '
        $0 == "## Commands" { in_commands = 1; next }
        in_commands && /^## / { exit }
        in_commands && /^\| \[`/ { split($0, parts, "`"); print parts[2] }
    ' "$home"
)

if ! diff -u \
    <(printf '%s\n' "${cli_commands[@]}") \
    <(printf '%s\n' "${wiki_commands[@]}"); then
    echo "wiki command check: Home.md command table differs from 'kei --help'" >&2
    exit 1
fi

checked=0
while IFS= read -r example; do
    # Home's canonical examples intentionally use simple argv with no shell
    # quoting or pipelines. Add explicit argv fixtures here if that changes.
    read -r -a args <<<"$example"
    if ! "$binary" "${args[@]}" --help >/dev/null 2>&1; then
        echo "wiki command check: invalid Home.md example: kei $example" >&2
        exit 1
    fi
    checked=$((checked + 1))
done < <(
    awk '
        /^```(sh|bash)$/ { in_shell = 1; next }
        /^```$/ { in_shell = 0; next }
        in_shell && /^kei / { sub(/^kei /, ""); print }
    ' "$home"
)

if [[ $checked -eq 0 ]]; then
    echo "wiki command check: Home.md has no command examples" >&2
    exit 1
fi

echo "wiki command check passed: ${#cli_commands[@]} commands, $checked examples"
