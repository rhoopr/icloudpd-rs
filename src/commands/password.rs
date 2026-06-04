#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print credential-store status to stdout"
)]

use crate::cli;
use crate::config;
use crate::credential;

/// Run the password subcommand: set, clear, or show backend.
pub(crate) fn run_password(
    action: cli::PasswordAction,
    globals: &config::GlobalArgs,
    pw: &cli::PasswordArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let (username, _password, _domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!(
            "Set your iCloud username with ICLOUD_USERNAME or [auth].username before managing a password."
        );
    }

    let store = credential::CredentialStore::new(&username, &cookie_directory);

    match action {
        cli::PasswordAction::Set => {
            let input = rpassword::prompt_password("iCloud Password: ")
                .map_err(|e| anyhow::anyhow!("Could not read password: {e}"))?;
            anyhow::ensure!(!input.is_empty(), "Password cannot be empty.");
            let backend = store.store(&input)?;
            println!("Password stored in {} backend.", backend.as_str());
        }
        cli::PasswordAction::Clear => {
            store.delete()?;
            println!("Stored credential removed.");
        }
        cli::PasswordAction::Backend => {
            println!("{}", store.backend_name());
        }
    }
    Ok(())
}
