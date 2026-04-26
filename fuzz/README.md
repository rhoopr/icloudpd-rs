# Fuzzing

Coverage-guided fuzz harnesses for the parsers in kei that consume
attacker-controllable input: CloudKit JSON from Apple, base64-wrapped
binary plists in `*Enc` metadata, and filename / path sanitization on
network-supplied strings.

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

## Future targets

`config.rs` (TOML) and `auth/responses.rs` (SRP responses) would be
high-value but both have intra-crate `use crate::...` lines that the
`#[path]` trick can't follow. Bringing them in needs either a small
refactor to remove the internal coupling or splitting kei into
`lib.rs` + `main.rs` so the fuzz crate can depend on it as a library.
