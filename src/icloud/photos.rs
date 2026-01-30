use std::collections::HashMap;
use std::sync::LazyLock;

use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};
use tracing::{debug, error, warn};

use crate::icloud::error::ICloudError;

const ROOT_FOLDER: &str = "----Root-Folder----";
const PROJECT_ROOT_FOLDER: &str = "----Project-Root-Folder----";

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AssetVersion {
    pub size: u64,
    pub url: String,
    pub asset_type: String,
    pub checksum: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetItemType {
    Image,
    Movie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetVersionSize {
    Original,
    Alternative,
    Medium,
    Thumb,
    Adjusted,
    LiveOriginal,
    LiveMedium,
    LiveThumb,
}

impl From<crate::types::VersionSize> for AssetVersionSize {
    fn from(v: crate::types::VersionSize) -> Self {
        match v {
            crate::types::VersionSize::Original => AssetVersionSize::Original,
            crate::types::VersionSize::Medium => AssetVersionSize::Medium,
            crate::types::VersionSize::Thumb => AssetVersionSize::Thumb,
            crate::types::VersionSize::Adjusted => AssetVersionSize::Adjusted,
            crate::types::VersionSize::Alternative => AssetVersionSize::Alternative,
        }
    }
}

// ---------------------------------------------------------------------------
// Session trait – thin abstraction over HTTP
// ---------------------------------------------------------------------------

/// Minimal async session used by the photos service.
/// The concrete implementation lives in `crate::auth::session`.
#[async_trait::async_trait]
#[allow(dead_code)]
pub trait PhotosSession: Send + Sync {
    async fn post(
        &self,
        url: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value>;

    async fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response>;

    /// Clone this session into a new boxed trait object.
    fn clone_box(&self) -> Box<dyn PhotosSession>;
}

// A convenience blanket implementation for `reqwest::Client` so that
// callers can use it directly without a full auth session.
#[async_trait::async_trait]
impl PhotosSession for reqwest::Client {
    async fn post(
        &self,
        url: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let mut builder = self.post(url).body(body.to_owned());
        for &(k, v) in headers {
            builder = builder.header(k, v);
        }
        let resp = builder.send().await?;
        let json: Value = resp.json().await?;
        Ok(json)
    }

    async fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<reqwest::Response> {
        let mut builder = reqwest::Client::get(self, url);
        for &(k, v) in headers {
            builder = builder.header(k, v);
        }
        let resp = builder.send().await?;
        Ok(resp)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// Query helpers – desired keys list used by list queries
// ---------------------------------------------------------------------------

const DESIRED_KEYS: &[&str] = &[
    "resJPEGFullWidth",
    "resJPEGFullHeight",
    "resJPEGFullFileType",
    "resJPEGFullFingerprint",
    "resJPEGFullRes",
    "resJPEGLargeWidth",
    "resJPEGLargeHeight",
    "resJPEGLargeFileType",
    "resJPEGLargeFingerprint",
    "resJPEGLargeRes",
    "resJPEGMedWidth",
    "resJPEGMedHeight",
    "resJPEGMedFileType",
    "resJPEGMedFingerprint",
    "resJPEGMedRes",
    "resJPEGThumbWidth",
    "resJPEGThumbHeight",
    "resJPEGThumbFileType",
    "resJPEGThumbFingerprint",
    "resJPEGThumbRes",
    "resVidFullWidth",
    "resVidFullHeight",
    "resVidFullFileType",
    "resVidFullFingerprint",
    "resVidFullRes",
    "resVidMedWidth",
    "resVidMedHeight",
    "resVidMedFileType",
    "resVidMedFingerprint",
    "resVidMedRes",
    "resVidSmallWidth",
    "resVidSmallHeight",
    "resVidSmallFileType",
    "resVidSmallFingerprint",
    "resVidSmallRes",
    "resSidecarWidth",
    "resSidecarHeight",
    "resSidecarFileType",
    "resSidecarFingerprint",
    "resSidecarRes",
    "itemType",
    "dataClassType",
    "filenameEnc",
    "originalOrientation",
    "resOriginalWidth",
    "resOriginalHeight",
    "resOriginalFileType",
    "resOriginalFingerprint",
    "resOriginalRes",
    "resOriginalAltWidth",
    "resOriginalAltHeight",
    "resOriginalAltFileType",
    "resOriginalAltFingerprint",
    "resOriginalAltRes",
    "resOriginalVidComplWidth",
    "resOriginalVidComplHeight",
    "resOriginalVidComplFileType",
    "resOriginalVidComplFingerprint",
    "resOriginalVidComplRes",
    "isDeleted",
    "isExpunged",
    "dateExpunged",
    "remappedRef",
    "recordName",
    "recordType",
    "recordChangeTag",
    "masterRef",
    "adjustmentRenderType",
    "assetDate",
    "addedDate",
    "isFavorite",
    "isHidden",
    "orientation",
    "duration",
    "assetSubtype",
    "assetSubtypeV2",
    "assetHDRType",
    "burstFlags",
    "burstFlagsExt",
    "burstId",
    "captionEnc",
    "locationEnc",
    "locationV2Enc",
    "locationLatitude",
    "locationLongitude",
    "adjustmentType",
    "timeZoneOffset",
    "vidComplDurValue",
    "vidComplDurScale",
    "vidComplDispValue",
    "vidComplDispScale",
    "keywordsEnc",
    "extendedDescEnc",
    "adjustedMediaMetaDataEnc",
    "adjustmentSimpleDataEnc",
    "vidComplVisibilityState",
    "customRenderedValue",
    "containerId",
    "itemId",
    "position",
    "isKeyAsset",
];

static DESIRED_KEYS_VALUES: LazyLock<Vec<Value>> = LazyLock::new(|| {
    DESIRED_KEYS.iter().map(|k| Value::String((*k).to_string())).collect()
});

// ---------------------------------------------------------------------------
// Item-type mapping
// ---------------------------------------------------------------------------

fn item_type_from_str(s: &str) -> Option<AssetItemType> {
    match s {
        "public.heic"
        | "public.heif"
        | "public.jpeg"
        | "public.png"
        | "com.adobe.raw-image"
        | "com.canon.cr2-raw-image"
        | "com.canon.crw-raw-image"
        | "com.sony.arw-raw-image"
        | "com.fuji.raw-image"
        | "com.panasonic.rw2-raw-image"
        | "com.nikon.nrw-raw-image"
        | "com.pentax.raw-image"
        | "com.nikon.raw-image"
        | "com.olympus.raw-image"
        | "com.canon.cr3-raw-image"
        | "com.olympus.or-raw-image" => Some(AssetItemType::Image),
        "com.apple.quicktime-movie" => Some(AssetItemType::Movie),
        _ => None,
    }
}

const PHOTO_VERSION_LOOKUP: &[(AssetVersionSize, &str)] = &[
    (AssetVersionSize::Original, "resOriginal"),
    (AssetVersionSize::Alternative, "resOriginalAlt"),
    (AssetVersionSize::Medium, "resJPEGMed"),
    (AssetVersionSize::Thumb, "resJPEGThumb"),
    (AssetVersionSize::Adjusted, "resJPEGFull"),
    (AssetVersionSize::LiveOriginal, "resOriginalVidCompl"),
    (AssetVersionSize::LiveMedium, "resVidMed"),
    (AssetVersionSize::LiveThumb, "resVidSmall"),
];

const VIDEO_VERSION_LOOKUP: &[(AssetVersionSize, &str)] = &[
    (AssetVersionSize::Original, "resOriginal"),
    (AssetVersionSize::Medium, "resVidMed"),
    (AssetVersionSize::Thumb, "resVidSmall"),
];

// ---------------------------------------------------------------------------
// Smart folder definitions
// ---------------------------------------------------------------------------

struct FolderDef {
    obj_type: &'static str,
    list_type: &'static str,
    query_filter: Option<Value>,
}

fn smart_folder_filter(field: &str, value: &str) -> Value {
    json!([{
        "fieldName": field,
        "comparator": "EQUALS",
        "fieldValue": {"type": "STRING", "value": value}
    }])
}

fn smart_folders() -> Vec<(&'static str, FolderDef)> {
    vec![
        (
            "Time-lapse",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Timelapse",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "TIMELAPSE")),
            },
        ),
        (
            "Videos",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Video",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "VIDEO")),
            },
        ),
        (
            "Slo-mo",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Slomo",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "SLOMO")),
            },
        ),
        (
            "Bursts",
            FolderDef {
                obj_type: "CPLAssetBurstStackAssetByAssetDate",
                list_type: "CPLBurstStackAssetAndMasterByAssetDate",
                query_filter: None,
            },
        ),
        (
            "Favorites",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Favorite",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "FAVORITE")),
            },
        ),
        (
            "Panoramas",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Panorama",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "PANORAMA")),
            },
        ),
        (
            "Screenshots",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Screenshot",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "SCREENSHOT")),
            },
        ),
        (
            "Live",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Live",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "LIVE")),
            },
        ),
        (
            "Recently Deleted",
            FolderDef {
                obj_type: "CPLAssetDeletedByExpungedDate",
                list_type: "CPLAssetAndMasterDeletedByExpungedDate",
                query_filter: None,
            },
        ),
        (
            "Hidden",
            FolderDef {
                obj_type: "CPLAssetHiddenByAssetDate",
                list_type: "CPLAssetAndMasterHiddenByAssetDate",
                query_filter: None,
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// URL parameter encoding helper
// ---------------------------------------------------------------------------

fn encode_params(params: &HashMap<String, Value>) -> String {
    use std::borrow::Cow;
    let pairs: Vec<String> = params
        .iter()
        .map(|(k, v)| {
            let val: Cow<'_, str> = match v {
                Value::String(s) => Cow::Borrowed(s.as_str()),
                Value::Bool(b) => Cow::Owned(b.to_string()),
                Value::Number(n) => Cow::Owned(n.to_string()),
                other => Cow::Owned(other.to_string()),
            };
            format!("{}={}", urlencoding::encode(k), urlencoding::encode(&val))
        })
        .collect();
    pairs.join("&")
}

// ---------------------------------------------------------------------------
// PhotoAsset
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PhotoAsset {
    master_record: Value,
    asset_record: Value,
}

#[allow(dead_code)]
impl PhotoAsset {
    pub fn new(master_record: Value, asset_record: Value) -> Self {
        Self {
            master_record,
            asset_record,
        }
    }

    /// The unique record name from the master record.
    pub fn id(&self) -> &str {
        self.master_record["recordName"]
            .as_str()
            .unwrap_or_default()
    }

    /// Decode the filename from the `filenameEnc` field.
    /// Returns `None` when the field is absent.
    pub fn filename(&self) -> Option<String> {
        let fields = &self.master_record["fields"];
        let enc = &fields["filenameEnc"];
        if enc.is_null() {
            return None;
        }
        let value = enc["value"].as_str()?;
        let enc_type = enc["type"].as_str().unwrap_or("STRING");
        match enc_type {
            "STRING" => Some(value.to_string()),
            "ENCRYPTED_BYTES" => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .ok()?;
                String::from_utf8(decoded).ok()
            }
            other => {
                warn!("Unsupported filenameEnc type: {}", other);
                None
            }
        }
    }

    /// Size in bytes of the original resource.
    pub fn size(&self) -> u64 {
        self.master_record["fields"]["resOriginalRes"]["value"]["size"]
            .as_u64()
            .unwrap_or(0)
    }

    /// The asset date (when the photo/video was taken), in UTC.
    pub fn asset_date(&self) -> DateTime<Utc> {
        self.asset_record["fields"]["assetDate"]["value"]
            .as_f64()
            .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
            .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
    }

    /// Convenience alias – returns `asset_date` converted to local time.
    pub fn created(&self) -> DateTime<Utc> {
        self.asset_date()
    }

    /// The date the asset was added to the library.
    pub fn added_date(&self) -> DateTime<Utc> {
        self.asset_record["fields"]["addedDate"]["value"]
            .as_f64()
            .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
            .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
    }

    /// Determine the item type from the `itemType` field.
    pub fn item_type(&self) -> Option<AssetItemType> {
        let item_type_str = self.master_record["fields"]["itemType"]["value"].as_str()?;
        if let Some(t) = item_type_from_str(item_type_str) {
            return Some(t);
        }
        // Fallback: guess from filename extension
        if let Some(name) = self.filename() {
            let lower = name.to_lowercase();
            if lower.ends_with(".heic")
                || lower.ends_with(".png")
                || lower.ends_with(".jpg")
                || lower.ends_with(".jpeg")
            {
                return Some(AssetItemType::Image);
            }
        }
        Some(AssetItemType::Movie)
    }

    /// Build the map of available versions for this asset.
    pub fn versions(&self) -> HashMap<AssetVersionSize, AssetVersion> {
        let lookup = if self.item_type() == Some(AssetItemType::Movie) {
            VIDEO_VERSION_LOOKUP
        } else {
            PHOTO_VERSION_LOOKUP
        };

        let mut versions = HashMap::new();
        for (key, prefix) in lookup {
            let res_field = format!("{prefix}Res");
            let type_field = format!("{prefix}FileType");

            // Try asset record first, then master record.
            let fields = if !self.asset_record["fields"][&res_field].is_null() {
                &self.asset_record["fields"]
            } else if !self.master_record["fields"][&res_field].is_null() {
                &self.master_record["fields"]
            } else {
                continue;
            };

            let res_entry = &fields[&res_field]["value"];
            if res_entry.is_null() {
                continue;
            }

            let size = res_entry["size"].as_u64().unwrap_or(0);
            let url = res_entry["downloadURL"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let checksum = res_entry["fileChecksum"]
                .as_str()
                .unwrap_or_default()
                .to_string();

            let asset_type = fields[&type_field]["value"]
                .as_str()
                .unwrap_or_default()
                .to_string();

            versions.insert(
                *key,
                AssetVersion {
                    size,
                    url,
                    asset_type,
                    checksum,
                },
            );
        }
        versions
    }
}

impl std::fmt::Display for PhotoAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<PhotoAsset: id={}>", self.id())
    }
}

