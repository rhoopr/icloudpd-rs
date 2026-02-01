# `--skip-created-after`

Skip assets created after a given date or time interval.

## Usage

```sh
# Skip everything after December 31, 2023
icloudpd-rs -u my@email.address -d /photos --skip-created-after 2023-12-31

# Skip everything from the last 7 days
icloudpd-rs -u my@email.address -d /photos --skip-created-after 7d
```

## Details

- **Default**: None (no upper bound)
- **Type**: ISO 8601 date or interval string

Accepts the same formats as [`--skip-created-before`](skip-created-before.md).

## See Also

- [`--skip-created-before`](skip-created-before.md)
- [Content Filtering](../features/content-filtering.md)
