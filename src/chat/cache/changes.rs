//! Exact, bounded reads of one original Changes item. Paths are display data;
//! the source key and ingestion-owned file index identify the patch.

use super::SourceKey;
use crate::chat::{ChangeKind, Role, ToolStatus};
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILES: usize = 128;

/// An explicit shortfall is never represented as a complete shortened patch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unavailable {
    MetadataTooLarge { limit: usize },
    SourceTooLarge { limit: usize },
    InvalidMetadata,
    IncompleteIndex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub role: Role,
    pub status: ToolStatus,
    pub at: Option<u64>,
    pub started_at: Option<u64>,
    /// Original counts, captured before presentation metadata can be shortened.
    pub total_files: usize,
    pub total_bytes: usize,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub unavailable: Option<Unavailable>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub file_index: usize,
    pub path: String,
    pub kind: ChangeKind,
    pub source_len: usize,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestQuery {
    pub source: SourceKey,
    pub offset: usize,
    /// File count; serialized metadata also obeys the source page byte budget.
    pub limit: usize,
    pub expected_epoch: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub source: SourceKey,
    pub epoch: u64,
    pub metadata: Metadata,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub files: Vec<File>,
    /// Nonzero only when metadata could not be indexed exactly. A terminal
    /// manifest still states how many original files it cannot describe.
    pub omitted_files: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchQuery {
    pub source: SourceKey,
    pub file_index: usize,
    pub offset: usize,
    pub limit: usize,
    pub expected_epoch: u64,
    /// Fixed original file length from its manifest; mandatory even at offset0.
    pub until: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchPage {
    pub source: SourceKey,
    pub epoch: u64,
    pub file: File,
    pub offset: usize,
    pub next_offset: usize,
    pub text: String,
    pub complete: bool,
}

/// One displayed slot may contain only a slice of the original file patch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePart {
    pub display_slot: usize,
    pub file_index: usize,
    pub start: usize,
    pub end: usize,
    pub source_len: usize,
}

/// Generated alongside the native window; callers never parse item ID suffixes
/// or infer original file identity from a possibly repeated path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub item_id: String,
    pub source: SourceKey,
    pub epoch: u64,
    pub metadata: Metadata,
    pub files: Vec<FilePart>,
    /// Bounded resident placeholders have no original byte range.
    pub replacement: bool,
}
