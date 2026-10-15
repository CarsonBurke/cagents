//! Typed editor requests and workspace edits. Protocol ranges are converted
//! only against immutable snapshots; unopened targets retain UTF-16 tags.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspEditKind { Rename, Formatting, RangeFormatting, CodeAction }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LspServerEditId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LspWorkspaceEdit {
    pub documents: Vec<LspDocumentEdits>,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDocumentEdits {
    pub path: PathBuf,
    /// The version supplied by the server, if any.
    pub version: Option<i32>,
    /// Open-buffer version used for conversion, including unversioned edits.
    pub observed_version: Option<i32>,
    pub coordinates: LspCoordinates,
    pub edits: Vec<LspTextEdit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspFormattingOptions { pub tab_size: u32, pub insert_spaces: bool }
impl Default for LspFormattingOptions {
    fn default()->Self {Self {tab_size:4,insert_spaces:true}}
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LspCapabilities {
    pub ready: bool,
    pub rename: bool,
    pub prepare_rename: bool,
    pub format_document: bool,
    pub format_selection: bool,
    pub code_actions: bool,
    pub code_action_resolve: bool,
    pub execute_commands: Vec<String>,
    pub signature_help: bool,
    pub signature_triggers: Vec<String>,
    pub signature_retriggers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LspCommand {
    pub title: String,
    pub command: String,
    pub arguments: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LspCodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub is_preferred: bool,
    pub disabled: Option<String>,
    pub edit: Option<LspWorkspaceEdit>,
    pub command: Option<LspCommand>,
    pub needs_resolve: bool,
    raw: lsp_types::CodeActionOrCommand,
    origin_version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSignatureHelp {
    pub signatures: Vec<LspSignature>,
    pub active_signature: usize,
    pub active_parameter: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSignature {
    pub label: String,
    pub documentation: Option<String>,
    pub parameters: Vec<LspParameter>,
    pub active_parameter: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspParameter {
    pub label: String,
    /// UTF-8 bytes in the signature label, never UTF-16 code units.
    pub range: Option<Range<usize>>,
    pub documentation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LspMessageLevel { Error, Warning, Info, Log }

#[derive(Debug, Clone, Default)]
pub(super) enum RequestArgs {
    #[default]
    None,
    Rename(String),
    Formatting(LspFormattingOptions),
    RangeFormatting {range:lsp_types::Range,options:LspFormattingOptions},
    CodeActions {range:lsp_types::Range,diagnostics:Vec<lsp_types::Diagnostic>},
    Resolve(lsp_types::CodeAction),
    Execute(LspCommand),
    Signature(Option<String>),
}

pub(super) enum RawFeatureResult {
    PrepareRename(Option<lsp_types::PrepareRenameResponse>),
    Rename(Option<lsp_types::WorkspaceEdit>),
    Formatting(Option<Vec<lsp_types::TextEdit>>),
    CodeActions(Option<lsp_types::CodeActionResponse>),
    CodeActionResolved(lsp_types::CodeAction),
    SignatureHelp(Option<lsp_types::SignatureHelp>),
    CommandExecuted(Option<serde_json::Value>),
}

impl LspManager {
    pub fn capabilities(&self, doc:LspDocumentId)->LspCapabilities {
        self.documents.get(&doc).and_then(|r| self.sessions.get(&r.session)).map(|s| {
            let mut caps=s.capabilities.lock().unwrap_or_else(|p|p.into_inner()).clone();
            caps.ready=s.status.lock().unwrap_or_else(|p|p.into_inner()).is_ready();caps
        }).unwrap_or_default()
    }
    pub fn formatting_settings(&self, doc:LspDocumentId)->Option<(bool,Duration)> {
        let session=self.documents.get(&doc).and_then(|r|self.sessions.get(&r.session))?;
        Some((session.server_settings.format_on_save,Duration::from_millis(session.server_settings.formatting_timeout_ms)))
    }
    pub fn signature_trigger(&self,doc:LspDocumentId,ch:char)->bool {
        let c=self.capabilities(doc);let ch=ch.to_string();c.ready && c.signature_triggers.iter().chain(&c.signature_retriggers).any(|t|t==&ch)
    }
    fn feature_request(&mut self,doc:LspDocumentId,at:LspPosition,kind:RequestKind,args:RequestArgs,timeout:Duration, supported:bool)->LspRequestId {
        if !supported {
            let id=LspRequestId(self.next_request);self.next_request+=1;
            self.events.emit_now(LspEvent::RequestFailed {request:id,doc,error:LspError::Unsupported(format!("{kind:?}"))});id
        } else {self.request_with_args(doc,at,kind,args,timeout.clamp(Duration::from_millis(10),Duration::from_secs(30)))}
    }
    pub fn request_prepare_rename(&mut self,doc:LspDocumentId,at:LspPosition)->LspRequestId {
        let caps=self.capabilities(doc);
        if caps.ready && caps.rename && !caps.prepare_rename {
            let id=LspRequestId(self.next_request);self.next_request+=1;
            let result=self.documents.get(&doc).and_then(|r|self.snapshots.latest(&r.uri)).ok_or(LspError::UnknownDocument(doc.0)).and_then(|s| identifier(&s,at));
            self.events.emit_now(match result {Ok((range,text))=>LspEvent::PrepareRename{request:id,doc,range:Some(range),placeholder:Some(text)},Err(error)=>LspEvent::RequestFailed{request:id,doc,error}});return id;
        }
        self.feature_request(doc,at,RequestKind::PrepareRename,RequestArgs::None,Duration::from_secs(15),caps.ready&&caps.prepare_rename)
    }
    pub fn request_rename(&mut self,doc:LspDocumentId,at:LspPosition,new_name:String)->LspRequestId {
        let c=self.capabilities(doc);self.feature_request(doc,at,RequestKind::Rename,RequestArgs::Rename(new_name),Duration::from_secs(15),c.ready&&c.rename)
    }
    pub fn request_formatting(&mut self,doc:LspDocumentId,options:LspFormattingOptions,timeout:Duration)->LspRequestId {
        let c=self.capabilities(doc);self.feature_request(doc,LspPosition::default(),RequestKind::Formatting,RequestArgs::Formatting(options),timeout,c.ready&&c.format_document)
    }
    pub fn request_range_formatting(&mut self,doc:LspDocumentId,range:LspRange,options:LspFormattingOptions,timeout:Duration)->LspRequestId {
        let c=self.capabilities(doc);let converted=self.documents.get(&doc).and_then(|r|self.snapshots.latest(&r.uri)).ok_or(LspError::UnknownDocument(doc.0)).and_then(|s|s.range_to_utf16(range));
        match converted { Ok(range)=>self.feature_request(doc,LspPosition::default(),RequestKind::RangeFormatting,RequestArgs::RangeFormatting{range,options},timeout,c.ready&&c.format_selection),Err(error)=>self.fail_feature(doc,error)}
    }
    pub fn request_code_actions(&mut self,doc:LspDocumentId,range:LspRange,diagnostics:&[LspDiagnostic])->LspRequestId {
        let c=self.capabilities(doc);let snapshot=self.documents.get(&doc).and_then(|r|self.snapshots.latest(&r.uri));
        let Some(snapshot)=snapshot else{return self.fail_feature(doc,LspError::UnknownDocument(doc.0));};
        let range=match snapshot.range_to_utf16(range){Ok(r)=>r,Err(e)=>return self.fail_feature(doc,e)};
        let diagnostics=diagnostics.iter().filter_map(|d|Some(lsp_types::Diagnostic {range:snapshot.range_to_utf16(d.range).ok()?,severity:Some(match d.severity{LspSeverity::Error=>lsp_types::DiagnosticSeverity::ERROR,LspSeverity::Warning=>lsp_types::DiagnosticSeverity::WARNING,LspSeverity::Information=>lsp_types::DiagnosticSeverity::INFORMATION,LspSeverity::Hint=>lsp_types::DiagnosticSeverity::HINT}),message:d.message.clone(),source:d.source.clone(),code:d.code.clone().map(lsp_types::NumberOrString::String),..Default::default()})).collect();
        self.feature_request(doc,LspPosition::default(),RequestKind::CodeActions,RequestArgs::CodeActions{range,diagnostics},Duration::from_secs(15),c.ready&&c.code_actions)
    }
    pub fn request_resolve_code_action(&mut self,doc:LspDocumentId,item:LspCodeAction)->LspRequestId {
        if self.document_version(doc)!=Some(item.origin_version){return self.fail_feature(doc,LspError::StaleVersion{result:item.origin_version,current:self.document_version(doc).unwrap_or(-1)});}
        let c=self.capabilities(doc);match item.raw {lsp_types::CodeActionOrCommand::CodeAction(raw)=>self.feature_request(doc,LspPosition::default(),RequestKind::ResolveCodeAction,RequestArgs::Resolve(raw),Duration::from_secs(15),c.ready&&c.code_action_resolve),_=>self.fail_feature(doc,LspError::Unsupported("this action does not require resolution".into()))}
    }
    pub fn execute_command(&mut self,doc:LspDocumentId,command:LspCommand)->LspRequestId {
        let c=self.capabilities(doc);let supported=c.ready&&c.execute_commands.contains(&command.command);
        self.feature_request(doc,LspPosition::default(),RequestKind::ExecuteCommand,RequestArgs::Execute(command),Duration::from_secs(15),supported)
    }
    pub fn request_signature_help(&mut self,doc:LspDocumentId,at:LspPosition,trigger:Option<String>)->LspRequestId {
        let c=self.capabilities(doc);self.feature_request(doc,at,RequestKind::SignatureHelp,RequestArgs::Signature(trigger),Duration::from_secs(15),c.ready&&c.signature_help)
    }
    fn fail_feature(&mut self,doc:LspDocumentId,error:LspError)->LspRequestId {let id=LspRequestId(self.next_request);self.next_request+=1;self.events.emit_now(LspEvent::RequestFailed{request:id,doc,error});id}
}

pub(super) fn negotiate(c:&lsp_types::ServerCapabilities)->LspCapabilities {
    fn enabled<T>(v:&Option<lsp_types::OneOf<bool,T>>)->bool{!matches!(v,None|Some(lsp_types::OneOf::Left(false)))}
    LspCapabilities {ready:true,rename:enabled(&c.rename_provider),prepare_rename:matches!(&c.rename_provider,Some(lsp_types::OneOf::Right(o)) if o.prepare_provider==Some(true)),format_document:enabled(&c.document_formatting_provider),format_selection:enabled(&c.document_range_formatting_provider),code_actions:!matches!(c.code_action_provider,None|Some(lsp_types::CodeActionProviderCapability::Simple(false))),code_action_resolve:matches!(&c.code_action_provider,Some(lsp_types::CodeActionProviderCapability::Options(o)) if o.resolve_provider==Some(true)),execute_commands:c.execute_command_provider.as_ref().map(|o|o.commands.clone()).unwrap_or_default(),signature_help:c.signature_help_provider.is_some(),signature_triggers:c.signature_help_provider.as_ref().and_then(|o|o.trigger_characters.clone()).unwrap_or_default(),signature_retriggers:c.signature_help_provider.as_ref().and_then(|o|o.retrigger_characters.clone()).unwrap_or_default()}
}

pub(super) async fn perform_feature(server:ServerSocket,kind:RequestKind,position:lsp_types::TextDocumentPositionParams,args:RequestArgs)->Result<RawFeatureResult,async_lsp::Error> {
    use RawFeatureResult as R;
    let progress=lsp_types::WorkDoneProgressParams::default();
    match (kind,args) {
        (RequestKind::PrepareRename,_)=>server.request::<request::PrepareRenameRequest>(position).await.map(R::PrepareRename),
        (RequestKind::Rename,RequestArgs::Rename(new_name))=>server.request::<request::Rename>(lsp_types::RenameParams{text_document_position:position,new_name,work_done_progress_params:progress}).await.map(R::Rename),
        (RequestKind::Formatting,RequestArgs::Formatting(options))=>server.request::<request::Formatting>(lsp_types::DocumentFormattingParams{text_document:position.text_document,options:format_options(options),work_done_progress_params:progress}).await.map(R::Formatting),
        (RequestKind::RangeFormatting,RequestArgs::RangeFormatting{range,options})=>server.request::<request::RangeFormatting>(lsp_types::DocumentRangeFormattingParams{text_document:position.text_document,range,options:format_options(options),work_done_progress_params:progress}).await.map(R::Formatting),
        (RequestKind::CodeActions,RequestArgs::CodeActions{range,diagnostics})=>server.request::<request::CodeActionRequest>(lsp_types::CodeActionParams{text_document:position.text_document,range,context:lsp_types::CodeActionContext{diagnostics,only:None,trigger_kind:Some(lsp_types::CodeActionTriggerKind::INVOKED)},work_done_progress_params:progress,partial_result_params:Default::default()}).await.map(R::CodeActions),
        (RequestKind::ResolveCodeAction,RequestArgs::Resolve(item))=>server.request::<request::CodeActionResolveRequest>(item).await.map(R::CodeActionResolved),
        (RequestKind::ExecuteCommand,RequestArgs::Execute(command))=>server.request::<request::ExecuteCommand>(lsp_types::ExecuteCommandParams{command:command.command,arguments:command.arguments,work_done_progress_params:progress}).await.map(R::CommandExecuted),
        (RequestKind::SignatureHelp,RequestArgs::Signature(trigger))=>server.request::<request::SignatureHelpRequest>(lsp_types::SignatureHelpParams{text_document_position_params:position,work_done_progress_params:progress,context:Some(lsp_types::SignatureHelpContext{trigger_kind:if trigger.is_some(){lsp_types::SignatureHelpTriggerKind::TRIGGER_CHARACTER}else{lsp_types::SignatureHelpTriggerKind::INVOKED},trigger_character:trigger,is_retrigger:false,active_signature_help:None})}).await.map(R::SignatureHelp),
        _=>Err(async_lsp::Error::Protocol("invalid editor request arguments".into()))
    }
}
fn format_options(o:LspFormattingOptions)->lsp_types::FormattingOptions{lsp_types::FormattingOptions{tab_size:o.tab_size.clamp(1,16),insert_spaces:o.insert_spaces,..Default::default()}}

fn identifier(snapshot:&Snapshot,at:LspPosition)->Result<(LspRange,String),LspError>{
    snapshot.to_utf16(at)?;
    let line=snapshot.line_content(at.line).ok_or_else(||snapshot.out_of_range(at.line,at.character))?;
    let chars:Vec<char>=line.chars().collect();let mut start=at.character as usize;let mut end=start;
    let word=|c:char|c=='_'||c.is_alphanumeric();while start>0&&word(chars[start-1]){start-=1;}while end<chars.len()&&word(chars[end]){end+=1;}
    if start==end{return Err(LspError::Unsupported("cursor is not on an identifier".into()));}
    Ok((LspRange::new(LspPosition::new(at.line,start as u32),LspPosition::new(at.line,end as u32)),chars[start..end].iter().collect()))
}

pub(super) fn convert_feature(store:&SnapshotStore,req:&PendingRequest,snapshot:&Snapshot,raw:RawFeatureResult)->LspEvent {
    let failed=|error|LspEvent::RequestFailed{request:req.id,doc:req.doc,error};
    let workspace=|edit,kind| match convert_workspace_edit(store,edit,Some(&req.observed_versions)) {Ok(edit)=>LspEvent::WorkspaceEdit{request:req.id,doc:req.doc,kind,edit},Err(e)=>failed(e)};
    match raw {
        RawFeatureResult::PrepareRename(response)=>{
            let result:Result<_,LspError>=match response {
                None=>Ok((None,None)),
                Some(lsp_types::PrepareRenameResponse::Range(range))=>snapshot.range_from_utf16(range).map(|r|(Some(r),None)),
                Some(lsp_types::PrepareRenameResponse::RangeWithPlaceholder{range,placeholder})=>snapshot.range_from_utf16(range).map(|r|(Some(r),Some(placeholder))),
                Some(lsp_types::PrepareRenameResponse::DefaultBehavior{default_behavior:true})=>snapshot.from_utf16(req.position).and_then(|p|identifier(snapshot,p)).map(|(r,p)|(Some(r),Some(p))),
                Some(lsp_types::PrepareRenameResponse::DefaultBehavior{default_behavior:false})=>Ok((None,None)),
            };
            match result {Ok((range,placeholder))=>LspEvent::PrepareRename{request:req.id,doc:req.doc,range,placeholder},Err(e)=>failed(e)}
        }
        RawFeatureResult::Rename(edit)=>workspace(edit.unwrap_or_default(),LspEditKind::Rename),
        RawFeatureResult::Formatting(edits)=>workspace(lsp_types::WorkspaceEdit{changes:Some(HashMap::from([((*req.uri).clone(),edits.unwrap_or_default())])),..Default::default()},if req.kind==RequestKind::RangeFormatting{LspEditKind::RangeFormatting}else{LspEditKind::Formatting}),
        RawFeatureResult::CodeActions(items)=>match items.unwrap_or_default().into_iter().map(|item|convert_action(store,req,item)).collect::<Result<Vec<_>,_>>() {Ok(items)=>LspEvent::CodeActions{request:req.id,doc:req.doc,items},Err(e)=>failed(e)},
        RawFeatureResult::CodeActionResolved(item)=>match convert_action(store,req,lsp_types::CodeActionOrCommand::CodeAction(item)){Ok(item)=>LspEvent::CodeActionResolved{request:req.id,doc:req.doc,item},Err(e)=>failed(e)},
        RawFeatureResult::SignatureHelp(help)=>LspEvent::SignatureHelp{request:req.id,doc:req.doc,help:help.map(convert_signature)},
        RawFeatureResult::CommandExecuted(_)=>LspEvent::CommandExecuted{request:req.id,doc:req.doc},
    }
}

pub(super) fn convert_workspace_edit(store:&SnapshotStore,edit:lsp_types::WorkspaceEdit,observed:Option<&HashMap<Url,i32>>)->Result<LspWorkspaceEdit,LspError>{
    let annotations=edit.change_annotations.unwrap_or_default();let mut out=LspWorkspaceEdit::default();
    let mut seen=std::collections::HashSet::new();
    let mut add=|uri:Url,version:Option<i32>,edits:Vec<lsp_types::OneOf<lsp_types::TextEdit,lsp_types::AnnotatedTextEdit>>|->Result<(),LspError>{
        if !seen.insert(uri.clone()){return Err(LspError::Unsupported("sequential document edits for the same URI".into()));}
        let path=uri.to_file_path().map_err(|_|LspError::Unsupported("workspace edits require file URIs".into()))?;
        let snapshot=store.latest(&uri);
        if let Some(snapshot)=&snapshot {
            if let Some(version)=version {if version!=snapshot.version{return Err(LspError::StaleVersion{result:version,current:snapshot.version});}}
            if let Some(observed)=observed {if observed.get(&uri)!=Some(&snapshot.version){return Err(LspError::StaleVersion{result:observed.get(&uri).copied().unwrap_or(-1),current:snapshot.version});}}
        }
        let converted=edits.into_iter().map(|edit|{
            let edit=match edit {
                lsp_types::OneOf::Left(edit)=>edit,
                lsp_types::OneOf::Right(edit)=>{
                    let annotation=annotations.get(&edit.annotation_id).ok_or_else(||LspError::Unsupported("unknown workspace-edit annotation".into()))?;
                    out.requires_confirmation|=annotation.needs_confirmation.unwrap_or(false);edit.text_edit
                }
            };
            let range=if let Some(snapshot)=&snapshot {snapshot.range_from_utf16(edit.range)?}else {if edit.range.start>edit.range.end{return Err(LspError::InvertedRange);}LspRange::new(LspPosition::new(edit.range.start.line,edit.range.start.character),LspPosition::new(edit.range.end.line,edit.range.end.character))};
            Ok(LspTextEdit{range,new_text:edit.new_text})
        }).collect::<Result<Vec<_>,LspError>>()?;
        out.documents.push(LspDocumentEdits{path,version,observed_version:snapshot.as_ref().map(|s|s.version),coordinates:if snapshot.is_some(){LspCoordinates::Scalar}else{LspCoordinates::Utf16},edits:converted});Ok(())
    };
    match edit.document_changes {
        Some(lsp_types::DocumentChanges::Edits(items))=>for item in items {add(item.text_document.uri,item.text_document.version,item.edits)?;},
        Some(lsp_types::DocumentChanges::Operations(items))=>for item in items {match item {lsp_types::DocumentChangeOperation::Edit(item)=>add(item.text_document.uri,item.text_document.version,item.edits)?,lsp_types::DocumentChangeOperation::Op(_)=>return Err(LspError::Unsupported("file creation, rename, and deletion in workspace edits".into()))}},
        None=>{let mut changes:Vec<_>=edit.changes.unwrap_or_default().into_iter().collect();changes.sort_by(|a,b|a.0.as_str().cmp(b.0.as_str()));for (uri,edits) in changes {add(uri,None,edits.into_iter().map(lsp_types::OneOf::Left).collect())?;}},
    }
    Ok(out)
}

fn command(c:lsp_types::Command)->LspCommand{LspCommand{title:c.title,command:c.command,arguments:c.arguments.unwrap_or_default()}}
fn convert_action(store:&SnapshotStore,req:&PendingRequest,raw:lsp_types::CodeActionOrCommand)->Result<LspCodeAction,LspError>{
    let (title,kind,is_preferred,disabled,edit,cmd,needs_resolve)=match &raw {
        lsp_types::CodeActionOrCommand::Command(c)=>(c.title.clone(),None,false,None,None,Some(command(c.clone())),false),
        lsp_types::CodeActionOrCommand::CodeAction(c)=>(c.title.clone(),c.kind.as_ref().map(|k|k.as_str().to_owned()),c.is_preferred.unwrap_or(false),c.disabled.as_ref().map(|d|d.reason.clone()),c.edit.clone().map(|e|convert_workspace_edit(store,e,Some(&req.observed_versions))).transpose()?,c.command.clone().map(command),c.edit.is_none()&&c.command.is_none()&&c.data.is_some()),
    };
    Ok(LspCodeAction{title,kind,is_preferred,disabled,edit,command:cmd,needs_resolve,raw,origin_version:req.version})
}
fn documentation(d:lsp_types::Documentation)->String{match d{lsp_types::Documentation::String(s)=>s,lsp_types::Documentation::MarkupContent(c)=>c.value}}
fn utf16_byte(label:&str,offset:u32)->Option<usize>{let mut units=0;for (byte,ch) in label.char_indices(){if units==offset{return Some(byte);}units+=ch.len_utf16() as u32;if units>offset{return None;}}(units==offset).then_some(label.len())}
fn convert_signature(help:lsp_types::SignatureHelp)->LspSignatureHelp {
    let signatures:Vec<_>=help.signatures.into_iter().map(|s|{
        let parameters:Vec<_>=s.parameters.unwrap_or_default().into_iter().map(|p|{
            let (label,range)=match p.label {
                lsp_types::ParameterLabel::Simple(label)=>{let mut matches=s.label.match_indices(&label);let start=matches.next().map(|(i,_)|i);let range=if matches.next().is_none(){start.map(|i|i..i+label.len())}else{None};(label,range)},
                lsp_types::ParameterLabel::LabelOffsets([start,end])=>{let range=utf16_byte(&s.label,start).zip(utf16_byte(&s.label,end)).and_then(|(a,b)|(a<=b).then_some(a..b));(range.as_ref().and_then(|r|s.label.get(r.clone())).unwrap_or("").to_owned(),range)},
            };LspParameter{label,range,documentation:p.documentation.map(documentation)}
        }).collect();let active_parameter=s.active_parameter.and_then(|p|((p as usize)<parameters.len()).then_some(p as usize));
        LspSignature{label:s.label,documentation:s.documentation.map(documentation),parameters,active_parameter}
    }).collect();
    let active_signature=(help.active_signature.unwrap_or(0) as usize).min(signatures.len().saturating_sub(1));
    let active_parameter=help.active_parameter.and_then(|p|signatures.get(active_signature).filter(|s|(p as usize)<s.parameters.len()).map(|_|p as usize));
    LspSignatureHelp{signatures,active_signature,active_parameter}
}

static NEXT_SERVER_EDIT_ID:AtomicU64=AtomicU64::new(1);

#[derive(Default)]
pub(super) struct ServerEdits {
    pending:Mutex<HashMap<LspServerEditId,(LspDocumentId,tokio::sync::oneshot::Sender<lsp_types::ApplyWorkspaceEditResponse>)>>,
}
impl ServerEdits {
    pub(super) fn close_doc(&self,doc:LspDocumentId){self.pending.lock().unwrap_or_else(|p|p.into_inner()).retain(|_,(d,_)|*d!=doc);}
    pub(super) fn clear(&self){self.pending.lock().unwrap_or_else(|p|p.into_inner()).clear();}
}
impl LspManager {
    pub fn respond_workspace_edit(&mut self,id:LspServerEditId,applied:bool,failure_reason:Option<String>){
        if let Some((_,reply))=self.server_edits.pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&id){let _=reply.send(lsp_types::ApplyWorkspaceEditResponse{applied,failure_reason,failed_change:None});}
    }
}

struct ServerEditGuard {id:LspServerEditId,edits:Arc<ServerEdits>}
impl Drop for ServerEditGuard {fn drop(&mut self){self.edits.pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&self.id);}}

pub(super) fn apply_workspace_edit(state:&mut RouterState,params:lsp_types::ApplyWorkspaceEditParams)->impl std::future::Future<Output=Result<lsp_types::ApplyWorkspaceEditResponse,async_lsp::Error>>+Send+'static {
    let result=(||{
        if state.retired.load(Ordering::Acquire){return Err("language server retired".to_owned());}
        let doc=state.snapshots.docs_for_session(&state.key).into_iter().next().ok_or("no open document owns this language server")?;
        let versions=state.snapshots.lock().uris.iter().filter(|(_,uri)|uri.session==state.key).map(|(uri,entry)|((**uri).clone(),entry.latest().version)).collect();
        let edit=convert_workspace_edit(&state.snapshots,params.edit,Some(&versions)).map_err(|e|e.to_string())?;
        let id=LspServerEditId(NEXT_SERVER_EDIT_ID.fetch_add(1,Ordering::Relaxed));
        let (tx,rx)=tokio::sync::oneshot::channel();
        {let mut pending=state.server_edits.pending.lock().unwrap_or_else(|p|p.into_inner());if pending.len()>=64{return Err("too many pending workspace-edit previews".to_owned());}pending.insert(id,(doc,tx));}
        state.events.emit_now(LspEvent::ApplyWorkspaceEdit{doc,id,label:params.label,edit});Ok((id,rx))
    })();
    let guard=result.as_ref().ok().map(|(id,_)|ServerEditGuard{id:*id,edits:Arc::clone(&state.server_edits)});
    async move {
        let _guard=guard;
        let failure=|message|lsp_types::ApplyWorkspaceEditResponse{applied:false,failure_reason:Some(message),failed_change:None};
        Ok(match result {
            Err(message)=>failure(message),
            Ok((_id,rx))=>{
                let response=match tokio::time::timeout(Duration::from_secs(120),rx).await {Ok(Ok(response))=>response,_=>failure("workspace-edit preview closed or timed out".into())};
                response
            }
        })
    }
}

pub(super) fn publish_message(state:&RouterState,level:lsp_types::MessageType,message:String,show:bool){
    if state.retired.load(Ordering::Acquire){return;}
    let level=match level{lsp_types::MessageType::ERROR=>LspMessageLevel::Error,lsp_types::MessageType::WARNING=>LspMessageLevel::Warning,lsp_types::MessageType::INFO=>LspMessageLevel::Info,_=>LspMessageLevel::Log};
    let message:String=message.chars().take(8192).collect();
    if show {for doc in state.snapshots.docs_for_session(&state.key){state.events.emit_now(LspEvent::ServerMessage{doc,level,message:message.clone()});}}
    match level {LspMessageLevel::Error=>tracing::error!(server=%state.key.server_id,%message,"language server message"),LspMessageLevel::Warning=>tracing::warn!(server=%state.key.server_id,%message,"language server message"),_=>tracing::info!(server=%state.key.server_id,%message,"language server message")}
}
pub(super) fn configuration(configuration:&serde_json::Value,section:Option<&str>)->serde_json::Value {
    let Some(section)=section.filter(|s|!s.is_empty()) else{return configuration.clone();};
    section.split('.').try_fold(configuration,|value,key|value.get(key)).cloned().unwrap_or(serde_json::Value::Null)
}
impl RequestArgs {
    pub(super) fn queued_bytes(&self)->usize {match self{
        Self::None=>0,Self::Rename(s)=>s.len(),Self::Formatting(_)|Self::RangeFormatting{..}=>std::mem::size_of::<Self>(),
        Self::CodeActions{diagnostics,..}=>diagnostics.iter().map(|d|d.message.len()+d.source.as_ref().map_or(0,String::len)+128).sum(),
        Self::Resolve(item)=>serde_json::to_vec(item).map_or(MAX_CONTENT_LENGTH,|b|b.len()),
        Self::Execute(c)=>c.command.len()+c.arguments.iter().map(|a|serde_json::to_vec(a).map_or(MAX_CONTENT_LENGTH,|b|b.len())).sum::<usize>(),
        Self::Signature(s)=>s.as_ref().map_or(0,String::len),
    }}
}

pub(super) fn add_client_capabilities(caps:&mut lsp_types::ClientCapabilities){
    let Some(text)=&mut caps.text_document else{return;};
    text.rename=Some(lsp_types::RenameClientCapabilities{prepare_support:Some(true),prepare_support_default_behavior:Some(lsp_types::PrepareSupportDefaultBehavior::IDENTIFIER),honors_change_annotations:Some(true),..Default::default()});
    text.formatting=Some(lsp_types::DocumentFormattingClientCapabilities::default());
    text.range_formatting=Some(lsp_types::DocumentRangeFormattingClientCapabilities::default());
    text.code_action=Some(lsp_types::CodeActionClientCapabilities{code_action_literal_support:Some(lsp_types::CodeActionLiteralSupport{code_action_kind:lsp_types::CodeActionKindLiteralSupport{value_set:vec!["quickfix".into(),"refactor".into(),"source".into()]}}),is_preferred_support:Some(true),disabled_support:Some(true),data_support:Some(true),resolve_support:Some(lsp_types::CodeActionCapabilityResolveSupport{properties:vec!["edit".into(),"command".into()]}),honors_change_annotations:Some(true),..Default::default()});
    text.signature_help=Some(lsp_types::SignatureHelpClientCapabilities{context_support:Some(true),signature_information:Some(lsp_types::SignatureInformationSettings{documentation_format:Some(vec![lsp_types::MarkupKind::Markdown,lsp_types::MarkupKind::PlainText]),parameter_information:Some(lsp_types::ParameterInformationSettings{label_offset_support:Some(true)}),active_parameter_support:Some(true)}),..Default::default()});
}

impl LspCodeAction {
    /// Constructs a local action without protocol resolution data.
    pub fn local(title:String,edit:Option<LspWorkspaceEdit>,command:Option<LspCommand>)->Self {
        let raw=lsp_types::CodeActionOrCommand::CodeAction(lsp_types::CodeAction{title:title.clone(),..Default::default()});
        Self{title,kind:None,is_preferred:false,disabled:None,edit,command,needs_resolve:false,raw,origin_version:-1}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn informational_notice_flood_does_not_replace_staged_error(){
        let mut pending=PendingInner::default();let doc=LspDocumentId(1);
        pending.stage(LspEvent::ServerMessage{doc,level:LspMessageLevel::Error,message:"workspace failed".into()});
        for i in 0..1024 {pending.stage(LspEvent::ServerMessage{doc,level:LspMessageLevel::Info,message:i.to_string()});}
        assert_eq!(pending.replaceable.len(),2);
        assert!(matches!(pending.take(false),Some(LspEvent::ServerMessage{level:LspMessageLevel::Error,..})));
        assert!(matches!(pending.take(false),Some(LspEvent::ServerMessage{level:LspMessageLevel::Info,message,..}) if message=="1023"));
    }

    #[test]
    fn workspace_annotations_require_confirmation_and_bad_ranges_refuse_the_whole_batch(){
        let store=SnapshotStore::default();let uri=Arc::new(Url::parse("file:///repo/a.rs").unwrap());let key=SessionKey{root:Arc::from(Path::new("/repo")),server_id:Arc::from("rust")};
        store.register(LspDocumentId(1),&uri,&key,4,Arc::from("a𝄞b"));
        let value=json!({"changeAnnotations":{"confirm":{"label":"Rename export","needsConfirmation":true}},"documentChanges":[{"textDocument":{"uri":uri.as_str(),"version":4},"edits":[{"annotationId":"confirm","range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"newText":"x"}]}]});
        let converted=convert_workspace_edit(&store,serde_json::from_value(value.clone()).unwrap(),None).unwrap();
        assert!(converted.requires_confirmation);assert_eq!(converted.documents[0].edits[0].range,LspRange::new(LspPosition::new(0,1),LspPosition::new(0,2)));
        let mut missing=value.clone();missing["changeAnnotations"]=json!({});assert!(matches!(convert_workspace_edit(&store,serde_json::from_value(missing).unwrap(),None),Err(LspError::Unsupported(_))));
        let mut split=value;split["documentChanges"][0]["edits"][0]["range"]["start"]["character"]=json!(2);assert!(matches!(convert_workspace_edit(&store,serde_json::from_value(split).unwrap(),None),Err(LspError::SplitSurrogate{..})));
        let duplicate: lsp_types::WorkspaceEdit=serde_json::from_value(json!({"documentChanges":[{"textDocument":{"uri":uri.as_str(),"version":4},"edits":[]},{"textDocument":{"uri":uri.as_str(),"version":4},"edits":[]}]})).unwrap();assert!(matches!(convert_workspace_edit(&store,duplicate,None),Err(LspError::Unsupported(_))));
    }

    #[test]
    fn cancelled_server_edit_future_releases_preview_even_before_first_poll(){
        let manager=LspManager::new(Vec::new());let uri=Arc::new(Url::parse("file:///repo/a.rs").unwrap());let key=SessionKey{root:Arc::from(Path::new("/repo")),server_id:Arc::from("rust")};
        manager.snapshots.register(LspDocumentId(1),&uri,&key,1,Arc::from("foo"));
        let mut router=RouterState{key,snapshots:Arc::clone(&manager.snapshots),events:manager.events.clone(),retired:Arc::new(AtomicBool::new(false)),server_edits:Arc::clone(&manager.server_edits),configuration:serde_json::Value::Null};
        let request=apply_workspace_edit(&mut router,lsp_types::ApplyWorkspaceEditParams{label:None,edit:Default::default()});
        assert_eq!(manager.server_edits.pending.lock().unwrap().len(),1);drop(request);assert!(manager.server_edits.pending.lock().unwrap().is_empty());
    }
}
