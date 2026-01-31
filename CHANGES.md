# Behavioral Changes from Python icloud-photos-downloader

- `--recent N` stops fetching from the API after N photos instead of enumerating the entire library first
- `--until-found` removed — will be replaced by stateful incremental sync with local database
