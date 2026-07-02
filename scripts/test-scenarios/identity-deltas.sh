#!/usr/bin/env bash
set -euo pipefail

cargo test --lib sibling_cplassets
cargo test --lib sibling_assets
cargo test --lib hard_delete
cargo test --lib selected_relation_add_without_photo
cargo test --lib master_family_soft_delete
