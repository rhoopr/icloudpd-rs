# `--directory` / `-d`

Local directory where photos are downloaded.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos
icloudpd-rs -u my@email.address --directory /photos
```

## Details

- **Required**: Yes (unless using `--auth-only`, `--list-albums`, or `--list-libraries`)
- **Type**: String (path)
- **Short**: `-d`

Photos are organized within this directory according to the [`--folder-structure`](folder-structure.md) template. The directory is created if it does not exist.

## See Also

- [`--folder-structure`](folder-structure.md)
- [`--dry-run`](dry-run.md)
