# `--album` / `-a`

Download photos from specific album(s) instead of the entire library.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos -a "Vacation 2024"
icloudpd-rs -u my@email.address -d /photos -a "Vacation" -a "Favorites"
```

## Details

- **Default**: All photos (no album filter)
- **Type**: String (repeatable)
- **Short**: `-a`

When specified, only photos from the named album(s) are downloaded. Use `-a` multiple times to select multiple albums. Album names are matched exactly (case-sensitive).

Use [`--list-albums`](list-albums.md) to see available album names.

## See Also

- [`--list-albums`](list-albums.md)
- [`--recent`](recent.md)
