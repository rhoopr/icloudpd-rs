//! Shared test fixtures and builders.
//!
//! Provides ergonomic builders for commonly constructed test objects
//! and reusable mock implementations of core traits.

use std::collections::VecDeque;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::icloud::photos::session::PhotosSession;
use crate::icloud::photos::PhotoAsset;
use crate::state::types::{AssetMetadata, AssetRecord, MediaType, VersionSizeKey};

// ── AssetRecord builder ─────────────────────────────────────────────

/// Builder for `AssetRecord::new_pending()` with sensible test defaults.
///
/// ```ignore
/// let record = TestAssetRecord::new("MY_ID").build();
/// let record = TestAssetRecord::new("MY_ID").checksum("abc").size(5000).build();
/// ```
pub struct TestAssetRecord {
    library: String,
    id: String,
    version_size: VersionSizeKey,
    checksum: String,
    filename: String,
    created_at: DateTime<Utc>,
    added_at: Option<DateTime<Utc>>,
    size_bytes: u64,
    media_type: MediaType,
    metadata: Option<AssetMetadata>,
}

impl TestAssetRecord {
    pub fn new(id: &str) -> Self {
        Self {
            library: crate::icloud::photos::PRIMARY_ZONE_NAME.to_string(),
            id: id.to_string(),
            version_size: VersionSizeKey::Original,
            checksum: "checksum123".to_string(),
            filename: "photo.jpg".to_string(),
            created_at: Utc::now(),
            added_at: None,
            size_bytes: 12345,
            media_type: MediaType::Photo,
            metadata: None,
        }
    }

    pub fn library(mut self, library: &str) -> Self {
        self.library = library.to_string();
        self
    }

    pub fn checksum(mut self, c: &str) -> Self {
        self.checksum = c.to_string();
        self
    }

    pub fn filename(mut self, f: &str) -> Self {
        self.filename = f.to_string();
        self
    }

    pub fn created_at(mut self, t: DateTime<Utc>) -> Self {
        self.created_at = t;
        self
    }

    pub fn added_at(mut self, t: DateTime<Utc>) -> Self {
        self.added_at = Some(t);
        self
    }

    pub fn size(mut self, s: u64) -> Self {
        self.size_bytes = s;
        self
    }

    pub fn media_type(mut self, m: MediaType) -> Self {
        self.media_type = m;
        self
    }

    pub fn metadata(mut self, m: AssetMetadata) -> Self {
        self.metadata = Some(m);
        self
    }

    pub fn version_size(mut self, v: VersionSizeKey) -> Self {
        self.version_size = v;
        self
    }

    pub fn build(self) -> AssetRecord {
        let record = AssetRecord::new_pending(
            std::sync::Arc::from(self.library),
            self.id,
            self.version_size,
            self.checksum,
            self.filename,
            self.created_at,
            self.added_at,
            self.size_bytes,
            self.media_type,
        );
        if let Some(meta) = self.metadata {
            record.with_metadata(meta)
        } else {
            record
        }
    }
}

// ── PhotoAsset builder ──────────────────────────────────────────────

/// Builder for `PhotoAsset::new()` with sensible test defaults.
///
/// ```ignore
/// let asset = TestPhotoAsset::new("TEST_1").build();
/// let asset = TestPhotoAsset::new("LIVE_1")
///     .filename("IMG_0001.HEIC")
///     .item_type("public.heic")
///     .orig_file_type("public.heic")
///     .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
///     .build();
/// ```
pub struct TestPhotoAsset {
    record_name: String,
    filename: String,
    item_type: String,
    orig_size: u64,
    orig_url: String,
    orig_checksum: String,
    orig_file_type: String,
    asset_date: f64,
    live_photo: Option<LivePhotoFields>,
    adjusted_version: Option<AdjustedVersionFields>,
    live_adjusted: Option<LivePhotoFields>,
    alt_version: Option<AltVersionFields>,
}

struct LivePhotoFields {
    url: String,
    checksum: String,
    size: u64,
}

struct AltVersionFields {
    url: String,
    checksum: String,
    size: u64,
    file_type: String,
}

struct AdjustedVersionFields {
    url: String,
    checksum: String,
    size: u64,
    file_type: String,
}

