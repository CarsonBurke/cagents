//! Immutable original commands and explicit approval decisions. These are not
//! a transcript cache: every durable access requires launch-account evidence
//! and an exact provider-confirmed conversation identity.
use serde::{Deserialize, Serialize};

#[cfg(feature = "host")]
pub(crate) mod storage;

pub const ORIGINAL_PAGE_BYTES: usize = 256 * 1024;
pub const ORIGINAL_TEXT_BYTES: usize = 32 * 1024 * 1024;

/// A provider-confirmed value, including an explicit default/unsupported
/// semantic. An unknown setting must never be replaced by a current default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Setting<T> {
    Value(T),
    Default,
    Unsupported,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalSettings {
    pub model: Setting<String>,
    pub reasoning: Setting<String>,
    pub fast: Setting<bool>,
    pub permission: Setting<String>,
}
impl OriginalSettings {
    pub fn is_exact(&self) -> bool {
        !matches!(self.model, Setting::Unknown)
            && !matches!(self.reasoning, Setting::Unknown)
            && !matches!(self.fast, Setting::Unknown)
            && !matches!(self.permission, Setting::Unknown)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalAttachment {
    /// Original picker location for display only.
    pub original_path: std::path::PathBuf,
    /// Immutable private bytes used for both accepted dispatch and Retry.
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub sha256: [u8; 32],
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalPrompt {
    /// Actual provider user identity, never a locally generated dispatch ID.
    pub user_id: String,
    pub turn_id: String,
    pub settings: OriginalSettings,
    pub attachments: Vec<OriginalAttachment>,
    pub text_bytes: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalPage {
    pub original: OriginalPrompt,
    pub offset: usize,
    pub next_offset: usize,
    pub text: String,
    pub complete: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OriginalError {
    Unavailable(String),
    InvalidRange,
    TooLarge,
    Changed,
    Transport(String),
}

/// The action identity is scoped to its native parent/child conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalKey {
    pub agent_id: Option<String>,
    pub item_id: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Awaiting,
    Allowed,
    Denied,
}
