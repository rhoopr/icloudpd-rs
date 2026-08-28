//! ISO-BMFF helpers for reading and safely updating XMP in HEIC / HEIF / AVIF
//! files.
//!
//! Adobe's XMP Toolkit has no HEIF handler, so kei reads HEIF item metadata
//! directly via [`mp4_atom`]. The writer below edits only the XMP item map and
//! raw box headers. All other boxes and payloads are copied byte-for-byte.

#![allow(
    clippy::map_err_ignore,
    reason = "Malformed untrusted bytes are reduced to stable typed layout errors at this boundary."
)]
#![allow(
    clippy::type_complexity,
    reason = "The parser returns a fixed group of layout coordinates used together for one rewrite."
)]

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

#[cfg(test)]
use mp4_atom::{Any, Encode, ItemInfoEntry, ItemLocation, ItemLocationExtent, Mdat, Meta};
use mp4_atom::{Atom, Buf, DecodeMaybe, FourCC, Header, Iinf, Iloc};

/// Typed failures from the HEIC writer. Each variant names the precise mode
/// so call-site logging and any future fall-back logic can distinguish
/// "file is truncated" from "kei's own re-encoder failed" from "this isn't
/// a HEIC at all" — instead of grepping anyhow strings.
///
/// `Decode`/`Encode` wrap the underlying [`mp4_atom::Error`] so the original
/// failure detail is preserved (`UnderDecode("infe")`, `OutOfBounds`, etc.)
/// while kei adds the byte offset / atom kind context.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HeifError {
    #[cfg(test)]
    #[error("Could not read HEIC metadata at byte {offset} of {total}: {source}")]
    Decode {
        offset: u64,
        total: u64,
        #[source]
        source: mp4_atom::Error,
    },

    #[cfg(test)]
    #[error(
        "Could not read trailing HEIC bytes at byte {offset} of {total}; the file may be truncated"
    )]
    UnparsableTail { offset: u64, total: u64 },

    #[error("Could not read HEIC metadata box `{kind}`: {source}")]
    MetaSubBoxDecode {
        kind: FourCC,
        #[source]
        source: mp4_atom::Error,
    },

    #[error("Could not safely rewrite HEIC XMP: {reason}")]
    InvalidLayout { reason: &'static str },

    #[error("HEIC XMP rewrite value does not fit in {field}")]
    ValueOverflow { field: &'static str },

    #[cfg(test)]
    #[error("Could not find a top-level HEIC `meta` box after scanning {input_len} bytes")]
    MissingMeta { input_len: usize },

    #[cfg(test)]
    #[error("Could not rewrite HEIC atom `{kind}`: {source}")]
    Encode {
        kind: FourCC,
        #[source]
        source: mp4_atom::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub(crate) enum HeifExifError {
    Io(std::io::Error),
    Malformed,
}

impl From<std::io::Error> for HeifExifError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy)]
struct FileAtom {
    kind: [u8; 4],
    body_start: u64,
    end: u64,
}

/// Whether this path's extension is HEIF / HEIC / HIF / AVIF — formats
/// that XMP Toolkit's bundled handlers can't open, handled here instead.
///
/// Used for pre-download decisions where the file doesn't exist yet, so
/// content sniffing isn't possible. For post-download dispatch on a file
/// that may have a temp suffix shadowing its real extension (`.kei-tmp`),
/// use [`is_heif_content`] instead.
pub(crate) fn is_heif_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            matches!(lower.as_str(), "heic" | "heif" | "hif" | "avif")
        })
        .unwrap_or(false)
}

/// Whether `bytes` starts with an ISO-BMFF `ftyp` box whose major brand is
/// in the HEIF family. Robust to part-file naming where the path extension
/// has been replaced by a temp suffix — the byte signature is the only
/// reliable way to dispatch HEIF vs the formats XMP Toolkit can sniff
/// itself (JPEG/PNG/TIFF/MP4/MOV).
///
/// Brands per ISO/IEC 23008-12 §A.6 (HEIF) and AV1 Image File Format
/// (`avif`/`avis`). Only the first 12 bytes are inspected: 4-byte size,
/// `ftyp` fourCC, then 4-byte major brand.
pub(crate) fn is_heif_content(bytes: &[u8]) -> bool {
    let Some(box_type) = bytes.get(4..8) else {
        return false;
    };
    if box_type != b"ftyp" {
        return false;
    }
    let Some(brand) = bytes.get(8..12) else {
        return false;
    };
    matches!(
        brand,
        b"heic"
            | b"heix"
            | b"heim"
            | b"heis"
            | b"hevc"
            | b"hevm"
            | b"hevs"
            | b"mif1"
            | b"msf1"
            | b"avif"
            | b"avis"
    )
}

pub(crate) fn locate_exif_tiff<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    file_len: u64,
) -> Result<Option<(u64, u64)>, HeifExifError> {
    let mut offset = 0_u64;
    let mut meta = None;
    while let Some(atom) = read_file_atom(source, offset, file_len)? {
        if atom.kind == *b"meta" {
            if meta.is_some() {
                return Err(HeifExifError::Malformed);
            }
            meta = Some(atom);
        }
        offset = atom.end;
    }
    let Some(meta) = meta else {
        return Ok(None);
    };

    let (iinf, iloc) = locate_meta_control_atoms(source, meta)?;
    let (Some(iinf), Some(iloc)) = (iinf, iloc) else {
        return Ok(None);
    };
    let Some(item_id) = read_exif_item_id(source, iinf)? else {
        return Ok(None);
    };
    let Some((base_offset, extent_offset, extent_length)) =
        read_exif_item_location(source, iloc, item_id)?
    else {
        return Ok(None);
    };

    let extent_start = base_offset
        .checked_add(extent_offset)
        .ok_or(HeifExifError::Malformed)?;
    let extent_end = extent_start
        .checked_add(extent_length)
        .filter(|end| *end <= file_len)
        .ok_or(HeifExifError::Malformed)?;
    if extent_length < 4 {
        return Err(HeifExifError::Malformed);
    }
    let mut prefix = [0_u8; 4];
    read_file_exact(source, file_len, extent_start, &mut prefix)?;
    let tiff_offset = u64::from(u32::from_be_bytes(prefix));
    let tiff_start = extent_start
        .checked_add(4)
        .and_then(|start| start.checked_add(tiff_offset))
        .filter(|start| *start <= extent_end)
        .ok_or(HeifExifError::Malformed)?;
    let tiff_len = extent_end
        .checked_sub(tiff_start)
        .ok_or(HeifExifError::Malformed)?;
    Ok(Some((tiff_start, tiff_len)))
}

fn locate_meta_control_atoms<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    meta: FileAtom,
) -> Result<(Option<FileAtom>, Option<FileAtom>), HeifExifError> {
    let mut prefix = [0_u8; 8];
    let prefix_len = usize::try_from((meta.end - meta.body_start).min(8))
        .map_err(|_error| HeifExifError::Malformed)?;
    let prefix_output = prefix
        .get_mut(..prefix_len)
        .ok_or(HeifExifError::Malformed)?;
    read_file_exact(source, meta.end, meta.body_start, prefix_output)?;
    let mut offset = if prefix.get(4..8) == Some(b"hdlr".as_slice()) {
        meta.body_start
    } else {
        meta.body_start
            .checked_add(4)
            .filter(|offset| *offset <= meta.end)
            .ok_or(HeifExifError::Malformed)?
    };
    let Some(handler) = read_file_atom(source, offset, meta.end)? else {
        return Ok((None, None));
    };
    if handler.kind != *b"hdlr" {
        return Err(HeifExifError::Malformed);
    }
    offset = handler.end;

    let mut iinf = None;
    let mut iloc = None;
    while let Some(atom) = read_file_atom(source, offset, meta.end)? {
        match &atom.kind {
            b"iinf" if iinf.replace(atom).is_some() => {
                return Err(HeifExifError::Malformed);
            }
            b"iloc" if iloc.replace(atom).is_some() => {
                return Err(HeifExifError::Malformed);
            }
            _ => {}
        }
        offset = atom.end;
    }
    Ok((iinf, iloc))
}

fn read_exif_item_id<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    iinf: FileAtom,
) -> Result<Option<u32>, HeifExifError> {
    let mut full_box = [0_u8; 8];
    read_file_exact(source, iinf.end, iinf.body_start, &mut full_box[..6])?;
    let version = full_box[0];
    let (entry_count, mut offset) = if version == 0 {
        (
            u32::from(u16::from_be_bytes([full_box[4], full_box[5]])),
            iinf.body_start
                .checked_add(6)
                .ok_or(HeifExifError::Malformed)?,
        )
    } else if version == 1 {
        read_file_exact(source, iinf.end, iinf.body_start, &mut full_box)?;
        (
            u32::from_be_bytes([full_box[4], full_box[5], full_box[6], full_box[7]]),
            iinf.body_start
                .checked_add(8)
                .ok_or(HeifExifError::Malformed)?,
        )
    } else {
        return Err(HeifExifError::Malformed);
    };

    let mut exif_id = None;
    for _ in 0..entry_count {
        let Some(entry) = read_file_atom(source, offset, iinf.end)? else {
            return Err(HeifExifError::Malformed);
        };
        if entry.kind != *b"infe" {
            return Err(HeifExifError::Malformed);
        }
        let mut fields = [0_u8; 14];
        let available = usize::try_from((entry.end - entry.body_start).min(fields.len() as u64))
            .map_err(|_error| HeifExifError::Malformed)?;
        let fields_output = fields
            .get_mut(..available)
            .ok_or(HeifExifError::Malformed)?;
        read_file_exact(source, entry.end, entry.body_start, fields_output)?;
        let item_id = match fields.first().copied() {
            Some(2) if available >= 12 => u32::from(u16::from_be_bytes([fields[4], fields[5]])),
            Some(3) if available >= 14 => {
                u32::from_be_bytes([fields[4], fields[5], fields[6], fields[7]])
            }
            _ => {
                offset = entry.end;
                continue;
            }
        };
        let item_type_offset = if fields[0] == 2 { 8 } else { 10 };
        if fields.get(item_type_offset..item_type_offset + 4) == Some(b"Exif".as_slice())
            && exif_id.replace(item_id).is_some()
        {
            return Err(HeifExifError::Malformed);
        }
        offset = entry.end;
    }
    Ok(exif_id)
}

fn read_exif_item_location<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    iloc: FileAtom,
    target_item_id: u32,
) -> Result<Option<(u64, u64, u64)>, HeifExifError> {
    let mut header = [0_u8; 10];
    read_file_exact(source, iloc.end, iloc.body_start, &mut header[..8])?;
    let version = header[0];
    if version > 2 {
        return Err(HeifExifError::Malformed);
    }
    let offset_size = header[4] >> 4;
    let length_size = header[4] & 0x0F;
    let base_offset_size = header[5] >> 4;
    let index_size = if version == 0 { 0 } else { header[5] & 0x0F };
    for size in [offset_size, length_size, base_offset_size, index_size] {
        if !matches!(size, 0 | 4 | 8) {
            return Err(HeifExifError::Malformed);
        }
    }
    let (item_count, mut cursor) = if version < 2 {
        (
            u32::from(u16::from_be_bytes([header[6], header[7]])),
            iloc.body_start
                .checked_add(8)
                .ok_or(HeifExifError::Malformed)?,
        )
    } else {
        read_file_exact(source, iloc.end, iloc.body_start, &mut header)?;
        (
            u32::from_be_bytes([header[6], header[7], header[8], header[9]]),
            iloc.body_start
                .checked_add(10)
                .ok_or(HeifExifError::Malformed)?,
        )
    };

    let mut found = None;
    for _ in 0..item_count {
        let item_id = u32::try_from(read_sized_integer(
            source,
            iloc.end,
            &mut cursor,
            if version < 2 { 2 } else { 4 },
        )?)
        .map_err(|_error| HeifExifError::Malformed)?;
        let construction_method = if version == 0 {
            0
        } else {
            u8::try_from(read_sized_integer(source, iloc.end, &mut cursor, 2)? & 0x0F)
                .map_err(|_error| HeifExifError::Malformed)?
        };
        let data_reference_index =
            u16::try_from(read_sized_integer(source, iloc.end, &mut cursor, 2)?)
                .map_err(|_error| HeifExifError::Malformed)?;
        let base_offset = read_sized_integer(source, iloc.end, &mut cursor, base_offset_size)?;
        let extent_count = u16::try_from(read_sized_integer(source, iloc.end, &mut cursor, 2)?)
            .map_err(|_error| HeifExifError::Malformed)?;
        let per_extent_size = u64::from(index_size)
            .checked_add(u64::from(offset_size))
            .and_then(|size| size.checked_add(u64::from(length_size)))
            .ok_or(HeifExifError::Malformed)?;
        if extent_count > 0 && per_extent_size == 0 {
            return Err(HeifExifError::Malformed);
        }
        cursor
            .checked_add(
                u64::from(extent_count)
                    .checked_mul(per_extent_size)
                    .ok_or(HeifExifError::Malformed)?,
            )
            .filter(|end| *end <= iloc.end)
            .ok_or(HeifExifError::Malformed)?;
        let mut selected_extent = None;
        for extent_index in 0..extent_count {
            let item_reference_index =
                read_sized_integer(source, iloc.end, &mut cursor, index_size)?;
            let extent_offset = read_sized_integer(source, iloc.end, &mut cursor, offset_size)?;
            let extent_length = read_sized_integer(source, iloc.end, &mut cursor, length_size)?;
            if item_id == target_item_id && extent_index == 0 {
                if item_reference_index != 0 {
                    return Err(HeifExifError::Malformed);
                }
                selected_extent = Some((extent_offset, extent_length));
            }
        }
        if item_id == target_item_id {
            if found.is_some()
                || construction_method != 0
                || data_reference_index != 0
                || extent_count != 1
            {
                return Err(HeifExifError::Malformed);
            }
            let Some((extent_offset, extent_length)) = selected_extent else {
                return Err(HeifExifError::Malformed);
            };
            found = Some((base_offset, extent_offset, extent_length));
        }
    }
    Ok(found)
}

fn read_sized_integer<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    parent_end: u64,
    cursor: &mut u64,
    size: u8,
) -> Result<u64, HeifExifError> {
    let mut bytes = [0_u8; 8];
    let size = usize::from(size);
    if size == 0 {
        return Ok(0);
    }
    let start = 8_usize.checked_sub(size).ok_or(HeifExifError::Malformed)?;
    let output = bytes.get_mut(start..).ok_or(HeifExifError::Malformed)?;
    read_file_exact(source, parent_end, *cursor, output)?;
    *cursor = cursor
        .checked_add(u64::try_from(size).map_err(|_error| HeifExifError::Malformed)?)
        .ok_or(HeifExifError::Malformed)?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_file_atom<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    offset: u64,
    parent_end: u64,
) -> Result<Option<FileAtom>, HeifExifError> {
    if offset == parent_end {
        return Ok(None);
    }
    let mut header = [0_u8; 16];
    read_file_exact(source, parent_end, offset, &mut header[..8])?;
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let kind = [header[4], header[5], header[6], header[7]];
    let (header_len, total_len) = match size32 {
        0 => (8_u64, parent_end - offset),
        1 => {
            read_file_exact(source, parent_end, offset, &mut header)?;
            let extended_size: [u8; 8] = header
                .get(8..16)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(HeifExifError::Malformed)?;
            (16, u64::from_be_bytes(extended_size))
        }
        size => (8, u64::from(size)),
    };
    if total_len < header_len {
        return Err(HeifExifError::Malformed);
    }
    let end = offset
        .checked_add(total_len)
        .filter(|end| *end <= parent_end)
        .ok_or(HeifExifError::Malformed)?;
    let body_start = offset
        .checked_add(header_len)
        .ok_or(HeifExifError::Malformed)?;
    Ok(Some(FileAtom {
        kind,
        body_start,
        end,
    }))
}

fn read_file_exact<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    parent_end: u64,
    offset: u64,
    output: &mut [u8],
) -> Result<(), HeifExifError> {
    let output_len = u64::try_from(output.len()).map_err(|_error| HeifExifError::Malformed)?;
    offset
        .checked_add(output_len)
        .filter(|end| *end <= parent_end)
        .ok_or(HeifExifError::Malformed)?;
    source.seek(std::io::SeekFrom::Start(offset))?;
    source.read_exact(output)?;
    Ok(())
}

/// Locate the primary image's embedded XMP packet, if any. Returns the raw
/// RDF/XML payload of the `mime` item with content_type
/// `"application/rdf+xml"` that [`select_xmp_item_id`] resolves to the primary
/// image. Used by the write path to preserve existing XMP on rewrite
/// (symmetric with xmp_toolkit's `file.xmp()`), so both ends of a
/// read-merge-write agree on which packet they own.
///
/// Walks top-level boxes by header only and descends into `meta` directly,
/// rather than using `Any::decode_maybe` which dispatches into mp4-atom's
/// full type table on every box kind. That dispatch is unsafe for
/// kei: parsers like `Dfla::decode_body` (`flac.rs::parse_vorbis_comment`)
/// and `Avcc::decode_body` allocated from attacker-controlled length fields
/// without a `min(..)` cap, so a malformed sub-100-byte HEIC turned into a
/// 20+ GiB allocation. Fixed upstream in kixelated/mp4-atom#157 (the rev
/// pinned in Cargo.toml includes it); this header-walk is retained as
/// defense-in-depth against the same class of bug surfacing in a sibling
/// decoder we don't actually need.
pub(crate) fn extract_xmp_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_xmp_strict(bytes).unwrap_or_default()
}

/// Strict variant of [`extract_xmp_bytes`] that distinguishes "the primary
/// image has no XMP item" (`Ok(None)`) from a file that cannot be resolved:
/// `Err(MetaSubBoxDecode)` when the iinf/iloc structure fails to decode, and
/// `Err(InvalidLayout)` when several packets are equally plausible candidates
/// for the primary image. Used by the metadata write path so a malformed or
/// undecidable item map fails loudly instead of silently stripping
/// pre-existing XMP.
pub(crate) fn extract_xmp_strict(bytes: &[u8]) -> Result<Option<Vec<u8>>, HeifError> {
    let mut cursor: &[u8] = bytes;
    while cursor.has_remaining() {
        let Some(header) = Header::decode_maybe(&mut cursor).ok().flatten() else {
            return Ok(None);
        };
        let body_size = header.size.unwrap_or(cursor.remaining());
        if body_size > cursor.remaining() {
            return Ok(None);
        }
        if header.kind == FourCC::new(b"meta") {
            let Some(body) = cursor.get(..body_size) else {
                return Ok(None);
            };
            // HEIC has at most one top-level `meta` box; stop either way.
            return extract_xmp_from_meta(bytes, body);
        }
        cursor.advance(body_size);
    }
    Ok(None)
}

/// Resolve the TIFF payload from the HEIF `Exif` item associated with the
/// primary image.
///
/// HEIF prefixes an Exif item with a four-byte big-endian offset from the end
/// of that field to the TIFF header. Only construction-method-0 items are
/// supported because their extents address file bytes directly.
pub(crate) fn extract_exif_tiff_bytes(bytes: &[u8]) -> Result<Option<Vec<u8>>, HeifError> {
    let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(bytes)?;
    let iinf_layout = parse_iinf(bytes, iinf)?;
    let Some(exif_item_id) = select_exif_item_id(
        bytes,
        iref,
        primary_item_id,
        &iinf_layout.exif_item_ids,
        &iinf_layout.item_ids,
    )?
    else {
        return Ok(None);
    };
    let iloc_layout = parse_iloc(bytes, iloc)?;
    let item = iloc_layout
        .items
        .iter()
        .find(|item| item.item_id == exif_item_id)
        .ok_or_else(|| invalid_layout("Exif item has no iloc entry"))?;
    if item.construction_method != 0 {
        return Err(invalid_layout(
            "Exif item uses an unsupported construction method",
        ));
    }
    let extents = resolve_item_extents(bytes, item)?;
    if extents.is_empty() {
        return Err(invalid_layout("Exif item has no extents"));
    }
    let payload_len = extents.iter().try_fold(0usize, |total, extent| {
        total
            .checked_add(extent.len())
            .filter(|length| *length <= bytes.len())
            .ok_or_else(|| invalid_layout("Exif item payload is too large"))
    })?;
    let mut payload = Vec::with_capacity(payload_len);
    for extent in extents {
        payload.extend_from_slice(extent);
    }
    let offset = payload
        .get(..4)
        .ok_or_else(|| invalid_layout("Exif item offset is truncated"))?;
    let offset = usize::try_from(u32::from_be_bytes(
        offset
            .try_into()
            .map_err(|_| invalid_layout("Exif item offset is invalid"))?,
    ))
    .map_err(|_| invalid_layout("Exif item offset is too large"))?;
    let tiff_start = 4usize
        .checked_add(offset)
        .ok_or_else(|| invalid_layout("Exif TIFF offset overflows"))?;
    let tiff = payload
        .get(tiff_start..)
        .ok_or_else(|| invalid_layout("Exif TIFF offset is outside the item"))?;
    Ok(Some(tiff.to_vec()))
}

#[derive(Debug, Clone, Copy)]
struct RawBox {
    start: usize,
    size: usize,
    header_size: usize,
    kind: [u8; 4],
}

impl RawBox {
    fn body_start(self) -> usize {
        self.start + self.header_size
    }

    fn end(self) -> usize {
        self.start + self.size
    }
}

#[derive(Debug, Clone)]
struct IlocExtent {
    offset_pos: Option<usize>,
    length_pos: Option<usize>,
    offset: u64,
    length: u64,
}

#[derive(Debug, Clone)]
struct IlocItem {
    item_id: u32,
    construction_method: u8,
    data_reference_index: u16,
    base_offset_pos: Option<usize>,
    base_offset: u64,
    extents: Vec<IlocExtent>,
}

#[derive(Debug, Clone)]
struct IlocLayout {
    version: u8,
    offset_size: u8,
    length_size: u8,
    base_offset_size: u8,
    index_size: u8,
    count_pos: usize,
    count_size: usize,
    items: Vec<IlocItem>,
}

#[derive(Debug)]
struct IinfLayout {
    version: u8,
    count_pos: usize,
    count_size: usize,
    max_item_id: u32,
    xmp_item_ids: Vec<u32>,
    exif_item_ids: Vec<u32>,
    tone_map_item_ids: Vec<u32>,
    item_ids: Vec<u32>,
}

#[derive(Debug, Clone, Copy)]
struct XmpLocation {
    item_id: u32,
    extent_start: usize,
    extent_length: usize,
}

fn invalid_layout(reason: &'static str) -> HeifError {
    HeifError::InvalidLayout { reason }
}

