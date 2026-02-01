# Download Pipeline

icloudpd-rs uses a streaming, concurrent download architecture designed for large libraries.

## Streaming Enumeration

Assets flow from the iCloud API directly into the download pipeline. The API is paginated, and while one page of results is being downloaded, the next page is being fetched. This eliminates the multi-minute startup delay that the Python version experiences on large libraries.

For a library with 100k+ photos, downloads begin within seconds of authentication completing.

## Concurrent Downloads

Multiple files can be downloaded simultaneously using [`--threads-num`](../cli/threads-num.md). Downloads use `buffer_unordered` — files complete in whatever order they finish, not the order they were started.

## Resumable Downloads

If a download is interrupted, the partially downloaded `.part` file is kept. On the next run, the existing bytes are verified and the download resumes from where it left off using HTTP Range requests. The final SHA256 checksum covers the entire file (existing + new bytes).

## Two-Phase Cleanup

After the main download pass, any failed downloads get a second attempt. The cleanup pass re-fetches CDN URLs from iCloud before retrying, which fixes failures caused by expired download URLs on large files.

## Checksum Verification

Every download is verified against the SHA256 checksum provided by iCloud. Both Apple's 32-byte raw format and 33-byte prefixed format are handled.

## Atomic Writes

Downloads write to a temporary `.part` file, then atomically rename to the final path. This prevents partial files from appearing in the download directory.

## Related Flags

- [`--threads-num`](../cli/threads-num.md)
- [`--max-retries`](../cli/max-retries.md)
- [`--retry-delay`](../cli/retry-delay.md)
- [`--dry-run`](../cli/dry-run.md)
