# Running kei as a service

`kei install` registers kei to start at boot and run continuously. This page explains what runs once the service is up, what each platform's artifact looks like, and how to inspect or control it from the platform's native tools.

For installing the service, see [install.md](install.md).

## What `kei service run` does

The command launched by every platform's service manager is `kei service run`. That's `kei sync` with one default change: when nothing else sets a watch interval, it polls iCloud once per day (86400 seconds) instead of running once and exiting.

Override the interval the same way you would for any sync:

- CLI flag in the config: `--watch-with-interval 3600` (every hour)
- Config file: `[watch] interval_seconds = 3600`
- Env var (Docker): `KEI_WATCH_WITH_INTERVAL=3600`

Resolution order is the standard CLI > env > TOML > default chain.

The worker handles 2FA, session refresh, and graceful shutdown the same way `kei sync --watch` does in the foreground. SIGINT/SIGTERM (Linux/macOS) and SCM stop requests (Windows) flush in-flight downloads, persist the sync state, and exit cleanly.

## Per-platform artifacts

### Linux (systemd)

The unit file is plain `[Service] ExecStart=/path/to/kei service run`. Per-user installs land at `~/.config/systemd/user/kei.service`; system-wide at `/etc/systemd/system/kei.service` with `User=` set to the operator account.

```sh
# View the unit
systemctl --user cat kei.service

# Live status
systemctl --user status kei.service

# Logs (follow)
journalctl --user -u kei.service -f

# Restart after a config change
systemctl --user restart kei.service
```

Drop the `--user` for system-wide installs.

The unit is `Type=notify` with `WatchdogSec=120`, so kei sends `sd_notify` readiness and watchdog pings; systemd manages the PID directly and no `PIDFile=` is set. `Restart=on-failure` with `RestartSec=10s` keeps the worker alive across crashes.

### macOS (launchd)

The plist is `~/Library/LaunchAgents/com.rhoopr.kei.plist`. It runs as your user and writes stdout/stderr to `~/Library/Logs/kei/stdout.log` and `~/Library/Logs/kei/stderr.log`.

```sh
# Confirm it's loaded
launchctl list com.rhoopr.kei

# Tail logs
tail -f ~/Library/Logs/kei/stderr.log

# Stop without uninstalling
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.rhoopr.kei.plist

# Start it again
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.rhoopr.kei.plist
```

`KeepAlive` is set, so launchd restarts the worker if it exits non-zero. `RunAtLoad` is true, so the worker starts the moment the agent loads (which means at every login, since LaunchAgents follow the GUI session).

### Windows (SCM)

The service name is `com.rhoopr.kei`. It runs as your Windows user account, with the password stored in LSA at install time.

```powershell
# Service summary
Get-Service com.rhoopr.kei

# Detailed view including binary path and account
sc.exe qc com.rhoopr.kei

# Start / stop without uninstalling
Start-Service com.rhoopr.kei
Stop-Service  com.rhoopr.kei

# SCM lifecycle events (start, stop, crash) under source "Service Control Manager"
Get-EventLog -LogName System -Source "Service Control Manager" -Newest 50 | Where-Object { $_.Message -like "*com.rhoopr.kei*" }
```

The service is configured `start= auto` so it runs at every boot. Failure actions: SCM restarts the worker up to three times with a 10-second delay between attempts; the failure counter resets after 24 hours of clean uptime.

kei's own log output (the `tracing` lines you see when running `kei sync` in a terminal) goes to stderr, which SCM discards. There is no first-class log-file destination configured by `kei install` on Windows in v0.14. To inspect kei's runtime behavior, run `kei sync` directly from PowerShell with the same flags rather than relying on the SCM-managed worker.

## Checking status

Two commands report on the service. Pick whichever fits the question.

### `kei status`

The first line of `kei status` is the cross-platform service summary. This is the right thing to script against:

```
Service: running (systemd user, pid 12345, since 2026-05-08 14:32 UTC)
```

Variants:

- `Service: not installed` -> `kei install` hasn't run.
- `Service: running (<backend>, pid <n>, since <utc>)` -> healthy.
- `Service: <state> (<backend>, ...)` -> registered but not currently running. `<state>` is the platform's own term (`inactive`, `failed`, `stopped`).
- `Service: installed (<backend>, <reason>)` -> registered, but kei couldn't reach the manager to ask for a state (typically a missing systemd user bus over SSH). Treat as "registered, status unknown".
- `Service: running in container (process supervisor: docker)` -> kei is inside a container; the container runtime is the supervisor.

The line is best-effort: a probe failure renders `Service: status unavailable` rather than blocking the rest of `kei status`.

### `kei service status`

The platform-tuned form. On Linux it surfaces the systemd `SubState` (`running`/`dead`/`failed`/...) for finer-grained debugging. On macOS and Windows it's similar to the line above. Use this when you want to see all the per-backend detail kei has, rather than the cross-platform summary.

## Updating after a kei upgrade

`brew upgrade rhoopr/kei/kei`, replacing the binary in `/usr/local/bin/`, or `cargo install kei` will all replace the binary in place. The unit/plist/SCM entry continues to point at the same path, so the next service restart picks up the new binary.

To force a restart:

- **Linux**: `systemctl --user restart kei.service`
- **macOS**: `launchctl kickstart -k gui/$(id -u)/com.rhoopr.kei`
- **Windows**: `Restart-Service com.rhoopr.kei`

If the binary moved (e.g. you switched from the homebrew install to a manually placed binary at a different path), run `kei uninstall && kei install` to re-render the artifact at the new path.

## Removing the service

`kei uninstall` removes only the platform artifact. State (DB, config, credentials) is preserved by default; pass `--purge` to wipe `~/.config/kei` along with the service entry. See [install.md](install.md) for the per-platform commands.
