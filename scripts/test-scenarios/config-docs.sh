#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
source "$script_dir/lib.sh"

run_scenario_test lib example_config_documents_supported_options
run_scenario_test test:branch_static migration_guide_uses_toml_for_durable_sync_settings
run_scenario_test test:branch_static contributor_docs_match_current_gate
