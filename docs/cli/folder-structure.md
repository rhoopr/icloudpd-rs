# `--folder-structure`

Template for organizing downloaded photos into date-based subdirectories.

## Usage

```sh
# Year/month/day (default)
icloudpd-rs -u my@email.address -d /photos --folder-structure "%Y/%m/%d"

# Year only
icloudpd-rs -u my@email.address -d /photos --folder-structure "%Y"

# Flat (no subdirectories)
icloudpd-rs -u my@email.address -d /photos --folder-structure "none"
```

## Details

- **Default**: `%Y/%m/%d`
- **Type**: String (strftime format)

Uses the asset's creation date to determine the subdirectory. Supports standard strftime format specifiers:

| Specifier | Example | Meaning |
|-----------|---------|---------|
| `%Y` | `2024` | Four-digit year |
| `%m` | `01` | Two-digit month |
| `%d` | `15` | Two-digit day |

The Python `{:%Y}` syntax is also supported for compatibility.

Setting the value to `none` places all photos directly in the download directory.

## See Also

- [`--directory`](directory.md)
- [Folder Structure](../features/folder-structure.md)
