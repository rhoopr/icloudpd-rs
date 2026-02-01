# `--retry-delay`

Initial delay in seconds before the first retry attempt.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --retry-delay 10
```

## Details

- **Default**: `5`
- **Type**: Integer (seconds)

The delay doubles with each subsequent retry (exponential backoff) and includes random jitter to avoid thundering herd problems.

| Retry | Base delay (5s default) |
|-------|------------------------|
| 1st | ~5s |
| 2nd | ~10s |
| 3rd | ~20s |

## See Also

- [`--max-retries`](max-retries.md)
- [Retry & Resilience](../features/retry.md)
