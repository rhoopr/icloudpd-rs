# `--log-level`

Controls log output verbosity.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --log-level info
```

## Details

- **Default**: `debug`
- **Type**: Enum
- **Values**: `debug`, `info`, `error`

| Level | Output |
|-------|--------|
| `debug` | Everything — API calls, response details, per-file progress |
| `info` | Downloads, skips, authentication events |
| `error` | Only failures |
