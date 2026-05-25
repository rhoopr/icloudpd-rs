# Removed compatibility surfaces

There are no active warning windows. Removed flags, commands, env vars, and TOML keys fail during CLI parsing or config validation.

## v0.20.0

The v0.20 cleanup removed the v0.13-v0.19 compatibility aliases. Current
migration guidance lives in [docs/v0.20-migration.md](docs/v0.20-migration.md).

Removed examples include:

- durable sync flags such as `--download-dir`, `--album`, `--smart-folder`,
  `--library`, `--skip-videos`, `--threads`, and `--watch-with-interval`
- durable sync env mirrors such as `KEI_DOWNLOAD_DIR`, `KEI_ALBUM`,
  `KEI_LIBRARY`, `KEI_THREADS`, and `KEI_WATCH_WITH_INTERVAL`
- old TOML aliases such as `[filters].album`, `[filters].exclude_albums`,
  `[filters].library`, `[download].threads_num`, and `[metrics].port`
- hidden command aliases such as `kei setup`, `kei retry-failed`, and
  `kei reset-state`
