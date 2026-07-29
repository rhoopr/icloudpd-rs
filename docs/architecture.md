# Architecture

kei is a Rust CLI that transfers iCloud Photos media and metadata to local
storage. This guide identifies the owner of each major decision and the
boundaries that protect user data.

Use it as a starting map. Read the owning module and its direct callers before
changing behavior.

## Design rules

- User media and metadata must not be lost, corrupted, truncated, overwritten,
  or silently discarded.
- Local file and metadata rewrites are opt-in.
- Provider-specific parsing and identity rules stay in the iCloud adapter.
- Sync policy stays in the sync and download orchestration layers.
- Path rendering does not decide whether an asset should sync.
- SQLite transitions and provider checkpoints must survive interruption.
- Prefer the smallest complete implementation in the owning module.

## Owners

| Area | Owner | Boundary |
|------|-------|----------|
| Process startup and dispatch | `src/lib.rs` | Starts the runtime, resolves bootstrap paths, configures logging, dispatches commands, and maps exit codes. |
| CLI shape | `src/cli.rs` | Defines clap arguments and parsing. It does not execute command behavior. |
| Runtime configuration | `src/config.rs` | Resolves TOML, environment, and command inputs into runtime policy. |
| Selection grammar | `src/selection.rs` | Parses album, smart-folder, library, exclusion, and unfiled selectors. |
| Sync/watch loop | `src/sync_loop.rs` | Owns authentication recovery, watch cadence, library/pass refresh, database pre-checks, and cycle-level reporting. |
| One sync cycle | `src/sync_cycle.rs` | Chooses source enumeration, reconciles config drift, dispatches each library, and advances or preserves provider checkpoints. |
| Library and pass planning | `src/commands/service.rs` | Resolves libraries, collection scope, album plans, smart folders, unfiled passes, and cross-zone hydration. |
| iCloud Photos adapter | `src/icloud/photos/` | Owns CloudKit records, queries, change streams, provider identity, albums, smart folders, and metadata decoding. |
| Download orchestration | `src/download/mod.rs` | Routes full, incremental, targeted-backfill, and durable-retry work and produces checkpoint evidence. |
| Asset planning | `src/download/planner.rs` | Applies filters, derives tasks, records dispatched pending work, and persists membership and identity mappings. |
| Streaming workers | `src/download/pipeline.rs` | Runs bounded producers and consumers, coordinates file transfer, metadata writes, adoption, and outcome aggregation. |
| File transfer | `src/download/file.rs` | Downloads, resumes, validates, and publishes one media file. |
| State finalization | `src/download/finalize.rs` | Persists downloaded or failed outcomes and retries deferred state writes. |
| Durable retry resolution | `src/download/retry.rs` | Revalidates pending provider identity and builds exact retry tasks. |
| Path rendering | `src/download/paths.rs` | Expands folder templates, normalizes names, and handles collision suffixes. |
| Metadata writing | `src/download/metadata.rs`, `src/download/heif.rs`, `src/download/metadata_rewrite.rs` | Probes and writes opt-in EXIF/XMP data and drains metadata-only retry markers. |
| SQLite state | `src/state/` | Owns schema migrations, role traits, asset state, membership snapshots, provider checkpoints, verification state, and sync runs. |
| Import-existing | `src/commands/import.rs` | Matches existing files to expected iCloud paths and adopts verified files into state. |
| Service integration | `src/service/` | Owns install, uninstall, status, service execution, and platform renderers. |
| Operator surfaces | `src/commands/status.rs`, `src/commands/doctor.rs`, `src/commands/manifest.rs` | Read local state for status, redacted diagnostics, and catalog export. |
| Reports and monitoring | `src/cycle_reporter.rs`, `src/report.rs`, `src/health.rs`, `src/metrics.rs`, `src/notifications.rs` | Converts cycle facts into reports, health, metrics, and notifications. |

## Main flows

### Command dispatch

```text
src/main.rs
  -> kei::main_inner
  -> src/lib.rs::run
  -> src/cli.rs
  -> command owner, service owner, or src/sync_loop.rs
```

The CLI requires a subcommand. `kei sync` enters the sync path. Commands such
as `status`, `doctor`, and `manifest` read local state without entering the
normal iCloud sync loop.

### Sync and provider checkpoints

```text
sync_loop::run_sync
  -> resolve configuration, credentials, libraries, and pass plans
  -> optional scoped changes/database pre-check
  -> sync_cycle::run_cycle
  -> download::download_photos_with_sync for each active library
  -> sync_cycle source checkpoint decision
  -> cycle reporting and watch control
```

The per-zone provider checkpoint and the scoped database pre-check token have
different gates:

- A zone checkpoint may advance after a transfer failure when the exact retry
  work is durable and enumeration/token proof is complete.
- The broader database pre-check token advances only after a clean aggregate
  cycle for the exact account, selection, filter, config, and selected-zone
  scope.

Eligibility-config drift preserves the active checkpoint while a complete
inventory and delta bridge build a replacement. Path-config drift preserves
provider checkpoints while local catalog paths are reconciled.