// ---------------------------------------------------------------------------
// PhotoAlbum
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct PhotoAlbum {
    pub(crate) name: String,
    params: HashMap<String, Value>,
    session: Box<dyn PhotosSession>,
    service_endpoint: String,
    list_type: String,
    obj_type: String,
    query_filter: Option<Value>,
    page_size: usize,
    zone_id: Value,
}

#[allow(dead_code)]
impl PhotoAlbum {
    pub fn new(
        params: HashMap<String, Value>,
        session: Box<dyn PhotosSession>,
        service_endpoint: String,
        name: String,
        list_type: String,
        obj_type: String,
        query_filter: Option<Value>,
        page_size: usize,
        zone_id: Value,
    ) -> Self {
        Self {
            name,
            params,
            session,
            service_endpoint,
            list_type,
            obj_type,
            query_filter,
            page_size,
            zone_id,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return total item count for this album via `HyperionIndexCountLookup`.
    pub async fn len(&self) -> anyhow::Result<u64> {
        let url = format!(
            "{}/internal/records/query/batch?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "batch": [{
                "resultsLimit": 1,
                "query": {
                    "filterBy": {
                        "fieldName": "indexCountID",
                        "fieldValue": {
                            "type": "STRING_LIST",
                            "value": [&self.obj_type]
                        },
                        "comparator": "IN",
                    },
                    "recordType": "HyperionIndexCountLookup",
                },
                "zoneWide": true,
                "zoneID": self.zone_id,
            }]
        });

