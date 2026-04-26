#![no_main]

// Inlines src/icloud/photos/cloudkit.rs without the rest of the kei build
// graph. cloudkit.rs has no `crate::` / `super::` imports, so it compiles
// standalone in the fuzz binary's crate. The harness exercises every
// response type that consumes Apple-controlled JSON, so libfuzzer-discovered
// inputs hit serde_json + every custom deserializer (`string_or_null`, the
// `flatten` on ZoneId, etc.).

#[allow(dead_code, reason = "harness doesn't read every deserialized field")]
#[path = "../../src/icloud/photos/cloudkit.rs"]
mod cloudkit;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<cloudkit::ZoneListResponse>(data);
    let _ = serde_json::from_slice::<cloudkit::QueryResponse>(data);
    let _ = serde_json::from_slice::<cloudkit::BatchQueryResponse>(data);
    let _ = serde_json::from_slice::<cloudkit::Record>(data);
    let _ = serde_json::from_slice::<cloudkit::ChangesDatabaseResponse>(data);
    let _ = serde_json::from_slice::<cloudkit::ChangesZoneResponse>(data);
    let _ = serde_json::from_slice::<cloudkit::ChangesZoneResult>(data);
    let _ = serde_json::from_slice::<cloudkit::ZoneId>(data);
});
