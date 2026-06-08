# Contributing

Contributions are welcome. For anything beyond a small fix, open an issue first so we can talk through the approach before you write the code.

## Workflow

1. Fork the repo and create a branch from `main`.
2. Make your changes.
3. Run the pre-push gate:
   ```sh
   just gate
   ```
   This runs the local release-quality gate: formatting, clippy with default
   and no-default features, default and no-default tests, doc lints, lockfile
   fetch, `cargo audit`, workflow and script lint, contract markers, typos,
   and the serializer round-trip detector. Fail-fast.

   Without `just` installed, run the raw commands (see `justfile` for the full list):
   ```sh
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo clippy --all-targets --no-default-features -- -D warnings
   cargo test --all-features
   cargo test --no-default-features
   RUSTDOCFLAGS="-Dwarnings" cargo doc --no-deps --all-features
   cargo fetch --locked
   cargo audit --deny warnings
   python3 .github/scripts/check_workflow_hardening.py
   PYTHONPYCACHEPREFIX=/tmp/codex/kei/pycache python3 -m py_compile .github/scripts/*.py scripts/full-test/*.py
   bash -n scripts/check-contracts scripts/check-roundtrip-gate.sh scripts/full-test/*.sh scripts/just/*.sh scripts/notify-synology-photos.sh tests/shell/*.sh docker/entrypoint.sh
   scripts/check-contracts
   typos
   bash scripts/check-roundtrip-gate.sh
   ```
4. Open a pull request against `main`.

All changes go through PRs - no direct commits to `main`.

## Auth tests

Some tests (`tests/sync.rs`, `tests/state_auth.rs`, and
`tests/import_existing_live.rs`) hit the live iCloud API and need real
credentials. These are `#[ignore]` by default. See
[tests/README.md](tests/README.md) for setup, then:

```sh
just test live
```

You don't need to run auth tests for most changes. CI runs the offline suite on
every PR.

## Code style

- Rust 2021 edition, stable toolchain
- `cargo fmt` and `cargo clippy` must pass clean
- Warnings are errors in CI (`RUSTFLAGS="-Dwarnings"`)

## Logs and config snippets

Redact Apple IDs, passwords, session cookies, bearer tokens, webhook URLs, and
local paths that you don't want public before posting logs or config snippets.
Keep enough surrounding text for the failure to be readable.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
