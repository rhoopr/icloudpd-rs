# kei roadmap

This roadmap is directional. GitHub milestones and issues track committed
release work.

## Current focus

Stability and reliability, with backup confidence as the user-visible proof.

kei already does the hard parts of local iCloud Photos backup: resumable
downloads, safe file landing, checksum checks, stateful retries, service mode,
reports, and selected-library sync. The near-term job is to make normal sync,
interrupted-run recovery, reports, retry behavior, token safety, and support
paths feel rock solid.

New product surface should wait unless it directly proves or debugs reliability.

## Roadmap themes

### Backup confidence

Help users prove kei is syncing safely and recovering correctly.

Candidate work:

- Harden normal sync stability.
- Harden interrupted sync recovery.
- Improve failed-download visibility and retry behavior.
- Guard against unsafe sync-token advancement.
- Show active sync work in `kei status`.
- Detect missing or damaged local files during incremental sync.
- Add manifest export as reliability/support tooling once core sync and
  reporting feel solid.
- Add `kei doctor` or a redacted support bundle once core sync and reporting
  feel solid.

Success criteria:

- Normal sync is dependable.
- Interrupted sync resumes correctly.
- Failed downloads are visible and recoverable.
- Sync tokens advance only after safe, complete work.
- Users can see what kei is doing during a long unattended run.
- A damaged or missing local file can be found and repaired without guesswork.

### Headless operations

Make unattended operation easier to run and support.

Candidate work:

- Add a notification test command.
- Improve webhook and desktop notification flows.
- Make browser-based watch-mode 2FA easier.
- Document Grafana or Prometheus examples.
- Keep service output machine-readable and predictable.

Success criteria:

- Service users can tell whether kei is healthy without opening an interactive
  shell.
- Notification and metrics setup can be tested before waiting for a real sync
  event.
- Support reports explain the local installation without exposing credentials.

### Scale and sync efficiency

Reduce wasted work on large libraries.

Candidate work:

- Stream large incremental syncs.
- Plan full-library syncs before downloads where that avoids duplicate or stale
  work.
- Continue improving filtered sync follow-ups.
- Keep sync-token advancement tied to complete and safe enumeration.

Success criteria:

- Large libraries start producing useful work quickly.
- Selected albums, smart folders, and libraries avoid unnecessary whole-library
  scans where the provider model allows it.
- Incomplete enumeration does not advance tokens.

### Media fidelity

Preserve the media users expect.

Candidate work:

- Handle edited-photo naming better.
- Add safe HEIC, HEIF, and AVIF embedded metadata writes when the write path is
  proven safe.
- Improve cross-library deduplication.
- Research shared albums separately from shared libraries.

Success criteria:

- Originals, edits, Live Photos, RAW siblings, and metadata stay traceable.
- File rewrites remain opt-in and safe.
- Provider-specific media quirks stay documented.

### Destinations and providers

Grow beyond local iCloud-to-folder backup once the local catalog is strong.

Candidate work:

- Immich integration.
- Google Takeout import.
- Nextcloud or WebDAV.
- S3 or object storage.
- More packaging where it has clear user demand.

Success criteria:

- New sources or destinations reuse the local catalog instead of bypassing it.
- Destructive or upload workflows have explicit dry-run and safety gates.
- The file-backed backup remains useful even when a downstream destination fails.

### Destructive lifecycle workflows

Add cleanup only after backup confidence surfaces exist.

Candidate work:

- `kei prune --dry-run` for local deletion planning.
- A later explicit delete mode, if dry-run proves the model.
- iCloud-side deletion only as a separate later workflow.

Success criteria:

- The first cleanup slice is read-only.
- Users can see why each file is a candidate.
- No destructive behavior is bundled into normal sync.

## Near-term milestones

### v0.22 - Stability and reliability

Goal: make normal sync, recovery, reporting, and token safety feel rock solid.

User outcome: a user can trust routine sync, recover from interruption or
failure, and tell whether their local backup is safe.

Candidate work:

- Normal sync stability.
- Interrupted sync recovery.
- Failed-download visibility and retry behavior.
- State-write failure token-blocking regression coverage.
- Active sync work in `kei status`.
- Local missing or damaged file detection.
- Manifest export as reliability/support tooling, only after core sync,
  recovery, and reporting feel solid.
- First `kei doctor` slice only if it directly helps prove or debug
  reliability.

Out of scope:

- General catalog query.
- Local file deletion.
- iCloud-side deletion.
- Immich upload.
- Provider expansion.
- UI work.

Success criteria:

- Normal sync is stable enough to trust as the default path.
- Interrupted sync resumes correctly.
- Failed downloads are visible and recoverable.
- Sync tokens do not advance after unsafe or incomplete work.
- Status and reports explain whether the user is safe without debug logs.
- Missing or damaged local files are visible during routine maintenance.

### v0.23 - Headless operations

Goal: make service and Docker operation easier to observe.

User outcome: a headless user can test notifications, inspect health, and gather
support data without restarting into an interactive workflow.

Candidate work:

- Notification test command.
- Better webhook and desktop notification setup.
- Watch-mode 2FA browser page.
- Grafana or Prometheus example docs.

Out of scope:

- Provider expansion.
- Destructive cleanup.

Success criteria:

- Headless setups expose clear health and diagnostic signals.
- Notification setup can be tested on demand.

### v0.24 - Scale and sync efficiency

Goal: reduce unnecessary work on large or filtered libraries.

User outcome: large-library and filtered-sync users spend less time rescanning
media that cannot affect the next run.

Candidate work:

- Large incremental sync streaming.
- Full-library planning before download where it reduces waste.
- Filtered sync follow-ups.
- Targeted retry work if it did not fit v0.22.

Out of scope:

- Unsafe token advancement.
- Optimizations that hide enumeration errors.

Success criteria:

- Large syncs do useful work earlier.
- Token rules stay conservative when enumeration is incomplete.

### v0.25 - Media fidelity

Goal: improve how kei preserves media variants and metadata.

User outcome: edited media, metadata, duplicates, and shared media are easier to
understand and preserve.

Candidate work:

- Edited-photo filename improvements.
- Safe HEIC, HEIF, and AVIF metadata writes.
- Cross-library deduplication.
- Shared albums research.

Out of scope:

- Broad provider expansion.
- Default file rewrites.

Success criteria:

- Media variants stay traceable.
- Metadata write behavior remains explicit and safe.

### Later - Destinations and providers

Goal: use kei's local catalog as the base for new sources and destinations.

Candidate work:

- Immich.
- Google Takeout.
- Nextcloud or WebDAV.
- S3 or object storage.
- Destructive lifecycle workflows after read-only planning exists.

Success criteria:

- New workflows build on the backup and catalog model.
- Users can test and inspect changes before any destructive action.