        let response = self
            .session
            .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
            .await?;

        let count = response["batch"][0]["records"][0]["fields"]["itemCount"]["value"]
            .as_u64()
            .unwrap_or(0);
        Ok(count)
    }

    /// Fetch all photos in this album, handling pagination.
    pub async fn photos(&self) -> anyhow::Result<Vec<PhotoAsset>> {
        let mut all_assets: Vec<PhotoAsset> = Vec::new();
        let mut offset: u64 = 0;

        loop {
            let url = format!(
                "{}/records/query?{}",
                self.service_endpoint,
                encode_params(&self.params)
            );
            let body = self.list_query(offset);
            debug!("Album '{}' POST URL: {}", self.name, url);
            let response = self
                .session
                .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
                .await?;
            debug!("Album '{}' response: {}", self.name, serde_json::to_string_pretty(&response).unwrap_or_default());

            let mut response = response;
            let records = match response.get_mut("records").and_then(|v| v.as_array_mut()) {
                Some(r) => std::mem::take(r),
                None => {
                    debug!("No 'records' field in response for album '{}'", self.name);
                    break;
                }
            };

            debug!(
                "Album '{}': got {} records at offset {}",
                self.name,
                records.len(),
                offset
            );

            let mut asset_records: HashMap<String, Value> = HashMap::new();
            let mut master_records: Vec<Value> = Vec::new();

            for mut rec in records {
                let record_type = rec["recordType"].as_str().unwrap_or_default();
                debug!("  record type: {}", record_type);
                if record_type == "CPLAsset" {
                    if let Some(master_id) =
                        rec["fields"]["masterRef"]["value"]["recordName"].as_str()
                    {
                        let master_id = master_id.to_string();
                        asset_records.insert(master_id, std::mem::take(&mut rec));
                    }
                } else if record_type == "CPLMaster" {
                    master_records.push(std::mem::take(&mut rec));
                }
            }

            if master_records.is_empty() {
                break;
            }

            for master in master_records {
                let record_name = master["recordName"].as_str().unwrap_or_default();
                if let Some(asset_rec) = asset_records.remove(record_name) {
                    all_assets.push(PhotoAsset::new(master, asset_rec));
                } else {
                    offset += 1;
                    continue;
                }
                offset += 1;
            }
        }

        Ok(all_assets)
    }

    fn list_query(&self, offset: u64) -> Value {
        let desired_keys = &*DESIRED_KEYS_VALUES;

        let mut filter_by = vec![
            json!({
                "fieldName": "startRank",
                "fieldValue": {"type": "INT64", "value": offset},
                "comparator": "EQUALS",
            }),
            json!({
                "fieldName": "direction",
                "fieldValue": {"type": "STRING", "value": "ASCENDING"},
                "comparator": "EQUALS",
            }),
        ];

        if let Some(ref qf) = self.query_filter {
            if let Some(arr) = qf.as_array() {
                filter_by.extend(arr.iter().cloned());
            }
        }

        let query_part = json!({
            "filterBy": &filter_by,
            "recordType": &self.list_type,
        });
        debug!("list_query filterBy ({} items): {}", filter_by.len(), serde_json::to_string(&query_part).unwrap_or_default());
        debug!("list_query zoneID: {}", serde_json::to_string(&self.zone_id).unwrap_or_default());

        json!({
            "query": {
                "filterBy": filter_by,
                "recordType": &self.list_type,
            },
            "resultsLimit": self.page_size * 2,
            "desiredKeys": desired_keys,
            "zoneID": self.zone_id,
        })
    }
}

