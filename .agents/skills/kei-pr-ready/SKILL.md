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
3. Confirm the current branch, upstream, working-tree state, and comparison
   base. Prefer the remote default branch; fall back to `origin/main` when the
   default cannot be resolved.
4. Inspect committed, staged, and unstaged changes. Return a not-ready verdict
   if unrelated work prevents an attributable review.

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

Do not run live tests unless the changed behavior requires them. Follow
`tests/README.md`, run live suites single-threaded, and preserve rate-limit and
shared-session constraints.

## Review and report

Self-review the complete end-state diff for unrelated scope, missed consumers,
weak evidence, accidental API changes, unnecessary abstractions, and stale
documentation.

Report:

- base, head, and reviewed diff scope
- impact classification, owners, safety contracts, and scenario slices
- exact validation commands and results
- unresolved failures, skipped checks, risks, and missing evidence
- final verdict: ready or not ready

Never report ready when a required check failed, was skipped without a
documented reason, or could not be attributed to the reviewed diff.
