//! State tracking module for persistent sync state.
//!
//! This module provides SQLite-based state tracking for iCloud photo downloads.
//! It tracks which assets have been seen, downloaded, or failed, enabling:
//! - Skip-by-DB downloads (faster than filesystem checks)
//! - Failure tracking and retry
//! - Status reporting
//! - Verification of downloaded files

pub mod db;
pub mod error;
pub mod schema;
pub mod types;

#[cfg(test)]
pub use db::ImportedRecord;
#[allow(
    unused_imports,
    reason = "schema v21 exports the replica role before download callers migrate to it"
)]
pub use db::{
    AssetReplica, DownloadStateStore, ImportStateStore, MembershipStore, MetadataRewriteStore,
    ReplicaDownloadEvidence, ReplicaStateStore, ReplicaStatus, ReportStateStore, SqliteStateDb,
    SyncTokenStore,
};
pub(crate) use db::{
    AssetVerificationState, CheckpointTransition, DownloadContextStateStore, DownloadedFileRecord,
    OwnedTempFile, RetryErrorRetention, ScopedDbSyncToken, TempFileOwnershipStore,
};
#[cfg(test)]
pub(crate) use types::MetadataCaptureStatus;
pub use types::{AssetMetadata, AssetRecord, AssetStatus, MediaType, SyncRunStats, VersionSizeKey};
pub(crate) use types::{METADATA_CAPTURE_REVISION, MetadataCaptureCandidate};