### Full and incremental enumeration

Full enumeration streams records/query results and gathers a provider token
from every active pass. Natural stream completion and usable, unanimous pass
tokens are the authoritative proof. Count probes and pagination differences
are diagnostics. Recoverable pass-token gaps can be retried in the same cycle.

Incremental enumeration consumes changes/zone events. It persists provider
identity mappings before applying created, soft-deleted, hard-deleted, or
hidden transitions. Album snapshots and smart folders may require targeted
refresh work before or alongside the incremental stream.

Recent and date-bounded runs may advance only when the producer proves the
bound did not truncate the stream.

### Download and publication

```text
PhotoAsset
  -> planner::TaskPlanner
  -> pipeline::run_download_pass
  -> file::download_file
  -> optional metadata_rewrite
  -> verified .part publication
  -> finalize downloaded or failed state
```

Only producer-dispatched work becomes pending through `upsert_seen`. Filtered
or skipped assets must not be left as retryable work unless a dedicated state
transition owns that result.

### Durable pending retry

Failed and pending rows are not recovered by replaying the entire provider
inventory. The retry owner:

1. Removes only work already proven source-deleted.
2. Resolves current provider records in targeted batches.
3. Uses durable asset/master mappings and checksum/size evidence for legacy
   identities.
4. Adopts an existing matching local file when safe.
5. Marks current filter exclusions as policy-excluded.
6. Persists unknown or transient verification state.
7. Queues exact unresolved asset/version/path tasks.

Unknown identity is not permission to delete or forget work.

### Import-existing

`src/commands/import.rs` shares configuration, selection, pass planning, and
path derivation with normal sync. It optionally compares remote prefix bytes,
hashes the local candidate, and calls `ImportStateStore::import_adopt`.
Size/mtime snapshots may skip later rehashing only while path, size, and mtime
still match.

## Data-safety invariants

### File landing

The media publication sequence is:

```text
write or resume .part
  -> validate response and content length
  -> validate expected size
  -> validate SHA-256 checksum
  -> validate content type and sniffed bytes
  -> apply configured pre-publish metadata
  -> publish without replacing an existing final path
  -> fsync the parent directory
  -> finalize SQLite state
```

A file may be safe on disk while its state write is still a cycle failure. Do
not weaken deferred state-write handling or infer that a visible final file
means the database transition succeeded.

### Checkpoint advancement

Preserve the zone checkpoint on:

- Dry-run
- Stale pass planning
- Session expiry or interruption
- Incomplete enumeration
- Non-durable state
- Missing, blank, mismatched, or otherwise blocked token proof

Transfer and metadata failures may use a newer checkpoint only when the
recovery work needed after that checkpoint is durable.

### Metadata

Provider metadata may be captured in SQLite without changing local media.
Embedding EXIF/XMP or writing sidecars requires explicit configuration.
Metadata failure markers must survive so a later run can retry metadata
without downloading the media again.

### State and serialization

Schema, primary-key, sentinel, durable-key, and serialization changes are
cross-cutting. Search every reader and writer, migrations, fixtures, reports,
status output, and round-trip tests before changing them.

## Change-impact checklist

| Change | Check |
|--------|-------|
| CLI command or flag | `src/cli.rs`, dispatch in `src/lib.rs`, help output, Docker, services, Homebrew, docs, CLI tests |
| TOML or runtime config | defaults, setup output, CLI precedence, hashes, persisted examples, docs |
| Selection or pass scope | list/sync/import parity, shared libraries, unknown names, unfiled scope, membership snapshots |
| Provider checkpoint | full and incremental proof, interruption, config drift, retry durability, scoped DB pre-check |
| SQLite schema/query | migration from every supported version, all readers/writers, status/report/manifest output, real SQLite tests |
| Provider record parsing | missing/malformed fields, shared zones, identity mapping, metadata capture, fixtures |
| File or path behavior | `.part`, checksum, no-overwrite publish, fsync, import compatibility, collision handling |
| Metadata writes | opt-in gate, pre-publish mutation, sidecars, retry markers, feature combinations |
| Service behavior | Linux, macOS, Windows, container defaults, status, install/uninstall renderers |
| Machine output | JSON/CSV shape, redaction, reports, health, metrics, downstream compatibility |

## Tests

- Unit tests live near their owner module.
- Cross-module and binary behavior lives under `tests/`.
- Live iCloud tests are ignored by default and run single-threaded.
- Shell suites cover crash, concurrency, state-machine, and container behavior.
- Fuzz targets cover parser and metadata trust boundaries.

See [the test guide](../tests/README.md) for the current suites and commands.

## Maintaining this guide

Update this file in the same pull request when a change:

- Moves an owning decision to another module
- Adds a command or cross-cutting state transition
- Changes file publication or metadata mutation
- Changes provider checkpoint or retry evidence
- Changes schema, durable keys, or serialization
- Changes the best test for an invariant

Keep the guide focused on stable ownership and safety. Source code remains the
final authority.
