//! Original tool arguments and the command projection captured before display
//! shortening. A JSON representation always says whether it came from streamed
//! fragments or from the provider's already parsed input object.
use super::{SourceEpoch, SourceKey};
use serde::{Deserialize, Serialize};

pub const RAW_SOURCE_BYTES: usize = 64 * 1024 * 1024;
/// Two independently readable fields for each of the128 resident parent items.
pub const RAW_PARTS: usize = 256;
pub const RAW_PART_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RawSourceField { ToolInput, CommandCall }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawRepresentation {
    /// Exact concatenated SDK input_json_delta strings, without reformatting.
    StreamJson,
    /// JSON serialization of the exact parsed native input object. This does
    /// not claim to retain whitespace from the enclosing transport frame.
    CanonicalJson,
    /// The app's original full command projection, before identity shortening.
    CommandMetadataJson,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawCommandCall {
    pub label: String,
    pub command: String,
    /// App projection at capture time; not a kernel/provider scope receipt.
    pub working_directory: String,
    pub action: crate::chat::CommandAction,
}
impl RawCommandCall {
    pub(crate) fn from_content(content:&crate::chat::ItemContent)->Option<Self> {
        match content {
            crate::chat::ItemContent::Command {label,command,working_directory,action,..}=>Some(Self {label:label.clone(),command:command.clone(),working_directory:working_directory.clone(),action:*action}),
            _=>None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSourceMetadata {
    pub tool_name: String,
    pub representation: RawRepresentation,
    /// False means the frozen prefix was still being captured. It must never
    /// be presented as a complete original call by a later page's observer.
    pub complete_call: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSourceQuery {
    pub source: SourceKey,
    pub field: RawSourceField,
    pub offset: usize,
    pub limit: usize,
    pub expected_epoch: Option<SourceEpoch>,
    pub until: Option<usize>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSourcePage {
    pub source: SourceKey,
    pub field: RawSourceField,
    pub epoch: SourceEpoch,
    pub source_len: usize,
    pub offset: usize,
    pub next_offset: usize,
    pub text: String,
    pub complete: bool,
    pub metadata: RawSourceMetadata,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawSourcePart {
    pub item_id: String,
    pub source: SourceKey,
    pub field: RawSourceField,
    pub epoch: SourceEpoch,
    pub source_len: usize,
    pub metadata: RawSourceMetadata,
}
/// Internal immutable ingress, prior to raw argument conversion into a patch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawToolCapture {
    Start { source:SourceKey, name:String },
    /// Canonical serialization of the provider's exact parsed input object.
    Full { source:SourceKey, name:String, input:String, command:Option<RawCommandCall> },
    /// Seal the exact previously observed stream prefix after parsing it. The
    /// hash is over canonical JSON, so a later parsed-object replay can prove
    /// equality without replacing the original streamed byte representation.
    Seal { source:SourceKey, input_len:usize, canonical_hash:[u8;32], command:Option<RawCommandCall> },
}
impl RawToolCapture {
    pub fn source(&self)->&SourceKey {
        match self { Self::Start {source,..}|Self::Full {source,..}|Self::Seal {source,..}=>source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_source_wire_preserves_representation_and_exact_store_epoch(){
        let source=SourceKey{agent_id:None,source_id:"opaque:native-part:1".into()};let epoch=SourceEpoch{incarnation:uuid::Uuid::new_v4(),revision:19};
        let query=RawSourceQuery{source:source.clone(),field:RawSourceField::ToolInput,offset:4,limit:256,expected_epoch:Some(epoch),until:Some(8)};
        let chat_id=crate::model::TerminalId::new();let request=crate::terminal_ipc::ClientRequest::ChatCommand{chat_id,command:crate::chat::Command::LoadRawSource{query}};
        assert_eq!(postcard::from_bytes::<crate::terminal_ipc::ClientRequest>(&postcard::to_stdvec(&request).unwrap()).unwrap(),request);
        for representation in [RawRepresentation::StreamJson,RawRepresentation::CanonicalJson,RawRepresentation::CommandMetadataJson]{
            let response=crate::terminal_ipc::ServerResponse::ChatRawSource{chat_id,result:Ok(RawSourcePage{source:source.clone(),field:RawSourceField::ToolInput,epoch,source_len:8,offset:4,next_offset:8,text:"null".into(),complete:true,metadata:RawSourceMetadata{tool_name:"Write".into(),representation,complete_call:false}})};
            assert_eq!(postcard::from_bytes::<crate::terminal_ipc::ServerResponse>(&postcard::to_stdvec(&response).unwrap()).unwrap(),response);
            assert_eq!(serde_json::from_str::<crate::terminal_ipc::ServerResponse>(&serde_json::to_string(&response).unwrap()).unwrap(),response);
        }
    }
}
