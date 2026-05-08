# Install kei

This guide covers installing kei on each supported platform and registering it as a long-running service. For the brief one-liners, see the project README. For what the service does once registered, see [service.md](service.md).

The flow is the same everywhere: install the binary, then run `kei install` to register the service.

## macOS

```sh
brew install rhoopr/kei/kei
kei install
```

`kei install` writes a per-user LaunchAgent to `~/Library/LaunchAgents/com.rhoopr.kei.plist` and bootstraps it via `launchctl`. The agent runs in your login session and writes logs to `~/Library/Logs/kei/`. There's no system-wide install on macOS in v0.14: `--system` returns an error pointing at the per-user form.

If the binary won't launch with a "cannot be opened because the developer cannot be verified" dialog, strip the gatekeeper quarantine bit:

```sh
xattr -dr com.apple.quarantine "$(which kei)"
```

`brew install` clears the bit on its own outputs, so this only matters if you grabbed a release tarball directly.

To uninstall:

```sh
kei uninstall            # remove the LaunchAgent
kei uninstall --purge    # also remove ~/.config/kei (state DB, config, credentials)
```

## Linux

Linux supports both per-user (default) and system-wide installs.

### Per-user (recommended)

```sh
kei install --user
```

Writes `~/.config/systemd/user/kei.service`, runs `systemctl --user daemon-reload`, and enables the unit so it starts immediately and on every login.

kei also attempts `loginctl enable-linger $USER` so the service keeps running after you log out. That call goes through polkit and may be denied; if it fails, the unit still works while you're logged in but stops when your session ends. Re-run `sudo loginctl enable-linger $USER` manually to enable lingering, or accept the per-session lifetime.

Verify with:

```sh
systemctl --user status kei.service
```

### System-wide

```sh
sudo kei install --system
```

Writes `/etc/systemd/system/kei.service`, sets `User=` to the user named in `$SUDO_USER`, and enables the unit. The service runs as that user (not root) but is supervised by the system manager, so it survives logout without linger.

Status check:

```sh
systemctl status kei.service
```

### Distro packaging

There's no apt/dnf repo yet. Use the static binary tarball from [Releases](https://github.com/rhoopr/kei/releases) and drop it into `/usr/local/bin/`, then run `kei install --user`.

### Uninstall

```sh
kei uninstall            # tries per-user first, then system (root needed for the latter)
kei uninstall --purge    # also wipes ~/.config/kei
```

## Windows

```powershell
kei install
```

Run this from an **elevated** PowerShell prompt (right-click PowerShell -> Run as administrator). Service Control Manager `CreateService` requires admin rights, and a non-elevated install fails with `Access is denied`.

`kei install` registers `com.rhoopr.kei` with SCM, set to run as your Windows user account. You'll be prompted for your account password during install: SCM stores it in the Local Security Authority (LSA) so the service can start under your identity at boot, and so it can read your iCloud password from Credential Manager. See [credential-storage.md](credential-storage.md) for how kei stores the iCloud password itself.

Per-user services aren't a Windows concept; `--user` and `--system` are both ignored. There is one install per machine.

Verify with:

```powershell
Get-Service com.rhoopr.kei
```

To uninstall:

```powershell
kei uninstall            # also from an elevated prompt
kei uninstall --purge    # also wipes %USERPROFILE%\.config\kei
```

## Docker

`kei install` is a no-op inside containers. Docker, Kubernetes, Podman, and similar runtimes already supervise the process; writing a launchd plist or systemd unit on the container's rootfs would never be invoked.

Existing `docker compose up -d` workflows are unchanged. The container's default `CMD` runs `kei sync --watch-with-interval 86400`, which gives the same once-a-day poll behavior `kei install` produces on a bare host. See the [Docker wiki page](https://github.com/rhoopr/kei/wiki/Docker) for compose examples and the [Synology guide](https://github.com/rhoopr/kei/wiki/Synology) for NAS setups.

## What `kei install` actually does

The command renders one platform-native artifact and hands it to the platform's service manager:

| Platform | Artifact | Manager |
|---|---|---|
| Linux (user)   | `~/.config/systemd/user/kei.service`     | `systemctl --user` |
| Linux (system) | `/etc/systemd/system/kei.service`         | `systemctl` |
| macOS          | `~/Library/LaunchAgents/com.rhoopr.kei.plist` | `launchctl` |
| Windows        | SCM entry `com.rhoopr.kei`                | Service Control Manager |

The service runs `kei service run`, which is `kei sync` with a watch interval default of once per day (86400 seconds). Override with the standard `--watch-with-interval` flag in your config. See [service.md](service.md) for what the worker does once running.

## Verifying the install

After `kei install` succeeds:

```sh
kei status
```

The first line is the `Service:` summary. On a bare host with the service running you'll see something like:

```
Service: running (systemd user, pid 12345, since 2026-05-08 14:32 UTC)
```

The exact backend label varies (`systemd user`, `systemd system`, `launchd user`, `windows scm`, or `running in container (process supervisor: docker)`). `not installed` means `kei install` hasn't run yet.

## Dry runs

`kei install --dry-run` writes the unit/plist to disk so you can inspect it but skips `daemon-reload` / `bootstrap` / `CreateService`. Useful when you want to see exactly what kei will register before committing. Windows `--dry-run` prints the SCM call it would make and skips the password prompt.
