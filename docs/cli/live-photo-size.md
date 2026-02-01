# `--live-photo-size`

Size variant for live photo MOV companion files.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --live-photo-size medium
```

## Details

- **Default**: `original`
- **Type**: Enum
- **Values**: `original`, `medium`, `thumb`

Controls the quality of the short video clip that accompanies live photos. The still image size is controlled separately by [`--size`](size.md).

## See Also

- [`--size`](size.md)
- [`--skip-live-photos`](skip-live-photos.md)
- [`--live-photo-mov-filename-policy`](live-photo-mov-filename-policy.md)
- [Live Photos](../features/live-photos.md)
