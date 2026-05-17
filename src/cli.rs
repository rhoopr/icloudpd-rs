use crate::types::{
    FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoSize, LogLevel,
    RawTreatmentPolicy, VersionSize,
};
use clap::{Args, FromArgMatches, Parser, Subcommand};

/// Reject empty strings at CLI parse time.
fn non_empty_string(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("value must not be empty".to_string())
    } else {
        Ok(s.to_string())
    }
}

/// Parse a human-readable byte-rate into bytes per second.
///
/// Accepts a non-negative number (integer or decimal, e.g. `1.5`) followed by
/// an optional unit suffix: decimal `K`/`M`/`G` (x1000) or binary
/// `Ki`/`Mi`/`Gi` (x1024). Suffix is case-insensitive. No suffix means
/// bytes/sec. Values that round to zero bytes/sec are rejected.
pub(crate) fn parse_bandwidth_limit(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("value must not be empty".to_string());
    }
    // Numeric part is digits plus at most one decimal point; everything after
    // that is the unit suffix. Leading sign is not accepted here (negative
    // bandwidth is meaningless; `-5M` errors with "must start with a number").
    let num_end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    if num_end == 0 {
        return Err(format!(
            "invalid bandwidth value `{s}`: must start with a number"
        ));
    }
    let (num_str, suffix) = trimmed.split_at(num_end);
    let n: f64 = num_str
        .parse()
        .map_err(|_e| format!("invalid bandwidth number `{num_str}`"))?;
    if !n.is_finite() || n < 0.0 {
        return Err(format!(
            "invalid bandwidth number `{num_str}`: must be a finite non-negative number"
        ));
    }
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "ki" | "kib" => 1_024,
        "mi" | "mib" => 1_024 * 1_024,
        "gi" | "gib" => 1_024 * 1_024 * 1_024,
        other => {
            return Err(format!(
                "invalid bandwidth unit `{other}`: expected one of K, M, G, Ki, Mi, Gi"
            ));
        }
    };
    // f64 multiplication is exact for the typical bandwidth range (KB/s to
    // GB/s) and off by less than a byte for extreme values; round to nearest
    // so inputs like `0.1K` land on 100 bytes/sec rather than 99.
    #[allow(
        clippy::cast_precision_loss,
        reason = "multiplier is a small constant (<= 2^30); u64::MAX is only a comparison bound where exact precision doesn't matter"
    )]
    let (max_f64, multiplier_f64) = (u64::MAX as f64, multiplier as f64);
    let total = n * multiplier_f64;
    if !total.is_finite() || total > max_f64 {
        return Err(format!("bandwidth value `{s}` overflows u64 bytes/sec"));
    }
    let rounded = total.round();
    if rounded < 1.0 {
        return Err(format!(
            "bandwidth value `{s}` rounds to zero bytes/sec; must be at least 1 byte/sec"
        ));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounds checked above: 1.0 <= rounded <= u64::MAX as f64"
    )]
    Ok(rounded as u64)
}

/// Strip non-digit characters and validate that the result is exactly 6 digits.
/// Accepts "123456", "123 456", "123-456", etc.
fn parse_2fa_code(s: &str) -> Result<String, String> {
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    if digits.len() == 6 {
        Ok(digits)
    } else {
        Err("must contain exactly 6 digits".to_string())
    }
}

/// Limit on which assets a sync pass processes.
///
/// `--recent 100` is a count limit (top N most-recent assets). `--recent 30d`
/// is a days limit (assets created in the last 30 days) and translates to a
/// `skip_created_before` cutoff at `Config::build` time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecentLimit {
    /// Take the N most-recent assets by creation date.
    Count(u32),
    /// Take assets created in the last N days.
    Days(u32),
}

/// Parse `--recent N` (count) or `--recent Nd` (days). Clap `value_parser`.
///
/// Rejects zero, empty, and unknown suffixes. Only `d` (days) is supported
/// today; this keeps the syntax open for future units without locking us in.
pub(crate) fn parse_recent_limit(s: &str) -> Result<RecentLimit, String> {
    if s.is_empty() {
        return Err("must not be empty".to_string());
    }
    let (num_str, is_days) = if let Some(stripped) = s.strip_suffix('d') {
        (stripped, true)
    } else {
        (s, false)
    };
    let n: u32 = num_str
        .parse()
        .map_err(|_e| format!("expected a positive integer or `Nd` form (got `{s}`)"))?;
    if n == 0 {
        return Err(format!("must be greater than zero (got `{s}`)"));
    }
    Ok(if is_days {
        RecentLimit::Days(n)
    } else {
        RecentLimit::Count(n)
    })
}

impl std::fmt::Display for RecentLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(n) => write!(f, "{n}"),
            Self::Days(n) => write!(f, "{n}d"),
        }
    }
}

impl serde::Serialize for RecentLimit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            // Count serializes as a bare integer so TOML round-trips cleanly
            // for the common case (`recent = 100`, not `recent = "100"`).
            Self::Count(n) => s.serialize_u32(*n),
            Self::Days(n) => s.serialize_str(&format!("{n}d")),
        }
    }
}

impl<'de> serde::Deserialize<'de> for RecentLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u32),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(0) => Err(D::Error::custom("`recent` must be greater than zero")),
            Raw::Int(n) => Ok(Self::Count(n)),
            Raw::Str(s) => parse_recent_limit(&s).map_err(D::Error::custom),
        }
    }
}

/// Password arguments shared across commands that authenticate.
/// Flattened only onto commands that actually need a password (sync, login,
/// list, import-existing, password).
#[derive(Parser, Debug, Clone, Default)]
pub struct PasswordArgs {
    /// iCloud password (if not provided, will prompt).
    /// WARNING: passing via --password is visible in process listings.
    /// Prefer the `ICLOUD_PASSWORD` environment variable instead.
    #[arg(short = 'p', long, env = "ICLOUD_PASSWORD", value_parser = non_empty_string)]
    pub password: Option<String>,

    /// Read password from a file on each auth attempt.
    /// Supports Docker secrets (e.g., /run/secrets/icloud_password).
    /// Trailing newline is stripped.
    #[arg(long, env = "KEI_PASSWORD_FILE", conflicts_with = "password")]
    pub password_file: Option<String>,

    /// Execute a shell command to obtain the password on each auth attempt.
    /// Supports external secret managers (1Password, Vault, pass).
    /// Example: --password-command "op read 'op://vault/icloud/password'"
    #[arg(long, env = "KEI_PASSWORD_COMMAND", conflicts_with_all = ["password", "password_file"])]
    pub password_command: Option<String>,
}

/// Arguments for the sync command (also used as default when no subcommand).
#[derive(Parser, Debug, Clone, Default)]
pub struct SyncArgs {
    /// Number of recent photos to download (e.g. `--recent 100`) or a recency
    /// window in days (e.g. `--recent 30d`). Days form maps to
    /// `--skip-created-before` internally.
    #[arg(long, value_parser = parse_recent_limit)]
    pub recent: Option<RecentLimit>,

    /// Do not modify local system or iCloud
    #[arg(long)]
    pub dry_run: bool,

    /// Disable progress bar
    #[arg(long, num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub no_progress_bar: Option<bool>,

    /// Skip assets created before this ISO date or interval (e.g., 2025-01-02 or 20d)
    #[arg(long)]
    pub skip_created_before: Option<String>,

    /// Skip assets created after this ISO date or interval (e.g., 2025-01-02 or 20d)
    #[arg(long)]
    pub skip_created_after: Option<String>,

    /// Only print filenames without downloading
    #[arg(long)]
    pub only_print_filenames: bool,

    /// After successful auth, persist the password to the credential store
    /// (OS keyring or encrypted file).
    #[arg(long)]
    pub save_password: bool,

    /// Re-sync only previously failed assets
    #[arg(long, conflicts_with = "dry_run")]
    pub retry_failed: bool,

    /// Internal durable config overrides for tests and programmatic call
    /// sites. Public CLI/env no longer expose these fields in v0.20.
    #[arg(skip)]
    pub(crate) config_overrides: crate::config::SyncConfigOverrides,
}

/// Arguments for the status command.
#[derive(Parser, Debug, Clone)]
pub struct StatusArgs {
    /// Show failed assets with error messages
    #[arg(long)]
    pub failed: bool,

    /// Show pending assets (known to the DB, not yet finalized this sync).
    /// Includes assets the current sync scope excludes via filters or album
    /// selection.
    #[arg(long)]
    pub pending: bool,

    /// Show downloaded assets
    #[arg(long)]
    pub downloaded: bool,
}

/// Arguments for the import-existing command.
#[derive(Parser, Debug, Clone)]
pub struct ImportArgs {
    #[command(flatten)]
    pub password: PasswordArgs,

    /// Library/libraries to import. Repeatable; default `primary`. Same value
    /// grammar as `[filters].libraries`: a CloudKit zone name (full UUID or
    /// the truncated 8-char `SharedSync-` prefix that `{library}` renders
    /// into paths), the sentinels `primary` / `shared` / `all` / `none`, or
    /// `!name` to exclude.
    #[arg(long = "library", env = "KEI_LIBRARY", value_parser = non_empty_string)]
    pub libraries: Vec<String>,

    /// Local directory containing existing downloads
    #[arg(short = 'd', long = "download-dir", env = "KEI_DOWNLOAD_DIR", value_parser = non_empty_string)]
    pub download_dir: Option<String>,

    /// Folder structure used by the unfiled pass when matching files on
    /// disk (must match `--folder-structure` during sync).
    #[arg(long, env = "KEI_FOLDER_STRUCTURE")]
    pub folder_structure: Option<String>,

    /// Folder structure used by every album pass when matching files on
    /// disk (must match `--folder-structure-albums` during sync). Default
    /// `{album}`.
    #[arg(long, env = "KEI_FOLDER_STRUCTURE_ALBUMS")]
    pub folder_structure_albums: Option<String>,

    /// Folder structure used by every smart-folder pass when matching
    /// files on disk (must match `--folder-structure-smart-folders` during
    /// sync). Default `{smart-folder}`.
    #[arg(long, env = "KEI_FOLDER_STRUCTURE_SMART_FOLDERS")]
    pub folder_structure_smart_folders: Option<String>,

    /// Keep Unicode in filenames (must match what was used during sync)
    #[arg(long, env = "KEI_KEEP_UNICODE_IN_FILENAMES", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub keep_unicode_in_filenames: Option<bool>,

