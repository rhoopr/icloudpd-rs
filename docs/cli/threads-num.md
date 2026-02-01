# `--threads-num`

Number of concurrent download streams.

## Usage

```sh
icloudpd-rs -u my@email.address -d /photos --threads-num 4
```

## Details

- **Default**: `1`
- **Type**: Integer (minimum: 1)

Controls how many files are downloaded simultaneously using async concurrency. Higher values speed up downloads but increase bandwidth and memory usage.

The download pipeline streams assets from the API directly into concurrent download tasks using `buffer_unordered`, so increasing this value has immediate effect without additional memory overhead from buffering.

## See Also

- [Download Pipeline](../features/download-pipeline.md)
