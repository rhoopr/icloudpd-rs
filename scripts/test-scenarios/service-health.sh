#!/usr/bin/env bash
set -euo pipefail

cargo test --lib healthz
cargo test --lib metrics
