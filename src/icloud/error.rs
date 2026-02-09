use thiserror::Error;

#[derive(Error, Debug)]
pub enum ICloudError {
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Photo library not finished indexing")]
    IndexingNotFinished,
    #[error(
        "iCloud service not activated ({code}): {reason}\n\n\
         This usually means one of:\n  \
         1. Advanced Data Protection (ADP) is enabled, which blocks third-party iCloud access.\n     \
            → Disable ADP in Settings > Apple Account > iCloud > Advanced Data Protection,\n     \
            or enable \"Access iCloud Data on the Web\" (Settings > Apple Account > iCloud).\n  \
         2. iCloud setup is incomplete.\n     \
            → Log into https://icloud.com/ and finish setting up your iCloud service."
    )]
    ServiceNotActivated { code: String, reason: String },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
