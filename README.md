# icloudpd-rs

A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`).

## Status

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/Status-Early%20Development-blue.svg)]()

> [!IMPORTANT]
> Early development. Core authentication (SRP, 2FA) and photo download are functional, but several features are still in progress. Expect breaking changes.

## Project Roadmap

<details open>
<summary><strong>Implemented</strong></summary>

- SRP-6a authentication with 2FA and session persistence
- iCloud Photos API — albums, smart folders, shared libraries, pagination
- Streaming download pipeline with concurrent downloads and parallel API fetching
- Photo, video, and live photo MOV downloads with size variants
- Content filtering by media type, date range, album, recency
- Date-based folder structures, filename sanitization, and deduplication policies
- EXIF DateTimeOriginal read/write and file modification time sync
- Retry with exponential backoff and error classification
- Resumable downloads with SHA256 checksum verification
- Two-phase cleanup pass with fresh CDN URLs for failures
- Dry-run, auth-only, list albums/libraries, watch mode
- Strongly typed API layer and structured error handling
- Low memory streaming for large libraries (100k+ photos)

</details>

<details open>
<summary><strong>Now</strong></summary>

- RAW file alignment (`--align-raw` version swapping)
- Robust session persistence (mid-sync re-auth, token expiry tracking, lock files)
- Progress bar integration
- Incremental sync with SQLite state tracking and CloudKit sync tokens
- Failed asset tracking with persistent state across runs
- Graceful shutdown with signal handling

</details>

<details>
<summary><strong>Next</strong></summary>

- XMP sidecar export (GPS, keywords, ratings, title/description)
- Shared library download integration
- SMS-based 2FA
- Keyring password storage
- Write all EXIF date tags (DateTime, DateTimeDigitized)
- Robust watch/daemon mode (session refresh, album re-enumeration, systemd/launchd)

</details>

<details>
<summary><strong>Later</strong></summary>

- Auto-delete synced removals (`--auto-delete`)
- Delete after download (`--delete-after-download`)
- Keep recent days in iCloud (`--keep-icloud-recent-days`)
- Email/SMTP notifications
- Notification scripts
- Multi-account support
- OS locale date formatting
- Fingerprint fallback filenames
- Docker and AUR builds

</details>

<details>
<summary><strong>Never</strong></summary>

- npm/PyPI packaging
- Web UI

</details>

## Build

```sh
cargo build --release
```

Binary: `target/release/icloudpd-rs`

## Usage

```sh
icloudpd-rs --username my@email.address --directory /photos
```

> [!TIP]
> Use `--dry-run` to preview what would be downloaded without writing any files. Use `--auth-only` to verify your credentials without starting a download.

## CLI Flags

| Flag | Purpose |
|------|---------|
| `-u, --username` | Apple ID email |
| `-p, --password` | iCloud password (or `ICLOUD_PASSWORD` env) |
| `-d, --directory` | Local download directory |
| `--auth-only` | Only authenticate, don't download |
| `-l, --list-albums` | List available albums |
| `--list-libraries` | List available libraries |
| `--recent N` | Download only the N most recent photos |
| `--threads-num N` | Number of concurrent downloads (default: 1) |
| `--max-retries N` | Max retries per download (default: 2, 0 = no retries) |
| `--retry-delay N` | Initial retry delay in seconds (default: 5) |
| `--dry-run` | Preview without modifying files or iCloud |

## License

MIT - see [LICENSE.md](LICENSE.md)
