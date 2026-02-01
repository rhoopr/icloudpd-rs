# TODO — icloudpd-rs Feature Parity with Python Reference

Status legend: ✅ Done | 🔧 Partial | ❌ Not started

---

## 1. Authentication

| Feature                                     | Status | Notes                                                                                                                                                                                                                                                                                                                                                                                                 |
| ------------------------------------------- | ------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| SRP-6a authentication                       | ✅     | Full implementation with Apple's custom variants                                                                                                                                                                                                                                                                                                                                                      |
| 2FA (trusted device code)                   | ✅     | Prompt + validation + trust                                                                                                                                                                                                                                                                                                                                                                           |
| 2FA via SMS                                 | ❌     | Python supports SMS code delivery to trusted phone numbers                                                                                                                                                                                                                                                                                                                                            |
| Two-Step Authentication (2SA)               | ❌     | Legacy device-based verification (select device → receive code)                                                                                                                                                                                                                                                                                                                                       |
| Session persistence (cookies + JSON)        | 🔧     | Basics work (token + trust token + cookies saved after every request, 0o600 perms, corrupt file recovery). Gaps: no trust token expiry tracking, no proactive session refresh during long syncs, cookie persistence doesn't capture expiry/domain/path attributes, no lock file for concurrent instances, session not accessible from download layer (bare `Client` clone severs session management). |
| Keyring password storage                    | ❌     | Python integrates with OS keyring (get/store/delete)                                                                                                                                                                                                                                                                                                                                                  |
| Multiple password providers                 | ❌     | Python chains: console, keyring, parameter, webui                                                                                                                                                                                                                                                                                                                                                     |
| Multiple MFA providers                      | ❌     | Python supports: console, webui                                                                                                                                                                                                                                                                                                                                                                       |
| Session re-auth on "Invalid global session" | ❌     | Python retries with fresh auth on session errors during download                                                                                                                                                                                                                                                                                                                                      |

---

## 2. iCloud API / Photos Service

| Feature                                                                  | Status | Notes                                                                     |
| ------------------------------------------------------------------------ | ------ | ------------------------------------------------------------------------- |
| Photo/video asset enumeration                                            | ✅     | Pagination, CPLMaster/CPLAsset parsing                                    |
| Album listing and fetching                                               | ✅     | Smart folders + user albums                                               |
| Shared library enumeration                                               | 🔧     | Libraries loadable but not integrated into download flow                  |
| Multiple asset versions (original, medium, thumb, adjusted, alternative) | ✅     |                                                                           |
| Live photo detection (MOV component)                                     | ✅     | Version lookup tables present; MOV download integrated into download loop |
| RAW file handling and version swapping                                   | 🔧     | CLI flag parsed but swap logic not wired into download engine             |
| Asset filename decoding (STRING + ENCRYPTED_BYTES)                       | ✅     |                                                                           |
| Fingerprint-based fallback filenames                                     | ❌     | Python falls back to asset fingerprint when filename unavailable          |

---

## 3. Download Engine

