# `--watch-with-interval`

Run continuously, repeating the download cycle every N seconds.

## Usage

```sh
# Sync every hour
icloudpd-rs -u my@email.address -d /photos --watch-with-interval 3600
```

## Details

- **Default**: None (single run)
- **Type**: Integer (seconds)

After completing a download pass, waits the specified number of seconds, then runs the entire download cycle again. Runs indefinitely until interrupted.

> [!WARNING]
> Current limitations in watch mode:
> - No signal handling — Ctrl+C may leave orphaned `.part` files
> - Albums are fetched once and not refreshed between iterations
> - Session is not re-validated between cycles
>
> These are planned improvements. See [Watch Mode](../features/watch-mode.md) for details.

## See Also

- [Watch Mode](../features/watch-mode.md)
