<p align="center">
  <img src="assets/logo.png" alt="kei logo" width="200">
</p>

<h1 align="center">kei: media sync engine</h1>

<p align="center">
  Sync your cloud photos and videos to local storage. Fast, resumable, runs unattended.<br><br>
  <a href="https://github.com/rhoopr/kei/blob/main/Cargo.toml"><img src="https://img.shields.io/badge/Rust_MSRV-1.91%2B-dea584?logo=rust" alt="Rust MSRV 1.91+"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/rhoopr/kei?color=8b959e" alt="License: MIT"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/v/release/rhoopr/kei?color=blue&label=version" alt="Version"></a>
  <a href="https://github.com/rhoopr/kei/actions/workflows/docker.yml"><img src="https://img.shields.io/github/actions/workflow/status/rhoopr/kei/docker.yml?branch=main&label=build&logo=github" alt="Build"></a><br>
  <a href="https://github.com/rhoopr/homebrew-kei"><img src="https://img.shields.io/badge/homebrew-tap-FBB040?logo=homebrew" alt="Homebrew"></a>
  <a href="https://github.com/rhoopr/kei/releases"><img src="https://img.shields.io/github/downloads/rhoopr/kei/total?logo=github&label=downloads" alt="Downloads"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/ghcr.io-kei-blue?logo=docker" alt="Docker"></a>
  <a href="https://ghcr.io/rhoopr/kei"><img src="https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fgithub.com%2Fipitio%2Fbackage%2Fraw%2Findex%2Frhoopr%2Fkei%2Fkei.json&query=%24.raw_downloads&logo=docker&label=pulls" alt="Pulls"></a></p>

- Parallel, resumable downloads verified by size and checksum
- Incremental CloudKit sync after the first run, with retry and watch mode
- Albums, smart folders, unfiled photos, PrimarySync, and shared libraries
- EXIF, XMP, RAW, Live Photo, and folder-template controls
- Friendly terminal progress on TTYs, structured logs for services and scripts
- Ops tools for status, verify, reconcile, import-existing, services, reports, health, and metrics
- iCloud Photos is supported today. Nextcloud, Immich, Ente, and Google Takeout are planned.

---

> [!CAUTION]
> kei is pre-release software under active development, and minor versions may contain breaking changes. We follow a deprecate-then-remove practice, but always check CHANGELOG when updating.

> [!IMPORTANT]
> v0.13 reshapes selection and folder-structure flags. `--exclude-album NAME` becomes `--album '!NAME'`. `--library` accepts multiple values. `kei sync` with no flags now runs per-album passes plus an unfiled pass. Legacy `{album}` in `--folder-structure` auto-migrates with a warning until v0.20. Full migration guide: [docs/v0.13-migration.md](docs/v0.13-migration.md).
>
> | Flag | Default |
> |---|---|
> | `--album` | `all` |
> | `--smart-folder` | `none` |
> | `--library` | `primary` |
> | `--unfiled` | `true` |
> | `--folder-structure` | `%Y/%m/%d` |
> | `--folder-structure-albums` | `{album}` |
> | `--folder-structure-smart-folders` | `{smart-folder}` |


---

## Install

```sh
brew install rhoopr/kei/kei             # Homebrew

docker pull ghcr.io/rhoopr/kei:latest   # Docker
# :latest follows tagged releases. Use :main to track main HEAD for unreleased builds.
```

