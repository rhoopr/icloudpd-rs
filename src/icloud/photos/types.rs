// Re-export provider-agnostic types from the core module so that existing
// iCloud-internal imports (`super::types::AssetVersionSize`, etc.) continue
// to resolve without changing every file in `icloud/photos/`.
pub use crate::types::{AssetItemType, AssetVersionSize, ChangeReason};

/// Information about a downloadable asset version.
///
/// Uses `Box<str>` instead of `String` for url, `asset_type`, and checksum
/// to save 8 bytes per field (16 vs 24 bytes) since these strings are
/// never mutated after construction.
#[derive(Debug, Clone)]
pub struct AssetVersion {
    pub size: u64,
    pub url: Box<str>,
    pub asset_type: Box<str>,
    pub checksum: Box<str>,
}
