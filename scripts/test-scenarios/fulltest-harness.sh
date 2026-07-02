#!/usr/bin/env bash
set -euo pipefail

cargo test --test branch_static full_test
cargo test --test branch_static scenario_fulltest_harness_rejects_unreferenced_helpers