impl std::fmt::Display for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl std::fmt::Debug for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<PhotoAlbum: '{}'>", self.name)
    }
}

// ---------------------------------------------------------------------------
// PhotoLibrary
// ---------------------------------------------------------------------------

pub struct PhotoLibrary {
    service_endpoint: String,
    params: HashMap<String, Value>,
    session: Box<dyn PhotosSession>,
    zone_id: Value,
    library_type: String,
}

#[allow(dead_code)]
impl PhotoLibrary {
    /// Create a new `PhotoLibrary`, verifying that indexing has finished.
    pub async fn new(
        service_endpoint: String,
        params: HashMap<String, Value>,
        session: Box<dyn PhotosSession>,
        zone_id: Value,
        library_type: String,
    ) -> Result<Self, ICloudError> {
        let url = format!(
            "{}/records/query?{}",
            service_endpoint,
            encode_params(&params)
        );
        let body = json!({
            "query": {"recordType": "CheckIndexingState"},
            "zoneID": &zone_id,
        });

        let response = session
            .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
            .await
            .map_err(|e| ICloudError::Connection(e.to_string()))?;

        let indexing_state = response["records"][0]["fields"]["state"]["value"]
            .as_str()
            .unwrap_or("");
        if indexing_state != "FINISHED" {
            return Err(ICloudError::IndexingNotFinished);
        }

        Ok(Self {
            service_endpoint,
            params,
            session,
            zone_id,
            library_type,
        })
    }

