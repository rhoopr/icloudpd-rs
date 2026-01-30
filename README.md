# icloudpd-rs

A Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`).

## Status

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE.md)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/Status-Early%20Development-blue.svg)]()

Early development. Core authentication (SRP, 2FA) and photo download are functional.

## Build

```sh
cargo build --release
```

Binary: `target/release/icloudpd-rs`

## Usage

```sh
icloudpd-rs --username my@email.address --directory /photos
```

## CLI Flags

| Flag | Purpose | Status |
|------|---------|--------|
| `-u, --username` | Apple ID email | Working |
| `-p, --password` | iCloud password (or `ICLOUD_PASSWORD` env) | Working |
| `-d, --directory` | Local download directory | Working |
| `--auth-only` | Only authenticate, don't download | Working |
| `-l, --list-albums` | List available albums | Working |
| `--list-libraries` | List available libraries | Working |
| `-a, --album` | Album(s) to download | Working |
| `--library` | Library to download (default: PrimarySync) | Parsed, not implemented |
| `--size` | Image size: original, medium, thumb | Working |
| `--live-photo-size` | Live photo video size | Parsed, not implemented |
| `--recent` | Download only N most recent photos | Parsed, not implemented |
| `--until-found` | Stop after N consecutive existing photos | Working |
| `--skip-videos` | Don't download videos | Working |
| `--skip-photos` | Don't download photos | Working |
| `--skip-live-photos` | Don't download live photos | Parsed, not implemented |
| `--force-size` | Only download requested size, no fallback | Parsed, not implemented |
| `--auto-delete` | Delete local files removed from iCloud | Parsed, not implemented |
| `--folder-structure` | Folder template (default: `%Y/%m/%d`) | Working |
| `--set-exif-datetime` | Write EXIF DateTimeOriginal if missing | Working |
| `--dry-run` | Preview without modifying files or iCloud | Working |
| `--domain` | iCloud domain: com or cn | Working |
| `--watch-with-interval` | Run continuously every N seconds | Working |
| `--log-level` | Log verbosity | Working |
| `--no-progress-bar` | Disable progress bar | Parsed, not implemented |
| `--cookie-directory` | Session/cookie storage (default: `~/.icloudpd-rs`) | Working |
| `--keep-unicode-in-filenames` | Preserve Unicode in filenames | Parsed, not implemented |
| `--live-photo-mov-filename-policy` | MOV naming: suffix, original | Parsed, not implemented |
| `--align-raw` | RAW treatment: as-is, alternative | Parsed, not implemented |
| `--file-match-policy` | Dedup policy | Parsed, not implemented |
| `--skip-created-before` | Skip assets before date/interval | Working |
| `--skip-created-after` | Skip assets after date/interval | Working |
| `--delete-after-download` | Delete from iCloud after download | Parsed, not implemented |
| `--keep-icloud-recent-days` | Keep N recent days in iCloud | Parsed, not implemented |
| `--only-print-filenames` | Print filenames without downloading | Parsed, not implemented |

## License

MIT - see [LICENSE.md](LICENSE.md)
