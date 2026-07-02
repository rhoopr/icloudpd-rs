#!/usr/bin/env bash
set -euo pipefail

cargo test --lib session_error_reauth_tries_persisted_session_before_stripping
cargo test --lib clear_validation_cache_for_reauth_preserves_routing_state
cargo test --lib live_validate_success_uses_existing_session_even_with_hsa_flags
cargo test --lib send_2fa_push_treats_fresh_validation_cache_as_authenticated
cargo test --lib send_2fa_push_treats_live_validate_success_as_authenticated
cargo test --lib get_code
cargo test --lib reauth