#[allow(
    clippy::indexing_slicing,
    reason = "The initial get proves the fixed eight-byte header before its size and type fields are sliced."
)]
fn parse_raw_box(bytes: &[u8], start: usize) -> Result<RawBox, HeifError> {
    let header = bytes
        .get(start..start.saturating_add(8))
        .ok_or_else(|| invalid_layout("truncated ISO-BMFF box header"))?;
    let size32 = u32::from_be_bytes(
        header[0..4]
            .try_into()
            .map_err(|_| invalid_layout("invalid ISO-BMFF box size"))?,
    );
    let kind = header[4..8]
        .try_into()
        .map_err(|_| invalid_layout("invalid ISO-BMFF box type"))?;
    let (header_size, size) = match size32 {
        0 => {
            return Err(invalid_layout(
                "ISO-BMFF box has no explicit size and cannot be rewritten",
            ));
        }
        1 => {
            let large = bytes
                .get(start + 8..start + 16)
                .ok_or_else(|| invalid_layout("truncated large ISO-BMFF box header"))?;
            let size = u64::from_be_bytes(
                large
                    .try_into()
                    .map_err(|_| invalid_layout("invalid large ISO-BMFF box size"))?,
            );
            let size =
                usize::try_from(size).map_err(|_| invalid_layout("ISO-BMFF box is too large"))?;
            (16, size)
        }
        size => (8, size as usize),
    };
    if size < header_size {
        return Err(invalid_layout("ISO-BMFF box is smaller than its header"));
    }
    let end = start
        .checked_add(size)
        .ok_or_else(|| invalid_layout("ISO-BMFF box end overflows"))?;
    if end > bytes.len() {
        return Err(invalid_layout("ISO-BMFF box extends past the file"));
    }
    Ok(RawBox {
        start,
        size,
        header_size,
        kind,
    })
}

fn scan_raw_boxes(bytes: &[u8], start: usize, end: usize) -> Result<Vec<RawBox>, HeifError> {
    if start > end || end > bytes.len() {
        return Err(invalid_layout("invalid ISO-BMFF box range"));
    }
    let mut boxes = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let atom = parse_raw_box(bytes, cursor)?;
        if atom.end() > end {
            return Err(invalid_layout(
                "nested ISO-BMFF box extends past its parent",
            ));
        }
        boxes.push(atom);
        cursor = atom.end();
    }
    if cursor != end {
        return Err(invalid_layout(
            "nested ISO-BMFF boxes leave a trailing fragment",
        ));
    }
    Ok(boxes)
}

fn box_with_body(kind: [u8; 4], body: &[u8]) -> Result<Vec<u8>, HeifError> {
    let size = body
        .len()
        .checked_add(8)
        .ok_or_else(|| invalid_layout("rewritten ISO-BMFF box size overflows"))?;
    let size =
        u32::try_from(size).map_err(|_| invalid_layout("rewritten ISO-BMFF box is too large"))?;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    Ok(out)
}

fn patch_box_size(box_bytes: &mut [u8]) -> Result<(), HeifError> {
    let size = u32::try_from(box_bytes.len())
        .map_err(|_| invalid_layout("rewritten ISO-BMFF box is too large"))?;
    let size_bytes = box_bytes
        .get_mut(..4)
        .ok_or_else(|| invalid_layout("rewritten ISO-BMFF box has no size field"))?;
    size_bytes.copy_from_slice(&size.to_be_bytes());
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the body range, and the explicit minimum lengths prove each fixed iinf field."
)]
fn parse_iinf(bytes: &[u8], iinf: RawBox) -> Result<IinfLayout, HeifError> {
    let body = &bytes[iinf.body_start()..iinf.end()];
    if body.len() < 6 {
        return Err(invalid_layout("iinf box is truncated"));
    }
    let version = body[0];
    let (count_pos, count_size, count) = match version {
        0 => (4, 2, u16::from_be_bytes([body[4], body[5]]) as u32),
        1 => {
            if body.len() < 8 {
                return Err(invalid_layout("iinf version 1 box is truncated"));
            }
            (
                4,
                4,
                u32::from_be_bytes(
                    body.get(4..8)
                        .ok_or_else(|| invalid_layout("iinf entry count is truncated"))?
                        .try_into()
                        .map_err(|_| invalid_layout("invalid iinf entry count"))?,
                ),
            )
        }
        _ => return Err(invalid_layout("unsupported iinf version")),
    };
    let entry_start = count_pos + count_size;
    let entries = scan_raw_boxes(body, entry_start, body.len())?;
    if entries.len() != usize::try_from(count).unwrap_or(usize::MAX) {
        return Err(invalid_layout(
            "iinf entry count does not match its contents",
        ));
    }
    let mut max_item_id = 0u32;
    let mut xmp_item_ids = Vec::new();
    let mut exif_item_ids = Vec::new();
    let mut tone_map_item_ids = Vec::new();
    let mut item_ids = HashSet::with_capacity(entries.len());
    for entry in entries {
        if entry.kind != *b"infe" {
            return Err(invalid_layout("iinf contains a non-infe entry"));
        }
        let entry_body = &body[entry.body_start()..entry.end()];
        let (item_id, item_type, content_type) = parse_infe(entry_body)?;
        if !item_ids.insert(item_id) {
            return Err(invalid_layout("iinf contains duplicate item IDs"));
        }
        max_item_id = max_item_id.max(item_id);
        if item_type == Some(*b"mime") && content_type.as_deref() == Some("application/rdf+xml") {
            xmp_item_ids.push(item_id);
        }
        if item_type == Some(*b"Exif") {
            exif_item_ids.push(item_id);
        }
        if item_type == Some(*b"tmap") {
            tone_map_item_ids.push(item_id);
        }
    }
    Ok(IinfLayout {
        version,
        count_pos,
        count_size,
        max_item_id,
        xmp_item_ids,
        exif_item_ids,
        tone_map_item_ids,
        item_ids: item_ids.into_iter().collect(),
    })
}

#[allow(
    clippy::indexing_slicing,
    reason = "The explicit version-specific minimum lengths prove each fixed infe field before access."
)]
fn parse_infe(body: &[u8]) -> Result<(u32, Option<[u8; 4]>, Option<String>), HeifError> {
    if body.len() < 8 {
        return Err(invalid_layout("infe box is truncated"));
    }
    let version = body[0];
    let (item_id, type_pos) = match version {
        0 => (u16::from_be_bytes([body[4], body[5]]) as u32, None),
        1 => return Err(invalid_layout("unsupported infe version 1")),
        2 => (u16::from_be_bytes([body[4], body[5]]) as u32, Some(8)),
        3 => {
            if body.len() < 14 {
                return Err(invalid_layout("infe version 3 box is truncated"));
            }
            (
                u32::from_be_bytes(
                    body.get(4..8)
                        .ok_or_else(|| invalid_layout("infe item id is truncated"))?
                        .try_into()
                        .map_err(|_| invalid_layout("invalid infe item id"))?,
                ),
                Some(10),
            )
        }
        _ => return Err(invalid_layout("unsupported infe version")),
    };
    let Some(type_pos) = type_pos else {
        return Ok((item_id, None, None));
    };
    let item_type = body
        .get(type_pos..type_pos + 4)
        .ok_or_else(|| invalid_layout("infe item type is truncated"))?
        .try_into()
        .map_err(|_| invalid_layout("invalid infe item type"))?;
    let mut cursor = type_pos + 4;
    skip_c_string(body, &mut cursor)?;
    let content_type = if item_type == *b"mime" {
        Some(read_c_string(body, &mut cursor)?.to_string())
    } else if item_type == *b"uri " {
        let _ = read_c_string(body, &mut cursor)?;
        None
    } else {
        None
    };
    Ok((item_id, Some(item_type), content_type))
}

#[allow(
    clippy::indexing_slicing,
    reason = "The terminator position comes from iterating the same slice, so the string range is in bounds."
)]
fn read_c_string<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a str, HeifError> {
    let rest = bytes
        .get(*cursor..)
        .ok_or_else(|| invalid_layout("unterminated HEIC item string"))?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| invalid_layout("unterminated HEIC item string"))?;
    let value = std::str::from_utf8(&rest[..end])
        .map_err(|_| invalid_layout("HEIC item string is not UTF-8"))?;
    *cursor += end + 1;
    Ok(value)
}

fn skip_c_string(bytes: &[u8], cursor: &mut usize) -> Result<(), HeifError> {
    let _ = read_c_string(bytes, cursor)?;
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the body range, and the explicit minimum lengths prove each fixed iloc field."
)]
fn parse_iloc(bytes: &[u8], iloc: RawBox) -> Result<IlocLayout, HeifError> {
    let body = &bytes[iloc.body_start()..iloc.end()];
    if body.len() < 8 {
        return Err(invalid_layout("iloc box is truncated"));
    }
    let version = body[0];
    if version > 2 {
        return Err(invalid_layout("unsupported iloc version"));
    }
    let offset_size = body[4] >> 4;
    let length_size = body[4] & 0x0f;
    let base_offset_size = body[5] >> 4;
    let index_size = if version == 0 { 0 } else { body[5] & 0x0f };
    for size in [offset_size, length_size, base_offset_size, index_size] {
        if !matches!(size, 0 | 4 | 8) {
            return Err(invalid_layout("iloc uses a reserved field width"));
        }
    }
    let (count_pos, count_size, count) = if version == 2 {
        if body.len() < 10 {
            return Err(invalid_layout("iloc version 2 box is truncated"));
        }
        (
            6,
            4,
            u32::from_be_bytes(
                body.get(6..10)
                    .ok_or_else(|| invalid_layout("iloc item count is truncated"))?
                    .try_into()
                    .map_err(|_| invalid_layout("invalid iloc item count"))?,
            ) as u64,
        )
    } else {
        (6, 2, u16::from_be_bytes([body[6], body[7]]) as u64)
    };
    let count =
        usize::try_from(count).map_err(|_| invalid_layout("iloc item count is too large"))?;
    // Every item occupies at least its id, the optional construction and
    // reserved fields, the data-reference index, its base offset, and the
    // extent count. Reject a count that cannot be backed by the remaining
    // body so a crafted iloc cannot force a large speculative allocation.
    let item_id_size = if version == 2 { 4u8 } else { 2 };
    let min_item_bytes = usize::from(item_id_size)
        + usize::from(if version == 0 { 0u8 } else { 2 })
        + 2
        + usize::from(base_offset_size)
        + 2;
    let body_after_count = body.len() - (count_pos + count_size);
    if count > body_after_count / min_item_bytes {
        return Err(invalid_layout("iloc item count exceeds its box body"));
    }
    let mut cursor = count_pos + count_size;
    let mut items = Vec::with_capacity(count);
    let mut item_ids = HashSet::with_capacity(count);
    for _ in 0..count {
        let item_id = u32::try_from(read_uint(body, &mut cursor, item_id_size)?)
            .map_err(|_| invalid_layout("iloc item ID is too large"))?;
        if !item_ids.insert(item_id) {
            return Err(invalid_layout("iloc contains duplicate item IDs"));
        }
        let construction_method = if version == 0 {
            0
        } else {
            let packed = read_uint(body, &mut cursor, 2)?;
            (packed & 0x0f) as u8
        };
        let data_reference_index = u16::try_from(read_uint(body, &mut cursor, 2)?)
            .map_err(|_| invalid_layout("iloc data reference index is too large"))?;
        let base_offset_pos = if base_offset_size == 0 {
            None
        } else {
            Some(cursor)
        };
        let base_offset = read_uint(body, &mut cursor, base_offset_size)?;
        let extent_count = read_uint(body, &mut cursor, 2)?;
        let extent_count = usize::try_from(extent_count)
            .map_err(|_| invalid_layout("iloc extent count is too large"))?;
        // Each extent occupies its optional index plus its offset and length
        // fields (index_size is zero for version 0). When all three widths are
        // zero the extents carry no bytes, so more than one is meaningless;
        // otherwise reject a count the remaining body cannot hold before
        // allocating for it.
        let min_extent_bytes =
            usize::from(index_size) + usize::from(offset_size) + usize::from(length_size);
        let max_extents = match min_extent_bytes {
            0 => 1,
            unit => (body.len() - cursor) / unit,
        };
        if extent_count > max_extents {
            return Err(invalid_layout("iloc extent count exceeds its box body"));
        }
        let mut extents = Vec::with_capacity(extent_count);
        for _ in 0..extent_count {
            if version != 0 {
                let _ = read_uint(body, &mut cursor, index_size)?;
            }
            let offset_pos = if offset_size == 0 { None } else { Some(cursor) };
            let offset = read_uint(body, &mut cursor, offset_size)?;
            let length_pos = if length_size == 0 { None } else { Some(cursor) };
            let length = read_uint(body, &mut cursor, length_size)?;
            extents.push(IlocExtent {
                offset_pos,
                length_pos,
                offset,
                length,
            });
        }
        items.push(IlocItem {
            item_id,
            construction_method,
            data_reference_index,
            base_offset_pos,
            base_offset,
            extents,
        });
    }
    if cursor != body.len() {
        return Err(invalid_layout("iloc contains an unparsed tail"));
    }
    if items.iter().any(|item| item.data_reference_index != 0) {
        return Err(invalid_layout("HEIC item uses an external data reference"));
    }
    Ok(IlocLayout {
        version,
        offset_size,
        length_size,
        base_offset_size,
        index_size,
        count_pos,
        count_size,
        items,
    })
}

fn read_uint(bytes: &[u8], cursor: &mut usize, width: u8) -> Result<u64, HeifError> {
    let width = usize::from(width);
    if width == 0 {
        return Ok(0);
    }
    let end = cursor
        .checked_add(width)
        .ok_or_else(|| invalid_layout("HEIC integer position overflows"))?;
    let value = match width {
        2 => u16::from_be_bytes(
            bytes
                .get(*cursor..end)
                .ok_or_else(|| invalid_layout("truncated HEIC integer"))?
                .try_into()
                .map_err(|_| invalid_layout("invalid HEIC integer"))?,
        ) as u64,
        4 => u32::from_be_bytes(
            bytes
                .get(*cursor..end)
                .ok_or_else(|| invalid_layout("truncated HEIC integer"))?
                .try_into()
                .map_err(|_| invalid_layout("invalid HEIC integer"))?,
        ) as u64,
        8 => u64::from_be_bytes(
            bytes
                .get(*cursor..end)
                .ok_or_else(|| invalid_layout("truncated HEIC integer"))?
                .try_into()
                .map_err(|_| invalid_layout("invalid HEIC integer"))?,
        ),
        _ => return Err(invalid_layout("unsupported HEIC integer width")),
    };
    *cursor = end;
    Ok(value)
}

fn write_uint(bytes: &mut [u8], pos: usize, width: u8, value: u64) -> Result<(), HeifError> {
    let width = usize::from(width);
    if width == 0 {
        if value == 0 {
            return Ok(());
        }
        return Err(invalid_layout(
            "non-zero value cannot use a zero-width field",
        ));
    }
    let end = pos
        .checked_add(width)
        .ok_or_else(|| invalid_layout("HEIC integer position overflows"))?;
    let dst = bytes
        .get_mut(pos..end)
        .ok_or_else(|| invalid_layout("HEIC integer field is truncated"))?;
    match width {
        2 => {
            let value = u16::try_from(value).map_err(|_| HeifError::ValueOverflow {
                field: "16-bit HEIC field",
            })?;
            dst.copy_from_slice(&value.to_be_bytes());
        }
        4 => {
            let value = u32::try_from(value).map_err(|_| HeifError::ValueOverflow {
                field: "32-bit HEIC field",
            })?;
            dst.copy_from_slice(&value.to_be_bytes());
        }
        8 => dst.copy_from_slice(&value.to_be_bytes()),
        _ => return Err(invalid_layout("unsupported HEIC integer width")),
    }
    Ok(())
}

fn find_meta_layout(
    bytes: &[u8],
) -> Result<(RawBox, RawBox, RawBox, Option<RawBox>, u32, usize), HeifError> {
    let top = scan_raw_boxes(bytes, 0, bytes.len())?;
    let meta = top
        .iter()
        .copied()
        .find(|atom| atom.kind == *b"meta")
        .ok_or_else(|| invalid_layout("HEIC has no top-level meta box"))?;
    const MAX_REWRITE_META_BYTES: usize = 8 * 1024 * 1024;
    if meta.size > MAX_REWRITE_META_BYTES {
        return Err(invalid_layout(
            "HEIC meta box is too large to rewrite safely",
        ));
    }
    if meta.header_size != 8 {
        return Err(invalid_layout("large-size meta boxes are unsupported"));
    }
    let body_start = meta.body_start();
    let body_end = meta.end();
    let prefix_size = if bytes
        .get(body_start + 8..body_start + 12)
        .is_some_and(|kind| kind == b"hdlr")
    {
        4
    } else if bytes
        .get(body_start + 4..body_start + 8)
        .is_some_and(|kind| kind == b"hdlr")
    {
        0
    } else {
        return Err(invalid_layout("HEIC meta box has no handler box"));
    };
    let children = scan_raw_boxes(bytes, body_start + prefix_size, body_end)?;
    let iinf = children
        .iter()
        .copied()
        .find(|atom| atom.kind == *b"iinf")
        .ok_or_else(|| invalid_layout("HEIC meta box has no iinf box"))?;
    let iloc = children
        .iter()
        .copied()
        .find(|atom| atom.kind == *b"iloc")
        .ok_or_else(|| invalid_layout("HEIC meta box has no iloc box"))?;
    let iref = children.iter().copied().find(|atom| atom.kind == *b"iref");
    let pitm = children
        .iter()
        .copied()
        .find(|atom| atom.kind == *b"pitm")
        .ok_or_else(|| invalid_layout("HEIC meta box has no primary item"))?;
    let primary_item_id = parse_primary_item_id(bytes, pitm)?;
    if iinf.header_size != 8 || iloc.header_size != 8 {
        return Err(invalid_layout(
            "large-size iinf or iloc boxes are unsupported",
        ));
    }
    if let Some(iref) = iref
        && iref.header_size != 8
    {
        return Err(invalid_layout("large-size iref boxes are unsupported"));
    }
    Ok((meta, iinf, iloc, iref, primary_item_id, prefix_size))
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the body range, and the six-byte minimum proves the version-0 pitm fields."
)]
fn parse_primary_item_id(bytes: &[u8], pitm: RawBox) -> Result<u32, HeifError> {
    let body = &bytes[pitm.body_start()..pitm.end()];
    if body.len() < 6 {
        return Err(invalid_layout("pitm box is truncated"));
    }
    match body[0] {
        0 => Ok(u32::from(u16::from_be_bytes([body[4], body[5]]))),
        1 => {
            let item_id = body
                .get(4..8)
                .ok_or_else(|| invalid_layout("pitm version 1 box is truncated"))?;
            Ok(u32::from_be_bytes(
                item_id
                    .try_into()
                    .map_err(|_| invalid_layout("invalid pitm item id"))?,
            ))
        }
        _ => Err(invalid_layout("unsupported pitm version")),
    }
}

fn resolve_item_extents<'a>(bytes: &'a [u8], item: &IlocItem) -> Result<Vec<&'a [u8]>, HeifError> {
    let mut resolved = Vec::with_capacity(item.extents.len());
    for extent in &item.extents {
        let start = item
            .base_offset
            .checked_add(extent.offset)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| invalid_layout("HEIC item offset overflows"))?;
        let length = usize::try_from(extent.length)
            .map_err(|_| invalid_layout("HEIC item extent is too large"))?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| invalid_layout("HEIC item extent overflows"))?;
        let data = bytes
            .get(start..end)
            .ok_or_else(|| invalid_layout("HEIC item extent is outside the file"))?;
        resolved.push(data);
    }
    Ok(resolved)
}

fn opaque_meta_sub_boxes(bytes: &[u8]) -> Result<Vec<([u8; 4], usize, usize)>, HeifError> {
    let (meta, _, _, _, _, prefix_size) = find_meta_layout(bytes)?;
    let children = scan_raw_boxes(bytes, meta.body_start() + prefix_size, meta.end())?;
    Ok(children
        .into_iter()
        .filter(|child| !matches!(&child.kind, b"iinf" | b"iloc" | b"iref"))
        .map(|child| (child.kind, child.start, child.end()))
        .collect())
}

/// Verify that a rewritten HEIF preserves every item payload except the
/// primary image's XMP packet, and every opaque `meta` sub-box, byte-for-byte.
///
/// The writer may update `iinf`, `iloc`, and `iref`, and may replace or append
/// the packet it owns. Any other difference is unsafe because it can redirect
/// or damage image data while leaving the XMP item readable. An auxiliary
/// image's XMP is that image's metadata, so it is preserved like any other
/// payload.
pub(crate) fn validate_rewrite_preserves_non_xmp_items(
    input: &[u8],
    rewritten: &[u8],
) -> Result<(), HeifError> {
    let (_, input_iinf, input_iloc, input_iref, input_primary, _) = find_meta_layout(input)?;
    let input_iinf_layout = parse_iinf(input, input_iinf)?;
    let input_iloc_layout = parse_iloc(input, input_iloc)?;
    let input_xmp_item_id = select_xmp_item_id(
        input,
        input_iref,
        input_primary,
        &input_iinf_layout.xmp_item_ids,
    )?;
    let (_, rewritten_iinf, rewritten_iloc, rewritten_iref, rewritten_primary, _) =
        find_meta_layout(rewritten)?;
    let rewritten_iinf_layout = parse_iinf(rewritten, rewritten_iinf)?;
    let rewritten_iloc_layout = parse_iloc(rewritten, rewritten_iloc)?;
    let rewritten_xmp_item_id = select_xmp_item_id(
        rewritten,
        rewritten_iref,
        rewritten_primary,
        &rewritten_iinf_layout.xmp_item_ids,
    )?;

    let input_items = input_iloc_layout
        .items
        .iter()
        .filter(|item| item.construction_method == 0 && Some(item.item_id) != input_xmp_item_id)
        .collect::<Vec<_>>();
    let rewritten_items = rewritten_iloc_layout
        .items
        .iter()
        .filter(|item| item.construction_method == 0 && Some(item.item_id) != rewritten_xmp_item_id)
        .collect::<Vec<_>>();
    if input_items.len() != rewritten_items.len() {
        return Err(invalid_layout(
            "HEIC rewrite changed the construction-method-0 item set",
        ));
    }
    for input_item in input_items {
        let rewritten_item = rewritten_items
            .iter()
            .find(|item| item.item_id == input_item.item_id)
            .ok_or_else(|| invalid_layout("HEIC rewrite removed a non-XMP item"))?;
        let input_extents = resolve_item_extents(input, input_item)?;
        let rewritten_extents = resolve_item_extents(rewritten, rewritten_item)?;
        if input_extents.len() != rewritten_extents.len()
            || input_extents
                .iter()
                .zip(&rewritten_extents)
                .any(|(before, after)| before != after)
        {
            return Err(invalid_layout(
                "HEIC rewrite changed a non-XMP item payload",
            ));
        }
    }

    let input_meta = opaque_meta_sub_boxes(input)?;
    let rewritten_meta = opaque_meta_sub_boxes(rewritten)?;
    if input_meta.len() != rewritten_meta.len() {
        return Err(invalid_layout(
            "HEIC rewrite changed the opaque meta sub-box set",
        ));
    }
    for ((input_kind, input_start, input_end), (rewritten_kind, rewritten_start, rewritten_end)) in
        input_meta.iter().zip(&rewritten_meta)
    {
        if input_kind != rewritten_kind
            || input.get(*input_start..*input_end)
                != rewritten.get(*rewritten_start..*rewritten_end)
        {
            return Err(invalid_layout(
                "HEIC rewrite changed an opaque meta sub-box",
            ));
        }
    }
    Ok(())
}

fn ensure_insertion_layout(bytes: &[u8]) -> Result<(), HeifError> {
    let top = scan_raw_boxes(bytes, 0, bytes.len())?;
    for atom in top {
        let allowed = atom.kind == *b"ftyp"
            || atom.kind == *b"meta"
            || atom.kind == *b"mdat"
            || atom.kind == *b"free"
            || atom.kind == *b"wide";
        if !allowed {
            return Err(invalid_layout(
                "HEIC insertion refuses top-level boxes with unhandled file offsets",
            ));
        }
    }
    Ok(())
}

