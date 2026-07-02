#!/usr/bin/env bash
set -euo pipefail

cargo test --lib example_config_documents_supported_options
cargo test --test branch_static migration_guide_uses_toml_for_durable_sync_settings
cargo test --test branch_static contributor_docs_match_current_gate
