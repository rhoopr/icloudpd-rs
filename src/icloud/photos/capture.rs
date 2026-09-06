//! Opt-in, sensitive response bodies. No provider metadata belongs in filenames or errors.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::{OwnedRwLockReadGuard, RwLock};

#[derive(Debug, thiserror::Error)]
#[error("iCloud Photos response capture failed or was interrupted; capture is incomplete")]
pub(super) struct CaptureError;

pub(crate) struct ResponseCapture {
    directory: PathBuf,
    next: AtomicU64,
    failed: AtomicBool,
    incomplete: AtomicBool,
    closed: Arc<RwLock<bool>>,
}

impl ResponseCapture {
    pub(crate) async fn new(data_dir: &Path) -> anyhow::Result<Arc<Self>> {
        let parent = data_dir.join(".diagnostics");
        let run = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            uuid::Uuid::new_v4()
        );
        let directory = parent.join(&run);
        let target = directory.clone();
        tokio::task::spawn_blocking(move || create_directory(&parent, &target))
            .await
            .map_err(|_join_error| CaptureError)?
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::Unsupported {
                    anyhow::Error::new(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "Raw iCloud Photos response capture requires Unix private filesystem permissions",
                    ))
                    .context("Could not initialize response capture")
                } else {
                    CaptureError.into()
                }
            })?;
        // Only the generated relative path is safe to log: data_dir may contain an Apple ID.
        tracing::warn!(
            directory = %format!(".diagnostics/{run}"),
            "Sensitive raw iCloud Photos responses will be saved under the application data directory. Files may contain personal metadata and credentials; do not share them unredacted."
        );
        Ok(Arc::new(Self {
            directory,
            next: AtomicU64::new(1),
            failed: AtomicBool::new(false),
            incomplete: AtomicBool::new(false),
            closed: Arc::new(RwLock::new(false)),
        }))
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(CaptureError.into());
        }
        Ok(())
    }

    /// Close admission and wait for network calls and their filesystem work.
    /// Call after dropping the inner sync future and its provider/session state.
    pub(crate) async fn finish(&self) -> anyhow::Result<()> {
        *self.closed.write().await = true;
        self.check()?;
        if self.incomplete.load(Ordering::Acquire) {
            return Err(CaptureError.into());
        }
        Ok(())
    }

    fn fail(&self) -> CaptureError {
        if !self.failed.swap(true, Ordering::AcqRel) {
            tracing::error!(
                "iCloud Photos response capture failed or was interrupted; stopping provider requests"
            );
        }
        CaptureError
    }

    pub(super) async fn begin(self: &Arc<Self>) -> anyhow::Result<CaptureRequest> {
        let lease = Arc::new(Arc::clone(&self.closed).read_owned().await);
        self.check()?;
        anyhow::ensure!(!**lease, "iCloud Photos response capture is closed");
        Ok(CaptureRequest {
            capture: Arc::clone(self),
            lease,
            complete: false,
        })
    }
}

#[cfg(unix)]
fn create_directory(parent: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    if let Some(data_dir) = parent.parent() {
        builder.recursive(true).create(data_dir)?;
        builder.recursive(false);
    }
    match builder.create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // data_dir is the trusted application boundary. Its private child must not
    // be a link, foreign-owned, or accessible to other users.
    let parent_handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(parent)?;
    let metadata = parent_handle.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != crate::service::env::effective_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    builder.create(target)?;
    parent_handle.sync_all()
}

#[cfg(not(unix))]
fn create_directory(_parent: &Path, _target: &Path) -> std::io::Result<()> {
    // Do not create sensitive files without an at-creation privacy guarantee.
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[must_use]
pub(super) struct CaptureRequest {
    capture: Arc<ResponseCapture>,
    // Blocking filesystem calls keep a clone: cancellation must not let finish
    // return while a detached blocking write/publication is still running.
    lease: Arc<OwnedRwLockReadGuard<bool>>,
    complete: bool,
}

impl CaptureRequest {
    pub(super) fn complete(&mut self) {
        self.complete = true;
    }

    /// Persist every byte, retaining only the requested prefix for HTTP errors.
    pub(super) async fn read_body(
        &mut self,
        mut response: reqwest::Response,
        prefix_limit: Option<usize>,
    ) -> anyhow::Result<Vec<u8>> {
        self.capture.check()?;
        let number = self.capture.next.fetch_add(1, Ordering::Relaxed);
        let part = self
            .capture
            .directory
            .join(format!("{number:06}.body.part"));
        let final_path = self.capture.directory.join(format!("{number:06}.body"));
        let path = part.clone();
        let lease = Arc::clone(&self.lease);
        let file = tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path)
        })
        .await
        .map_err(|_join_error| self.capture.fail())?
        .map_err(|_io_error| self.capture.fail())?;
        let file = Arc::new(file);
        let mut retained = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            // A failed transport attempt is retryable, but the run must still
            // report its incomplete capture even if a later attempt succeeds.
            self.capture.incomplete.store(true, Ordering::Release);
            self.complete();
            error.without_url()
        })? {
            self.capture.check()?;
            let take = prefix_limit
                .map_or(chunk.len(), |limit| limit.saturating_sub(retained.len()))
                .min(chunk.len());
            retained.extend(chunk.iter().take(take));
            let file = Arc::clone(&file);
            let lease = Arc::clone(&self.lease);
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                let _lease = lease;
                (&*file).write_all(&chunk)
            })
            .await
            .map_err(|_join_error| self.capture.fail())?
            .map_err(|_io_error| self.capture.fail())?;
        }
        self.capture.check()?;
        let directory = self.capture.directory.clone();
        let lease = Arc::clone(&self.lease);
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let _lease = lease;
            file.sync_all()?;
            // hard_link is an atomic no-overwrite publication on Unix.
            std::fs::hard_link(&part, final_path)?;
            std::fs::remove_file(part)?;
            std::fs::File::open(directory)?.sync_all()
        })
        .await
        .map_err(|_join_error| self.capture.fail())?
        .map_err(|_io_error| self.capture.fail())?;
        self.complete();
        self.capture.check()?;
        Ok(retained)
    }
}