/// Refuse insertion when a construction-method-0 item's data overlaps the
/// `meta` box. Insertion grows `meta` and relocates everything after it, so an
/// item whose bytes fall inside the region being rewritten cannot be preserved
/// or safely repointed. Such a layout is malformed; fail closed and leave the
/// file untouched rather than emit a file whose item points at changed bytes.
fn reject_meta_overlapping_items(layout: &IlocLayout, meta: RawBox) -> Result<(), HeifError> {
    let meta_start = meta.start as u64;
    let meta_end = meta.end() as u64;
    for item in &layout.items {
        if item.construction_method != 0 {
            continue;
        }
        for extent in &item.extents {
            let start = item
                .base_offset
                .checked_add(extent.offset)
                .ok_or_else(|| invalid_layout("HEIC item offset overflows"))?;
            let end = start
                .checked_add(extent.length)
                .ok_or_else(|| invalid_layout("HEIC item extent overflows"))?;
            if start < meta_end && meta_start < end {
                return Err(invalid_layout(
                    "HEIC item data overlaps the meta box and cannot be rewritten",
                ));
            }
        }
    }
    Ok(())
}

fn make_infe(item_id: u32) -> Result<Vec<u8>, HeifError> {
    let mut body = Vec::new();
    body.extend_from_slice(&[if item_id <= u16::MAX as u32 { 2 } else { 3 }, 0, 0, 0]);
    if item_id <= u16::MAX as u32 {
        let item_id = u16::try_from(item_id)
            .map_err(|_| invalid_layout("HEIC item ID does not fit infe version 2"))?;
        body.extend_from_slice(&item_id.to_be_bytes());
    } else {
        body.extend_from_slice(&item_id.to_be_bytes());
    }
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(b"mime");
    body.extend_from_slice(b"XMP\0");
    body.extend_from_slice(b"application/rdf+xml\0");
    body.push(0);
    box_with_body(*b"infe", &body)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the iinf body range before it is copied for a bounded field update."
)]
fn append_iinf_entry(
    bytes: &[u8],
    iinf: RawBox,
    count_pos: usize,
    count_size: usize,
    item_id: u32,
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iinf.body_start()..iinf.end()];
    let entry = make_infe(item_id)?;
    let mut new_body = body.to_vec();
    let mut count_cursor = count_pos;
    let count_size =
        u8::try_from(count_size).map_err(|_| invalid_layout("iinf count width is too large"))?;
    let count = read_uint(new_body.as_slice(), &mut count_cursor, count_size)?;
    let new_count = count
        .checked_add(1)
        .ok_or_else(|| invalid_layout("iinf entry count overflows"))?;
    write_uint(&mut new_body, count_pos, count_size, new_count)?;
    new_body.extend_from_slice(&entry);
    box_with_body(iinf.kind, &new_body)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser and version-specific size checks prove every iref child field before direct access."
)]
fn cdsc_pairs(bytes: &[u8], iref: RawBox) -> Result<Vec<(u32, Vec<u32>)>, HeifError> {
    let body = &bytes[iref.body_start()..iref.end()];
    if body.len() < 4 {
        return Err(invalid_layout("iref box is truncated"));
    }
    let version = body[0];
    let children = scan_raw_boxes(body, 4, body.len())?;
    let mut pairs = Vec::new();
    for child in children {
        if child.kind != *b"cdsc" {
            continue;
        }
        let child_body = &body[child.body_start()..child.end()];
        let (from_item_id, count_pos, id_width) = match version {
            0 => {
                if child_body.len() < 4 {
                    return Err(invalid_layout("iref child is truncated"));
                }
                (
                    u32::from(u16::from_be_bytes([child_body[0], child_body[1]])),
                    2,
                    2,
                )
            }
            1 => {
                if child_body.len() < 6 {
                    return Err(invalid_layout("iref version 1 child is truncated"));
                }
                (
                    u32::from_be_bytes(
                        child_body[0..4]
                            .try_into()
                            .map_err(|_| invalid_layout("invalid iref source item ID"))?,
                    ),
                    4,
                    4,
                )
            }
            _ => return Err(invalid_layout("unsupported iref version")),
        };
        let count = usize::from(u16::from_be_bytes(
            child_body[count_pos..count_pos + 2]
                .try_into()
                .map_err(|_| invalid_layout("invalid iref reference count"))?,
        ));
        let expected_len = count_pos
            .checked_add(2)
            .and_then(|length| length.checked_add(count.checked_mul(id_width)?))
            .ok_or_else(|| invalid_layout("iref reference list overflows"))?;
        if child_body.len() != expected_len {
            return Err(invalid_layout("iref reference list is inconsistent"));
        }
        let mut targets = Vec::with_capacity(count);
        for index in 0..count {
            let start = count_pos + 2 + index * id_width;
            let to_item_id = if id_width == 2 {
                u32::from(u16::from_be_bytes([
                    child_body[start],
                    child_body[start + 1],
                ]))
            } else {
                u32::from_be_bytes(
                    child_body[start..start + id_width]
                        .try_into()
                        .map_err(|_| invalid_layout("invalid iref target item ID"))?,
                )
            };
            targets.push(to_item_id);
        }
        pairs.push((from_item_id, targets));
    }
    Ok(pairs)
}

/// Resolve which XMP item holds the primary image's metadata.
///
/// A `cdsc` reference binds a descriptive item to the image it describes, so
/// an XMP item that names an auxiliary image (a depth map, a portrait matte, a
/// gain map) is that image's metadata and not the photograph's. Absence of a
/// reference is evidence too: when every packet names an auxiliary image the
/// primary has no XMP of its own, which is the same conclusion as a file with
/// no XMP at all, and `Ok(None)` lets the caller insert one.
///
/// Items with no `cdsc` reference are unattributed. A single unattributed
/// packet is the ordinary single-XMP HEIC and resolves to itself; several are
/// undecidable and fail closed rather than risk overwriting the wrong one.
fn select_xmp_item_id(
    bytes: &[u8],
    iref: Option<RawBox>,
    primary_item_id: u32,
    xmp_item_ids: &[u32],
) -> Result<Option<u32>, HeifError> {
    let Some((first, rest)) = xmp_item_ids.split_first() else {
        return Ok(None);
    };
    let Some(iref) = iref else {
        return if rest.is_empty() {
            Ok(Some(*first))
        } else {
            Err(invalid_layout(
                "multiple XMP items have no primary-image association",
            ))
        };
    };
    let pairs = cdsc_pairs(bytes, iref)?;
    let mut primary = xmp_item_ids.iter().copied().filter(|item_id| {
        pairs
            .iter()
            .any(|(from, targets)| from == item_id && targets.contains(&primary_item_id))
    });
    if let Some(item_id) = primary.next() {
        return if primary.next().is_some() {
            Err(invalid_layout(
                "multiple XMP items describe the primary image",
            ))
        } else {
            Ok(Some(item_id))
        };
    }
    let mut unattributed = xmp_item_ids
        .iter()
        .copied()
        .filter(|item_id| !pairs.iter().any(|(from, _)| from == item_id));
    match (unattributed.next(), unattributed.next()) {
        (None, _) => Ok(None),
        (Some(item_id), None) => Ok(Some(item_id)),
        (Some(_), Some(_)) => Err(invalid_layout(
            "multiple XMP items have no primary-image association",
        )),
    }
}

fn select_exif_item_id(
    bytes: &[u8],
    iref: Option<RawBox>,
    primary_item_id: u32,
    exif_item_ids: &[u32],
    known_item_ids: &[u32],
) -> Result<Option<u32>, HeifError> {
    let Some((first, rest)) = exif_item_ids.split_first() else {
        return Ok(None);
    };
    let Some(iref) = iref else {
        return if rest.is_empty() {
            Ok(Some(*first))
        } else {
            Err(invalid_layout(
                "multiple Exif items have no primary-image association",
            ))
        };
    };
    let pairs = cdsc_pairs(bytes, iref)?;
    let known_item_ids: HashSet<u32> = known_item_ids.iter().copied().collect();
    if pairs.iter().any(|(from, targets)| {
        exif_item_ids.contains(from)
            && targets
                .iter()
                .any(|target| !known_item_ids.contains(target))
    }) {
        return Err(invalid_layout(
            "Exif item describes an item absent from iinf",
        ));
    }
    let mut primary = exif_item_ids.iter().copied().filter(|item_id| {
        pairs
            .iter()
            .any(|(from, targets)| from == item_id && targets.contains(&primary_item_id))
    });
    if let Some(item_id) = primary.next() {
        return if primary.next().is_some() {
            Err(invalid_layout(
                "multiple Exif items describe the primary image",
            ))
        } else {
            Ok(Some(item_id))
        };
    }
    let mut unattributed = exif_item_ids
        .iter()
        .copied()
        .filter(|item_id| !pairs.iter().any(|(from, _)| from == item_id));
    match (unattributed.next(), unattributed.next()) {
        (None, _) => Ok(None),
        (Some(item_id), None) => Ok(Some(item_id)),
        (Some(_), Some(_)) => Err(invalid_layout(
            "multiple Exif items have no primary-image association",
        )),
    }
}

/// Encode one `cdsc` reference from a descriptive item to the images it
/// describes. `version` is the enclosing `iref` version, which fixes the
/// item-id width at 16 or 32 bits.
fn cdsc_body(version: u8, from_item_id: u32, to_item_ids: &[u32]) -> Result<Vec<u8>, HeifError> {
    let count = u16::try_from(to_item_ids.len())
        .map_err(|_| invalid_layout("cdsc names too many images"))?;
    if count == 0 {
        return Err(invalid_layout("cdsc names no image"));
    }
    let mut body = Vec::new();
    match version {
        0 => {
            let from = u16::try_from(from_item_id).map_err(|_| HeifError::ValueOverflow {
                field: "16-bit cdsc source item id",
            })?;
            body.extend_from_slice(&from.to_be_bytes());
            body.extend_from_slice(&count.to_be_bytes());
            for to_item_id in to_item_ids {
                let to = u16::try_from(*to_item_id).map_err(|_| HeifError::ValueOverflow {
                    field: "16-bit cdsc target item id",
                })?;
                body.extend_from_slice(&to.to_be_bytes());
            }
        }
        1 => {
            body.extend_from_slice(&from_item_id.to_be_bytes());
            body.extend_from_slice(&count.to_be_bytes());
            for to_item_id in to_item_ids {
                body.extend_from_slice(&to_item_id.to_be_bytes());
            }
        }
        _ => return Err(invalid_layout("unsupported iref version")),
    }
    Ok(body)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser and version-specific size checks prove every iref child field before direct access."
)]
fn append_cdsc_reference(
    bytes: &[u8],
    iref: RawBox,
    xmp_item_id: u32,
    described_item_ids: &[u32],
    known_item_ids: &[u32],
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iref.body_start()..iref.end()];
    if body.len() < 4 {
        return Err(invalid_layout("iref box is truncated"));
    }
    let version = body[0];
    let children = scan_raw_boxes(body, 4, body.len())?;
    let known_item_ids: HashSet<u32> = known_item_ids.iter().copied().collect();
    for child in children {
        let child_body = &body[child.body_start()..child.end()];
        let (from_item_id, count_pos, id_width) = match version {
            0 => {
                if child_body.len() < 4 {
                    return Err(invalid_layout("iref child is truncated"));
                }
                (
                    u32::from(u16::from_be_bytes([child_body[0], child_body[1]])),
                    2,
                    2,
                )
            }
            1 => {
                if child_body.len() < 6 {
                    return Err(invalid_layout("iref version 1 child is truncated"));
                }
                (
                    u32::from_be_bytes(
                        child_body[0..4]
                            .try_into()
                            .map_err(|_| invalid_layout("invalid iref source item ID"))?,
                    ),
                    4,
                    4,
                )
            }
            _ => return Err(invalid_layout("unsupported iref version")),
        };
        let count = u16::from_be_bytes(
            child_body[count_pos..count_pos + 2]
                .try_into()
                .map_err(|_| invalid_layout("invalid iref reference count"))?,
        ) as usize;
        let expected_len = count_pos
            .checked_add(2)
            .and_then(|len| len.checked_add(count.checked_mul(id_width)?))
            .ok_or_else(|| invalid_layout("iref reference list overflows"))?;
        if child_body.len() != expected_len {
            return Err(invalid_layout("iref reference list is inconsistent"));
        }
        if !known_item_ids.contains(&from_item_id) {
            return Err(invalid_layout("iref source item ID is absent from iinf"));
        }
        for index in 0..count {
            let start = count_pos + 2 + index * id_width;
            let to_item_id = if id_width == 2 {
                u32::from(u16::from_be_bytes([
                    child_body[start],
                    child_body[start + 1],
                ]))
            } else {
                u32::from_be_bytes(
                    child_body[start..start + id_width]
                        .try_into()
                        .map_err(|_| invalid_layout("invalid iref target item ID"))?,
                )
            };
            if !known_item_ids.contains(&to_item_id) {
                return Err(invalid_layout("iref target item ID is absent from iinf"));
            }
        }
    }
    let mut new_body = body.to_vec();
    new_body.extend_from_slice(&box_with_body(
        *b"cdsc",
        &cdsc_body(version, xmp_item_id, described_item_ids)?,
    )?);
    box_with_body(iref.kind, &new_body)
}

/// Build a fresh `iref` box carrying a single `cdsc` reference from the new XMP
/// item to the images it describes. Used when a file has no `iref` of its own so
/// insertion can still associate the descriptive XMP with the image, the way
/// Apple's own writer does, rather than refusing the file and leaving a retry
/// marker that never resolves. Version 0 encodes 16-bit ids; version 1 is used
/// when any id exceeds 16 bits.
fn synthesise_cdsc_iref(
    xmp_item_id: u32,
    described_item_ids: &[u32],
) -> Result<Vec<u8>, HeifError> {
    let version = if xmp_item_id <= u32::from(u16::MAX)
        && described_item_ids
            .iter()
            .all(|item_id| *item_id <= u32::from(u16::MAX))
    {
        0u8
    } else {
        1u8
    };
    let mut body = vec![version, 0, 0, 0];
    body.extend_from_slice(&box_with_body(
        *b"cdsc",
        &cdsc_body(version, xmp_item_id, described_item_ids)?,
    )?);
    box_with_body(*b"iref", &body)
}

fn locate_xmp(
    bytes: &[u8],
    iinf: RawBox,
    iloc: RawBox,
    iref: Option<RawBox>,
    primary_item_id: u32,
) -> Result<
    (
        Option<XmpLocation>,
        Option<u32>,
        u32,
        u8,
        usize,
        usize,
        Vec<u32>,
    ),
    HeifError,
> {
    let iinf_layout = parse_iinf(bytes, iinf)?;
    let iloc_layout = parse_iloc(bytes, iloc)?;
    let mut max_item_id = iinf_layout.max_item_id;
    for item in &iloc_layout.items {
        if !iinf_layout.item_ids.contains(&item.item_id) {
            return Err(invalid_layout("iloc contains an item ID absent from iinf"));
        }
        max_item_id = max_item_id.max(item.item_id);
    }
    let xmp_item_id = select_xmp_item_id(bytes, iref, primary_item_id, &iinf_layout.xmp_item_ids)?;
    let xmp = xmp_item_id.and_then(|item_id| {
        iloc_layout
            .items
            .iter()
            .find(|item| item.item_id == item_id)
            .and_then(|item| {
                let extent = item.extents.first()?;
                let start = item.base_offset.checked_add(extent.offset)?;
                let start = usize::try_from(start).ok()?;
                let length = usize::try_from(extent.length).ok()?;
                let end = start.checked_add(length)?;
                if item.construction_method == 0 && end <= bytes.len() {
                    Some(XmpLocation {
                        item_id,
                        extent_start: start,
                        extent_length: length,
                    })
                } else {
                    None
                }
            })
    });
    Ok((
        xmp,
        xmp_item_id,
        max_item_id,
        iinf_layout.version,
        iinf_layout.count_pos,
        iinf_layout.count_size,
        iinf_layout.item_ids,
    ))
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the iloc body range, and the single-extent check proves the accessed entry."
)]
fn rewrite_existing_iloc(
    bytes: &[u8],
    iloc: RawBox,
    layout: &IlocLayout,
    item_id: u32,
    new_offset: u64,
    new_length: u64,
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iloc.body_start()..iloc.end()];
    let mut new_body = body.to_vec();
    let item = layout
        .items
        .iter()
        .find(|item| item.item_id == item_id)
        .ok_or_else(|| invalid_layout("XMP item has no iloc entry"))?;
    if item.construction_method != 0 || item.extents.len() != 1 {
        return Err(invalid_layout("XMP item uses an unsupported iloc layout"));
    }
    let extent = &item.extents[0];
    if let Some(pos) = extent.offset_pos {
        let offset = new_offset
            .checked_sub(item.base_offset)
            .ok_or_else(|| invalid_layout("XMP iloc offset is below its base offset"))?;
        write_uint(&mut new_body, pos, layout.offset_size, offset)?;
    } else if let Some(pos) = item.base_offset_pos {
        write_uint(&mut new_body, pos, layout.base_offset_size, new_offset)?;
    } else if new_offset != 0 {
        return Err(invalid_layout("XMP iloc has no writable offset field"));
    }
    if let Some(pos) = extent.length_pos {
        write_uint(&mut new_body, pos, layout.length_size, new_length)?;
    } else if new_length != 0 {
        return Err(invalid_layout("XMP iloc has no writable length field"));
    }
    box_with_body(iloc.kind, &new_body)
}

fn xmp_extent_is_shared(
    layout: &IlocLayout,
    item_id: u32,
    extent_start: u64,
    extent_length: u64,
) -> bool {
    let Some(extent_end) = extent_start.checked_add(extent_length) else {
        return true;
    };
    layout.items.iter().any(|item| {
        item.item_id != item_id
            && item.construction_method == 0
            && item.extents.iter().any(|extent| {
                let Some(start) = item.base_offset.checked_add(extent.offset) else {
                    return true;
                };
                let Some(end) = start.checked_add(extent.length) else {
                    return true;
                };
                start < extent_end && extent_start < end
            })
    })
}

fn extent_is_within_mdat_payload(
    bytes: &[u8],
    extent_start: usize,
    extent_length: usize,
) -> Result<bool, HeifError> {
    let extent_end = extent_start
        .checked_add(extent_length)
        .ok_or_else(|| invalid_layout("existing XMP extent overflows"))?;
    Ok(scan_raw_boxes(bytes, 0, bytes.len())?
        .into_iter()
        .filter(|atom| atom.kind == *b"mdat")
        .any(|atom| atom.body_start() <= extent_start && extent_end <= atom.end()))
}

fn make_new_iloc_entry(
    layout: &IlocLayout,
    item_id: u32,
    data_offset: u64,
    length: u64,
) -> Result<Vec<u8>, HeifError> {
    let id_width = if layout.version == 2 { 4 } else { 2 };
    let mut entry = Vec::new();
    write_uint_vec(&mut entry, id_width, u64::from(item_id))?;
    if layout.version != 0 {
        write_uint_vec(&mut entry, 2, 0)?;
    }
    write_uint_vec(&mut entry, 2, 0)?;
    // The item's file position must land in whichever field can hold it: the
    // extent offset when present, otherwise the base offset. Writing it to a
    // zero-width field would silently drop it and point the item at offset 0.
    let (base_value, offset_value) = if layout.offset_size != 0 {
        (0, data_offset)
    } else if layout.base_offset_size != 0 {
        (data_offset, 0)
    } else {
        return Err(invalid_layout("XMP iloc cannot represent a file offset"));
    };
    write_uint_vec(&mut entry, layout.base_offset_size, base_value)?;
    write_uint_vec(&mut entry, 2, 1)?;
    if layout.version != 0 {
        write_uint_vec(&mut entry, layout.index_size, 0)?;
    }
    write_uint_vec(&mut entry, layout.offset_size, offset_value)?;
    write_uint_vec(&mut entry, layout.length_size, length)?;
    Ok(entry)
}

fn write_uint_vec(bytes: &mut Vec<u8>, width: u8, value: u64) -> Result<(), HeifError> {
    match width {
        0 => {
            if value != 0 {
                return Err(invalid_layout(
                    "non-zero value cannot use a zero-width field",
                ));
            }
        }
        2 => bytes.extend_from_slice(
            &u16::try_from(value)
                .map_err(|_| HeifError::ValueOverflow {
                    field: "16-bit HEIC field",
                })?
                .to_be_bytes(),
        ),
        4 => bytes.extend_from_slice(
            &u32::try_from(value)
                .map_err(|_| HeifError::ValueOverflow {
                    field: "32-bit HEIC field",
                })?
                .to_be_bytes(),
        ),
        8 => bytes.extend_from_slice(&value.to_be_bytes()),
        _ => return Err(invalid_layout("unsupported HEIC integer width")),
    }
    Ok(())
}

/// Whether an extent's absolute start sits at or past the boundary where the
/// appended metadata begins, so it must move with the shift. An offset that
/// overflows `u64` is treated as past the boundary; the shift below uses
/// `checked_add` and rejects an offset it cannot represent rather than writing
/// a wrapped value.
fn extent_at_or_past_boundary(base_offset: u64, extent_offset: u64, boundary: u64) -> bool {
    extent_offset
        .checked_add(base_offset)
        .is_none_or(|absolute| absolute >= boundary)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the iloc body range before the validated field positions are updated."
)]
fn append_iloc_entry(
    bytes: &[u8],
    iloc: RawBox,
    layout: &IlocLayout,
    item_id: u32,
    data_offset: u64,
    length: u64,
    delta: u64,
    shift_boundary: u64,
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iloc.body_start()..iloc.end()];
    let mut new_body = body.to_vec();
    for item in &layout.items {
        if item.construction_method != 0 {
            continue;
        }
        let shifted = item
            .extents
            .iter()
            .map(|extent| {
                extent_at_or_past_boundary(item.base_offset, extent.offset, shift_boundary)
            })
            .collect::<Vec<_>>();
        if shifted.iter().any(|shift| *shift) {
            if shifted.iter().all(|shift| *shift) {
                if let Some(pos) = item.base_offset_pos {
                    write_uint(
                        &mut new_body,
                        pos,
                        layout.base_offset_size,
                        item.base_offset
                            .checked_add(delta)
                            .ok_or_else(|| invalid_layout("HEIC iloc base offset overflows"))?,
                    )?;
                } else if item
                    .extents
                    .iter()
                    .any(|extent| extent.offset_pos.is_none())
                {
                    return Err(invalid_layout("HEIC iloc offset cannot be shifted safely"));
                } else {
                    for extent in &item.extents {
                        let shifted_offset = extent
                            .offset
                            .checked_add(delta)
                            .ok_or_else(|| invalid_layout("HEIC iloc offset overflows"))?;
                        if let Some(pos) = extent.offset_pos {
                            write_uint(&mut new_body, pos, layout.offset_size, shifted_offset)?;
                        }
                    }
                }
            } else if item
                .extents
                .iter()
                .any(|extent| extent.offset_pos.is_none())
            {
                return Err(invalid_layout("HEIC iloc extents shift inconsistently"));
            } else {
                for extent in &item.extents {
                    if extent_at_or_past_boundary(item.base_offset, extent.offset, shift_boundary)
                        && let Some(pos) = extent.offset_pos
                    {
                        let shifted_offset = extent
                            .offset
                            .checked_add(delta)
                            .ok_or_else(|| invalid_layout("HEIC iloc offset overflows"))?;
                        write_uint(&mut new_body, pos, layout.offset_size, shifted_offset)?;
                    }
                }
            }
        }
    }
    let mut count_cursor = layout.count_pos;
    let count_size = u8::try_from(layout.count_size)
        .map_err(|_| invalid_layout("iloc count width is too large"))?;
    let count = read_uint(&new_body, &mut count_cursor, count_size)?;
    write_uint(
        &mut new_body,
        layout.count_pos,
        count_size,
        count
            .checked_add(1)
            .ok_or_else(|| invalid_layout("iloc item count overflows"))?,
    )?;
    new_body.extend_from_slice(&make_new_iloc_entry(layout, item_id, data_offset, length)?);
    box_with_body(iloc.kind, &new_body)
}

