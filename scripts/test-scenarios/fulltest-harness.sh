#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test test:branch_static full_test
run_scenario_test test:branch_static scenario_fulltest_harness_rejects_unreferenced_helpers
run_scenario_test test:branch_static scenario_runner_rejects_filters_that_match_no_tests