    /// File matching and dedup policy (must match what was used during sync)
    #[arg(long, env = "KEI_FILE_MATCH_POLICY", value_enum)]
    pub file_match_policy: Option<FileMatchPolicy>,

    /// Image size to import (must match what was used during sync). Default: original.
    #[arg(long, env = "KEI_SIZE", value_enum)]
    pub size: Option<VersionSize>,

    /// Live photo handling: both, image-only, video-only, skip
    /// (must match what was used during sync)
    #[arg(long, env = "KEI_LIVE_PHOTO_MODE", value_enum)]
    pub live_photo_mode: Option<LivePhotoMode>,

    /// Live photo video size (must match what was used during sync)
    #[arg(long, env = "KEI_LIVE_PHOTO_SIZE", value_enum)]
    pub live_photo_size: Option<LivePhotoSize>,

    /// Live photo MOV filename policy (must match what was used during sync)
    #[arg(long, env = "KEI_LIVE_PHOTO_MOV_FILENAME_POLICY", value_enum)]
    pub live_photo_mov_filename_policy: Option<LivePhotoMovFilenamePolicy>,

    /// RAW treatment policy (must match what was used during sync)
    #[arg(long, env = "KEI_ALIGN_RAW", value_enum)]
    pub align_raw: Option<RawTreatmentPolicy>,

    /// Only check the requested size (don't fall back to original)
    #[arg(long, env = "KEI_FORCE_SIZE", num_args = 0..=1, default_missing_value = "true", hide_possible_values = true)]
    pub force_size: Option<bool>,

    /// Number of recent photos to check (`--recent 100`). The `--recent Nd`
    /// days form is only supported in `sync`; import-existing errors on use.
    #[arg(long, value_parser = parse_recent_limit)]
    pub recent: Option<RecentLimit>,

    /// Scan and report matches without writing to the state DB. Useful for
    /// verifying that `--folder-structure` and `--keep-unicode-in-filenames`
    /// match the tree you're importing before committing.
    #[arg(long)]
    pub dry_run: bool,

    /// Disable progress bar
    #[arg(long)]
    pub no_progress_bar: bool,

    /// Override the empty-library safety guard. Without this flag,
    /// `import-existing` aborts when a selected library returns zero
    /// assets while the state DB has prior asset rows -- often a
    /// transient iCloud permissions glitch or stale auth, where
    /// scanning would silently produce a misleading `matched: 0`
    /// report. Set this if you genuinely emptied the library, or if
    /// you're attaching a new sub-library to an account that already
    /// has data (the prior-row check is global, not per-zone).
    #[arg(long, env = "KEI_FORCE_EMPTY")]
    pub force_empty: bool,
}

/// Arguments for the verify command.
#[derive(Parser, Debug, Clone)]
pub struct VerifyArgs {
    /// Verify checksums (slower but more thorough)
    #[arg(long)]
    pub checksums: bool,
}

/// Arguments for the reconcile command.
#[derive(Parser, Debug, Clone)]
pub struct ReconcileArgs {
    /// Show what would change without updating the state database.
    #[arg(long)]
    pub dry_run: bool,
}

// ── New subcommand types ─────────────────────────────────────────────

/// Login subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum LoginCommand {
    /// Request a 2FA code be sent to your trusted devices
    GetCode,
    /// Submit a 2FA code non-interactively (for Docker / headless use)
    SubmitCode {
        /// 6-digit 2FA code from your trusted device
        #[arg(value_parser = parse_2fa_code)]
        code: String,
    },
}

/// List subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ListCommand {
    /// List available albums
    Albums,
    /// List available libraries
    Libraries,
}

/// Password management actions.
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordAction {
    /// Store a password in the credential store (prompts interactively)
    Set,
    /// Remove a stored password
    Clear,
    /// Show which credential backend is active (keyring, encrypted-file, none)
    Backend,
}

/// Reset subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ResetCommand {
    /// Delete the state database and start fresh
    State {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Clear stored sync tokens so the next sync does a full enumeration.
    ///
    /// Without `--yes`, prompts for confirmation on a TTY (the next sync will
    /// re-enumerate every asset, which can be expensive on large libraries)
    /// and errors out under non-interactive use, matching `reset state`.
    SyncToken {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// Config management actions.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    /// Dump resolved config as TOML and exit
    Show,
    /// Interactively generate a config file
    Setup {
        /// Output path (overrides --config)
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
}

/// Arguments for `kei install`.
///
/// Per-platform defaults: Linux installs a per-user systemd unit unless
/// `--system` is passed; macOS installs a per-user launchd agent (system
/// daemons require root and are out of scope for v0.14); Windows registers
/// a system service via the Service Control Manager (per-user services
/// are not a Windows concept).
#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    /// Install per-user (Linux/macOS default; ignored on Windows).
    #[arg(long, conflicts_with = "system")]
    pub user: bool,

    /// Install system-wide (Linux only; requires root). On macOS and
    /// Windows the per-platform default is used regardless of this flag.
    #[arg(long, conflicts_with = "user")]
    pub system: bool,

    /// Render the service file and report what would happen, without
    /// invoking the platform service manager (no `systemctl daemon-reload`,
    /// no `launchctl bootstrap`, no SCM `CreateService`). The unit file is
    /// still written to disk so it can be inspected.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `kei uninstall`.
#[derive(Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Also remove the state database, configuration, and stored
    /// credentials. Default off: data is sacred, removal is opt-in.
    #[arg(long)]
    pub purge: bool,
}

/// Arguments for `kei service run`.
///
/// Identical to `kei sync` arguments; carried as its own struct so the
/// surrounding [`ServiceAction`] enum does not balloon by ~600 bytes
/// (the size of `SyncArgs`) on the unrelated `Status` variant.
#[derive(Args, Debug, Clone)]
pub struct ServiceRunArgs {
    #[command(flatten)]
    pub password: PasswordArgs,

    #[command(flatten)]
    pub sync: SyncArgs,
}

/// Subcommands under `kei service`.
#[derive(Subcommand, Debug, Clone)]
pub enum ServiceAction {
    /// Run the service worker (invoked by launchd / systemd / Windows SCM,
    /// or directly for testing). Equivalent to `kei sync` with service-mode
    /// defaults: when no other source provides a watch interval, defaults
    /// to 86400 seconds so the daemon polls once per day.
    Run(Box<ServiceRunArgs>),

    /// Show whether kei is registered as a service on this host and when
    /// it last started. For a combined summary including photo library
    /// stats, use `kei status` instead.
    Status,
}

/// Subcommands for kei.
#[allow(
    clippy::large_enum_variant,
    reason = "parsed once at startup; boxing flattened clap args would add command plumbing without runtime benefit"
)]
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Download photos from iCloud (the default: running `kei` with no command does this)
    Sync {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(flatten)]
        sync: SyncArgs,
    },

    /// Authenticate interactively (creates/refreshes session tokens)
    Login {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(subcommand)]
        subcommand: Option<LoginCommand>,
    },

    /// List available albums or libraries
    List {
        #[command(flatten)]
        password: PasswordArgs,

        /// Library/libraries to list albums from. Repeatable; default
        /// `primary` (the PrimarySync zone). Same value grammar as
        /// `[filters].libraries`: a CloudKit zone name (full UUID or the
        /// truncated 8-char `SharedSync-` prefix that `{library}` renders
        /// into paths), the sentinels `primary` / `shared` / `all` /
        /// `none`, or `!name` to exclude. Only consulted by
        /// `kei list albums`; `kei list libraries` always lists every
        /// library on the account.
        #[arg(long = "library", env = "KEI_LIBRARY", value_parser = non_empty_string)]
        libraries: Vec<String>,

        #[command(subcommand)]
        what: ListCommand,
    },

    /// Manage stored credentials (OS keyring or encrypted file)
    Password {
        #[command(flatten)]
        password: PasswordArgs,

        #[command(subcommand)]
        action: PasswordAction,
    },

    /// Reset state database or sync tokens
    Reset {
        #[command(subcommand)]
        what: ResetCommand,
    },

    /// Config management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Show sync status and database summary
    Status(StatusArgs),

    /// Import existing local files into the state database
    ImportExisting(ImportArgs),

    /// Verify downloaded files exist and optionally check checksums
    Verify(VerifyArgs),

    /// Reconcile state database with files on disk: mark assets as
    /// failed when their local file is missing, so the next sync
    /// re-downloads them.
    Reconcile(ReconcileArgs),

    /// Register kei as a system service (launchd on macOS, systemd on
    /// Linux, Service Control Manager on Windows). Inside a Docker
    /// container the command logs that compose-managed deployments are
    /// already supervised and exits without writing anything.
    Install(InstallArgs),

    /// Remove the kei service registered by `kei install`. Pass `--purge`
    /// to also delete the state database, configuration, and stored
    /// credentials.
    Uninstall(UninstallArgs),

    /// Service-mode operations: run the long-lived worker, or query
    /// service registration status.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Parser, Debug)]
#[command(
    name = "kei",
    about = "kei: photo sync engine",
    version,
    after_help = "Getting started:\n  \
        kei config setup    Generate your config interactively\n  \
        kei sync            Download your photos (runs after setup)\n\n  \
        Common:\n  \
        kei status          See what's been downloaded\n  \
        kei login           Sign in to iCloud\n  \
        kei list albums     Browse albums and libraries\n\n  \
        Advanced:\n  \
        kei install         Register as a background service\n  \
        kei verify          Verify downloaded files\n  \
        kei import-existing  Adopt an existing local photo tree"
)]
pub struct Cli {
    // ── Global options ──────────────────────────────────────────────
    /// Log level
    #[arg(long, value_enum, global = true, env = "KEI_LOG_LEVEL")]
    pub log_level: Option<LogLevel>,

