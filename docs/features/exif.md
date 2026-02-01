# EXIF Handling

icloudpd-rs reads and optionally writes EXIF date metadata in downloaded photos.

## Reading EXIF Dates

After downloading a JPEG file, the `DateTimeOriginal` EXIF tag is read. If present, it is used to set the file's modification time so that file managers sort photos by capture date rather than download date.

## Writing EXIF Dates

When [`--set-exif-datetime`](../cli/set-exif-datetime.md) is enabled, icloudpd-rs checks each downloaded JPEG for the `DateTimeOriginal` tag (EXIF tag 36867). If the tag is missing, the asset's iCloud creation date is written into it.

This is useful for:

- Screenshots that lack EXIF data
- Images downloaded from the web and saved to iCloud
- Edited photos where the original EXIF was stripped

### Supported Formats

EXIF writing currently applies to JPEG files only. HEIC files are not modified.

### Current Limitations

- Only the `DateTimeOriginal` tag (36867) is written. The Python version also writes `DateTime` (306) and `DateTimeDigitized` (36868) — this is planned.
- The `little_exif` crate used for writing has a known issue where writes may silently fail to persist. This is under investigation.

## File Modification Time

Regardless of the `--set-exif-datetime` flag, the file modification time is always synced to the asset's creation date from iCloud. This ensures correct chronological ordering in file managers.

## Related Flags

- [`--set-exif-datetime`](../cli/set-exif-datetime.md)