impl TestPhotoAsset {
    pub fn new(record_name: &str) -> Self {
        Self {
            record_name: record_name.to_string(),
            filename: "photo.jpg".to_string(),
            item_type: "public.jpeg".to_string(),
            orig_size: 1000,
            orig_url: "https://p01.icloud-content.com/orig".to_string(),
            orig_checksum: "abc123".to_string(),
            orig_file_type: "public.jpeg".to_string(),
            asset_date: 1736899200000.0,
            live_photo: None,
            adjusted_version: None,
            live_adjusted: None,
            alt_version: None,
        }
    }

    pub fn filename(mut self, f: &str) -> Self {
        self.filename = f.to_string();
        self
    }

    pub fn item_type(mut self, t: &str) -> Self {
        self.item_type = t.to_string();
        self
    }

    pub fn orig_size(mut self, s: u64) -> Self {
        self.orig_size = s;
        self
    }

    pub fn orig_url(mut self, u: &str) -> Self {
        self.orig_url = u.to_string();
        self
    }

    pub fn orig_checksum(mut self, c: &str) -> Self {
        self.orig_checksum = c.to_string();
        self
    }

    pub fn orig_file_type(mut self, t: &str) -> Self {
        self.orig_file_type = t.to_string();
        self
    }

    pub fn asset_date(mut self, d: f64) -> Self {
        self.asset_date = d;
        self
    }

    pub fn live_photo(mut self, url: &str, checksum: &str, size: u64) -> Self {
        self.live_photo = Some(LivePhotoFields {
            url: url.to_string(),
            checksum: checksum.to_string(),
            size,
        });
        self
    }

    pub fn adjusted_version(
        mut self,
        url: &str,
        checksum: &str,
        size: u64,
        file_type: &str,
    ) -> Self {
        self.adjusted_version = Some(AdjustedVersionFields {
            url: url.to_string(),
            checksum: checksum.to_string(),
            size,
            file_type: file_type.to_string(),
        });
        self
    }

    pub fn live_adjusted(mut self, url: &str, checksum: &str, size: u64) -> Self {
        self.live_adjusted = Some(LivePhotoFields {
            url: url.to_string(),
            checksum: checksum.to_string(),
            size,
        });
        self
    }

    pub fn alt_version(mut self, url: &str, checksum: &str, size: u64, file_type: &str) -> Self {
        self.alt_version = Some(AltVersionFields {
            url: url.to_string(),
            checksum: checksum.to_string(),
            size,
            file_type: file_type.to_string(),
        });
        self
    }

    pub fn build(self) -> PhotoAsset {
        let mut fields = json!({
            "filenameEnc": {"value": self.filename, "type": "STRING"},
            "itemType": {"value": self.item_type},
            "resOriginalRes": {"value": {
                "size": self.orig_size,
                "downloadURL": self.orig_url,
                "fileChecksum": self.orig_checksum,
            }},
            "resOriginalFileType": {"value": self.orig_file_type},
        });

        if let Some(lp) = &self.live_photo {
            fields["resOriginalVidComplRes"] = json!({"value": {
                "size": lp.size,
                "downloadURL": lp.url,
                "fileChecksum": lp.checksum,
            }});
            fields["resOriginalVidComplFileType"] = json!({"value": "com.apple.quicktime-movie"});
        }

        if let Some(adjusted) = &self.adjusted_version {
            fields["resJPEGFullRes"] = json!({"value": {
                "size": adjusted.size,
                "downloadURL": adjusted.url,
                "fileChecksum": adjusted.checksum,
            }});
            fields["resJPEGFullFileType"] = json!({"value": adjusted.file_type});
        }

        if let Some(lp) = &self.live_adjusted {
            fields["resVidFullRes"] = json!({"value": {
                "size": lp.size,
                "downloadURL": lp.url,
                "fileChecksum": lp.checksum,
            }});
            fields["resVidFullFileType"] = json!({"value": "com.apple.quicktime-movie"});
        }

        if let Some(alt) = &self.alt_version {
            fields["resOriginalAltRes"] = json!({"value": {
                "size": alt.size,
                "downloadURL": alt.url,
                "fileChecksum": alt.checksum,
            }});
            fields["resOriginalAltFileType"] = json!({"value": alt.file_type});
        }

        let master = json!({
            "recordName": self.record_name,
            "fields": fields,
        });
        let asset = json!({
            "fields": {"assetDate": {"value": self.asset_date}},
        });
        PhotoAsset::new(master, asset)
    }
}

// ── CloudKit/Photos response flow builder ───────────────────────────