    /// Return smart-folder albums plus user-created albums.
    pub async fn albums(&self) -> anyhow::Result<HashMap<String, PhotoAlbum>> {
        let mut albums = HashMap::new();

        // Smart folders
        for (name, def) in smart_folders() {
            albums.insert(
                name.to_string(),
                PhotoAlbum::new(
                    self.params.clone(),
                    self.clone_session(),
                    self.service_endpoint.clone(),
                    name.to_string(),
                    def.list_type.to_string(),
                    def.obj_type.to_string(),
                    def.query_filter,
                    100,
                    self.zone_id.clone(),
                ),
            );
        }

        // User albums (skip for shared libraries)
        if self.library_type != "shared" {
            let folders = self.fetch_folders().await?;
            for folder in folders {
                let record_name = folder["recordName"].as_str().unwrap_or_default();
                if record_name == ROOT_FOLDER
                    || record_name == PROJECT_ROOT_FOLDER
                {
                    continue;
                }
                // Skip deleted folders
                if folder["fields"]["isDeleted"]["value"]
                    .as_bool()
                    .unwrap_or(false)
                {
                    continue;
                }

                let folder_id = record_name.to_string();
                let folder_obj_type =
                    format!("CPLContainerRelationNotDeletedByAssetDate:{folder_id}");

                let folder_name = match folder["fields"]["albumNameEnc"]["value"].as_str() {
                    Some(enc) => {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(enc)
                            .unwrap_or_default();
                        String::from_utf8(decoded).unwrap_or_else(|_| folder_id.clone())
                    }
                    None => folder_id.clone(),
                };

                let query_filter = Some(json!([{
                    "fieldName": "parentId",
                    "comparator": "EQUALS",
                    "fieldValue": {"type": "STRING", "value": &folder_id},
                }]));

                albums.insert(
                    folder_name.clone(),
                    PhotoAlbum::new(
                        self.params.clone(),
                        self.clone_session(),
                        self.service_endpoint.clone(),
                        folder_name,
                        "CPLContainerRelationLiveByAssetDate".to_string(),
                        folder_obj_type,
                        query_filter,
                        100,
                        self.zone_id.clone(),
                    ),
                );
            }
        }

        Ok(albums)
    }

