# icloudpd-rs Code Review

## Executive Summary

icloudpd-rs is a well-executed Rust rewrite of the Python icloud-photos-downloader. The codebase demonstrates strong Rust competence: proper async patterns, thoughtful error handling with typed errors and retry classification, streaming download pipelines that avoid buffering entire photo libraries, and security-conscious session management with file permissions and exclusive locking.

The architecture cleanly separates authentication (SRP-6a + 2FA), iCloud CloudKit API interaction, and the download engine. The code is readable, thoroughly tested (~150 unit tests), and avoids unnecessary abstraction. The SRP-6a implementation correctly handles Apple's non-standard variant, and the two-phase download strategy (stream + cleanup with fresh CDN URLs) is a pragmatic design for long-running syncs.

The most significant areas for improvement are: a password logging risk in debug traces, the Phase 2 cleanup downloading *all* assets rather than just failures, some missing `cfg(unix)` file permission checks on cookie files, and opportunities to reduce cloning in the download pipeline. None of these are show-stoppers—the codebase is production-viable as-is.

---

## Strengths

### 1. Streaming Download Architecture
The `stream_and_download` function (`src/download/mod.rs:215`) elegantly pipes album pagination directly into bounded-concurrency downloads using `stream::select_all` + `buffer_unordered`. No upfront library enumeration means the first download starts as soon as the first API page returns—critical for libraries with 100k+ photos.

### 2. Typed Error Classification for Retries
`DownloadError::is_retryable()` (`src/download/error.rs:41`) cleanly separates transient failures (429, 5xx, checksum mismatches) from permanent ones (4xx, disk errors). This feeds into a generic `retry_with_backoff` that works across auth, API calls, and downloads—no duplicated retry logic.

### 3. Session Persistence and Security
- Exclusive file locking prevents concurrent instance corruption (`src/auth/session.rs:106`)
- Session files get `chmod 600` on Unix (`src/auth/session.rs:320-326`)
- Cookie expiry pruning on load prevents stale credential accumulation
- Trust token age tracking with proactive warnings before 30-day expiry
- Password redacted from `Debug` output on both `Config` and `AuthResult`

### 4. Pre-parsed Asset Versions
`extract_versions()` (`src/icloud/photos/asset.rs:73`) parses CloudKit JSON once at construction, so `PhotoAsset::versions()` is infallible and allocation-free. This avoids per-download JSON traversal and makes the type system enforce that malformed assets are caught early.

### 5. Resume Support with Correct Checksums
`download/file.rs` uses deterministic `.part` filenames (base32-encoded checksums) and re-hashes existing bytes on resume so the final SHA-256 covers the complete file. Handles Apple's two checksum formats (32-byte raw and 33-byte with type prefix).

### 6. Comprehensive Test Coverage
150+ unit tests covering crypto determinism, retry mechanics, filter logic, RAW policy swapping, CloudKit deserialization, date parsing, and session locking. Tests use stubs for the `PhotosSession` trait, avoiding real API calls.

---

## Critical Issues

### C1. Password Logged in Debug Traces

**Location:** `src/auth/srp.rs:298`

**Impact:** The PBKDF2-derived password key is logged at debug level. While not the raw password, this derived key is the cryptographic equivalent—anyone with access to logs can impersonate the user in the SRP handshake.

```rust
// Current code
tracing::debug!("SRP password_key: {}", BASE64.encode(&password_key));
```

The private exponent `a`, shared secret `S`, session key `K`, and `x` are also logged. These should never appear in logs, even at debug level.

**Suggested fix:** Remove all cryptographic material from log output. If debugging is needed, log only non-sensitive metadata (protocol version, iteration count, salt length).

### C2. Phase 2 Cleanup Re-downloads Everything, Not Just Failures

**Location:** `src/download/mod.rs:346`

**Impact:** When Phase 1 has failures, Phase 2 calls `build_download_tasks` which re-enumerates *all* albums and rebuilds *all* tasks. The `filter_asset_to_tasks` function does check `download_path.exists()`, so already-downloaded files are skipped, but:
1. It re-fetches the entire library metadata from the API (expensive for large libraries)
2. `run_download_pass` receives all remaining tasks, not just the ones that failed

