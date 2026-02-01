# `--recent`

Download only the N most recent photos.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --recent 100
```

## Details

- **Default**: None (download all)
- **Type**: Integer

Limits the download to the N most recently added photos. When used with [`--album`](album.md), the limit applies per album.

API enumeration stops after N photos are fetched, avoiding the overhead of paginating through your entire library.

## See Also

- [`--skip-created-before`](skip-created-before.md) / [`--skip-created-after`](skip-created-after.md) for date-based filtering