    /// Convenience: return a `PhotoAlbum` representing the whole collection.
    pub fn all(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            self.params.clone(),
            self.clone_session(),
            self.service_endpoint.clone(),
            String::new(),
            "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".to_string(),
            "CPLAssetByAssetDateWithoutHiddenOrDeleted".to_string(),
            None,
            100,
            self.zone_id.clone(),
        )
    }

    /// Convenience: return a `PhotoAlbum` for recently deleted items.
    pub fn recently_deleted(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            self.params.clone(),
            self.clone_session(),
            self.service_endpoint.clone(),
            String::new(),
            "CPLAssetAndMasterDeletedByExpungedDate".to_string(),
            "CPLAssetDeletedByExpungedDate".to_string(),
            None,
            100,
            self.zone_id.clone(),
        )
    }

    // -- internal helpers --

    async fn fetch_folders(&self) -> anyhow::Result<Vec<Value>> {
        let url = format!(
            "{}/records/query?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "query": {"recordType": "CPLAlbumByPositionLive"},
            "zoneID": &self.zone_id,
        });
        let response = self
            .session
            .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
            .await?;

        let records = response["records"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(records)
    }

    /// Clone the session as a boxed trait object, preserving the original
    /// client's cookies and configuration.
    fn clone_session(&self) -> Box<dyn PhotosSession> {
        self.session.clone_box()
    }
}

