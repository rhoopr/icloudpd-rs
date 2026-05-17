# Migrating from `icloudpd`

> Last updated on 5-17-26 for v0.20.

This guide is for moving an existing
[`icloud-photos-downloader`](https://github.com/icloud-photos-downloader/icloud_photos_downloader)
(`icloudpd`) library to kei without downloading everything again. It matches
kei v0.20.

The short version:

1. Run `kei import-existing` against the photo directory you already have.
2. Use the same path-shaping options Python used.
3. Run `kei sync` from then on.

kei can't reuse Python's `~/.pyicloud` cookies, so the first run still needs a
fresh Apple login and 2FA approval. Your local photo files can stay in place.

## Step 1: import the existing tree

Point kei at the directory that `icloudpd` has been writing to:

```sh
ICLOUD_USERNAME=you@example.com kei import-existing --download-dir ~/Photos/iCloud
```

`import-existing` signs in, enumerates the selected iCloud libraries, computes
the paths kei would use for each asset, and adopts any local file with the
right path and size into the SQLite state database. The next `kei sync` skips
those adopted files and downloads only new or previously failed assets.

Run a dry import first when you're not sure the flags are right:

```sh
ICLOUD_USERNAME=you@example.com kei import-existing --download-dir ~/Photos/iCloud --dry-run
```

If most files show as unmatched, stop and fix the path options before running a
real import. A mismatch doesn't error, and it doesn't rewrite your files. It
just leaves those assets unknown to kei, which means a later sync may download
another copy.

The import is safe to repeat. Re-importing the same files updates the existing
state rows.

## Match Python's path layout

Most failed migrations come from one of these differences.

### Folder structure

Python's default `--folder-structure "{:%Y/%m/%d}"` can be passed to kei as
either the Python wrapper form or plain strftime:

```sh
kei import-existing --download-dir ~/Photos/iCloud --folder-structure "%Y/%m/%d"
```

kei 0.13 split folder templates by pass type:

| kei flag | Used for | Default |
|---|---|---|
| `--folder-structure` | Unfiled photos | `%Y/%m/%d` |
| `--folder-structure-albums` | User album passes | `{album}` |
| `--folder-structure-smart-folders` | Smart folder passes | `{smart-folder}` |

If your Python tree included album names, put the album template on
`--folder-structure-albums`:

```sh
ICLOUD_USERNAME=you@example.com kei import-existing --download-dir ~/Photos/iCloud \
  --folder-structure-albums "{album}/%Y/%m/%d"
```

On v0.20 and later, `{album}` is only accepted in
`--folder-structure-albums`.

### Filename policy

If Python was run with `--file-match-policy name-id7`, import with the same
policy:

```sh
kei import-existing --download-dir ~/Photos/iCloud --file-match-policy name-id7
```

kei also matches Python's size-suffixed duplicate fallback, invalid filename
replacement, RAW naming, live-photo MOV companions, and Unicode stripping by
default. If you used `--keep-unicode-in-filenames`, pass that to import too.

### Live photos, RAW, and sizes

Use the same media-shaping flags you used with Python:

```sh
kei import-existing --download-dir ~/Photos/iCloud \
  --size original \
  --live-photo-mode both \
  --live-photo-size original \
  --live-photo-mov-filename-policy suffix \
  --align-raw as-is
```

`import-existing --recent 100` is supported. The days form, such as
`--recent 30d`, is sync-only and import will reject it.

### Libraries and albums

kei defaults to the primary iCloud Photos library. If you want shared libraries
too, import with the same library selector that sync will read from TOML:

```sh
kei import-existing --library all --download-dir ~/Photos/iCloud --config ~/.config/kei/config.toml
```

```toml
[filters]
libraries = ["all"]
```

Then run sync with that config:

```sh
kei sync --config ~/.config/kei/config.toml
```

For multi-library trees, add `{library}` to the active templates if you want
zone-separated directories:

```sh
kei import-existing --library all --download-dir ~/Photos/iCloud \
  --folder-structure "{library}/%Y/%m/%d" \
  --folder-structure-albums "{library}/{album}/%Y/%m/%d"
```

`import-existing` doesn't have `--album`, `--smart-folder`, or `--unfiled` CLI
flags. It reads those selectors from `config.toml`, using the same defaults as
sync: all user albums, no smart folders, and an unfiled pass. If you use a
config file for sync selection, pass that same `--config` during import.

## Step 2: sync

After the import looks good:

```sh
kei sync --config ~/.config/kei/config.toml
```

Put the account and sync directory in TOML:

```toml
[auth]
username = "you@example.com"

[download]
directory = "~/Photos/iCloud"
```

After the first 2FA approval, kei stores session state under `data_dir`
(default `~/.config/kei`; `KEI_DATA_DIR` can override it for automation). Run
`kei password set` or `kei sync --save-password` if you want kei to store the
password for future runs.

## Common flag mapping

| `icloudpd` | kei | Notes |
|---|---|---|
| `-u`, `--username` | `[auth].username` | `ICLOUD_USERNAME` still works for automation. |
| `-p`, `--password` | `--password`, `[auth].password_file`, `[auth].password_command`, or `kei password set` | Avoid putting passwords in process lists when you can. |
| `-d`, `--directory` | `import-existing --download-dir`; sync uses `[download].directory` | `--directory` was removed in v0.20. |
| `-a`, `--album` | `[filters].albums = ["Name"]` | Use `["!Name"]` for exclusions. |
| `--exclude-album NAME` | `[filters].albums = ["!NAME"]` | Removed in v0.20. |
| `--list-albums` | `kei list albums` | The old flag was removed in v0.20. |
| `--list-libraries` | `kei list libraries` | The old flag was removed in v0.20. |
| `--cookie-directory` | `data_dir` | The old flag was removed in v0.20. New default is `~/.config/kei`; `KEI_DATA_DIR` still works for automation. |
| `--folder-structure "{:%Y/%m/%d}"` | `[download].folder_structure = "%Y/%m/%d"` | Python wrapper syntax is still accepted. Use `folder_structure_albums` and `folder_structure_smart_folders` for the other passes. |
| `--size original` | `[photos].size = "original"` | Values: `original`, `medium`, `thumb`, `adjusted`, `alternative`. kei accepts one size per run. |
| `--threads-num` | `[download].threads` | Default is 10. With `[download].bandwidth_limit` and no explicit thread count, default drops to 1. |
| `--skip-videos`, `--skip-photos` | `[filters].skip_videos`, `[filters].skip_photos` | Boolean TOML settings replace the sync flags. |
| `--skip-live-photos` | `[photos].live_photo_mode = "skip"` | The old flag was removed in v0.20. |
| `--recent 100` | `--recent 100` | Count form works for sync and import. |
| `--recent 30d` | `--recent 30d` for sync | Import rejects the days form. |
| `--set-exif-datetime` | `[download].set_exif_datetime = true` | kei also has EXIF rating, GPS, and description settings. |
| `--keep-unicode-in-filenames` | `[photos].keep_unicode_in_filenames = true` | Match this during import. |
| `--live-photo-mov-filename-policy` | `[photos].live_photo_mov_filename_policy` | `suffix` or `original`. |
| `--file-match-policy` | `[photos].file_match_policy` | `name-size-dedup-with-suffix` or `name-id7`. |
| `--domain` | `[auth].domain` | `com` or `cn`. |
| `--watch-with-interval` | `[watch].interval`; Docker uses `kei service run` | Built-in watch mode moved to durable config and service commands. |
| `--log-level` | `--log-level` or `log_level` | `debug`, `info`, `warn`, `error`. |
| `--only-print-filenames` | `--only-print-filenames` | Per-run flag that prints paths and doesn't download. |
| `--dry-run` | `--dry-run` | Per-run flag that doesn't modify local state or iCloud. |
| `--notification-script` | `[notifications].script` | kei sends `KEI_EVENT`, `KEI_MESSAGE`, and `KEI_ICLOUD_USERNAME`. |

## Python flags without a kei equivalent

| Python flag | kei status |
|---|---|
| `--until-found` | Replaced by SQLite state. |
| `--auto-delete` | Planned. |
| `--delete-after-download` | Planned. |
| `--keep-icloud-recent-days` | Planned. |
| `--smtp-*` | Planned. |
| `--use-os-locale` | Not planned. |
| `--password-provider` | Use `--password-file`, `--password-command`, or `kei password set`. |
| `--mfa-provider` | Not applicable. Use trusted-device 2FA with `kei login get-code` and `kei login submit-code`. |

`[download].xmp_sidecar` is implemented in kei, along with
`[download].embed_xmp`. They write iCloud metadata that Python didn't handle
the same way.

## Useful kei-only options

| Option or command | What it does |
|---|---|
| `kei config setup` | Interactive config wizard. |
| `kei config show` | Print resolved config as TOML. |
| `--config <path>` | Use a TOML config file. |
| `--password-file <path>` | Read the password from a file or Docker secret. |
| `--password-command <cmd>` | Read the password from a shell command. |
| `kei password set` | Store the password in the OS keyring or encrypted file backend. |
| `--save-password` | Save the password after a successful auth. |
| `[filters].libraries` | Select primary, shared, all, named, or excluded iCloud libraries. |
| `[filters].smart_folders` | Sync Favorites, Screenshots, Hidden, Recently Deleted, and other Apple smart folders. |
| `[filters].unfiled = false` | Disable the separate pass for photos not in any user album. |
| `[filters].filename_exclude` | Skip files by glob, such as `*.AAE` or `Screenshot*`. |
| `[download].bandwidth_limit` | Cap total download throughput, for example `10M` or `1.5Mi`. |
| `[download].set_exif_rating`, `.set_exif_gps`, `.set_exif_description` | Write more iCloud metadata into downloaded media. |
| `[download].embed_xmp`, `[download].xmp_sidecar` | Embed XMP or write `.xmp` sidecars. |
| `[download.retry].max_retries` | Retry downloads. Default is 3. |
| `[download].temp_suffix` | Temp file suffix for partial downloads. Default is `.kei-tmp`. |
| `[report].json` | Write the latest sync report atomically as JSON. |
| `[server].bind`, `[server].port` | Serve `/healthz` and `/metrics` during watch mode. |
| `[watch].reconcile_every_n_cycles` | Periodic read-only reconciliation warning pass during watch mode. |
| `--friendly`, `--no-friendly` | Control the interactive progress UI. |
| `kei status` | Show database and sync status. |
| `kei verify` | Check downloaded files exist, with optional checksums. |
| `kei reconcile` | Mark missing local files as failed so the next sync re-downloads them. |

## Removed kei compatibility aliases

v0.20 removed the remaining v0.13 compatibility aliases:

| Deprecated | Use |
|---|---|
| `--exclude-album`, `KEI_EXCLUDE_ALBUM` | `[filters].albums = ["!NAME"]` |
| `{album}` inside `--folder-structure` | `[download].folder_structure_albums = "{album}/..."` |
| `[filters].album` | `[filters].albums = ["NAME"]` |
| `[filters].exclude_albums` | `[filters].albums = ["!NAME"]` |
| `[filters].library` | `[filters].libraries` |

## Docker migration

The official image is `ghcr.io/rhoopr/kei:latest`. It uses `/config` and
`/photos` volumes. The image runs `kei service run --config /config/config.toml`
by default, with `KEI_DATA_DIR=/config`. Set `[download].directory = "/photos"`
in the config file.

A minimal compose file:

```yaml
services:
  kei:
    image: ghcr.io/rhoopr/kei:latest
    container_name: kei
    restart: unless-stopped
    stop_grace_period: 30s
    environment:
      - ICLOUD_USERNAME=${ICLOUD_USERNAME}
      - TZ=${TZ:-UTC}
    volumes:
      - ./config:/config
      - /path/to/photos:/photos
```

Set `PUID` and `PGID` when the container should write files as a host user,
which is common on NAS installs.

Avoid `ICLOUD_PASSWORD` in Docker if you can; it shows up in `docker inspect`.
Use a Docker secret with `--password-file /run/secrets/icloud_password`, an
external secret manager via `--password-command`, or run `kei password set`
inside the container if your credential backend supports it.

For a one-shot import against an existing mounted tree:

```sh
docker exec kei kei import-existing --download-dir /photos
```

For headless 2FA:

```sh
docker exec --user $(id -u):$(id -g) kei kei login get-code
docker exec --user $(id -u):$(id -g) kei kei login submit-code 123456
```

## Known output differences

- EXIF writes can change file size compared with Python because kei uses a
  different metadata writer. The image/video content is the same.
- kei preserves the source extension case Apple reports. Python often
  lowercased extensions.
- If Apple doesn't send an original filename, kei falls back to a sanitized
  title when available, then the record name.
- `[download].embed_xmp` and `[download].xmp_sidecar` add metadata that
  Python-created files won't have.

## What you don't need to worry about

- Re-downloading the whole library after import. Adopted rows are trusted until
  relevant download settings change.
- Python cookies. Re-auth once; kei stores its own session state.
- Full scans on every run. After the first sync, kei uses CloudKit sync tokens
  for incremental changes.
- `--until-found`. The state DB is the marker of what has already been
  downloaded.
