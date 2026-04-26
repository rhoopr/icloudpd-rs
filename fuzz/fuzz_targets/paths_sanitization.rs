#![no_main]

// Inlines src/download/paths.rs (filename + path-component sanitization).
// These functions take untrusted strings (asset filenames, album names) and
// produce filesystem paths, so panics or path-traversal regressions are
// directly user-visible. The harness covers the byte-truncation boundary
// in `clean_filename`, the `..`-replacement + reserved-name logic in
// `sanitize_path_component`, and the strftime `{album}` token expansion.

#[allow(dead_code, reason = "harness doesn't call every helper in the module")]
#[path = "../../src/download/paths.rs"]
mod paths;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The functions all take `&str`; production callers receive UTF-8 from
    // CloudKit JSON, so non-UTF-8 bytes never reach them. Skip rather than
    // exercise an impossible path.
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };

    let _ = paths::clean_filename(s);
    let _ = paths::sanitize_path_component(s);
    let _ = paths::strip_python_wrapper(s);
    let _ = paths::remove_unicode_chars(s);

    // expand_album_token splits the input into (folder_structure, album)
    // halves at the first NUL so the fuzzer can drive both arguments from
    // one corpus entry. Without a split, libfuzzer would only ever see
    // (s, None), never the `{album}` substitution path.
    let (template, album) = match s.split_once('\0') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let _ = paths::expand_album_token(template, album);

    // add_dedup_suffix drives the size-suffix code path (rfind('.') edge
    // cases, capacity calculation). Sweep a few sizes including bounds.
    for size in [0u64, 1, u64::MAX] {
        let _ = paths::add_dedup_suffix(s, size);
    }
});
