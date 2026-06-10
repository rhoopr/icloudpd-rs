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
pub(crate) use db::ScopedDbSyncToken;
pub use db::{
    DownloadStateStore, ImportStateStore, MembershipStore, MetadataRewriteStore, ReportStateStore,
    SqliteStateDb, SyncTokenStore,
};
pub use types::{AssetMetadata, AssetRecord, AssetStatus, MediaType, SyncRunStats, VersionSizeKey};
