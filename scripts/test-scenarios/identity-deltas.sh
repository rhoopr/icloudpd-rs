#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib sibling_cplassets
run_scenario_test lib sibling_assets
run_scenario_test lib hard_delete
run_scenario_test lib selected_relation_add_without_photo
run_scenario_test lib master_family_soft_delete
