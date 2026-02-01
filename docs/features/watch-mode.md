# Watch Mode

Watch mode runs icloudpd-rs as a long-lived process, repeating the download cycle at a fixed interval.

## Usage

```sh
# Sync every hour
icloudpd-rs -u my@email.address -d /photos --watch-with-interval 3600
```

## How It Works

After completing a full download pass (enumerate → filter → download), the process sleeps for the specified number of seconds, then starts a new pass. This repeats indefinitely until the process is killed.

Each cycle performs a full enumeration from the API. Files that already exist locally are skipped based on filename presence.

## Current Limitations

Watch mode works but has several known gaps:

- **No signal handling** — Ctrl+C or SIGTERM kills the process immediately, potentially leaving orphaned `.part` files or corrupt session state
- **No album refresh** — Albums are fetched once at startup and reused across all iterations. New albums created in iCloud won't appear until restart
- **No session refresh** — The authentication session is not re-validated between cycles. If the session expires, downloads will fail until the process is restarted
- **No systemd/launchd integration** — No PID file, no `sd_notify`, no readiness signaling

These are planned improvements.

## Related Flags

- [`--watch-with-interval`](../cli/watch-with-interval.md)
