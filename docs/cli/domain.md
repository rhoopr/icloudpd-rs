# `--domain`

iCloud region domain.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --domain cn
```

## Details

- **Default**: `com`
- **Type**: Enum
- **Values**: `com`, `cn`

| Value | Region | Auth endpoint | Home |
|-------|--------|---------------|------|
| `com` | International | `idmsa.apple.com` | `www.icloud.com` |
| `cn` | China mainland | `idmsa.apple.com.cn` | `www.icloud.com.cn` |

If Apple redirects your account to a different domain during authentication, icloudpd-rs will report an error and ask you to re-run with the correct `--domain` value.
