# `--max-retries`

Maximum number of retry attempts per failed download.

## Usage

```sh
# Disable retries
icloudpd-rs -u my@email.address -d /photos --max-retries 0

# Increase retries for unreliable connections
icloudpd-rs -u my@email.address -d /photos --max-retries 5
```

## Details

- **Default**: `2`
- **Type**: Integer (0 = no retries)

When a download fails with a transient error (HTTP 5xx, timeout, connection reset), it is retried up to this many times with exponential backoff. Permanent errors (HTTP 4xx) are not retried.

After all per-asset retries are exhausted, failed downloads get a second chance in a cleanup pass that re-fetches CDN URLs from iCloud before retrying.

## See Also

- [`--retry-delay`](retry-delay.md)
- [Retry & Resilience](../features/retry.md)
