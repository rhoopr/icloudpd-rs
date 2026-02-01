# `--size`

Image size variant to download.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --size medium
```

## Details

- **Default**: `original`
- **Type**: Enum
- **Values**: `original`, `medium`, `thumb`, `adjusted`, `alternative`

| Value | Description |
|-------|-------------|
| `original` | Full-resolution original |
| `medium` | Mid-resolution version |
| `thumb` | Thumbnail |
| `adjusted` | Edited version (falls back to original if unavailable) |
| `alternative` | Alternative version (e.g., RAW when JPEG is original) |

When `medium` or `thumb` is selected, a suffix is added to the filename (e.g., `IMG_1234-medium.jpg`). The `original` size never adds a suffix.

> [!NOTE]
> Currently only a single size can be specified per run. Multiple sizes per run is a planned feature.
