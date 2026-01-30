use clap::Parser;
use crate::types::*;

#[derive(Parser, Debug)]
#[command(name = "icloudpd-rs", about = "Download iCloud photos and videos")]
pub struct Cli {
    /// Apple ID email address
    #[arg(short = 'u', long)]
    pub username: String,

    /// iCloud password (if not provided, will prompt).
    /// WARNING: passing via --password is visible in process listings.
    /// Prefer the ICLOUD_PASSWORD environment variable instead.
    #[arg(short = 'p', long, env = "ICLOUD_PASSWORD")]
    pub password: Option<String>,

    /// Local directory for downloads
    #[arg(short = 'd', long)]
    pub directory: Option<String>,

    /// Only authenticate (create/update session tokens)
    #[arg(long)]
    pub auth_only: bool,

    /// List available albums
    #[arg(short = 'l', long)]
    pub list_albums: bool,

    /// List available libraries
    #[arg(long)]
    pub list_libraries: bool,

    /// Album(s) to download
    #[arg(short = 'a', long = "album")]
    pub albums: Vec<String>,

    /// Library to download (default: PrimarySync)
    #[arg(long, default_value = "PrimarySync")]
    pub library: String,

    /// Image size to download
    #[arg(long, value_enum, default_value = "original")]
    pub size: VersionSize,

    /// Live photo video size
    #[arg(long, value_enum, default_value = "original")]
    pub live_photo_size: LivePhotoSize,

    /// Number of recent photos to download
    #[arg(long)]
    pub recent: Option<u32>,

    /// Download until finding X consecutive existing photos
    #[arg(long)]
    pub until_found: Option<u32>,

    /// Don't download videos
    #[arg(long)]
    pub skip_videos: bool,

    /// Don't download photos
    #[arg(long)]
    pub skip_photos: bool,

    /// Don't download live photos
    #[arg(long)]
    pub skip_live_photos: bool,

    /// Only download requested size (don't fall back to original)
    #[arg(long)]
    pub force_size: bool,

    /// Scan "Recently Deleted" and delete local files found there
    #[arg(long)]
    pub auto_delete: bool,

    /// Folder structure for organizing downloads
    #[arg(long, default_value = "%Y/%m/%d")]
    pub folder_structure: String,

    /// Write DateTimeOriginal EXIF tag if missing
    #[arg(long)]
    pub set_exif_datetime: bool,

    /// Do not modify local system or iCloud
    #[arg(long)]
    pub dry_run: bool,

    /// iCloud domain (com or cn)
    #[arg(long, value_enum, default_value = "com")]
    pub domain: Domain,

    /// Run continuously, waiting N seconds between runs
    #[arg(long)]
    pub watch_with_interval: Option<u64>,

    /// Log level
    #[arg(long, value_enum, default_value = "debug")]
    pub log_level: LogLevel,

    /// Disable progress bar
    #[arg(long)]
    pub no_progress_bar: bool,

    /// Directory for cookies/session data
    #[arg(long, default_value = "~/.icloudpd-rs")]
    pub cookie_directory: String,

    /// Keep Unicode in filenames
    #[arg(long)]
    pub keep_unicode_in_filenames: bool,

    /// Live photo MOV filename policy
    #[arg(long, value_enum, default_value = "suffix")]
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,

    /// RAW treatment policy
    #[arg(long, value_enum, default_value = "as-is")]
    pub align_raw: RawTreatmentPolicy,

    /// File matching and dedup policy
    #[arg(long, value_enum, default_value = "name-size-dedup-with-suffix")]
    pub file_match_policy: FileMatchPolicy,

    /// Skip assets created before this ISO date or interval (e.g., 2025-01-02 or 20d)
    #[arg(long)]
    pub skip_created_before: Option<String>,

    /// Skip assets created after this ISO date or interval
    #[arg(long)]
    pub skip_created_after: Option<String>,

    /// Delete from iCloud after downloading
    #[arg(long)]
    pub delete_after_download: bool,

    /// Keep photos newer than N days in iCloud, delete the rest
    #[arg(long)]
    pub keep_icloud_recent_days: Option<u32>,

    /// Only print filenames without downloading
    #[arg(long)]
    pub only_print_filenames: bool,
}
