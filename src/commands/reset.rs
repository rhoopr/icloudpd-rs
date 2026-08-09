#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print reset status to stdout"
)]

use std::path::{Path, PathBuf};

use anyhow::Context;
use fs4::fs_std::FileExt;

use crate::auth;
use crate::cli;
use crate::config;
use crate::state;

/// Run the reset-state command.
pub(crate) async fn run_reset_state(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    if !yes {
        use std::io::Write;
        println!("This will delete the state database at:");
        println!("  {}", db_path.display());
        println!();
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    tokio::fs::remove_file(&db_path).await?;
    println!("State database deleted.");

    // Also remove WAL and SHM files if they exist
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");
    let _ = tokio::fs::remove_file(&wal_path).await;
    let _ = tokio::fs::remove_file(&shm_path).await;

    Ok(())
}

/// Run the reset-sync-token command.
///
/// `yes` skips the confirmation prompt. Without it, prompts on a TTY and
/// errors under non-interactive use, mirroring `reset state`. Clearing the
/// sync token forces the next sync to do a full enumeration of every asset,
/// which on a 100k-asset library is slow and chats up Apple's API; the
/// confirmation is here to keep a typo (or muscle memory after `reset state`)
/// from triggering that work by accident.
pub(crate) async fn run_reset_sync_token(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    if !yes {
        use std::io::IsTerminal;
        use std::io::Write;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "`kei reset sync-token` needs `--yes` when stdin is not a terminal. The next sync will re-enumerate every asset, which can take a long time on a large library."
            );
        }
        println!("This will clear stored sync tokens at:");
        println!("  {}", db_path.display());
        println!();
        println!("Next sync will re-enumerate every asset.");
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    db.set_metadata("db_sync_token", "").await?;
    let cleared = db.delete_metadata_by_prefix("sync_token:").await?;
    let scoped_cleared = db.delete_scoped_db_sync_tokens().await?;
    println!(
        "Cleared sync tokens ({} zone token{} + db token + {} scoped db token{}). Next sync will do a full enumeration.",
        cleared,
        if cleared == 1 { "" } else { "s" },
        scoped_cleared,
        if scoped_cleared == 1 { "" } else { "s" }
    );

    Ok(())
}

/// Session artifacts for `username` inside `cookie_directory`, in removal
/// order: the cookie jar, the persisted session, and the response cache.
///
/// The password store (`<user>.credential`) and the state database are
/// deliberately excluded: `reset session` discards Apple session state
/// (including trust tokens) only, so `kei login` can mint a clean session
/// afterwards.
fn session_files(cookie_directory: &Path, username: &str) -> Vec<PathBuf> {
    let slug = auth::session::sanitize_username(username);
    vec![
        cookie_directory.join(&slug),
        cookie_directory.join(format!("{slug}.session")),
        cookie_directory.join(format!("{slug}.cache")),
    ]
}

/// Acquire the per-account advisory session lock, failing on contention.
///
/// Mirrors the lock acquisition in `auth::session::Session::new`: a held lock
/// means another kei instance (sync, service, or login) is active for this
/// account, and `reset session` must not remove session files from under it.
fn acquire_session_lock(cookie_directory: &Path, slug: &str) -> anyhow::Result<std::fs::File> {
    let lock_path = cookie_directory.join(format!("{slug}.lock"));
    let file = std::fs::File::create(&lock_path)
        .with_context(|| format!("Could not create session lock file {}", lock_path.display()))?;
    let acquired = file
        .try_lock_exclusive()
        .with_context(|| format!("Could not acquire session lock {}", lock_path.display()))?;
    anyhow::ensure!(
        acquired,
        "Another kei instance is running for this account (lock: {}). \
         Stop it before resetting the session. If running in Docker, check \
         for containers with `docker ps` and stop them with `docker stop <name>`.",
        lock_path.display()
    );
    Ok(file)
}

/// Remove every file in `files` that exists, returning the paths removed.
async fn discard(files: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for path in files {
        match tokio::fs::remove_file(path).await {
            Ok(()) => removed.push(path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("Could not remove {}", path.display()));
            }
        }
    }
    Ok(removed)
}

