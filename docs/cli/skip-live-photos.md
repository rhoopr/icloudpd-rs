# `--skip-live-photos`

Don't download live photo MOV companion files.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --skip-live-photos
```

## Details

- **Default**: `false`
- **Type**: Boolean flag

When a photo is a live photo, iCloud stores both a still image and a short MOV video. By default, both are downloaded. This flag skips the MOV companion — the still image is still downloaded.

## See Also

- [`--skip-videos`](skip-videos.md)
- [`--live-photo-size`](live-photo-size.md)
- [`--live-photo-mov-filename-policy`](live-photo-mov-filename-policy.md)
- [Live Photos](../features/live-photos.md)
