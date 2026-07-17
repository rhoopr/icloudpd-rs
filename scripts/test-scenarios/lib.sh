#!/usr/bin/env bash

run_scenario_test() {
  local target="$1"
  local filter="$2"
  local cargo_bin="${CARGO:-cargo}"
  local -a target_args

  case "$target" in
    lib)
      target_args=(--lib)
      ;;
    test:*)
      target_args=(--test "${target#test:}")
      ;;
    *)
      echo "scenario runner: unsupported target '$target'" >&2
      return 2
      ;;
  esac

  local listed
  if ! listed=$("$cargo_bin" test "${target_args[@]}" "$filter" -- --list); then
    echo "scenario runner: could not list target=$target filter=$filter" >&2
    return 1
  fi
  if ! grep -q ': test$' <<<"$listed"; then
    echo "scenario runner: no tests matched target=$target filter=$filter" >&2
    return 2
  fi

  "$cargo_bin" test "${target_args[@]}" "$filter"
}
