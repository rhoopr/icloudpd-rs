# Content Filtering

icloudpd-rs provides several ways to control which assets are downloaded.

## By Media Type

| Flag | Effect |
|------|--------|
| [`--skip-videos`](../cli/skip-videos.md) | Skip standalone video files |
| [`--skip-photos`](../cli/skip-photos.md) | Skip image files (download only videos) |
| [`--skip-live-photos`](../cli/skip-live-photos.md) | Skip live photo MOV companions |

These can be combined. For example, `--skip-photos --skip-live-photos` downloads only standalone videos.

## By Date Range

| Flag | Effect |
|------|--------|
| [`--skip-created-before`](../cli/skip-created-before.md) | Skip assets older than a date or interval |
| [`--skip-created-after`](../cli/skip-created-after.md) | Skip assets newer than a date or interval |

Both accept ISO 8601 dates (`2024-01-01`) or relative intervals (`30d`).

Combine them to download a specific window:

```sh
icloudpd-rs -u me@email.com -d /photos \
  --skip-created-before 2024-01-01 \
  --skip-created-after 2024-12-31
```

## By Album

Use [`--album`](../cli/album.md) to download from specific albums instead of the entire library. Use [`--list-albums`](../cli/list-albums.md) to see what's available.

## By Recency

Use [`--recent N`](../cli/recent.md) to download only the N most recently added photos. This limits API pagination — enumeration stops after N assets are found.

## Filter Ordering

Filters are applied in the download pipeline after assets are enumerated from the API:

1. Media type filters (`--skip-videos`, `--skip-photos`)
2. Date range filters (`--skip-created-before/after`)
3. Recency limit (`--recent`)
4. Existing file check (skip already-downloaded files)
