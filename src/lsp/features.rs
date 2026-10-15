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
    pub annotations: Vec<LspChangeAnnotation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspChangeAnnotation {
    pub id:String,
    pub label:String,
    pub description:Option<String>,
    pub needs_confirmation:bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDocumentEdits {
    pub path: PathBuf,
    /// The version supplied by the server, if any.
    pub version: Option<i32>,
    /// Open-buffer version used for conversion, including unversioned edits.
    pub observed_version: Option<i32>,
    /// Registration identity paired atomically with the converted snapshot.
    pub observed_incarnation: Option<LspDocumentIncarnation>,
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
    // Retains original peer context across an opaque lazy resolve. Recapturing
    // current versions would bless stale server data after close/reopen.
    observed_documents: Arc<HashMap<Url, ObservedDocument>>,
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
    Resolve { action:lsp_types::CodeAction, observed:Arc<HashMap<Url,ObservedDocument>> },
    Execute(LspCommand),
    Signature {trigger:Option<String>,is_retrigger:bool,active:Option<lsp_types::SignatureHelp>},
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
            caps.ready=!s.retired.load(Ordering::Acquire) && !s.commands.is_closed() && s.status.lock().unwrap_or_else(|p|p.into_inner()).is_ready();caps
        }).unwrap_or_default()
    }
    pub fn formatting_settings(&self, doc:LspDocumentId)->Option<(bool,Duration)> {
        let session=self.documents.get(&doc).and_then(|r|self.sessions.get(&r.session))?;
        Some((session.server_settings.format_on_save,Duration::from_millis(session.server_settings.formatting_timeout_ms)))
    }
    pub fn signature_trigger(&self,doc:LspDocumentId,ch:char)->bool {
        self.signature_trigger_with_context(doc,ch,false)
    }
    pub fn signature_trigger_with_context(&self,doc:LspDocumentId,ch:char,visible:bool)->bool {
        let Some(session)=self.documents.get(&doc).and_then(|document|self.sessions.get(&document.session)) else{return false;};
        if session.retired.load(Ordering::Acquire)||session.commands.is_closed()||!session.status.lock().unwrap_or_else(|p|p.into_inner()).is_ready(){return false;}
        let caps=session.capabilities.lock().unwrap_or_else(|p|p.into_inner());let mut encoded=[0;4];let ch:&str=ch.encode_utf8(&mut encoded);
        caps.signature_help && (caps.signature_triggers.iter().any(|trigger|trigger==ch)||(visible&&caps.signature_retriggers.iter().any(|trigger|trigger==ch)))
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
        let diagnostics=diagnostics.iter().filter_map(|d|{
            let mut raw=d.protocol.as_deref().cloned().unwrap_or_else(||lsp_types::Diagnostic{message:d.message.clone(),source:d.source.clone(),code:d.code.clone().map(lsp_types::NumberOrString::String),severity:Some(match d.severity{LspSeverity::Error=>lsp_types::DiagnosticSeverity::ERROR,LspSeverity::Warning=>lsp_types::DiagnosticSeverity::WARNING,LspSeverity::Information=>lsp_types::DiagnosticSeverity::INFORMATION,LspSeverity::Hint=>lsp_types::DiagnosticSeverity::HINT}),..Default::default()});
            raw.range=snapshot.range_to_utf16(d.range).ok()?;Some(raw)
        }).collect();
        self.feature_request(doc,LspPosition::default(),RequestKind::CodeActions,RequestArgs::CodeActions{range,diagnostics},Duration::from_secs(15),c.ready&&c.code_actions)
    }
    pub fn request_resolve_code_action(&mut self,doc:LspDocumentId,item:LspCodeAction)->LspRequestId {
        if self.document_version(doc)!=Some(item.origin_version){return self.fail_feature(doc,LspError::StaleVersion{result:item.origin_version,current:self.document_version(doc).unwrap_or(-1)});}
        let c=self.capabilities(doc);match item.raw {lsp_types::CodeActionOrCommand::CodeAction(raw)=>self.feature_request(doc,LspPosition::default(),RequestKind::ResolveCodeAction,RequestArgs::Resolve {action:raw,observed:item.observed_documents},Duration::from_secs(15),c.ready&&c.code_action_resolve),_=>self.fail_feature(doc,LspError::Unsupported("this action does not require resolution".into()))}
    }
    /// Before executing an opaque previously offered action command, the UI
    /// must validate its original peer context. An edit-only action instead
    /// fences only its actual targets via observed_incarnation in the planner.
    pub fn validate_code_action_context(&self,item:&LspCodeAction)->Result<(),LspError> {
        for (uri,observed) in item.observed_documents.iter() {
            let Some((snapshot,incarnation))=self.snapshots.observation(uri) else {
                return Err(LspError::Unsupported("A code-action peer document was closed after the action was requested".into()));
            };
            if incarnation!=observed.incarnation {
                return Err(LspError::Unsupported("A code-action peer document was closed and reopened after the action was requested".into()));
            }
            if snapshot.version!=observed.version {
                return Err(LspError::StaleVersion{result:observed.version,current:snapshot.version});
            }
        }
        Ok(())
    }

    pub fn execute_command(&mut self,doc:LspDocumentId,command:LspCommand)->LspRequestId {
        let c=self.capabilities(doc);let supported=c.ready&&c.execute_commands.contains(&command.command);
        self.feature_request(doc,LspPosition::default(),RequestKind::ExecuteCommand,RequestArgs::Execute(command),Duration::from_secs(15),supported)
    }
    pub fn request_signature_help(&mut self,doc:LspDocumentId,at:LspPosition,trigger:Option<String>)->LspRequestId {
        self.request_signature_help_retrigger(doc,at,trigger,false,None)
    }
    pub fn request_signature_help_retrigger(&mut self,doc:LspDocumentId,at:LspPosition,trigger:Option<String>,is_retrigger:bool,active:Option<LspSignatureHelp>)->LspRequestId {
        let c=self.capabilities(doc);let active=if is_retrigger {active.map(signature_to_protocol)}else{None};
        self.feature_request(doc,at,RequestKind::SignatureHelp,RequestArgs::Signature{trigger,is_retrigger,active},Duration::from_secs(15),c.ready&&c.signature_help)
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
        (RequestKind::ResolveCodeAction,RequestArgs::Resolve{action,..})=>server.request::<request::CodeActionResolveRequest>(action).await.map(R::CodeActionResolved),
        (RequestKind::ExecuteCommand,RequestArgs::Execute(command))=>server.request::<request::ExecuteCommand>(lsp_types::ExecuteCommandParams{command:command.command,arguments:command.arguments,work_done_progress_params:progress}).await.map(R::CommandExecuted),
        (RequestKind::SignatureHelp,RequestArgs::Signature{trigger,is_retrigger,active})=>server.request::<request::SignatureHelpRequest>(lsp_types::SignatureHelpParams{text_document_position_params:position,work_done_progress_params:progress,context:Some(lsp_types::SignatureHelpContext{trigger_kind:if trigger.is_some(){lsp_types::SignatureHelpTriggerKind::TRIGGER_CHARACTER}else{lsp_types::SignatureHelpTriggerKind::INVOKED},trigger_character:trigger,is_retrigger,active_signature_help:active})}).await.map(R::SignatureHelp),
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
        RawFeatureResult::CodeActions(items)=>{
            let items=items.unwrap_or_default();
            if items.len()>512 || serde_json::to_vec(&items).map_or(true,|data|data.len()>16*1024*1024){return failed(LspError::Unsupported("code-action response exceeds 512 items or 16 MiB".into()));}
            match items.into_iter().map(|item|convert_action(store,req,item)).collect::<Result<Vec<_>,_>>(){Ok(items)=>LspEvent::CodeActions{request:req.id,doc:req.doc,items},Err(e)=>failed(e)}
        },
        RawFeatureResult::CodeActionResolved(item)=>match convert_action(store,req,lsp_types::CodeActionOrCommand::CodeAction(item)){Ok(item)=>LspEvent::CodeActionResolved{request:req.id,doc:req.doc,item},Err(e)=>failed(e)},
        RawFeatureResult::SignatureHelp(help)=>match help.map(convert_signature).transpose(){Ok(help)=>LspEvent::SignatureHelp{request:req.id,doc:req.doc,help},Err(error)=>failed(error)},
        RawFeatureResult::CommandExecuted(_)=>LspEvent::CommandExecuted{request:req.id,doc:req.doc},
    }
}

