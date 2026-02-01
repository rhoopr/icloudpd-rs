# `--auth-only`

Authenticate with iCloud and exit without downloading anything.

## Usage

```sh
icloudpd-rs -u my@email.address --auth-only
```

## Details

- **Default**: `false`
- **Type**: Boolean flag

Useful for:

- Verifying your credentials work before a long download
- Completing 2FA setup and establishing a trusted session
- Refreshing an expiring session token without triggering downloads
