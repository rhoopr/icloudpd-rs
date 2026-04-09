//! Pre-authentication fixture for integration tests.
//!
//! Establishes an iCloud session so auth-requiring tests can run without
//! interactive 2FA. Requires `ICLOUD_USERNAME` and `ICLOUD_PASSWORD` in `.env`.
//!
//! Setup (interactive -- prompts for 2FA code):
//!
//! ```sh
//! env (cat .env | grep -v '^#') cargo run -- login --data-dir .test-cookies
//! ```
//!
//! Verify an existing session is still valid:
//!
//! ```sh
//! cargo test --test setup_auth -- --ignored
//! ```

mod common;

/// Verify that a pre-authenticated session exists and is usable.
///
/// Calls `require_preauth()` which skips SRP when the session file is
/// fresh (< 48 hours). When the session is stale, performs a full login
/// to refresh it.
#[test]
#[ignore]
fn verify_preauth_session() {
    let (_username, _password, _cookie_dir) = common::require_preauth();
    eprintln!("Pre-auth session is valid.");
}