    /// Verbose output (alias for --log-level info, restores full target+timestamp format)
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Use friendly progress UI (default on interactive terminals)
    #[arg(
        long,
        global = true,
        overrides_with = "no_friendly",
        long_help = "Use friendly progress UI (verb-cycling spinners, curated phase narration, summary card, sign-off). \
                     Default: on for plain TTYs, off in service/container/journal contexts and whenever a \
                     machine-output mode (`--only-print-filenames` or TOML report JSON) or an explicit \
                     `--log-level` / `RUST_LOG` is in play. `--friendly` overrides the TOML `[ui] friendly` \
                     setting and the auto-detected default; environmental hard-off contexts still win."
    )]
    pub friendly: bool,

    /// Disable friendly progress UI
    #[arg(
        long,
        global = true,
        overrides_with = "friendly",
        long_help = "Force friendly progress messages off (preserves v0.13 scrollback byte-for-byte). \
                     Overrides `--friendly`, the TOML `[ui] friendly` setting, and the auto-detected default. \
                     Use this when piping kei output to a log aggregator on an interactive TTY where \
                     auto-detection would otherwise enable friendly mode."
    )]
    pub no_friendly: bool,

    /// Path to TOML config file
    #[arg(
        long,
        global = true,
        default_value = "~/.config/kei/config.toml",
        env = "KEI_CONFIG"
    )]
    pub config: String,

    #[command(subcommand)]
    pub command: Option<Command>,

    // ── Backward compat: bare invocation = sync ────────────────────
    #[command(flatten)]
    pub password: PasswordArgs,

    #[command(flatten)]
    pub sync: SyncArgs,
}

impl SyncArgs {
    /// Merge top-level (fallback) sync args into self.
    /// Subcommand values take precedence; top-level fills in gaps.
    fn merge_from(&mut self, fallback: &Self) {
        if self.recent.is_none() {
            self.recent = fallback.recent;
        }
        self.dry_run = self.dry_run || fallback.dry_run;
        if self.no_progress_bar.is_none() {
            self.no_progress_bar = fallback.no_progress_bar;
        }
        if self.skip_created_before.is_none() {
            self.skip_created_before
                .clone_from(&fallback.skip_created_before);
        }
        if self.skip_created_after.is_none() {
            self.skip_created_after
                .clone_from(&fallback.skip_created_after);
        }
        self.only_print_filenames = self.only_print_filenames || fallback.only_print_filenames;
        self.save_password = self.save_password || fallback.save_password;
        self.retry_failed = self.retry_failed || fallback.retry_failed;
    }
}

impl PasswordArgs {
    /// Merge top-level (fallback) password args into self.
    fn merge_from(&mut self, fallback: &Self) {
        if self.password.is_none() {
            self.password.clone_from(&fallback.password);
        }
        if self.password_file.is_none() {
            self.password_file.clone_from(&fallback.password_file);
        }
        if self.password_command.is_none() {
            self.password_command.clone_from(&fallback.password_command);
        }
    }
}

impl Cli {
    /// User-stated friendly-mode preference, distilled from the
    /// `--friendly` / `--no-friendly` pair. `Some(true)` and `Some(false)` are explicit user requests
    /// that override the TOML and the auto-detected default; `None` means
    /// neither flag was set, so the resolution chain falls through to TOML
    /// then to the default-on-for-TTY policy.
    ///
    /// Clap's `overrides_with` makes the two flags mutually exclusive at the
    /// argument level: when both appear, the last one wins, so the
    /// post-parse state has at most one of `friendly` / `no_friendly` set.
    #[must_use]
    pub fn friendly_request(&self) -> Option<bool> {
        if self.no_friendly {
            Some(false)
        } else if self.friendly {
            Some(true)
        } else {
            None
        }
    }

    /// Get the effective command, treating bare invocation as sync.
    ///
    /// When a subcommand is present, top-level password/sync args are merged
    /// as fallbacks so `kei --password X sync` works the same as
    /// `kei sync --password X`.
    pub fn effective_command(&self) -> Command {
        if let Some(cmd) = &self.command {
            let mut cmd = cmd.clone();
            cmd.merge_top_level_args(&self.password, &self.sync);
            cmd
        } else {
            Command::Sync {
                password: self.password.clone(),
                sync: self.sync.clone(),
            }
        }
    }
}

impl Cli {
    /// Validate that sync-only top-level flags are not combined with a
    /// non-sync subcommand.
    ///
    /// `kei` accepts a bare invocation as shorthand for `kei sync`, so flags
    /// like `--skip-videos` are wired at the top level. clap by itself will
    /// happily parse `kei --skip-videos status`: the top-level flag is
    /// consumed and silently dropped because `Status` carries no sync args.
    /// The user thinks they ran a status check with their flag honoured;
    /// they actually ran something different from what they typed.
    ///
    /// This validator runs after `Cli::parse()` and rejects any such
    /// combination, naming every offending flag in the error message.
    /// Bare invocation (no subcommand) and the `sync` subcommand
    /// legitimately use these flags and pass.
    pub fn validate(&self, explicit_sync_flags: &[&'static str]) -> Result<(), String> {
        // Bare invocation = sync alias; flags are valid.
        let Some(cmd) = &self.command else {
            return Ok(());
        };
        // Sync carries sync args directly; top-level merge into it is intended.
        // `service run` also carries SyncArgs and merges top-level flags.
        if matches!(
            cmd,
            Command::Sync { .. }
                | Command::Service {
                    action: ServiceAction::Run(..),
                }
        ) {
            return Ok(());
        }
        if explicit_sync_flags.is_empty() {
            return Ok(());
        }
        let cmd_name = subcommand_display_name(cmd);
        let flag_list = explicit_sync_flags.join(", ");
        Err(format!(
            "the following sync-only flag{plural} cannot be combined with `kei {cmd_name}`: {flag_list}\n\
             bare-kei (no subcommand) is shorthand for `kei sync`; sync-only flags are only valid there or under `kei sync`",
            plural = if explicit_sync_flags.len() == 1 {
                ""
            } else {
                "s"
            },
        ))
    }
}

/// Human-readable subcommand name for error messages.
fn subcommand_display_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Sync { .. } => "sync",
        Command::Login { .. } => "login",
        Command::List { .. } => "list",
        Command::Password { .. } => "password",
        Command::Reset { .. } => "reset",
        Command::Config { .. } => "config",
        Command::Status(_) => "status",
        Command::ImportExisting(_) => "import-existing",
        Command::Verify(_) => "verify",
        Command::Reconcile(_) => "reconcile",
        Command::Install(_) => "install",
        Command::Uninstall(_) => "uninstall",
        Command::Service { action } => match action {
            ServiceAction::Run(_) => "service run",
            ServiceAction::Status => "service status",
        },
    }
}

/// Return the set of sync-only top-level flags that the user actually
/// provided. An empty Vec means every field is at its `SyncArgs::default()`
/// value.
///
/// Used by [`Cli::validate`] to name every offending flag when the user
/// combines a non-sync subcommand with bare-kei sync flags. Each branch
/// corresponds 1:1 to a `SyncArgs` field; when adding a new sync flag,
/// extend this function so it shows up in the rejection message.
/// Return the set of sync-only top-level flags that were explicitly
/// provided on the command line (not via environment variables or
/// defaults). An empty Vec means no sync flag came from the CLI.
///
/// Used by [`Cli::validate`] to name offending flags when the user
/// combines a non-sync subcommand with bare-kei sync flags. Each branch
/// corresponds 1:1 to a `SyncArgs` field; when adding a new sync flag,
/// extend this function so it shows up in the rejection message.
fn explicit_top_level_sync_flags(matches: &clap::ArgMatches) -> Vec<&'static str> {
    use clap::parser::ValueSource;
    let mut out = Vec::new();
    if matches.value_source("recent") == Some(ValueSource::CommandLine) {
        out.push("--recent");
    }
    if matches.value_source("dry_run") == Some(ValueSource::CommandLine) {
        out.push("--dry-run");
    }
    if matches.value_source("no_progress_bar") == Some(ValueSource::CommandLine) {
        out.push("--no-progress-bar");
    }
    if matches.value_source("skip_created_before") == Some(ValueSource::CommandLine) {
        out.push("--skip-created-before");
    }
    if matches.value_source("skip_created_after") == Some(ValueSource::CommandLine) {
        out.push("--skip-created-after");
    }
    if matches.value_source("only_print_filenames") == Some(ValueSource::CommandLine) {
        out.push("--only-print-filenames");
    }
    if matches.value_source("save_password") == Some(ValueSource::CommandLine) {
        out.push("--save-password");
    }
    if matches.value_source("retry_failed") == Some(ValueSource::CommandLine) {
        out.push("--retry-failed");
    }
    out
}

/// Parse CLI arguments and return both the parsed struct and the list of
/// sync-only flags that were explicitly provided on the command line.
///
/// This is the production entry point. It replaces `Cli::parse()` so the
/// validator can distinguish between CLI-provided and env-provided flags.
pub fn parse_cli_with_sources<I, T>(itr: I) -> Result<(Cli, Vec<&'static str>), clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cmd = <Cli as clap::CommandFactory>::command();
    let matches = match cmd.try_get_matches_from(itr) {
        Ok(m) => m,
        Err(e) => e.exit(),
    };
    let explicit_sync_flags = explicit_top_level_sync_flags(&matches);
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    Ok((cli, explicit_sync_flags))
}

impl Command {
    /// Merge top-level CLI password/sync args as fallbacks into the
    /// subcommand's own args.
    fn merge_top_level_args(&mut self, top_password: &PasswordArgs, top_sync: &SyncArgs) {
        // Merge sync args for commands that carry them
        match self {
            Self::Sync { sync, .. } => {
                sync.merge_from(top_sync);
            }
            Self::Service {
                action: ServiceAction::Run(args),
            } => {
                args.sync.merge_from(top_sync);
            }
            _ => {}
        }
        // Merge password args for commands that carry them
        if let Some(pw) = self.password_args_mut() {
            pw.merge_from(top_password);
        }
    }

    /// Inject the `ICLOUD_PASSWORD` value captured before `Cli::parse()`.
    ///
    /// The env var is removed from the process environment for security
    /// (prevents leaking via `/proc/*/environ`), but clap's `env` attribute
    /// never sees it. This method restores it into whichever `PasswordArgs`
    /// the active command carries.
    pub fn inject_env_password(&mut self, env_password: Option<String>) {
        let Some(pw) = env_password else { return };
        if let Some(args) = self.password_args_mut() {
            if args.password.is_none() {
                args.password = Some(pw);
            }
        }
    }

