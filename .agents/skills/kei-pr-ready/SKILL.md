---
name: kei-pr-ready
description: Validate a kei branch before review without publishing or changing it. Use when asked whether a kei change is PR-ready, to run the repository gate, to check review readiness, to validate a branch or diff, or to identify missing tests, consumers, documentation, safety evidence, or user-flow checks before PR preparation.
---

# Validate a kei branch

Produce an evidence-backed readiness verdict. Keep the pass read-only unless the
user separately asks to fix a failure. Do not commit, push, open a pull request,
or change GitHub state.

## Preflight

1. Read repository-root `AGENTS.md`, then the relevant sections of
   `CONTRIBUTING.md`, `docs/architecture.md`, and `tests/README.md`.
2. Run `just agent-status`.
3. Resolve the remote default branch, falling back to `origin/main`, then run
   `just review-scope BASE=<resolved-base>`.
4. Record the exact base, merge base, and head. Inspect committed, staged,
   unstaged, and untracked changes. Return a not-ready verdict if unrelated
   work prevents an attributable review.
5. Create a coverage ledger for every changed `(status, path)` entry, including
   tests, docs, workflows, deletions, and renames. Mark each entry `reviewed` or
   `skipped` with a concrete reason. Keep unchanged callers and invariant owners
   in a separate context list. Never silently omit a changed entry.

## Classify impact

Map every changed behavior to the owner and direct consumers in
`docs/architecture.md`. Apply each matching row in its change-impact checklist.
At minimum, classify:

- provider identity, enumeration, checkpoint, or retry behavior
- SQLite schema, query, durable key, sentinel, or serialization
- file, path, publication, import, or metadata behavior
- CLI, configuration, machine output, service, or documentation
- tests, scripts, workflows, packaging, or release behavior

Trace shared types and literal consumers with `rg`. Identify the applicable
safety-contract IDs and focused scenario slices. Do not treat the round-trip
gate or a passing unit test as proof that all consumers were traced.

## Validate

1. Run the smallest matching focused test or `just test scenario NAME`.
2. For behavior changes, prefer at least one test through the production call
   graph and cover the relevant failure, retry, interruption, or boundary case.
3. For CLI or user-flow changes, run the changed command and inspect help,
   Docker, service, Homebrew, and documentation consumers where applicable.
4. For schema, primary-key, sentinel, durable-key, or serialization changes,
   search every old literal and prove migration and round-trip behavior.
5. Run `just gate`.
6. Investigate every failure. Use bounded output and
   `just agent-failure-summary` when a retained full-test log is relevant.

Treat gate, CI, and full-test results as proof only when the record shows that
the run stayed on one head and that head matches the reviewed head. Label
results from the same branch at another head as `STALE` and results from
another branch as `OTHER BRANCH`.

Do not run live tests unless the changed behavior requires them. Follow
`tests/README.md`, run live suites single-threaded, and preserve rate-limit and
shared-session constraints.

## Review and report

Self-review the complete end-state diff for unrelated scope, missed consumers,
weak evidence, accidental API changes, unnecessary abstractions, and stale
documentation.

Report:

- base, head, and reviewed diff scope
- merge base and validation provenance
- changed-file coverage ledger and separate context-file list
- impact classification, owners, safety contracts, and scenario slices
- exact validation commands and results
- unresolved failures, skipped checks, risks, and missing evidence
- final verdict: ready or not ready

Never report ready when a required check failed, was skipped without a
documented reason, could not be attributed to the reviewed head, or any changed
file is unaccounted for.
