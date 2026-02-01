# `--dry-run`

Preview what would be downloaded without writing any files or modifying iCloud.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --dry-run
```

## Details

- **Default**: `false`
- **Type**: Boolean flag

Performs the full authentication and asset enumeration flow, then logs each file that would be downloaded (with path, size, and type) without actually writing anything to disk. No files are created, modified, or deleted.

Useful for:

- Estimating how many photos will be downloaded
- Verifying filters (`--album`, `--recent`, `--skip-*`) produce the expected set
- Checking folder structure output before committing to a layout
