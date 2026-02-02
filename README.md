# icloudpd-rs

A ground-up Rust rewrite of [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (`icloudpd`).

- **Single binary download.** No Python, no pip, no dependency conflicts.
- **Native parallel downloads.** Large libraries don't take forever.
- **Built for long-running daemons.** Won't leak memory or crash after a week.
- **SQLite state tracking.** Knows what's downloaded, what failed, and where to resume.

## Status

[![License: MIT](https://img.shields.io/github/license/rhoopr/icloudpd-rs?color=8b959e)](LICENSE.md)
[![Rust](https://shields.io/badge/-Rust-v1.92.0-2D2B28?logo=rust&logoColor=DEA584)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/Status-Early%20Development-blue.svg)]()

> [!IMPORTANT]
> Early development. Core authentication (SRP, 2FA) and photo download are functional, but several features are still in progress. Expect breaking changes.

## Project Roadmap

See [CHANGELOG.md](CHANGELOG.md) for what's already implemented.

**Now** — Incremental sync (skip already-downloaded assets across runs) and mid-sync session recovery.

**Next** — XMP sidecar export, shared library downloads, OS keyring integration, robust daemon mode with systemd/launchd support, and additional download controls.

**Later** — iCloud lifecycle management (auto-delete, delete-after-download), notifications, headless MFA for Docker, and multi-account support.



# Documentation

See the [Wiki](https://github.com/rhoopr/icloudpd-rs/wiki) for detailed CLI flag reference and feature guides.

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

| Flag | Purpose | Default |
|------|---------|---------|
| `-u, --username` | Apple ID email | |
| `-p, --password` | iCloud password (or `ICLOUD_PASSWORD` env) | prompt |
| `-d, --directory` | Local download directory | |
| `-a, --album` | Album(s) to download (repeatable) | all |
| `--recent N` | Download only the N most recent photos | |
| `--threads-num N` | Number of concurrent downloads | `1` |
| `--folder-structure` | Folder template for organizing downloads | `%Y/%m/%d` |
| `--watch-with-interval N` | Run continuously, waiting N seconds between runs | |
| `--dry-run` | Preview without modifying files or iCloud | |

See the [Wiki](https://github.com/rhoopr/icloudpd-rs/wiki) for the full flag reference.



# License

MIT - see [LICENSE.md](LICENSE.md)
