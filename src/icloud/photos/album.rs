use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use anyhow::Context;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::Stream;

use super::asset::{ChangeEvent, DeltaRecordBuffer, PhotoAsset};
use super::cloudkit::ChangesZoneResponse;
use super::queries::{build_changes_zone_request, encode_params, DESIRED_KEYS_VALUES};
use super::session::{check_changes_zone_error, PhotosSession};
use crate::retry::RetryConfig;

/// How many consecutive empty /records/query pages trigger true EOF.
///
/// CloudKit's /records/query does not expose a `moreComing` flag; an empty
/// page can be either real end-of-list or a transient gap at this rank
/// range (e.g., a block of fully-deleted records aligning with a page
/// boundary). We probe forward by one `page_size` on each empty page and
/// only terminate after this many consecutive empty probes.
///
/// Set conservatively so a multi-page run of fully-deleted records does
/// not silently truncate enumeration; the cost on true EOF is at most
/// `MAX_EMPTY_PAGE_PROBES - 1` extra empty requests per fetcher.
const MAX_EMPTY_PAGE_PROBES: u32 = 5;

/// A boxed, pinned stream of photo asset results.
type PhotoStream = Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>>;

/// Await all fetcher handles, logging and returning `true` if any panicked.
async fn await_fetcher_handles(handles: Vec<JoinHandle<()>>) -> bool {
    let mut panicked = false;
    for handle in handles {
        if let Err(e) = handle.await {
            if e.is_panic() {
                tracing::error!(error = ?e, "Photo fetcher task panicked");
                panicked = true;
            }
        }
    }
    panicked
}

/// A boxed, pinned stream of change event results.
type ChangeStream = Pin<Box<dyn Stream<Item = anyhow::Result<ChangeEvent>> + Send + 'static>>;

/// Determine how many parallel fetcher tasks to spawn.
///
/// We never spawn more fetchers than total pages (no empty fetchers)
/// and never more than the requested concurrency level.
fn determine_fetcher_count(total_items: u64, page_size: usize, concurrency: usize) -> usize {
    let total_pages = total_items.div_ceil(page_size as u64);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded to concurrency (usize) immediately via .min()"
    )]
    let pages_as_usize = total_pages as usize;
    pages_as_usize.min(concurrency).max(1)
}

/// Metadata at DEBUG; raw body only at TRACE. Including the body in
/// the DEBUG event allocates ~MB per page on busy libraries (every
/// fetched page formats the full response value).
fn log_fetcher_response(album: &str, response: &Value) {
    tracing::debug!(album = %album, "Fetcher response");
    tracing::trace!(
        album = %album,
        response = %response,
        "Fetcher response body",
    );
}

/// Return a full-enumeration sync token only when every fetcher that reported
/// one agreed. A single overwritten token is unsafe: if two parallel fetchers
/// observed different zone tokens, advancing either token could skip records
/// that were not present in the other fetcher's snapshot.
fn unanimous_fetcher_sync_token(album: &str, tokens: &[String]) -> Option<String> {
    let first = tokens.first()?;
    if tokens.iter().all(|token| token == first) {
        return Some(first.clone());
    }

    let mut unique_tokens = FxHashSet::default();
    for token in tokens {
        unique_tokens.insert(token.as_str());
    }
    tracing::warn!(
        album,
        token_count = tokens.len(),
        unique_token_count = unique_tokens.len(),
        "Full enumeration syncToken mismatch across parallel fetchers; \
         blocking sync token advancement"
    );
    None
}

/// Configuration for creating a `PhotoAlbum`, bundling all non-session fields.
#[derive(Debug)]
pub struct PhotoAlbumConfig {
    pub params: Arc<HashMap<String, Value>>,
    pub service_endpoint: Arc<str>,
    pub name: Arc<str>,
    pub list_type: Arc<str>,
    pub obj_type: Arc<str>,
    pub query_filter: Option<Arc<Value>>,
    pub page_size: usize,
    pub zone_id: Arc<Value>,
    pub retry_config: RetryConfig,
}

pub struct PhotoAlbum {
    pub(crate) name: Arc<str>,
    params: Arc<HashMap<String, Value>>,
    session: Box<dyn PhotosSession>,
    service_endpoint: Arc<str>,
    list_type: Arc<str>,
    obj_type: Arc<str>,
    query_filter: Option<Arc<Value>>,
    page_size: usize,
    zone_id: Arc<Value>,
    retry_config: RetryConfig,
}

impl std::fmt::Debug for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotoAlbum")
            .field("name", &self.name)
            .field("service_endpoint", &self.service_endpoint)
            .field("list_type", &self.list_type)
            .field("obj_type", &self.obj_type)
            .field("page_size", &self.page_size)
            .finish_non_exhaustive()
    }
}

impl PhotoAlbum {
    pub fn new(config: PhotoAlbumConfig, session: Box<dyn PhotosSession>) -> Self {
        Self {
            name: config.name,
            params: config.params,
            session,
            service_endpoint: config.service_endpoint,
            list_type: config.list_type,
            obj_type: config.obj_type,
            query_filter: config.query_filter,
            page_size: config.page_size,
            zone_id: config.zone_id,
            retry_config: config.retry_config,
        }
    }