| Feature                                   | Status | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| ----------------------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| HTTP streaming download                   | ✅     | Chunked response body                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Resumable downloads (.part files)         | ✅     | Resumes partial downloads via HTTP Range requests; existing bytes are hashed on resume so the final SHA256 checksum covers the entire file                                                                                                                                                                                                                                                                                                                                                                                       |
| Retry with backoff                        | ✅     | Exponential backoff with jitter, typed error classification (transient vs permanent), configurable `--max-retries` and `--retry-delay`, retries on both downloads and API calls (album fetch, zone list)                                                                                                                                                                                                                                                                                          |
| SHA256 checksum verification              | ✅     | All downloads verified — handles both 32-byte raw and 33-byte prefixed Apple checksum formats                                                                                                                                                                                                                                                                                                                                                                                                     |
| Session re-auth on mid-sync failure       | ❌     | If session expires during a large sync, downloads fail without re-authenticating. Python catches "Invalid global session" and retries with fresh auth.                                                                                                                                                                                                                                                                                                                                            |
| Failed asset tracking / summary           | 🔧     | Two-phase download with cleanup pass retries failures using fresh CDN URLs. Summary reports succeeded/failed/total counts with elapsed time. Remaining: no persistent state tracks downloaded vs failed assets across runs.                                                                                                                                                                                                                                                                                                                                                       |
| Atomic temp → final rename                | ✅     |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| File modification time sync to asset date | ✅     |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Dry-run mode                              | ✅     |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Progress bar                              | 🔧     | Dependency imported but integration appears minimal vs Python's tqdm                                                                                                                                                                                                                                                                                                                                                                                                                              |
| Parallel concurrent downloads             | ✅     | Streaming pipeline: assets flow from API pagination directly into `buffer_unordered` downloads with `--threads-num` concurrency (default 1)                                                                                                                                                                                                                                                                                                                                                              |
| Low memory footprint for large libraries  | ✅     | `PhotoAsset` is a compact struct with only needed fields; `photo_stream()` yields assets page-by-page via async channel instead of collecting into a `Vec`                                                                                                                                                                                              |
| Concurrent async API requests             | ✅     | Downloads and album fetching use `buffer_unordered` (via `--threads-num`). `photo_stream()` prefetches the next API page via mpsc channel while current batch is processed.                                                                                                                                                                                                                                                                                                                                         |
| Incremental sync with state tracking      | ❌     | No local database or sync state. Every run re-enumerates the entire library from the API and relies solely on `file.exists()` checks. No tracking of downloaded/failed/skipped assets, no CloudKit sync token persistence. A SQLite database (via `rusqlite`) could store asset IDs, checksums, download status, and sync tokens to skip already-processed assets without re-fetching from the API.                                                                                               |
| Graceful shutdown / signal handling       | ❌     | No `tokio::signal` or any signal handling. Ctrl+C mid-download can orphan `.part` files, corrupt session/cookie files mid-write, or interrupt EXIF writes. Affects both single-run and watch mode. Need a `CancellationToken` propagated through the download loop to finish the current file before exiting.                                                                                                                                                                                     |
| Strongly typed API responses              | ✅     | CloudKit responses (zones, queries, records) use `#[derive(Deserialize)]` structs. Auth responses are fully typed. `PhotoAsset` is a compact struct with pre-parsed fields.                                                                                                                                                                             |
| Robust compile-time error handling        | ✅     | Typed error enums throughout: `DownloadError` (with `is_retryable()` classification), `PhotosError` (with `MissingField` for malformed assets), `AuthError`, `ICloudError`. `versions()` returns `Result` instead of silently defaulting. Callers can match on specific failure modes.                                                                                                                                                                                                            |

---

## 4. File Handling & Naming

| Feature                                                     | Status | Notes                                                 |
| ----------------------------------------------------------- | ------ | ----------------------------------------------------- |
| Clean invalid filesystem characters                         | ✅     |                                                       |
| Unicode character stripping                                 | ✅     | `--keep-unicode-in-filenames`                         |
| Size-based dedup suffix                                     | ✅     | `name-size-dedup-with-suffix` policy                  |
| ID7-based dedup                                             | 🔧     | CLI flag exists; verify implementation completeness   |
| Live photo MOV naming (suffix style)                        | ✅     | Integrated: HEIC→`_HEVC.MOV`, others→`.MOV`          |
| Live photo MOV naming (original style)                      | ✅     | Integrated: replaces extension with `.MOV`            |
| Version suffixes (-medium, -thumb, -adjusted, -alternative) | ✅     | In asset version building                             |
| Extension mapping (16+ formats)                             | ✅     |                                                       |

---

## 5. Metadata & EXIF

| Feature                                    | Status | Notes                                                                                                                                                                                                   |
| ------------------------------------------ | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Read DateTimeOriginal EXIF tag             | ✅     |                                                                                                                                                                                                         |
| Write DateTimeOriginal EXIF tag            | 🔧     | `--set-exif-datetime` — `little_exif` crate's `write_to_file` silently fails to persist DateTimeOriginal tag; `exiftool` confirms tag is missing after write. Needs investigation or replacement crate. |
| Write to DateTime + DateTimeDigitized tags | ❌     | Python writes to tags 306, 36867, and 36868; Rust only writes 36867                                                                                                                                     |
| XMP sidecar file export                    | ❌     | Python has full `--xmp-sidecar` with RDF/XML output                                                                                                                                                     |
| XMP: GPS data (lat, lon, altitude, speed)  | ❌     | Part of XMP sidecar                                                                                                                                                                                     |
| XMP: Keywords (from plist-encoded field)   | ❌     | Part of XMP sidecar                                                                                                                                                                                     |
| XMP: Title and description                 | ❌     | Part of XMP sidecar                                                                                                                                                                                     |
| XMP: Orientation                           | ❌     | From zlib-compressed adjustmentSimpleDataEnc                                                                                                                                                            |
| XMP: Photo ratings (favorites, rejected)   | ❌     | Part of XMP sidecar                                                                                                                                                                                     |
| XMP: Hidden/deleted marking                | ❌     | Part of XMP sidecar                                                                                                                                                                                     |
| XMP: Screenshot detection                  | ❌     | Part of XMP sidecar                                                                                                                                                                                     |

---

## 6. Content Filtering

