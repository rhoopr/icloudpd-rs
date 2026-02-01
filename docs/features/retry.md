# Retry & Resilience

icloudpd-rs is designed to handle unreliable network conditions and transient iCloud API failures.

## Error Classification

Errors are classified as transient or permanent:

| Type | Examples | Retried? |
|------|----------|----------|
| Transient | HTTP 5xx, 429 (rate limit), timeouts, connection resets | Yes |
| Permanent | HTTP 4xx, checksum mismatch, missing asset | No |

## Exponential Backoff

Retries use exponential backoff with jitter. The initial delay is configured by [`--retry-delay`](../cli/retry-delay.md) (default: 5 seconds), and doubles with each attempt:

| Attempt | Approximate delay |
|---------|-------------------|
| 1st retry | ~5s |
| 2nd retry | ~10s |
| 3rd retry | ~20s |

Random jitter is added to prevent multiple concurrent downloads from retrying at the same time.

## Two-Phase Recovery

The download pipeline has two phases:

1. **Main pass** — Downloads all assets with per-file retries (up to [`--max-retries`](../cli/max-retries.md))
2. **Cleanup pass** — Re-fetches CDN URLs from iCloud for all failures, then retries them

The cleanup pass addresses a common failure mode: CDN download URLs expire during long syncs. By re-fetching URLs from iCloud, the cleanup pass gets fresh URLs that are more likely to succeed.

## API Call Retries

Retries apply not just to downloads but also to API calls:

- Album and photo enumeration
- Zone listing
- Library fetching

These use the same backoff strategy.

## Download Summary

After all downloads complete, a summary is printed with:

- Total assets processed
- Successful downloads
- Failed downloads
- Elapsed time

## Related Flags

- [`--max-retries`](../cli/max-retries.md)
- [`--retry-delay`](../cli/retry-delay.md)
