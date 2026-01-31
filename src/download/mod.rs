pub mod exif;
pub mod file;
pub mod paths;

use std::fs::FileTimes;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use reqwest::Client;

use std::path::PathBuf;

use futures_util::stream::{self, StreamExt};

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
    pub(crate) set_exif_datetime: bool,
    pub(crate) dry_run: bool,
    pub(crate) concurrent_downloads: usize,
    pub(crate) recent: Option<u32>,
}

/// A unit of work produced by the filter phase and consumed by the download phase.
struct DownloadTask {
    url: String,
    download_path: PathBuf,
    checksum: String,
    created_local: DateTime<Local>,
}

/// Main download loop: fetch all photos from each album, apply filters,
/// check local existence, download missing files, and optionally stamp
/// EXIF datetime.
///
/// Uses a two-phase approach:
/// 1. Sequential filter pass — builds a list of download tasks
/// 2. Parallel download — consumes tasks with bounded concurrency
pub async fn download_photos(
    client: &Client,
    albums: &[PhotoAlbum],
    config: &DownloadConfig,
) -> Result<()> {
    // ── Phase 1: Concurrent album fetch + sequential filter ─────────
    let album_results: Vec<Result<Vec<_>>> = stream::iter(albums)
        .map(|album| async move {
            album.photos(config.recent).await
        })
        .buffer_unordered(config.concurrent_downloads)
        .collect()
        .await;

    let mut tasks: Vec<DownloadTask> = Vec::new();
    for album_result in album_results {
        let assets = album_result?;

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
            if let Some(before) = &config.skip_created_before {
                if created_utc < *before {
                    continue;
                }
            }
            if let Some(after) = &config.skip_created_after {
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
                filename,
            );

            if download_path.exists() {
                tracing::debug!("{} already exists", download_path.display());
                continue;
            }

            // --- collect task ---
            let versions = asset.versions();
            if let Some(version) = versions.get(&config.size) {
                tasks.push(DownloadTask {
                    url: version.url.clone(),
                    download_path,
                    checksum: version.checksum.clone(),
                    created_local,
                });
            }
        }
    }

    if tasks.is_empty() {
        tracing::info!("No new photos to download");
        return Ok(());
    }

    tracing::info!(
        "Found {} photos to download (concurrency: {})",
        tasks.len(),
        config.concurrent_downloads
    );

    if config.dry_run {
        for task in &tasks {
            tracing::info!("[DRY RUN] Would download {}", task.download_path.display());
        }
        print_summary(&tasks, config);
        return Ok(());
    }

    // ── Phase 2: Parallel download ───────────────────────────────────
    let client = client.clone();
    let set_exif = config.set_exif_datetime;

    let results: Vec<Result<()>> = stream::iter(tasks)
        .map(|task| {
            let client = client.clone();
            async move {
                if let Some(parent) = task.download_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                let success = file::download_file(
                    &client,
                    &task.url,
                    &task.download_path,
                    &task.checksum,
                    false,
                )
                .await?;

                if success {
                    // Set file modification time to photo creation date.
                    let mtime_path = task.download_path.clone();
                    let ts = task.created_local.timestamp();
                    if let Err(e) = tokio::task::spawn_blocking(move || {
                        set_file_mtime(&mtime_path, ts)
                    })
                    .await?
                    {
                        tracing::warn!(
                            "Could not set mtime on {}: {}",
                            task.download_path.display(),
                            e
                        );
                    }

                    tracing::info!("Downloaded {}", task.download_path.display());

                    // Stamp EXIF DateTimeOriginal on JPEGs if missing.
                    if set_exif {
                        let ext = task
                            .download_path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg") {
                            let exif_path = task.download_path.clone();
                            let date_str = task
                                .created_local
                                .format("%Y:%m:%d %H:%M:%S")
                                .to_string();
                            let exif_result = tokio::task::spawn_blocking(move || {
                                match exif::get_photo_exif(&exif_path) {
                                    Ok(None) => {
                                        if let Err(e) =
                                            exif::set_photo_exif(&exif_path, &date_str)
                                        {
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
                            })
                            .await;
                            if let Err(e) = exif_result {
                                tracing::warn!("EXIF task panicked: {}", e);
                            }
                        }
                    }
                }

                Ok(())
            }
        })
        .buffer_unordered(config.concurrent_downloads)
        .collect()
        .await;

    let failed = results.iter().filter(|r| r.is_err()).count();
    for result in &results {
        if let Err(e) = result {
            tracing::error!("Download failed: {}", e);
        }
    }

    let succeeded = results.len() - failed;
    tracing::info!("── Summary ──");
    tracing::info!("  {} downloaded, {} failed, {} total", succeeded, failed, results.len());

    if failed > 0 {
        anyhow::bail!("{} of {} downloads failed", failed, results.len());
    }

    Ok(())
}

/// Print a dry-run summary of what would be downloaded.
fn print_summary(tasks: &[DownloadTask], config: &DownloadConfig) {
    tracing::info!("── Dry Run Summary ──");
    tracing::info!("  {} files would be downloaded", tasks.len());
    tracing::info!("  destination: {}", config.directory.display());
    tracing::info!("  concurrency: {}", config.concurrent_downloads);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_file(name: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp/claude/download_tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"test").unwrap();
        p
    }

    #[test]
    fn test_set_file_mtime_positive_timestamp() {
        let p = tmp_file("pos.txt");
        set_file_mtime(&p, 1_700_000_000).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }

    #[test]
    fn test_set_file_mtime_zero_timestamp() {
        let p = tmp_file("zero.txt");
        set_file_mtime(&p, 0).unwrap();
        let meta = fs::metadata(&p).unwrap();
        let mtime = meta.modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH);
    }

    #[test]
    fn test_set_file_mtime_negative_timestamp() {
        let p = tmp_file("neg.txt");
        // Should not panic — clamps or uses pre-epoch time
        set_file_mtime(&p, -86400).unwrap();
    }

    #[test]
    fn test_set_file_mtime_nonexistent_file() {
        let p = PathBuf::from("/tmp/claude/download_tests/nonexistent_file.txt");
        let _ = fs::remove_file(&p); // ensure absent
        assert!(set_file_mtime(&p, 0).is_err());
    }
}