| Feature                                 | Status | Notes                                                                                             |
| --------------------------------------- | ------ | ------------------------------------------------------------------------------------------------- |
| Skip videos (`--skip-videos`)           | ✅     |                                                                                                   |
| Skip photos (`--skip-photos`)           | ✅     |                                                                                                   |
| Skip live photos (`--skip-live-photos`) | ✅     | Integrated into download filter                                                                   |
| Recent N photos (`--recent`)            | ✅     | Limit is per-album (matches Python); consider making it global when multiple albums are specified |
| Until-found N (`--until-found`)         | ❌     | Removed — will be superseded by incremental sync with state tracking (see item 10)                |
| Skip by creation date (before/after)    | ✅     | ISO dates and interval syntax                                                                     |
| Album selection (`--album`)             | ✅     |                                                                                                   |
| Library selection (`--library`)         | ✅     |                                                                                                   |

---

## 7. Notifications

| Feature                                    | Status | Notes                                          |
| ------------------------------------------ | ------ | ---------------------------------------------- |
| Email notification on 2FA expiration       | ❌     | Python has full SMTP support with TLS          |
| SMTP configuration (host, port, TLS, auth) | ❌     | 6 CLI flags in Python                          |
| External notification script               | ❌     | `--notification-script` runs arbitrary command |

---

## 8. Headless MFA

| Feature                                       | Status | Notes                            |
| --------------------------------------------- | ------ | -------------------------------- |
| `docker exec` MFA code submission             | ❌     | Feed MFA codes into running instance via `--submit-code` (file drop or Unix socket). Replaces Python's Flask web UI with a zero-dependency approach. |

---

## 9. Operational Features

| Feature                              | Status | Notes                                                                                                                                                                                                                                                                                  |
| ------------------------------------ | ------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Watch mode (`--watch-with-interval`) | 🔧     | Native async loop works, but: no signal handling (SIGTERM/SIGINT leaves orphaned .part files), albums fetched once and never refreshed across iterations, session never re-validated in long-running mode, no graceful shutdown, no systemd/launchd integration (PID file, sd_notify). |
| Auth-only mode (`--auth-only`)       | ✅     |                                                                                                                                                                                                                                                                                        |
| List libraries (`--list-libraries`)  | ✅     |                                                                                                                                                                                                                                                                                        |
| List albums (`--list-albums`)        | ✅     |                                                                                                                                                                                                                                                                                        |
| Only print filenames                 | 🔧     | CLI flag parsed but not wired into business logic                                                                                                                                                                                                                                                                                        |
| Folder structure templates           | ✅     | Supports `%Y/%m/%d` and Python `{:%Y}` syntax                                                                                                                                                                                                                                          |
| OS locale for date formatting        | ❌     | Python has `--use-os-locale`                                                                                                                                                                                                                                                           |
| Domain selection (com/cn)            | ✅     |                                                                                                                                                                                                                                                                                        |
| Log levels (debug/info/error)        | ✅     |                                                                                                                                                                                                                                                                                        |
| No-progress-bar flag                 | 🔧     | CLI flag parsed but no progress bar implementation exists                                                                                                                                                                                                                                                                                        |
| Multi-account support                | ❌     | Python supports multiple `--username` arguments in one run                                                                                                                                                                                                                             |

---

## Priority Recommendations

### High Priority (core functionality gaps)

1. **RAW alignment** — verify `--align-raw` version swapping matches Python's `raw_policy.py`
2. **Robust session persistence** — (a) pass `Session` (not bare `Client`) to the download layer so mid-sync re-auth is possible; (b) track trust token expiry and warn before it lapses; (c) proactively refresh sessions during long syncs/watch mode; (d) parse cookie expiry attributes instead of storing raw strings; (e) add a lock file to prevent concurrent instances from corrupting session state.
3. **Progress bar integration** — wire up indicatif/tqdm-style progress for download loop
4. **Incremental sync with SQLite state tracking** — every run re-enumerates the entire library and relies on `file.exists()` to skip downloads. Add a local SQLite database (via `rusqlite`) to track: asset ID, checksum, download status (success/failed/skipped), local path, and CloudKit sync tokens. Benefits: (a) skip API pages of already-synced assets using sync tokens; (b) retry only previously-failed assets; (c) detect moved/renamed local files without re-downloading; (d) survive folder structure config changes; (e) provide accurate progress/stats across runs.
    - **Migration from Python version:** The Python version has no database — it's purely stateless, using only filesystem checks. There's no schema to be compatible with. Migration support should focus on two things:
      - **Filesystem compatibility:** Provide a `--import-existing` command that scans an existing download directory (created by the Python version) and populates the SQLite database by matching files to iCloud assets by filename + size. This requires the Rust version to produce identical paths — same folder structure templates (`{:%Y/%m/%d}` syntax), same `clean_filename()` logic, same dedup suffix format, same live photo MOV naming.
      - **Session compatibility:** The Python version stores cookies in LWPCookieJar format at `~/.pyicloud/` (default), while Rust uses a custom `url\tcookie` format at `~/.icloudpd-rs/`. Consider a `--cookie-directory` option pointing to the Python cookie dir, with a parser that reads LWPCookieJar format, so users can reuse their trusted 2FA session without re-authenticating.