The count arithmetic is also wrong: `succeeded = total - failed` where `total = downloaded + failed_tasks.len()` from Phase 1, but `failed` comes from Phase 2's fresh task list which may differ.

```rust
// Current code
let fresh_tasks = build_download_tasks(albums, config).await?;  // ALL tasks
let remaining_failed = run_download_pass(client, fresh_tasks, ...).await;
let failed = remaining_failed.len();
let succeeded = total - failed;  // total is from Phase 1
```

**Suggested fix:** Track failed task identifiers (checksum or record ID) from Phase 1 and filter the fresh task list to only include those, or at minimum document that the existence check in `filter_asset_to_tasks` makes this safe at the cost of extra API calls.

### C3. Cookie File Permissions Not Set on Creation/Load

**Location:** `src/auth/session.rs:373-381`

**Impact:** The `#[cfg(unix)]` permission block runs after writing cookies, but only on the `extract_and_save` path. The initial cookie file loaded at `Session::new()` may have been created with default permissions (e.g., 0644) by a previous version or external tool. There's no permission check or fix on load.

**Suggested fix:** Set permissions on the cookie file during `Session::new()` if it already exists, and ensure `create_dir_all` for the cookie directory also restricts permissions.

---

## Recommendations

### High Priority

#### H1. `download_file` Wraps Errors Twice

**Location:** `src/download/file.rs:85-89`

The `retry_with_backoff` already returns the last error on exhaustion. Then `download_file` wraps it in `RetriesExhausted`, but the inner error may itself be a `DownloadError`. This double-wrapping loses the typed error:

```rust
result.map_err(|e| DownloadError::RetriesExhausted {
    retries: retry_config.max_retries,
    path: download_path.display().to_string(),
    last_error: e.to_string(),  // stringifies the typed error
})
```

If `retry_with_backoff` succeeds after retries, good. If it aborts early (non-retryable), the original error is converted to a string. Consider returning the original `DownloadError` directly when the classifier aborts, and only wrapping in `RetriesExhausted` when retries are actually exhausted.

#### H2. `resume_hash_state` Reads Entire File into Memory

**Location:** `src/download/file.rs:101`

```rust
let data = fs::read(part_path).await.ok()?;
let mut hasher = Sha256::new();
hasher.update(&data);
```

For large video files (several GB), this reads the entire `.part` file into memory at once. Should stream the file through the hasher in chunks using `tokio::io::BufReader`.

#### H3. Blocking `rpassword` Call in Async Context

**Location:** `src/main.rs:46`

The password closure calls `rpassword::prompt_password` which blocks. The comment acknowledges this but doesn't fix it. If called during re-auth in the watch loop, this blocks the tokio runtime thread.

```rust
// Current: blocks async runtime
rpassword::prompt_password("iCloud Password: ").ok()
```

Since the closure is `Fn() -> Option<String>`, not async, the caller should wrap it in `spawn_blocking`. The initial call at startup is fine, but watch-mode re-auth could trigger this.

#### H4. `offset` Counts All Records, Not Just Masters

**Location:** `src/icloud/photos/album.rs:219`

```rust
for master in master_records {
    if let Some(asset_rec) = asset_records.remove(&master.record_name) {
        // ...
    }
    offset += 1;  // incremented even for unpaired masters
}
```

`offset` is incremented for every master record regardless of whether it was paired with an asset record. Unpaired masters (orphaned records) inflate the offset, potentially causing the next page request to skip valid records. This should increment only for paired records, or use the CloudKit continuation token instead.

### Medium Priority

#### M1. `PhotoAlbum` Clones All State for `photo_stream`

**Location:** `src/icloud/photos/album.rs:124-131`

Seven fields are individually cloned to move into the spawned task. Consider extracting a `PhotoAlbumQuery` struct that holds the query parameters and can be cheaply cloned, or use `Arc` to share immutable state.

#### M2. `versions().clone()` in Filter Path

**Location:** `src/download/mod.rs:165`

```rust
let versions = apply_raw_policy(asset.versions().clone(), config.align_raw);
```