/// Rewrite an XMP packet without decoding or re-encoding the surrounding
/// HEIC item graph. Existing packets are replaced in place when possible;
/// otherwise only the XMP iloc entry is repointed to an appended mdat. Files
/// without XMP receive one new item, one new location, and a `cdsc` reference
/// to the primary image, synthesising an `iref` box when the file has none.
/// Every unrelated box and payload is copied unchanged.
///
/// Insertion is limited to files whose top-level boxes have no unhandled
/// absolute offsets. Existing-XMP updates do not grow `meta` and can append
/// after otherwise unsupported top-level boxes. The complete rewritten file
/// is materialised before it is written to `writer`.
#[allow(
    clippy::indexing_slicing,
    reason = "All rewrite ranges come from validated box boundaries and checked XMP extent arithmetic."
)]
pub(crate) fn rewrite_xmp<W: Write>(
    input: &[u8],
    xmp: &[u8],
    mut writer: W,
) -> Result<(), HeifError> {
    if !is_heif_content(input) {
        return Err(invalid_layout("input is not a HEIF-family file"));
    }
    let (meta, iinf, iloc, iref, primary_item_id, prefix_size) = find_meta_layout(input)?;
    let iloc_layout = parse_iloc(input, iloc)?;
    let (
        existing,
        xmp_item_id,
        max_item_id,
        iinf_version,
        iinf_count_pos,
        iinf_count_size,
        iinf_item_ids,
    ) = locate_xmp(input, iinf, iloc, iref, primary_item_id)?;

    reject_meta_overlapping_items(&iloc_layout, meta)?;
    for item in &iloc_layout.items {
        if item.construction_method == 0 {
            let _ = resolve_item_extents(input, item)?;
        }
    }

    if let Some(location) = existing {
        let item = iloc_layout
            .items
            .iter()
            .find(|item| item.item_id == location.item_id)
            .ok_or_else(|| invalid_layout("XMP item has no iloc entry"))?;
        if item.extents.len() != 1 {
            return Err(invalid_layout("XMP item uses multiple iloc extents"));
        }
        let end = location
            .extent_start
            .checked_add(location.extent_length)
            .ok_or_else(|| invalid_layout("existing XMP extent overflows"))?;
        if end > input.len() {
            return Err(invalid_layout("existing XMP extent is outside the file"));
        }
        let shared = xmp_extent_is_shared(
            &iloc_layout,
            location.item_id,
            u64::try_from(location.extent_start)
                .map_err(|_| invalid_layout("existing XMP offset overflows"))?,
            u64::try_from(location.extent_length)
                .map_err(|_| invalid_layout("existing XMP length overflows"))?,
        );
        let in_mdat =
            extent_is_within_mdat_payload(input, location.extent_start, location.extent_length)?;
        if xmp.len() <= location.extent_length && !shared && in_mdat {
            let mut output = input.to_vec();
            output[location.extent_start..location.extent_start + xmp.len()].copy_from_slice(xmp);
            output[location.extent_start + xmp.len()..end].fill(b' ');
            writer.write_all(&output)?;
            return Ok(());
        }
        let new_data_offset = u64::try_from(input.len())
            .ok()
            .and_then(|len| len.checked_add(8))
            .ok_or_else(|| invalid_layout("new XMP offset overflows"))?;
        let new_iloc = rewrite_existing_iloc(
            input,
            iloc,
            &iloc_layout,
            location.item_id,
            new_data_offset,
            u64::try_from(xmp.len()).map_err(|_| invalid_layout("XMP packet is too large"))?,
        )?;
        let mut output = Vec::new();
        output.extend_from_slice(&input[..iloc.start]);
        output.extend_from_slice(&new_iloc);
        output.extend_from_slice(&input[iloc.end()..]);
        output.extend_from_slice(&box_with_body(*b"mdat", xmp)?);
        writer.write_all(&output)?;
        return Ok(());
    }

    if xmp_item_id.is_some() {
        return Err(invalid_layout(
            "existing XMP item uses an unsupported iloc layout",
        ));
    }
    ensure_insertion_layout(input)?;
    if !parse_iinf(input, iinf)?.tone_map_item_ids.is_empty() {
        return Err(invalid_layout(
            "HEIC XMP insertion with a tone-mapped image is unsupported",
        ));
    }
    let described_item_ids = [primary_item_id];
    let new_item_id = max_item_id
        .checked_add(1)
        .ok_or_else(|| invalid_layout("no free HEIC item id remains"))?;
    if iinf_version == 0 && new_item_id > u16::MAX as u32 && iloc_layout.version != 2 {
        return Err(invalid_layout(
            "new XMP item id does not fit this HEIC item map",
        ));
    }
    let new_iinf = append_iinf_entry(input, iinf, iinf_count_pos, iinf_count_size, new_item_id)?;
    let (new_iref, old_iref_size) = match iref {
        Some(iref) => (
            append_cdsc_reference(
                input,
                iref,
                new_item_id,
                &described_item_ids,
                &iinf_item_ids,
            )?,
            iref.size,
        ),
        None => (synthesise_cdsc_iref(new_item_id, &described_item_ids)?, 0),
    };
    let old_meta_len = meta.size;
    let xmp_length =
        u64::try_from(xmp.len()).map_err(|_| invalid_layout("XMP packet is too large"))?;
    let placeholder_iloc = append_iloc_entry(
        input,
        iloc,
        &iloc_layout,
        new_item_id,
        u64::try_from(input.len())
            .ok()
            .and_then(|len| len.checked_add(8))
            .ok_or_else(|| invalid_layout("new XMP offset overflows"))?,
        xmp_length,
        0,
        u64::try_from(meta.end()).map_err(|_| invalid_layout("HEIC meta offset overflows"))?,
    )?;
    let new_meta_len = old_meta_len
        .checked_add(new_iinf.len())
        .and_then(|len| len.checked_sub(iinf.size))
        .and_then(|len| len.checked_add(new_iref.len()))
        .and_then(|len| len.checked_sub(old_iref_size))
        .and_then(|len| len.checked_add(placeholder_iloc.len()))
        .and_then(|len| len.checked_sub(iloc.size))
        .ok_or_else(|| invalid_layout("new HEIC meta size overflows"))?;
    let delta = u64::try_from(new_meta_len)
        .ok()
        .and_then(|new_len| {
            u64::try_from(old_meta_len)
                .ok()
                .and_then(|old_len| new_len.checked_sub(old_len))
        })
        .ok_or_else(|| invalid_layout("new HEIC meta size is invalid"))?;
    let data_offset = u64::try_from(input.len())
        .ok()
        .and_then(|len| len.checked_add(delta))
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| invalid_layout("new XMP offset overflows"))?;
    let new_iloc = append_iloc_entry(
        input,
        iloc,
        &iloc_layout,
        new_item_id,
        data_offset,
        xmp_length,
        delta,
        u64::try_from(meta.end()).map_err(|_| invalid_layout("HEIC meta offset overflows"))?,
    )?;
    let new_meta_len = old_meta_len
        .checked_add(new_iinf.len())
        .and_then(|len| len.checked_sub(iinf.size))
        .and_then(|len| len.checked_add(new_iref.len()))
        .and_then(|len| len.checked_sub(old_iref_size))
        .and_then(|len| len.checked_add(new_iloc.len()))
        .and_then(|len| len.checked_sub(iloc.size))
        .ok_or_else(|| invalid_layout("new HEIC meta size overflows"))?;
    let mut meta_bytes = Vec::with_capacity(new_meta_len);
    meta_bytes.extend_from_slice(&input[meta.start..meta.body_start() + prefix_size]);
    let children = scan_raw_boxes(input, meta.body_start() + prefix_size, meta.end())?;
    for child in children {
        if child.start == iinf.start {
            meta_bytes.extend_from_slice(&new_iinf);
            if iref.is_none() {
                meta_bytes.extend_from_slice(&new_iref);
            }
        } else if iref.is_some_and(|existing| existing.start == child.start) {
            meta_bytes.extend_from_slice(&new_iref);
        } else if child.start == iloc.start {
            meta_bytes.extend_from_slice(&new_iloc);
        } else {
            meta_bytes.extend_from_slice(&input[child.start..child.end()]);
        }
    }
    patch_box_size(&mut meta_bytes)?;
    if meta_bytes.len() != new_meta_len {
        return Err(invalid_layout("rewritten HEIC meta size is inconsistent"));
    }
    let mut output = Vec::new();
    output.extend_from_slice(&input[..meta.start]);
    output.extend_from_slice(&meta_bytes);
    output.extend_from_slice(&input[meta.end()..]);
    output.extend_from_slice(&box_with_body(*b"mdat", xmp)?);
    writer.write_all(&output)?;
    Ok(())
}

/// Pull the iinf + iloc out of a `meta` box body and resolve the XMP extent
/// against the original file bytes. Walks the meta sub-boxes by header only
/// and decodes only `iinf` and `iloc`, so a hostile sub-atom (e.g. a nested
/// `dfLa`) can't reach the unbounded-allocation parsers either.
///
/// Returns `Err(MetaSubBoxDecode)` if iinf or iloc decode fails — the caller
/// can then mark the asset as needing metadata-rewrite. Returns `Ok(None)`
/// for legitimately-absent XMP (no iinf, no XMP item, etc).
fn extract_xmp_from_meta(
    file_bytes: &[u8],
    meta_body: &[u8],
) -> Result<Option<Vec<u8>>, HeifError> {
    let mut cursor: &[u8] = meta_body;

    // Two on-the-wire formats for `meta`:
    //   - ISO/IEC 14496-12: 4-byte version+flags, then sub-boxes (first is hdlr).
    //   - Apple QuickTime: starts with hdlr directly, no version+flags.
    // Detect by peeking offset 4..8 for "hdlr"; same heuristic as
    // mp4_atom::Meta::decode_body.
    if cursor.len() >= 8 && cursor.get(4..8) != Some(b"hdlr".as_slice()) {
        if cursor.len() < 4 {
            return Ok(None);
        }
        cursor.advance(4);
    }

    // Skip hdlr; we don't need its contents.
    let Some(hdlr) = Header::decode_maybe(&mut cursor).ok().flatten() else {
        return Ok(None);
    };
    let hdlr_size = hdlr.size.unwrap_or(cursor.remaining());
    if hdlr_size > cursor.remaining() {
        return Ok(None);
    }
    cursor.advance(hdlr_size);

    let mut iinf: Option<Iinf> = None;
    let mut iloc: Option<Iloc> = None;
    while cursor.has_remaining() {
        let Some(h) = Header::decode_maybe(&mut cursor).ok().flatten() else {
            return Ok(None);
        };
        let sz = h.size.unwrap_or(cursor.remaining());
        if sz > cursor.remaining() {
            return Ok(None);
        }
        let Some(body) = cursor.get(..sz) else {
            return Ok(None);
        };
        // Defense-in-depth cap on the bytes handed to the typed decoders:
        // HEIC iinf/iloc are KB-scale in real-world files. The original
        // `Vec::with_capacity(<attacker count>)` shape that bit
        // `parse_vorbis_comment` is fixed upstream (kixelated/mp4-atom#157,
        // closing #154); this guard remains so the same pattern surfacing
        // later in `ItemInfoEntry::decode_body` or `ItemLocation::decode_body`
        // shorts the OOM before the decoder ever sees the body.
        const MAX_META_SUBBOX_BYTES: usize = 8 * 1024 * 1024;
        if body.len() <= MAX_META_SUBBOX_BYTES {
            if h.kind == FourCC::new(b"iinf") {
                iinf = Some(
                    decode_iinf(body).map_err(|source| HeifError::MetaSubBoxDecode {
                        kind: FourCC::new(b"iinf"),
                        source,
                    })?,
                );
            } else if h.kind == FourCC::new(b"iloc") {
                iloc = Some(Iloc::decode_body(&mut &body[..]).map_err(|source| {
                    HeifError::MetaSubBoxDecode {
                        kind: FourCC::new(b"iloc"),
                        source,
                    }
                })?);
            }
        }
        cursor.advance(sz);
    }

    let (Some(iinf), Some(iloc)) = (iinf, iloc) else {
        return Ok(None);
    };
    let xmp_item_ids: Vec<u32> = iinf
        .item_infos
        .iter()
        .filter(|e| {
            e.item_type == Some(FourCC::new(b"mime"))
                && e.content_type.as_deref() == Some("application/rdf+xml")
        })
        .map(|e| e.item_id)
        .collect();
    if xmp_item_ids.is_empty() {
        return Ok(None);
    }
    let xmp_item_id = match find_meta_layout(file_bytes) {
        Ok((_, _, _, iref, primary_item_id, _)) => {
            match select_xmp_item_id(file_bytes, iref, primary_item_id, &xmp_item_ids)? {
                Some(item_id) => item_id,
                None => return Ok(None),
            }
        }
        // Without a resolvable item graph there is no association to read, so a
        // lone packet still answers for the primary image.
        Err(err) => match xmp_item_ids.as_slice() {
            [item_id] => *item_id,
            _ => return Err(err),
        },
    };
    let Some(loc) = iloc
        .item_locations
        .iter()
        .find(|l| l.item_id == xmp_item_id)
    else {
        return Ok(None);
    };
    if loc.data_reference_index != 0 {
        return Err(invalid_layout("XMP item uses an external data reference"));
    }
    if loc.construction_method != 0 {
        return Ok(None);
    }
    let Some(extent) = loc.extents.first() else {
        return Ok(None);
    };
    #[allow(
        clippy::cast_possible_truncation,
        reason = "HEIC file byte offsets/lengths fit in usize on 64-bit; kei targets 64-bit platforms"
    )]
    let start = loc.base_offset.saturating_add(extent.offset) as usize;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "HEIC extent length fits in usize on 64-bit"
    )]
    let Some(end) = start.checked_add(extent.length as usize) else {
        return Ok(None);
    };
    Ok(file_bytes.get(start..end).map(<[u8]>::to_vec))
}

/// Decode `iinf` while shielding kei from known mp4-atom panic paths.
///
/// `mp4-atom` 0.11.0 still has an `unimplemented!` branch for version-1
/// `infe` entries. `iinf` comes from user-controlled HEIF bytes, so kei
/// pre-screens that unsupported shape and converts it to a normal decode
/// error until upstream returns `Err` itself:
/// <https://github.com/kixelated/mp4-atom/issues/164>.
fn decode_iinf(body: &[u8]) -> Result<Iinf, mp4_atom::Error> {
    if contains_unsupported_infe_v1(body) {
        return Err(mp4_atom::Error::Unsupported("infe version 1 extensions"));
    }
    Iinf::decode_body(&mut &body[..])
}

fn contains_unsupported_infe_v1(mut body: &[u8]) -> bool {
    let Some(version) = body.first().copied() else {
        return false;
    };
    let Some(mut entries) = iinf_entry_count(body, version) else {
        return false;
    };

    let count_len = if version == 0 { 2 } else { 4 };
    let Some(entries_body) = body.get(4 + count_len..) else {
        return false;
    };
    body = entries_body;

    while entries > 0 {
        let before = body.len();
        let Some(header) = Header::decode_maybe(&mut body).ok().flatten() else {
            return false;
        };
        let header_len = before - body.len();
        let entry_body_len = header.size.unwrap_or(body.len());
        if entry_body_len > body.len() {
            return false;
        }

        // `mp4-atom` decodes every child declared by the `iinf` entry count
        // as `ItemInfoEntry` without checking the child FourCC first, so a
        // malformed child kind can still reach the version-1 `infe` panic.
        if body.first() == Some(&1) {
            return true;
        }

        let Some(rest) = body.get(entry_body_len..) else {
            return false;
        };
        body = rest;
        entries -= 1;

        if header_len == 0 {
            return false;
        }
    }

    false
}

fn iinf_entry_count(body: &[u8], version: u8) -> Option<u32> {
    match version {
        0 => body
            .get(4..6)
            .and_then(|count| count.try_into().ok())
            .map(|count| u16::from_be_bytes(count) as u32),
        1 => body
            .get(4..8)
            .and_then(|count| count.try_into().ok())
            .map(u32::from_be_bytes),
        _ => None,
    }
}

/// Fuzz-only driver that exercises [`rewrite_xmp`] with a fixed XMP marker and
/// asserts the writer's safety contract, rather than merely that it does not
/// crash. A rejected rewrite must emit nothing. An accepted rewrite must keep
/// the container HEIF, make the written packet readable again, preserve every
/// item payload it does not own, and preserve opaque `meta` sub-boxes
/// byte-for-byte.
/// Compiled only for the fuzz harness; absent from production builds.
#[cfg(feature = "__fuzz_internals")]
pub(crate) fn fuzz_rewrite_xmp_preserves(input: &[u8]) {
    const MARKER: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
    let mut output = Vec::new();
    match rewrite_xmp(input, MARKER, &mut output) {
        Err(_) => assert!(
            output.is_empty(),
            "a rejected HEIC rewrite must not emit bytes"
        ),
        Ok(()) => {
            assert!(
                is_heif_content(&output),
                "writer output must remain HEIF content"
            );
            assert_eq!(
                fuzz_extract_xmp(&output)
                    .as_deref()
                    .map(<[u8]>::trim_ascii_end),
                Some(MARKER),
                "the written XMP packet must be locatable and round-trip"
            );
            let validation = validate_rewrite_preserves_non_xmp_items(input, &output);
            assert!(
                validation.is_ok(),
                "accepted rewrite must preserve non-XMP items and opaque meta sub-boxes: {validation:?}"
            );
        }
    }
}

/// Extract the XMP packet through the writer-side parser, for the fuzz safety
/// check. Independent of the mp4-atom read path so item ids above `u16` and
/// iloc version 2 are covered.
#[cfg(feature = "__fuzz_internals")]
fn fuzz_extract_xmp(bytes: &[u8]) -> Option<Vec<u8>> {
    let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(bytes).ok()?;
    let (location, _, _, _, _, _, _) = locate_xmp(bytes, iinf, iloc, iref, primary_item_id).ok()?;
    let location = location?;
    let end = location.extent_start.checked_add(location.extent_length)?;
    bytes.get(location.extent_start..end).map(<[u8]>::to_vec)
}

/// Legacy typed-atom XMP writer retained for parser fixture tests.
///
/// The HEIC container is ISO-BMFF, a sequence of top-level atoms. XMP lives
/// inside the `meta` atom as an item with `item_type = "mime"` and
/// `content_type = "application/rdf+xml"`. This helper appends the XMP bytes as a new
/// trailing `mdat` (construction_method 0, file-absolute offsets), so the
/// encoded image bytes in the original `mdat` stay byte-for-byte identical
/// even after `meta` grows.
#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    reason = "meta_idx comes from .position() over atoms; new_mdat_idx is atoms.len() - 1 \
              after a push; new_positions is built from the same atoms vec; all indexing \
              here is in-bounds by construction"
)]
pub(crate) fn insert_xmp<W: Write>(
    input: &[u8],
    xmp: &[u8],
    mut writer: W,
) -> Result<(), HeifError> {
    // Record each top-level atom along with its original byte offset in the
    // input so we can rewrite file-absolute iloc entries correctly — the
    // existing iloc offsets point into the original mdat, and those offsets
    // must be updated so that after re-serialization they still land on the
    // same image bytes even though the meta box grew.
    let total = input.len() as u64;
    let mut cursor: &[u8] = input;
    let mut atoms: Vec<Any> = Vec::new();
    let mut original_offsets: Vec<u64> = Vec::new();
    while !cursor.is_empty() {
        let offset = total - cursor.len() as u64;
        match Any::decode_maybe(&mut cursor).map_err(|source| HeifError::Decode {
            offset,
            total,
            source,
        })? {
            Some(a) => {
                atoms.push(a);
                original_offsets.push(offset);
            }
            None => {
                return Err(HeifError::UnparsableTail { offset, total });
            }
        }
    }

    let meta_idx =
        atoms
            .iter()
            .position(|a| matches!(a, Any::Meta(_)))
            .ok_or(HeifError::MissingMeta {
                input_len: input.len(),
            })?;

    // Step 1: locate and drop the trailing mdat that a prior kei write
    // appended (if any) so we don't accumulate stale XMP payloads on
    // re-sync. We identify it by: (a) the existing XMP iloc entry's
    // extent range, (b) it sitting past the image-data mdat, (c) no
    // other iloc entry pointing into it.
    let stale_mdat_idx = locate_stale_kei_mdat(&atoms, &original_offsets, meta_idx);

    // Step 2: remove the XMP entries from iinf and iloc.
    if let Any::Meta(meta) = &mut atoms[meta_idx] {
        let removed_ids = remove_existing_xmp_items(meta);
        if let Some(iloc) = meta.get_mut::<Iloc>() {
            iloc.item_locations
                .retain(|loc| !removed_ids.contains(&loc.item_id));
        }
    }

    // Step 3: drop the stale mdat atom (indexes shift, recompute meta_idx
    // relative to the surviving atoms).
    let meta_idx = if let Some(stale) = stale_mdat_idx {
        atoms.remove(stale);
        original_offsets.remove(stale);
        if stale < meta_idx {
            meta_idx - 1
        } else {
            meta_idx
        }
    } else {
        meta_idx
    };

    // Step 4: reserve the iinf + iloc entries for the new XMP. The iloc
    // offset is the file offset our appended mdat's DATA will have in the
    // re-serialized output. mp4-atom encodes Iloc at fixed width regardless
    // of offset value, so we can append the mdat atom first, compute the
    // resulting running offsets, then populate the iloc offset.
    let new_item_id = {
        #[allow(
            clippy::unreachable,
            reason = "meta_idx comes from matches!(a, Any::Meta(_)) above"
        )]
        let Any::Meta(meta) = &atoms[meta_idx] else {
            unreachable!()
        };
        next_free_item_id(meta)
    };

    atoms.push(Any::Mdat(Mdat { data: xmp.to_vec() }));
    let new_mdat_idx = atoms.len() - 1;

    // Insert placeholder iloc entry (offset=0) and iinf entry so that running
    // offsets reflect the final meta size.
    {
        #[allow(
            clippy::unreachable,
            reason = "meta_idx comes from matches!(a, Any::Meta(_)) above"
        )]
        let Any::Meta(meta) = &mut atoms[meta_idx] else {
            unreachable!()
        };
        push_iinf_entry(
            meta,
            ItemInfoEntry {
                item_id: new_item_id,
                item_protection_index: 0,
                item_type: Some(FourCC::new(b"mime")),
                item_name: String::new(),
                content_type: Some("application/rdf+xml".to_string()),
                content_encoding: Some(String::new()),
                item_uri_type: None,
                item_not_in_presentation: false,
            },
        );
        push_iloc_entry(
            meta,
            ItemLocation {
                item_id: new_item_id,
                construction_method: 0,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![ItemLocationExtent {
                    item_reference_index: 0,
                    offset: 0,
                    length: xmp.len() as u64,
                }],
            },
        );
    }

    // Step 5: remap pre-existing file-offset iloc entries and fill in the
    // offset for the XMP iloc entry we just pushed.
    let new_positions = running_offsets(&atoms);
    let xmp_file_offset = new_positions[new_mdat_idx] + header_size_of(&atoms[new_mdat_idx]);

    let file_offset_map: Vec<(u64, u64, u64)> = atoms
        .iter()
        .enumerate()
        .take(new_mdat_idx) // skip the mdat we just added; it has no matching original
        .filter_map(|(idx, _a)| {
            let orig = *original_offsets.get(idx)?;
            // Use the original atom's actual extent, not encoded_size(a).
            // Meta::encode_body always writes ISO format (with 4-byte
            // version+flags) even when the input was Apple QuickTime
            // (without version+flags). encoded_size would report the
            // re-encoded ISO size, making this range 4 bytes wider than
            // the original atom — iloc entries in that overshoot region
            // are then captured by the wrong range.
            let orig_end = original_offsets.get(idx + 1).copied().unwrap_or(total);
            Some((orig, orig_end, new_positions[idx]))
        })
        .collect();

    if let Any::Meta(meta) = &mut atoms[meta_idx]
        && let Some(iloc) = meta.get_mut::<Iloc>()
    {
        remap_file_offsets(iloc, &file_offset_map);
        // Now fill in the XMP entry's offset (last iloc entry).
        if let Some(xmp_loc) = iloc
            .item_locations
            .iter_mut()
            .find(|l| l.item_id == new_item_id)
            && let Some(extent) = xmp_loc.extents.first_mut()
        {
            extent.offset = xmp_file_offset;
        }
    }

    // mp4-atom's Encode requires BufMut (bytes), not Write; a reusable
    // per-atom Vec caps in-memory output at one atom (the image mdat
    // is typically the largest) rather than the full serialized file.
    let mut atom_buf: Vec<u8> = Vec::new();
    for atom in &atoms {
        atom_buf.clear();
        let kind = atom.kind();
        atom.encode(&mut atom_buf)
            .map_err(|source| HeifError::Encode { kind, source })?;
        writer.write_all(&atom_buf)?;
    }
    Ok(())
}

