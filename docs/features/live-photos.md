# Live Photos

Live photos consist of a still image and a short MOV video clip. icloudpd-rs downloads both components.

## How It Works

When a photo asset has a live photo component, the download pipeline creates two download tasks: one for the still image and one for the MOV file. Both are downloaded to the same directory, side by side.

## MOV Filename Policy

The [`--live-photo-mov-filename-policy`](../cli/live-photo-mov-filename-policy.md) flag controls how the MOV file is named:

### `suffix` (default)

Adds a suffix to distinguish the MOV from the still image:

| Source | Still image | MOV file |
|--------|-------------|----------|
| HEIC | `IMG_1234.HEIC` | `IMG_1234_HEVC.MOV` |
| JPEG | `IMG_1234.JPG` | `IMG_1234.MOV` |

### `original`

Replaces the extension with `.MOV`:

| Source | Still image | MOV file |
|--------|-------------|----------|
| HEIC | `IMG_1234.HEIC` | `IMG_1234.MOV` |
| JPEG | `IMG_1234.JPG` | `IMG_1234.MOV` |

## MOV Size

The MOV video quality can be controlled independently from the still image using [`--live-photo-size`](../cli/live-photo-size.md). Options are `original`, `medium`, and `thumb`.

## Skipping Live Photos

Use [`--skip-live-photos`](../cli/skip-live-photos.md) to download only the still images without the MOV companions. This does not affect standalone videos — use [`--skip-videos`](../cli/skip-videos.md) for that.

## Related Flags

- [`--live-photo-size`](../cli/live-photo-size.md)
- [`--live-photo-mov-filename-policy`](../cli/live-photo-mov-filename-policy.md)
- [`--skip-live-photos`](../cli/skip-live-photos.md)