/// Run the reset-session command: discard the local session so the next
/// `kei login` starts from a clean slate.
///
/// `yes` skips the confirmation prompt. Without it, prompts on a TTY and
/// errors under non-interactive use, mirroring `reset sync-token`.
/// Discarding the session (including trust tokens) forces the next
/// `kei login` to run the full password + 2FA flow; the stored password
/// and the state database are kept.
pub(crate) async fn run_reset_session(
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    // Reset never authenticates; resolve_auth only reads the password from
    // CLI/env args, so empty PasswordArgs are fine here.
    let password_args = cli::PasswordArgs::default();
    let (username, _, _, cookie_directory) = config::resolve_auth(globals, &password_args, toml);

    if username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
    }

    let files = session_files(&cookie_directory, &username);
    let existing: Vec<&PathBuf> = files.iter().filter(|p| p.exists()).collect();

    // Nothing to remove: report and exit before the lock and the `--yes`
    // guard, mirroring the no-DB early return of the other reset subcommands.
    if existing.is_empty() {
        println!(
            "No local session found in {} — nothing to reset.",
            cookie_directory.display()
        );
        return Ok(());
    }

    // Hold the same advisory lock the sync service takes — acquired before
    // the prompt, so no instance can start mid-confirmation and we never
    // yank session files out from under a running one.
    let slug = auth::session::sanitize_username(&username);
    let _lock_file = tokio::task::spawn_blocking({
        let cookie_directory = cookie_directory.clone();
        move || acquire_session_lock(&cookie_directory, &slug)
    })
    .await??;

    if !yes {
        use std::io::IsTerminal;
        use std::io::Write;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "`kei reset session` needs `--yes` when stdin is not a terminal. This discards the local session and trust tokens, and the next `kei login` will run the full password + 2FA flow again."
            );
        }
        println!("This will discard the local iCloud session (including trust tokens):");
        for path in &existing {
            println!("  {}", path.display());
        }
        println!();
        println!("The stored password and the state database are kept.");
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let removed = discard(&files).await?;
    for path in &removed {
        println!("Removed {}", path.display());
    }
    println!("Session reset. Run `kei login` to authenticate again.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_files_cover_jar_session_and_cache_only() {
        let dir = Path::new("/data");
        let files = session_files(dir, "user@test.com");
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let slug = auth::session::sanitize_username("user@test.com");
        assert_eq!(
            names,
            vec![
                slug.clone(),
                format!("{slug}.session"),
                format!("{slug}.cache")
            ]
        );
    }

    #[tokio::test]
    async fn reset_session_removes_nothing_while_the_session_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let slug = auth::session::sanitize_username("user@test.com");
        std::fs::write(dir.path().join(&slug), b"jar").unwrap();
        std::fs::write(dir.path().join(format!("{slug}.session")), b"session").unwrap();

        // Simulate a running instance holding the advisory lock.
        let held = acquire_session_lock(dir.path(), &slug).unwrap();

        // The reset's lock step must fail on contention, so discard never runs.
        let err = acquire_session_lock(dir.path(), &slug).unwrap_err();
        assert!(
            err.to_string().contains("Another kei instance is running"),
            "unexpected error: {err}"
        );
        assert!(dir.path().join(&slug).exists());
        assert!(dir.path().join(format!("{slug}.session")).exists());

        // Once the running instance goes away, the lock is acquirable again.
        drop(held);
        let _reacquired = acquire_session_lock(dir.path(), &slug).unwrap();
    }

    #[tokio::test]
    async fn discard_removes_only_listed_files_and_tolerates_missing() {
        let dir = tempfile::tempdir().unwrap();
        let slug = auth::session::sanitize_username("user@test.com");
        let files = session_files(dir.path(), "user@test.com");

        // Present: jar + cache. Missing: .session. Bystanders: credential + db.
        std::fs::write(dir.path().join(&slug), b"jar").unwrap();
        std::fs::write(dir.path().join(format!("{slug}.cache")), b"cache").unwrap();
        let credential = dir.path().join(format!("{slug}.credential"));
        let db = dir.path().join(format!("{slug}.db"));
        std::fs::write(&credential, b"cred").unwrap();
        std::fs::write(&db, b"db").unwrap();

        let removed = discard(&files).await.unwrap();

        assert_eq!(removed.len(), 2);
        assert!(!dir.path().join(&slug).exists());
        assert!(!dir.path().join(format!("{slug}.cache")).exists());
        assert!(credential.exists(), "password store must survive the reset");
        assert!(db.exists(), "state database must survive the reset");
    }
}