/// Small builder for the queued CloudKit responses used by `MockPhotosSession`.
///
/// This intentionally only covers the response shapes current tests repeat:
/// album-count batches, `/records/query` pages, `/changes/database` pages,
/// `/changes/zone` pages, and queued transport errors. It is not a fake
/// CloudKit implementation.
pub struct MockPhotosFlow {
    session: MockPhotosSession,
}

impl MockPhotosFlow {
    pub fn new() -> Self {
        Self {
            session: MockPhotosSession::new(),
        }
    }

    pub fn album_count(mut self, count: u64) -> Self {
        self.session = self.session.ok(json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
        }));
        self
    }

    pub fn query_page(mut self, records: Vec<Value>, sync_token: Option<&str>) -> Self {
        let mut page = json!({ "records": records });
        if let Some(token) = sync_token {
            page["syncToken"] = json!(token);
        }
        self.session = self.session.ok(page);
        self
    }

    pub fn query_photo_page(mut self, record_name: &str, sync_token: Option<&str>) -> Self {
        self.session = self
            .session
            .ok(mock_photo_query_page(record_name, sync_token));
        self
    }

    pub fn empty_query_page(self, sync_token: Option<&str>) -> Self {
        self.query_page(Vec::new(), sync_token)
    }

    pub fn changes_database(
        mut self,
        sync_token: &str,
        changed_zones: &[(&str, &str)],
        more_coming: bool,
    ) -> Self {
        let zones: Vec<Value> = changed_zones
            .iter()
            .map(|(zone_name, zone_sync_token)| {
                json!({
                    "zoneID": {"zoneName": zone_name, "ownerRecordName": "_defaultOwner"},
                    "syncToken": zone_sync_token,
                })
            })
            .collect();
        self.session = self.session.ok(json!({
            "syncToken": sync_token,
            "moreComing": more_coming,
            "zones": zones,
        }));
        self
    }

    pub fn changes_zone_page(
        mut self,
        records: Vec<Value>,
        sync_token: &str,
        more_coming: bool,
    ) -> Self {
        self.session = self.session.ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": more_coming,
                "records": records,
            }]
        }));
        self
    }

    pub fn changes_photo_page(
        self,
        record_name: &str,
        sync_token: &str,
        more_coming: bool,
    ) -> Self {
        self.changes_zone_page(mock_photo_records(record_name), sync_token, more_coming)
    }

    pub fn changes_zone_error(
        mut self,
        server_error_code: &str,
        reason: &str,
        sync_token: &str,
    ) -> Self {
        self.session = self.session.ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": false,
                "serverErrorCode": server_error_code,
                "reason": reason,
            }]
        }));
        self
    }

    pub fn error(mut self, message: &str) -> Self {
        self.session = self.session.err(message);
        self
    }

    pub fn build(self) -> MockPhotosSession {
        self.session
    }
}

pub(crate) fn mock_photo_query_page(record_name: &str, sync_token: Option<&str>) -> Value {
    let mut page = json!({ "records": mock_photo_records(record_name) });
    if let Some(token) = sync_token {
        page["syncToken"] = json!(token);
    }
    page
}

fn mock_photo_records(record_name: &str) -> Vec<Value> {
    vec![
        json!({
            "recordName": record_name,
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": "dGVzdC5qcGc=", "type": "STRING"},
                "resOriginalRes": {
                    "value": {
                        "downloadURL": "https://p01.icloud-content.com/photo.jpg",
                        "size": 1024,
                        "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                    }
                },
                "resOriginalWidth": {"value": 100, "type": "INT64"},
                "resOriginalHeight": {"value": 100, "type": "INT64"},
                "resOriginalFileType": {"value": "public.jpeg"},
                "itemType": {"value": "public.jpeg"},
                "adjustmentRenderType": {"value": 0, "type": "INT64"}
            },
            "recordChangeTag": "ct1"
        }),
        json!({
            "recordName": format!("asset-{record_name}"),
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                    "value": {
                        "recordName": record_name,
                        "zoneID": {"zoneName": "PrimarySync"}
                    },
                    "type": "REFERENCE"
                },
                "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
            },
            "recordChangeTag": "ct2"
        }),
    ]
}

impl Default for MockPhotosFlow {
    fn default() -> Self {
        Self::new()
    }
}

// ── Mock PhotosSession ──────────────────────────────────────────────

