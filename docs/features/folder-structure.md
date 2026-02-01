# Folder Structure

icloudpd-rs organizes downloaded photos into date-based subdirectories within your download directory.

## Default Layout

With the default `--folder-structure "%Y/%m/%d"`, a photo taken on March 15, 2024 is saved to:

```
/photos/2024/03/15/IMG_1234.HEIC
```

## Custom Templates

The folder structure is a strftime format string applied to each asset's creation date:

| Template | Example path |
|----------|-------------|
| `%Y/%m/%d` | `2024/03/15/IMG_1234.HEIC` |
| `%Y/%m` | `2024/03/IMG_1234.HEIC` |
| `%Y` | `2024/IMG_1234.HEIC` |
| `none` | `IMG_1234.HEIC` |

The Python `{:%Y/%m/%d}` syntax is also accepted for compatibility with existing configurations.

## Filename Handling

Within each folder, filenames are preserved from iCloud. Special characters that are invalid on the local filesystem are sanitized. Duplicate filenames within the same folder are handled by the deduplication policy (size-based suffix by default).

## Compatibility with Python Version

The Rust version produces identical folder paths when using the same template string. If you're migrating from the Python version with the default folder structure, your existing directory layout will be preserved.

## Related Flags

- [`--folder-structure`](../cli/folder-structure.md)
- [`--directory`](../cli/directory.md)