/// Walk existing iinf/iloc to find any previously-kei-appended XMP mdat.
/// Criteria: an iinf entry flagged as `mime` + `application/rdf+xml`, its
/// iloc entry references a range that lies entirely within a single trailing
/// mdat atom, and no other iloc entry references into that atom.
#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    reason = "meta_idx is caller-validated and idx comes from atoms.iter().enumerate() \
              with original_offsets built 1:1 alongside atoms in insert_xmp"
)]
fn locate_stale_kei_mdat(
    atoms: &[Any],
    original_offsets: &[u64],
    meta_idx: usize,
) -> Option<usize> {
    let meta = if let Any::Meta(m) = &atoms[meta_idx] {
        m
    } else {
        return None;
    };
    let iinf = meta.get::<Iinf>()?;
    let iloc = meta.get::<Iloc>()?;

    let xmp_item_ids: Vec<u32> = iinf
        .item_infos
        .iter()
        .filter(|e| {
            e.item_type == Some(FourCC::new(b"mime"))
                && e.content_type.as_deref() == Some("application/rdf+xml")
        })
        .map(|e| e.item_id)
        .collect();
    if xmp_item_ids.is_empty() {
        return None;
    }

    for item_id in &xmp_item_ids {
        let Some(loc) = iloc.item_locations.iter().find(|l| l.item_id == *item_id) else {
            continue;
        };
        if loc.construction_method != 0 {
            continue;
        }
        let Some(extent) = loc.extents.first() else {
            continue;
        };
        let abs_start = loc.base_offset.saturating_add(extent.offset);
        let abs_end = abs_start.saturating_add(extent.length);

        for (idx, atom) in atoms.iter().enumerate() {
            if !matches!(atom, Any::Mdat(_)) {
                continue;
            }
            let atom_start = original_offsets[idx];
            let atom_end = original_offsets
                .get(idx + 1)
                .copied()
                .unwrap_or_else(|| atom_start + encoded_size(atom));
            if abs_start < atom_start || abs_end > atom_end {
                continue;
            }
            let other_refs = iloc.item_locations.iter().any(|other| {
                if other.item_id == *item_id || other.construction_method != 0 {
                    return false;
                }
                other.extents.iter().any(|e| {
                    let o_start = other.base_offset.saturating_add(e.offset);
                    o_start >= atom_start && o_start < atom_end
                })
            });
            if !other_refs {
                return Some(idx);
            }
        }
    }
    None
}

/// Byte size of an atom's box header (the length field + 4-byte kind code).
/// mp4-atom always emits a 32-bit-length header for atoms that fit — large
/// mdats (>4GB) would use a 16-byte header, but kei isn't going to hit that.
#[cfg(test)]
fn header_size_of(_atom: &Any) -> u64 {
    8
}

/// Return a vector where entry `i` is the byte offset at which atom `i` will
/// sit in the re-serialized output (i.e. the running sum of preceding atom
/// sizes).
#[cfg(test)]
fn running_offsets(atoms: &[Any]) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(atoms.len());
    let mut running = 0u64;
    for atom in atoms {
        offsets.push(running);
        running += encoded_size(atom);
    }
    offsets
}

/// Translate each construction_method-0 iloc offset from "original file
/// offset" to "new file offset", using the per-atom old_start/old_end/new_start
/// table. An offset that falls within `[old_start, old_end)` is rebased onto
/// `new_start` with the same intra-atom position.
#[cfg(test)]
fn remap_file_offsets(iloc: &mut Iloc, ranges: &[(u64, u64, u64)]) {
    for loc in &mut iloc.item_locations {
        if loc.construction_method != 0 {
            continue;
        }
        // Some encoders put the whole file offset in `base_offset` and leave
        // extent offsets at 0; others leave base_offset 0 and put absolute
        // offsets on each extent. Handle both by remapping either piece that
        // lands in a known original-atom range.
        loc.base_offset = remap_point(loc.base_offset, ranges).unwrap_or(loc.base_offset);
        for extent in &mut loc.extents {
            let absolute = loc.base_offset.saturating_add(extent.offset);
            if let Some(new_abs) = remap_point(absolute, ranges) {
                extent.offset = new_abs.saturating_sub(loc.base_offset);
            }
        }
    }
}

#[cfg(test)]
fn remap_point(file_offset: u64, ranges: &[(u64, u64, u64)]) -> Option<u64> {
    for &(old_start, old_end, new_start) in ranges {
        if file_offset >= old_start && file_offset < old_end {
            return Some(new_start + (file_offset - old_start));
        }
    }
    None
}

#[cfg(test)]
fn encoded_size(atom: &Any) -> u64 {
    let mut sink = Vec::new();
    if let Err(e) = atom.encode(&mut sink) {
        tracing::warn!(
            error = %e,
            "encoded_size: atom re-encode failed; size estimate may be wrong, \
             downstream offset remap will skip this atom"
        );
    }
    sink.len() as u64
}

#[cfg(test)]
fn remove_existing_xmp_items(meta: &mut Meta) -> Vec<u32> {
    let mut removed = Vec::new();
    if let Some(iinf) = meta.get_mut::<Iinf>() {
        iinf.item_infos.retain(|entry| {
            let is_xmp = entry.item_type == Some(FourCC::new(b"mime"))
                && entry.content_type.as_deref() == Some("application/rdf+xml");
            if is_xmp {
                removed.push(entry.item_id);
                false
            } else {
                true
            }
        });
    }
    removed
}

#[cfg(test)]
fn next_free_item_id(meta: &Meta) -> u32 {
    meta.get::<Iinf>()
        .map(|iinf| {
            iinf.item_infos
                .iter()
                .map(|e| e.item_id)
                .max()
                .map(|m| m + 1)
                .unwrap_or(1)
        })
        .unwrap_or(1)
}

#[cfg(test)]
fn push_iinf_entry(meta: &mut Meta, entry: ItemInfoEntry) {
    match meta.get_mut::<Iinf>() {
        Some(iinf) => iinf.item_infos.push(entry),
        None => meta.push(Iinf {
            item_infos: vec![entry],
        }),
    }
}

#[cfg(test)]
fn push_iloc_entry(meta: &mut Meta, loc: ItemLocation) {
    match meta.get_mut::<Iloc>() {
        Some(iloc) => iloc.item_locations.push(loc),
        None => meta.push(Iloc {
            item_locations: vec![loc],
        }),
    }
}

