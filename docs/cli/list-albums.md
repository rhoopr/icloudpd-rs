# `--list-albums` / `-l`

List all available albums and exit.

## Usage

```sh
icloudpd-rs -u my@email.address -l
icloudpd-rs -u my@email.address --list-albums
```

## Details

- **Default**: `false`
- **Type**: Boolean flag
- **Short**: `-l`

Prints the names of all albums (user-created and smart folders) in your iCloud Photos library, then exits without downloading.

## See Also

- [`--album`](album.md)
- [`--list-libraries`](list-libraries.md)
