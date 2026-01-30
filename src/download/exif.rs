use std::path::Path;

use anyhow::{Context, Result};

/// Read the `DateTimeOriginal` EXIF tag from an image file.
///
/// Returns `Ok(Some(value))` if the tag is present, `Ok(None)` if the file
/// has no EXIF data or the tag is missing, and `Err` only on I/O failure.
pub fn get_photo_exif(path: &Path) -> Result<Option<String>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    let mut bufreader = std::io::BufReader::new(&file);
    let exif_reader = exif::Reader::new();

    match exif_reader.read_from_container(&mut bufreader) {
        Ok(exif_data) => {
            if let Some(field) =
                exif_data.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
            {
                Ok(Some(field.display_value().to_string()))
            } else {
                Ok(None)
            }
        }
        Err(e) => {
            tracing::debug!("No EXIF data in {}: {}", path.display(), e);
            Ok(None)
        }
    }
}

/// Write the `DateTimeOriginal` EXIF tag to a JPEG file.
///
/// The `datetime_str` should be in `"YYYY:MM:DD HH:MM:SS"` format.
pub fn set_photo_exif(path: &Path, datetime_str: &str) -> Result<()> {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = Metadata::new_from_path(path)
        .with_context(|| format!("Reading EXIF metadata from {}", path.display()))?;
    metadata.set_tag(ExifTag::DateTimeOriginal(datetime_str.to_string()));
    metadata
        .write_to_file(path)
        .with_context(|| format!("Writing EXIF metadata to {}", path.display()))?;

    tracing::debug!("Set EXIF DateTimeOriginal={} on {}", datetime_str, path.display());
    Ok(())
}