#[cfg(test)]
pub(crate) use tests::apple_multi_xmp_heic;

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(body.len() + 8).expect("test atom size");
        let mut bytes = Vec::with_capacity(size as usize);
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    fn exif_infe(item_id: u32, version: u8) -> Vec<u8> {
        let mut infe_body = vec![version, 0, 0, 0];
        if version == 2 {
            infe_body.extend_from_slice(
                &u16::try_from(item_id)
                    .expect("version 2 item id")
                    .to_be_bytes(),
            );
        } else {
            infe_body.extend_from_slice(&item_id.to_be_bytes());
        }
        infe_body.extend_from_slice(&0_u16.to_be_bytes());
        infe_body.extend_from_slice(b"Exif");
        infe_body.push(0);
        atom(b"infe", &infe_body)
    }

    fn exif_iinf() -> Vec<u8> {
        exif_iinf_entries(&[exif_infe(1, 2)])
    }

    fn exif_iinf_entries(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut iinf_body = vec![0, 0, 0, 0];
        iinf_body.extend_from_slice(
            &u16::try_from(entries.len())
                .expect("iinf entry count")
                .to_be_bytes(),
        );
        for entry in entries {
            iinf_body.extend_from_slice(entry);
        }
        atom(b"iinf", &iinf_body)
    }

    fn exif_iloc(extent_offset: u32, extent_length: u32, extent_count: u16) -> Vec<u8> {
        exif_iloc_options(extent_offset, extent_length, extent_count, 0, 0, 0)
    }

    fn exif_iloc_options(
        extent_offset: u32,
        extent_length: u32,
        extent_count: u16,
        version: u8,
        construction_method: u16,
        data_reference_index: u16,
    ) -> Vec<u8> {
        let mut body = vec![version, 0, 0, 0, 0x44, 0];
        if version == 2 {
            body.extend_from_slice(&1_u32.to_be_bytes());
            body.extend_from_slice(&1_u32.to_be_bytes());
        } else {
            body.extend_from_slice(&1_u16.to_be_bytes());
            body.extend_from_slice(&1_u16.to_be_bytes());
        }
        if version > 0 {
            body.extend_from_slice(&construction_method.to_be_bytes());
        }
        body.extend_from_slice(&data_reference_index.to_be_bytes());
        body.extend_from_slice(&extent_count.to_be_bytes());
        for _ in 0..extent_count {
            body.extend_from_slice(&extent_offset.to_be_bytes());
            body.extend_from_slice(&extent_length.to_be_bytes());
        }
        atom(b"iloc", &body)
    }

    fn heif_with_exif(
        tiff: &[u8],
        tiff_header_offset: u32,
        extent_count: u16,
        trailing_media: usize,
    ) -> Vec<u8> {
        let ftyp = ftyp_prefix(b"heic");
        let build_meta = |extent_offset| {
            let mut body = vec![0, 0, 0, 0];
            body.extend_from_slice(&atom(b"hdlr", &[]));
            body.extend_from_slice(&exif_iinf());
            body.extend_from_slice(&exif_iloc(
                extent_offset,
                u32::try_from(4 + tiff_header_offset as usize + tiff.len())
                    .expect("EXIF extent length"),
                extent_count,
            ));
            atom(b"meta", &body)
        };
        let placeholder_meta = build_meta(0);
        let extent_offset =
            u32::try_from(ftyp.len() + placeholder_meta.len() + 8).expect("EXIF file offset");
        let meta = build_meta(extent_offset);
        assert_eq!(meta.len(), placeholder_meta.len());

        let mut mdat_body = Vec::new();
        mdat_body.extend_from_slice(&tiff_header_offset.to_be_bytes());
        mdat_body.resize(4 + tiff_header_offset as usize, 0);
        mdat_body.extend_from_slice(tiff);
        mdat_body.resize(mdat_body.len() + trailing_media, 0);

        [ftyp, meta, atom(b"mdat", &mdat_body)].concat()
    }

    struct CountingCursor {
        inner: std::io::Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl std::io::Read for CountingCursor {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let read = std::io::Read::read(&mut self.inner, output)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    impl std::io::Seek for CountingCursor {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            std::io::Seek::seek(&mut self.inner, position)
        }
    }

    #[test]
    fn locate_exif_tiff_streams_control_data_and_honours_prefix_offset() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let bytes = heif_with_exif(&tiff, 7, 1, 1024 * 1024);
        let len = bytes.len() as u64;
        let mut source = CountingCursor {
            inner: std::io::Cursor::new(bytes),
            bytes_read: 0,
        };

        let (start, tiff_len) = locate_exif_tiff(&mut source, len)
            .expect("locate HEIF EXIF")
            .expect("HEIF EXIF extent");
        let mut actual = vec![0_u8; tiff.len()];
        read_file_exact(&mut source, len, start, &mut actual).expect("read located TIFF");

        assert_eq!(tiff_len, tiff.len() as u64);
        assert_eq!(actual, tiff);
        assert!(
            source.bytes_read < 512,
            "HEIF EXIF location read {} bytes",
            source.bytes_read
        );
    }

    #[test]
    fn locate_exif_tiff_rejects_out_of_file_and_multiple_extents() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let mut out_of_file = heif_with_exif(&tiff, 0, 1, 0);
        let iloc_kind = out_of_file
            .windows(4)
            .position(|window| window == b"iloc")
            .expect("iloc atom");
        out_of_file[iloc_kind + 22..iloc_kind + 26].copy_from_slice(&u32::MAX.to_be_bytes());
        let len = out_of_file.len() as u64;
        assert!(matches!(
            locate_exif_tiff(&mut std::io::Cursor::new(out_of_file), len),
            Err(HeifExifError::Malformed)
        ));

        let multiple = heif_with_exif(&tiff, 0, 2, 0);
        let len = multiple.len() as u64;
        assert!(matches!(
            locate_exif_tiff(&mut std::io::Cursor::new(multiple), len),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn read_exif_item_location_supports_versions_and_rejects_non_file_locations() {
        for version in 0..=2 {
            let bytes = exif_iloc_options(123, 45, 1, version, 0, 0);
            let atom = FileAtom {
                kind: *b"iloc",
                body_start: 8,
                end: bytes.len() as u64,
            };
            assert_eq!(
                read_exif_item_location(&mut std::io::Cursor::new(bytes), atom, 1)
                    .expect("parse iloc"),
                Some((0, 123, 45))
            );
        }

        for (construction_method, data_reference_index) in [(1, 0), (0, 1)] {
            let bytes = exif_iloc_options(123, 45, 1, 1, construction_method, data_reference_index);
            let atom = FileAtom {
                kind: *b"iloc",
                body_start: 8,
                end: bytes.len() as u64,
            };
            assert!(matches!(
                read_exif_item_location(&mut std::io::Cursor::new(bytes), atom, 1),
                Err(HeifExifError::Malformed)
            ));
        }
    }

    #[test]
    fn read_exif_item_location_rejects_zero_width_extents_before_iteration() {
        let mut body = vec![0, 0, 0, 0, 0, 0];
        body.extend_from_slice(&1_u16.to_be_bytes());
        body.extend_from_slice(&1_u16.to_be_bytes());
        body.extend_from_slice(&0_u16.to_be_bytes());
        body.extend_from_slice(&u16::MAX.to_be_bytes());
        let bytes = atom(b"iloc", &body);
        let atom = FileAtom {
            kind: *b"iloc",
            body_start: 8,
            end: bytes.len() as u64,
        };

        assert!(matches!(
            read_exif_item_location(&mut std::io::Cursor::new(bytes), atom, 1),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn read_exif_item_id_supports_versions_and_rejects_ambiguity() {
        for (version, item_id) in [(2, 1), (3, 70_000)] {
            let bytes = exif_iinf_entries(&[exif_infe(item_id, version)]);
            let atom = FileAtom {
                kind: *b"iinf",
                body_start: 8,
                end: bytes.len() as u64,
            };
            assert_eq!(
                read_exif_item_id(&mut std::io::Cursor::new(bytes), atom).expect("parse iinf"),
                Some(item_id)
            );
        }

        let duplicate = exif_iinf_entries(&[exif_infe(1, 2), exif_infe(2, 2)]);
        let atom = FileAtom {
            kind: *b"iinf",
            body_start: 8,
            end: duplicate.len() as u64,
        };
        assert!(matches!(
            read_exif_item_id(&mut std::io::Cursor::new(duplicate), atom),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn read_file_atom_handles_extended_and_parent_sized_atoms() {
        let mut extended = Vec::new();
        extended.extend_from_slice(&1_u32.to_be_bytes());
        extended.extend_from_slice(b"meta");
        extended.extend_from_slice(&24_u64.to_be_bytes());
        extended.extend_from_slice(&[0_u8; 8]);
        let atom = read_file_atom(
            &mut std::io::Cursor::new(&extended),
            0,
            extended.len() as u64,
        )
        .expect("read extended atom")
        .expect("extended atom");
        assert_eq!(atom.body_start, 16);
        assert_eq!(atom.end, 24);

        let mut parent_sized = Vec::new();
        parent_sized.extend_from_slice(&0_u32.to_be_bytes());
        parent_sized.extend_from_slice(b"mdat");
        parent_sized.extend_from_slice(&[0_u8; 8]);
        let atom = read_file_atom(
            &mut std::io::Cursor::new(&parent_sized),
            0,
            parent_sized.len() as u64,
        )
        .expect("read parent-sized atom")
        .expect("parent-sized atom");
        assert_eq!(atom.body_start, 8);
        assert_eq!(atom.end, parent_sized.len() as u64);
    }

    #[test]
    fn is_heif_path_recognises_heic_variants() {
        assert!(is_heif_path(Path::new("/a/b.heic")));
        assert!(is_heif_path(Path::new("/a/b.HEIC")));
        assert!(is_heif_path(Path::new("/a/b.HEIF")));
        assert!(is_heif_path(Path::new("/a/b.hif")));
        assert!(is_heif_path(Path::new("/a/b.avif")));
        assert!(!is_heif_path(Path::new("/a/b.jpg")));
        assert!(!is_heif_path(Path::new("/a/b.mov")));
        assert!(!is_heif_path(Path::new("/a/b")));
    }

    /// Build a minimal ftyp prefix with the given major brand for tests.
    fn ftyp_prefix(brand: &[u8; 4]) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"mif1");
        bytes.extend_from_slice(b"heic");
        bytes
    }

    #[test]
    fn is_heif_content_accepts_all_known_brands() {
        for brand in [
            b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevm", b"hevs", b"mif1", b"msf1",
            b"avif", b"avis",
        ] {
            assert!(
                is_heif_content(&ftyp_prefix(brand)),
                "expected brand {:?} to be HEIF",
                std::str::from_utf8(brand).unwrap()
            );
        }
    }

    #[test]
    fn is_heif_content_rejects_non_heif_iso_bmff() {
        // mp4/mov: ftyp present but brand is not in the HEIF family.
        for brand in [b"mp42", b"isom", b"qt  ", b"M4V "] {
            assert!(
                !is_heif_content(&ftyp_prefix(brand)),
                "expected brand {:?} to NOT be HEIF",
                std::str::from_utf8(brand).unwrap()
            );
        }
    }

    #[test]
    fn is_heif_content_rejects_jpeg_magic() {
        // SOI + APP0 prefix; bytes 4..8 are not "ftyp".
        let bytes = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
        ];
        assert!(!is_heif_content(&bytes));
    }

    #[test]
    fn is_heif_content_rejects_short_or_empty_input() {
        assert!(!is_heif_content(&[]));
        assert!(!is_heif_content(&[0; 11]));
    }

    #[test]
    fn is_heif_content_rejects_garbage_with_no_ftyp() {
        let blob: Vec<u8> = (0..32_u8).collect();
        assert!(!is_heif_content(&blob));
    }

    // ── extract_xmp_bytes: malformed input must not panic, must return None ──
    //
    // The original suite only covered `is_heif_path`; the parser entry points
    // (`extract_xmp_bytes`, `insert_xmp`) had no malformed-input regression.
    // A regression that panicked on truncated bytes would crash the metadata
    // worker on any partial download — silent data loss in the surrounding
    // sync. These pin the "return None / bail" contract for the universe of
    // garbage inputs the wild can produce.

    #[test]
    fn extract_xmp_bytes_empty_input_returns_none() {
        // Zero bytes is the most basic malformed case.
        assert!(extract_xmp_bytes(&[]).is_none());
    }

    #[test]
    fn extract_xmp_bytes_random_bytes_returns_none() {
        // Plausible-looking-but-not-HEIF blob: must not panic, must return
        // None. The previous mp4_atom decode loop swallowed errors via
        // `if let Ok(Some(...)) = ...`, but a future refactor that switched
        // to `.unwrap()` would explode on this input.
        let blob: Vec<u8> = (0..256_u16).map(|i| (i & 0xff) as u8).collect();
        assert!(extract_xmp_bytes(&blob).is_none());
    }

    #[test]
    fn extract_xmp_bytes_truncated_atom_header_returns_none() {
        // 4 bytes is shorter than any valid ISO-BMFF box header (8 bytes).
        // Decoder must not panic on the short read.
        let bytes = [0x00, 0x00, 0x00, 0x18];
        assert!(extract_xmp_bytes(&bytes).is_none());
    }

    #[test]
    fn extract_xmp_bytes_no_meta_box_returns_none() {
        // A syntactically valid `ftyp` atom with no following `meta` — there
        // is no XMP to find, so the function must return None without error.
        // ftyp box: size=0x18 (24), kind=ftyp, major_brand=heic, minor_version=0,
        // compatible_brands=[heic, mif1].
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        assert_eq!(bytes.len(), 0x18);
        assert!(extract_xmp_bytes(&bytes).is_none());
    }

    #[test]
    fn extract_xmp_bytes_atom_with_oversized_length_field_returns_none() {
        // size field claims 0xFFFFFFFF bytes (way past end of buffer). A
        // robust parser must reject this, not allocate or read out of bounds.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0xFFFF_FFFF_u32.to_be_bytes());
        bytes.extend_from_slice(b"meta");
        bytes.extend_from_slice(&[0; 16]); // payload tail (will be cut short)
        assert!(extract_xmp_bytes(&bytes).is_none());
    }

    #[test]
    fn extract_xmp_bytes_top_level_dfla_does_not_oom() {
        // Regression: this 110-byte input was the first OOM repro from the
        // libfuzzer harness (`fuzz/seeds/heif_atoms/regression-iloc-oom`).
        // Pre-fix, `Any::decode_maybe` saw a top-level `dfLa` FourCC,
        // dispatched to `Dfla::decode_body` -> `parse_vorbis_comment`, and
        // tried to `Vec::with_capacity(~876_000_000)` for a `Vec<String>`
        // (~21 GiB) - upstream kixelated/mp4-atom#154, fixed in #157. The
        // kei-side fix is independent: walk top-level boxes by header and
        // only descend into `meta`, so a hostile `dfLa` here is skipped
        // even if a future regression reintroduces the upstream bug.
        const REPRO: &[u8] = &[
            0x00, 0x00, 0x00, 0x08, 0x00, 0x1d, 0x00, 0x22, 0x00, 0x00, 0x00, 0x00, 0x64, 0x66,
            0x4c, 0x61, 0x00, 0x00, 0x00, 0xf6, 0x6a, 0x00, 0x00, 0x10, 0x0d, 0xaa, 0x6b, 0x9d,
            0xbb, 0xff, 0xff, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x0c, 0x0c, 0x1b, 0x00, 0x04, 0x00,
            0x00, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x66, 0x6c, 0x36, 0x34, 0x00, 0x32, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x4f, 0xe0,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x22, 0x00, 0x00, 0x00,
            0x64, 0x66, 0x4c, 0x61, 0x00, 0x00, 0x00, 0xf6, 0x6a, 0x00, 0x00, 0x10, 0x0d, 0xaa,
            0x6b, 0x9d, 0xbb, 0xff, 0xff, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x00, 0x00,
        ];
        assert_eq!(REPRO.len(), 110);
        assert!(extract_xmp_bytes(REPRO).is_none());
    }

    #[test]
    fn extract_xmp_bytes_meta_with_nested_dfla_is_safe() {
        // The same upstream OOM was reachable from a `meta` box containing
        // a nested `dfLa` sub-atom, because mp4_atom::Meta::decode_body uses
        // `Any::decode_maybe` internally on every item. Upstream #157 caps
        // the `parse_vorbis_comment` allocation at the root, but the kei
        // header-walk also descends into meta with only `iinf`/`iloc`
        // decoders, so an attacker-supplied `dfLa` inside `meta` is skipped
        // regardless of upstream regressions.
        //
        // Layout: <meta box header> <version+flags> <hdlr box> <dfLa box>.
        // The dfLa body declares 0xFFFF_FFFF Vorbis comment fields; pre-fix
        // (or with a future Meta::decode_body-using rewrite) this would
        // allocate ~103 GiB.
        let mut hdlr: Vec<u8> = Vec::new();
        hdlr.extend_from_slice(&0x21_u32.to_be_bytes()); // size = header(8) + body(25) = 33
        hdlr.extend_from_slice(b"hdlr");
        hdlr.extend_from_slice(&[0; 4]); // version+flags
        hdlr.extend_from_slice(&[0; 4]); // pre_defined
        hdlr.extend_from_slice(b"pict"); // handler_type
        hdlr.extend_from_slice(&[0; 12]); // reserved
        hdlr.push(0); // empty name (null-terminated)

        let mut dfla: Vec<u8> = Vec::new();
        dfla.extend_from_slice(&0x18_u32.to_be_bytes()); // size = 24
        dfla.extend_from_slice(b"dfLa");
        dfla.extend_from_slice(&[0; 4]); // version+flags
        // metadata block header: last_block=1, type=4 (vorbis_comment), length=8
        dfla.extend_from_slice(&[0x84, 0x00, 0x00, 0x08]);
        // vorbis comment body: vendor_string_length=0, number_of_fields=0xFFFF_FFFF
        dfla.extend_from_slice(&0_u32.to_le_bytes());
        dfla.extend_from_slice(&u32::MAX.to_le_bytes());

        let mut meta_body: Vec<u8> = Vec::new();
        meta_body.extend_from_slice(&[0; 4]); // version+flags
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&dfla);

        let mut meta_box: Vec<u8> = Vec::new();
        let total = (8 + meta_body.len()) as u32;
        meta_box.extend_from_slice(&total.to_be_bytes());
        meta_box.extend_from_slice(b"meta");
        meta_box.extend_from_slice(&meta_body);

        // Must return None instead of allocating gigabytes.
        assert!(extract_xmp_bytes(&meta_box).is_none());
    }

    /// CG-14 / MS-5-full: a malformed iinf inside an otherwise-walkable
    /// meta box previously surfaced as a silent None — indistinguishable
    /// from "no XMP present". The strict variant must surface the
    /// structural failure as a typed `HeifError::MetaSubBoxDecode` so the
    /// metadata-write path can mark the asset for rewrite next sync. The
    /// lenient `extract_xmp_bytes` collapses the same input to None for
    /// callers (e.g. the EXIF probe) that don't care about the cause.
    #[test]
    fn extract_xmp_strict_returns_meta_sub_box_decode_on_malformed_iinf() {
        let meta_box = malformed_iinf_meta_box();

        let err = extract_xmp_strict(&meta_box).unwrap_err();
        match err {
            HeifError::MetaSubBoxDecode { kind, .. } => {
                assert_eq!(kind, FourCC::new(b"iinf"));
            }
            other => panic!("expected MetaSubBoxDecode for iinf, got {other:?}"),
        }

        // Lenient variant: same input, structural failure collapsed to None.
        assert!(extract_xmp_bytes(&meta_box).is_none());
    }

    #[test]
    fn extract_xmp_bytes_unsupported_infe_v1_returns_none_not_panic() {
        // Durable unit regression for fuzz artifact
        // crash-26040ebf1e311287ba7f285b767ac5a6ca9aef5e. The unsupported
        // version-1 `infe` shape must not panic in the lenient probe path.
        const REPRO: &[u8] = &[
            0x00, 0x00, 0x00, 0x00, b'm', b'e', b't', b'a', 0x00, 0x1d, 0x00, 0x22, 0x00, 0x00,
            0x00, 0x08, 0x00, 0x00, 0x00, 0x5b, 0x00, 0x00, 0x00, 0x00, b'i', b'i', b'n', b'f',
            0x00, 0x00, 0x00, 0x00, 0x5b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0x80,
            0x01, 0x00, 0x00, 0x04, 0x00, b'p', b'y', b't', b'f',
        ];

        let lenient = std::panic::catch_unwind(|| extract_xmp_bytes(REPRO));
        assert!(
            lenient.is_ok(),
            "lenient HEIF XMP probe must not panic on unsupported infe v1"
        );
        assert_eq!(lenient.unwrap(), None);

        let strict = std::panic::catch_unwind(|| extract_xmp_strict(REPRO));
        assert!(
            strict.is_ok(),
            "strict HEIF XMP probe must convert unsupported infe v1 to a typed error"
        );
        match strict.unwrap() {
            Err(HeifError::MetaSubBoxDecode { kind, .. }) => {
                assert_eq!(kind, FourCC::new(b"iinf"));
            }
            other => panic!("expected MetaSubBoxDecode for unsupported infe v1, got {other:?}"),
        }
    }

    fn malformed_iinf_meta_box() -> Vec<u8> {
        let mut hdlr: Vec<u8> = Vec::new();
        hdlr.extend_from_slice(&0x21_u32.to_be_bytes());
        hdlr.extend_from_slice(b"hdlr");
        hdlr.extend_from_slice(&[0; 4]);
        hdlr.extend_from_slice(&[0; 4]);
        hdlr.extend_from_slice(b"pict");
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.push(0);

        let mut iinf: Vec<u8> = Vec::new();
        // size = 9 (header 8 + body 1); body is 1 byte but Iinf::decode_body
        // requires at least version+flags+entry_count (6 bytes).
        iinf.extend_from_slice(&0x09_u32.to_be_bytes());
        iinf.extend_from_slice(b"iinf");
        iinf.push(0);

        let mut meta_body: Vec<u8> = Vec::new();
        meta_body.extend_from_slice(&[0; 4]);
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&iinf);

        let mut meta_box: Vec<u8> = Vec::new();
        let total = (8 + meta_body.len()) as u32;
        meta_box.extend_from_slice(&total.to_be_bytes());
        meta_box.extend_from_slice(b"meta");
        meta_box.extend_from_slice(&meta_body);
        meta_box
    }

    // ── insert_xmp: typed errors per failure mode ──
    //
    // Each pin asserts on a specific HeifError variant so a future refactor
    // that drops or reclassifies a failure lands a test failure rather than
    // a silent regression. Variant matching keeps the assertions stable
    // across error-message rewording.

    #[test]
    fn insert_xmp_returns_missing_meta_on_input_with_no_meta_box() {
        // ftyp-only fixture — syntactically valid ISO-BMFF, but no `meta`
        // box, so HEIC surgery has nothing to operate on.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");

        let mut out: Vec<u8> = Vec::new();
        let err = insert_xmp(&bytes, b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>", &mut out)
            .expect_err("insert_xmp must reject input without a meta box");
        assert!(
            matches!(err, HeifError::MissingMeta { input_len } if input_len == bytes.len()),
            "expected MissingMeta with correct input_len, got: {err:?}",
        );
        // Critical: nothing should have been written to the writer.
        assert!(
            out.is_empty(),
            "no bytes should be flushed when input has no meta box; got {} bytes",
            out.len()
        );
    }

    #[test]
    fn insert_xmp_returns_unparsable_tail_on_short_trailing_bytes() {
        // ftyp box followed by 3 stray bytes that can't form a valid atom
        // header. The parser must surface this as UnparsableTail (truncation
        // signal), not as a generic Decode error.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let total = bytes.len() as u64;

        let mut out: Vec<u8> = Vec::new();
        let err = insert_xmp(&bytes, b"<x/>", &mut out)
            .expect_err("insert_xmp must surface parse errors on unparsable tail");
        assert!(
            matches!(err, HeifError::UnparsableTail { offset: 0x18, total: t } if t == total),
            "expected UnparsableTail at offset 0x18 of {total}, got: {err:?}",
        );
    }

    #[test]
    fn insert_xmp_output_keeps_ftyp_with_known_heif_brand() {
        // The post-download magic-byte check (`is_heif_content`) currently
        // runs on the original bytes; nothing re-checks the file header
        // after `insert_xmp` rewrites it. A regression in the rewriter
        // that produced a malformed prefix (e.g. truncated `ftyp` size,
        // wrong brand, double atom) would land on disk and only surface
        // when downstream tools (Immich, iCloud re-import) refuse the
        // file. This test pins the invariant on the canonical fixture so
        // any prefix-shape regression fails loudly here first.
        const SAMPLE_HEIC: &[u8] = include_bytes!("../../tests/data/sample.heic");
        // Sanity: the fixture itself has to start with a HEIF brand or
        // the test is meaningless.
        assert!(
            is_heif_content(SAMPLE_HEIC),
            "fixture sample.heic must already be HEIF-shaped"
        );

        let mut out: Vec<u8> = Vec::new();
        insert_xmp(
            SAMPLE_HEIC,
            b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>",
            &mut out,
        )
        .expect("insert_xmp on a valid HEIC fixture must succeed");
        assert!(
            out.len() >= 12,
            "rewritten output must contain at least ftyp(8) + brand(4); got {} bytes",
            out.len()
        );
        // Bytes 4..8 must be the FourCC "ftyp"; bytes 8..12 must be one
        // of the brands `is_heif_content` accepts. This is exactly the
        // contract the post-rewrite magic-byte check would assert.
        assert_eq!(
            &out[4..8],
            b"ftyp",
            "rewritten output must begin with an ftyp box; first 12 bytes: {:?}",
            &out[..12]
        );
        assert!(
            is_heif_content(&out),
            "rewritten output must still pass is_heif_content; first 12 bytes: {:?}",
            &out[..12]
        );
    }

    #[test]
    fn insert_xmp_then_extract_round_trips_payload() {
        let heic = include_bytes!("../../tests/data/sample.heic");
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF/></x:xmpmeta>";

        let mut rewritten: Vec<u8> = Vec::new();
        insert_xmp(heic.as_slice(), xmp.as_slice(), &mut rewritten)
            .expect("insert_xmp must succeed on a valid HEIC");

        let extracted = extract_xmp_bytes(&rewritten)
            .expect("extract_xmp_bytes must find the XMP we just inserted");
        assert_eq!(extracted, xmp, "round-tripped XMP must be byte-identical");
    }

    #[test]
    fn rewrite_xmp_preserves_real_heic_image_data_and_is_idempotent() {
        let input = include_bytes!("../../tests/data/sample.heic");
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>5</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut first = Vec::new();
        rewrite_xmp(input, xmp, &mut first).expect("real HEIC XMP insertion");
        assert!(is_heif_content(&first));
        assert_eq!(extract_xmp_bytes(&first).as_deref(), Some(xmp.as_slice()));
        let (_, image_data) = find_mdat(input).expect("source image mdat");
        let (rewritten_image_start, rewritten_image_data) =
            find_mdat(&first).expect("rewritten image mdat");
        assert_eq!(Some(image_data), Some(rewritten_image_data));
        let iloc = decode_iloc_from_heic(&first).expect("rewritten iloc");
        let image_item = iloc
            .item_locations
            .iter()
            .find(|item| item.item_id == 1)
            .expect("source image item");
        let image_offset = image_item
            .base_offset
            .saturating_add(image_item.extents[0].offset);
        assert_eq!(
            image_offset,
            rewritten_image_start + 8,
            "rewritten image item must still point at the image mdat"
        );
        assert!(
            cdsc_references(input)
                .into_iter()
                .all(|reference| cdsc_references(&first).contains(&reference)),
            "pre-existing cdsc associations must remain intact"
        );
        let (xmp_item_id, primary_item_id) = xmp_and_primary_item_ids(&first);
        assert!(
            cdsc_references(&first).contains(&(xmp_item_id, primary_item_id)),
            "new XMP item must describe the primary image through cdsc"
        );

        let mut second = Vec::new();
        rewrite_xmp(&first, xmp, &mut second).expect("repeat HEIC XMP insertion");
        assert_eq!(
            second, first,
            "repeating the same XMP update must be byte-idempotent"
        );

        let changed = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating><dc:description xmlns:dc='http://purl.org/dc/elements/1.1/'>changed</dc:description></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut third = Vec::new();
        rewrite_xmp(&first, changed, &mut third).expect("changed HEIC XMP update");
        assert_eq!(
            extract_xmp_bytes(&third).as_deref(),
            Some(changed.as_slice())
        );
        assert_eq!(Some(image_data), find_mdat(&third).map(|(_, data)| data));
    }

    #[test]
    fn fuzz_seeds_reach_existing_xmp_replacement_paths() {
        const MARKER: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let fits = include_bytes!("../../fuzz/seeds/heif_rewrite/replacement-fits");
        let grows = include_bytes!("../../fuzz/seeds/heif_rewrite/replacement-grows");

        let mut replaced_in_place = Vec::new();
        rewrite_xmp(fits, MARKER, &mut replaced_in_place).expect("fitting replacement seed");
        assert_eq!(
            replaced_in_place.len(),
            fits.len(),
            "fitting seed must take the in-place replacement path"
        );

        let mut replaced_by_append = Vec::new();
        rewrite_xmp(grows, MARKER, &mut replaced_by_append).expect("growing replacement seed");
        assert!(
            replaced_by_append.len() > grows.len(),
            "growing seed must repoint XMP to an appended mdat"
        );

        // The fuzzer will not synthesise a valid multi-image item map on its
        // own, so selection stays unreachable without a seed carrying one.
        let multi = include_bytes!("../../fuzz/seeds/heif_rewrite/multi-xmp");
        assert_eq!(
            extract_xmp_raw(multi).as_deref(),
            Some(PRIMARY_XMP),
            "multi-image seed must resolve to the primary image's packet"
        );
        #[cfg(feature = "__fuzz_internals")]
        fuzz_rewrite_xmp_preserves(multi);
    }

    #[test]
    fn rewrite_xmp_appends_when_existing_extent_includes_mdat_header() {
        const MARKER: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut input = include_bytes!("../../fuzz/seeds/heif_rewrite/replacement-fits").to_vec();
        let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(&input).unwrap();
        let (location, xmp_item_id, _, _, _, _, _) =
            locate_xmp(&input, iinf, iloc, iref, primary_item_id).unwrap();
        let location = location.expect("seed XMP location");
        let layout = parse_iloc(&input, iloc).unwrap();
        let item = layout
            .items
            .iter()
            .find(|item| Some(item.item_id) == xmp_item_id)
            .expect("seed XMP item");
        let iloc_body = &mut input[iloc.body_start()..iloc.end()];
        if let Some(base_offset_pos) = item.base_offset_pos {
            write_uint(iloc_body, base_offset_pos, layout.base_offset_size, 0).unwrap();
        }
        write_uint(
            iloc_body,
            item.extents[0].offset_pos.expect("XMP extent offset"),
            layout.offset_size,
            u64::try_from(location.extent_start - 8).unwrap(),
        )
        .unwrap();

        let mut output = Vec::new();
        rewrite_xmp(&input, MARKER, &mut output).expect("safe append replacement");

        assert!(
            output.len() > input.len(),
            "an XMP extent that includes an mdat header must not be changed in place"
        );
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MARKER));
        validate_rewrite_preserves_non_xmp_items(&input, &output)
            .expect("append replacement must preserve protected bytes");
    }

    #[test]
    fn rewrite_xmp_rejects_malformed_heic_without_writing() {
        let input = &include_bytes!("../../tests/data/sample.heic")[..input_len()];
        let mut output = Vec::new();
        let result = rewrite_xmp(input, b"<x:xmpmeta/>", &mut output);
        assert!(result.is_err());
        assert!(output.is_empty());

        fn input_len() -> usize {
            include_bytes!("../../tests/data/sample.heic").len() - 3
        }
    }

    #[test]
    fn rewrite_xmp_rejects_insertion_when_top_level_offsets_are_unhandled() {
        let mut input = include_bytes!("../../tests/data/sample.heic").to_vec();
        input.extend_from_slice(&8u32.to_be_bytes());
        input.extend_from_slice(b"moov");

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, b"<x:xmpmeta/>", &mut output);
        assert!(result.is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn rewrite_xmp_rejects_orphan_iloc_item_id() {
        let mut atoms = Vec::new();
        let mut cursor: &[u8] = include_bytes!("../../tests/data/sample.heic");
        while let Some(atom) = Any::decode_maybe(&mut cursor).expect("sample HEIC") {
            atoms.push(atom);
        }
        let meta = atoms
            .iter_mut()
            .find_map(|atom| match atom {
                Any::Meta(meta) => Some(meta),
                _ => None,
            })
            .expect("sample meta");
        meta.get_mut::<Iloc>()
            .expect("sample iloc")
            .item_locations
            .push(ItemLocation {
                item_id: 999,
                construction_method: 0,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![ItemLocationExtent {
                    item_reference_index: 0,
                    offset: 0,
                    length: 0,
                }],
            });
        let mut input = Vec::new();
        for atom in atoms {
            atom.encode(&mut input).expect("encode malformed fixture");
        }

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, b"<x:xmpmeta/>", &mut output);
        assert!(result.is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn rewrite_xmp_existing_item_can_append_after_unhandled_top_level_box() {
        let seed = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF/></x:xmpmeta>";
        let mut seeded = Vec::new();
        insert_xmp(
            include_bytes!("../../tests/data/sample.heic"),
            seed,
            &mut seeded,
        )
        .expect("seed XMP");
        seeded.extend_from_slice(&8u32.to_be_bytes());
        seeded.extend_from_slice(b"moov");

        let replacement =
            b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>5</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut output = Vec::new();
        rewrite_xmp(&seeded, replacement, &mut output).expect("existing XMP append");
        assert_eq!(
            extract_xmp_bytes(&output).as_deref(),
            Some(replacement.as_slice())
        );
        assert!(
            output
                .windows(8)
                .any(|window| window == [0, 0, 0, 8, b'm', b'o', b'o', b'v'])
        );
    }

    /// Independent-reader oracle. kei's reader and writer share a code base, so
    /// a packet that round-trips through both proves only self-consistency. This
    /// drives ExifTool, which parses the HEIF item map itself and applies the
    /// same primary-image rule, over the rewritten bytes.
    ///
    /// ExifTool is optional for a local run. `KEI_REQUIRE_HEIF_ORACLE` makes it
    /// mandatory, so CI cannot lose this coverage by failing to install it.
    #[test]
    fn rewrite_xmp_is_readable_by_an_independent_heif_reader() {
        use std::process::Command;

        let available = Command::new("exiftool")
            .arg("-ver")
            .output()
            .is_ok_and(|out| out.status.success());
        if !available {
            let required = std::env::var("KEI_REQUIRE_HEIF_ORACLE")
                .is_ok_and(|value| !value.trim().is_empty());
            assert!(
                !required,
                "KEI_REQUIRE_HEIF_ORACLE is set but exiftool is not installed"
            );
            eprintln!("exiftool unavailable; skipping independent HEIF reader check");
            return;
        }

        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>5</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let dir = tempfile::tempdir().expect("reader fixture directory");

        let read_tag = |path: &std::path::Path, tag: &str| -> String {
            let out = Command::new("exiftool")
                .args(["-s3", tag])
                .arg(path)
                .output()
                .expect("run exiftool");
            assert!(
                out.status.success(),
                "exiftool must accept {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        for (name, source) in [
            (
                "sample.heic",
                include_bytes!("../../tests/data/sample.heic").as_slice(),
            ),
            (
                "apple-hdr-gainmap.heic",
                include_bytes!("../../tests/data/apple-hdr-gainmap.heic").as_slice(),
            ),
            (
                "white_1x1.avif",
                include_bytes!("../../tests/data/white_1x1.avif").as_slice(),
            ),
        ] {
            let mut output = Vec::new();
            rewrite_xmp(source, xmp, &mut output).expect("rewrite for reader check");
            let path = dir.path().join(name);
            std::fs::write(&path, &output).expect("write reader fixture");

            assert_eq!(
                read_tag(&path, "-Validate"),
                "OK",
                "{name} must still validate after a rewrite"
            );
            assert_eq!(
                read_tag(&path, "-XMP:Rating"),
                "5",
                "an independent reader must resolve the packet kei wrote into {name}"
            );

            let source_path = dir.path().join(format!("source-{name}"));
            std::fs::write(&source_path, source).expect("write source fixture");
            assert_eq!(
                read_tag(&path, "-HDRGainMapVersion"),
                read_tag(&source_path, "-HDRGainMapVersion"),
                "{name} must keep whatever gain map it arrived with"
            );
        }
    }

    fn xmp_and_primary_item_ids(bytes: &[u8]) -> (u32, u32) {
        let (_, iinf, _, iref, primary_item_id, _) = find_meta_layout(bytes).unwrap();
        let iinf_layout = parse_iinf(bytes, iinf).unwrap();
        (
            select_xmp_item_id(bytes, iref, primary_item_id, &iinf_layout.xmp_item_ids)
                .unwrap()
                .unwrap(),
            primary_item_id,
        )
    }

    /// Every `cdsc` edge as a `(descriptive item, described image)` pair. A
    /// reference may name several images, and each one becomes its own pair.
    fn cdsc_references(bytes: &[u8]) -> Vec<(u32, u32)> {
        let (_, _, _, Some(iref), _, _) = find_meta_layout(bytes).unwrap() else {
            return Vec::new();
        };
        let body = &bytes[iref.body_start()..iref.end()];
        let version = body[0];
        scan_raw_boxes(body, 4, body.len())
            .unwrap()
            .into_iter()
            .filter(|child| child.kind == *b"cdsc")
            .flat_map(|child| {
                let child_body = &body[child.body_start()..child.end()];
                let width = if version == 0 { 2 } else { 4 };
                let from = if version == 0 {
                    u32::from(u16::from_be_bytes([child_body[0], child_body[1]]))
                } else {
                    u32::from_be_bytes(child_body[0..4].try_into().unwrap())
                };
                let count = u16::from_be_bytes([child_body[width], child_body[width + 1]]) as usize;
                (0..count)
                    .map(|index| {
                        let start = width + 2 + index * width;
                        let to = if version == 0 {
                            u32::from(u16::from_be_bytes([
                                child_body[start],
                                child_body[start + 1],
                            ]))
                        } else {
                            u32::from_be_bytes(child_body[start..start + 4].try_into().unwrap())
                        };
                        (from, to)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn insert_xmp_twice_retains_only_latest() {
        let heic = include_bytes!("../../tests/data/sample.heic");
        let first = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><first/></x:xmpmeta>";
        let second = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><second/></x:xmpmeta>";

        let mut after_first: Vec<u8> = Vec::new();
        insert_xmp(heic.as_slice(), first.as_slice(), &mut after_first)
            .expect("first insert must succeed");

        let mut after_second: Vec<u8> = Vec::new();
        insert_xmp(&after_first, second.as_slice(), &mut after_second)
            .expect("second insert must succeed");

        let extracted =
            extract_xmp_bytes(&after_second).expect("extract must find XMP after double insert");
        assert_eq!(
            extracted,
            second.as_slice(),
            "only the latest XMP packet should be present"
        );
    }

    #[test]
    fn heif_error_io_variant_carries_underlying_io_error() {
        // Sanity-check the Io variant — the writer used by insert_xmp is
        // any std::io::Write, and io::Error must convert via From.
        let io_err = std::io::Error::other("disk full");
        let err: HeifError = io_err.into();
        assert!(matches!(err, HeifError::Io(_)));
    }

    // ── Regression: insert_xmp with Apple QuickTime Meta (no version+flags) ──
    //
    // mp4_atom::Meta::encode_body always writes ISO format (4-byte
    // version+flags) regardless of the input format. When the input Meta
    // is Apple QuickTime format (no version+flags), the re-encoded Meta is
    // 4 bytes larger than the original. The file_offset_map used
    // encoded_size() as old_end, which inflates Meta's range 4 bytes into
    // the next atom's territory. An iloc entry pointing to the start of mdat
    // is captured by Meta's bloated range and incorrectly remapped to the
    // old (pre-growth) position instead of the correct shifted position.
    //
    // Fix: use original_offsets[i+1] (or total for last atom) as old_end.
    #[test]
    fn insert_xmp_remaps_iloc_correctly_with_apple_qt_meta() {
        let input = build_apple_qt_heic_fixture();
        assert!(is_heif_content(&input));

        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF/></x:xmpmeta>";
        let mut output: Vec<u8> = Vec::new();
        insert_xmp(&input, xmp, &mut output).expect("insert_xmp must succeed");

        assert!(is_heif_content(&output));

        // The image mdat atom must have shifted past the (now-larger) meta.
        // Decode the output iloc and verify it points to the actual mdat data.
        let out_iloc = decode_iloc_from_heic(&output).expect("output iloc");
        let out_loc = out_iloc
            .item_locations
            .iter()
            .find(|l| l.item_id == 1)
            .expect("item 1 in output iloc");
        let out_abs = out_loc
            .base_offset
            .saturating_add(out_loc.extents.first().map(|e| e.offset).unwrap_or(0));
        let (out_mdat_start, out_mdat_data) = find_mdat(&output).expect("output mdat");

        assert_eq!(
            out_abs,
            out_mdat_start + 8,
            "iloc must point to output mdat data at offset {}+8, got {out_abs}",
            out_mdat_start,
        );

        // The input mdat must match output mdat.
        let (_in_mdat_start, in_mdat_data) = find_mdat(&input).expect("input mdat");
        assert_eq!(in_mdat_data, out_mdat_data, "mdat data must be preserved");

        // Verify the meta box actually grew (mdat shifted).
        let in_meta_end = find_atom_end(&input, "meta").expect("input meta");
        let out_meta_end = find_atom_end(&output, "meta").expect("output meta");
        assert!(out_meta_end > in_meta_end, "meta must grow on re-encode");
    }

    /// Build a minimal HEIC with Apple QuickTime Meta (no version+flags)
    /// and meta-before-mdat layout. Contains one hvc1 image item with
    /// a 16-byte payload.
    fn build_apple_qt_heic_fixture() -> Vec<u8> {
        use mp4_atom::{
            Hdlr, Hvcc, Iinf, Iloc, Ipco, Ipma, Iprp, ItemInfoEntry, ItemLocation,
            ItemLocationExtent, Pitm, PropertyAssociation, PropertyAssociations,
        };

        let payload: Vec<u8> = (0..16).collect();
        let mut tmp: Vec<u8> = Vec::new();

        let hdlr = Hdlr {
            handler: FourCC::new(b"pict"),
            name: String::new(),
        };
        hdlr.encode(&mut tmp).unwrap();
        let hdlr_enc: Vec<u8> = std::mem::take(&mut tmp);

        Pitm { item_id: 1 }.encode(&mut tmp).unwrap();
        let pitm_enc: Vec<u8> = std::mem::take(&mut tmp);

        Iinf {
            item_infos: vec![ItemInfoEntry {
                item_id: 1,
                item_protection_index: 0,
                item_type: Some(FourCC::new(b"hvc1")),
                item_name: String::new(),
                content_type: None,
                content_encoding: None,
                item_uri_type: None,
                item_not_in_presentation: false,
            }],
        }
        .encode(&mut tmp)
        .unwrap();
        let iinf_enc: Vec<u8> = std::mem::take(&mut tmp);

        Iprp {
            ipco: Ipco {
                properties: vec![Any::Hvcc(Hvcc::new())],
            },
            ipma: vec![Ipma {
                item_properties: vec![PropertyAssociations {
                    item_id: 1,
                    associations: vec![PropertyAssociation {
                        essential: true,
                        property_index: 1,
                    }],
                }],
            }],
        }
        .encode(&mut tmp)
        .unwrap();
        let iprp_enc: Vec<u8> = std::mem::take(&mut tmp);

        // Iterate to find iloc size / mdat offset fixed point.
        let non_iloc = hdlr_enc.len() + pitm_enc.len() + iinf_enc.len() + iprp_enc.len();
        let ftyp = 24u64;
        let meta_hdr = 8u64; // no version+flags for Apple QT
        let mut base: u64 = 0;
        let iloc_enc: Vec<u8> = loop {
            let iloc = Iloc {
                item_locations: vec![ItemLocation {
                    item_id: 1,
                    construction_method: 0,
                    data_reference_index: 0,
                    base_offset: base,
                    extents: vec![ItemLocationExtent {
                        item_reference_index: 0,
                        offset: 0,
                        length: payload.len() as u64,
                    }],
                }],
            };
            iloc.encode(&mut tmp).unwrap();
            let ilc = std::mem::take(&mut tmp);
            let correct = ftyp + meta_hdr + non_iloc as u64 + ilc.len() as u64 + 8;
            if correct == base {
                break ilc;
            }
            base = correct;
        };

        let meta_box = 8 + non_iloc + iloc_enc.len();
        let mdat_box = 8 + payload.len();
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&0x18_u32.to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(b"heic");
        buf.extend_from_slice(&0_u32.to_be_bytes());
        buf.extend_from_slice(b"mif1");
        buf.extend_from_slice(b"heic");
        buf.extend_from_slice(&(meta_box as u32).to_be_bytes());
        buf.extend_from_slice(b"meta");
        buf.extend_from_slice(&hdlr_enc);
        buf.extend_from_slice(&pitm_enc);
        buf.extend_from_slice(&iloc_enc);
        buf.extend_from_slice(&iinf_enc);
        buf.extend_from_slice(&iprp_enc);
        buf.extend_from_slice(&(mdat_box as u32).to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&payload);
        buf
    }

    /// Find the first mdat atom: return (file_offset_of_atom, data_bytes).
    fn find_mdat(bytes: &[u8]) -> Option<(u64, &[u8])> {
        let mut pos = 0;
        while pos + 8 <= bytes.len() {
            let sz =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            if &bytes[pos + 4..pos + 8] == b"mdat" {
                let end = (pos + sz).min(bytes.len());
                if end > pos + 8 {
                    return Some((pos as u64, &bytes[pos + 8..end]));
                }
            }
            if sz == 0 || pos + sz > bytes.len() {
                break;
            }
            pos += sz;
        }
        None
    }

    /// Decode the Iloc from the first Meta box in an ISO-BMFF file.
    fn decode_iloc_from_heic(bytes: &[u8]) -> Option<Iloc> {
        use mp4_atom::{Atom, Iloc};
        let mut pos = 0;
        while pos + 8 <= bytes.len() {
            let sz =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            if &bytes[pos + 4..pos + 8] == b"meta" && pos + sz <= bytes.len() {
                let body = &bytes[pos + 8..pos + sz];
                let mut cur: &[u8] = body;
                // Skip version+flags if present (ISO format).
                if cur.len() >= 8 && cur.get(4..8) != Some(b"hdlr".as_slice()) {
                    cur = cur.get(4..)?;
                }
                while cur.len() >= 8 {
                    let ss = u32::from_be_bytes([cur[0], cur[1], cur[2], cur[3]]) as usize;
                    if &cur[4..8] == b"iloc" && cur.len() >= ss {
                        return Iloc::decode_body(&mut &cur[8..ss]).ok();
                    }
                    if ss == 0 || ss > cur.len() {
                        break;
                    }
                    cur = &cur[ss..];
                }
                return None;
            }
            if sz == 0 || pos + sz > bytes.len() {
                break;
            }
            pos += sz;
        }
        None
    }

    /// Return the byte offset of the end (start + size) of the first
    /// top-level atom with the given FourCC.
    fn find_atom_end(bytes: &[u8], tag: &str) -> Option<u64> {
        let tag = tag.as_bytes();
        let mut pos = 0;
        while pos + 8 <= bytes.len() {
            let sz =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            if &bytes[pos + 4..pos + 8] == tag {
                return Some((pos + sz) as u64);
            }
            if sz == 0 || pos + sz > bytes.len() {
                break;
            }
            pos += sz;
        }
        None
    }

    const MATRIX_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>4</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";

    /// One HEIF item for [`build_heic`]. `data` is placed in the top-level
    /// `mdat` for construction method 0; construction methods 1 and 2 leave it
    /// empty and use `offset`/`length` verbatim.
    struct ItemSpec {
        item_id: u32,
        item_type: [u8; 4],
        infe_version: u8,
        construction_method: u8,
        data: Vec<u8>,
        offset: u64,
        length: u64,
    }

    /// A hand-built HEIF file covering layout shapes the real `sample.heic`
    /// fixture cannot express: grid/dimg derivation, construction methods 1
    /// and 2, item ids above `u16`, and files with no `iref`.
    struct HeicSpec {
        iloc_version: u8,
        offset_size: u8,
        length_size: u8,
        base_offset_size: u8,
        index_size: u8,
        primary_id: u32,
        items: Vec<ItemSpec>,
        idat: Option<Vec<u8>>,
        iref_children: Vec<Vec<u8>>,
    }

    fn sbox(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 8);
        out.extend_from_slice(&u32::try_from(body.len() + 8).unwrap().to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn write_width(buf: &mut Vec<u8>, width: u8, value: u64) {
        match width {
            0 => {}
            2 => buf.extend_from_slice(&u16::try_from(value).unwrap().to_be_bytes()),
            4 => buf.extend_from_slice(&u32::try_from(value).unwrap().to_be_bytes()),
            8 => buf.extend_from_slice(&value.to_be_bytes()),
            _ => panic!("unsupported field width"),
        }
    }

    /// A version-0 single-item-type reference box (`dimg`, `thmb`, ...) with
    /// 16-bit ids, matching the shapes `append_cdsc_reference` validates.
    fn ref_box(kind: &[u8; 4], from: u16, to: &[u16]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&from.to_be_bytes());
        body.extend_from_slice(&u16::try_from(to.len()).unwrap().to_be_bytes());
        for target in to {
            body.extend_from_slice(&target.to_be_bytes());
        }
        sbox(kind, &body)
    }

    fn build_heic(spec: &HeicSpec) -> Vec<u8> {
        let ftyp = {
            let mut body = Vec::new();
            body.extend_from_slice(b"heic");
            body.extend_from_slice(&0u32.to_be_bytes());
            body.extend_from_slice(b"mif1");
            sbox(b"ftyp", &body)
        };

        let build_meta = |mdat_data_start: u64| -> Vec<u8> {
            let hdlr = {
                let mut body = vec![0u8, 0, 0, 0, 0, 0, 0, 0];
                body.extend_from_slice(b"pict");
                body.extend_from_slice(&[0u8; 12]);
                body.push(0);
                sbox(b"hdlr", &body)
            };
            let pitm = if spec.primary_id <= u32::from(u16::MAX) {
                let mut body = vec![0u8, 0, 0, 0];
                body.extend_from_slice(&(spec.primary_id as u16).to_be_bytes());
                sbox(b"pitm", &body)
            } else {
                let mut body = vec![1u8, 0, 0, 0];
                body.extend_from_slice(&spec.primary_id.to_be_bytes());
                sbox(b"pitm", &body)
            };
            let iinf = {
                let mut body = vec![0u8, 0, 0, 0];
                body.extend_from_slice(&u16::try_from(spec.items.len()).unwrap().to_be_bytes());
                for item in &spec.items {
                    let mut entry = vec![item.infe_version, 0, 0, 0];
                    if item.infe_version == 3 {
                        entry.extend_from_slice(&item.item_id.to_be_bytes());
                    } else {
                        entry.extend_from_slice(&(item.item_id as u16).to_be_bytes());
                    }
                    entry.extend_from_slice(&0u16.to_be_bytes());
                    entry.extend_from_slice(&item.item_type);
                    entry.push(0);
                    if item.item_type == *b"mime" {
                        entry.extend_from_slice(b"application/rdf+xml\0");
                        entry.push(0);
                    }
                    body.extend_from_slice(&sbox(b"infe", &entry));
                }
                sbox(b"iinf", &body)
            };
            let iloc = {
                let mut body = vec![spec.iloc_version, 0, 0, 0];
                body.push((spec.offset_size << 4) | spec.length_size);
                body.push((spec.base_offset_size << 4) | spec.index_size);
                if spec.iloc_version == 2 {
                    body.extend_from_slice(&u32::try_from(spec.items.len()).unwrap().to_be_bytes());
                } else {
                    body.extend_from_slice(&u16::try_from(spec.items.len()).unwrap().to_be_bytes());
                }
                let mut cm0_cursor = mdat_data_start;
                for item in &spec.items {
                    if spec.iloc_version == 2 {
                        body.extend_from_slice(&item.item_id.to_be_bytes());
                    } else {
                        body.extend_from_slice(&(item.item_id as u16).to_be_bytes());
                    }
                    if spec.iloc_version > 0 {
                        body.extend_from_slice(&u16::from(item.construction_method).to_be_bytes());
                    }
                    body.extend_from_slice(&0u16.to_be_bytes());
                    let (base, offset, length) = if item.construction_method == 0 {
                        let base = cm0_cursor;
                        cm0_cursor += item.data.len() as u64;
                        (base, 0u64, item.data.len() as u64)
                    } else {
                        (0u64, item.offset, item.length)
                    };
                    write_width(&mut body, spec.base_offset_size, base);
                    body.extend_from_slice(&1u16.to_be_bytes());
                    if spec.iloc_version > 0 {
                        write_width(&mut body, spec.index_size, 0);
                    }
                    write_width(&mut body, spec.offset_size, offset);
                    write_width(&mut body, spec.length_size, length);
                }
                sbox(b"iloc", &body)
            };
            let iref = if spec.iref_children.is_empty() {
                None
            } else {
                let mut body = vec![0u8, 0, 0, 0];
                for child in &spec.iref_children {
                    body.extend_from_slice(child);
                }
                Some(sbox(b"iref", &body))
            };
            let idat = spec.idat.as_ref().map(|data| sbox(b"idat", data));

            let mut meta_body = vec![0u8, 0, 0, 0];
            meta_body.extend_from_slice(&hdlr);
            meta_body.extend_from_slice(&pitm);
            meta_body.extend_from_slice(&iinf);
            meta_body.extend_from_slice(&iloc);
            if let Some(iref) = &iref {
                meta_body.extend_from_slice(iref);
            }
            if let Some(idat) = &idat {
                meta_body.extend_from_slice(idat);
            }
            sbox(b"meta", &meta_body)
        };

        let placeholder = build_meta(0);
        let mdat_data_start = ftyp.len() as u64 + placeholder.len() as u64 + 8;
        let meta = build_meta(mdat_data_start);
        assert_eq!(
            meta.len(),
            placeholder.len(),
            "meta size must not depend on offset values"
        );

        let mut mdat_data = Vec::new();
        for item in &spec.items {
            if item.construction_method == 0 {
                mdat_data.extend_from_slice(&item.data);
            }
        }

        let mut file = Vec::new();
        file.extend_from_slice(&ftyp);
        file.extend_from_slice(&meta);
        if !mdat_data.is_empty() {
            file.extend_from_slice(&sbox(b"mdat", &mdat_data));
        }
        file
    }

    /// Resolve a construction-method-0 item's payload bytes through kei's raw
    /// iloc parser. Used to prove image bytes survive a rewrite and that
    /// shifted offsets still point at the same data.
    fn resolve_item_data(bytes: &[u8], item_id: u32) -> Vec<u8> {
        let (_, _, iloc, _, _, _) = find_meta_layout(bytes).expect("meta layout");
        let layout = parse_iloc(bytes, iloc).expect("iloc");
        let item = layout
            .items
            .iter()
            .find(|item| item.item_id == item_id)
            .expect("item present");
        assert_eq!(
            item.construction_method, 0,
            "resolver handles construction method 0 only"
        );
        let mut out = Vec::new();
        for extent in &item.extents {
            let start = usize::try_from(item.base_offset + extent.offset).unwrap();
            let length = usize::try_from(extent.length).unwrap();
            out.extend_from_slice(&bytes[start..start + length]);
        }
        out
    }

    /// Extract the XMP packet through kei's own writer-side parser rather than
    /// the mp4-atom read path, so item ids above `u16` and iloc version 2 are
    /// covered too.
    fn extract_xmp_raw(bytes: &[u8]) -> Option<Vec<u8>> {
        let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(bytes).ok()?;
        let (location, _, _, _, _, _, _) =
            locate_xmp(bytes, iinf, iloc, iref, primary_item_id).ok()?;
        let location = location?;
        Some(bytes[location.extent_start..location.extent_start + location.extent_length].to_vec())
    }

    fn meta_child_bytes(bytes: &[u8], kind: &[u8; 4]) -> Option<Vec<u8>> {
        let (meta, _, _, _, _, prefix) = find_meta_layout(bytes).ok()?;
        let children = scan_raw_boxes(bytes, meta.body_start() + prefix, meta.end()).ok()?;
        children
            .iter()
            .find(|child| child.kind == *kind)
            .map(|child| bytes[child.start..child.end()].to_vec())
    }

    #[test]
    fn parse_iloc_rejects_item_count_exceeding_body() {
        let mut body = vec![0u8, 0, 0, 0];
        body.push(0x44);
        body.push(0x00);
        body.extend_from_slice(&u16::MAX.to_be_bytes());
        let boxed = sbox(b"iloc", &body);
        let raw = parse_raw_box(&boxed, 0).expect("iloc header");
        let err = parse_iloc(&boxed, raw).unwrap_err();
        assert!(
            matches!(err, HeifError::InvalidLayout { reason } if reason.contains("item count")),
            "a count with no backing bytes must be refused, got {err:?}"
        );
    }

    #[test]
    fn parse_iloc_rejects_extent_count_exceeding_body() {
        let mut body = vec![0u8, 0, 0, 0];
        body.push(0x44);
        body.push(0x00);
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&u16::MAX.to_be_bytes());
        let boxed = sbox(b"iloc", &body);
        let raw = parse_raw_box(&boxed, 0).expect("iloc header");
        let err = parse_iloc(&boxed, raw).unwrap_err();
        assert!(
            matches!(err, HeifError::InvalidLayout { reason } if reason.contains("extent count")),
            "an extent count with no backing bytes must be refused, got {err:?}"
        );
    }

    #[test]
    fn rewrite_xmp_inserts_into_grid_dimg_layout() {
        let grid_header = vec![0u8, 0, 1, 1, 0, 0x40, 0, 0x40];
        let spec = HeicSpec {
            iloc_version: 1,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"grid",
                    infe_version: 2,
                    construction_method: 1,
                    data: Vec::new(),
                    offset: 0,
                    length: grid_header.len() as u64,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..40).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 3,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (40u8..96).collect(),
                    offset: 0,
                    length: 0,
                },
            ],
            idat: Some(grid_header.clone()),
            iref_children: vec![ref_box(b"dimg", 1, &[2, 3])],
        };
        let input = build_heic(&spec);
        assert!(is_heif_content(&input));

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("grid layout insertion");

        assert_eq!(resolve_item_data(&input, 2), resolve_item_data(&output, 2));
        assert_eq!(resolve_item_data(&input, 3), resolve_item_data(&output, 3));
        assert_eq!(
            meta_child_bytes(&input, b"idat"),
            meta_child_bytes(&output, b"idat"),
            "construction-method-1 grid header must survive"
        );
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        let dimg = ref_box(b"dimg", 1, &[2, 3]);
        assert!(
            output.windows(dimg.len()).any(|window| window == dimg),
            "existing dimg reference must be copied unchanged"
        );
        let (xmp_id, _) = xmp_and_primary_item_ids(&output);
        assert!(cdsc_references(&output).contains(&(xmp_id, 1)));

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("grid layout idempotent");
        assert_eq!(
            again, output,
            "repeated grid rewrite must be byte-idempotent"
        );
    }

    const AUX_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>";
    const PRIMARY_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='1'/></rdf:RDF></x:xmpmeta>";

    /// The shape iOS writes for an HDR or portrait capture: a `grid` primary
    /// over `hvc1` tiles, an auxiliary image, and one XMP item per image. Only
    /// the packet whose `cdsc` names the primary is the photograph's.
    fn apple_multi_xmp_spec(xmp_items: Vec<ItemSpec>, cdsc: Vec<Vec<u8>>) -> HeicSpec {
        let grid_header = vec![0u8, 0, 1, 1, 0, 0x40, 0, 0x40];
        let mut items = vec![
            ItemSpec {
                item_id: 1,
                item_type: *b"grid",
                infe_version: 2,
                construction_method: 1,
                data: Vec::new(),
                offset: 0,
                length: grid_header.len() as u64,
            },
            ItemSpec {
                item_id: 2,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..40).collect(),
                offset: 0,
                length: 0,
            },
            ItemSpec {
                item_id: 3,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (40u8..96).collect(),
                offset: 0,
                length: 0,
            },
            ItemSpec {
                item_id: 4,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (96u8..128).collect(),
                offset: 0,
                length: 0,
            },
        ];
        items.extend(xmp_items);
        let mut iref_children = vec![ref_box(b"dimg", 1, &[2, 3])];
        iref_children.extend(cdsc);
        HeicSpec {
            iloc_version: 1,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items,
            idat: Some(grid_header),
            iref_children,
        }
    }

    fn xmp_item(item_id: u32, packet: &[u8]) -> ItemSpec {
        ItemSpec {
            item_id,
            item_type: *b"mime",
            infe_version: 2,
            construction_method: 0,
            data: packet.to_vec(),
            offset: 0,
            length: 0,
        }
    }

    /// A HEIF file shaped like an iOS HDR capture, for tests outside this
    /// module: a `grid` primary over `hvc1` tiles, an auxiliary image carrying
    /// its own XMP packet, and optionally the photograph's packet bound to the
    /// primary by `cdsc`.
    pub(crate) fn apple_multi_xmp_heic(primary_xmp: Option<&[u8]>, aux_xmp: &[u8]) -> Vec<u8> {
        let mut xmp_items = vec![xmp_item(5, aux_xmp)];
        let mut cdsc = vec![ref_box(b"cdsc", 5, &[4])];
        if let Some(packet) = primary_xmp {
            xmp_items.push(xmp_item(6, packet));
            cdsc.push(ref_box(b"cdsc", 6, &[1, 3]));
        }
        build_heic(&apple_multi_xmp_spec(xmp_items, cdsc))
    }

    #[test]
    fn rewrite_xmp_targets_the_xmp_item_describing_the_primary_image() {
        let spec = apple_multi_xmp_spec(
            vec![xmp_item(5, AUX_XMP), xmp_item(6, PRIMARY_XMP)],
            vec![ref_box(b"cdsc", 5, &[4]), ref_box(b"cdsc", 6, &[1, 3])],
        );
        let input = build_heic(&spec);

        assert_eq!(
            extract_xmp_raw(&input).as_deref(),
            Some(PRIMARY_XMP),
            "the writer must resolve the packet the cdsc binds to the primary"
        );
        assert_eq!(
            extract_xmp_strict(&input).unwrap().as_deref(),
            Some(PRIMARY_XMP),
            "the reader must resolve the same packet as the writer, or a merge \
             would move auxiliary metadata onto the photograph"
        );

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("multi-XMP rewrite");

        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        assert_eq!(
            resolve_item_data(&output, 5),
            AUX_XMP,
            "the auxiliary image's packet must survive byte-for-byte"
        );
        for tile in [2, 3, 4] {
            assert_eq!(
                resolve_item_data(&input, tile),
                resolve_item_data(&output, tile),
                "item {tile} payload must be untouched"
            );
        }
        validate_rewrite_preserves_non_xmp_items(&input, &output)
            .expect("every item but the selected XMP packet must be preserved");

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("multi-XMP idempotent");
        assert_eq!(again, output, "repeated rewrite must be byte-idempotent");
    }

    /// The synthetic multi-XMP tests above control the item map precisely, but
    /// they are still kei's own idea of an Apple file. This drives the same
    /// selection rule through a genuine iOS 17.6.1 HDR capture: a `grid`
    /// primary over six tiles, a gain map, and two XMP packets, one describing
    /// the photograph and one describing the gain map. Choosing the wrong one
    /// moves the user's rating onto the gain map.
    #[test]
    fn rewrite_xmp_targets_the_primary_packet_in_a_real_apple_hdr_capture() {
        const PRIMARY_XMP_ITEM: u32 = 9;
        const GAIN_MAP_XMP_ITEM: u32 = 11;
        let input = include_bytes!("../../tests/data/apple-hdr-gainmap.heic");

        let gain_map_packet = resolve_item_data(input, GAIN_MAP_XMP_ITEM);
        let selected = extract_xmp_raw(input).expect("the capture carries XMP");
        assert_eq!(
            selected,
            resolve_item_data(input, PRIMARY_XMP_ITEM),
            "the writer must resolve the packet bound to the primary image"
        );
        assert_ne!(
            selected, gain_map_packet,
            "the gain map's packet must never answer for the photograph"
        );
        assert_eq!(
            extract_xmp_strict(input).unwrap().as_deref(),
            Some(selected.as_slice()),
            "the reader must resolve the same packet as the writer"
        );

        let mut output = Vec::new();
        rewrite_xmp(input, MATRIX_XMP, &mut output).expect("real Apple HDR rewrite");

        // The capture's packet is far larger than the replacement, so this is
        // the in-place branch: the extent is reused and the tail padded.
        let written = extract_xmp_raw(&output).expect("rewritten capture carries XMP");
        assert_eq!(written.trim_ascii_end(), MATRIX_XMP);
        assert_eq!(
            written.len(),
            selected.len(),
            "a shrinking packet must reuse the existing extent"
        );
        assert_eq!(
            resolve_item_data(&output, GAIN_MAP_XMP_ITEM),
            gain_map_packet,
            "the gain map's packet must survive byte-for-byte"
        );
        validate_rewrite_preserves_non_xmp_items(input, &output)
            .expect("every tile, the gain map, and Exif must be preserved");

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("real Apple HDR idempotent");
        assert_eq!(again, output, "repeated rewrite must be byte-idempotent");
    }

    #[test]
    fn rewrite_xmp_inserts_when_every_packet_describes_an_auxiliary_image() {
        // Twelve of sixty-one files in a real library carry one auxiliary
        // packet, and eight carry several. In both the photograph has no XMP,
        // so selecting one would overwrite a gain map's metadata with the
        // photograph's.
        for (xmp_items, cdsc) in [
            (vec![xmp_item(5, AUX_XMP)], vec![ref_box(b"cdsc", 5, &[4])]),
            (
                vec![xmp_item(5, AUX_XMP), xmp_item(6, AUX_XMP)],
                vec![ref_box(b"cdsc", 5, &[4]), ref_box(b"cdsc", 6, &[3])],
            ),
        ] {
            let ids: Vec<u32> = xmp_items.iter().map(|item| item.item_id).collect();
            let spec = apple_multi_xmp_spec(xmp_items, cdsc);
            let input = build_heic(&spec);

            assert!(
                extract_xmp_strict(&input).unwrap().is_none(),
                "an auxiliary packet must not answer for the primary image"
            );

            let mut output = Vec::new();
            rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("insertion beside auxiliary XMP");

            assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
            for id in &ids {
                assert_eq!(
                    resolve_item_data(&output, *id),
                    AUX_XMP,
                    "auxiliary packet {id} must survive byte-for-byte"
                );
            }
            let (xmp_id, _) = xmp_and_primary_item_ids(&output);
            assert!(
                cdsc_references(&output).contains(&(xmp_id, 1)),
                "the inserted item must be bound to the primary image"
            );
            validate_rewrite_preserves_non_xmp_items(&input, &output)
                .expect("insertion must preserve every existing item");

            let mut again = Vec::new();
            rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("insertion idempotent");
            assert_eq!(again, output, "repeated rewrite must be byte-idempotent");
        }
    }

    #[test]
    fn rewrite_xmp_refuses_unproved_tone_map_insertion() {
        let tone_map = || ItemSpec {
            item_id: 7,
            item_type: *b"tmap",
            infe_version: 2,
            construction_method: 0,
            data: (0u8..24).collect(),
            offset: 0,
            length: 0,
        };

        let mut insertion = apple_multi_xmp_spec(Vec::new(), Vec::new());
        insertion.items.push(tone_map());
        let input = build_heic(&insertion);
        assert!(
            extract_xmp_strict(&input).unwrap().is_none(),
            "the fixture must carry no XMP so the writer takes the insertion path"
        );
        let mut output = Vec::new();
        let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
        assert!(
            result.is_err(),
            "insertion must fail until the relevant tone map can be selected from real relationship evidence"
        );
        assert!(output.is_empty(), "a refused rewrite must emit no bytes");

        let mut replacement = apple_multi_xmp_spec(
            vec![xmp_item(6, PRIMARY_XMP)],
            vec![ref_box(b"cdsc", 6, &[1])],
        );
        replacement.items.push(tone_map());
        let input = build_heic(&replacement);
        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output)
            .expect("replacing an existing packet must still be allowed");
        assert_eq!(
            extract_xmp_raw(&output)
                .as_deref()
                .map(<[u8]>::trim_ascii_end),
            Some(MATRIX_XMP)
        );
        assert_eq!(
            cdsc_references(&output),
            cdsc_references(&input),
            "replacing a packet must leave every existing reference alone"
        );
        assert_eq!(
            resolve_item_data(&output, 7),
            (0u8..24).collect::<Vec<u8>>(),
            "the tone-mapped image must survive byte-for-byte"
        );
    }

    #[test]
    fn rewrite_xmp_rejects_external_data_references() {
        for construction_method in 0..=2 {
            let spec = HeicSpec {
                iloc_version: 1,
                offset_size: 4,
                length_size: 4,
                base_offset_size: 4,
                index_size: 0,
                primary_id: 1,
                items: vec![ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method,
                    data: if construction_method == 0 {
                        (0u8..32).collect()
                    } else {
                        Vec::new()
                    },
                    offset: 0,
                    length: 0,
                }],
                idat: None,
                iref_children: Vec::new(),
            };
            let mut input = build_heic(&spec);
            let (_, _, iloc, _, _, _) = find_meta_layout(&input).expect("meta layout");
            let data_reference_pos = iloc.body_start() + 12;
            input[data_reference_pos..data_reference_pos + 2].copy_from_slice(&1u16.to_be_bytes());
            assert!(
                parse_iloc(&input, iloc).is_err(),
                "construction method {construction_method} must reject an external data reference"
            );

            let mut output = Vec::new();
            let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
            assert!(
                result.is_err(),
                "the writer cannot resolve or preserve externally referenced item bytes"
            );
            assert!(output.is_empty(), "a refused rewrite must emit no bytes");
        }
    }

    #[test]
    fn rewrite_xmp_refuses_undecidable_xmp_item_maps() {
        // Two packets naming the primary, and two naming nothing at all. Both
        // shapes leave no evidence for choosing, so neither may be overwritten.
        let ambiguous = [
            (
                vec![xmp_item(5, AUX_XMP), xmp_item(6, PRIMARY_XMP)],
                vec![ref_box(b"cdsc", 5, &[1]), ref_box(b"cdsc", 6, &[1])],
            ),
            (vec![xmp_item(5, AUX_XMP), xmp_item(6, PRIMARY_XMP)], vec![]),
        ];
        for (xmp_items, cdsc) in ambiguous {
            let spec = apple_multi_xmp_spec(xmp_items, cdsc);
            let input = build_heic(&spec);

            let mut output = Vec::new();
            let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
            assert!(
                result.is_err(),
                "an undecidable item map must not be written"
            );
            assert!(output.is_empty(), "a refused rewrite must emit nothing");
            assert!(
                extract_xmp_strict(&input).is_err(),
                "the reader must refuse whatever the writer refuses"
            );
        }
    }

    #[test]
    fn rewrite_xmp_preserves_construction_method_two_item() {
        let spec = HeicSpec {
            iloc_version: 1,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..48).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 2,
                    data: Vec::new(),
                    offset: 0,
                    length: 16,
                },
            ],
            idat: None,
            iref_children: vec![ref_box(b"dimg", 1, &[2])],
        };
        let input = build_heic(&spec);

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("construction-method-2 insertion");

        assert_eq!(resolve_item_data(&input, 1), resolve_item_data(&output, 1));
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));

        let before = parse_iloc(&input, find_meta_layout(&input).unwrap().2).unwrap();
        let after = parse_iloc(&output, find_meta_layout(&output).unwrap().2).unwrap();
        let item_before = before.items.iter().find(|item| item.item_id == 2).unwrap();
        let item_after = after.items.iter().find(|item| item.item_id == 2).unwrap();
        assert_eq!(
            (
                item_before.construction_method,
                item_before.base_offset,
                item_before.extents[0].offset,
                item_before.extents[0].length,
            ),
            (
                item_after.construction_method,
                item_after.base_offset,
                item_after.extents[0].offset,
                item_after.extents[0].length,
            ),
            "construction-method-2 item must be copied without shifting"
        );
    }

    #[test]
    fn rewrite_xmp_synthesises_iref_when_absent() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..32).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..16).collect(),
                    offset: 0,
                    length: 0,
                },
            ],
            idat: None,
            iref_children: Vec::new(),
        };
        let input = build_heic(&spec);
        assert!(
            find_meta_layout(&input).unwrap().3.is_none(),
            "fixture must have no iref"
        );

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("insertion synthesises an iref");

        assert_eq!(resolve_item_data(&input, 1), resolve_item_data(&output, 1));
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        assert!(
            find_meta_layout(&output).unwrap().3.is_some(),
            "insertion must synthesise an iref"
        );
        let (xmp_id, primary) = xmp_and_primary_item_ids(&output);
        assert_eq!(primary, 1);
        let references = cdsc_references(&output);
        assert!(
            references.contains(&(xmp_id, 1)),
            "synthesised iref must carry a cdsc from the XMP item to the primary"
        );
        assert_eq!(
            references,
            vec![(xmp_id, 1)],
            "a synthesised cdsc must describe only the proven primary image"
        );

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("idempotent synthesised iref");
        assert_eq!(again, output);
    }

    #[test]
    fn rewrite_xmp_refuses_item_data_overlapping_meta() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![ItemSpec {
                item_id: 1,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..32).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let mut input = build_heic(&spec);
        let (meta, _, iloc, _, _, _) = find_meta_layout(&input).unwrap();
        let layout = parse_iloc(&input, iloc).unwrap();
        let base_pos = iloc.body_start() + layout.items[0].base_offset_pos.unwrap();
        let inside_meta = u32::try_from(meta.start + 40).unwrap();
        input[base_pos..base_pos + 4].copy_from_slice(&inside_meta.to_be_bytes());

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
        assert!(
            result.is_err(),
            "an item whose data overlaps the meta box must be refused"
        );
        assert!(output.is_empty(), "a refused rewrite must not emit bytes");
    }

    #[test]
    fn rewrite_xmp_refuses_existing_xmp_data_overlapping_meta() {
        let mut input = Vec::new();
        rewrite_xmp(
            include_bytes!("../../tests/data/sample.heic"),
            b"<x:xmpmeta>existing packet padding</x:xmpmeta>",
            &mut input,
        )
        .expect("seed existing XMP");
        let (meta, iinf, iloc, iref, primary_item_id, prefix_size) =
            find_meta_layout(&input).unwrap();
        let children = scan_raw_boxes(&input, meta.body_start() + prefix_size, meta.end()).unwrap();
        let iprp = children
            .iter()
            .find(|child| child.kind == *b"iprp")
            .expect("sample HEIC iprp");
        let (_, xmp_item_id, _, _, _, _, _) =
            locate_xmp(&input, iinf, iloc, iref, primary_item_id).unwrap();
        let layout = parse_iloc(&input, iloc).unwrap();
        let xmp_item = layout
            .items
            .iter()
            .find(|item| Some(item.item_id) == xmp_item_id)
            .expect("seeded XMP iloc item");
        let inside_iprp = u64::try_from(iprp.body_start() + 16).unwrap();
        let iloc_body = &mut input[iloc.body_start()..iloc.end()];
        if let Some(base_offset_pos) = xmp_item.base_offset_pos {
            write_uint(iloc_body, base_offset_pos, layout.base_offset_size, 0).unwrap();
        }
        write_uint(
            iloc_body,
            xmp_item.extents[0]
                .offset_pos
                .expect("XMP extent offset field"),
            layout.offset_size,
            inside_iprp,
        )
        .unwrap();

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, b"<x:xmpmeta>short</x:xmpmeta>", &mut output);

        assert!(
            matches!(result, Err(HeifError::InvalidLayout { reason }) if reason.contains("overlaps the meta box")),
            "an existing XMP extent inside meta must be refused, got {result:?}"
        );
        assert!(output.is_empty(), "a refused rewrite must not emit bytes");
    }

    #[test]
    fn validate_rewrite_rejects_changed_non_xmp_payload_and_opaque_meta() {
        let input = include_bytes!("../../tests/data/sample.heic");
        let mut rewritten = Vec::new();
        rewrite_xmp(input, MATRIX_XMP, &mut rewritten).expect("sample HEIC rewrite");
        validate_rewrite_preserves_non_xmp_items(input, &rewritten)
            .expect("writer output must preserve protected bytes");

        let (_, _, iloc, _, _, _) = find_meta_layout(&rewritten).unwrap();
        let layout = parse_iloc(&rewritten, iloc).unwrap();
        let image = layout
            .items
            .iter()
            .find(|item| item.item_id == 1)
            .expect("primary image item");
        let image_start = usize::try_from(
            image.base_offset + image.extents.first().expect("image extent").offset,
        )
        .unwrap();
        let mut changed_payload = rewritten.clone();
        changed_payload[image_start] ^= 1;
        assert!(
            validate_rewrite_preserves_non_xmp_items(input, &changed_payload).is_err(),
            "changed image payload must fail validation"
        );

        let (meta, _, _, _, _, prefix_size) = find_meta_layout(&rewritten).unwrap();
        let children =
            scan_raw_boxes(&rewritten, meta.body_start() + prefix_size, meta.end()).unwrap();
        let iprp = children
            .iter()
            .find(|child| child.kind == *b"iprp")
            .expect("sample HEIC iprp");
        let mut changed_meta = rewritten.clone();
        changed_meta[iprp.body_start()] ^= 1;
        assert!(
            validate_rewrite_preserves_non_xmp_items(input, &changed_meta).is_err(),
            "changed opaque meta bytes must fail validation"
        );
    }

    #[test]
    fn extract_exif_tiff_bytes_resolves_sample_item() {
        let tiff = extract_exif_tiff_bytes(include_bytes!("../../tests/data/sample.heic"))
            .expect("sample HEIC item map")
            .expect("sample HEIC Exif item");
        assert!(
            tiff.starts_with(b"MM\0*") || tiff.starts_with(b"II*\0"),
            "resolved Exif item must start with a TIFF header"
        );
    }

    fn heic_with_exif_items(exif_items: &[(u32, &[u8])], iref_children: Vec<Vec<u8>>) -> Vec<u8> {
        let mut items = vec![
            ItemSpec {
                item_id: 1,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..32).collect(),
                offset: 0,
                length: 0,
            },
            ItemSpec {
                item_id: 4,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (32u8..48).collect(),
                offset: 0,
                length: 0,
            },
        ];
        items.extend(exif_items.iter().map(|(item_id, data)| ItemSpec {
            item_id: *item_id,
            item_type: *b"Exif",
            infe_version: 2,
            construction_method: 0,
            data: data.to_vec(),
            offset: 0,
            length: 0,
        }));
        build_heic(&HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items,
            idat: None,
            iref_children,
        })
    }

    #[test]
    fn multiple_exif_items_do_not_block_xmp_rewrite() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..32).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"Exif",
                    infe_version: 2,
                    construction_method: 0,
                    data: b"\0\0\0\0MM\0*".to_vec(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 3,
                    item_type: *b"Exif",
                    infe_version: 2,
                    construction_method: 0,
                    data: b"\0\0\0\0II*\0".to_vec(),
                    offset: 0,
                    length: 0,
                },
            ],
            idat: None,
            iref_children: vec![ref_box(b"cdsc", 2, &[1]), ref_box(b"cdsc", 3, &[1])],
        };
        let input = build_heic(&spec);
        let mut output = Vec::new();

        assert!(
            extract_exif_tiff_bytes(&input).is_err(),
            "several Exif items naming the primary image are ambiguous"
        );
        rewrite_xmp(&input, MATRIX_XMP, &mut output)
            .expect("multiple Exif items must not block an unrelated XMP rewrite");
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
    }

    #[test]
    fn exif_probe_selects_item_associated_with_primary_image() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..32).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"Exif",
                    infe_version: 2,
                    construction_method: 0,
                    data: b"\0\0\0\0MM\0*".to_vec(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 3,
                    item_type: *b"Exif",
                    infe_version: 2,
                    construction_method: 0,
                    data: b"\0\0\0\0II*\0".to_vec(),
                    offset: 0,
                    length: 0,
                },
            ],
            idat: None,
            iref_children: vec![ref_box(b"cdsc", 3, &[1])],
        };
        let input = build_heic(&spec);

        assert_eq!(
            extract_exif_tiff_bytes(&input).unwrap().as_deref(),
            Some(b"II*\0".as_slice())
        );
    }

    #[test]
    fn exif_probe_ignores_lone_item_associated_with_an_auxiliary_image() {
        let input = heic_with_exif_items(&[(2, b"\0\0\0\0MM\0*")], vec![ref_box(b"cdsc", 2, &[4])]);

        assert!(
            extract_exif_tiff_bytes(&input).unwrap().is_none(),
            "an auxiliary image's Exif item must not answer for the primary image"
        );
    }

    #[test]
    fn exif_probe_accepts_lone_unattributed_item() {
        let input = heic_with_exif_items(&[(2, b"\0\0\0\0MM\0*")], vec![ref_box(b"dimg", 1, &[4])]);

        assert_eq!(
            extract_exif_tiff_bytes(&input).unwrap().as_deref(),
            Some(b"MM\0*".as_slice()),
            "a lone unattributed Exif item remains compatible with ordinary HEIF files"
        );
    }

    #[test]
    fn exif_probe_rejects_dangling_association() {
        let input = heic_with_exif_items(&[(2, b"\0\0\0\0MM\0*")], vec![ref_box(b"cdsc", 2, &[5])]);

        assert!(
            extract_exif_tiff_bytes(&input).is_err(),
            "a dangling association cannot prove whether Exif belongs to the primary image"
        );
    }

    #[test]
    fn rewrite_xmp_refuses_zero_sized_child_box() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![ItemSpec {
                item_id: 1,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..32).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let mut input = build_heic(&spec);
        let (_, iinf, _, _, _, _) = find_meta_layout(&input).unwrap();
        let entries = scan_raw_boxes(&input, iinf.body_start() + 6, iinf.end()).unwrap();
        let infe_start = entries[0].start;
        input[infe_start..infe_start + 4].copy_from_slice(&0u32.to_be_bytes());

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
        assert!(
            result.is_err(),
            "a box with no explicit size cannot be appended after and must be refused"
        );
        assert!(output.is_empty(), "a refused rewrite must not emit bytes");
    }

    #[test]
    fn rewrite_xmp_inserts_with_item_ids_above_u16() {
        let big_id = 70_000u32;
        let spec = HeicSpec {
            iloc_version: 2,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: big_id,
            items: vec![ItemSpec {
                item_id: big_id,
                item_type: *b"hvc1",
                infe_version: 3,
                construction_method: 0,
                data: (0u8..64).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let input = build_heic(&spec);
        assert!(is_heif_content(&input));

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("item ids above u16 insertion");

        assert_eq!(
            resolve_item_data(&input, big_id),
            resolve_item_data(&output, big_id)
        );
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        let (xmp_id, primary) = xmp_and_primary_item_ids(&output);
        assert!(
            xmp_id > u32::from(u16::MAX),
            "new item id must exceed u16 to exercise infe v3 and iloc v2"
        );
        assert_eq!(primary, big_id);
        assert!(cdsc_references(&output).contains(&(xmp_id, big_id)));
    }

    #[test]
    fn rewrite_xmp_inserts_with_base_offset_only_iloc() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 0,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![ItemSpec {
                item_id: 1,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..48).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let input = build_heic(&spec);

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("base-offset-only insertion");

        assert_eq!(resolve_item_data(&input, 1), resolve_item_data(&output, 1));
        assert_eq!(
            extract_xmp_raw(&output).as_deref(),
            Some(MATRIX_XMP),
            "the new XMP item must resolve through the base offset, not offset 0"
        );

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("base-offset-only idempotent");
        assert_eq!(again, output);
    }
}
