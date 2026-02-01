# `--cookie-directory`

Directory for storing session cookies and authentication tokens.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --cookie-directory /path/to/sessions
```

## Details

- **Default**: `~/.icloudpd-rs`
- **Type**: String (path)

The directory stores:

- Session cookies
- Trust tokens (from 2FA)
- Account authentication state

Files are created with `0o600` permissions. Tilde (`~`) is expanded to your home directory.

> [!TIP]
> If migrating from the Python version, that tool stores sessions at `~/.pyicloud/` in LWPCookieJar format, which is not compatible with icloudpd-rs. You will need to re-authenticate.
