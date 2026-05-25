<p align="center">
  <img src="assets/logo.png" alt="kei logo" width="400">
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

- Download your iCloud Photos library, including shared libraries
- Keep local albums, smart folders, and unfiled photos organized
- Run once, on a schedule, in Docker, or as a background service
- Preserve originals, edited versions, RAW files, Live Photos, and metadata
- Recover from interruptions without starting over
- Check, repair, and adopt an existing photo archive
- Immich, Google Takeout, Nextcloud, and Ente support are on the roadmap

---

> [!WARNING]
> kei is pre-release software. Minor versions may contain breaking changes.
> 
> Check [CHANGELOG.md](CHANGELOG.md) before updating.

> [!IMPORTANT]
> **v0.20 is a breaking config release.**
>
> The old "everything can be a CLI flag or `KEI_*` env var" model was replaced by a smaller source model:
>
> - TOML is for settings you want to keep.
> - CLI flags are for one run or one action.
> - Env vars are for secrets, config discovery, service-manager glue, and tests.
>
> If you run kei from Docker, systemd, launchd, cron, or a shell alias, move your saved sync settings into `config.toml` before upgrading. Use the [v0.20 migration guide](docs/v0.20-migration.md) for old flag/env names and [example.config.toml](example.config.toml) for the full config file.
>
> Common config moves:
>
> | Old durable setting | New v0.20 setting |
> |---|---|
> | `--download-dir`, `KEI_DOWNLOAD_DIR` | `[download].directory` |
> | `--album`, `--smart-folder`, `--library`, `--unfiled` | `[filters].albums`, `.smart_folders`, `.libraries`, `.unfiled` |
> | `--skip-videos`, `--skip-photos` | `[filters].media` |
> | `--size`, `--live-photo-size`, `--force-size` | `[photos].resolution`, `.live_resolution`, `.force_resolution` |
> | `--watch-with-interval` | `[watch].interval` |
> | `--report-json`, `--notification-script` | `[report].json`, `[notifications].script` |
> | `--http-bind`, `--http-port` | `[server].bind`, `[server].port` |


---

## Install

```sh
brew install rhoopr/kei/kei             # Homebrew

docker pull ghcr.io/rhoopr/kei:latest   # Docker tagged release
docker pull ghcr.io/rhoopr/kei:0.20.0   # Docker pinned release
```

Pre-built binaries for macOS, Linux, and Windows are on the [Releases page](https://github.com/rhoopr/kei/releases). For Docker Compose, building from source, FreeBSD, and other install paths, see the [Install wiki page](https://github.com/rhoopr/kei/wiki/Install).


> [!IMPORTANT]
> kei needs iCloud Photos web access. If `Advanced Data Protection` is on, turn it off and enable "Access iCloud Data on the Web" in your Apple ID settings. Details: [Authentication wiki](https://github.com/rhoopr/kei/wiki/Authentication#advanced-data-protection-adp).

---

## Quick start

```sh
kei config setup
kei sync
```

`kei config setup` writes a TOML config and can save your password in the OS
keyring, or in an encrypted file when a keyring isn't available. You'll still
approve 2FA on a trusted device the first time.

Minimal config:

```toml
[auth]
username = "you@example.com"

[download]
directory = "~/Photos/iCloud"
```

> [!TIP]
> Coming from `icloudpd`? The [icloudpd migration guide](docs/migration-from-icloudpd.md) shows how to `kei sync` without re-downloading.

---

## Features

Sync what matters:

- Sync your full iCloud Photos library, selected albums, smart folders, shared libraries, or only recent media.
- Keep unfiled photos and shared-library media organized without manual sorting.
- Filter by media type, filename, date, and library when you only want part of the archive.
- Details: [Configuration](https://github.com/rhoopr/kei/wiki/Configuration)

Run it your way:

- Run kei once, on a schedule, in Docker, or as a background service.
- Watch mode keeps syncing after the first run, with health checks and metrics for long-running setups.
- Friendly terminal output is used for interactive runs. Services and scripts get structured logs.
- Details: [Docker](https://github.com/rhoopr/kei/wiki/Docker), [Service](https://github.com/rhoopr/kei/wiki/Service)

Preserve the media you care about:

- Download originals, edited versions, RAW files, Live Photos, and alternate sizes.
- Keep local folders predictable with templates for albums, smart folders, dates, and libraries.
- Optional metadata writing can update EXIF, embed XMP, or write XMP sidecars.
- Details: [Configuration](https://github.com/rhoopr/kei/wiki/Configuration)

Recover and maintain your archive:

- Interrupted downloads resume instead of starting from scratch.
- `kei status`, `kei verify`, and `kei reconcile` help you inspect and repair local state.
- `kei import-existing` can adopt an existing photo archive without re-downloading everything.
- Details: [Commands](https://github.com/rhoopr/kei/wiki/Home#commands)

---

## Docs

Everything else lives on the [wiki](https://github.com/rhoopr/kei/wiki): commands, configuration, Docker, services, credentials, filtering, folder templates, and troubleshooting.

- [Commands](https://github.com/rhoopr/kei/wiki/Home#commands) - command reference and examples
- [Configuration](https://github.com/rhoopr/kei/wiki/Configuration) - TOML config model and source precedence
- [Docker](https://github.com/rhoopr/kei/wiki/Docker) - Compose with mounted `config.toml` and headless 2FA
- [Service](https://github.com/rhoopr/kei/wiki/Service) - background service setup for Linux, macOS, and Windows
- [Synology](https://github.com/rhoopr/kei/wiki/Synology) - Container Manager setup and NAS ownership notes
- [Credentials](https://github.com/rhoopr/kei/wiki/Credentials) - keyring, encrypted file, password files, and commands
- [v0.20 migration guide](docs/v0.20-migration.md)
- [icloudpd migration guide](docs/migration-from-icloudpd.md)
- [Complete example config](example.config.toml)
- [Changelog](CHANGELOG.md)

---

## Contributing

Contributions welcome. Open an issue first if you're planning something big.

```sh
just gate    # local pre-push checks
just --list  # see every recipe
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [tests/README.md](tests/README.md) for the test catalog.

## License

MIT - see [LICENSE](LICENSE)

## Acknowledgments

The iCloud adapter builds on years of reverse-engineering by the [icloud-photos-downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) project. kei was originally published as `icloudpd-rs` before broadening beyond iCloud Photos. Thanks to the original maintainers for the work that made kei's iCloud support possible.
