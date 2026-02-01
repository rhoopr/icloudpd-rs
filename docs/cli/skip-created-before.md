# `--skip-created-before`

Skip assets created before a given date or time interval.

## Usage

```sh
# Skip everything before January 1, 2024
icloudpd-rs -u my@email.address -d /photos --skip-created-before 2024-01-01

# Skip everything older than 30 days
icloudpd-rs -u my@email.address -d /photos --skip-created-before 30d
```

## Details

- **Default**: None (no lower bound)
- **Type**: ISO 8601 date or interval string

Accepts two formats:

| Format | Example | Meaning |
|--------|---------|---------|
| ISO date | `2024-01-01` | Skip assets created before this date |
| Interval | `30d` | Skip assets older than 30 days from now |

## See Also

- [`--skip-created-after`](skip-created-after.md)
- [`--recent`](recent.md)
- [Content Filtering](../features/content-filtering.md)
