# `--skip-videos`

Don't download video files.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --skip-videos
```

## Details

- **Default**: `false`
- **Type**: Boolean flag

Filters out all video assets (MOV, MP4, etc.) from the download. Photos and live photo still images are still downloaded.

> [!NOTE]
> This does not affect live photo MOV companion files. Use [`--skip-live-photos`](skip-live-photos.md) to skip those.

## See Also

- [`--skip-photos`](skip-photos.md)
- [`--skip-live-photos`](skip-live-photos.md)
