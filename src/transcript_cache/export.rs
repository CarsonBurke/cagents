//! Cache export association/lease authority. Serialized records are evidence,
//! never a path-based read grant. Unknown legacy scopes remain unavailable.
use std::{fmt, io};
mod types;
mod fs;
pub(crate) use types::{AccountBindingData, BindingData, CacheIdentity, FileIdentity,
    SessionNamespace, SourceReadProofData, UnixPath};

#[derive(Debug)]
pub(crate) enum Error {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidMetadata(&'static str),
    Unsupported(&'static str),
    Incomplete(&'static str),
    Capacity,
    Revoked,
    Deadline,
    LegacyInventoryRequired,
}
pub(crate) type Result<T> = std::result::Result<T, Error>;
impl From<io::Error> for Error { fn from(error: io::Error) -> Self { Self::Io(error) } }
impl From<serde_json::Error> for Error { fn from(error: serde_json::Error) -> Self { Self::Json(error) } }
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Export ownership I/O failed: {error}"),
            Self::Json(error) => write!(f, "Export ownership record is invalid: {error}"),
            Self::InvalidMetadata(reason) | Self::Unsupported(reason) | Self::Incomplete(reason) => f.write_str(reason),
            Self::Capacity => f.write_str("Export ownership index reached its validated admission bound"),
            Self::Revoked => f.write_str("This export cache generation was revoked"),
            Self::Deadline => f.write_str("Export ownership admission deadline expired"),
            Self::LegacyInventoryRequired => f.write_str("Existing account exports need a complete legacy ownership inventory before admission"),
        }
    }
}
impl std::error::Error for Error {}
