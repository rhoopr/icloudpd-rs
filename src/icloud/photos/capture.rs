use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct ResponseCapture {
    directory: PathBuf,
    next: AtomicU64,
}

impl ResponseCapture {
    pub(crate) async fn new(data_dir: &Path) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            cfg!(unix),
            "Response capture requires Unix private file permissions"
        );
        let parent = data_dir.join(".diagnostics");
        let directory = parent.join(uuid::Uuid::new_v4().to_string());
        let target = directory.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.recursive(true).create(&parent)?;
            let metadata = std::fs::symlink_metadata(&parent)?;
            if !metadata.is_dir() {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.mode() & 0o077 != 0
                    || metadata.uid() != crate::service::env::effective_uid()
                {
                    return Err(std::io::ErrorKind::PermissionDenied.into());
                }
            }
            builder.recursive(false).create(&target)
        })
        .await??;
        tracing::warn!(
            "Saving unredacted Photos responses under the data directory's .diagnostics folder; do not share these files"
        );
        Ok(Arc::new(Self {
            directory,
            next: AtomicU64::new(1),
        }))
    }

    pub(super) async fn write(&self, bytes: bytes::Bytes) -> anyhow::Result<()> {
        let path = self.directory.join(format!(
            "{:06}.body",
            self.next.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            let part = path.with_extension("body.part");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&part)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::hard_link(&part, &path)?;
            std::fs::remove_file(part)?;
            crate::fs_util::fsync_parent_dir(&path)
        })
        .await??;
        Ok(())
    }
}