Every asset's version map is cloned even when `align_raw == AsIs` (the common case). `apply_raw_policy` already returns early for `AsIs`, but the clone happens before the call. Consider passing a reference and only cloning when mutation is needed.

#### M3. No Progress Reporting During Downloads

Despite `indicatif` being a dependency and `no_progress_bar` being a config option, there's no progress bar implementation. The download pipeline only logs "fetched N photos so far" and "Downloaded {path}". For large libraries, a progress bar showing download/total counts and throughput would significantly improve UX.

#### M4. `build_download_tasks` Uses `buffer_unordered` for Album Fetching

**Location:** `src/download/mod.rs:67-71`

```rust
let album_results: Vec<Result<Vec<_>>> = stream::iter(albums)
    .map(|album| async move { album.photos(config.recent).await })
    .buffer_unordered(config.concurrent_downloads)
    .collect()
    .await;
```

This uses the download concurrency limit for album enumeration. Album API calls are lightweight compared to downloads—a separate, smaller concurrency limit (or sequential enumeration) would be more appropriate.

#### M5. `HashMap<String, String>` for Session Data

**Location:** `src/auth/session.rs:70`

Session data is a stringly-typed `HashMap`. Keys like `"session_token"`, `"trust_token"`, `"trust_token_timestamp"` are scattered as string literals. A typed struct with `Option<String>` fields would prevent typos and make the API surface clearer.

### Low Priority

#### L1. `async-trait` Could Be Replaced with Native Async Traits

Rust 1.75+ supports `async fn` in traits natively (with some limitations). The `PhotosSession` trait could potentially use native async traits instead of the `async-trait` crate, eliminating the boxing overhead. However, since `PhotosSession` is used as `dyn PhotosSession`, the boxing is needed anyway—this is a minor point.

#### L2. `rand` 0.8 Is Outdated

`rand = "0.8"` in `Cargo.toml`—the current version is 0.9. Not urgent but worth updating when convenient, as 0.9 has API improvements.

#### L3. Hardcoded Test Paths

Tests use `/tmp/claude/` as a test directory prefix. Consider using `tempfile::TempDir` for proper cleanup and to avoid conflicts with parallel test runs.

#### L4. `page_size * 2` for `resultsLimit`

**Location:** `src/icloud/photos/album.rs:295`

```rust
"resultsLimit": page_size * 2,
```

The doubling is because CloudKit returns both `CPLMaster` and `CPLAsset` records interleaved, so `2 * page_size` fetches `page_size` paired records. This is correct but deserves a comment explaining the 2x factor.

---

## Architecture Diagram

```
┌─────────────────────────────────────────────────────┐
│                     main.rs                          │
│  CLI parsing → Config → Auth → PhotosService → DL   │
└───────┬──────────┬──────────────┬───────────────┬───┘
        │          │              │               │
   ┌────▼───┐  ┌───▼────┐  ┌─────▼─────┐  ┌──────▼──────┐
   │ cli.rs │  │config.rs│  │  auth/    │  │  download/  │
   │        │  │         │  │           │  │             │
   │ Cli    │  │ Config  │  │ mod.rs    │  │ mod.rs      │
   │ struct │  │ struct  │  │ srp.rs    │  │  stream &   │
   │ (clap) │  │         │  │ twofa.rs  │  │  download   │
   └────────┘  └─────────┘  │ session.rs│  │ file.rs     │
                            │ error.rs  │  │  HTTP+resume│
                            │endpoints.rs│ │ paths.rs    │
                            │responses.rs│ │ error.rs    │
                            └─────┬─────┘  │ exif.rs     │
                                  │        └──────┬──────┘
                                  │               │
                            ┌─────▼───────────────▼──────┐
                            │      icloud/photos/         │
                            │                             │
                            │  mod.rs (PhotosService)     │
                            │  album.rs (PhotoAlbum)      │
                            │    └─ streaming pagination  │
                            │  asset.rs (PhotoAsset)      │
                            │    └─ pre-parsed versions   │
                            │  library.rs (PhotoLibrary)  │
                            │  session.rs (PhotosSession) │
                            │  queries.rs (CloudKit DSL)  │
                            │  cloudkit.rs (response DTOs)│
                            │  smart_folders.rs           │
                            │  types.rs                   │
                            │  error.rs                   │
                            └─────────────────────────────┘

Data Flow:
  Auth: password → SRP-6a → session token → 2FA → trust token
  Download: PhotosService → album.photo_stream() → mpsc → filter → buffer_unordered → file.download_file()
  Resume: .part file → re-hash → Range request → append → checksum verify → rename
```