// ---------------------------------------------------------------------------
// PhotosService
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct PhotosService {
    service_root: String,
    session: Box<dyn PhotosSession>,
    params: HashMap<String, Value>,
    primary_library: PhotoLibrary,
    private_libraries: Option<HashMap<String, PhotoLibrary>>,
    shared_libraries: Option<HashMap<String, PhotoLibrary>>,
}

#[allow(dead_code)]
impl PhotosService {
    /// Create a new `PhotosService`.
    ///
    /// This checks that the primary library has finished indexing.
    pub async fn new(
        service_root: String,
        session: Box<dyn PhotosSession>,
        mut params: HashMap<String, Value>,
    ) -> Result<Self, ICloudError> {
        params.insert("remapEnums".to_string(), Value::Bool(true));
        params.insert("getCurrentSyncToken".to_string(), Value::Bool(true));

        let service_endpoint = Self::build_service_endpoint(&service_root, "private");
        let zone_id = json!({"zoneName": "PrimarySync"});

        // Clone the session for the primary library.
        let lib_session = session.clone_box();

        let primary_library = PhotoLibrary::new(
            service_endpoint,
            params.clone(),
            lib_session,
            zone_id,
            "private".to_string(),
        )
        .await?;

        Ok(Self {
            service_root,
            session,
            params,
            primary_library,
            private_libraries: None,
            shared_libraries: None,
        })
    }

    /// Compute the service endpoint URL for a given library type.
    pub fn get_service_endpoint(&self, library_type: &str) -> String {
        Self::build_service_endpoint(&self.service_root, library_type)
    }

    fn build_service_endpoint(service_root: &str, library_type: &str) -> String {
        format!("{service_root}/database/1/com.apple.photos.cloud/production/{library_type}")
    }

    /// Return albums from the primary library.
    pub async fn albums(&self) -> anyhow::Result<HashMap<String, PhotoAlbum>> {
        self.primary_library.albums().await
    }

    /// Return a `PhotoAlbum` for the entire primary collection.
    pub fn all(&self) -> PhotoAlbum {
        self.primary_library.all()
    }

    /// Fetch private libraries (lazily, first call triggers the HTTP request).
    pub async fn fetch_private_libraries(
        &mut self,
    ) -> anyhow::Result<&HashMap<String, PhotoLibrary>> {
        if self.private_libraries.is_none() {
            let libs = self.fetch_libraries("private").await?;
            self.private_libraries = Some(libs);
        }
        Ok(self.private_libraries.as_ref().unwrap())
    }

    /// Fetch shared libraries (lazily, first call triggers the HTTP request).
    pub async fn fetch_shared_libraries(
        &mut self,
    ) -> anyhow::Result<&HashMap<String, PhotoLibrary>> {
        if self.shared_libraries.is_none() {
            let libs = self.fetch_libraries("shared").await?;
            self.shared_libraries = Some(libs);
        }
        Ok(self.shared_libraries.as_ref().unwrap())
    }

    async fn fetch_libraries(&self, library_type: &str) -> anyhow::Result<HashMap<String, PhotoLibrary>> {
        let mut libraries = HashMap::new();
        let service_endpoint = self.get_service_endpoint(library_type);
        let url = format!("{service_endpoint}/zones/list");

        let response = self
            .session
            .post(&url, "{}", &[("Content-type", "text/plain")])
            .await?;

        let zones = match response["zones"].as_array() {
            Some(z) => z,
            None => return Ok(libraries),
        };

        for zone in zones {
            if zone.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            let zone_name = zone["zoneID"]["zoneName"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let zone_id = zone["zoneID"].clone();
            let ep = self.get_service_endpoint(library_type);
            let lib_session = self.session.clone_box();

            match PhotoLibrary::new(
                ep,
                self.params.clone(),
                lib_session,
                zone_id,
                library_type.to_string(),
            )
            .await
            {
                Ok(lib) => {
                    debug!("Loaded library zone: {}", zone_name);
                    libraries.insert(zone_name, lib);
                }
                Err(e) => {
                    error!("Failed to load library zone {}: {}", zone_name, e);
                }
            }
        }

        Ok(libraries)
    }
}
