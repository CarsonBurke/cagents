//! Bounded, requester-specific reads of original cached prose. Native display
//! part IDs are aliases, never source IDs: callers retain the original key.
use serde::{Deserialize, Serialize};

pub const PAGE_BYTES: usize = 256 * 1024;
pub const SOURCE_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    pub agent_id: Option<String>,
    pub source_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceQuery {
    pub source: SourceKey,
    pub offset: usize,
    pub limit: usize,
    pub expected_epoch: Option<u64>,
    /// Prefix length from the first page. Appends do not extend this read.
    pub until: Option<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePage {
    pub source: SourceKey,
    pub epoch: u64,
    /// Fixed prefix length for this read, rather than the latest streaming length.
    pub source_len: usize,
    pub offset: usize,
    pub next_offset: usize,
    pub text: String,
    pub complete: bool,
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
        let query=SourceQuery{source:source.clone(),offset:4,limit:PAGE_BYTES,expected_epoch:Some(17),until:Some(8)};
        let chat_id=TerminalId::new();
        let request=ClientRequest::ChatCommand{chat_id,command:Command::LoadSource{query}};
        assert_eq!(postcard::from_bytes::<ClientRequest>(&postcard::to_stdvec(&request).unwrap()).unwrap(),request);
        let page=SourcePage{source,epoch:17,source_len:8,offset:4,next_offset:8,text:"text".into(),complete:true};
        for result in [Ok(page),Err(SourceError::NotFound),Err(SourceError::Unsupported),Err(SourceError::Changed),Err(SourceError::InvalidRange),Err(SourceError::TooLarge{source_len:SOURCE_BYTES+1,limit:SOURCE_BYTES}),Err(SourceError::Transport("closed".into()))] {
            let reply=ServerResponse::ChatSource{chat_id,result};
            assert_eq!(postcard::from_bytes::<ServerResponse>(&postcard::to_stdvec(&reply).unwrap()).unwrap(),reply);
        }
    }
}
