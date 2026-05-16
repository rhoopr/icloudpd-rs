# Deprecated

Flags, TOML keys, and subcommands scheduled for removal. Each deprecated item still works and logs a warning until its target version, then it's gone.

See [CHANGELOG.md](CHANGELOG.md) for the release where each item was first marked deprecated.

## v0.20.0

- `--exclude-album NAME` CLI flag and `KEI_EXCLUDE_ALBUM` env var. Use `--album '!NAME'` (the new inline-exclusion grammar; `!Foo` operates on the category default).
- `{album}` token in `--folder-structure` (CLI, TOML `[download] folder_structure`, `KEI_FOLDER_STRUCTURE` env). Use `--folder-structure-albums "{album}/..."` and keep `--folder-structure` for the unfiled pass. kei auto-migrates at startup with a paste-ready suggestion.
- `[filters] album` (single-string) TOML key. Use `[filters] albums = ["name"]` (array form).
- `[filters] exclude_albums` TOML key. Merge into `[filters] albums` as `"!name"` entries.
- `[filters] library` (single-string) TOML key. Use `[filters] libraries = ["name"]` (array form).
- Implicit `--album all` promotion from `{album}` in the folder-structure template. v0.13's new `--album all` default makes this redundant; explicit `--album all` (now the default) replaces it.