Pre-built binaries for macOS, Linux, and Windows are on the [Releases page](https://github.com/rhoopr/kei/releases). For Docker Compose, building from source, FreeBSD, and other install paths, see the [Install wiki page](https://github.com/rhoopr/kei/wiki/Install).


> [!IMPORTANT]
> kei can't access your photos if Advanced Data Protection is on. Turn ADP off and enable "Access iCloud Data on the Web" in your Apple ID settings. Details: [Authentication wiki](https://github.com/rhoopr/kei/wiki/Authentication#advanced-data-protection-adp).

---

## Quick start

```sh
kei sync -u you@example.com -d ~/Photos/iCloud --save-password
```

You'll be prompted for your password, then asked to approve 2FA on a trusted device. Downloads start right after. After the first run, just `kei sync` - username, directory, and password are all remembered.

For a guided walkthrough, run `kei config setup` instead.

> [!TIP]
> Coming from `icloudpd`? The [Migration Guide](docs/migration-from-icloudpd.md) shows how to `kei sync` without re-downloading.

---

## Features

Sync scope:

- Choose albums with `--album`, smart folders with `--smart-folder`, and source libraries with `--library`.
- Use `--unfiled` for photos that don't belong to a user album.
- Filter by media type, filename glob, creation date, or recent count/window.
- Keep multi-library trees separate with `{library}` in folder templates.

Download behavior:

- Full scan on first run, CloudKit syncToken deltas after that.
- Parallel workers, resumable temp files, checksum checks, retry limits, and failed-asset retry.
- Optional bandwidth cap with `--bandwidth-limit`.
- `--dry-run` and `--only-print-filenames` for planning without writes.
- Watch mode with `--watch-with-interval`.

Media and metadata:

- Download original, adjusted, medium, thumb, or alternative resources.
- Pick Live Photo behavior: both, image-only, video-only, or skip.
- Control RAW pairing and Live Photo MOV filenames.
- Optionally write EXIF date, rating, GPS, and description fields.
- Embed XMP or write `.xmp` sidecars with album, people, keyword, hidden, archived, burst, and subtype metadata.

Operations:

- `kei status` shows the local state database summary.
- `kei verify --checksums` checks files already recorded in state.
- `kei reconcile --dry-run` finds state rows whose files disappeared.
- `kei import-existing` adopts an existing local photo tree.
- `kei install`, `kei uninstall`, and `kei service status` manage the [background service](https://github.com/rhoopr/kei/wiki/Service) on Linux, macOS, and Windows.
- Friendly output is on by default on plain TTYs. Use `--no-friendly` for structured logs, or `--friendly` to opt in where hard-off contexts don't apply.
- `--report-json` writes an atomic per-cycle sync report.
- Watch mode serves `/healthz` and `/metrics` on the configured HTTP bind and port.
- Docker runs watch mode by default and supports `PUID`/`PGID` ownership for NAS deployments.
- Passwords can come from the OS keyring, encrypted file fallback, environment, file, or command.
- Notification scripts can run on 2FA, sync start, sync success, sync failure, and session expiry.

---

## Docs

Everything lives on the [wiki](https://github.com/rhoopr/kei/wiki): full CLI reference, filtering and folder templates, watch mode, Docker Compose, credentials, troubleshooting, and more.

- [Commands](https://github.com/rhoopr/kei/wiki/Home#commands) - `sync`, `install`, `uninstall`, `service`, `login`, `list`, `password`, `config`, `reset`, `status`, `verify`, `reconcile`, `import-existing`
- [Configuration](https://github.com/rhoopr/kei/wiki/Configuration) - TOML file, env vars, precedence
- [Docker](https://github.com/rhoopr/kei/wiki/Docker) - Compose files and headless 2FA
- [Synology](https://github.com/rhoopr/kei/wiki/Synology) - Container Manager setup, PUID/PGID, Synology Photos integration
- [Credentials](https://github.com/rhoopr/kei/wiki/Credentials) - keyring, encrypted file, password files and commands
- [Changelog](CHANGELOG.md)
- [How iCloud's Incremental Sync Works](https://robhooper.xyz/blog-synctoken) - deep dive on CloudKit syncTokens

---

## Contributing

Contributions welcome. Open an issue first if you're planning something big.

```sh
just gate    # pre-push gate: fmt, clippy, tests, doc, audit, typos
just --list  # see every recipe
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [tests/README.md](tests/README.md) for the test catalog.

## License

MIT - see [LICENSE](LICENSE)

## Acknowledgments

The iCloud adapter builds on years of reverse-engineering by the [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) project (kei was originally published as `icloudpd-rs` before broadening to a multi-provider sync engine). Thanks to the original maintainers for their work, which made the iCloud side of kei possible.
