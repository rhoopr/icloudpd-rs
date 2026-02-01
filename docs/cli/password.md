# `--password` / `-p`

iCloud password for authentication.

## Usage

```sh
icloudpd-rs -u my@email.address -p 'my-password' -d /photos
ICLOUD_PASSWORD='my-password' icloudpd-rs -u my@email.address -d /photos
```

## Details

- **Required**: No (will prompt interactively if omitted)
- **Type**: String
- **Environment variable**: `ICLOUD_PASSWORD`

If neither the flag nor the environment variable is set, you will be prompted to enter your password interactively. The interactive prompt does not echo characters.

> [!CAUTION]
> Passing passwords via command-line arguments may expose them in shell history and process listings. Prefer the environment variable or interactive prompt.