    /// Return the CloudKit zone name this album belongs to
    /// (e.g. `PrimarySync`, `SharedSync-<uuid>`). Falls back to an empty
    /// string if the zone_id JSON lacks a `zoneName` field, which should
    /// only happen in hand-constructed test fixtures.
    pub fn zone_name(&self) -> &str {
        self.zone_id
            .get("zoneName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
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
                            "value": [&*self.obj_type]
                        },
                        "comparator": "IN",
                    },
                    "recordType": "HyperionIndexCountLookup",
                },
                "zoneWide": true,
                "zoneID": *self.zone_id,
            }]
        });

        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;

        let batch: super::cloudkit::BatchQueryResponse =
            serde_json::from_value(response).context("failed to parse album count response")?;
        let count = batch
            .batch
            .first()
            .and_then(|q| q.records.first())
            .and_then(|r| {
                r.fields
                    .get("itemCount")
                    .and_then(|f| f.get("value"))
                    .and_then(Value::as_u64)
            })
            .unwrap_or(0);
        Ok(count)
    }

    /// Convenience wrapper over `photo_stream()` that collects all assets
    /// into a `Vec`. Prefer `photo_stream()` when memory is a concern.
    ///
    /// Fetcher panics are surfaced as an `Err` so the caller cannot mistake
    /// a truncated enumeration for a complete one. Propagating the real
    /// panic payload back through `anyhow` isn't worth the ceremony — a
    /// sentinel string is enough for the operator to know the enumeration
    /// was incomplete and to correlate with the fetcher's prior
    /// `tracing::error!` log line.
    pub async fn photos(&self, limit: Option<u32>) -> anyhow::Result<Vec<PhotoAsset>> {
        use tokio_stream::StreamExt;
        let (stream, panic_rx) = self.photo_stream(limit, None, 1);
        let items = stream.collect::<Result<Vec<_>, _>>().await?;
        if panic_rx.await.unwrap_or(false) {
            anyhow::bail!(
                "photo enumeration aborted: a fetcher task panicked; \
                 results are incomplete, see earlier error log"
            );
        }
        Ok(items)
    }

    /// Stream photos page-by-page without buffering the full album in memory.
    ///
    /// Returns the stream paired with a `oneshot::Receiver<bool>` that
    /// yields `true` once every fetcher task has completed **iff any
    /// fetcher panicked**. The caller should await the receiver
    /// **after** the stream is exhausted and fail the enumeration if
    /// the flag is set — otherwise a panicked fetcher presents as a
    /// silently truncated stream (a "No silent failures" violation).
    ///
    /// When `total_count` is provided and `concurrency > 1`, the offset range
    /// is partitioned across multiple parallel fetcher tasks for faster
    /// enumeration. Each fetcher pages through its assigned slice and sends
    /// assets into a shared channel. When `total_count` is `None` or
    /// `concurrency` is 1, a single sequential fetcher is used (original
    /// behavior).
    ///
    /// The channel buffer is `page_size * num_fetchers`, giving each fetcher
    /// one page of headroom before back-pressure kicks in.
    pub fn photo_stream(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        concurrency: usize,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<bool>) {
        let (panic_tx, panic_rx) = tokio::sync::oneshot::channel();
        let (stream, handles) = self.photo_stream_inner(limit, total_count, concurrency, None);
        tokio::spawn(async move {
            let panicked = await_fetcher_handles(handles).await;
            let _ = panic_tx.send(panicked);
        });
        (stream, panic_rx)
    }

    /// Like [`photo_stream()`](Self::photo_stream), but also returns a
    /// `oneshot::Receiver` that will yield the zone-level `syncToken` from
    /// the last API response page once the stream is fully consumed.
    ///
    /// The caller should `.await` the receiver **after** the stream is
    /// exhausted:
    ///
    /// ```ignore
    /// let (stream, token_rx) = album.photo_stream_with_token(limit, count, concurrency);
    /// tokio::pin!(stream);
    /// while let Some(item) = stream.next().await { /* ... */ }
    /// let sync_token = token_rx.await.ok().flatten();
    /// ```
    pub fn photo_stream_with_token(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        concurrency: usize,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let fetcher_sync_tokens: Arc<tokio::sync::Mutex<Vec<String>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));

        let (stream, handles) = self.photo_stream_inner(
            limit,
            total_count,
            concurrency,
            Some(fetcher_sync_tokens.clone()),
        );
        let album_name = Arc::clone(&self.name);

        // Spawn a monitor task that waits for all fetcher tasks to complete,
        // then delivers the captured syncToken through the oneshot channel.
        // The fetchers' mpsc senders are dropped when they finish, which
        // closes the ReceiverStream. The caller awaits the oneshot after the
        // stream is exhausted.
        tokio::spawn(async move {
            let fetcher_panicked = await_fetcher_handles(handles).await;
            // Suppress sync token if any fetcher panicked — the enumeration
            // is incomplete and the next sync must do a full re-enumeration.
            let final_token = if fetcher_panicked {
                None
            } else {
                let tokens = fetcher_sync_tokens.lock().await;
                unanimous_fetcher_sync_token(&album_name, &tokens)
            };
            let _ = token_tx.send(final_token);
        });

        (stream, token_rx)
    }

    /// Stream record changes since the given syncToken via `changes/zone`.
    ///
    /// Returns a stream of `ChangeEvent`s and a oneshot receiver for the final syncToken.
    /// The syncToken is sent through the oneshot after all pages have been consumed
    /// (moreComing: false), or on error with the last successfully consumed token.
    ///
    /// This method is inherently sequential -- each page's syncToken feeds the next request.
    /// No parallel fetchers.
    pub fn changes_stream(
        &self,
        sync_token: &str,
    ) -> (ChangeStream, tokio::sync::oneshot::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<anyhow::Result<ChangeEvent>>(200);
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();

        let session = self.session.clone_box();
        let service_endpoint = Arc::clone(&self.service_endpoint);
        let params = Arc::clone(&self.params);
        let zone_id = Arc::clone(&self.zone_id);
        let initial_token = sync_token.to_string();
        let album_name = Arc::clone(&self.name);
        let retry_config = self.retry_config;

        tokio::spawn(async move {
            let mut buffer = DeltaRecordBuffer::new();
            let mut current_token = initial_token;

            let url = format!(
                "{}/changes/zone?{}",
                service_endpoint,
                encode_params(&params)
            );

            let stream_error: Option<anyhow::Error> = loop {
                let body = build_changes_zone_request(&zone_id, Some(&current_token), 200);
                tracing::debug!(
                    album = %album_name,
                    token = %current_token,
                    "changes/zone request"
                );

                let response = match super::session::retry_post(
                    session.as_ref(),
                    &url,
                    &body.to_string(),
                    &[("Content-type", "text/plain")],
                    &retry_config,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => break Some(e),
                };

                let changes_resp: ChangesZoneResponse = match serde_json::from_value(response) {
                    Ok(r) => r,
                    Err(e) => break Some(e.into()),
                };

                let Some(zone_result) = changes_resp.zones.into_iter().next() else {
                    break Some(anyhow::anyhow!("changes/zone returned empty zones array"));
                };

                // Check for zone-level errors BEFORE advancing current_token.
                // On any zone error (including transient RETRY_LATER), the loop
                // breaks with current_token still set to the last-known-good
                // value so the caller can retry from a valid checkpoint.
                let zone_name = zone_result.zone_id.zone_name.clone();
                if let Err(sync_err) = check_changes_zone_error(
                    zone_result.server_error_code.as_deref(),
                    zone_result.reason.as_deref(),
                    &zone_name,
                ) {
                    break Some(sync_err.into());
                }

                current_token = zone_result.sync_token;
                let more_coming = zone_result.more_coming;

                tracing::debug!(
                    album = %album_name,
                    records = zone_result.records.len(),
                    more_coming,
                    new_token = %current_token,
                    "changes/zone page received"
                );

                let events = buffer.process_records(zone_result.records);
                for event in events {
                    if tx.send(Ok(event)).await.is_err() {
                        // Receiver dropped -- no one to flush to
                        let _ = token_tx.send(current_token);
                        return;
                    }
                }

                if !more_coming {
                    break None;
                }
            };

            // Always flush unpaired records, even on error
            let flush_events = buffer.flush();
            if stream_error.is_some() && !flush_events.is_empty() {
                tracing::warn!(
                    album = %album_name,
                    orphaned = flush_events.len(),
                    "flushing unpaired records after stream error"
                );
            }
            for event in flush_events {
                if tx.send(Ok(event)).await.is_err() {
                    let _ = token_tx.send(current_token);
                    return;
                }
            }

            if let Some(e) = stream_error {
                let _ = tx.send(Err(e)).await;
            }

            let _ = token_tx.send(current_token);
        });

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            token_rx,
        )
    }

    /// Shared implementation for `photo_stream` and `photo_stream_with_token`.
    ///
    /// When `fetcher_sync_tokens` is `Some`, each fetcher appends its last
    /// observed `syncToken` to the shared token list.
    ///
    /// Returns the stream and all spawned fetcher `JoinHandle`s.
    fn photo_stream_inner(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        concurrency: usize,
        fetcher_sync_tokens: Option<Arc<tokio::sync::Mutex<Vec<String>>>>,
    ) -> (PhotoStream, Vec<JoinHandle<()>>) {
        let page_size = self.page_size;
        let mut handles = Vec::new();

        // Compute effective total, capped by --recent if set.
        let effective_total = total_count
            .map(|tc| limit.map_or(tc, |lim| tc.min(u64::from(lim))))
            .or_else(|| limit.map(u64::from));

        // Use 2x concurrency for enumeration fetchers — Apple's CloudKit
        // doesn't throttle at these levels and it halves enumeration time.
        let (num_fetchers, parallel_total) = match effective_total {
            Some(total) if concurrency > 1 => {
                let num_fetchers = determine_fetcher_count(total, page_size, concurrency * 2);
                if num_fetchers > 1 {
                    (num_fetchers, Some(total))
                } else {
                    (1, None)
                }
            }
            _ => (1, None),
        };

        let (tx, rx) =
            mpsc::channel::<anyhow::Result<PhotoAsset>>((page_size * num_fetchers).min(500));

        if let Some(total) = parallel_total {
            // Partition offset range into non-overlapping chunks aligned to
            // page_size boundaries so each fetcher starts on a clean page.
            let chunk_size_items = {
                let raw = total.div_ceil(num_fetchers as u64);
                // Round up to next page_size boundary
                let ps = page_size as u64;
                raw.div_ceil(ps) * ps
            };

            tracing::debug!(
                fetchers = num_fetchers,
                chunk_size = chunk_size_items,
                total = total,
                "Parallel photo enumeration"
            );

            for i in 0..num_fetchers {
                let start = i as u64 * chunk_size_items;
                let end = ((i as u64 + 1) * chunk_size_items).min(total);
                if start >= total {
                    break;
                }
                // Per-fetcher limit: don't exceed the chunk size, and for the
                // last fetcher also respect the global --recent cap.
                let fetcher_limit = match limit {
                    Some(lim) => {
                        let remaining = u64::from(lim).saturating_sub(start);
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "bounded by min(end-start, limit) where both operands originated from u32 fetcher limits"
                        )]
                        let capped = remaining.min(end - start) as u32;
                        Some(capped)
                    }
                    None => None,
                };
                handles.push(self.spawn_fetcher(
                    tx.clone(),
                    start,
                    end,
                    fetcher_limit,
                    fetcher_sync_tokens.clone(),
                ));
            }
            // Drop our sender so channel closes when all fetchers finish.
            drop(tx);
        } else {
            tracing::info!("Fetching photos from iCloud...");
            // Move tx directly — no clone needed for a single fetcher.
            handles.push(self.spawn_fetcher(tx, 0, u64::MAX, limit, fetcher_sync_tokens));
        }

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            handles,
        )
    }

    /// Spawn a background tokio task that pages through records from
    /// `start_offset` up to (but not including) `end_offset`, sending each
    /// `PhotoAsset` into `tx`. The task stops when:
    /// - `offset >= end_offset`
    /// - the API returns zero records (end of album)
    /// - the per-fetcher `limit` is reached
    /// - the receiver is dropped
    ///
    /// If `fetcher_sync_tokens` is provided, the fetcher appends the last
    /// non-None `syncToken` it observed. The monitor task compares every
    /// fetcher's final token before allowing token advancement.
    fn spawn_fetcher(
        &self,
        tx: mpsc::Sender<anyhow::Result<PhotoAsset>>,
        start_offset: u64,
        end_offset: u64,
        limit: Option<u32>,
        fetcher_sync_tokens: Option<Arc<tokio::sync::Mutex<Vec<String>>>>,
    ) -> JoinHandle<()> {
        let session = self.session.clone_box();
        let service_endpoint = Arc::clone(&self.service_endpoint);
        let params = Arc::clone(&self.params);
        let name = Arc::clone(&self.name);
        let list_type = Arc::clone(&self.list_type);
        let query_filter = self.query_filter.as_ref().map(Arc::clone);
        let retry_config = self.retry_config;
        let page_size = self.page_size;
        let zone_id = Arc::clone(&self.zone_id);

        tokio::spawn(async move {
            let mut offset = start_offset;
            let mut total_sent: u64 = 0;
            let mut last_sync_token: Option<String> = None;
            let mut pending_masters: FxHashMap<String, super::cloudkit::Record> =
                FxHashMap::default();
            let mut consecutive_empty_pages: u32 = 0;
            let url = format!(
                "{}/records/query?{}",
                service_endpoint,
                encode_params(&params)
            );

            loop {
                if offset >= end_offset {
                    break;
                }

                let body = Self::build_list_query(
                    &list_type,
                    query_filter.as_deref(),
                    page_size,
                    &zone_id,
                    offset,
                    "ASCENDING",
                );
                tracing::debug!(
                    album = %name,
                    range_start = start_offset,
                    range_end = end_offset,
                    offset,
                    "Fetcher POST"
                );
                let response = match super::session::retry_post(
                    session.as_ref(),
                    &url,
                    &body.to_string(),
                    &[("Content-type", "text/plain")],
                    &retry_config,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                };
                log_fetcher_response(&name, &response);

                let query: super::cloudkit::QueryResponse = match serde_json::from_value(response) {
                    Ok(q) => q,
                    Err(e) => {
                        tracing::warn!(
                            album = %name,
                            error = %e,
                            "Failed to deserialize fetcher response (body logged above at DEBUG)",
                        );
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };

                // Capture the zone-level syncToken from each page response.
                if let Some(token) = &query.sync_token {
                    last_sync_token = Some(token.clone());
                }

                let records = query.records;
                let record_count = records.len();

                tracing::debug!(
                    album = %name,
                    count = record_count,
                    offset,
                    "Got records"
                );

                // An empty page can mean either true end-of-list or a transient
                // gap at this rank range (e.g., a run of fully-deleted records
                // aligning with a page boundary). The API has no `moreComing`
                // flag on /records/query, so we probe forward by one
                // page_size before committing to EOF. The guard terminates
                // after MAX_EMPTY_PAGE_PROBES consecutive empty pages to avoid
                // unbounded scanning on genuinely empty tails.
                if record_count == 0 {
                    consecutive_empty_pages += 1;
                    if consecutive_empty_pages >= MAX_EMPTY_PAGE_PROBES {
                        // Promoted to info! so an enumeration that
                        // terminates after probing past empty pages is
                        // visible in normal logs — operators chasing a
                        // suspected silent truncation should see the
                        // probe count and total_sent here.
                        tracing::info!(
                            album = %name,
                            offset,
                            probes = consecutive_empty_pages,
                            total_sent,
                            "End of album (consecutive empty pages)"
                        );
                        break;
                    }
                    tracing::debug!(
                        album = %name,
                        offset,
                        probes = consecutive_empty_pages,
                        "Empty page, probing forward one page_size"
                    );
                    offset += page_size as u64;
                    continue;
                }
                consecutive_empty_pages = 0;

                // Collect current page's records, trying to pair with
                // buffered unpaired records from previous pages.
                let mut page_assets: FxHashMap<String, super::cloudkit::Record> =
                    FxHashMap::default();
                let mut page_masters: Vec<super::cloudkit::Record> = Vec::new();

                for rec in records {
                    tracing::debug!(record_type = %rec.record_type, "  record");
                    if rec.record_type == "CPLAsset" {
                        if let Some(master_id) = rec
                            .fields
                            .get("masterRef")
                            .and_then(|f| f.get("value"))
                            .and_then(|v| v.get("recordName"))
                            .and_then(Value::as_str)
                        {
                            let master_id = master_id.to_string();
                            // Try to pair with a buffered master from a previous page
                            if let Some(master) = pending_masters.remove(&master_id) {
                                let asset = PhotoAsset::from_records(master, &rec);
                                if tx.send(Ok(asset)).await.is_err() {
                                    return;
                                }
                                total_sent += 1;
                            } else {
                                page_assets.insert(master_id, rec);
                            }
                        }
                    } else if rec.record_type == "CPLMaster" {
                        page_masters.push(rec);
                    }
                }

                if page_masters.is_empty() {
                    // No masters on this page. Advance offset to avoid
                    // re-requesting the same page forever. Use the unmatched
                    // asset count as a proxy for rank positions covered
                    // (each asset references one master/rank), with a minimum
                    // of 1 to guarantee forward progress.
                    let advance = page_assets.len().max(1) as u64;
                    offset += advance;
                    tracing::warn!(
                        album = %name,
                        record_count,
                        advance,
                        offset,
                        "Page returned records but no CPLMaster entries; advancing offset",
                    );
                }

                let mut limit_reached = false;
                for master in page_masters {
                    if let Some(n) = limit {
                        if total_sent >= u64::from(n) {
                            limit_reached = true;
                            break;
                        }
                    }
                    if let Some(asset_rec) = page_assets.remove(&master.record_name) {
                        let asset = PhotoAsset::from_records(master, &asset_rec);
                        if tx.send(Ok(asset)).await.is_err() {
                            return;
                        }
                        total_sent += 1;
                    } else {
                        // Buffer unpaired master for pairing on subsequent pages
                        pending_masters.insert(master.record_name.clone(), master);
                    }
                    offset += 1;
                }

                tracing::debug!(
                    count = total_sent,
                    pending = pending_masters.len(),
                    range_start = start_offset,
                    "fetched photos so far"
                );

                if limit_reached {
                    break;
                }
            }

            // Log any remaining unpaired masters that couldn't be paired
            if !pending_masters.is_empty() {
                tracing::warn!(
                    count = pending_masters.len(),
                    "Unpaired CPLMaster records after full enumeration (no matching CPLAsset found)"
                );
                for id in pending_masters.keys() {
                    tracing::debug!(master_id = %id, "Unpaired CPLMaster");
                }
            }

            if let (Some(shared), Some(token)) = (&fetcher_sync_tokens, last_sync_token) {
                shared.lock().await.push(token);
            }
        })
    }

    #[cfg(test)]
    fn list_query(&self, offset: u64, direction: &str) -> Value {
        Self::build_list_query(
            &self.list_type,
            self.query_filter.as_deref(),
            self.page_size,
            &self.zone_id,
            offset,
            direction,
        )
    }

    fn build_list_query(
        list_type: &str,
        query_filter: Option<&Value>,
        page_size: usize,
        zone_id: &Value,
        offset: u64,
        direction: &str,
    ) -> Value {
        let desired_keys = &*DESIRED_KEYS_VALUES;

        let mut filter_by = vec![
            json!({
                "fieldName": "startRank",
                "fieldValue": {"type": "INT64", "value": offset},
                "comparator": "EQUALS",
            }),
            json!({
                "fieldName": "direction",
                "fieldValue": {"type": "STRING", "value": direction},
                "comparator": "EQUALS",
            }),
        ];

        if let Some(qf) = query_filter {
            if let Some(arr) = qf.as_array() {
                filter_by.extend(arr.iter().cloned());
            }
        }

        let query_part = json!({
            "filterBy": &filter_by,
            "recordType": list_type,
        });
        tracing::debug!(
            count = filter_by.len(),
            query = %serde_json::to_string(&query_part).unwrap_or_default(),
            "list_query filterBy"
        );
        tracing::debug!(
            zone_id = %serde_json::to_string(zone_id).unwrap_or_default(),
            "list_query zoneID"
        );

        json!({
            "query": {
                "filterBy": filter_by,
                "recordType": list_type,
            },
            // CloudKit returns interleaved CPLMaster + CPLAsset records,
            // so 2 * page_size fetches page_size paired records.
            "resultsLimit": page_size * 2,
            "desiredKeys": desired_keys,
            "zoneID": zone_id,
        })
    }
}

