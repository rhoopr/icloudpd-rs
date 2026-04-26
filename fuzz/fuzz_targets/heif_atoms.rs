#![no_main]

// Inlines src/download/heif.rs and exercises `extract_xmp_bytes`, which walks
// the ISO-BMFF atom tree of a HEIC file via mp4-atom to find the XMP packet.
// Bug #274 (panic on `uri ` infe items in iOS-17 HEICs) lived here, which is
// the case study for why this surface needs fuzz coverage: the parser sees
// arbitrary container bytes from iCloud-hosted assets.

#[allow(dead_code, reason = "harness only calls extract_xmp_bytes")]
#[path = "../../src/download/heif.rs"]
mod heif;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = heif::extract_xmp_bytes(data);
    let _ = heif::is_heif_content(data);
});
