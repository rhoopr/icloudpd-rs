#![no_main]

// Mirrors `download/metadata.rs::probe_exif_heif`: takes raw HEIC bytes,
// extracts the XMP packet via mp4-atom, validates UTF-8, and parses the
// packet through xmp_toolkit. The pipeline is what kei runs on every
// HEIC it touches; the bug class this catches is one stage feeding the
// next something only the next stage rejects badly (e.g. extract returns
// bytes that the C++ parser walks off the end of).

#[allow(dead_code, reason = "harness only calls extract_xmp_bytes")]
#[path = "../../src/download/heif.rs"]
mod heif;

use libfuzzer_sys::fuzz_target;
use xmp_toolkit::XmpMeta;

fuzz_target!(|data: &[u8]| {
    let Some(xmp_bytes) = heif::extract_xmp_bytes(data) else {
        return;
    };
    let Ok(s) = std::str::from_utf8(&xmp_bytes) else {
        return;
    };
    let _ = s.parse::<XmpMeta>();
});
