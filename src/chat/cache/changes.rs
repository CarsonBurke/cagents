//! Exact, bounded reads of one original Changes item. Paths are display data;
//! the source key and ingestion-owned file index identify the patch.

use super::SourceKey;
use crate::chat::{ChangeKind, Role, ToolStatus};
use serde::{Deserialize, Serialize};

pub const MANIFEST_FILES: usize = 128;

/// Cache incarnation is explicit because the Store may live in an execed
/// worker whose local revision allocator restarts with the process.
pub use super::SourceEpoch as Epoch;

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
    pub expected_epoch: Option<Epoch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub source: SourceKey,
    pub epoch: Epoch,
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
    pub expected_epoch: Epoch,
    /// Fixed original file length from its manifest; mandatory even at offset0.
    pub until: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchPage {
    pub source: SourceKey,
    pub epoch: Epoch,
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
    pub epoch: Epoch,
    pub metadata: Metadata,
    pub files: Vec<FilePart>,
    /// Bounded resident placeholders have no original byte range.
    pub replacement: bool,
}

impl Manifest {
    pub fn matches_query(&self, query:&ManifestQuery)->bool {
        if self.source!=query.source || self.offset!=query.offset
            || query.expected_epoch.is_some_and(|epoch|epoch!=self.epoch)
            || self.files.len()>query.limit
            || !self.files.iter().enumerate().all(|(at,file)|self.offset.checked_add(at)==Some(file.file_index)) {
            return false;
        }
        if self.omitted_files>0 {
            return self.offset==0 && self.files.is_empty() && self.next_offset.is_none()
                && self.omitted_files==self.metadata.total_files && self.metadata.unavailable.is_some();
        }
        let Some(end)=self.offset.checked_add(self.files.len()) else {return false;};
        match self.next_offset {
            Some(next)=>next==end && next>self.offset && next<self.metadata.total_files,
            None=>end==self.metadata.total_files,
        }
    }
}

impl PatchPage {
    pub fn matches_query(&self,query:&PatchQuery)->bool {
        self.source==query.source && self.epoch==query.expected_epoch
            && self.file.file_index==query.file_index && self.file.source_len==query.until
            && self.offset==query.offset && self.text.len()<=query.limit
            && self.offset.checked_add(self.text.len())==Some(self.next_offset)
            && self.next_offset<=query.until && self.complete==(self.next_offset==query.until)
            && (self.complete || self.next_offset>self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chat::Command,model::TerminalId,terminal_ipc::{ClientRequest,ServerResponse}};
    #[test]
    fn exact_changes_queries_and_typed_replies_round_trip_on_matched_wire() {
        let source=SourceKey {agent_id:Some("child".into()),source_id:"opaque:native-part:1:changes:0-8:0-0".into()};
        let epoch=Epoch {incarnation:uuid::Uuid::new_v4(),revision:9};
        let metadata=Metadata {role:Role::Assistant,status:ToolStatus::Succeeded,at:Some(5),started_at:Some(2),total_files:2,total_bytes:8,additions:Some(1),deletions:Some(1),unavailable:None};
        let file=File {file_index:1,path:"same.rs".into(),kind:ChangeKind::Updated,source_len:4,additions:None,deletions:None};
        let chat_id=TerminalId::new();
        let query=ManifestQuery {source:source.clone(),offset:1,limit:1,expected_epoch:Some(epoch)};
        let patch=PatchQuery {source:source.clone(),file_index:1,offset:0,limit:4,expected_epoch:epoch,until:4};
        for command in [Command::LoadChangesManifest {query:query.clone()},Command::LoadChangePatch {query:patch.clone()}] {
            let request=ClientRequest::ChatCommand {chat_id,command};
            assert_eq!(postcard::from_bytes::<ClientRequest>(&postcard::to_stdvec(&request).unwrap()).unwrap(),request);
        }
        let manifest=Manifest {source:source.clone(),epoch,metadata,offset:1,next_offset:None,files:vec![file.clone()],omitted_files:0};
        assert!(manifest.matches_query(&query));
        let page=PatchPage {source,epoch,file,offset:0,next_offset:4,text:"data".into(),complete:true};
        assert!(page.matches_query(&patch));
        for response in [ServerResponse::ChatChangesManifest {chat_id,result:Ok(manifest)},ServerResponse::ChatChangePatch {chat_id,result:Ok(page)}] {
            assert_eq!(postcard::from_bytes::<ServerResponse>(&postcard::to_stdvec(&response).unwrap()).unwrap(),response);
        }
        let unavailable=ServerResponse::ChatChangesManifest {chat_id,result:Err(super::super::SourceError::Changed)};
        assert_eq!(postcard::from_bytes::<ServerResponse>(&postcard::to_stdvec(&unavailable).unwrap()).unwrap(),unavailable);
    }
}
