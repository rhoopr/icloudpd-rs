pub mod exif;
pub mod file;
pub mod paths;

use std::fs::FileTimes;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use reqwest::Client;

use crate::icloud::photos::{AssetItemType, AssetVersionSize, PhotoAlbum};

/// Application configuration for the download engine.
///
/// This is a standalone subset of fields consumed by the download loop.
/// The canonical `Config` struct may live elsewhere; this serves as the
/// interface contract for the download engine.
#[derive(Debug)]
pub struct DownloadConfig {
    pub(crate) directory: std::path::PathBuf,
    pub(crate) folder_structure: String,
    pub(crate) size: AssetVersionSize,
    pub(crate) skip_videos: bool,
    pub(crate) skip_photos: bool,
    pub(crate) skip_created_before: Option<DateTime<Utc>>,
    pub(crate) skip_created_after: Option<DateTime<Utc>>,
    pub(crate) until_found: Option<u32>,
    pub(crate) set_exif_datetime: bool,
    pub(crate) dry_run: bool,
}

/// Main download loop: fetch all photos from each album, apply filters,
/// check local existence, download missing files, and optionally stamp
/// EXIF datetime.
pub async fn download_photos(
    client: &Client,
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
) -> Result<()> {
    for album in albums {
        let mut consecutive_found: u32 = 0;

        // PhotoAlbum::photos() handles pagination internally and returns
        // all assets in the album.
        let assets = album.photos().await?;

        for asset in &assets {
            // --- type filter ---
            if config.skip_videos && asset.item_type() == Some(AssetItemType::Movie) {
                continue;
            }
            if config.skip_photos && asset.item_type() == Some(AssetItemType::Image) {
                continue;
            }

            // --- date filters (UTC comparison) ---
            let created_utc = asset.created();
            if let Some(ref before) = config.skip_created_before {
                if created_utc < *before {
                    continue;
                }
            }
            if let Some(ref after) = config.skip_created_after {
                if created_utc > *after {
                    continue;
                }
            }

            // --- build local path ---
            let filename = match asset.filename() {
                Some(f) => f,
                None => {
                    tracing::warn!("Asset {} has no filename, skipping", asset.id());
                    continue;
                }
            };

            let created_local: DateTime<Local> = created_utc.with_timezone(&Local);
            let download_path = paths::local_download_path(
                &config.directory,
                &config.folder_structure,
                &created_local,
                &filename,
            );

            // --- existence / until-found check ---
            if download_path.exists() {
                consecutive_found += 1;
                tracing::debug!("{} already exists", download_path.display());

                if let Some(until_found) = config.until_found {
                    if consecutive_found >= until_found {
                        tracing::info!(
                            "Found {} consecutive previously downloaded photos. Exiting",
                            until_found
                        );
                        return Ok(());
                    }
                }
                continue;
            }

            consecutive_found = 0;

            // --- download ---
            let versions = asset.versions();
            if let Some(version) = versions.get(&config.size) {
                // Ensure parent directory exists (skip in dry-run).
                if !config.dry_run {
                    if let Some(parent) = download_path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                }

                let success = file::download_file(
                    client,
                    &version.url,
                    &download_path,
                    &version.checksum,
                    config.dry_run,
                )
                .await?;

                if success {
                    // Set file modification time to photo creation date.
                    if !config.dry_run {
                        let mtime_path = download_path.clone();
                        let ts = created_local.timestamp();
                        if let Err(e) = tokio::task::spawn_blocking(move || {
                            set_file_mtime(&mtime_path, ts)
                        }).await? {
                            tracing::warn!(
                                "Could not set mtime on {}: {}",
                                download_path.display(),
                                e
                            );
                        }
                    }

                    tracing::info!("Downloaded {}", download_path.display());

                    // Stamp EXIF DateTimeOriginal on JPEGs if missing.
                    if config.set_exif_datetime && !config.dry_run {
                        let ext = download_path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg") {
                            let exif_path = download_path.clone();
                            let date_str = created_local
                                .format("%Y:%m:%d %H:%M:%S")
                                .to_string();
                            let exif_result = tokio::task::spawn_blocking(move || {
                                match exif::get_photo_exif(&exif_path) {
                                    Ok(None) => {
                                        if let Err(e) = exif::set_photo_exif(&exif_path, &date_str) {
                                            tracing::warn!(
                                                "Failed to set EXIF on {}: {}",
                                                exif_path.display(),
                                                e
                                            );
                                        }
                                    }
                                    Ok(Some(_)) => {}
                                    Err(e) => {
                                        tracing::warn!(
                                            "Failed to read EXIF from {}: {}",
                                            exif_path.display(),
                                            e
                                        );
                                    }
                                }
                            }).await;
                            if let Err(e) = exif_result {
                                tracing::warn!("EXIF task panicked: {}", e);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Set the modification and access times of a file to the given Unix
/// timestamp. Uses `std::fs::File::set_times` (stable since Rust 1.75).
///
/// Handles negative timestamps (dates before 1970) gracefully by clamping
/// to the Unix epoch.
fn set_file_mtime(path: &Path, timestamp: i64) -> std::io::Result<()> {
    let time = if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp as u64)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    let times = FileTimes::new().set_modified(time).set_accessed(time);
    let file = std::fs::File::options().write(true).open(path)?;
    file.set_times(times)?;
    Ok(())
}
