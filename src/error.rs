//! Error type of the crate. One variant per failure the Python launcher raises
//! as a distinct exception class, so a caller can match on them.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Camoufox executable is outdated, or no supported build is published.
    #[error("{0}")]
    UnsupportedVersion(String),
    /// A required GitHub release asset is missing.
    #[error("{0}")]
    MissingRelease(String),
    /// A downloaded asset does not match its expected sha256 digest.
    #[error("{0}")]
    CorruptedDownload(String),
    #[error("{0}")]
    UnsupportedArchitecture(String),
    #[error("{0}")]
    UnsupportedOs(String),
    /// A config value has the wrong type for its property.
    #[error("{0}")]
    InvalidPropertyType(String),
    #[error("Invalid addon path: {0}")]
    InvalidAddonPath(String),
    #[error("{0}")]
    InvalidLocale(String),
    #[error("Unknown territory: {0}")]
    UnknownTerritory(String),
    #[error("Unknown language: {0}")]
    UnknownLanguage(String),
    #[error("{0}")]
    UnknownIpLocation(String),
    #[error("{0}")]
    InvalidIp(String),
    #[error("{0}")]
    InvalidProxy(String),
    #[error("{0}")]
    NonFirefoxFingerprint(String),
    #[error("{0}")]
    InvalidOs(String),
    #[error("{0}")]
    VirtualDisplay(String),
    #[error("{0}")]
    CamoufoxNotInstalled(String),
    #[error("{0}")]
    ProfileDirectory(String),
    #[error("{0}")]
    FpgenModel(String),
    /// fpgen: the conditions leave no possible value.
    #[error("{0}")]
    RestrictiveConstraints(String),
    /// fpgen: a condition names a value or node the model does not have.
    #[error("{0}")]
    InvalidConstraints(String),
    #[error("{0}")]
    InvalidNode(String),
    /// A caller passed an option combination the launcher rejects.
    #[error("{0}")]
    Value(String),
    #[error("{0}")]
    Runtime(String),
    #[error("file not found: {0}")]
    FileNotFound(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    MaxMind(#[from] maxminddb::MaxMindDBError),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn value(msg: impl Into<String>) -> Self {
        Error::Value(msg.into())
    }

    pub fn runtime(msg: impl Into<String>) -> Self {
        Error::Runtime(msg.into())
    }
}
