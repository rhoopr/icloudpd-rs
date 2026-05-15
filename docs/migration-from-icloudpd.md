# Migrating from `icloudpd`

> Last updated on 5-16-26 for v0.14.2.

This guide is for moving an existing
[`icloud-photos-downloader`](https://github.com/icloud-photos-downloader/icloud_photos_downloader)
(`icloudpd`) library to kei without downloading everything again. It matches
kei 0.14.x.

The short version:

1. Run `kei import-existing` against the photo directory you already have.
2. Use the same path-shaping options Python used.
3. Run `kei sync` from then on.

kei can't reuse Python's `~/.pyicloud` cookies, so the first run still needs a
fresh Apple login and 2FA approval. Your local photo files can stay in place.

## Step 1: import the existing tree

Point kei at the directory that `icloudpd` has been writing to:

```sh
kei import-existing --username you@example.com --download-dir ~/Photos/iCloud
```

`import-existing` signs in, enumerates the selected iCloud libraries, computes
the paths kei would use for each asset, and adopts any local file with the
right path and size into the SQLite state database. The next `kei sync` skips
those adopted files and downloads only new or previously failed assets.

Run a dry import first when you're not sure the flags are right:

```sh
kei import-existing --username you@example.com --download-dir ~/Photos/iCloud --dry-run
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
kei import-existing --username you@example.com --download-dir ~/Photos/iCloud \
  --folder-structure-albums "{album}/%Y/%m/%d"
```

Putting `{album}` in `--folder-structure` still works for now, but kei prints a
deprecation warning and auto-migrates it. Update the command or TOML before
v0.20.

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
too, import and sync with the same library selector:

```sh
kei import-existing --library all --download-dir ~/Photos/iCloud
kei sync --library all --download-dir ~/Photos/iCloud
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
kei sync --username you@example.com --download-dir ~/Photos/iCloud
```

After the first 2FA approval, kei stores session state under `--data-dir`
(default `~/.config/kei`). Run `kei password set` or `kei sync --save-password`
if you want kei to store the password for future runs.

## Common flag mapping

| `icloudpd` | kei | Notes |
|---|---|---|
| `-u`, `--username` | Same | Also `ICLOUD_USERNAME`. |
| `-p`, `--password` | Same | Works, but process lists can expose it. Prefer `kei password set`, `--password-file`, or `--password-command`. |
| `-d`, `--directory` | `-d`, `--download-dir` | `--directory` still parses as a hidden deprecated alias through v0.20. |
| `-a`, `--album` | Same | Repeatable. Default is `all`; use `--album '!Name'` for exclusions. |
| `--exclude-album NAME` | `--album '!NAME'` | Deprecated alias through v0.20. |
| `--list-albums` | `kei list albums` | The old flag is hidden and deprecated. |
| `--list-libraries` | `kei list libraries` | The old flag is hidden and deprecated. |
| `--cookie-directory` | `--data-dir` | New default is `~/.config/kei`. |
| `--folder-structure "{:%Y/%m/%d}"` | `--folder-structure "%Y/%m/%d"` | Python wrapper syntax is still accepted. Album and smart-folder templates now have separate flags. |
| `--size original` | Same | Values: `original`, `medium`, `thumb`, `adjusted`, `alternative`. kei accepts one size per run. |
| `--threads-num` | `--threads` | Default is 10. With `--bandwidth-limit` and no explicit thread count, default drops to 1. |
| `--skip-videos`, `--skip-photos` | Same | Boolean flags also accept explicit false when overriding config. |
| `--skip-live-photos` | `--live-photo-mode skip` | Deprecated alias through v0.20. |
| `--recent 100` | Same | Count form works for sync and import. |
| `--recent 30d` | Same for sync | Import rejects the days form. |
| `--set-exif-datetime` | Same | kei also has EXIF rating, GPS, and description flags. |
| `--keep-unicode-in-filenames` | Same | Must match during import. |
| `--live-photo-mov-filename-policy` | Same | `suffix` or `original`. |
| `--file-match-policy` | Same | `name-size-dedup-with-suffix` or `name-id7`. |
| `--domain` | Same | `com` or `cn`. |
| `--watch-with-interval` | Same | Built-in watch mode. |
| `--log-level` | Same | `debug`, `info`, `warn`, `error`. |
| `--only-print-filenames` | Same | Prints paths and doesn't download. |
| `--dry-run` | Same | Doesn't modify local state or iCloud. |
| `--notification-script` | Same flag | kei sends `KEI_EVENT`, `KEI_MESSAGE`, and `KEI_ICLOUD_USERNAME`. |

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

`--xmp-sidecar` is implemented in kei, along with `--embed-xmp`. They write
iCloud metadata that Python didn't handle the same way.

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
| `--library` | Select primary, shared, all, named, or excluded iCloud libraries. |
| `--smart-folder` | Sync Favorites, Screenshots, Hidden, Recently Deleted, and other Apple smart folders. |
| `--unfiled false` | Disable the separate pass for photos not in any user album. |
| `--filename-exclude` | Skip files by glob, such as `*.AAE` or `Screenshot*`. |
| `--bandwidth-limit` | Cap total download throughput, for example `10M` or `1.5Mi`. |
| `--set-exif-rating`, `--set-exif-gps`, `--set-exif-description` | Write more iCloud metadata into downloaded media. |
| `--embed-xmp`, `--xmp-sidecar` | Embed XMP or write `.xmp` sidecars. |
| `--max-retries` | Retry downloads. Default is 3. |
| `--temp-suffix` | Temp file suffix for partial downloads. Default is `.kei-tmp`. |
| `--report-json` | Write the latest sync report atomically as JSON. |
| `--http-bind`, `--http-port` | Serve `/healthz` and `/metrics` during watch mode. |
| `--friendly`, `--no-friendly` | Control the interactive progress UI. |
| `kei status` | Show database and sync status. |
| `kei verify` | Check downloaded files exist, with optional checksums. |
| `kei reconcile` | Mark missing local files as failed so the next sync re-downloads them. |
| `--reconcile-every-n-cycles` | Periodic read-only reconciliation warning pass during watch mode. |

## Deprecated kei compatibility aliases

These still work in 0.14.x and are scheduled for removal in v0.20:

| Deprecated | Use |
|---|---|
| `--directory`, `KEI_DIRECTORY` | `--download-dir`, `KEI_DOWNLOAD_DIR` |
| `--cookie-directory`, `[auth].cookie_directory` | `--data-dir`, top-level `data_dir` |
| `--exclude-album`, `KEI_EXCLUDE_ALBUM` | `--album '!NAME'` |
| `{album}` inside `--folder-structure` | `--folder-structure-albums "{album}/..."` |
| `[filters].album` | `[filters].albums = ["NAME"]` |
| `[filters].exclude_albums` | `[filters].albums = ["!NAME"]` |
| `[filters].library` | `[filters].libraries` |
| `--skip-live-photos`, `[filters].skip_live_photos` | `--live-photo-mode skip`, `[photos].live_photo_mode = "skip"` |
| `--threads-num`, `[download].threads_num` | `--threads`, `[download].threads` |
| `--retry-delay`, `[download.retry].delay` | Let kei derive retry delay from `--max-retries`. |
| `--no-incremental` | Run `kei reset sync-token` before sync. |
| `KEI_METRICS_PORT`, `[metrics].port` | `KEI_HTTP_PORT`, `[server].port` |
| `kei credential` | `kei password` |
| `kei setup` | `kei config setup` |
| `kei get-code` | `kei login get-code` |
| `kei submit-code` | `kei login submit-code` |
| `kei auth-only` or `--auth-only` | `kei login` |
| `kei retry-failed` | `kei sync --retry-failed` |
| `kei reset-state` | `kei reset state` |
| `kei reset-sync-token` or `--reset-sync-token` | `kei reset sync-token` |

## Docker migration

The official image is `ghcr.io/rhoopr/kei:latest`. It uses `/config` and
`/photos` volumes. The image runs `kei sync --config /config/config.toml
--data-dir /config --download-dir /photos` by default, with a 24-hour watch
interval from `KEI_WATCH_WITH_INTERVAL=86400`.

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
- `--embed-xmp` and `--xmp-sidecar` add metadata that Python-created files
  won't have.

## What you don't need to worry about

- Re-downloading the whole library after import. Adopted rows are trusted until
  relevant download settings change.
- Python cookies. Re-auth once; kei stores its own session state.
- Full scans on every run. After the first sync, kei uses CloudKit sync tokens
  for incremental changes.
- `--until-found`. The state DB is the marker of what has already been
  downloaded.
