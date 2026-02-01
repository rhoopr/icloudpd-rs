# `--set-exif-datetime`

Write the DateTimeOriginal EXIF tag to downloaded photos if it is missing.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --set-exif-datetime
```

## Details

- **Default**: `false`
- **Type**: Boolean flag

When enabled, after downloading a JPEG file, icloudpd-rs checks whether the `DateTimeOriginal` EXIF tag (tag 36867) is present. If missing, it writes the asset's iCloud creation date into the tag.

This is useful for photos that were uploaded without EXIF data (screenshots, downloaded images, etc.) where you want the file's metadata to reflect when it was added to your library.

> [!NOTE]
> Currently only writes the `DateTimeOriginal` tag. Writing `DateTime` (tag 306) and `DateTimeDigitized` (tag 36868) is a planned feature.

## See Also

- [EXIF Handling](../features/exif.md)