    /// Return a mutable reference to the command's `PasswordArgs`, if any.
    fn password_args_mut(&mut self) -> Option<&mut PasswordArgs> {
        match self {
            Self::Sync { password, .. }
            | Self::Login { password, .. }
            | Self::List { password, .. }
            | Self::Password { password, .. } => Some(password),
            Self::ImportExisting(args) => Some(&mut args.password),
            Self::Service {
                action: ServiceAction::Run(args),
            } => Some(&mut args.password),
            Self::Reset { .. }
            | Self::Config { .. }
            | Self::Status(_)
            | Self::Verify(_)
            | Self::Reconcile(_)
            | Self::Install(_)
            | Self::Uninstall(_)
            | Self::Service {
                action: ServiceAction::Status,
            } => None,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::multiple_unsafe_ops_per_block,
    clippy::undocumented_unsafe_blocks,
    reason = "env var ops in tests are sequenced under a mutex — splitting/documenting adds noise"
)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    fn assert_removed_sync_flag(args: &[&str]) {
        let err = Cli::try_parse_from(args).expect_err("removed sync flag must fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    fn assert_removed_global_option(option: &str, value: &str) {
        let err = Cli::try_parse_from(["kei", option, value])
            .expect_err("removed global option must fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    fn assert_removed_sync_option(tail: &[&str]) {
        let mut args = vec!["kei"];
        args.extend_from_slice(tail);
        assert_removed_sync_flag(&args);
    }

    /// Parse argv into a `Cli` and compute the explicit sync flags list.
    /// Used by validation tests that need to distinguish CLI-provided
    /// values from env-provided ones.
    fn parse_and_validate(argv: &[&str]) -> Result<(), String> {
        let cmd = <Cli as clap::CommandFactory>::command();
        let matches = cmd
            .try_get_matches_from(argv)
            .map_err(|err| err.to_string())?;
        let explicit_sync_flags = explicit_top_level_sync_flags(&matches);
        let cli = Cli::from_arg_matches(&matches).map_err(|err| err.to_string())?;
        cli.validate(&explicit_sync_flags)
    }

    // ── RecentLimit parser ──────────────────────────────────────────

    #[test]
    fn parse_recent_limit_bare_count() {
        assert_eq!(parse_recent_limit("100").unwrap(), RecentLimit::Count(100));
        assert_eq!(parse_recent_limit("1").unwrap(), RecentLimit::Count(1));
    }

    #[test]
    fn parse_recent_limit_days_suffix() {
        assert_eq!(parse_recent_limit("30d").unwrap(), RecentLimit::Days(30));
        assert_eq!(parse_recent_limit("1d").unwrap(), RecentLimit::Days(1));
    }

    #[test]
    fn parse_recent_limit_rejects_zero() {
        assert!(parse_recent_limit("0").unwrap_err().contains("zero"));
        assert!(parse_recent_limit("0d").unwrap_err().contains("zero"));
    }

    #[test]
    fn parse_recent_limit_rejects_empty() {
        assert!(parse_recent_limit("").is_err());
    }

    #[test]
    fn parse_recent_limit_rejects_unknown_suffix() {
        // Only `d` is accepted today. Other units (w, m, y) would need
        // explicit design decisions around month/year boundaries.
        assert!(parse_recent_limit("3w").is_err());
        assert!(parse_recent_limit("1y").is_err());
        assert!(parse_recent_limit("2m").is_err());
        assert!(parse_recent_limit("30days").is_err());
    }

    #[test]
    fn parse_recent_limit_rejects_garbage() {
        assert!(parse_recent_limit("abc").is_err());
        assert!(parse_recent_limit("-5").is_err());
        assert!(parse_recent_limit("10.5").is_err());
        assert!(parse_recent_limit("10 d").is_err());
    }

    #[test]
    fn recent_limit_toml_parses_integer() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            recent: RecentLimit,
        }
        let got: Wrap = toml::from_str("recent = 100").unwrap();
        assert_eq!(got.recent, RecentLimit::Count(100));
    }

    #[test]
    fn recent_limit_toml_parses_days_string() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            recent: RecentLimit,
        }
        let got: Wrap = toml::from_str(r#"recent = "30d""#).unwrap();
        assert_eq!(got.recent, RecentLimit::Days(30));
    }

    #[test]
    fn recent_limit_toml_rejects_zero_integer() {
        #[derive(serde::Deserialize, Debug)]
        struct Wrap {
            #[allow(dead_code)]
            recent: RecentLimit,
        }
        let err = toml::from_str::<Wrap>("recent = 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("zero"), "got: {err}");
    }

    #[test]
    fn recent_limit_toml_rejects_garbage_string() {
        #[derive(serde::Deserialize, Debug)]
        struct Wrap {
            #[allow(dead_code)]
            recent: RecentLimit,
        }
        assert!(toml::from_str::<Wrap>(r#"recent = "abc""#).is_err());
    }

    #[test]
    fn recent_limit_display_roundtrip() {
        assert_eq!(RecentLimit::Count(100).to_string(), "100");
        assert_eq!(RecentLimit::Days(30).to_string(), "30d");
    }

    #[test]
    fn recent_limit_serializes_count_as_integer() {
        let json = serde_json::to_string(&RecentLimit::Count(5)).unwrap();
        assert_eq!(json, "5");
    }

    #[test]
    fn recent_limit_serializes_days_as_string() {
        let json = serde_json::to_string(&RecentLimit::Days(7)).unwrap();
        assert_eq!(json, "\"7d\"");
    }

    fn base_args() -> Vec<&'static str> {
        vec!["kei"]
    }

    /// Scrub auth-related env vars for the duration of the returned guard so
    /// tests that exercise clap's flag parsing don't get contaminated when the
    /// developer has `ICLOUD_USERNAME` / `ICLOUD_PASSWORD` exported (via
    /// `.env` sourcing for live tests). A process-wide mutex serializes
    /// concurrent calls to this helper.
    ///
    /// Note that this only protects against other callers of `scrub_auth_env`.
    /// `setenv`/`getenv` on POSIX aren't thread-safe against each other, so an
    /// unrelated test reading an env var while the guard is mutating could
    /// theoretically race. The CLI unit tests only touch these two vars via
    /// clap's `env = "..."` attributes during `try_parse_from`, which happens
    /// synchronously on one thread per test — so in practice the guard is
    /// sufficient for this suite. If a future test reads env from another
    /// thread, it needs to coordinate through the same mutex.
    fn scrub_auth_env() -> AuthEnvGuard {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_user = std::env::var("ICLOUD_USERNAME").ok();
        let prev_pw = std::env::var("ICLOUD_PASSWORD").ok();
        // SAFETY: the enclosing MutexGuard serializes every other caller of
        // scrub_auth_env, and the test suite does not read these env vars
        // from separate threads.
        unsafe {
            std::env::remove_var("ICLOUD_USERNAME");
            std::env::remove_var("ICLOUD_PASSWORD");
        }
        AuthEnvGuard {
            _lock: guard,
            prev_user,
            prev_pw,
        }
    }

    struct AuthEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_user: Option<String>,
        prev_pw: Option<String>,
    }

    impl Drop for AuthEnvGuard {
        fn drop(&mut self) {
            // SAFETY: still holding the static mutex, so restoration is
            // exclusive under the same "no cross-thread readers" condition
            // described on scrub_auth_env.
            unsafe {
                if let Some(v) = self.prev_user.take() {
                    std::env::set_var("ICLOUD_USERNAME", v);
                }
                if let Some(v) = self.prev_pw.take() {
                    std::env::set_var("ICLOUD_PASSWORD", v);
                }
            }
        }
    }

    // ── Global args ───────────────────────────────────────────────

    #[test]
    fn test_username_global_removed() {
        assert_removed_global_option("--username", "test@example.com");
    }

    #[test]
    fn test_domain_global_removed() {
        assert_removed_global_option("--domain", "cn");
    }

    #[test]
    fn test_data_dir_global_removed() {
        assert_removed_global_option("--data-dir", "/config");
    }

    #[test]
    fn test_config_flag_default() {
        let cli = parse(&base_args());
        assert_eq!(cli.config, "~/.config/kei/config.toml");
    }

    #[test]
    fn test_config_flag_custom() {
        let mut args = base_args();
        args.extend(["--config", "/etc/kei.toml"]);
        let cli = parse(&args);
        assert_eq!(cli.config, "/etc/kei.toml");
    }

    // ── Bare invocation (no subcommand = sync) ────────────────────

    #[test]
    fn test_bare_invocation_without_username() {
        let _guard = scrub_auth_env();
        let cli = Cli::try_parse_from(["kei"]).unwrap();
        assert!(cli.command.is_none());
    }
    #[test]
    fn test_backwards_compatibility_no_subcommand() {
        assert_removed_sync_option(&["--download-dir", "/photos"]);
    }

    // ── New subcommands ───────────────────────────────────────────

    #[test]
    fn test_login_subcommand() {
        let cli = Cli::try_parse_from(["kei", "login"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Login {
                subcommand: None,
                ..
            }
        ));
    }

    #[test]
    fn test_login_get_code() {
        let cli = Cli::try_parse_from(["kei", "login", "get-code"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Login {
                subcommand: Some(LoginCommand::GetCode),
                ..
            }
        ));
    }

    #[test]
    fn test_login_submit_code() {
        let cli = Cli::try_parse_from(["kei", "login", "submit-code", "123456"]).unwrap();
        match cli.effective_command() {
            Command::Login {
                subcommand: Some(LoginCommand::SubmitCode { code }),
                ..
            } => assert_eq!(code, "123456"),
            _ => panic!("Expected Login SubmitCode"),
        }
    }

    #[test]
    fn test_list_albums() {
        let cli = Cli::try_parse_from(["kei", "list", "albums"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Albums,
                ..
            }
        ));
    }