pub(super) fn convert_workspace_edit(store:&SnapshotStore,edit:lsp_types::WorkspaceEdit,observed:Option<&HashMap<Url,ObservedDocument>>)->Result<LspWorkspaceEdit,LspError>{
    let annotations=edit.change_annotations.unwrap_or_default();let mut out=LspWorkspaceEdit::default();
    let mut seen=std::collections::HashSet::new();
    let mut used_annotations=std::collections::HashSet::new();
    let mut add=|uri:Url,version:Option<i32>,edits:Vec<lsp_types::OneOf<lsp_types::TextEdit,lsp_types::AnnotatedTextEdit>>|->Result<(),LspError>{
        if !seen.insert(uri.clone()){return Err(LspError::Unsupported("sequential document edits for the same URI".into()));}
        let path=uri.to_file_path().map_err(|_|LspError::Unsupported("workspace edits require file URIs".into()))?;
        let current=store.observation(&uri);
        if let Some(observed)=observed {
            match (observed.get(&uri), current.as_ref()) {
                (Some(previous), Some((snapshot, incarnation))) => {
                    if previous.incarnation != *incarnation {
                        return Err(LspError::Unsupported("A workspace-edit target was closed and reopened after the request".into()));
                    }
                    if previous.version != snapshot.version {
                        return Err(LspError::StaleVersion { result:previous.version, current:snapshot.version });
                    }
                }
                (Some(_), None) => return Err(LspError::Unsupported("An open workspace-edit target was closed after the request".into())),
                (None, Some((snapshot, _))) => return Err(LspError::StaleVersion { result:-1, current:snapshot.version }),
                (None, None) => {}
            }
        }
        let (snapshot, observed_incarnation)=match current {
            Some((snapshot, incarnation)) => (Some(snapshot), Some(incarnation)),
            None => (None, None),
        };
        if let (Some(snapshot), Some(version))=(&snapshot, version) {
            if version!=snapshot.version{return Err(LspError::StaleVersion{result:version,current:snapshot.version});}
        }
        let converted=edits.into_iter().map(|edit|{
            let edit=match edit {
                lsp_types::OneOf::Left(edit)=>edit,
                lsp_types::OneOf::Right(edit)=>{
                    let annotation=annotations.get(&edit.annotation_id).ok_or_else(||LspError::Unsupported("unknown workspace-edit annotation".into()))?;
                    let needs_confirmation=annotation.needs_confirmation.unwrap_or(false);
                    out.requires_confirmation|=needs_confirmation;
                    if used_annotations.insert(edit.annotation_id.clone()) {
                        if edit.annotation_id.len()>1024 || annotation.label.len()>1024 || annotation.description.as_ref().is_some_and(|description|description.len()>4096) {
                            return Err(LspError::Unsupported("workspace-edit annotation metadata exceeds its bounded label/description budget".into()));
                        }
                        if used_annotations.len()>256 {return Err(LspError::Unsupported("workspace edit contains more than 256 annotation groups".into()));}
                        out.annotations.push(LspChangeAnnotation{id:edit.annotation_id,label:annotation.label.clone(),description:annotation.description.clone(),needs_confirmation});
                    }
                    edit.text_edit
                }
            };
            let range=if let Some(snapshot)=&snapshot {snapshot.range_from_utf16(edit.range)?}else {if edit.range.start>edit.range.end{return Err(LspError::InvertedRange);}LspRange::new(LspPosition::new(edit.range.start.line,edit.range.start.character),LspPosition::new(edit.range.end.line,edit.range.end.character))};
            Ok(LspTextEdit{range,new_text:edit.new_text})
        }).collect::<Result<Vec<_>,LspError>>()?;
        out.documents.push(LspDocumentEdits{path,version,observed_version:snapshot.as_ref().map(|s|s.version),observed_incarnation,coordinates:if snapshot.is_some(){LspCoordinates::Scalar}else{LspCoordinates::Utf16},edits:converted});Ok(())
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
    let valid_command=|c:&lsp_types::Command|c.title.len()<=4096&&c.command.len()<=4096;
    let valid=match &raw {
        lsp_types::CodeActionOrCommand::Command(c)=>valid_command(c),
        lsp_types::CodeActionOrCommand::CodeAction(c)=>c.title.len()<=4096 && c.kind.as_ref().is_none_or(|kind|kind.as_str().len()<=1024) && c.disabled.as_ref().is_none_or(|disabled|disabled.reason.len()<=4096) && c.command.as_ref().is_none_or(valid_command),
    };
    if !valid || serde_json::to_vec(&raw).map_or(true,|data|data.len()>16*1024*1024){return Err(LspError::Unsupported("code-action title, disabled reason, command, or payload exceeds its bounded metadata budget".into()));}
    let (title,kind,is_preferred,disabled,edit,cmd,needs_resolve)=match &raw {
        lsp_types::CodeActionOrCommand::Command(c)=>(c.title.clone(),None,false,None,None,Some(command(c.clone())),false),
        lsp_types::CodeActionOrCommand::CodeAction(c)=>(c.title.clone(),c.kind.as_ref().map(|k|k.as_str().to_owned()),c.is_preferred.unwrap_or(false),c.disabled.as_ref().map(|d|d.reason.clone()),c.edit.clone().map(|e|convert_workspace_edit(store,e,Some(&req.observed_versions))).transpose()?,c.command.clone().map(command),(c.edit.is_none()||c.command.is_none())&&c.data.is_some()),
    };
    Ok(LspCodeAction{title,kind,is_preferred,disabled,edit,command:cmd,needs_resolve,raw,origin_version:req.version,observed_documents:req.observed_versions.clone()})
}
fn documentation(d:lsp_types::Documentation)->String{match d{lsp_types::Documentation::String(s)=>s,lsp_types::Documentation::MarkupContent(c)=>c.value}}
fn utf16_byte(label:&str,offset:u32)->Option<usize>{let mut units=0;for (byte,ch) in label.char_indices(){if units==offset{return Some(byte);}units+=ch.len_utf16() as u32;if units>offset{return None;}}(units==offset).then_some(label.len())}
fn convert_signature(help:lsp_types::SignatureHelp)->Result<LspSignatureHelp,LspError> {
    fn doc_len(doc:&lsp_types::Documentation)->usize{match doc{lsp_types::Documentation::String(text)=>text.len(),lsp_types::Documentation::MarkupContent(content)=>content.value.len()}}
    let invalid=||LspError::Unsupported("signature help exceeds its label, documentation, parameter, or retained-text budget".into());
    if help.signatures.len()>128{return Err(invalid());}
    let mut retained=0usize;let mut parameters=0usize;
    for signature in &help.signatures {
        let docs=signature.documentation.as_ref().map_or(0,doc_len);
        if signature.label.len()>16*1024||docs>64*1024{return Err(invalid());}
        retained=retained.saturating_add(signature.label.len()+docs);
        let params=signature.parameters.as_deref().unwrap_or_default();parameters+=params.len();
        if params.len()>256||parameters>4096{return Err(invalid());}
        for parameter in params {
            let docs=parameter.documentation.as_ref().map_or(0,doc_len);
            if docs>16*1024{return Err(invalid());}
            let bytes=match &parameter.label {
                lsp_types::ParameterLabel::Simple(label)=>{if label.len()>4096{return Err(invalid());}label.len()},
                // Three UTF-8 bytes per UTF-16 unit is an upper bound, and
                // overlapping parameter spans must not amplify one label.
                lsp_types::ParameterLabel::LabelOffsets([start,end])=>((end.saturating_sub(*start) as usize).saturating_mul(3)).min(signature.label.len()),
            };
            retained=retained.saturating_add(bytes+docs);
        }
        if retained>4*1024*1024{return Err(invalid());}
    }
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
    Ok(LspSignatureHelp{signatures,active_signature,active_parameter})
}

fn signature_to_protocol(help:LspSignatureHelp)->lsp_types::SignatureHelp {
    let signatures=help.signatures.into_iter().map(|signature| {
        let parameters=signature.parameters.into_iter().map(|parameter| {
            let offsets=parameter.range.and_then(|range| {
                signature.label.get(range.clone())?;
                Some([u32::try_from(signature.label[..range.start].encode_utf16().count()).ok()?,u32::try_from(signature.label[..range.end].encode_utf16().count()).ok()?])
            });
            lsp_types::ParameterInformation {label:offsets.map(lsp_types::ParameterLabel::LabelOffsets).unwrap_or(lsp_types::ParameterLabel::Simple(parameter.label)),documentation:parameter.documentation.map(lsp_types::Documentation::String)}
        }).collect();
        lsp_types::SignatureInformation {label:signature.label,documentation:signature.documentation.map(lsp_types::Documentation::String),parameters:Some(parameters),active_parameter:signature.active_parameter.and_then(|index|u32::try_from(index).ok())}
    }).collect();
    lsp_types::SignatureHelp {signatures,active_signature:u32::try_from(help.active_signature).ok(),active_parameter:help.active_parameter.and_then(|index|u32::try_from(index).ok())}
}

static NEXT_SERVER_EDIT_ID:AtomicU64=AtomicU64::new(1);

struct ServerEdit {
    doc: LspDocumentId,
    deadline: std::time::Instant,
    origin_retired: Arc<AtomicBool>,
    // None means the publication lease owns the response. Its timeout and
    // document-close paths no longer have authority to report failure.
    reply: Option<tokio::sync::oneshot::Sender<lsp_types::ApplyWorkspaceEditResponse>>,
}

#[derive(Default)]
pub(super) struct ServerEdits {
    pending: Mutex<HashMap<LspServerEditId, ServerEdit>>,
}
impl ServerEdits {
    fn preview(&self, doc:LspDocumentId, deadline:std::time::Instant, origin_retired:Arc<AtomicBool>)
        -> Result<(LspServerEditId, tokio::sync::oneshot::Receiver<lsp_types::ApplyWorkspaceEditResponse>),String> {
        let mut pending = self.pending.lock().unwrap_or_else(|p|p.into_inner());
        if pending.len() >= 64 { return Err("too many pending workspace-edit previews".into()); }
        let id = LspServerEditId(NEXT_SERVER_EDIT_ID.fetch_update(Ordering::Relaxed,Ordering::Relaxed,
            |next|next.checked_add(1)).map_err(|_|"workspace-edit request identity space exhausted".to_owned())?);
        let (reply, receiver) = tokio::sync::oneshot::channel();
        pending.insert(id, ServerEdit { doc, deadline, origin_retired, reply:Some(reply) });
        Ok((id, receiver))
    }
    pub(super) fn close_doc(&self, doc: LspDocumentId) {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
            .retain(|_, entry| entry.doc != doc || entry.reply.is_none());
    }
    pub(super) fn clear(&self) {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
            .retain(|_, entry| entry.reply.is_none());
    }
    fn remove_preview(&self, id: LspServerEditId) -> Option<ServerEdit> {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.get(&id).is_some_and(|entry| entry.reply.is_some()) {
            pending.remove(&id)
        } else { None }
    }
}

/// Exclusive response and publication authority for an admitted server edit.
/// Claim immediately before any buffer or filesystem mutation, then keep this
/// non-cloneable lease until publication completes. Dropping an unused lease
/// rejects the request; preview expiry cannot reject a claimed publication.
#[must_use = "keep the server-edit lease until publication finishes"]
pub struct LspServerEditLease {
    id: LspServerEditId,
    edits: Arc<ServerEdits>,
    reply: Option<tokio::sync::oneshot::Sender<lsp_types::ApplyWorkspaceEditResponse>>,
}
impl std::fmt::Debug for LspServerEditLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServerEditLease").field("id", &self.id).finish_non_exhaustive()
    }
}
impl LspServerEditLease {
    pub fn respond(mut self, applied: bool, failure_reason: Option<String>) {
        self.finish(applied, failure_reason);
    }
    fn finish(&mut self, applied: bool, failure_reason: Option<String>) {
        if let Some(reply) = self.reply.take() {
            self.edits.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
            let _ = reply.send(lsp_types::ApplyWorkspaceEditResponse { applied, failure_reason, failed_change: None });
        }
    }
}
impl Drop for LspServerEditLease {
    fn drop(&mut self) {
        self.finish(false, Some("workspace-edit publication was cancelled".into()));
    }
}
impl LspManager {
    /// Unit controller fixtures own the real bounded preview/reply state; a
    /// synthetic event ID cannot accidentally grant publication authority.
    #[cfg(test)]
    pub(crate) fn fixture_workspace_edit_preview(&self, doc:LspDocumentId, timeout:Duration)
        -> Result<(LspServerEditId, tokio::sync::oneshot::Receiver<lsp_types::ApplyWorkspaceEditResponse>),String> {
        if self.document_version(doc).is_none() { return Err("The fixture document is not registered".into()); }
        let deadline = std::time::Instant::now().checked_add(timeout)
            .ok_or("The fixture preview deadline is outside the clock range")?;
        let origin_retired = self.documents.get(&doc).and_then(|document|self.sessions.get(&document.session))
            .map(|session|Arc::clone(&session.retired)).unwrap_or_else(||Arc::new(AtomicBool::new(false)));
        self.server_edits.preview(doc, deadline, origin_retired)
    }
    /// Marks the actual admission's originating authority as retired, for
    /// controller refusal fixtures that intentionally launch no server.
    #[cfg(test)]
    pub(crate) fn fixture_retire_workspace_edit_source(&self, id:LspServerEditId) {
        if let Some(entry) = self.server_edits.pending.lock().unwrap_or_else(|p|p.into_inner()).get(&id) {
            entry.origin_retired.store(true, Ordering::Release);
        }
    }
    /// Atomically claims an unexpired preview from its exact live server.
    /// Closing its document, retiring its original session, or timing out
    /// before this point makes claiming fail without publication authority.
    /// Once claimed, the lease owns the response across preview expiry and
    /// manager shutdown; it must be acknowledged after the actual publication.
    pub fn claim_workspace_edit(&mut self, id: LspServerEditId) -> Result<LspServerEditLease, String> {
        if self.stopping {
            let reason = "The language manager is shutting down; the server edit cannot be published".to_owned();
            self.respond_workspace_edit(id, false, Some(reason.clone()));
            return Err(reason);
        }
        let mut pending = self.server_edits.pending.lock().unwrap_or_else(|p| p.into_inner());
        let entry = pending.get_mut(&id).ok_or("The server edit expired or its source was closed; request it again")?;
        if entry.reply.is_none() { return Err("The server edit is already being published".into()); }
        // A protocol worker can finish admitting a request after the manager's
        // close/clear pass. Reprove the original binding at the claim boundary.
        let has_document = self.document_version(entry.doc).is_some()
            || self.snapshots.lock().docs.contains_key(&entry.doc);
        let refusal = if entry.origin_retired.load(Ordering::Acquire) {
            Some("The originating language server retired; request the edit again")
        } else if !has_document {
            Some("The server edit source was closed; request it again")
        } else if std::time::Instant::now() >= entry.deadline {
            Some("The server edit preview expired; request it again")
        } else { None };
        if let Some(reason) = refusal {
            let entry = pending.remove(&id).unwrap();
            drop(pending);
            if let Some(reply) = entry.reply {
                let _ = reply.send(lsp_types::ApplyWorkspaceEditResponse { applied: false,
                    failure_reason: Some(reason.into()), failed_change: None });
            }
            return Err(reason.into());
        }
        let reply = entry.reply.take();
        Ok(LspServerEditLease { id, edits: Arc::clone(&self.server_edits), reply })
    }
    /// Completes an unclaimed preview, for rejection or a successful no-op.
    /// Actual mutations must first claim and complete a publication lease.
    pub fn respond_workspace_edit(&mut self, id:LspServerEditId, applied:bool, failure_reason:Option<String>) {
        if let Some(entry) = self.server_edits.remove_preview(id) {
            if let Some(reply) = entry.reply {
                let _ = reply.send(lsp_types::ApplyWorkspaceEditResponse { applied, failure_reason, failed_change:None });
            }
        }
    }
}