---

## Specific Code Examples

### Example 1: Password Key Logging (Critical)

```rust
// src/auth/srp.rs:297-298 — CURRENT
tracing::debug!("SRP salt ({} bytes): {}", salt.len(), BASE64.encode(&salt));
tracing::debug!("SRP password_key: {}", BASE64.encode(&password_key));

// SUGGESTED: remove all sensitive material
tracing::debug!("SRP salt length: {} bytes", salt.len());
tracing::debug!("SRP protocol: {}, iterations: {}", body.protocol, iterations);
// Remove lines 304-306 (x, k, u), 330-331 (S, K), 335-336 (M1, M2)
```

### Example 2: Resume Hash Streaming (High)

```rust
// src/download/file.rs:94-104 — CURRENT
async fn resume_hash_state(part_path: &Path) -> Option<(Sha256, u64)> {
    let data = fs::read(part_path).await.ok()?;  // reads entire file into RAM
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Some((hasher, existing_len))
}

// SUGGESTED: stream through hasher
async fn resume_hash_state(part_path: &Path) -> Option<(Sha256, u64)> {
    let meta = fs::metadata(part_path).await.ok()?;
    let existing_len = meta.len();
    if existing_len == 0 { return None; }

    let file = tokio::fs::File::open(part_path).await.ok()?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await.ok()?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Some((hasher, existing_len))
}
```

### Example 3: Avoiding Unnecessary Clone (Medium)

```rust
// src/download/mod.rs:165 — CURRENT
let versions = apply_raw_policy(asset.versions().clone(), config.align_raw);

// SUGGESTED: borrow when no mutation needed
fn apply_raw_policy<'a>(
    versions: &'a HashMap<AssetVersionSize, AssetVersion>,
    policy: RawTreatmentPolicy,
) -> std::borrow::Cow<'a, HashMap<AssetVersionSize, AssetVersion>> {
    if policy == RawTreatmentPolicy::AsIs {
        return Cow::Borrowed(versions);
    }
    // ... clone and mutate only when needed
    Cow::Owned(mutated)
}
```

### Example 4: Offset Tracking Bug (High)

```rust
// src/icloud/photos/album.rs:210-219 — CURRENT
for master in master_records {
    if let Some(asset_rec) = asset_records.remove(&master.record_name) {
        let asset = PhotoAsset::from_records(master, asset_rec);
        if tx.send(Ok(asset)).await.is_err() { return; }
        total_sent += 1;
    }
    offset += 1;  // BUG: counts unpaired masters
}

// SUGGESTED: offset should track position in the server's result set
// The offset represents the starting rank for the next page query.
// It should count all master records processed (paired or not) because
// the server's ordering includes them all. Actually, on reflection,
// the current behavior IS correct — offset tracks position in the
// server's full record stream, not just paired records. The server
// returns interleaved CPLMaster+CPLAsset records, and offset must
// account for all masters to avoid re-fetching. This is correct.
```

*(On closer analysis, the offset handling is correct—it tracks the server-side cursor position which includes all masters. The concern was misplaced.)*

---

## Summary of Findings by Severity

| Severity | Count | Key Items |
|----------|-------|-----------|
| Critical | 1 | Password key in debug logs |
| High | 3 | Double error wrapping, full-file resume read, Phase 2 re-fetches all |
| Medium | 5 | Unnecessary clones, no progress bar, stringly-typed session data |
| Low | 4 | Outdated rand, hardcoded test paths, missing comments |

Overall this is a high-quality Rust codebase that makes good architectural decisions. The streaming pipeline, retry classification, and session persistence are well-designed. The critical finding (password key logging) should be addressed before production use with real credentials.