/// Recorded call to `MockPhotosSession::post()`.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub url: String,
    pub _body: String,
}

/// Response action for a single `post()` call.
pub enum MockResponse {
    /// Return `Ok(value)`.
    Ok(Value),
    /// Return `Err(...)`.
    Err(String),
}

/// A configurable mock `PhotosSession` that supports:
/// - Sequenced responses (success or error per call)
/// - Call recording for assertion
/// - Fallback to empty `{"records": []}` when the queue is exhausted
///
/// ```ignore
/// let mock = MockPhotosSession::new()
///     .ok(json!({"records": [...]}))
///     .err("simulated failure")
///     .ok(json!({"records": []}));
/// ```
pub struct MockPhotosSession {
    responses: Mutex<VecDeque<MockResponse>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl MockPhotosSession {
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Queue a successful response.
    pub fn ok(self, value: Value) -> Self {
        self.responses
            .lock()
            .expect("poisoned")
            .push_back(MockResponse::Ok(value));
        self
    }

    /// Queue an error response.
    pub fn err(self, message: &str) -> Self {
        self.responses
            .lock()
            .expect("poisoned")
            .push_back(MockResponse::Err(message.to_string()));
        self
    }

    /// Return all recorded calls for assertion.
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("poisoned").clone()
    }

    /// Return the number of calls made.
    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("poisoned").len()
    }
}

#[async_trait::async_trait]
impl PhotosSession for MockPhotosSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        self.calls.lock().expect("poisoned").push(RecordedCall {
            url: url.to_string(),
            _body: body,
        });

        let response = self.responses.lock().expect("poisoned").pop_front();

        match response {
            Some(MockResponse::Ok(v)) => Ok(v),
            Some(MockResponse::Err(msg)) => Err(anyhow::anyhow!("{msg}")),
            None => Ok(json!({"records": []})),
        }
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        let remaining: Vec<MockResponse> = {
            let queue = self.responses.lock().expect("poisoned");
            queue
                .iter()
                .map(|r| match r {
                    MockResponse::Ok(v) => MockResponse::Ok(v.clone()),
                    MockResponse::Err(msg) => MockResponse::Err(msg.clone()),
                })
                .collect()
        };
        let mut new = MockPhotosSession::new();
        *new.responses.get_mut().unwrap() = remaining.into();
        Box::new(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_session_records_calls_and_sequences_responses() {
        let mock = MockPhotosSession::new()
            .ok(json!({"records": [{"id": 1}]}))
            .err("server error");

        assert_eq!(mock.call_count(), 0);

        let r1 = mock
            .post("https://example.com/query", "{}".to_owned(), &[])
            .await;
        assert!(r1.is_ok());
        assert_eq!(mock.call_count(), 1);

        let r2 = mock
            .post("https://example.com/changes", "{}".to_owned(), &[])
            .await;
        assert!(r2.is_err());
        assert_eq!(mock.call_count(), 2);

        // Exhausted queue falls back to empty records
        let r3 = mock
            .post("https://example.com/extra", "{}".to_owned(), &[])
            .await;
        assert_eq!(r3.unwrap(), json!({"records": []}));

        let calls = mock.recorded_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].url, "https://example.com/query");
        assert_eq!(calls[1].url, "https://example.com/changes");
    }

    #[tokio::test]
    async fn mock_photos_flow_queues_common_cloudkit_shapes() {
        let mock = MockPhotosFlow::new()
            .album_count(7)
            .changes_database("db-token", &[("PrimarySync", "zone-token")], true)
            .error("transport failure")
            .build();

        let count = mock
            .post("https://example.com/count", "{}".to_owned(), &[])
            .await
            .expect("count response");
        assert_eq!(
            count["batch"][0]["records"][0]["fields"]["itemCount"]["value"],
            7
        );

        let changes = mock
            .post("https://example.com/changes/database", "{}".to_owned(), &[])
            .await
            .expect("changes database response");
        assert_eq!(changes["syncToken"], "db-token");
        assert_eq!(changes["zones"][0]["zoneID"]["zoneName"], "PrimarySync");
        assert_eq!(changes["zones"][0]["syncToken"], "zone-token");
        assert!(changes["moreComing"].as_bool().unwrap_or(false));

        let err = mock
            .post("https://example.com/fail", "{}".to_owned(), &[])
            .await
            .expect_err("queued transport error");
        assert!(err.to_string().contains("transport failure"));
    }
}
