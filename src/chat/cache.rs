//! Bounded, requester-specific reads of original cached prose. Native display
//! part IDs are aliases, never source IDs: callers retain the original key.
pub mod raw;
use serde::{Deserialize, Serialize};

pub const PAGE_BYTES: usize = 256 * 1024;
pub const SOURCE_BYTES: usize = 32 * 1024 * 1024;
/// A rewrite revision is valid only within the exact ingestion Store that
/// issued it. Execed actor replacement must never reuse an older read proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceEpoch {
    pub incarnation: uuid::Uuid,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    pub agent_id: Option<String>,
    pub source_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceMetadata {
    pub role: super::Role,
    pub thinking: bool,
    pub phase: super::Phase,
    pub at: Option<u64>,
    pub started_at: Option<u64>,
}
impl SourceMetadata {
    pub(crate) fn from_item(item:&super::Item)->Option<Self>{
        let super::ItemContent::Text{thinking,phase,..}=&item.content else {return None;};
        Some(Self{role:item.role,thinking:*thinking,phase:*phase,at:item.at,started_at:item.started_at})
    }
}
/// Store-authored association between one visible item and original prose.
/// Provider IDs remain opaque even when they resemble a native part suffix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePart {
    pub item_id: String,
    pub source: SourceKey,
    pub start: usize,
    pub end: usize,
    pub source_len: usize,
    /// A bounded resident child placeholder represents no original bytes.
    pub replacement: bool,
    /// Changes only when the original source is replaced, never on append.
    pub epoch: SourceEpoch,
    pub metadata: SourceMetadata,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceQuery {
    pub source: SourceKey,
    pub offset: usize,
    pub limit: usize,
    pub expected_epoch: Option<SourceEpoch>,
    /// Prefix length from the first page. Appends do not extend this read.
    pub until: Option<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePage {
    pub source: SourceKey,
    pub epoch: SourceEpoch,
    /// Fixed prefix length for this read, rather than the latest streaming length.
    pub source_len: usize,
    pub offset: usize,
    pub next_offset: usize,
    pub text: String,
    pub complete: bool,
    /// Sampled with this page; the first page binds the frozen prefix metadata.
    pub metadata: SourceMetadata,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceError {
    NotFound,
    Unsupported,
    Changed,
    InvalidRange,
    TooLarge { source_len: usize, limit: usize },
    Transport(String),
}
impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("Original text is no longer cached"),
            Self::Unsupported => f.write_str("This cached item is not prose"),
            Self::Changed => f.write_str("Original text changed; restart the read"),
            Self::InvalidRange => f.write_str("Invalid source page boundary or limit"),
            Self::TooLarge { source_len, limit } => write!(f, "Original text is {source_len} bytes, exceeding the {limit} byte display limit"),
            Self::Transport(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for SourceError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chat::Command,model::TerminalId,terminal_ipc::{ClientRequest,ServerResponse}};
    #[test]
    fn source_queries_and_typed_replies_round_trip_on_matched_wire() {
        let source=SourceKey{agent_id:Some("child".into()),source_id:"original:native-part:1:text:0-8:0-0".into()};
        let epoch=SourceEpoch{incarnation:uuid::Uuid::new_v4(),revision:17};
        let query=SourceQuery{source:source.clone(),offset:4,limit:PAGE_BYTES,expected_epoch:Some(epoch),until:Some(8)};
        let chat_id=TerminalId::new();
        let request=ClientRequest::ChatCommand{chat_id,command:Command::LoadSource{query}};
        assert_eq!(postcard::from_bytes::<ClientRequest>(&postcard::to_stdvec(&request).unwrap()).unwrap(),request);
        let page=SourcePage{source,epoch,source_len:8,offset:4,next_offset:8,text:"text".into(),complete:true,metadata:SourceMetadata{role:crate::chat::Role::Assistant,thinking:false,phase:crate::chat::Phase::Final,at:Some(1),started_at:Some(1)}};
        for result in [Ok(page),Err(SourceError::NotFound),Err(SourceError::Unsupported),Err(SourceError::Changed),Err(SourceError::InvalidRange),Err(SourceError::TooLarge{source_len:SOURCE_BYTES+1,limit:SOURCE_BYTES}),Err(SourceError::Transport("closed".into()))] {
            let reply=ServerResponse::ChatSource{chat_id,result};
            assert_eq!(postcard::from_bytes::<ServerResponse>(&postcard::to_stdvec(&reply).unwrap()).unwrap(),reply);
        }
    }
}