struct ServerEditGuard { id: LspServerEditId, edits: Arc<ServerEdits> }
impl Drop for ServerEditGuard {
    fn drop(&mut self) { self.edits.remove_preview(self.id); }
}

pub(super) fn apply_workspace_edit(state:&mut RouterState, params:lsp_types::ApplyWorkspaceEditParams)
    -> impl std::future::Future<Output=Result<lsp_types::ApplyWorkspaceEditResponse,async_lsp::Error>>+Send+'static {
    apply_workspace_edit_with_timeout(state, params, Duration::from_secs(120))
}
fn apply_workspace_edit_with_timeout(state:&mut RouterState, params:lsp_types::ApplyWorkspaceEditParams, timeout:Duration)
    -> impl std::future::Future<Output=Result<lsp_types::ApplyWorkspaceEditResponse,async_lsp::Error>>+Send+'static {
    let deadline = std::time::Instant::now() + timeout;
    let result=(||{
        if state.retired.load(Ordering::Acquire){return Err("language server retired".to_owned());}
        let doc=state.snapshots.docs_for_session(&state.key).into_iter().next().ok_or("no open document owns this language server")?;
        let versions=state.snapshots.observations_for_session(&state.key);
        let edit=convert_workspace_edit(&state.snapshots,params.edit,Some(&versions)).map_err(|e|e.to_string())?;
        let (id,rx)=state.server_edits.preview(doc,deadline,Arc::clone(&state.retired))?;
        state.events.emit_now(LspEvent::ApplyWorkspaceEdit{doc,id,label:params.label,edit});Ok((id,rx))
    })();
    let guard=result.as_ref().ok().map(|(id,_)|ServerEditGuard{id:*id,edits:Arc::clone(&state.server_edits)});
    let server_edits = Arc::clone(&state.server_edits);
    async move {
        let _guard=guard;
        let failure=|message|lsp_types::ApplyWorkspaceEditResponse{applied:false,failure_reason:Some(message),failed_change:None};
        Ok(match result {
            Err(message)=>failure(message),
            Ok((id,mut rx))=>{
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match tokio::time::timeout(remaining, &mut rx).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(_)) => failure("workspace-edit preview closed".into()),
                    Err(_) => {
                        // Timeout and claim arbitrate under the same mutex.
                        // Removing only Preview cannot defeat an already
                        // admitted write or drop its receiver prematurely.
                        if let Some(entry) = server_edits.remove_preview(id) {
                            if let Some(reply) = entry.reply {
                                let _ = reply.send(failure("workspace-edit preview timed out".into()));
                            }
                        }
                        rx.await.unwrap_or_else(|_| failure("workspace-edit publication closed".into()))
                    }
                }
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
        Self::CodeActions{diagnostics,..}=>serde_json::to_vec(diagnostics).map_or(MAX_CONTENT_LENGTH,|bytes|bytes.len()),
        Self::Resolve{action,observed}=>serde_json::to_vec(action).map_or(MAX_CONTENT_LENGTH,|b|b.len()).saturating_add(observed.iter().map(|(uri,_)|uri.as_str().len().saturating_add(std::mem::size_of::<ObservedDocument>())).sum::<usize>()),
        Self::Execute(c)=>c.command.len()+c.arguments.iter().map(|a|serde_json::to_vec(a).map_or(MAX_CONTENT_LENGTH,|b|b.len())).sum::<usize>(),
        Self::Signature{trigger,active,..}=>trigger.as_ref().map_or(0,String::len).saturating_add(active.as_ref().map_or(0,|help|serde_json::to_vec(help).map_or(MAX_CONTENT_LENGTH,|bytes|bytes.len()))),
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
        Self{title,kind:None,is_preferred:false,disabled:None,edit,command,needs_resolve:false,raw,origin_version:-1,observed_documents:Arc::new(HashMap::new())}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn code_action_queue_accounts_for_opaque_diagnostic_metadata() {
        let diagnostic=lsp_types::Diagnostic {message:"short".into(),data:Some(json!({"opaque":"x".repeat(1024*1024)})),..Default::default()};
        let args=RequestArgs::CodeActions {range:Default::default(),diagnostics:vec![diagnostic]};
        assert!(args.queued_bytes()>=1024*1024,"opaque diagnostic data is retained by each queued request");
    }

    #[test]
    fn code_actions_can_resolve_an_edit_even_when_a_command_is_already_present() {
        let store=SnapshotStore::default();let uri=Arc::new(Url::parse("file:///repo/a.rs").unwrap());let key=SessionKey{root:Arc::from(Path::new("/repo")),server_id:Arc::from("rust")};
        store.register(LspDocumentId(1),&uri,&key,1,Arc::from("foo")).unwrap();
        let request=PendingRequest{id:LspRequestId(9),doc:LspDocumentId(1),uri,worktree:Arc::from(Path::new("/repo")),kind:RequestKind::CodeActions,position:lsp_types::Position::default(),version:1,args:RequestArgs::None,timeout:Duration::from_secs(15),observed_versions:store.observations_for_session(&key)};
        let raw:lsp_types::CodeActionOrCommand=serde_json::from_value(json!({"title":"Fix and run","command":{"title":"Run","command":"server.run"},"data":{"resolve_edit":true}})).unwrap();
        let item=convert_action(&store,&request,raw).unwrap();assert!(item.needs_resolve);assert!(item.command.is_some());assert!(item.edit.is_none());
        let sibling=convert_action(&store,&request,item.raw.clone()).unwrap();
        assert!(Arc::ptr_eq(&item.observed_documents,&request.observed_versions));
        assert!(Arc::ptr_eq(&item.observed_documents,&sibling.observed_documents));
    }

    #[test]
    fn command_acknowledgement_survives_its_own_workspace_edit(){
        let store=SnapshotStore::default();let uri=Arc::new(Url::parse("file:///repo/a.rs").unwrap());let key=SessionKey{root:Arc::from(Path::new("/repo")),server_id:Arc::from("rust")};
        store.register(LspDocumentId(1),&uri,&key,2,Arc::from("fixed"));
        let request=PendingRequest{id:LspRequestId(9),doc:LspDocumentId(1),uri,worktree:Arc::from(Path::new("/repo")),kind:RequestKind::ExecuteCommand,position:lsp_types::Position::default(),version:1,args:RequestArgs::None,timeout:Duration::from_secs(15),observed_versions:Arc::new(HashMap::new())};
        assert!(matches!(convert_result(&store,&request,RawResult::Feature(RawFeatureResult::CommandExecuted(None))),LspEvent::CommandExecuted{request:LspRequestId(9),..}));
    }

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
        assert!(converted.requires_confirmation);assert_eq!(converted.annotations[0].label,"Rename export");assert!(converted.annotations[0].needs_confirmation);assert_eq!(converted.documents[0].edits[0].range,LspRange::new(LspPosition::new(0,1),LspPosition::new(0,2)));
        let mut oversized=value.clone();oversized["changeAnnotations"]["confirm"]["label"]=json!("x".repeat(1025));assert!(matches!(convert_workspace_edit(&store,serde_json::from_value(oversized).unwrap(),None),Err(LspError::Unsupported(_))));
        let mut oversized=value.clone();oversized["changeAnnotations"]["confirm"]["description"]=json!("x".repeat(4097));assert!(matches!(convert_workspace_edit(&store,serde_json::from_value(oversized).unwrap(),None),Err(LspError::Unsupported(_))));
        let mut missing=value.clone();missing["changeAnnotations"]=json!({});assert!(matches!(convert_workspace_edit(&store,serde_json::from_value(missing).unwrap(),None),Err(LspError::Unsupported(_))));
        let mut split=value;split["documentChanges"][0]["edits"][0]["range"]["start"]["character"]=json!(2);assert!(matches!(convert_workspace_edit(&store,serde_json::from_value(split).unwrap(),None),Err(LspError::SplitSurrogate{..})));
        let duplicate: lsp_types::WorkspaceEdit=serde_json::from_value(json!({"documentChanges":[{"textDocument":{"uri":uri.as_str(),"version":4},"edits":[]},{"textDocument":{"uri":uri.as_str(),"version":4},"edits":[]}]})).unwrap();assert!(matches!(convert_workspace_edit(&store,duplicate,None),Err(LspError::Unsupported(_))));
    }

    fn server_edit_fixture() -> (LspManager, RouterState) {
        let manager = LspManager::new(Vec::new());
        let uri = Arc::new(Url::parse("file:///repo/a.rs").unwrap());
        let key = SessionKey { root:Arc::from(Path::new("/repo")), server_id:Arc::from("rust") };
        manager.snapshots.register(LspDocumentId(1), &uri, &key, 1, Arc::from("foo"));
        let router = RouterState { key, snapshots:Arc::clone(&manager.snapshots), events:manager.events.clone(),
            retired:Arc::new(AtomicBool::new(false)), server_edits:Arc::clone(&manager.server_edits), configuration:serde_json::Value::Null };
        (manager, router)
    }
    fn latest_server_edit(manager: &LspManager) -> LspServerEditId {
        *manager.server_edits.pending.lock().unwrap().keys().max_by_key(|id|id.0).unwrap()
    }
    fn empty_server_edit() -> lsp_types::ApplyWorkspaceEditParams {
        lsp_types::ApplyWorkspaceEditParams { label:None, edit:Default::default() }
    }

    #[test]
    fn expired_server_edit_preview_cannot_claim_publication_authority() {
        let (mut manager, mut router) = server_edit_fixture();
        let request = apply_workspace_edit_with_timeout(&mut router, empty_server_edit(), Duration::ZERO);
        let id = latest_server_edit(&manager);
        assert!(manager.claim_workspace_edit(id).unwrap_err().contains("expired"));
        let response = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(request).unwrap();
        assert!(!response.applied);
        assert!(manager.server_edits.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn late_server_edit_admission_cannot_reopen_closed_or_stopping_authority() {
        let (mut manager, _) = server_edit_fixture();
        manager.snapshots.unregister(LspDocumentId(1));
        let (id, mut response) = manager.server_edits.preview(LspDocumentId(1), std::time::Instant::now()+Duration::from_secs(120), Arc::new(AtomicBool::new(false))).unwrap();
        assert!(manager.claim_workspace_edit(id).unwrap_err().contains("source was closed"));
        assert!(!response.try_recv().unwrap().applied);
        manager.shutdown();
        let (id, mut response) = manager.server_edits.preview(LspDocumentId(1), std::time::Instant::now()+Duration::from_secs(120), Arc::new(AtomicBool::new(false))).unwrap();
        assert!(manager.claim_workspace_edit(id).unwrap_err().contains("shutting down"));
        assert!(!response.try_recv().unwrap().applied);
        assert!(manager.server_edits.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn claimed_server_edit_owns_reply_across_preview_timeout_close_and_shutdown() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (mut manager, mut router) = server_edit_fixture();
            let mut request = Box::pin(apply_workspace_edit_with_timeout(&mut router, empty_server_edit(), Duration::from_millis(10)));
            let id = latest_server_edit(&manager);
            let lease = manager.claim_workspace_edit(id).unwrap();
            assert!(manager.claim_workspace_edit(id).is_err());
            manager.server_edits.close_doc(LspDocumentId(1));
            manager.shutdown();
            manager.respond_workspace_edit(id, false, Some("stale preview cancellation".into()));
            assert_eq!(manager.server_edits.pending.lock().unwrap().len(), 1);
            assert!(tokio::time::timeout(Duration::from_millis(40), &mut request).await.is_err(),
                "preview expiry/close cannot reject already admitted publication");
            lease.respond(true, None);
            assert!(request.await.unwrap().applied);
            assert!(manager.server_edits.pending.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn retired_server_cannot_claim_preview_but_keeps_already_admitted_publication() {
        let (mut manager, mut router) = server_edit_fixture();
        let request = apply_workspace_edit(&mut router, empty_server_edit());
        let id = latest_server_edit(&manager);
        router.retired.store(true, Ordering::Release);
        assert!(manager.claim_workspace_edit(id).unwrap_err().contains("server retired"));
        let response = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(request).unwrap();
        assert!(!response.applied);
        router.retired = Arc::new(AtomicBool::new(false));
        let request = apply_workspace_edit(&mut router, empty_server_edit());
        let lease = manager.claim_workspace_edit(latest_server_edit(&manager)).unwrap();
        router.retired.store(true, Ordering::Release);
        lease.respond(true, None);
        let response = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(request).unwrap();
        assert!(response.applied, "retirement cannot revoke a previously admitted publication");
    }

    #[test]
    fn dropped_publication_lease_rejects_without_leaking_pending_budget() {
        let (mut manager, mut router) = server_edit_fixture();
        let request = apply_workspace_edit(&mut router, empty_server_edit());
        let id = latest_server_edit(&manager);
        let lease = manager.claim_workspace_edit(id).unwrap();
        drop(lease);
        let response = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(request).unwrap();
        assert!(!response.applied);
        assert!(response.failure_reason.unwrap().contains("publication was cancelled"));
        assert!(manager.server_edits.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn cancelled_server_request_keeps_claimed_budget_until_publication_finishes() {
        let (mut manager, mut router) = server_edit_fixture();
        let request = apply_workspace_edit(&mut router, empty_server_edit());
        let lease = manager.claim_workspace_edit(latest_server_edit(&manager)).unwrap();
        drop(request);
        assert_eq!(manager.server_edits.pending.lock().unwrap().len(), 1);
        drop(lease);
        assert!(manager.server_edits.pending.lock().unwrap().is_empty());
        let request = apply_workspace_edit(&mut router, empty_server_edit());
        manager.server_edits.close_doc(LspDocumentId(1));
        let response = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(request).unwrap();
        assert!(!response.applied, "document close still cancels an unclaimed preview");
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