impl std::fmt::Display for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// Panic-on-call `PhotosSession` for tests that inspect a `PhotoAlbum` by
/// name/metadata only. Any actual network call is a test bug.
#[cfg(test)]
pub(crate) struct StubSession;

#[cfg(test)]
#[async_trait::async_trait]
impl PhotosSession for StubSession {
    async fn post(
        &self,
        _url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        unimplemented!("stub")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(StubSession)
    }
}

#[cfg(test)]
impl PhotoAlbum {
    /// Construct a `PhotoAlbum` with the given name for cross-module unit
    /// tests. Wires [`StubSession`], so the album is only safe to inspect by
    /// name/metadata - any network call panics.
    pub(crate) fn stub_for_test(name: Arc<str>) -> Self {
        Self::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name,
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(serde_json::json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
            },
            Box::new(StubSession),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{mock_photo_query_page, MockPhotosFlow, MockPhotosSession};
    use serde_json::json;

    fn make_album(
        page_size: usize,
        query_filter: Option<Arc<Value>>,
        zone_id: Value,
    ) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter,
                page_size,
                zone_id: Arc::new(zone_id),
                retry_config: RetryConfig::default(),
            },
            Box::new(StubSession),
        )
    }

    fn default_zone() -> Value {
        json!({"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner", "zoneType": "REGULAR_CUSTOM_ZONE"})
    }

    #[test]
    fn test_list_query_ascending_offset_zero() {
        let album = make_album(200, None, default_zone());
        let q = album.list_query(0, "ASCENDING");
        let filters = q["query"]["filterBy"].as_array().unwrap();
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0]["fieldValue"]["value"], json!(0));
        assert_eq!(filters[1]["fieldValue"]["value"], "ASCENDING");
    }

    #[test]
    fn test_list_query_with_offset() {
        let album = make_album(200, None, default_zone());
        let q = album.list_query(42, "ASCENDING");
        assert_eq!(q["query"]["filterBy"][0]["fieldValue"]["value"], json!(42));
    }

    #[test]
    fn test_list_query_results_limit_double_page_size() {
        let album = make_album(100, None, default_zone());
        let q = album.list_query(0, "ASCENDING");
        assert_eq!(q["resultsLimit"], json!(200));
    }

    #[test]
    fn test_list_query_with_extra_filter() {
        let extra = json!([{"fieldName": "albumName", "comparator": "EQUALS", "fieldValue": {"type": "STRING", "value": "Favorites"}}]);
        let album = make_album(200, Some(Arc::new(extra)), default_zone());
        let q = album.list_query(0, "ASCENDING");
        let filters = q["query"]["filterBy"].as_array().unwrap();
        assert_eq!(filters.len(), 3);
        assert_eq!(filters[2]["fieldName"], "albumName");
    }

    #[test]
    fn test_list_query_zone_id_passed_through() {
        let zone = json!({"zoneName": "CustomZone"});
        let album = make_album(200, None, zone.clone());
        let q = album.list_query(0, "ASCENDING");
        assert_eq!(q["zoneID"], zone);
    }

    // --- determine_fetcher_count tests ---

    #[test]
    fn test_fetcher_count_single_page() {
        // 50 items, page_size 100, concurrency 10 → 1 page → 1 fetcher
        assert_eq!(determine_fetcher_count(50, 100, 10), 1);
    }

    #[test]
    fn test_fetcher_count_exact_pages() {
        // 500 items, page_size 100, concurrency 10 → 5 pages → 5 fetchers
        assert_eq!(determine_fetcher_count(500, 100, 10), 5);
    }

    #[test]
    fn test_fetcher_count_capped_by_concurrency() {
        // 5000 items, page_size 100, concurrency 10 → 50 pages → capped to 10
        assert_eq!(determine_fetcher_count(5000, 100, 10), 10);
    }

    #[test]
    fn test_fetcher_count_more_pages_than_concurrency() {
        // 50000 items, page_size 100, concurrency 10 → 500 pages → capped to 10
        assert_eq!(determine_fetcher_count(50000, 100, 10), 10);
    }

    #[test]
    fn test_fetcher_count_zero_items() {
        // 0 items → at least 1 fetcher (the loop will just exit immediately)
        assert_eq!(determine_fetcher_count(0, 100, 10), 1);
    }

    #[test]
    fn test_fetcher_count_concurrency_one() {
        // concurrency=1 always gives 1 fetcher
        assert_eq!(determine_fetcher_count(50000, 100, 1), 1);
    }

    #[test]
    fn test_fetcher_count_partial_page() {
        // 150 items, page_size 100 → 2 pages, concurrency 10 → 2 fetchers
        assert_eq!(determine_fetcher_count(150, 100, 10), 2);
    }

    // --- photo_stream parameter tests ---

    #[tokio::test]
    async fn test_photo_stream_no_total_count_uses_single_fetcher() {
        // When total_count is None, should produce a stream (1 sequential fetcher).
        // We can't easily test the internal spawning, but we verify it doesn't panic.
        let album = make_album(100, None, default_zone());
        let (_stream, _panic_rx) = album.photo_stream(None, None, 10);
        // Stream is valid — the fetcher will fail since StubSession panics on call,
        // but that's fine; we're testing the setup path, not the fetch.
    }

    #[tokio::test]
    async fn test_photo_stream_small_recent_uses_single_fetcher() {
        // --recent 50 with page_size 100 → 1 page → 1 fetcher even with concurrency 10
        let album = make_album(100, None, default_zone());
        let (_stream, _panic_rx) = album.photo_stream(Some(50), Some(1000), 10);
    }

    // StubSession::post does `unimplemented!("stub")` which panics when the
    // fetcher hits the first page. Consuming the stream therefore causes the
    // fetcher JoinHandle to finish with a panic; the monitor task should
    // forward that through the oneshot as `true`.
    #[tokio::test]
    async fn photo_stream_surfaces_fetcher_panic_via_oneshot() {
        use tokio_stream::StreamExt;
        let album = make_album(100, None, default_zone());
        let (stream, panic_rx) = album.photo_stream(None, None, 1);
        tokio::pin!(stream);
        // Drain whatever the stream yields before the fetcher dies.
        while stream.next().await.is_some() {}
        assert!(
            panic_rx.await.unwrap_or(false),
            "panic_rx must signal `true` when a fetcher panicked"
        );
    }

    // The convenience `photos()` wrapper must not hand back a
    // silently-truncated Vec when a fetcher panics.
    #[tokio::test]
    async fn photos_returns_err_when_fetcher_panics() {
        let album = make_album(100, None, default_zone());
        let result = album.photos(None).await;
        assert!(
            result.is_err(),
            "photos() must surface fetcher panic as Err, got Ok({:?})",
            result.ok().map(|v| v.len())
        );
    }

    // --- photo_stream_with_token tests ---

    fn make_album_with_session(page_size: usize, session: Box<dyn PhotosSession>) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size,
                zone_id: Arc::new(default_zone()),
                retry_config: RetryConfig::default(),
            },
            session,
        )
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_returns_sync_token() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .query_photo_page("master-1", Some("st-zone-abc"))
            // Second call returns empty records to stop the fetcher
            .empty_query_page(Some("st-zone-abc"))
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
        tokio::pin!(stream);

        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        assert_eq!(count, 1, "should yield exactly one photo asset");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token.as_deref(), Some("st-zone-abc"));
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_no_sync_token_in_response() {
        use tokio_stream::StreamExt;

        // Responses without syncToken field
        let mock = MockPhotosFlow::new()
            .query_photo_page("master-1", None)
            .empty_query_page(None)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
        tokio::pin!(stream);

        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
        }

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, None, "no syncToken in responses means None");
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_last_token_wins() {
        use tokio_stream::StreamExt;

        // Two pages with different syncTokens — last one should be captured.
        // page_size=1 so each page yields 1 master record and the fetcher
        // advances offset by 1.
        let mock = MockPhotosFlow::new()
            .query_photo_page("master-1", Some("st-first"))
            .query_photo_page("master-2", Some("st-second"))
            .empty_query_page(None)
            .build();
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
        tokio::pin!(stream);

        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        assert_eq!(count, 2);

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token.as_deref(), Some("st-second"));
    }

    async fn drain_photo_stream_count(stream: PhotoStream) -> u32 {
        use tokio_stream::StreamExt;

        tokio::pin!(stream);
        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        count
    }

    #[tokio::test]
    async fn offline_replay_full_pass_fixture() {
        let mock = MockPhotosFlow::new()
            .query_photo_page("master-replay-full", Some("token-full"))
            .empty_query_page(Some("token-full"))
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);

        assert_eq!(drain_photo_stream_count(stream).await, 1);
        assert_eq!(
            token_rx.await.expect("sync token sender").as_deref(),
            Some("token-full")
        );
    }

    #[tokio::test]
    async fn offline_replay_paginated_full_pass_fixture() {
        let mock = MockPhotosFlow::new()
            .query_photo_page("master-replay-page-1", Some("token-page-1"))
            .query_photo_page("master-replay-page-2", Some("token-page-2"))
            .empty_query_page(Some("token-page-2"))
            .build();
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);

        assert_eq!(drain_photo_stream_count(stream).await, 2);
        assert_eq!(
            token_rx.await.expect("sync token sender").as_deref(),
            Some("token-page-2")
        );
    }

    #[tokio::test]
    async fn offline_replay_empty_page_probe_fixture() {
        let mock = MockPhotosFlow::new()
            .query_photo_page("master-before-gap", None)
            .empty_query_page(None)
            .query_photo_page("master-after-gap", None)
            .build();
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(None, None, 1, None);

        assert_eq!(
            drain_photo_stream_count(stream).await,
            2,
            "single empty records/query page must be treated as a gap, not EOF"
        );
    }

    #[tokio::test]
    async fn offline_replay_incremental_changes_fixture() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_photo_page("master-replay-change", "token-incremental", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);
        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("change event"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "master-replay-change");
        assert!(events[0].asset.is_some());
        assert_eq!(
            token_rx.await.expect("sync token sender"),
            "token-incremental"
        );
    }

    #[tokio::test]
    async fn offline_replay_retryable_error_page_fixture() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_zone_error("RETRY_LATER", "temporary backend issue", "")
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);
        let mut errors = Vec::new();
        while let Some(result) = stream.next().await {
            if let Err(error) = result {
                errors.push(error);
            }
        }

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].to_string().contains("RETRY_LATER"),
            "retryable fixture should surface the CloudKit retry code: {}",
            errors[0]
        );
        assert_eq!(
            token_rx.await.expect("sync token sender"),
            "token-before",
            "retryable error must preserve the last-good token"
        );
    }

    #[derive(Clone, Debug)]
    struct TokenByOffsetSession {
        tokens_by_offset: Arc<HashMap<u64, &'static str>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for TokenByOffsetSession {
        async fn post(
            &self,
            _url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            let offset = request["query"]["filterBy"]
                .as_array()
                .and_then(|filters| {
                    filters.iter().find_map(|filter| {
                        (filter["fieldName"] == "startRank")
                            .then(|| filter["fieldValue"]["value"].as_u64())
                            .flatten()
                    })
                })
                .unwrap_or(0);

            Ok(self.tokens_by_offset.get(&offset).map_or_else(
                || json!({"records": []}),
                |token| mock_photo_query_page(&format!("master-{offset}"), Some(token)),
            ))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn token_by_offset_session(tokens: &[(u64, &'static str)]) -> TokenByOffsetSession {
        TokenByOffsetSession {
            tokens_by_offset: Arc::new(tokens.iter().copied().collect()),
        }
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_parallel_fetchers_agree() {
        let album = make_album_with_session(
            1,
            Box::new(token_by_offset_session(&[(0, "st-same"), (1, "st-same")])),
        );

        let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 2);
        assert_eq!(drain_photo_stream_count(stream).await, 2);

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token.as_deref(), Some("st-same"));
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_parallel_fetchers_disagree_suppresses_token() {
        let album = make_album_with_session(
            1,
            Box::new(token_by_offset_session(&[
                (0, "st-first"),
                (1, "st-second"),
            ])),
        );

        let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 2);
        assert_eq!(drain_photo_stream_count(stream).await, 2);

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, None,
            "mismatched parallel fetcher tokens must block advancement"
        );
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_empty_album() {
        use tokio_stream::StreamExt;

        // Album with no records at all
        let mock = MockPhotosSession::new().ok(json!({"records": []}));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
        tokio::pin!(stream);

        let items: Vec<_> = stream.collect().await;
        assert!(items.is_empty());

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, None);
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_setup_does_not_panic() {
        // Verify photo_stream_with_token setup path works with StubSession
        // (which panics on call). Same as the photo_stream setup tests.
        let album = make_album(100, None, default_zone());
        let (_stream, _token_rx) = album.photo_stream_with_token(None, None, 10);
    }

    // --- limit / --recent edge case tests ---

    #[tokio::test]
    async fn test_photo_stream_limit_zero_yields_nothing() {
        use tokio_stream::StreamExt;

        // --recent 0 should produce 0 items. The mock has a valid page
        // available, but limit=0 means the fetcher should never send it.
        let mock = MockPhotosSession::new()
            .ok(mock_photo_query_page("master-1", None))
            .ok(json!({"records": []}));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(Some(0), Some(10), 1, None);
        tokio::pin!(stream);

        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 0, "--recent 0 should yield 0 items");
    }

    #[tokio::test]
    async fn test_photo_stream_limit_one_yields_exactly_one() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new()
            .ok(mock_photo_query_page("master-1", None))
            .ok(mock_photo_query_page("master-2", None))
            .ok(json!({"records": []}));
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(Some(1), Some(10), 1, None);
        tokio::pin!(stream);

        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1, "--recent 1 should yield exactly 1 item");
        items[0].as_ref().expect("item should be Ok");
    }

    // --- pagination edge case tests ---

    /// When a page returns only CPLAsset records (no CPLMaster), the
    /// fetcher must advance the offset and continue to subsequent pages
    /// instead of terminating prematurely.
    #[tokio::test]
    async fn test_photo_stream_continues_past_master_less_page() {
        use tokio_stream::StreamExt;

        // Page 1: Only CPLAsset records — no matching CPLMaster on this page.
        let page1 = json!({
            "records": [
                {
                    "recordName": "asset-orphan-1",
                    "recordType": "CPLAsset",
                    "fields": {
                        "masterRef": {
                            "value": {"recordName": "orphan-master", "zoneID": {"zoneName": "PrimarySync"}},
                            "type": "REFERENCE"
                        },
                        "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                        "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                    },
                    "recordChangeTag": "ct1"
                }
            ]
        });

        // Page 2: Valid paired CPLMaster + CPLAsset.
        let page2 = mock_photo_query_page("master-ok", None);

        // Page 3: Empty → terminates.
        let mock = MockPhotosSession::new()
            .ok(page1)
            .ok(page2)
            .ok(json!({"records": []}));
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(None, None, 1, None);
        tokio::pin!(stream);

        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        assert_eq!(
            count, 1,
            "page 2's paired asset should be yielded despite page 1 having no masters"
        );
    }

    /// A single empty /records/query page is not sufficient to conclude
    /// EOF. The fetcher must probe forward by one `page_size` before
    /// terminating so a transient gap doesn't silently cut enumeration
    /// short.
    #[tokio::test]
    async fn test_photo_stream_probes_past_single_empty_page() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new()
            .ok(mock_photo_query_page("master-1", None))
            // Page 2 is empty (simulated gap); must not terminate.
            .ok(json!({"records": []}))
            // Page 3 contains records past the gap.
            .ok(mock_photo_query_page("master-2", None));
        // MockPhotosSession then returns the default {"records": []} on
        // every subsequent call; the fetcher requires MAX_EMPTY_PAGE_PROBES
        // consecutive empties to commit to EOF.
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(None, None, 1, None);
        tokio::pin!(stream);

        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        assert_eq!(
            count, 2,
            "both master-1 and master-2 should be yielded; the single empty page in between must not terminate enumeration"
        );
    }

    /// Robustness regression for empty-page-run truncation: a contiguous
    /// run of fully-deleted records aligned to the page boundary used to
    /// truncate enumeration after 2 empty probes, leaving real assets
    /// past the run silently absent. With `MAX_EMPTY_PAGE_PROBES = 5`,
    /// four consecutive empty pages must not terminate; records on page 6
    /// must still be enumerated.
    #[tokio::test]
    async fn test_photo_stream_tolerates_four_consecutive_empty_pages() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new()
            .ok(mock_photo_query_page("master-1", None))
            // 4 consecutive empty pages (within tolerance).
            .ok(json!({"records": []}))
            .ok(json!({"records": []}))
            .ok(json!({"records": []}))
            .ok(json!({"records": []}))
            // Records reappear past the empty run.
            .ok(mock_photo_query_page("master-2", None));
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(None, None, 1, None);
        tokio::pin!(stream);

        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        assert_eq!(
            count, 2,
            "master-2 must be yielded even after 4 consecutive empty probes; \
             the previous threshold of 2 would have silently dropped it"
        );
    }

    /// Pins the upper bound on the probe walk: `MAX_EMPTY_PAGE_PROBES`
    /// consecutive empty pages must terminate before any subsequent
    /// records are observed. This guards against an unbounded probe
    /// regression in the other direction (the fetcher walking forever on
    /// a genuinely empty tail).
    #[tokio::test]
    async fn test_photo_stream_terminates_after_max_empty_probes() {
        use tokio_stream::StreamExt;

        // 1 record, then 5 empty pages (= MAX_EMPTY_PAGE_PROBES). A 6th
        // page with a record would be unreachable; the test asserts it is
        // never observed.
        let mock = MockPhotosSession::new()
            .ok(mock_photo_query_page("master-1", None))
            .ok(json!({"records": []}))
            .ok(json!({"records": []}))
            .ok(json!({"records": []}))
            .ok(json!({"records": []}))
            .ok(json!({"records": []}))
            // Should never be requested — terminator should fire first.
            .ok(mock_photo_query_page("master-unreachable", None));
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(None, None, 1, None);
        tokio::pin!(stream);

        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        assert_eq!(
            count, 1,
            "only master-1 should be yielded; enumeration must terminate \
             after MAX_EMPTY_PAGE_PROBES consecutive empty pages"
        );
    }

    // --- changes_stream tests ---

    /// Build a canned `ChangesZoneResponse` JSON with the given records,
    /// syncToken, and moreComing flag.
    fn canned_changes_page(records: &[Value], sync_token: &str, more_coming: bool) -> Value {
        json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": more_coming,
                "records": records
            }]
        })
    }

    /// Build a CPLMaster record for changes/zone tests.
    fn changes_master(record_name: &str) -> Value {
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
        })
    }

    /// Build a CPLAsset record that references the given master.
    fn changes_asset(record_name: &str, master_ref: &str) -> Value {
        json!({
            "recordName": record_name,
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                    "value": {"recordName": master_ref, "zoneID": {"zoneName": "PrimarySync"}},
                    "type": "REFERENCE"
                },
                "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
            },
            "recordChangeTag": "ct2"
        })
    }

    #[tokio::test]
    async fn test_changes_stream_single_page() {
        use tokio_stream::StreamExt;

        let records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let mock = MockPhotosFlow::new()
            .changes_zone_page(records, "token-final", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "master-1");
        assert!(events[0].asset.is_some());
        assert_eq!(events[0].record_type.as_deref(), Some("CPLMaster"));

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-final");
    }

    #[tokio::test]
    async fn test_changes_stream_multiple_pages() {
        use tokio_stream::StreamExt;

        let page1_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let page2_records = vec![
            changes_master("master-2"),
            changes_asset("asset-2", "master-2"),
        ];
        let mock = MockPhotosFlow::new()
            .changes_zone_page(page1_records, "token-page1", true)
            .changes_zone_page(page2_records, "token-page2", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 2);
        assert_eq!(&*events[0].record_name, "master-1");
        assert_eq!(&*events[1].record_name, "master-2");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-page2");
    }

    #[tokio::test]
    async fn test_changes_stream_empty_page_continues() {
        use tokio_stream::StreamExt;

        // First page: empty records but moreComing: true (normal API behavior)
        // Second page: actual records, moreComing: false
        let page2_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let mock = MockPhotosFlow::new()
            .changes_zone_page(Vec::new(), "token-empty", true)
            .changes_zone_page(page2_records, "token-final", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1, "should yield the event from page 2");
        assert_eq!(&*events[0].record_name, "master-1");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-final");
    }

    #[tokio::test]
    async fn test_changes_stream_zone_error() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_zone_error("BAD_REQUEST", "Unknown sync continuation type", "")
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("bad-token");
        tokio::pin!(stream);

        let mut items: Vec<anyhow::Result<ChangeEvent>> = Vec::new();
        while let Some(result) = stream.next().await {
            items.push(result);
        }

        assert_eq!(items.len(), 1, "should have exactly one error item");
        let err = items.into_iter().next().expect("should have item");
        assert!(err.is_err());
        let err_msg = format!("{}", err.unwrap_err());
        assert!(
            err_msg.contains("Invalid sync token"),
            "error should mention invalid sync token, got: {err_msg}"
        );

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "bad-token",
            "on error, should preserve the last-good token for checkpoint"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_transient_zone_error_preserves_initial_token() {
        // A transient zone code (RETRY_LATER, SERVER_INTERNAL_ERROR, etc.)
        // on the very first page must not lose the caller's initial sync_token.
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_zone_error("RETRY_LATER", "temporary backend issue", "")
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-T0");
        tokio::pin!(stream);

        let mut errors = 0usize;
        while let Some(result) = stream.next().await {
            if result.is_err() {
                errors += 1;
            }
        }
        assert_eq!(errors, 1, "should surface the zone error");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "token-T0",
            "transient zone error on first page must preserve the caller's initial token"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_mid_stream_error_preserves_last_good_token() {
        use tokio_stream::StreamExt;

        let page1_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let page2_records = vec![
            changes_master("master-2"),
            changes_asset("asset-2", "master-2"),
        ];
        // Pages 1-2 succeed, page 3 returns a zone error
        let mock = MockPhotosSession::new()
            .ok(canned_changes_page(&page1_records, "token-page1", true))
            .ok(canned_changes_page(&page2_records, "token-page2", true))
            .ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "",
                    "moreComing": false,
                    "serverErrorCode": "BAD_REQUEST",
                    "reason": "Unknown sync continuation type"
                }]
            }));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => events.push(event),
                Err(e) => errors.push(e),
            }
        }

        assert_eq!(events.len(), 2, "should have events from pages 1 and 2");
        assert_eq!(&*events[0].record_name, "master-1");
        assert_eq!(&*events[1].record_name, "master-2");
        assert_eq!(errors.len(), 1, "should have exactly one error from page 3");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "token-page2",
            "should preserve last-good token from page 2, not initial or error page"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_hard_deleted_record() {
        use super::super::types::ChangeReason;
        use tokio_stream::StreamExt;

        let records = vec![json!({
            "recordName": "deleted-record-1",
            "recordType": null,
            "deleted": true,
            "recordChangeTag": "ct-del"
        })];
        let mock =
            MockPhotosSession::new().ok(canned_changes_page(&records, "token-after-delete", false));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "deleted-record-1");
        assert_eq!(events[0].reason, ChangeReason::HardDeleted);
        assert!(events[0].asset.is_none(), "hard-deleted has no asset");
        assert!(
            events[0].record_type.is_none(),
            "hard-deleted has no record type"
        );

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-after-delete");
    }

    #[tokio::test]
    async fn test_changes_stream_invalid_token_yields_typed_error() {
        use crate::icloud::photos::session::SyncTokenError;
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new().ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "",
                "moreComing": false,
                "serverErrorCode": "BAD_REQUEST",
                "reason": "Unknown sync continuation type"
            }]
        }));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, _token_rx) = album.changes_stream("old-token");
        tokio::pin!(stream);

        let mut items: Vec<anyhow::Result<ChangeEvent>> = Vec::new();
        while let Some(result) = stream.next().await {
            items.push(result);
        }

        assert_eq!(items.len(), 1, "should have exactly one error item");
        let err = items
            .into_iter()
            .next()
            .expect("should have item")
            .expect_err("should be an error");

        let sync_err = err
            .downcast_ref::<SyncTokenError>()
            .expect("error should downcast to SyncTokenError");

        match sync_err {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(&**reason, "Unknown sync continuation type");
            }
            other => panic!("expected InvalidToken variant, got: {other:?}"),
        }
    }

    #[test]
    fn fetcher_response_body_only_logs_at_trace() {
        use std::io::Write;
        use std::sync::Arc;

        struct VecMakeWriter(Arc<std::sync::Mutex<Vec<u8>>>);
        struct VecWriter(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for VecWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecMakeWriter {
            type Writer = VecWriter;
            fn make_writer(&'a self) -> Self::Writer {
                VecWriter(Arc::clone(&self.0))
            }
        }

        // A unique marker buried in the response. If anything in the
        // event includes the response value via Display/Debug, the
        // marker leaks into the formatted output and the assertion
        // fires.
        const MARKER: &str = "FETCHER_RESPONSE_BODY_TEST_MARKER_xyz123";
        let response = serde_json::json!({
            "records": [{"recordName": "abc", "fields": {"value": MARKER}}],
            "continuationMarker": MARKER,
        });

        let buf_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub_debug = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("debug"))
            .with_writer(VecMakeWriter(Arc::clone(&buf_debug)))
            .with_ansi(false)
            .finish();
        {
            let _g = tracing::subscriber::set_default(sub_debug);
            log_fetcher_response("TestAlbum", &response);
        }
        let out_debug = String::from_utf8_lossy(
            &buf_debug
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned();
        assert!(
            out_debug.contains("Fetcher response"),
            "DEBUG-level log should produce a Fetcher response event; got: {out_debug}",
        );
        assert!(
            !out_debug.contains(MARKER),
            "DEBUG-level log MUST NOT include the response body. The marker \
             leaked into captured output, indicating the per-page log carries \
             the full response value at DEBUG (issue #347 regression). \
             Captured: {out_debug}",
        );

        let buf_trace = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub_trace = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
            .with_writer(VecMakeWriter(Arc::clone(&buf_trace)))
            .with_ansi(false)
            .finish();
        {
            let _g = tracing::subscriber::set_default(sub_trace);
            log_fetcher_response("TestAlbum", &response);
        }
        let out_trace = String::from_utf8_lossy(
            &buf_trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned();
        assert!(
            out_trace.contains("Fetcher response body"),
            "TRACE level should produce a 'Fetcher response body' event; got: {out_trace}",
        );
        assert!(
            out_trace.contains(MARKER),
            "TRACE level should include the response body in the event; got: {out_trace}",
        );
    }
}
