#![no_main]

// Inlines src/icloud/photos/enc.rs (binary plist + base64 metadata decoders).
// These layered parsers (JSON → string → base64 → plist) are the most
// likely place for crashes on malicious server responses; both decode paths
// are exercised below.

#[allow(dead_code, reason = "harness doesn't call every helper in the module")]
#[path = "../../src/icloud/photos/enc.rs"]
mod enc;

use base64::Engine;
use libfuzzer_sys::fuzz_target;
use serde_json::{json, Value};

fuzz_target!(|data: &[u8]| {
    // Path 1: treat the input as a full JSON document and run every decoder
    // over the resulting Value. Exercises the JSON-shape edge cases.
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        let _ = enc::decode_string(&value, "captionEnc");
        let _ = enc::decode_string(&value, "extendedDescEnc");
        let _ = enc::decode_keywords(&value);
        let _ = enc::decode_location(&value, "locationEnc");
        let _ = enc::decode_location(&value, "locationV2Enc");
        let _ = enc::decode_location_with_fallback(&value);
    }

    // Path 2: treat the input as raw bytes that an attacker controls inside
    // a base64 ENCRYPTED_BYTES payload, and synthesize the JSON wrapper
    // around it. This drives the fuzzer toward the plist parser without
    // making it discover the JSON wrapper structure itself.
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let wrapped = json!({
        "captionEnc":     {"value": b64.clone(), "type": "ENCRYPTED_BYTES"},
        "keywordsEnc":    {"value": b64.clone(), "type": "ENCRYPTED_BYTES"},
        "locationEnc":    {"value": b64.clone(), "type": "ENCRYPTED_BYTES"},
        "locationV2Enc":  {"value": b64,         "type": "ENCRYPTED_BYTES"},
    });
    let _ = enc::decode_string(&wrapped, "captionEnc");
    let _ = enc::decode_keywords(&wrapped);
    let _ = enc::decode_location(&wrapped, "locationEnc");
    let _ = enc::decode_location(&wrapped, "locationV2Enc");
    let _ = enc::decode_location_with_fallback(&wrapped);
});
