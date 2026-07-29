# AGENTS.md

kei is a Rust CLI that syncs iCloud Photos media to local storage.

Read first:

1. `CONTRIBUTING.md` for the contributor workflow and review contract.
2. `docs/architecture.md` for owners, flows, and data-safety invariants.
3. `tests/README.md` before changing or attributing test behavior.

## Discovery

Before editing:

1. Run `cargo check` for code work.
2. Find the owning area in `docs/architecture.md`.
3. Use `rg` for the exact symbol, command, flag, SQL name, config key, error,
   durable key, or serialization shape.
4. Read the owner module and direct callers.
5. Trace every consumer before changing a shared type, trait, CLI/API surface,
   schema, primary key, sentinel, token, path, or serialized value.

Keep policy in its owner. Do not put sync policy in path rendering or CloudKit
parsing in the download pipeline.

## Safety

- User media and metadata must not be lost, corrupted, truncated, overwritten,
  or silently discarded.
- Preserve `.part` writes, SHA-256 verification, no-overwrite publication,
  parent-directory fsync, and durable state finalization.
- Local file or metadata rewrites require an explicit user-controlled option.
- Preserve provider checkpoint gates across interruption, retry, partial work,
  config drift, and stale planning.
- Unknown provider identity is durable retry evidence, not permission to
  delete or forget work.
- Keep provider quirks and record parsing in `src/icloud/`.
- Do not remove trust-boundary validation or data-loss guards as cleanup.

## Implementation

- Make the smallest complete change in the fewest owning files.
- Reuse or delete before adding abstractions.
- Prefer the standard library, native platform/SQLite support, and existing
  dependencies, in that order.
- Avoid speculative knobs, compatibility, helpers, factories, builders, and
  one-implementation traits.
- Prefer borrowing to cloning and enums to boolean mode arguments.
- Use newtypes for IDs, paths, units, tokens, and other easy-to-mix values.
- Use named constants for sentinels, magic values, timeouts, and retry limits.
- Use typed errors and `?`.
- Do not use `unwrap` in production code.
- Use `expect` only for a proven same-flow invariant, and state the invariant
  in the message.
- Do not block the async runtime. Use async I/O or `spawn_blocking`.
- Keep internal APIs `pub(crate)` unless public or test-harness precedent
  requires `pub`.
- Add `#[must_use]` when ignoring a result can lose state, safety, or a
  user-visible decision.
- Keep provider, state, filesystem, and policy layers separate.

## Tests and completion

- Every behavior change needs a focused test.
- Prefer at least one test through the real production call graph.
- Put unit tests near their owner and integration tests under `tests/`.
- Assert concrete success, failure, retry, interruption, and boundary behavior.
- Investigate every failure before changing product code or calling it
  unrelated.
- Use check-only formatter and linter commands unless explicitly asked to
  autofix.

Finish code changes with:

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
```

Run focused tests while iterating and `just gate` for PR-ready work. Run live
tests single-threaded only when the change needs them; follow
`tests/README.md`.

For CLI changes, run the changed command and inspect Docker CMD, systemd
`ExecStart`, Homebrew formula paths, help, and docs where applicable.

For schema, primary-key, sentinel, durable-key, or serialization changes,
search every old literal and prove migration and round-trip behavior.

Update `docs/architecture.md` when ownership, a documented flow, or an
invariant changes.

## Restrictions

Get explicit approval before:

- Deleting production code or tests
- Changing a public CLI/API contract
- Adding a dependency
- Pushing, opening a pull request, or changing `main`

Never use star imports, unexplained `#[allow(...)]`, `git add -A`, `git add .`,
`git commit --amend`, or `sudo`.

Documentation uses direct language and no em dashes.
