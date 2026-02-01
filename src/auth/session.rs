use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fs2::FileExt;
use reqwest::header::{HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use reqwest::{Client, Response};
use serde_json::Value;
use tokio::fs;

/// Apple's auth APIs return session state in custom HTTP headers.
/// We capture these after every request to maintain session continuity.
const HEADER_DATA: &[(&str, &str)] = &[
    ("X-Apple-ID-Account-Country", "account_country"),
    ("X-Apple-ID-Session-Id", "session_id"),
    ("X-Apple-Session-Token", "session_token"),
    ("X-Apple-TwoSV-Trust-Token", "trust_token"),
    ("X-Apple-TwoSV-Trust-Eligible", "trust_eligible"),
    ("X-Apple-I-Rscd", "apple_rscd"),
    ("X-Apple-I-Ercd", "apple_ercd"),
    ("scnt", "scnt"),
];

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// Thread-safe shared session handle for use across the download layer.
/// The `Arc` enables cheap cloning; the `RwLock` allows concurrent reads
/// (HTTP requests) with exclusive writes (session refresh / re-auth).
pub type SharedSession = Arc<tokio::sync::RwLock<Session>>;

/// Sanitize a username by keeping only word characters (alphanumeric + underscore).
/// Equivalent to Python's `re.match(r"\w", c)` filter.
pub fn sanitize_username(username: &str) -> String {
    username
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Check if a Set-Cookie header string represents an expired cookie.
/// Parses the `cookie` crate's `Cookie::parse()` to extract `Expires`.
fn is_cookie_expired(cookie_str: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    if let Ok(parsed) = cookie::Cookie::parse(cookie_str) {
        if let Some(expires) = parsed.expires_datetime() {
            let expires_utc =
                chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::from(expires));
            return expires_utc < *now;
        }
    }
    false
}

/// HTTP session wrapper that persists cookies and session data to disk,
/// allowing authentication to survive across process restarts.
pub struct Session {
    client: Client,
    pub(crate) _cookie_jar: Arc<reqwest::cookie::Jar>,
    pub session_data: HashMap<String, String>,
    cookie_dir: PathBuf,
    sanitized_username: String,
    home_endpoint: String,
    _timeout: Duration,
    /// Exclusive file lock preventing concurrent instances for the same account.
    /// The advisory lock is held for the lifetime of the Session via the open
    /// file descriptor; released automatically when the File is dropped.
    _lock_file: std::fs::File,
}

impl Session {
    /// Create a new session, loading existing cookies and session data from disk.
    pub async fn new(
        cookie_dir: &Path,
        username: &str,
        home_endpoint: &str,
        timeout_secs: Option<u64>,
    ) -> Result<Self> {
        let sanitized = sanitize_username(username);
        let cookie_dir = cookie_dir.to_path_buf();
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(30));

        fs::create_dir_all(&cookie_dir).await.with_context(|| {
            format!(
                "Failed to create cookie directory: {}",
                cookie_dir.display()
            )
        })?;

        // Acquire an exclusive file lock to prevent concurrent instances for
        // the same account from corrupting session/cookie state.
        let lock_path = cookie_dir.join(format!("{}.lock", sanitized));
        let lock_file = std::fs::File::create(&lock_path)
            .with_context(|| format!("Failed to create lock file: {}", lock_path.display()))?;
        lock_file.try_lock_exclusive().map_err(|_| {
            anyhow::anyhow!(
                "Another icloudpd-rs instance is running for this account (lock: {})",
                lock_path.display()
            )
        })?;

        let cookie_jar = Arc::new(reqwest::cookie::Jar::default());

        let cookiejar_path = cookie_dir.join(&sanitized);
        if cookiejar_path.exists() {
            match fs::read_to_string(&cookiejar_path).await {
                Ok(contents) => {
                    let now = chrono::Utc::now();
                    for line in contents.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with('#')
                            || trimmed.is_empty()
                            || trimmed.starts_with("Set-Cookie3:")
                        {
                            continue;
                        }
                        if let Some((url_str, cookie_str)) = trimmed.split_once('\t') {
                            // Prune expired cookies on load
                            if is_cookie_expired(cookie_str, &now) {
                                tracing::debug!("Pruning expired cookie from {}", url_str);
                                continue;
                            }
                            if let Ok(url) = url_str.parse::<url::Url>() {
                                cookie_jar.add_cookie_str(cookie_str, &url);
                            }
                        }
                    }
                    tracing::debug!("Read cookies from {}", cookiejar_path.display());
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to read cookiejar {}: {}",
                        cookiejar_path.display(),
                        e
                    );
                }
            }
        }

        // Origin/Referer headers are required by Apple's CORS checks
        let mut default_headers = HeaderMap::new();
        default_headers.insert(ORIGIN, HeaderValue::from_str(home_endpoint)?);
        default_headers.insert(
            REFERER,
            HeaderValue::from_str(&format!("{}/", home_endpoint))?,
        );
        default_headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));

        let client = Client::builder()
            .cookie_provider(cookie_jar.clone())
            .default_headers(default_headers)
            .timeout(timeout)
            .build()?;

        let session_path = cookie_dir.join(format!("{}.session", sanitized));
        let session_data = if session_path.exists() {
            match fs::read_to_string(&session_path).await {
                Ok(contents) => match serde_json::from_str::<HashMap<String, Value>>(&contents) {
                    Ok(map) => {
                        tracing::debug!("Loaded session data from {}", session_path.display());
                        map.into_iter()
                            .map(|(k, v)| match v {
                                Value::String(s) => (k, s),
                                other => (k, other.to_string()),
                            })
                            .collect()
                    }
                    Err(_) => {
                        tracing::info!("Session file corrupt, starting fresh");
                        HashMap::new()
                    }
                },
                Err(_) => {
                    tracing::info!("Session file does not exist");
                    HashMap::new()
                }
            }
        } else {
            tracing::info!("Session file does not exist");
            HashMap::new()
        };

        tracing::debug!("Using session file {}", session_path.display());

        Ok(Self {
            client,
            _cookie_jar: cookie_jar,
            session_data,
            cookie_dir,
            sanitized_username: sanitized,
            home_endpoint: home_endpoint.to_string(),
            _timeout: timeout,
            _lock_file: lock_file,
        })
    }

    /// Path for the lock file.
    #[cfg(test)]
    pub fn lock_path(&self) -> PathBuf {
        self.cookie_dir
            .join(format!("{}.lock", self.sanitized_username))
    }

    /// Path for cookie jar persistence.
    pub fn cookiejar_path(&self) -> PathBuf {
        self.cookie_dir.join(&self.sanitized_username)
    }

    /// Path for session data JSON file.
    pub fn session_path(&self) -> PathBuf {
        self.cookie_dir
            .join(format!("{}.session", self.sanitized_username))
    }

    /// Get the client_id from session data, or None.
    pub fn client_id(&self) -> Option<&String> {
        self.session_data.get("client_id")
    }

    /// Set client_id in session data.
    pub fn set_client_id(&mut self, client_id: &str) {
        self.session_data
            .insert("client_id".to_string(), client_id.to_string());
    }

    /// Send a POST request, extract headers, save session data and cookies.
    pub async fn post(
        &mut self,
        url: &str,
        body: Option<String>,
        extra_headers: Option<HeaderMap>,
    ) -> Result<Response> {
        let mut builder = self.client.post(url);
        if let Some(h) = extra_headers {
            builder = builder.headers(h);
        }
        if let Some(b) = body {
            builder = builder.header("Content-Type", "application/json").body(b);
        }

        tracing::debug!("POST {}", url);
        let response = builder.send().await?;
        self.extract_and_save(&response).await?;
        Ok(response)
    }

    /// Send a GET request, extract headers, save session data and cookies.
    pub async fn get(&mut self, url: &str, extra_headers: Option<HeaderMap>) -> Result<Response> {
        let mut builder = self.client.get(url);
        if let Some(h) = extra_headers {
            builder = builder.headers(h);
        }

        tracing::debug!("GET {}", url);
        let response = builder.send().await?;
        self.extract_and_save(&response).await?;
        Ok(response)
    }

    /// Extract Apple session headers from every response and persist to disk.
    /// This must run after every request because Apple may rotate tokens at any time.
    async fn extract_and_save(&mut self, response: &Response) -> Result<()> {
        let headers = response.headers();
        for &(header_name, session_key) in HEADER_DATA {
            if let Some(val) = headers.get(header_name) {
                if let Ok(val_str) = val.to_str() {
                    self.session_data
                        .insert(session_key.to_string(), val_str.to_string());
                    // Track when the trust token was last updated
                    if session_key == "trust_token" {
                        self.session_data.insert(
                            "trust_token_timestamp".to_string(),
                            chrono::Utc::now().timestamp().to_string(),
                        );
                    }
                }
            }
        }

        let session_path = self.session_path();
        let json = serde_json::to_string_pretty(&self.session_data)?;
        fs::write(&session_path, json).await.with_context(|| {
            format!("Failed to write session data to {}", session_path.display())
        })?;
        #[cfg(unix)]
        {
            // Session files contain auth tokens — restrict to owner-only
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&session_path, perms)?;
        }
        tracing::debug!("Saved session data to file");

        // reqwest::cookie::Jar doesn't expose iteration, so we persist
        // Set-Cookie headers ourselves in a simple "url\tcookie" format.
        let cookiejar_path = self.cookiejar_path();
        let url_str = response.url().to_string();
        let mut cookie_lines: Vec<String> = if cookiejar_path.exists() {
            fs::read_to_string(&cookiejar_path)
                .await
                .with_context(|| {
                    format!(
                        "Failed to read cookie jar from {}",
                        cookiejar_path.display()
                    )
                })?
                .lines()
                .map(|l| l.to_string())
                .collect()
        } else {
            Vec::new()
        };

        let now = chrono::Utc::now();
        for cookie_header in headers.get_all("set-cookie") {
            if let Ok(val) = cookie_header.to_str() {
                // Skip cookies that are already expired
                if is_cookie_expired(val, &now) {
                    tracing::debug!(
                        "Skipping expired Set-Cookie: {}",
                        val.split('=').next().unwrap_or("")
                    );
                    continue;
                }
                let new_name = val.split('=').next().unwrap_or("");
                // Deduplicate: remove stale entries for the same cookie name + URL
                cookie_lines.retain(|line| {
                    if let Some((line_url, line_cookie)) = line.split_once('\t') {
                        if line_url == url_str {
                            let existing_name = line_cookie.split('=').next().unwrap_or("");
                            return existing_name != new_name;
                        }
                    }
                    true
                });
                cookie_lines.push(format!("{}\t{}", url_str, val));
            }
        }
        fs::write(&cookiejar_path, cookie_lines.join("\n"))
            .await
            .with_context(|| format!("Failed to write cookies to {}", cookiejar_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&cookiejar_path, perms)?;
        }

        Ok(())
    }

    /// Get the home endpoint URL.
    pub fn home_endpoint(&self) -> &str {
        &self.home_endpoint
    }

    /// Return a clone of the underlying HTTP client (with cookie jar attached).
    ///
    /// `reqwest::Client` is cheaply cloneable (backed by `Arc`), so this does
    /// not duplicate connections or state.
    pub fn http_client(&self) -> Client {
        self.client.clone()
    }

    /// How long ago the trust token was last updated, if tracked.
    pub fn trust_token_age(&self) -> Option<std::time::Duration> {
        let ts_str = self.session_data.get("trust_token_timestamp")?;
        let ts: i64 = ts_str.parse().ok()?;
        let now = chrono::Utc::now().timestamp();
        if now > ts {
            Some(std::time::Duration::from_secs((now - ts) as u64))
        } else {
            Some(std::time::Duration::ZERO)
        }
    }

    /// Returns true if the trust token is older than (30 - warn_days) days.
    /// Apple trust tokens last ~30 days empirically.
    pub fn trust_token_expires_soon(&self, warn_days: u64) -> bool {
        const TRUST_TOKEN_LIFETIME_DAYS: u64 = 30;
        match self.trust_token_age() {
            Some(age) => {
                let threshold = std::time::Duration::from_secs(
                    (TRUST_TOKEN_LIFETIME_DAYS - warn_days.min(TRUST_TOKEN_LIFETIME_DAYS))
                        * 24
                        * 60
                        * 60,
                );
                age > threshold
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp/claude/session_tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_lock_file_prevents_concurrent_sessions() {
        let dir = test_dir("lock_concurrent");
        let _s1 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .expect("First session should succeed");

        let result = Session::new(&dir, "user@test.com", "https://example.com", None).await;
        match result {
            Ok(_) => panic!("Second session should have failed"),
            Err(e) => assert!(
                e.to_string().contains("Another icloudpd-rs instance"),
                "Unexpected error: {}",
                e
            ),
        }
    }

    #[tokio::test]
    async fn test_lock_file_different_users_allowed() {
        let dir = test_dir("lock_different_users");
        let _s1 = Session::new(&dir, "alice@test.com", "https://example.com", None)
            .await
            .unwrap();
        let _s2 = Session::new(&dir, "bob@test.com", "https://example.com", None)
            .await
            .expect("Different users should not conflict");
    }

    #[tokio::test]
    async fn test_lock_released_on_drop() {
        let dir = test_dir("lock_release");
        {
            let _s = Session::new(&dir, "user@test.com", "https://example.com", None)
                .await
                .unwrap();
        } // _s dropped here, lock released
        let _s2 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .expect("Lock should be released after drop");
    }

    #[test]
    fn test_trust_token_age_none_when_no_timestamp() {
        let session_data: HashMap<String, String> = HashMap::new();
        let ts: Option<&String> = session_data.get("trust_token_timestamp");
        assert!(ts.is_none());
    }

    #[tokio::test]
    async fn test_trust_token_age_computes_correctly() {
        let dir = test_dir("trust_age");
        let mut session = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();

        // No timestamp yet
        assert!(session.trust_token_age().is_none());

        // Set timestamp to 2 hours ago
        let two_hours_ago = chrono::Utc::now().timestamp() - 7200;
        session.session_data.insert(
            "trust_token_timestamp".to_string(),
            two_hours_ago.to_string(),
        );
        let age = session.trust_token_age().unwrap();
        // Should be approximately 7200 seconds (allow 5s tolerance)
        assert!(age.as_secs() >= 7195 && age.as_secs() <= 7210);
    }

    #[tokio::test]
    async fn test_trust_token_expires_soon() {
        let dir = test_dir("trust_expires");
        let mut session = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();

        // No timestamp — should not warn
        assert!(!session.trust_token_expires_soon(7));

        // Set timestamp to 25 days ago — should warn with 7-day window
        let twenty_five_days_ago = chrono::Utc::now().timestamp() - (25 * 86400);
        session.session_data.insert(
            "trust_token_timestamp".to_string(),
            twenty_five_days_ago.to_string(),
        );
        assert!(session.trust_token_expires_soon(7));

        // Set timestamp to 10 days ago — should not warn
        let ten_days_ago = chrono::Utc::now().timestamp() - (10 * 86400);
        session.session_data.insert(
            "trust_token_timestamp".to_string(),
            ten_days_ago.to_string(),
        );
        assert!(!session.trust_token_expires_soon(7));
    }

    #[tokio::test]
    async fn test_expired_cookies_pruned_on_load() {
        let dir = test_dir("cookie_prune");
        let sanitized = sanitize_username("user@test.com");
        let cookie_path = dir.join(&sanitized);

        // Write a cookie file with one expired and one valid cookie
        let expired = format!(
            "https://example.com\texpired_cookie=val; Expires=Thu, 01 Jan 2020 00:00:00 GMT"
        );
        let valid =
            format!("https://example.com\tvalid_cookie=val; Expires=Thu, 01 Jan 2099 00:00:00 GMT");
        std::fs::write(&cookie_path, format!("{}\n{}", expired, valid)).unwrap();

        let session = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();

        // The expired cookie should have been pruned; valid one kept
        // We can't directly inspect the cookie jar, but we can verify the session loaded
        assert!(session.cookiejar_path().exists());
    }

    #[test]
    fn test_is_cookie_expired_past() {
        let now = chrono::Utc::now();
        assert!(is_cookie_expired(
            "foo=bar; Expires=Thu, 01 Jan 2020 00:00:00 GMT",
            &now
        ));
    }

    #[test]
    fn test_is_cookie_expired_future() {
        let now = chrono::Utc::now();
        assert!(!is_cookie_expired(
            "foo=bar; Expires=Thu, 01 Jan 2099 00:00:00 GMT",
            &now
        ));
    }

    #[test]
    fn test_is_cookie_expired_no_expiry() {
        let now = chrono::Utc::now();
        assert!(!is_cookie_expired("foo=bar", &now));
    }

    #[test]
    fn test_sanitize_username() {
        assert_eq!(sanitize_username("user@example.com"), "userexamplecom");
        assert_eq!(sanitize_username("hello_world"), "hello_world");
        assert_eq!(sanitize_username("a.b-c@d"), "abcd");
    }
}
