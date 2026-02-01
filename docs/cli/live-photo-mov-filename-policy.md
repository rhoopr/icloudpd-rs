# `--live-photo-mov-filename-policy`

Controls how live photo MOV companion files are named.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --live-photo-mov-filename-policy original
```

## Details

- **Default**: `suffix`
- **Type**: Enum
- **Values**: `suffix`, `original`

| Policy | HEIC source | JPEG source | Result |
|--------|-------------|-------------|--------|
| `suffix` | `IMG_1234.HEIC` | `IMG_1234.JPG` | `IMG_1234_HEVC.MOV` / `IMG_1234.MOV` |
| `original` | `IMG_1234.HEIC` | `IMG_1234.JPG` | `IMG_1234.MOV` |

The `suffix` policy adds `_HEVC` for HEIC sources to distinguish the MOV from a potential standalone video with the same name. The `original` policy always replaces the extension with `.MOV`.

## See Also

- [`--live-photo-size`](live-photo-size.md)
- [`--skip-live-photos`](skip-live-photos.md)
- [Live Photos](../features/live-photos.md)
