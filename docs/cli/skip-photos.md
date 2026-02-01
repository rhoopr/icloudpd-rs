# `--skip-photos`

Don't download photo files (download only videos).

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --skip-photos
```

## Details

- **Default**: `false`
- **Type**: Boolean flag

Filters out all image assets (JPEG, HEIC, PNG, etc.) from the download. Only standalone video files are downloaded.

## See Also

- [`--skip-videos`](skip-videos.md)
- [`--skip-live-photos`](skip-live-photos.md)
