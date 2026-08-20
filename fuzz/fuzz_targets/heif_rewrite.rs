#![no_main]

// Drives the byte-preserving HEIC XMP writer `rewrite_xmp` from
// `src/download/heif.rs` over arbitrary bytes.  `heif_atoms` and
// `heif_xmp_probe` cover the read path; this target mutates media at write
// time, the shape that made the old mp4-atom writer dangerous.  The harness
// asserts the writer's safety contract, not just that it doesn't crash: a
// rejected rewrite emits nothing, and an accepted rewrite keeps the container
// HEIF, round-trips the written packet, preserves every non-XMP item's bytes,
// and preserves opaque meta sub-boxes.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kei::__fuzz::heif_rewrite_xmp_preserves(data);
});
