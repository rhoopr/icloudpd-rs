# `--username` / `-u`

Apple ID email address used to authenticate with iCloud.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos
icloudpd-rs --username my@email.address -d /photos
```

## Details

- **Required**: Yes
- **Type**: String

The username is passed directly to Apple's SRP-6a authentication flow. This must be the email address associated with your Apple ID.
