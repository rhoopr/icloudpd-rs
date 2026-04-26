# Fuzzing

Coverage-guided fuzz harnesses for the parsers in kei that consume
attacker-controllable input: CloudKit JSON from Apple, base64-wrapped
binary plists in `*Enc` metadata, filename / path sanitization on
network-supplied strings, the HEIC atom tree on every downloaded
HEIC, and the XMP packet inside it (which goes through Adobe's
vendored C++ XMP Toolkit).

Run by hand. Not part of `just gate` or CI. The point is to leave a
target running locally for minutes-to-hours when poking at a parser, or
to repro a specific input.

## Prereqs

```sh
rustup install nightly
cargo install cargo-fuzz
```

cargo-fuzz needs nightly because libfuzzer-sys uses unstable sanitizer
flags. Production kei still builds on stable; the `fuzz/` crate is its
own package and isn't part of any workspace.

## Run

```sh
just fuzz list                          # what's available
just fuzz run cloudkit_json             # 60s default
just fuzz run cloudkit_json 600         # 10 minutes
```

Or directly:

```sh
cargo +nightly fuzz run cloudkit_json -- -max_total_time=60
```

A discovered crash lands in `fuzz/artifacts/<target>/crash-<hash>` with
a one-line repro:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<hash>
```

Corpus entries accumulate in `fuzz/corpus/<target>/`. Both directories
are gitignored.

## Targets

| Name | What it fuzzes | Source |
|------|----------------|--------|
| `cloudkit_json` | every CloudKit response type (`ZoneListResponse`, `QueryResponse`, `Record`, `ChangesDatabaseResponse`, etc.) via `serde_json::from_slice` | `src/icloud/photos/cloudkit.rs` |
| `enc_decoders` | `decode_string`, `decode_keywords`, `decode_location`, `decode_location_with_fallback` - both the JSON-shape path and the bplist-via-base64 path | `src/icloud/photos/enc.rs` |
| `paths_sanitization` | `clean_filename`, `sanitize_path_component`, `expand_album_token`, `add_dedup_suffix`, `strip_python_wrapper`, `remove_unicode_chars` | `src/download/paths.rs` |
| `heif_atoms` | `extract_xmp_bytes` and `is_heif_content` - mp4-atom walks the ISO-BMFF box tree on attacker-controlled HEIC bytes | `src/download/heif.rs` |
| `xmp_packet` | `XmpMeta::from_str` directly - drives Adobe's vendored XMP Toolkit (C++ via FFI) on arbitrary UTF-8 | `xmp_toolkit` crate |
| `heif_xmp_probe` | full `probe_exif_heif` pipeline: extract XMP from HEIC bytes, then parse it through xmp_toolkit | `src/download/heif.rs` + `xmp_toolkit` |

## Findings

`heif_atoms` and `heif_xmp_probe` both find an unbounded allocation in
`mp4-atom` within seconds: a 110-byte malformed input drives a
~21 GiB `malloc` and trips libfuzzer's OOM guard. Repros are checked
in at `fuzz/seeds/heif_atoms/regression-iloc-oom` and
`fuzz/seeds/heif_xmp_probe/regression-iloc-oom`. Run them through
the harness to reproduce:

```sh
cargo +nightly fuzz run heif_atoms fuzz/seeds/heif_atoms/regression-iloc-oom
```

The bug is upstream in `mp4-atom`'s box parser, not in kei's code.
kei calls `extract_xmp_bytes` synchronously on every HEIC during the
EXIF probe, so a hostile HEIC could DoS the sync. Worth either an
upstream PR or a wrapping size guard before the call.

Until that's fixed, `just fuzz run heif_atoms` will always trip the
seed on its first iteration. To fuzz past it (so libfuzzer can find
*other* bugs in the same target) raise the OOM limit:

```sh
cargo +nightly fuzz run heif_atoms -- -max_total_time=60 -rss_limit_mb=4096
```

## How the harnesses are wired

kei is a binary-only crate, so `[dependencies.kei]` from `fuzz/Cargo.toml`
won't link. Each harness pulls in its module with a `#[path]` attribute
and lists the external crates it needs in `fuzz/Cargo.toml` directly.
That keeps the production crate untouched. The cost is that any
parser whose module has internal `crate::` imports can't be fuzzed
this way without first being reshaped.

## Adding a target

1. Write `fuzz/fuzz_targets/<name>.rs` with a `#[path = "../../src/.../foo.rs"] mod foo;` and a `fuzz_target!` body that calls into `foo`.
2. Add `[[bin]]` entry to `fuzz/Cargo.toml` with `name = "<name>"`.
3. If the inlined module uses extra crates, add them to `[dependencies]` in `fuzz/Cargo.toml`.
4. `just fuzz build` to confirm it links.

Pick something whose input crosses a trust boundary - network response,
on-disk file, user config. A pure-internal helper isn't worth a fuzz
target.

## Seeds and regressions

`fuzz/seeds/<target>/` is checked in. cargo-fuzz reads it on `run`
alongside the auto-managed `corpus/<target>/`, so any input committed
there survives a `corpus/` wipe and is replayed on every run. Use it
for:

- Repros for known crashes (filename prefix `regression-`).
- Hand-crafted inputs that exercise a code path the fuzzer keeps
  missing.

`corpus/<target>/` and `artifacts/<target>/` stay gitignored because
they grow into the megabytes and aren't deterministic across runs.

## Future targets

These would need a real lib refactor (or selective `pub` widening) to
get inlined cleanly:

- `config.rs` TOML parser - has `use crate::password::SecretString` and `use crate::types::*`.
- `auth/responses.rs` SRP / 2FA response deser - has `use super::error::AuthError`.
- `icloud/photos/asset.rs`'s `PhotoAsset::from_records` - chains through 5 sibling modules plus `crate::state::AssetMetadata` and `crate::string_interner`. The inlining works but the stub set gets brittle.
- `state/types.rs` `FromStr` impls for `VersionSizeKey` / `AssetStatus` / `MediaType` - small surface, but they pull `crate::types::AssetVersionSize`.

The cleanest fix for all four is splitting kei into `lib.rs` + `main.rs`
so `fuzz/Cargo.toml` can `[dependencies] kei = { path = ".." }` and the
harnesses can call `kei::...` directly. That's a separate PR.