5. **Graceful shutdown with signal handling** — zero signal handling currently. Use `tokio::signal::ctrl_c()` + a `tokio_util::sync::CancellationToken` propagated into the download loop so Ctrl+C/SIGTERM finishes the current file, flushes session state, and cleans up `.part` files before exiting. Affects both single-run and watch mode.

### Medium Priority (valuable features)

6. **Multiple size downloads** — `--size` accepting multiple values per run (currently single only)
7. **`--force-size`** — don't fall back to original when requested size is missing (flag parsed but unused)
8. **`--file-match-policy`** — existing-file matching strategies (flag parsed but unused)
9. **`--only-print-filenames`** — filename-only dry-run output (flag parsed but unused)
10. **Write all EXIF date tags** — DateTime (306) and DateTimeDigitized (36868) in addition to DateTimeOriginal
11. **XMP sidecar export** — `--xmp-sidecar` with GPS, keywords, ratings, title/description
12. **Shared library download integration** — connect enumerated shared libraries to download flow
13. **SMS-based 2FA** — support sending codes to trusted phone numbers
14. **Password providers with priority ordering** — chain: parameter, keyring, console
15. **Robust watch/daemon mode** — re-fetch albums each iteration, refresh session between cycles, systemd `sd_notify` / launchd PID file
16. **Relative day intervals for date range filters** — e.g., `30` for last 30 days

### Low Priority (nice-to-have)

17. **`--auto-delete`** — After all downloads complete, scan iCloud's "Recently Deleted" folder. For each item found there, delete the matching local file (and XMP sidecar) from the download directory. This is a one-way sync: if a photo is deleted in iCloud, the local copy is cleaned up. If the photo is later restored in iCloud, it gets re-downloaded on the next run. Must respect `--dry-run`. Implementation reference: `reference/python/src/icloudpd/autodelete.py`.
18. **`--delete-after-download`** — During the download loop, after each successful download, make a CloudKit API call to `/records/modify` setting `isDeleted: 1` on the CPLAsset record. The photo moves to iCloud's "Recently Deleted" (30-day grace period). Mutually exclusive with `--auto-delete` (they conflict — one deletes local copies, the other deletes iCloud copies). Must respect `--dry-run`. Implementation reference: `reference/python/src/icloudpd/base.py` lines 1087-1140.
19. **`--keep-icloud-recent-days N`** — During the download loop, check each asset's age (`now - created_date`). Photos newer than N days are kept in iCloud; older ones are deleted via the same API call as `--delete-after-download`. Setting N=0 deletes everything from iCloud. Mutually exclusive with `--delete-after-download`. Must respect `--dry-run`. Implementation reference: `reference/python/src/icloudpd/base.py` lines 1090-1117.
20. **Headless MFA via `docker exec`** — support `docker exec <container> icloudpd-rs --submit-code 123456` to feed MFA codes into a running instance (via file drop or Unix socket). Zero dependencies, works with any orchestrator.
21. **Email notifications** — SMTP alerts on 2FA token expiration
22. **Notification scripts** — external command execution on events
23. **Multi-account support** — multiple usernames in single run
24. **OS locale date formatting** — `--use-os-locale`
25. **Fingerprint fallback filenames** — when asset filename is unavailable

---

## CLI Flags Needing Verification

The following flags are parsed in `src/cli.rs` but need end-to-end verification or are not yet wired into business logic:

| Flag                               | Purpose                                                    | Status            |
| ---------------------------------- | ---------------------------------------------------------- | ----------------- |
| `--library`                        | Library to download (default: PrimarySync)                 | Parsed but unused |
| `--force-size`                     | Only download requested size, no fallback                  | Parsed but unused |
| `--no-progress-bar`                | Disable progress bar                                       | Parsed but unused |
| `--keep-unicode-in-filenames`      | Preserve Unicode in filenames                              | Parsed but unused |
| `--align-raw`                      | RAW treatment: as-is, original, alternative                | Parsed but unused |
| `--file-match-policy`              | Dedup policy                                               | Parsed but unused |
| `--only-print-filenames`           | Print filenames without downloading                        | Parsed but unused |
