pub mod endpoints;
pub mod error;
pub mod session;
pub mod srp;
pub mod twofa;

use std::path::Path;

use anyhow::Result;
use uuid::Uuid;

use self::endpoints::Endpoints;
use self::error::AuthError;
use self::session::Session;

/// Result of a successful authentication, including the account data payload.
pub struct AuthResult {
    pub session: Session,
    pub data: serde_json::Value,
}

impl std::fmt::Debug for AuthResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthResult")
            .field("session", &"<redacted>")
            .field("data", &"<...>")
            .finish()
    }
}

/// Top-level authentication orchestrator.
///
/// 1. Tries to validate the existing session token.
/// 2. If invalid, obtains a password and performs SRP authentication.
/// 3. Authenticates with the resulting token.
/// 4. Checks if 2FA is required; if so, prompts the user.
/// 5. Returns the authenticated session and account data.
pub async fn authenticate(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &dyn Fn() -> Option<String>,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
) -> Result<AuthResult> {
    let endpoints = Endpoints::for_domain(domain)?;

    let mut session = Session::new(cookie_dir, apple_id, endpoints.home, timeout_secs).await?;

    // Resolve client_id: prefer session data, then provided, then generate new
    let client_id = session
        .client_id()
        .cloned()
        .or(client_id)
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.set_client_id(&client_id);

    // Step 1: Try to validate existing token
    let mut data: Option<serde_json::Value> = None;
    if session.session_data.contains_key("session_token") {
        tracing::debug!("Checking session token validity");
        match twofa::validate_token(&mut session, &endpoints).await {
            Ok(d) => {
                tracing::debug!("Existing session token is valid");
                data = Some(d);
            }
            Err(e) => {
                tracing::debug!("Invalid authentication token, will log in from scratch: {}", e);
            }
        }
    }

    // Step 2: If no valid token, do SRP auth
    if data.is_none() {
        let password = password_provider()
            .ok_or_else(|| AuthError::FailedLogin("Password provider returned no data".into()))?;

        tracing::debug!("Authenticating as {}", apple_id);

        srp::authenticate_srp(
            &mut session,
            &endpoints,
            apple_id,
            &password,
            &client_id,
            domain,
        )
        .await?;

        // Step 3: Authenticate with the session token obtained from SRP
        let account_data = twofa::authenticate_with_token(&mut session, &endpoints).await?;
        data = Some(account_data);
    }

    let data = data.ok_or_else(|| anyhow::anyhow!("Authentication produced no account data"))?;

    // Step 4: Check if 2FA is required
    let requires_2fa = check_requires_2fa(&data);
    if requires_2fa {
        tracing::info!("Two-factor authentication is required");

        let verified = twofa::request_2fa_code(&mut session, &endpoints, &client_id, domain).await?;
        if !verified {
            return Err(AuthError::TwoFactorFailed("2FA verification failed".into()).into());
        }

        // Trust the session
        twofa::trust_session(&mut session, &endpoints, &client_id, domain).await?;

        // Re-authenticate with token after 2FA
        let account_data = twofa::authenticate_with_token(&mut session, &endpoints).await?;

        tracing::info!("Authentication completed successfully");
        return Ok(AuthResult {
            session,
            data: account_data,
        });
    }

    tracing::info!("Authentication completed successfully");
    Ok(AuthResult { session, data })
}

/// Check if 2FA is required based on the account data response.
fn check_requires_2fa(data: &serde_json::Value) -> bool {
    let hsa_version = data
        .pointer("/dsInfo/hsaVersion")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let hsa_challenge_required = data
        .get("hsaChallengeRequired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let hsa_trusted_browser = data
        .get("hsaTrustedBrowser")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let has_qualifying_device = data
        .pointer("/dsInfo/hasICloudQualifyingDevice")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // requires_2fa: hsaVersion == 2, challenge required or not trusted, and has qualifying device
    hsa_version == 2
        && (hsa_challenge_required || !hsa_trusted_browser)
        && has_qualifying_device
}