    #[test]
    fn test_list_libraries() {
        let cli = Cli::try_parse_from(["kei", "list", "libraries"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                what: ListCommand::Libraries,
                ..
            }
        ));
    }

    #[test]
    fn test_password_set() {
        let cli = Cli::try_parse_from(["kei", "password", "set"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Password {
                action: PasswordAction::Set,
                ..
            }
        ));
    }

    #[test]
    fn test_password_clear() {
        let cli = Cli::try_parse_from(["kei", "password", "clear"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Password {
                action: PasswordAction::Clear,
                ..
            }
        ));
    }

    #[test]
    fn test_password_backend() {
        let cli = Cli::try_parse_from(["kei", "password", "backend"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Password {
                action: PasswordAction::Backend,
                ..
            }
        ));
    }

    #[test]
    fn test_reset_state() {
        let cli = Cli::try_parse_from(["kei", "reset", "state", "--yes"]).unwrap();
        match cli.effective_command() {
            Command::Reset {
                what: ResetCommand::State { yes },
            } => assert!(yes),
            _ => panic!("Expected Reset State"),
        }
    }

    #[test]
    fn install_dry_run_parses_with_user_flag() {
        let cli = Cli::try_parse_from(["kei", "install", "--user", "--dry-run"]).unwrap();
        let args = match cli.effective_command() {
            Command::Install(a) => a.clone(),
            other => panic!("expected Install, got {other:?}"),
        };
        assert!(args.user);
        assert!(args.dry_run);
        assert!(!args.system);
    }

    #[test]
    fn install_user_and_system_conflict_at_parse_time() {
        // clap rejects mutually exclusive flags during parse; this guards
        // the `conflicts_with` annotation on InstallArgs.
        let err = Cli::try_parse_from(["kei", "install", "--user", "--system"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn uninstall_purge_parses_to_struct_field() {
        let cli = Cli::try_parse_from(["kei", "uninstall", "--purge"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Uninstall(UninstallArgs { purge: true })
        ));
    }

    #[test]
    fn test_reset_sync_token() {
        let cli = Cli::try_parse_from(["kei", "reset", "sync-token"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Reset {
                what: ResetCommand::SyncToken { yes: false },
            }
        ));
    }

    #[test]
    fn test_reset_sync_token_with_yes() {
        let cli = Cli::try_parse_from(["kei", "reset", "sync-token", "--yes"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Reset {
                what: ResetCommand::SyncToken { yes: true },
            }
        ));
    }

    #[test]
    fn test_reset_sync_token_with_short_y() {
        let cli = Cli::try_parse_from(["kei", "reset", "sync-token", "-y"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Reset {
                what: ResetCommand::SyncToken { yes: true },
            }
        ));
    }

    #[test]
    fn test_config_show() {
        let cli = Cli::try_parse_from(["kei", "config", "show"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Config {
                action: ConfigAction::Show,
            }
        ));
    }

    #[test]
    fn test_config_setup() {
        let cli = Cli::try_parse_from(["kei", "config", "setup"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::Config {
                action: ConfigAction::Setup { output: None },
            }
        ));
    }

    #[test]
    fn test_config_setup_with_output() {
        let cli =
            Cli::try_parse_from(["kei", "config", "setup", "-o", "/tmp/config.toml"]).unwrap();
        match cli.effective_command() {
            Command::Config {
                action: ConfigAction::Setup { output },
            } => assert_eq!(output.as_deref(), Some("/tmp/config.toml")),
            _ => panic!("Expected Config Setup"),
        }
    }
    #[test]
    fn test_sync_retry_failed_flag() {
        assert_removed_sync_flag(&["kei", "sync", "--retry-failed", "--download-dir", "/photos"]);
    }

    // ── Legacy subcommand compat ──────────────────────────────────
    #[test]
    fn test_max_download_attempts_cli_parse() {
        assert_removed_sync_flag(&["kei", "sync", "--max-download-attempts", "5"]);
    }
    #[test]
    fn test_max_download_attempts_defaults_to_none() {
        assert_removed_sync_flag(&["kei", "sync", "--max-download-attempts", "5"]);
    }

    #[test]
    fn test_password_flag() {
        let mut args = base_args();
        args.extend(["--password", "secret123"]);
        let cli = parse(&args);
        assert_eq!(cli.password.password.as_deref(), Some("secret123"));
    }

    #[test]
    fn test_password_file_flag() {
        let _guard = scrub_auth_env();
        let mut args = base_args();
        args.extend(["--password-file", "/run/secrets/pw"]);
        let cli = parse(&args);
        assert_eq!(
            cli.password.password_file.as_deref(),
            Some("/run/secrets/pw")
        );
    }

    #[test]
    fn test_password_command_flag() {
        let _guard = scrub_auth_env();
        let mut args = base_args();
        args.extend(["--password-command", "op read 'op://vault/icloud/pw'"]);
        let cli = parse(&args);
        assert_eq!(
            cli.password.password_command.as_deref(),
            Some("op read 'op://vault/icloud/pw'")
        );
    }

    #[test]
    fn test_password_conflicts_with_password_file() {
        let mut args = base_args();
        args.extend(["--password", "pw", "--password-file", "/tmp/pw.txt"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_password_conflicts_with_password_command() {
        let mut args = base_args();
        args.extend(["--password", "pw", "--password-command", "echo pw"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_password_file_conflicts_with_password_command() {
        let mut args = base_args();
        args.extend([
            "--password-file",
            "/tmp/pw",
            "--password-command",
            "echo pw",
        ]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_save_password_flag() {
        let mut args = base_args();
        args.push("--save-password");
        let cli = parse(&args);
        assert!(cli.sync.save_password);
    }

    #[test]
    fn test_save_password_merges_into_subcommand() {
        let cli = parse(&["kei", "--save-password", "sync"]);
        if let Command::Sync { sync, .. } = cli.effective_command() {
            assert!(sync.save_password);
        } else {
            panic!("expected Sync command");
        }
    }

    // ── Sync args ─────────────────────────────────────────────────
    #[test]
    fn test_library_accepts_custom_value() {
        assert_removed_sync_flag(&["kei", "--library", "SharedSync-ABCD1234"]);
    }
    #[test]
    fn test_library_repeatable_with_sentinels() {
        assert_removed_sync_flag(&["kei", "--library", "primary", "--library", "shared"]);
    }
    #[test]
    fn test_bandwidth_limit_bare_bytes() {
        assert_removed_sync_option(&["--bandwidth-limit", "1024"]);
    }
    #[test]
    fn test_bandwidth_limit_decimal_suffixes() {
        assert_removed_sync_option(&["--bandwidth-limit", "1.5M"]);
    }

    #[test]
    fn test_bandwidth_limit_binary_suffix() {
        assert_removed_sync_option(&["--bandwidth-limit", "2Mi"]);
    }

    #[test]
    fn test_bandwidth_limit_case_insensitive() {
        assert_eq!(parse_bandwidth_limit("500k"), Ok(500_000));
        assert_eq!(parse_bandwidth_limit("500K"), Ok(500_000));
        assert_eq!(parse_bandwidth_limit("1gib"), Ok(1024 * 1024 * 1024));
    }

    #[test]
    fn test_bandwidth_limit_rejects_zero() {
        let mut args = base_args();
        args.extend(["--bandwidth-limit", "0"]);
        assert!(Cli::try_parse_from(&args).is_err());
        assert!(parse_bandwidth_limit("0K").is_err());
    }

    #[test]
    fn test_bandwidth_limit_rejects_invalid() {
        assert!(parse_bandwidth_limit("").is_err());
        assert!(parse_bandwidth_limit("abc").is_err());
        assert!(parse_bandwidth_limit("10X").is_err());
        assert!(parse_bandwidth_limit("-5M").is_err());
        // 1.5M is NOW accepted (see test_bandwidth_limit_decimal_*).
        // Malformed decimals like `1..5M` or `1.5.5M` stay rejected.
        assert!(parse_bandwidth_limit("1..5M").is_err());
        assert!(parse_bandwidth_limit("1.5.5M").is_err());
    }

    // ── Decimal bandwidth values ───────────────────────────────────

    #[test]
    fn test_bandwidth_limit_decimal_mb() {
        assert_eq!(parse_bandwidth_limit("1.5M"), Ok(1_500_000));
        assert_eq!(parse_bandwidth_limit("2.5G"), Ok(2_500_000_000));
        assert_eq!(parse_bandwidth_limit("0.5K"), Ok(500));
    }

    #[test]
    fn test_bandwidth_limit_decimal_binary() {
        // 2.5 * 1_048_576 = 2_621_440
        assert_eq!(parse_bandwidth_limit("2.5Mi"), Ok(2_621_440));
        // 1.5 * 1024 = 1536
        assert_eq!(parse_bandwidth_limit("1.5Ki"), Ok(1_536));
    }

    #[test]
    fn test_bandwidth_limit_decimal_leading_dot() {
        assert_eq!(parse_bandwidth_limit(".5M"), Ok(500_000));
        assert_eq!(parse_bandwidth_limit(".25K"), Ok(250));
    }

    #[test]
    fn test_bandwidth_limit_decimal_trailing_dot() {
        // Trailing dot means integer-valued decimal - `1.M` is 1.0 MB/s.
        assert_eq!(parse_bandwidth_limit("1.M"), Ok(1_000_000));
    }

    #[test]
    fn test_bandwidth_limit_decimal_rounds_to_zero_rejected() {
        // 0.0001 * 1000 = 0.1 bytes/sec, rounds to 0 - reject so users
        // don't think kei is throttling when it effectively isn't.
        let err = parse_bandwidth_limit("0.0001K").unwrap_err();
        assert!(err.contains("rounds to zero"), "{err}");
        // 0.4 bare bytes/sec rounds to 0 too.
        assert!(parse_bandwidth_limit("0.4").is_err());
    }

    #[test]
    fn test_bandwidth_limit_decimal_rounds_to_nearest_byte() {
        // 0.1 * 1000 = 100 in theory, 99.99999... in f64. Round to 100.
        assert_eq!(parse_bandwidth_limit("0.1K"), Ok(100));
    }

    #[test]
    fn test_bandwidth_limit_rejects_special_floats() {
        // f64::parse accepts "nan", "inf", "infinity" - but these make no
        // sense as a bandwidth value.
        assert!(parse_bandwidth_limit("nanK").is_err());
        assert!(parse_bandwidth_limit("infM").is_err());
        assert!(parse_bandwidth_limit("inf").is_err());
    }
    #[test]
    fn test_max_retries_custom() {
        assert_removed_sync_option(&["--max-retries", "10"]);
    }
    #[test]
    fn test_max_retries_zero_disables() {
        assert_removed_sync_option(&["--max-retries", "0"]);
    }
    #[test]
    fn test_temp_suffix_custom() {
        assert_removed_sync_option(&["--temp-suffix", ".downloading"]);
    }
    #[test]
    fn test_skip_videos_flag() {
        assert_removed_sync_option(&["--skip-videos"]);
    }

    #[test]
    fn test_unfiled_flag_default_none() {
        let cli = parse(&base_args());
        assert_eq!(cli.sync.config_overrides.unfiled, None);
    }
    #[test]
    fn test_unfiled_flag_bare_true() {
        assert_removed_sync_option(&["--unfiled"]);
    }
    #[test]
    fn test_unfiled_flag_explicit_false() {
        assert_removed_sync_option(&["--unfiled", "false"]);
    }

    #[test]
    fn test_unfiled_flag_explicit_true() {
        assert_removed_sync_option(&["--unfiled", "true"]);
    }

    #[test]
    fn test_skip_videos_explicit_false() {
        assert_removed_sync_option(&["--skip-videos", "false"]);
    }
    #[test]
    fn test_skip_photos_flag() {
        assert_removed_sync_option(&["--skip-photos"]);
    }
    #[test]
    fn test_force_size_flag() {
        assert_removed_sync_option(&["--force-size"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_set_exif_datetime_flag() {
        assert_removed_sync_option(&["--set-exif-datetime"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_embed_xmp_flag() {
        assert_removed_sync_option(&["--embed-xmp"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_embed_xmp_flag_explicit_false() {
        assert_removed_sync_option(&["--embed-xmp=false"]);
    }
    #[cfg(feature = "xmp")]
    #[test]
    fn test_xmp_sidecar_flag() {
        assert_removed_sync_option(&["--xmp-sidecar"]);
    }

    #[test]
    fn test_no_progress_bar_flag() {
        let mut args = base_args();
        args.push("--no-progress-bar");
        let cli = parse(&args);
        assert_eq!(cli.sync.no_progress_bar, Some(true));
    }
    #[test]
    fn test_keep_unicode_in_filenames_flag() {
        assert_removed_sync_option(&["--keep-unicode-in-filenames"]);
    }
    #[test]
    fn test_notify_systemd_flag() {
        assert_removed_sync_option(&["--notify-systemd"]);
    }
    #[test]
    fn test_pid_file_flag() {
        assert_removed_sync_option(&["--pid-file", "/tmp/kei.pid"]);
    }
    #[test]
    fn test_reconcile_every_n_cycles_flag() {
        assert_removed_sync_option(&["--reconcile-every-n-cycles", "24"]);
    }

    #[test]
    fn test_reconcile_every_n_cycles_default_unset() {
        let cli = parse(&base_args());
        assert!(cli.sync.config_overrides.reconcile_every_n_cycles.is_none());
    }

    // 0 is "off" via TOML (or absence); the CLI flag rejects it so users
    // omit the flag instead of passing a magic value. Anything else <0 or
    // non-numeric also fails clap's range parser.
    #[test]
    fn test_reconcile_every_n_cycles_rejects_zero() {
        assert_removed_sync_option(&["--reconcile-every-n-cycles", "0"]);
    }
    #[test]
    fn test_notification_script_flag() {
        assert_removed_sync_option(&["--notification-script", "/tmp/notify.sh"]);
    }
    #[test]
    fn test_report_json_flag() {
        assert_removed_sync_option(&["--report-json", "/tmp/run.json"]);
    }

    // ── Enum variants ──────────────────────────────────────────────
    #[test]
    fn test_size_all_variants() {
        assert_removed_sync_option(&["--size", "medium"]);
    }
    #[test]
    fn test_live_photo_size_all_variants() {
        assert_removed_sync_option(&["--live-photo-size", "medium"]);
    }
    #[test]
    fn test_live_photo_mov_filename_policy_all_variants() {
        assert_removed_sync_option(&["--live-photo-mov-filename-policy", "original"]);
    }
    #[test]
    fn test_align_raw_all_variants() {
        assert_removed_sync_option(&["--align-raw", "prefer-original"]);
    }

    #[test]
    fn test_align_raw_rejects_invalid() {
        let mut args = base_args();
        args.extend(["--align-raw", "bogus"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }
    #[test]
    fn test_file_match_policy_all_variants() {
        assert_removed_sync_option(&["--file-match-policy", "name-id7"]);
    }

    #[test]
    fn test_log_level_all_variants() {
        for (input, expected) in [
            ("debug", LogLevel::Debug),
            ("info", LogLevel::Info),
            ("warn", LogLevel::Warn),
            ("error", LogLevel::Error),
        ] {
            let mut args = base_args();
            args.extend(["--log-level", input]);
            let cli = parse(&args);
            assert_eq!(cli.log_level, Some(expected), "log_level variant: {input}");
        }
    }

    // ── Optional value flags ───────────────────────────────────────
    #[test]
    fn test_folder_structure_custom() {
        assert_removed_sync_option(&["--folder-structure", "%Y-%m"]);
    }
    #[test]
    fn test_download_dir_custom() {
        assert_removed_sync_option(&["--download-dir", "/photos"]);
    }
    #[test]
    fn test_watch_with_interval() {
        assert_removed_sync_option(&["--watch-with-interval", "3600"]);
    }
    #[test]
    fn test_watch_with_interval_rejects_below_minimum() {
        assert_removed_sync_option(&["--watch-with-interval", "59"]);
    }
    #[test]
    fn test_watch_with_interval_rejects_above_maximum() {
        assert_removed_sync_option(&["--watch-with-interval", "86401"]);
    }

    #[test]
    fn test_skip_created_before() {
        let mut args = base_args();
        args.extend(["--skip-created-before", "2024-01-01"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_created_before.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn test_skip_created_after() {
        let mut args = base_args();
        args.extend(["--skip-created-after", "2025-06-01"]);
        let cli = parse(&args);
        assert_eq!(cli.sync.skip_created_after.as_deref(), Some("2025-06-01"));
    }
    #[test]
    fn test_albums_multiple() {
        assert_removed_sync_option(&["--album", "Favorites", "--album", "Vacation"]);
    }

    #[test]
    fn test_albums_empty_by_default() {
        let cli = parse(&base_args());
        assert!(cli.sync.config_overrides.albums.is_empty());
    }
    #[test]
    fn test_album_all_accepted() {
        assert_removed_sync_flag(&["kei", "-a", "all"]);
    }

    // ── Input validation ───────────────────────────────────────────

    #[test]
    fn test_empty_username_rejected() {
        assert_removed_global_option("--username", "");
    }

    #[test]
    fn test_empty_password_rejected() {
        let mut args = base_args();
        args.extend(["--password", ""]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_empty_download_dir_rejected() {
        assert_removed_sync_option(&["--download-dir", ""]);
    }

    #[test]
    fn test_empty_album_rejected() {
        let mut args = base_args();
        args.extend(["--album", ""]);
        assert!(Cli::try_parse_from(&args).is_err());
    }

    #[test]
    fn test_submit_code_rejects_non_digits() {
        assert!(Cli::try_parse_from(["kei", "submit-code", "abcdef"]).is_err());
    }

    #[test]
    fn test_submit_code_rejects_too_short() {
        assert!(Cli::try_parse_from(["kei", "submit-code", "12345"]).is_err());
    }

    #[test]
    fn test_submit_code_rejects_too_long() {
        assert!(Cli::try_parse_from(["kei", "submit-code", "1234567"]).is_err());
    }

    #[test]
    fn test_submit_code_rejects_empty() {
        assert!(Cli::try_parse_from(["kei", "submit-code", ""]).is_err());
    }

    #[test]
    fn test_max_retries_rejects_above_100() {
        let mut args = base_args();
        args.extend(["--max-retries", "101"]);
        assert!(Cli::try_parse_from(&args).is_err());
    }
    #[test]
    fn test_max_retries_accepts_100() {
        assert_removed_sync_option(&["--max-retries", "100"]);
    }
    #[test]
    fn test_sync_subcommand() {
        let cli = Cli::try_parse_from(["kei", "sync"]).unwrap();
        assert!(matches!(cli.effective_command(), Command::Sync { .. }));
    }

    #[test]
    fn test_status_subcommand() {
        let cli = Cli::try_parse_from(["kei", "status"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status(_))));
    }

    #[test]
    fn test_status_with_failed_flag() {
        let cli = Cli::try_parse_from(["kei", "status", "--failed"]).unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(args.failed);
            assert!(!args.pending);
            assert!(!args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_status_with_pending_flag() {
        let cli = Cli::try_parse_from(["kei", "status", "--pending"]).unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(!args.failed);
            assert!(args.pending);
            assert!(!args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_status_with_downloaded_flag() {
        let cli = Cli::try_parse_from(["kei", "status", "--downloaded"]).unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(!args.failed);
            assert!(!args.pending);
            assert!(args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    #[test]
    fn test_status_flags_combine() {
        let cli = Cli::try_parse_from(["kei", "status", "--failed", "--pending", "--downloaded"])
            .unwrap();
        if let Some(Command::Status(args)) = cli.command {
            assert!(args.failed);
            assert!(args.pending);
            assert!(args.downloaded);
        } else {
            panic!("Expected Status command");
        }
    }

    // ── Global flags work with subcommands ────────────────────────

    #[test]
    fn test_config_global_before_subcommand() {
        let cli =
            Cli::try_parse_from(["kei", "--config", "/custom/config.toml", "status"]).unwrap();
        assert_eq!(cli.config, "/custom/config.toml");
        assert!(matches!(cli.command, Some(Command::Status(_))));
    }

    #[test]
    fn test_username_global_before_subcommand_removed() {
        let err = Cli::try_parse_from(["kei", "--username", "test@example.com", "sync"])
            .expect_err("removed global option must fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    // ── import-existing ───────────────────────────────────────────

    #[test]
    fn test_import_existing_subcommand() {
        let cli =
            Cli::try_parse_from(["kei", "import-existing", "--download-dir", "/photos"]).unwrap();
        if let Some(Command::ImportExisting(args)) = cli.command {
            assert_eq!(args.download_dir.as_deref(), Some("/photos"));

            assert!(args.folder_structure.is_none());
            assert!(args.recent.is_none());
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    #[test]
    fn test_import_existing_library_flag_single() {
        let cli = Cli::try_parse_from([
            "kei",
            "import-existing",
            "--library",
            "SharedSync-ABCD1234",
            "--download-dir",
            "/photos",
        ])
        .unwrap();
        if let Some(Command::ImportExisting(args)) = cli.command {
            assert_eq!(args.libraries, vec!["SharedSync-ABCD1234".to_string()]);
        } else {
            panic!("Expected ImportExisting command");
        }
    }

    /// Parity check with `kei sync --library`: import-existing's flag is also
    /// repeatable and accepts mixed sentinels, zone names, and `!name`
    /// exclusions in one invocation. Pre-v0.13 the flag was a single
    /// `Option<String>` and a second `--library` silently won, so
    /// multi-library import had to be configured via TOML.
    #[test]
    fn test_import_existing_library_flag_repeatable_with_mixed_grammar() {
        let cli = Cli::try_parse_from([
            "kei",
            "import-existing",
            "--library",
            "primary",
            "--library",
            "SharedSync-ABCD1234",
            "--library",
            "!SharedSync-Photos",
            "--download-dir",
            "/photos",
        ])
        .unwrap();
        let Some(Command::ImportExisting(args)) = cli.command else {
            panic!("Expected ImportExisting command");
        };
        assert_eq!(
            args.libraries,
            vec![
                "primary".to_string(),
                "SharedSync-ABCD1234".to_string(),
                "!SharedSync-Photos".to_string(),
            ]
        );
    }

    // ── import-existing path-derivation flags ──────────────────────────
    //
    // Each flag here changes how `expected_paths_for` derives the on-disk
    // path. A regression in the clap value_parser (e.g. mapping `medium` to
    // `Original`) is silent unless the parsed variant is asserted -- a
    // `--help`-driven smoke test catches "spelling vanished" but not
    // "spelling reaches the wrong variant". These tests pin the
    // CLI-string -> enum mapping for every accepted value.

    fn parse_import(extra: &[&str]) -> ImportArgs {
        let mut args = vec!["kei", "import-existing", "--download-dir", "/tmp"];
        args.extend(extra.iter().copied());
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Some(Command::ImportExisting(a)) => a,
            _ => panic!("Expected ImportExisting command"),
        }
    }

    #[test]
    fn import_existing_size_flag_parses_to_correct_variant() {
        for (input, expected) in [
            ("original", VersionSize::Original),
            ("medium", VersionSize::Medium),
            ("thumb", VersionSize::Thumb),
            ("adjusted", VersionSize::Adjusted),
            ("alternative", VersionSize::Alternative),
        ] {
            let args = parse_import(&["--size", input]);
            assert_eq!(args.size, Some(expected), "size={input}");
        }
    }

    #[test]
    fn import_existing_live_photo_mode_parses_to_correct_variant() {
        for (input, expected) in [
            ("both", LivePhotoMode::Both),
            ("image-only", LivePhotoMode::ImageOnly),
            ("video-only", LivePhotoMode::VideoOnly),
            ("skip", LivePhotoMode::Skip),
        ] {
            let args = parse_import(&["--live-photo-mode", input]);
            assert_eq!(args.live_photo_mode, Some(expected), "mode={input}");
        }
    }

    #[test]
    fn import_existing_live_photo_size_parses_to_correct_variant() {
        for (input, expected) in [
            ("original", LivePhotoSize::Original),
            ("medium", LivePhotoSize::Medium),
            ("thumb", LivePhotoSize::Thumb),
            ("adjusted", LivePhotoSize::Adjusted),
        ] {
            let args = parse_import(&["--live-photo-size", input]);
            assert_eq!(args.live_photo_size, Some(expected), "size={input}");
        }
    }

    #[test]
    fn import_existing_live_photo_mov_filename_policy_parses_to_correct_variant() {
        for (input, expected) in [
            ("suffix", LivePhotoMovFilenamePolicy::Suffix),
            ("original", LivePhotoMovFilenamePolicy::Original),
        ] {
            let args = parse_import(&["--live-photo-mov-filename-policy", input]);
            assert_eq!(
                args.live_photo_mov_filename_policy,
                Some(expected),
                "policy={input}"
            );
        }
    }

    #[test]
    fn import_existing_align_raw_parses_to_correct_variant() {
        for (input, expected) in [
            ("as-is", RawTreatmentPolicy::Unchanged),
            ("original", RawTreatmentPolicy::PreferOriginal),
            ("alternative", RawTreatmentPolicy::PreferAlternative),
        ] {
            let args = parse_import(&["--align-raw", input]);
            assert_eq!(args.align_raw, Some(expected), "policy={input}");
        }
    }

    #[test]
    fn import_existing_force_size_flag_parses_to_true() {
        let args = parse_import(&["--force-size"]);
        assert_eq!(args.force_size, Some(true));
    }

    #[test]
    fn import_existing_force_empty_flag_parses_to_true() {
        let _guard = scrub_auth_env();
        // SAFETY: scrub_auth_env serializes against the env_var test that
        // also mutates KEI_FORCE_EMPTY. Clearing here protects against a
        // developer shell that has KEI_FORCE_EMPTY=true exported.
        unsafe {
            std::env::remove_var("KEI_FORCE_EMPTY");
        }
        let args = parse_import(&["--force-empty"]);
        assert!(args.force_empty);
    }

    #[test]
    fn import_existing_force_empty_default_is_false() {
        let _guard = scrub_auth_env();
        // SAFETY: same rationale as the flag test above.
        unsafe {
            std::env::remove_var("KEI_FORCE_EMPTY");
        }
        let args = parse_import(&[]);
        assert!(!args.force_empty);
    }

    #[test]
    fn import_existing_force_empty_env_var_parses_to_true() {
        let _guard = scrub_auth_env();
        // SAFETY: scrub_auth_env serializes against any other test that
        // mutates these env vars; KEI_FORCE_EMPTY is read synchronously by
        // clap during parse below.
        unsafe {
            std::env::set_var("KEI_FORCE_EMPTY", "true");
        }
        let cli =
            Cli::try_parse_from(["kei", "import-existing", "--download-dir", "/tmp"]).unwrap();
        unsafe {
            std::env::remove_var("KEI_FORCE_EMPTY");
        }
        match cli.command {
            Some(Command::ImportExisting(args)) => assert!(args.force_empty),
            _ => panic!("Expected ImportExisting command"),
        }
    }

    #[test]
    fn import_existing_keep_unicode_flag_parses_to_true() {
        let args = parse_import(&["--keep-unicode-in-filenames"]);
        assert_eq!(args.keep_unicode_in_filenames, Some(true));
    }

    #[test]
    fn import_existing_keep_unicode_env_var_parses_to_true() {
        let _guard = scrub_auth_env();
        // SAFETY: scrub_auth_env serializes against any other test that
        // mutates these env vars; KEI_KEEP_UNICODE_IN_FILENAMES is read
        // synchronously by clap during parse below.
        unsafe {
            std::env::set_var("KEI_KEEP_UNICODE_IN_FILENAMES", "true");
        }
        let cli =
            Cli::try_parse_from(["kei", "import-existing", "--download-dir", "/tmp"]).unwrap();
        unsafe {
            std::env::remove_var("KEI_KEEP_UNICODE_IN_FILENAMES");
        }
        match cli.command {
            Some(Command::ImportExisting(args)) => {
                assert_eq!(args.keep_unicode_in_filenames, Some(true));
            }
            _ => panic!("Expected ImportExisting command"),
        }
    }

    #[test]
    fn import_existing_file_match_policy_parses_to_correct_variant() {
        for (input, expected) in [
            (
                "name-size-dedup-with-suffix",
                FileMatchPolicy::NameSizeDedupWithSuffix,
            ),
            ("name-id7", FileMatchPolicy::NameId7),
        ] {
            let args = parse_import(&["--file-match-policy", input]);
            assert_eq!(args.file_match_policy, Some(expected), "policy={input}");
        }
    }

    #[test]
    fn test_list_albums_library_flag() {
        let cli = Cli::try_parse_from(["kei", "list", "--library", "all", "albums"]).unwrap();
        assert!(matches!(
            cli.effective_command(),
            Command::List {
                ref libraries,
                what: ListCommand::Albums,
                ..
            } if libraries == &vec!["all".to_string()]
        ));
    }

    /// Parity check with `kei sync --library`: list's flag is also
    /// repeatable. Pre-fix it was `Option<String>` and a second
    /// `--library` silently won, so multi-value users had no way to
    /// list across both primary and a specific shared library.
    #[test]
    fn test_list_albums_library_flag_repeatable() {
        let cli = Cli::try_parse_from([
            "kei",
            "list",
            "--library",
            "primary",
            "--library",
            "SharedSync-ABCD1234",
            "albums",
        ])
        .unwrap();
        match cli.effective_command() {
            Command::List {
                libraries,
                what: ListCommand::Albums,
                ..
            } => {
                assert_eq!(
                    libraries,
                    vec!["primary".to_string(), "SharedSync-ABCD1234".to_string()]
                );
            }
            other => panic!("expected Command::List, got {other:?}"),
        }
    }

    /// `--library` accepts the v0.13 `!name` exclusion sentinel.
    #[test]
    fn test_list_albums_library_flag_exclusion() {
        let cli = Cli::try_parse_from([
            "kei",
            "list",
            "--library",
            "all",
            "--library",
            "!SharedSync-ABCD1234",
            "albums",
        ])
        .unwrap();
        match cli.effective_command() {
            Command::List {
                libraries,
                what: ListCommand::Albums,
                ..
            } => {
                assert_eq!(
                    libraries,
                    vec!["all".to_string(), "!SharedSync-ABCD1234".to_string()]
                );
            }
            other => panic!("expected Command::List, got {other:?}"),
        }
    }

    /// `kei list libraries` also accepts the same flag grammar; the
    /// flag is parsed regardless of subcommand and is ignored on the
    /// libraries side (it's documented as albums-only).
    #[test]
    fn test_list_libraries_library_flag_repeatable() {
        let cli = Cli::try_parse_from([
            "kei",
            "list",
            "--library",
            "primary",
            "--library",
            "shared",
            "libraries",
        ])
        .unwrap();
        match cli.effective_command() {
            Command::List {
                libraries,
                what: ListCommand::Libraries,
                ..
            } => {
                assert_eq!(libraries, vec!["primary".to_string(), "shared".to_string()]);
            }
            other => panic!("expected Command::List, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_subcommand() {
        let cli = Cli::try_parse_from(["kei", "verify", "--checksums"]).unwrap();
        if let Some(Command::Verify(args)) = cli.command {
            assert!(args.checksums);
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_reconcile_subcommand() {
        let cli = Cli::try_parse_from(["kei", "reconcile"]).unwrap();
        if let Some(Command::Reconcile(args)) = cli.command {
            assert!(!args.dry_run);
        } else {
            panic!("Expected Reconcile command");
        }
    }

    #[test]
    fn test_reconcile_dry_run_flag() {
        let cli = Cli::try_parse_from(["kei", "reconcile", "--dry-run"]).unwrap();
        if let Some(Command::Reconcile(args)) = cli.command {
            assert!(args.dry_run);
        } else {
            panic!("Expected Reconcile command");
        }
    }

    // ── New filter flags ───────────────────────────────────────────
    #[test]
    fn test_live_photo_mode_all_variants() {
        assert_removed_sync_option(&["--live-photo-mode", "skip"]);
    }

    #[test]
    fn test_filename_exclude_single() {
        assert_removed_sync_option(&["--filename-exclude", "*.AAE"]);
    }
    #[test]
    fn test_filename_exclude_multiple() {
        assert_removed_sync_option(&["--filename-exclude", "*.AAE,Screenshot*"]);
    }

    #[test]
    fn test_exclude_album_rejected() {
        let mut args = base_args();
        args.extend(["--exclude-album", "Hidden"]);
        assert!(Cli::try_parse_from(args).is_err());
    }

    // ── Cli::validate: bare-kei sync flags + non-sync subcommand ──
    //
    // Regression: clap silently consumed a top-level sync flag and ran
    // whatever subcommand the user named. `kei --skip-videos status`
    // looked like `kei status`; the user thought their flag was honoured
    // and saw a different action than they typed.
    #[test]
    fn validate_rejects_skip_videos_with_status() {
        let err = Cli::try_parse_from(["kei", "--skip-videos=true", "status"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
    #[test]
    fn validate_rejects_skip_photos_with_list_albums() {
        let err = Cli::try_parse_from(["kei", "--skip-photos=true", "list", "albums"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
    #[test]
    fn validate_rejects_live_photo_mode_with_reset_state() {
        let err = Cli::try_parse_from([
            "kei",
            "--live-photo-mode",
            "skip",
            "reset",
            "state",
            "--yes",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn validate_rejects_value_flag_with_verify() {
        let err = parse_and_validate(&["kei", "--download-dir", "/photos", "verify"])
            .expect_err("validation must reject");
        assert!(err.contains("--download-dir"), "got: {err}");
    }

    #[test]
    fn validate_rejects_threads_with_reconcile() {
        let err = parse_and_validate(&["kei", "--threads", "4", "reconcile"])
            .expect_err("validation must reject");
        assert!(err.contains("--threads"), "got: {err}");
    }

    #[test]
    fn validate_rejects_dry_run_with_status() {
        let err = parse_and_validate(&["kei", "--dry-run", "status"])
            .expect_err("validation must reject");
        assert!(err.contains("--dry-run"), "got: {err}");
    }

    #[test]
    fn validate_rejects_album_with_password_set() {
        let err = parse_and_validate(&["kei", "--album", "Vacation", "password", "set"])
            .expect_err("validation must reject");
        assert!(err.contains("--album"), "got: {err}");
    }

    #[test]
    fn removed_auth_only_flag_is_rejected() {
        let err = parse_and_validate(&["kei", "--auth-only", "status"])
            .expect_err("parse must reject removed flag");
        assert!(err.contains("--auth-only"), "got: {err}");
    }
    #[test]
    fn validate_lists_every_offending_flag() {
        let err = parse_and_validate(&[
            "kei",
            "--dry-run",
            "--only-print-filenames",
            "--recent",
            "100",
            "status",
        ])
        .expect_err("validation must reject");
        assert!(err.contains("--dry-run"), "got: {err}");
        assert!(err.contains("--only-print-filenames"), "got: {err}");
        assert!(err.contains("--recent"), "got: {err}");
    }

    // ── Validator must NOT fire on legitimate uses ─────────────────
    #[test]
    fn validate_allows_bare_kei_with_sync_flags() {
        parse_and_validate(&["kei", "--recent", "100"])
            .expect("bare-kei with kept per-run sync flag must validate");
    }
    #[test]
    fn validate_allows_sync_subcommand_with_sync_flags() {
        parse_and_validate(&[
            "kei",
            "sync",
            "--recent",
            "100",
            "--skip-created-before",
            "2025-01-01",
        ])
        .expect("explicit sync subcommand with kept sync flags must validate");
    }
    #[test]
    fn validate_allows_top_level_sync_flag_then_sync_subcommand() {
        parse_and_validate(&["kei", "--recent", "100", "sync"])
            .expect("top-level kept sync flag with sync subcommand must validate");
    }
    #[test]
    fn validate_allows_retry_failed_with_sync_flag() {
        parse_and_validate(&["kei", "sync", "--retry-failed", "--recent", "100"])
            .expect("sync --retry-failed accepts kept per-run sync flags");
    }

    #[test]
    fn validate_allows_kept_global_flags_with_non_sync_subcommand() {
        parse_and_validate(&["kei", "--log-level", "debug", "status"])
            .expect("global flags must validate with any subcommand");
    }

    #[test]
    fn validate_allows_status_with_no_top_level_flags() {
        parse_and_validate(&["kei", "status"]).expect("plain `kei status` must validate");
    }
    #[test]
    fn validate_allows_service_run_with_sync_flags() {
        parse_and_validate(&["kei", "--recent", "100", "service", "run", "--dry-run"])
            .expect("service run must accept kept top-level sync flags");
    }

    #[test]
    fn validate_rejects_service_status_with_sync_flags() {
        let err = parse_and_validate(&["kei", "--download-dir", "/photos", "service", "status"])
            .expect_err("validation must reject sync flags with service status");
        assert!(err.contains("--download-dir"), "got: {err}");
    }

    // Parametric coverage so this stays green when new sync-only flags
    // are added: every flag in this list must (a) be parseable at the
    // top level, and (b) be rejected when combined with `status`.
    //
    // Boolean flags declared with `num_args = 0..=1` must use the `=value`
    // form when followed by a subcommand, otherwise clap eats the
    // subcommand name as the flag's value.
    #[test]
    fn validate_rejects_each_sync_only_flag_with_status() {
        let cases: &[(&str, &[&str])] = &[
            ("skip_videos", &["--skip-videos=true"]),
            ("skip_photos", &["--skip-photos=true"]),
            ("unfiled", &["--unfiled=true"]),
            ("force_size", &["--force-size=true"]),
            ("no_progress_bar", &["--no-progress-bar=true"]),
            ("keep_unicode", &["--keep-unicode-in-filenames=true"]),
            ("notify_systemd", &["--notify-systemd=true"]),
            ("dry_run", &["--dry-run"]),
            ("only_print_filenames", &["--only-print-filenames"]),
            ("save_password", &["--save-password"]),
            ("retry_failed", &["--retry-failed"]),
            ("download_dir", &["--download-dir", "/photos"]),
            ("album", &["--album", "Vacation"]),
            ("smart_folder", &["--smart-folder", "Favorites"]),
            ("filename_exclude", &["--filename-exclude", "*.AAE"]),
            ("library", &["--library", "primary"]),
            ("size", &["--size", "original"]),
            ("live_photo_size", &["--live-photo-size", "original"]),
            ("recent", &["--recent", "100"]),
            ("threads", &["--threads", "4"]),
            ("bandwidth_limit", &["--bandwidth-limit", "10M"]),
            ("live_photo_mode", &["--live-photo-mode", "skip"]),
            ("folder_structure", &["--folder-structure", "%Y/%m"]),
            (
                "folder_structure_albums",
                &["--folder-structure-albums", "{album}"],
            ),
            (
                "folder_structure_smart_folders",
                &["--folder-structure-smart-folders", "{smart-folder}"],
            ),
            ("watch_with_interval", &["--watch-with-interval", "3600"]),
            (
                "live_photo_mov_filename_policy",
                &["--live-photo-mov-filename-policy", "original"],
            ),
            ("align_raw", &["--align-raw", "original"]),
            (
                "file_match_policy",
                &["--file-match-policy", "name-size-dedup-with-suffix"],
            ),
            (
                "skip_created_before",
                &["--skip-created-before", "2025-01-01"],
            ),
            (
                "skip_created_after",
                &["--skip-created-after", "2025-12-31"],
            ),
            ("max_retries", &["--max-retries", "3"]),
            ("temp_suffix", &["--temp-suffix", ".part"]),
            ("pid_file", &["--pid-file", "/tmp/kei.pid"]),
            (
                "reconcile_every_n_cycles",
                &["--reconcile-every-n-cycles", "24"],
            ),
            (
                "notification_script",
                &["--notification-script", "/tmp/notify.sh"],
            ),
            ("report_json", &["--report-json", "/tmp/report.json"]),
            ("http_port", &["--http-port", "9090"]),
            ("http_bind", &["--http-bind", "127.0.0.1"]),
            ("max_download_attempts", &["--max-download-attempts", "5"]),
        ];
        for (name, args) in cases {
            let mut argv: Vec<&str> = vec!["kei"];
            argv.extend_from_slice(args);
            argv.push("status");
            let result = parse_and_validate(&argv);
            assert!(
                result.is_err(),
                "validate() must reject sync-only flag `{name}` ({args:?}) with status; got {result:?}"
            );
        }
    }

    // ── friendly_request() resolution ───────────────────────────────────
    //
    // The helper distils the `--friendly` / `--no-friendly` pair into the
    // tristate `lib.rs` actually consumes. Tests here prove clap's
    // `overrides_with` wiring matches the documented behaviour: last flag
    // wins when both appear, neither set yields `None`.

    #[test]
    fn friendly_request_none_when_neither_flag() {
        let cli = parse(&["kei", "status"]);
        assert_eq!(cli.friendly_request(), None);
    }

    #[test]
    fn friendly_request_some_true_with_friendly() {
        let cli = parse(&["kei", "--friendly", "status"]);
        assert_eq!(cli.friendly_request(), Some(true));
    }

    #[test]
    fn friendly_request_some_false_with_no_friendly() {
        let cli = parse(&["kei", "--no-friendly", "status"]);
        assert_eq!(cli.friendly_request(), Some(false));
    }

    #[test]
    fn friendly_request_last_wins_no_friendly_after_friendly() {
        let cli = parse(&["kei", "--friendly", "--no-friendly", "status"]);
        assert_eq!(
            cli.friendly_request(),
            Some(false),
            "--no-friendly after --friendly must win via overrides_with"
        );
    }

    #[test]
    fn friendly_request_last_wins_friendly_after_no_friendly() {
        let cli = parse(&["kei", "--no-friendly", "--friendly", "status"]);
        assert_eq!(
            cli.friendly_request(),
            Some(true),
            "--friendly after --no-friendly must win via overrides_with"
        );
    }
}