impl Drop for CaptureRequest {
    fn drop(&mut self) {
        if !self.complete {
            self.capture.fail();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::Arc;
    use std::time::Duration;

    use super::ResponseCapture;

    #[tokio::test]
    async fn creates_missing_data_dir_privately_without_changing_existing_modes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let data = root.path().join("missing/data");
        let capture = ResponseCapture::new(&data).await.unwrap();
        for path in [
            root.path().join("missing"),
            data.clone(),
            data.join(".diagnostics"),
            capture.directory.clone(),
        ] {
            assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o700);
        }
        assert_eq!(
            std::fs::metadata(root.path()).unwrap().mode() & 0o777,
            0o750
        );
        capture.finish().await.unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o750)).unwrap();
        ResponseCapture::new(&data)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(&data).unwrap().mode() & 0o777, 0o750);
        assert_eq!(
            std::fs::metadata(root.path()).unwrap().mode() & 0o777,
            0o750
        );
    }

    #[tokio::test]
    async fn private_runs_exact_bytes_and_no_overwrite() {
        let data = tempfile::tempdir().unwrap();
        let first = ResponseCapture::new(data.path()).await.unwrap();
        let second = ResponseCapture::new(data.path()).await.unwrap();
        assert_ne!(first.directory, second.directory);
        for path in [
            data.path().join(".diagnostics"),
            first.directory.clone(),
            second.directory.clone(),
        ] {
            let metadata = std::fs::metadata(path).unwrap();
            assert_eq!(metadata.mode() & 0o777, 0o700);
            assert_eq!(metadata.uid(), crate::service::env::effective_uid());
        }
        let bytes = b" {\"unknown\":1,\"unknown\":2} \n";
        let mut request = first.begin().await.unwrap();
        let response = reqwest::Response::from(http::Response::new(bytes.to_vec()));
        assert_eq!(request.read_body(response, None).await.unwrap(), bytes);
        drop(request);
        let body = first.directory.join("000001.body");
        assert_eq!(std::fs::read(&body).unwrap(), bytes);
        assert_eq!(std::fs::metadata(body).unwrap().mode() & 0o777, 0o600);
        assert!(!first.directory.join("000001.body.part").exists());

        // An existing completed file is never replaced, and failure stays latched.
        std::fs::write(first.directory.join("000002.body"), b"existing").unwrap();
        let mut request = first.begin().await.unwrap();
        let response = reqwest::Response::from(http::Response::new(b"new".to_vec()));
        assert!(request.read_body(response, None).await.is_err());
        drop(request);
        assert_eq!(
            std::fs::read(first.directory.join("000002.body")).unwrap(),
            b"existing"
        );
        assert_eq!(
            std::fs::read(first.directory.join("000002.body.part")).unwrap(),
            b"new"
        );
        assert!(first.check().is_err());
        assert!(first.begin().await.is_err());
        assert!(first.finish().await.is_err());
        second.finish().await.unwrap();
        assert!(second.begin().await.is_err());
        second.finish().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_untrusted_diagnostics_and_existing_part() {
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let diagnostics = data.path().join(".diagnostics");
        symlink(outside.path(), &diagnostics).unwrap();
        assert!(ResponseCapture::new(data.path()).await.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        std::fs::remove_file(&diagnostics).unwrap();
        std::fs::write(&diagnostics, b"not a directory").unwrap();
        assert!(ResponseCapture::new(data.path()).await.is_err());
        std::fs::remove_file(&diagnostics).unwrap();
        std::fs::create_dir(&diagnostics).unwrap();
        std::fs::set_permissions(&diagnostics, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ResponseCapture::new(data.path()).await.is_err());
        std::fs::set_permissions(&diagnostics, std::fs::Permissions::from_mode(0o700)).unwrap();
        if crate::service::env::effective_uid() == 0 {
            std::os::unix::fs::chown(&diagnostics, Some(1), None).unwrap();
            assert!(ResponseCapture::new(data.path()).await.is_err());
            std::os::unix::fs::chown(&diagnostics, Some(0), None).unwrap();
        }
        let capture = ResponseCapture::new(data.path()).await.unwrap();
        let part = capture.directory.join("000001.body.part");
        std::fs::write(&part, b"existing").unwrap();
        let mut request = capture.begin().await.unwrap();
        let response = reqwest::Response::from(http::Response::new(b"new".to_vec()));
        assert!(request.read_body(response, None).await.is_err());
        drop(request);
        assert_eq!(std::fs::read(part).unwrap(), b"existing");
        assert!(capture.finish().await.is_err());
    }

    #[tokio::test]
    async fn finish_drains_detached_filesystem_work_after_cancellation() {
        let data = tempfile::tempdir().unwrap();
        let capture = ResponseCapture::new(data.path()).await.unwrap();
        let request = capture.begin().await.unwrap();
        let lease = Arc::clone(&request.lease);
        let (release, wait) = std::sync::mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _lease = lease;
            wait.recv().unwrap();
        });
        drop(request);
        let finish = capture.finish();
        tokio::pin!(finish);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut finish)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        worker.await.unwrap();
        assert!(finish.await.is_err());
        assert!(capture.begin().await.is_err());
    }
}
