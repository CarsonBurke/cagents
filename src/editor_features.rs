//! Shared native language operations for the ADE and the standalone editor.
//! Preparation and path validation run on workers. Native edits are staged,
//! replayed, and committed together in one reducer turn under the actual server
//! publication lease. This controller refuses unopened targets before effects.
use crate::{
    editor_workspace_edit as workspace,
    lsp::*,
    model::{CardId, RepositoryView},
    repository::CancellationToken,
    repository_widget::{RepositoryAction, RepositoryRuntime},
};
use cosmic::{
    Element,
    iced::{self, Length, Task},
    widget,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

fn control_id(ticket: u64, control: &str) -> widget::Id {
    widget::Id::new(format!("editor.language.{ticket}.{control}"))
}

struct CapturedOpen {
    path: PathBuf,
    source: crate::repository_widget::text_editor::TextSnapshot,
    version: i32,
    protocol_version: Option<i32>,
    incarnation: LspDocumentIncarnation,
}
/// Complete source text is serialized only inside the preparation worker.
fn materialize_snapshots(
    captured: Vec<CapturedOpen>,
) -> Result<HashMap<PathBuf, workspace::OpenSnapshot>, String> {
    let mut open = HashMap::<PathBuf, workspace::OpenSnapshot>::new();
    for captured in captured {
        let snapshot = workspace::OpenSnapshot {
            text: captured.source.text(),
            version: captured.version,
            protocol_version: captured.protocol_version,
            protocol_incarnation: Some(captured.incarnation),
            writable: true,
        };
        if let Some(previous) = open.insert(captured.path, snapshot.clone()) {
            if previous.text != snapshot.text
                || previous.version != snapshot.version
                || previous.protocol_version != snapshot.protocol_version
                || previous.protocol_incarnation != snapshot.protocol_incarnation
            {
                return Err("Two open views of the same file disagree".into());
            }
        }
    }
    Ok(open)
}

mod dialog;

type Buffers<'a> = [(CardId, &'a mut RepositoryRuntime)];

#[derive(Debug, Clone)]
pub enum Action {
    Rename(CardId),
    RenameDraft {
        ticket: u64,
        value: String,
    },
    RenameSubmit {
        ticket: u64,
        value: String,
    },
    Format {
        card: CardId,
        selection: bool,
        save_after: bool,
    },
    CodeActions(CardId),
    ChooseAction {
        ticket: u64,
        index: usize,
    },
    Signature {
        card: CardId,
        trigger: Option<String>,
    },
    Event(LspEvent),
    Targets {
        ticket: u64,
        result: Result<Arc<workspace::ResolvedTargets>, String>,
    },
    Prepared {
        ticket: u64,
        result: Result<Arc<workspace::Plan>, String>,
    },
    Validated {
        ticket: u64,
        result: Result<(), String>,
    },
    PreviewFile {
        ticket: u64,
        index: usize,
    },
    PreviewDiff {
        ticket: u64,
        selected: usize,
        result: Result<Option<Arc<crate::diff_view::DiffModel>>, String>,
    },
    PreviewRepository {
        ticket: u64,
        action: RepositoryAction,
    },
    AnnotationPage {
        ticket: u64,
        page: usize,
    },
    Apply(u64),
    Cancel(u64),
}

#[derive(Clone)]
struct Operation {
    card: CardId,
    doc: LspDocumentId,
    version: i32,
    incarnation: LspDocumentIncarnation,
    at: LspPosition,
    selection: Option<LspRange>,
    view: RepositoryView,
    kind: LspEditKind,
    save_after: bool,
    command: Option<LspCommand>,
    command_context: Option<LspCodeActionContext>,
    server: Option<LspServerEditId>,
    annotations: Vec<LspChangeAnnotation>,
    confirmation: bool,
    open_files: Vec<(CardId, PathBuf)>,
    original_root: Arc<crate::fs_safety::Root>,
}
struct Rename {
    ticket: u64,
    operation: Operation,
    draft: String,
}
struct Choices {
    ticket: u64,
    operation: Operation,
    items: Vec<LspCodeAction>,
}
struct Preview {
    ticket: u64,
    operation: Operation,
    plan: Arc<workspace::Plan>,
    selected: usize,
    page: usize,
    diff_ticket: u64,
    card: CardId,
    runtime: Option<RepositoryRuntime>,
    cancel: CancellationToken,
    validating: bool,
    error: Option<String>,
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    RenameStart,
    Rename,
    Format,
    Actions,
    Resolve,
    Signature,
    Command,
}
#[derive(Default)]
pub struct State {
    next: u64,
    requests: HashMap<LspRequestId, (RequestKind, Operation)>,
    preparing: Option<(u64, Operation)>,
    rename: Option<Rename>,
    choices: Option<Choices>,
    preview: Option<Preview>,
}

/// Current shell policy; it never replaces per-document or protocol proof.
#[derive(Clone, Copy)]
pub struct Admission {
    pub writable: bool,
    pub other_modal: bool,
    pub close_save: Option<CardId>,
    /// The shell's active, mounted source pane. Only interactive choices use
    /// this scope; an inactive buffer may still finish formatting or Save.
    pub choice_card: Option<CardId>,
}
impl Admission {
    fn permits(self, operation: &Operation) -> bool {
        self.writable
            && (!self.other_modal
                || (operation.save_after && self.close_save == Some(operation.card)))
    }
    fn permits_choice(self, operation: &Operation) -> bool {
        self.permits(operation) && self.choice_card == Some(operation.card)
    }
}

/// The shell drains native/LSP receipts before completing this publication and
/// requesting an ordinary protected Save. All three happen in this same turn.
#[derive(Default)]
pub struct Effects {
    pub task: Task<Action>,
    pub repository_tasks: Vec<Task<RepositoryAction>>,
    pub changed: Vec<CardId>,
    pub focus: Option<CardId>,
    pub choice_restore: Option<CardId>,
    pub save: Vec<CardId>,
    pub notices: Vec<(LspMessageLevel, String)>,
    completion: Option<Completion>,
}
struct Completion {
    operation: Operation,
    lease: Option<LspServerEditLease>,
}
impl Effects {
    fn error(&mut self, message: impl Into<String>) {
        self.notices.push((LspMessageLevel::Error, message.into()));
    }
}

/// Restore the source after an interactive dialog only while the current
/// native tree has no newer foreign keyboard owner.
pub fn restore_choice_focus(target: widget::Id) -> Task<Action> {
    dialog::restore(target)
}

fn buffer<'a, 'b>(buffers: &'b Buffers<'a>, card: CardId) -> Option<&'b RepositoryRuntime> {
    buffers
        .iter()
        .find(|(id, _)| *id == card)
        .map(|(_, runtime)| &**runtime)
}
fn buffer_mut<'a, 'b>(
    buffers: &'b mut Buffers<'a>,
    card: CardId,
) -> Option<&'b mut RepositoryRuntime> {
    buffers
        .iter_mut()
        .find(|(id, _)| *id == card)
        .map(|(_, runtime)| &mut **runtime)
}
fn open_files(buffers: &Buffers<'_>) -> Vec<(CardId, PathBuf)> {
    let mut files = buffers
        .iter()
        .filter_map(|(card, runtime)| Some((*card, runtime.workspace_edit_candidate_source()?)))
        .collect::<Vec<_>>();
    files.sort_by_key(|(card, _)| card.to_string());
    files
}
impl State {
    fn ticket(&mut self) -> Option<u64> {
        self.next = self.next.checked_add(1)?;
        Some(self.next)
    }
    pub fn pending(&self) -> bool {
        self.dialog_open() || self.preparing.is_some() || !self.requests.is_empty()
    }
    pub fn dialog_open(&self) -> bool {
        self.rename.is_some() || self.choices.is_some() || self.preview.is_some()
    }
    pub fn save_close_card(&self) -> Option<CardId> {
        self.preview
            .as_ref()
            .filter(|preview| preview.operation.save_after)
            .map(|preview| preview.operation.card)
    }
    pub fn escape(&self) -> Option<Action> {
        self.preview
            .as_ref()
            .map(|p| Action::Cancel(p.ticket))
            .or_else(|| self.rename.as_ref().map(|r| Action::Cancel(r.ticket)))
            .or_else(|| self.choices.as_ref().map(|c| Action::Cancel(c.ticket)))
    }
    pub fn consumes(&self, event: &LspEvent) -> bool {
        match event {
            LspEvent::PrepareRename { .. }
            | LspEvent::WorkspaceEdit { .. }
            | LspEvent::CodeActions { .. }
            | LspEvent::CodeActionResolved { .. }
            | LspEvent::SignatureHelp { .. }
            | LspEvent::ServerMessage { .. }
            | LspEvent::ApplyWorkspaceEdit { .. }
            | LspEvent::CommandExecuted { .. } => true,
            LspEvent::RequestFailed { request, doc, .. } => self
                .requests
                .get(request)
                .is_some_and(|(_, operation)| operation.doc == *doc),
            _ => false,
        }
    }
    pub fn cancel_all(&mut self, lsp: &mut LspManager) {
        for (request, _) in self.requests.drain() {
            lsp.cancel(request);
        }
        if let Some((_, operation)) = self.preparing.take() {
            Self::reject(&operation, lsp, "The editor operation was canceled");
        }
        if let Some(preview) = self.preview.take() {
            Self::reject(&preview.operation, lsp, "The preview was canceled");
        }
        self.rename = None;
        self.choices = None;
    }
    fn operation(
        &self,
        card: CardId,
        kind: LspEditKind,
        lsp: &LspManager,
        buffers: &Buffers<'_>,
    ) -> Option<Operation> {
        let runtime = buffer(buffers, card)?;
        if !runtime.editor_feature_writable() {
            return None;
        }
        let doc = runtime.lsp_document()?;
        Some(Operation {
            card,
            doc,
            version: lsp.document_version(doc)?,
            incarnation: lsp.document_incarnation(doc)?,
            at: runtime.cursor_position().unwrap_or_default(),
            selection: runtime.editor_selection_range(),
            view: runtime.descriptor.clone(),
            kind,
            save_after: false,
            command: None,
            command_context: None,
            server: None,
            annotations: Vec::new(),
            confirmation: false,
            open_files: open_files(buffers),
            original_root: runtime.retained_editor_root()?,
        })
    }
    fn current(operation: &Operation, lsp: &LspManager, buffers: &Buffers<'_>) -> bool {
        buffer(buffers, operation.card).is_some_and(|runtime| {
            runtime.editor_feature_writable()
                && runtime.lsp_document() == Some(operation.doc)
                && runtime
                    .retained_editor_root()
                    .is_some_and(|root| Arc::ptr_eq(&root, &operation.original_root))
        }) && lsp.document_version(operation.doc) == Some(operation.version)
            && lsp.document_incarnation(operation.doc).as_ref() == Some(&operation.incarnation)
    }
    fn choice_current(operation: &Operation, lsp: &LspManager, buffers: &Buffers<'_>) -> bool {
        Self::current(operation, lsp, buffers)
            && buffer(buffers, operation.card).is_some_and(|runtime| {
                runtime.cursor_position() == Some(operation.at)
                    && runtime.editor_selection_range() == operation.selection
            })
    }
    fn reject(operation: &Operation, lsp: &mut LspManager, reason: &str) {
        if let Some(id) = operation.server {
            lsp.respond_workspace_edit(id, false, Some(reason.into()));
        }
    }
    fn take_request(
        &mut self,
        request: LspRequestId,
        doc: LspDocumentId,
        kind: RequestKind,
    ) -> Option<Operation> {
        if !self
            .requests
            .get(&request)
            .is_some_and(|(expected, operation)| *expected == kind && operation.doc == doc)
        {
            return None;
        }
        self.requests
            .remove(&request)
            .map(|(_, operation)| operation)
    }
    fn track(
        &mut self,
        request: LspRequestId,
        kind: RequestKind,
        operation: Operation,
        lsp: &mut LspManager,
    ) {
        let obsolete = self
            .requests
            .iter()
            .filter_map(|(id, (old_kind, old))| {
                // A newer interactive intent owns the next choice dialog,
                // even when it names a different pane or a different kind of
                // choice. Submitted edits/commands and background formatting
                // keep their independent lifetime and publication authority.
                ((matches!(kind, RequestKind::RenameStart | RequestKind::Actions)
                    && matches!(old_kind, RequestKind::RenameStart | RequestKind::Actions))
                    || (old.card == operation.card && *old_kind == kind))
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in obsolete {
            self.requests.remove(&id);
            lsp.cancel(id);
        }
        self.requests.insert(request, (kind, operation));
    }
    fn prepare(
        &mut self,
        mut operation: Operation,
        edit: LspWorkspaceEdit,
        lsp: &mut LspManager,
        effects: &mut Effects,
    ) {
        if self.preparing.is_some() || self.dialog_open() {
            Self::reject(&operation, lsp, "Another workspace edit is open");
            effects.error("Finish the current workspace edit first");
            return;
        }
        let Some(ticket) = self.ticket() else {
            Self::reject(
                &operation,
                lsp,
                "Editor operation identity space is exhausted",
            );
            return;
        };
        operation.annotations = edit.annotations;
        operation.confirmation = edit.requires_confirmation
            || operation
                .annotations
                .iter()
                .any(|annotation| annotation.needs_confirmation);
        let root = operation.original_root.clone();
        let open = operation
            .open_files
            .iter()
            .map(|(_, path)| path.clone())
            .collect();
        let documents = edit
            .documents
            .into_iter()
            .map(|document| workspace::DocumentEdits {
                path: document.path,
                version: document.version,
                observed_version: document.observed_version,
                observed_incarnation: document.observed_incarnation,
                coordinates: document.coordinates,
                edits: document.edits,
            })
            .collect();
        self.preparing = Some((ticket, operation));
        effects.task = cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                workspace::resolve_targets_at(root, documents, open).map(Arc::new)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Action::Targets { ticket, result }
        });
    }
    fn snapshots(
        paths: &[PathBuf],
        lsp: &LspManager,
        buffers: &Buffers<'_>,
    ) -> Result<Vec<CapturedOpen>, String> {
        let mut open = Vec::new();
        let mut bytes = 0usize;
        for (_, runtime) in buffers {
            if !runtime
                .workspace_edit_candidate_source()
                .is_some_and(|path| paths.contains(&path))
            {
                continue;
            }
            bytes = bytes.saturating_add(runtime.workspace_edit_snapshot_size());
            if bytes > 64 * 1024 * 1024 {
                return Err("The open targets exceed the 64 MiB preview budget".into());
            }
            if !runtime.editor_feature_writable() {
                return Err("The target is not a current writable original-root buffer".into());
            }
            let (path, source, version) = runtime
                .workspace_edit_native_snapshot()
                .ok_or("Wait for the target to finish loading")?;
            let doc = runtime
                .lsp_document()
                .ok_or("An open target is not synchronized with its language server")?;
            let incarnation = lsp
                .document_incarnation(doc)
                .ok_or("An edited document was closed or reopened")?;
            open.push(CapturedOpen {
                path,
                source,
                version,
                protocol_version: lsp.document_version(doc),
                incarnation,
            });
        }
        Ok(open)
    }
    fn select_preview(
        &mut self,
        index: usize,
        theme: crate::settings::EditorSyntaxTheme,
        effects: &mut Effects,
    ) {
        let Some(diff_ticket) = self.ticket() else {
            return;
        };
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        let Some(document) = preview.plan.documents.get(index).cloned() else {
            return;
        };
        preview.cancel.cancel();
        preview.cancel = CancellationToken::new();
        preview.runtime = None;
        preview.error = None;
        preview.selected = index;
        preview.diff_ticket = diff_ticket;
        let cancel = preview.cancel.clone();
        effects.task = cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::diff_view::build_document_diff(
                    document.before,
                    &document.after,
                    &document.relative,
                    theme,
                    &cancel,
                )
                .map(Arc::new)
            })
            .await
            .map_err(|error| error.to_string());
            Action::PreviewDiff {
                ticket: diff_ticket,
                selected: index,
                result,
            }
        });
    }
    /// After all shells have drained native receipts, hold the actual server
    /// lease through its reply and queue an already-admitted command immediately.
    pub fn finish_effects(&mut self, effects: &mut Effects, lsp: &mut LspManager) {
        let Some(mut completion) = effects.completion.take() else {
            return;
        };
        if let Some(lease) = completion.lease.take() {
            lease.respond(true, None);
        }
        if let Some(command) = completion.operation.command.take() {
            let request = lsp.execute_command(completion.operation.doc, command);
            completion.operation.save_after = false;
            completion.operation.command_context = None;
            completion.operation.server = None;
            self.track(request, RequestKind::Command, completion.operation, lsp);
        }
    }
    fn publish(
        &mut self,
        operation: Operation,
        plan: Arc<workspace::Plan>,
        lsp: &mut LspManager,
        buffers: &mut Buffers<'_>,
        admission: Admission,
        recovery: &crate::operation_recovery::Shared,
        effects: &mut Effects,
    ) {
        if !admission.permits(&operation)
            || !Self::current(&operation, lsp, buffers)
            || operation.open_files != open_files(buffers)
        {
            Self::reject(&operation, lsp, "The source or open target set changed");
            effects.error("The source or open tabs changed; prepare the edit again");
            return;
        }
        if let Some(context) = &operation.command_context {
            if let Err(error) = lsp.validate_code_action_context_token(context) {
                Self::reject(&operation, lsp, &error.to_string());
                effects.error(error.to_string());
                return;
            }
        }
        let lease = match operation.server {
            Some(id) => match lsp.claim_workspace_edit(id) {
                Ok(lease) => Some(lease),
                Err(error) => {
                    effects.error(error.to_string());
                    return;
                }
            },
            None => None,
        };
        let mut matched = HashSet::new();
        let mut staged = Vec::new();
        let staging = (|| -> Result<(), String> {
            for (card, runtime) in buffers.iter_mut() {
                let Some(path) = runtime.workspace_edit_candidate_source() else {
                    continue;
                };
                let Some((index, document)) = plan
                    .documents
                    .iter()
                    .enumerate()
                    .find(|(_, document)| document.open_paths.contains(&path))
                else {
                    continue;
                };
                if recovery
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .blocks(&runtime.descriptor.worktree)
                {
                    return Err(
                        "Resolve the retained file operation before applying language edits".into(),
                    );
                }
                let doc = runtime
                    .lsp_document()
                    .ok_or("An edited buffer lost its language server")?;
                if lsp.document_version(doc) != document.protocol_version
                    || lsp.document_incarnation(doc) != document.protocol_incarnation
                {
                    return Err("An edited document changed or was reopened".into());
                }
                matched.insert(index);
                staged.push((*card, runtime.prepare_workspace_edit(document)?));
            }
            if matched.len() != plan.documents.len() {
                return Err("An edited buffer was closed".into());
            }
            Ok(())
        })();
        if let Err(error) = staging {
            if let Some(lease) = lease {
                lease.respond(false, Some(error.clone()));
            }
            effects.error(error);
            return;
        }
        let mut applied = Vec::new();
        let mut error = None;
        for (card, prepared) in staged {
            match buffer_mut(buffers, card)
                .expect("staged membership cannot change in one turn")
                .apply_workspace_edit(prepared)
            {
                Ok(receipt) => applied.push((card, receipt)),
                Err(cause) => {
                    effects.changed.push(card);
                    error = Some(cause);
                    break;
                }
            }
        }
        if error.is_none()
            && applied.iter().any(|(card, receipt)| {
                !buffer(buffers, *card)
                    .is_some_and(|runtime| runtime.workspace_commit_current(receipt))
            })
        {
            error = Some("The native edit did not remain in its original update turn".into());
        }
        if let Some(mut error) = error {
            for (card, receipt) in applied.into_iter().rev() {
                if let Err(cause) = buffer_mut(buffers, card)
                    .expect("applied membership is fixed")
                    .rollback_workspace_edit(receipt)
                {
                    error.push_str(&format!("; {cause}"));
                    effects.changed.push(card);
                }
            }
            if let Some(lease) = lease {
                lease.respond(false, Some(error.clone()));
            }
            effects.error(error);
            return;
        }
        // No await, user callback, membership change or native edit occurs
        // between the all-buffer validation above and these infallible commits.
        for (card, receipt) in applied {
            let runtime = buffer_mut(buffers, card).expect("validated membership is fixed");
            runtime
                .commit_workspace_edit(receipt)
                .expect("validated same-turn native commit invariant");
            effects.changed.push(card);
            effects
                .repository_tasks
                .push(runtime.programmatic_edit_task(card));
        }
        if operation.save_after {
            effects.save.push(operation.card);
        }
        effects.focus = Some(operation.card);
        effects.completion = Some(Completion { operation, lease });
    }
    pub fn update(
        &mut self,
        action: Action,
        lsp: &mut LspManager,
        buffers: &mut Buffers<'_>,
        admission: Admission,
        recovery: &crate::operation_recovery::Shared,
        theme: crate::settings::EditorSyntaxTheme,
    ) -> Effects {
        let mut effects = Effects::default();
        match action {
            Action::Rename(card) | Action::CodeActions(card) => {
                if !admission.writable
                    || admission.other_modal
                    || self.dialog_open()
                    || self.preparing.is_some()
                {
                    return effects;
                }
                let rename = matches!(action, Action::Rename(_));
                let Some(operation) = self.operation(
                    card,
                    if rename {
                        LspEditKind::Rename
                    } else {
                        LspEditKind::CodeAction
                    },
                    lsp,
                    buffers,
                ) else {
                    return effects;
                };
                if !admission.permits_choice(&operation) {
                    return effects;
                }
                let request = if rename {
                    lsp.request_prepare_rename(operation.doc, operation.at)
                } else {
                    let range = operation
                        .selection
                        .unwrap_or(LspRange::new(operation.at, operation.at));
                    lsp.request_code_actions(
                        operation.doc,
                        range,
                        buffer(buffers, card).unwrap().editor_diagnostics(),
                    )
                };
                self.track(
                    request,
                    if rename {
                        RequestKind::RenameStart
                    } else {
                        RequestKind::Actions
                    },
                    operation,
                    lsp,
                );
            }
            Action::Format {
                card,
                selection,
                save_after,
            } => {
                let Some(mut operation) = self.operation(
                    card,
                    if selection {
                        LspEditKind::RangeFormatting
                    } else {
                        LspEditKind::Formatting
                    },
                    lsp,
                    buffers,
                ) else {
                    return effects;
                };
                operation.save_after = save_after;
                if !admission.permits(&operation) || self.dialog_open() || self.preparing.is_some()
                {
                    return effects;
                }
                if save_after
                    && self
                        .requests
                        .values()
                        .any(|(_, pending)| pending.card == card && pending.save_after)
                {
                    return effects;
                }
                let timeout = lsp
                    .formatting_settings(operation.doc)
                    .map_or(Duration::from_secs(5), |(_, timeout)| timeout);
                let request = if selection {
                    let Some(range) = operation.selection else {
                        effects.error("Select source text to format a range");
                        return effects;
                    };
                    lsp.request_range_formatting(
                        operation.doc,
                        range,
                        LspFormattingOptions::default(),
                        timeout,
                    )
                } else {
                    lsp.request_formatting(operation.doc, LspFormattingOptions::default(), timeout)
                };
                self.track(request, RequestKind::Format, operation, lsp);
            }
            Action::Signature { card, trigger } => {
                if !admission.writable || admission.other_modal {
                    return effects;
                }
                let Some(operation) = self.operation(card, LspEditKind::CodeAction, lsp, buffers)
                else {
                    return effects;
                };
                let active = buffer(buffers, card)
                    .and_then(|runtime| runtime.editor_signature())
                    .cloned();
                let trigger = trigger.filter(|value| {
                    value.chars().next().is_some_and(|ch| {
                        lsp.signature_trigger_with_context(operation.doc, ch, active.is_some())
                    })
                });
                let request = lsp.request_signature_help_retrigger(
                    operation.doc,
                    operation.at,
                    trigger,
                    active.is_some(),
                    active,
                );
                self.track(request, RequestKind::Signature, operation, lsp);
            }
            Action::RenameDraft { ticket, value } => {
                if admission.writable
                    && !admission.other_modal
                    && value.len() <= 4096
                    && let Some(rename) = self
                        .rename
                        .as_mut()
                        .filter(|rename| rename.ticket == ticket)
                {
                    rename.draft = value;
                }
            }
            Action::RenameSubmit { ticket, value } => {
                if !admission.writable
                    || admission.other_modal
                    || value.trim().is_empty()
                    || value.len() > 4096
                {
                    return effects;
                }
                if !self.rename.as_ref().is_some_and(|rename| {
                    rename.ticket == ticket && admission.permits_choice(&rename.operation)
                }) {
                    return effects;
                }
                let Some(rename) = self.rename.take() else {
                    return effects;
                };
                if rename.ticket != ticket {
                    self.rename = Some(rename);
                    return effects;
                }
                if !Self::choice_current(&rename.operation, lsp, buffers) {
                    effects.error("The source changed; request rename again");
                    return effects;
                }
                let request = lsp.request_rename(rename.operation.doc, rename.operation.at, value);
                self.track(request, RequestKind::Rename, rename.operation, lsp);
            }
            Action::ChooseAction { ticket, index } => {
                if !admission.writable || admission.other_modal {
                    return effects;
                }
                if !self.choices.as_ref().is_some_and(|choices| {
                    choices.ticket == ticket && admission.permits_choice(&choices.operation)
                }) {
                    return effects;
                }
                let Some(choices) = self.choices.take() else {
                    return effects;
                };
                if choices.ticket != ticket {
                    self.choices = Some(choices);
                    return effects;
                }
                if !Self::choice_current(&choices.operation, lsp, buffers) {
                    effects.error("The code action is no longer current");
                    return effects;
                }
                let Some(item) = choices
                    .items
                    .get(index)
                    .filter(|item| item.disabled.is_none())
                    .cloned()
                else {
                    return effects;
                };
                let mut operation = choices.operation;
                operation.command_context = Some(item.context());
                if let Err(error) = lsp.validate_code_action_context(&item) {
                    effects.error(error.to_string());
                    return effects;
                }
                if item.needs_resolve {
                    let request = lsp.request_resolve_code_action(operation.doc, item);
                    self.track(request, RequestKind::Resolve, operation, lsp);
                } else {
                    self.action_edit(operation, item, lsp, &mut effects);
                }
            }
            Action::Event(event) => self.event(event, lsp, buffers, admission, &mut effects),
            Action::Targets { ticket, result } => {
                let Some((expected, operation)) = self.preparing.as_ref() else {
                    return effects;
                };
                if *expected != ticket {
                    return effects;
                }
                let result = if !Self::current(operation, lsp, buffers) {
                    Err("The source changed while resolving targets".into())
                } else {
                    result
                };
                let open = match &result {
                    Ok(targets) => Self::snapshots(&targets.open_paths, lsp, buffers),
                    Err(error) => Err(error.clone()),
                };
                match result.and_then(|targets| open.map(|open| (targets, open))) {
                    Ok((targets, open)) => {
                        effects.task = cosmic::task::future(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                materialize_snapshots(open)
                                    .and_then(|open| {
                                        workspace::prepare_resolved(
                                            Arc::unwrap_or_clone(targets),
                                            &open,
                                        )
                                    })
                                    .map(Arc::new)
                            })
                            .await
                            .map_err(|error| error.to_string())
                            .and_then(|result| result);
                            Action::Prepared { ticket, result }
                        })
                    }
                    Err(error) => {
                        let (_, operation) = self.preparing.take().unwrap();
                        Self::reject(&operation, lsp, &error);
                        effects.error(error);
                        if operation.save_after
                            && admission.permits(&operation)
                            && Self::current(&operation, lsp, buffers)
                        {
                            effects.save.push(operation.card);
                        }
                    }
                }
            }
            Action::Prepared { ticket, result } => {
                let Some((expected, operation)) = self.preparing.take() else {
                    return effects;
                };
                if expected != ticket {
                    self.preparing = Some((expected, operation));
                    return effects;
                }
                let plan = match result {
                    Ok(plan) if Self::current(&operation, lsp, buffers) => plan,
                    Ok(_) => {
                        Self::reject(
                            &operation,
                            lsp,
                            "The source changed while preparing the edit",
                        );
                        effects.error("The source changed; request the language edit again");
                        return effects;
                    }
                    Err(error) => {
                        Self::reject(&operation, lsp, &error);
                        effects.error(error);
                        if operation.save_after
                            && admission.permits(&operation)
                            && Self::current(&operation, lsp, buffers)
                        {
                            effects.save.push(operation.card);
                        }
                        return effects;
                    }
                };
                if !admission.permits(&operation) {
                    Self::reject(
                        &operation,
                        lsp,
                        "The window changed while preparing the edit",
                    );
                    effects.error("The language edit was canceled because another dialog or window operation is active");
                    return effects;
                }
                let needs_preview = operation.kind == LspEditKind::Rename
                    || operation.confirmation
                    || plan.documents.len() > 1
                    || !admission.permits(&operation);
                if self.dialog_open() {
                    Self::reject(&operation, lsp, "Another language dialog is open");
                    effects.error("Finish the current language dialog first");
                    return effects;
                }
                self.preview = Some(Preview {
                    ticket,
                    operation,
                    plan,
                    selected: 0,
                    page: 0,
                    diff_ticket: 0,
                    card: CardId::new(),
                    runtime: None,
                    cancel: CancellationToken::new(),
                    validating: false,
                    error: None,
                });
                if needs_preview {
                    self.select_preview(0, theme, &mut effects);
                } else {
                    effects.task = self.validate_preview(ticket);
                }
            }
            Action::PreviewFile { ticket, index } => {
                if admission.writable
                    && !admission.other_modal
                    && self
                        .preview
                        .as_ref()
                        .is_some_and(|preview| preview.ticket == ticket && !preview.validating)
                {
                    self.select_preview(index, theme, &mut effects);
                }
            }
            Action::PreviewDiff {
                ticket,
                selected,
                result,
            } => {
                let Some(preview) = self.preview.as_mut().filter(|preview| {
                    preview.diff_ticket == ticket && preview.selected == selected
                }) else {
                    return effects;
                };
                match result {
                    Ok(Some(model)) => {
                        let mut runtime = RepositoryRuntime::new(preview.operation.view.clone());
                        runtime.set_workspace_preview(
                            preview.plan.documents[selected].relative.clone(),
                            Arc::unwrap_or_clone(model),
                        );
                        preview.runtime = Some(runtime);
                    }
                    Ok(None) => {
                        preview.error =
                            Some("The comparison was canceled or could not be prepared".into())
                    }
                    Err(error) => preview.error = Some(error),
                }
            }
            Action::PreviewRepository { ticket, action } => {
                if let Some(preview) = self
                    .preview
                    .as_mut()
                    .filter(|preview| preview.diff_ticket == ticket)
                    && let Some(runtime) = &mut preview.runtime
                {
                    // Preview runtimes contain no editable document. Only diff
                    // geometry/scroll/selection callbacks can affect this view.
                    effects.task = runtime
                        .update(action)
                        .map(move |action| Action::PreviewRepository { ticket, action });
                }
            }
            Action::AnnotationPage { ticket, page } => {
                if admission.writable
                    && !admission.other_modal
                    && let Some(preview) = self
                        .preview
                        .as_mut()
                        .filter(|preview| preview.ticket == ticket)
                {
                    preview.page =
                        page.min(preview.operation.annotations.len().saturating_sub(1) / 8);
                }
            }
            Action::Apply(ticket) => {
                if self.preview.as_ref().is_some_and(|preview| {
                    preview.ticket == ticket
                        && !preview.validating
                        && admission.permits(&preview.operation)
                }) {
                    effects.task = self.validate_preview(ticket);
                }
            }
            Action::Validated { ticket, result } => {
                let Some(preview) = self.preview.take() else {
                    return effects;
                };
                if preview.ticket != ticket {
                    self.preview = Some(preview);
                    return effects;
                }
                if let Err(error) = result {
                    Self::reject(&preview.operation, lsp, &error);
                    effects.error(error);
                    return effects;
                }
                self.publish(
                    preview.operation.clone(),
                    preview.plan.clone(),
                    lsp,
                    buffers,
                    admission,
                    recovery,
                    &mut effects,
                );
            }
            Action::Cancel(ticket) => {
                if !admission.writable
                    || (admission.other_modal
                        && !self
                            .preview
                            .as_ref()
                            .is_some_and(|preview| admission.permits(&preview.operation)))
                {
                    return effects;
                }
                if let Some(rename) = self.rename.take_if(|rename| rename.ticket == ticket) {
                    effects.choice_restore = admission
                        .choice_card
                        .filter(|card| *card == rename.operation.card);
                }
                if let Some(choices) = self.choices.take_if(|choices| choices.ticket == ticket) {
                    effects.choice_restore = admission
                        .choice_card
                        .filter(|card| *card == choices.operation.card);
                }
                if self
                    .preview
                    .as_ref()
                    .is_some_and(|preview| preview.ticket == ticket)
                    && let Some(preview) = self.preview.take()
                {
                    effects.focus = Some(preview.operation.card);
                    Self::reject(&preview.operation, lsp, "The user canceled the preview");
                }
            }
        }
        effects
    }
    fn validate_preview(&mut self, ticket: u64) -> Task<Action> {
        let Some(preview) = self
            .preview
            .as_mut()
            .filter(|preview| preview.ticket == ticket)
        else {
            return Task::none();
        };
        preview.validating = true;
        let plan = preview.plan.clone();
        cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(move || workspace::validate_for_publication(&plan))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
            Action::Validated { ticket, result }
        })
    }
    fn action_edit(
        &mut self,
        mut operation: Operation,
        item: LspCodeAction,
        lsp: &mut LspManager,
        effects: &mut Effects,
    ) {
        operation.command_context = Some(item.context());
        operation.command = item.command;
        if let Some(edit) = item.edit {
            self.prepare(operation, edit, lsp, effects);
        } else if let Some(command) = operation.command.take() {
            if let Some(context) = &operation.command_context
                && let Err(error) = lsp.validate_code_action_context_token(context)
            {
                effects.error(error.to_string());
                return;
            }
            effects.focus = Some(operation.card);
            let request = lsp.execute_command(operation.doc, command);
            self.track(request, RequestKind::Command, operation, lsp);
        }
    }
    fn event(
        &mut self,
        event: LspEvent,
        lsp: &mut LspManager,
        buffers: &mut Buffers<'_>,
        admission: Admission,
        effects: &mut Effects,
    ) {
        match event {
            LspEvent::PrepareRename {
                request,
                doc,
                range,
                placeholder,
            } => {
                let Some(operation) = self.take_request(request, doc, RequestKind::RenameStart)
                else {
                    return;
                };
                if !admission.permits_choice(&operation)
                    || !Self::choice_current(&operation, lsp, buffers)
                    || self.dialog_open()
                {
                    return;
                }
                let Some(range) = range else {
                    effects.error("This symbol cannot be renamed");
                    return;
                };
                let draft = placeholder
                    .or_else(|| {
                        buffer(buffers, operation.card)
                            .and_then(|runtime| runtime.editor_text_in_range(range))
                    })
                    .unwrap_or_default();
                if let Some(ticket) = self.ticket() {
                    self.rename = Some(Rename {
                        ticket,
                        operation,
                        draft,
                    });
                    effects.task = dialog::focus(ticket, control_id(ticket, "rename"));
                }
            }
            LspEvent::WorkspaceEdit {
                request,
                doc,
                edit,
                kind,
            } => {
                let stage =
                    if matches!(kind, LspEditKind::Formatting | LspEditKind::RangeFormatting) {
                        RequestKind::Format
                    } else {
                        RequestKind::Rename
                    };
                let Some(operation) = self.take_request(request, doc, stage) else {
                    return;
                };
                if admission.permits(&operation) && Self::current(&operation, lsp, buffers) {
                    self.prepare(operation, edit, lsp, effects);
                }
            }
            LspEvent::CodeActions {
                request,
                doc,
                items,
            } => {
                let Some(operation) = self.take_request(request, doc, RequestKind::Actions) else {
                    return;
                };
                if admission.permits_choice(&operation)
                    && Self::choice_current(&operation, lsp, buffers)
                    && !self.dialog_open()
                    && let Some(ticket) = self.ticket()
                {
                    if items.is_empty() {
                        effects.notices.push((
                            LspMessageLevel::Info,
                            "No code actions are available here".into(),
                        ));
                    } else {
                        self.choices = Some(Choices {
                            ticket,
                            operation,
                            items,
                        });
                        effects.task = dialog::focus(ticket, control_id(ticket, "action.0"));
                    }
                }
            }
            LspEvent::CodeActionResolved { request, doc, item } => {
                let Some(operation) = self.take_request(request, doc, RequestKind::Resolve) else {
                    return;
                };
                if admission.permits(&operation) && Self::current(&operation, lsp, buffers) {
                    self.action_edit(operation, item, lsp, effects);
                } else {
                    effects.error("The resolved action was refused because the window or source changed; request it again");
                }
            }
            LspEvent::SignatureHelp { request, doc, help } => {
                let Some(operation) = self.take_request(request, doc, RequestKind::Signature)
                else {
                    return;
                };
                if Self::choice_current(&operation, lsp, buffers)
                    && let Some(runtime) = buffer_mut(buffers, operation.card)
                {
                    runtime.set_editor_signature(help);
                }
            }
            LspEvent::ApplyWorkspaceEdit { doc, id, edit, .. } => {
                let owner = buffers.iter().find_map(|(card, runtime)| {
                    (runtime.lsp_document() == Some(doc)).then_some(*card)
                });
                let Some(mut operation) = owner
                    .and_then(|card| self.operation(card, LspEditKind::CodeAction, lsp, buffers))
                else {
                    lsp.respond_workspace_edit(
                        id,
                        false,
                        Some("The source document was closed".into()),
                    );
                    return;
                };
                operation.server = Some(id);
                if admission.permits(&operation) && !self.dialog_open() {
                    self.prepare(operation, edit, lsp, effects);
                } else {
                    Self::reject(&operation, lsp, "Another dialog is open");
                }
            }
            LspEvent::CommandExecuted { request, doc } => {
                self.take_request(request, doc, RequestKind::Command);
            }
            LspEvent::RequestFailed {
                request,
                doc,
                error,
            } => {
                if !self
                    .requests
                    .get(&request)
                    .is_some_and(|(_, operation)| operation.doc == doc)
                {
                    return;
                }
                let (_, operation) = self.requests.remove(&request).unwrap();
                if !matches!(error, LspError::Cancelled) {
                    effects.error(error.to_string());
                }
                if operation.save_after
                    && admission.permits(&operation)
                    && Self::current(&operation, lsp, buffers)
                {
                    effects.save.push(operation.card);
                }
            }
            LspEvent::ServerMessage { level, message, .. } => {
                effects.notices.push((level, message))
            }
            _ => unreachable!("the shell passes only feature-owned events"),
        }
    }

    /// Only the bounded list is mounted; source comparisons use the ordinary
    /// virtualized diff renderer rather than materializing one row per line.
    pub fn view(&self, choice_card: Option<CardId>) -> Option<Element<'_, Action>> {
        let (ticket, source) = self
            .rename
            .as_ref()
            .map(|value| (value.ticket, Some(value.operation.card)))
            .or_else(|| {
                self.choices
                    .as_ref()
                    .map(|value| (value.ticket, Some(value.operation.card)))
            })
            .or_else(|| self.preview.as_ref().map(|value| (value.ticket, None)))?;
        // A queued mouse release must never inherit the pressed state of a
        // different dialog, even when Cancel and replacement share a frame.
        Some(dialog::scoped(
            ticket,
            source.is_none_or(|card| choice_card == Some(card)),
            self.view_unkeyed()?,
        ))
    }
    fn view_unkeyed(&self) -> Option<Element<'_, Action>> {
        if let Some(rename) = &self.rename {
            let ticket = rename.ticket;
            return Some(
                widget::column::with_children(vec![
                    widget::text::title3("Rename symbol").into(),
                    widget::text_input("New name", &rename.draft)
                        .id(control_id(ticket, "rename"))
                        .on_input(move |value| Action::RenameDraft { ticket, value })
                        .on_submit(move |value| Action::RenameSubmit { ticket, value })
                        .into(),
                    widget::row::with_children(vec![
                        widget::button::standard("Cancel")
                            .id(control_id(ticket, "cancel"))
                            .on_press(Action::Cancel(ticket))
                            .into(),
                        widget::button::suggested("Preview rename")
                            .id(control_id(ticket, "submit"))
                            .on_press_maybe((!rename.draft.trim().is_empty()).then(|| {
                                Action::RenameSubmit {
                                    ticket,
                                    value: rename.draft.clone(),
                                }
                            }))
                            .into(),
                    ])
                    .spacing(8)
                    .into(),
                ])
                .spacing(12)
                .into(),
            );
        }
        if let Some(choices) = &self.choices {
            let ticket = choices.ticket;
            let rows = choices
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let label = match &item.disabled {
                        Some(reason) => format!("{} — {reason}", item.title),
                        None => item.title.clone(),
                    };
                    widget::button::standard(label)
                        .id(control_id(ticket, &format!("action.{index}")))
                        .on_press_maybe(
                            item.disabled
                                .is_none()
                                .then_some(Action::ChooseAction { ticket, index }),
                        )
                        .into()
                })
                .collect::<Vec<Element<'_, Action>>>();
            return Some(
                widget::column::with_children(vec![
                    widget::text::title3("Code actions").into(),
                    widget::scrollable(widget::column::with_children(rows).spacing(4))
                        .height(Length::Fixed(360.0))
                        .into(),
                    widget::button::standard("Cancel")
                        .id(control_id(ticket, "cancel"))
                        .on_press(Action::Cancel(ticket))
                        .into(),
                ])
                .spacing(12)
                .into(),
            );
        }
        let preview = self.preview.as_ref()?;
        let ticket = preview.ticket;
        let mut rows: Vec<Element<'_, Action>> = vec![widget::text::title3("Review language edits").into(), widget::text::body("Changes apply to open buffers. Save writes each file through its ordinary protected save path.").into()];
        let start = preview.selected / 32 * 32;
        let files: Vec<Element<'_, Action>> = preview
            .plan
            .documents
            .iter()
            .enumerate()
            .skip(start)
            .take(32)
            .map(|(index, document)| {
                widget::button::standard(document.relative.to_string_lossy().into_owned())
                    .on_press_maybe(
                        (!preview.validating).then_some(Action::PreviewFile { ticket, index }),
                    )
                    .into()
            })
            .collect();
        rows.push(widget::row::with_children(files).spacing(4).wrap().into());
        if start > 0 {
            rows.push(
                widget::button::standard("Previous files")
                    .on_press(Action::PreviewFile {
                        ticket,
                        index: start - 1,
                    })
                    .into(),
            );
        }
        if start + 32 < preview.plan.documents.len() {
            rows.push(
                widget::button::standard("More files")
                    .on_press(Action::PreviewFile {
                        ticket,
                        index: start + 32,
                    })
                    .into(),
            );
        }
        for annotation in preview
            .operation
            .annotations
            .iter()
            .skip(preview.page * 8)
            .take(8)
        {
            rows.push(
                widget::text::body(format!(
                    "{}{}",
                    if annotation.needs_confirmation {
                        "Confirmation required: "
                    } else {
                        ""
                    },
                    annotation.label
                ))
                .into(),
            );
            if let Some(description) = &annotation.description {
                rows.push(widget::text::body(description.clone()).into());
            }
        }
        if preview.operation.annotations.len() > 8 {
            rows.push(
                widget::row::with_children(vec![
                    widget::button::standard("Previous notes")
                        .on_press_maybe((preview.page > 0).then_some(Action::AnnotationPage {
                            ticket,
                            page: preview.page.saturating_sub(1),
                        }))
                        .into(),
                    widget::button::standard("More notes")
                        .on_press_maybe(
                            ((preview.page + 1) * 8 < preview.operation.annotations.len())
                                .then_some(Action::AnnotationPage {
                                    ticket,
                                    page: preview.page + 1,
                                }),
                        )
                        .into(),
                ])
                .spacing(8)
                .into(),
            );
        }
        if let Some(runtime) = &preview.runtime {
            let diff_ticket = preview.diff_ticket;
            rows.push(
                widget::container(crate::repository_widget::view(preview.card, runtime).map(
                    move |action| Action::PreviewRepository {
                        ticket: diff_ticket,
                        action,
                    },
                ))
                .height(Length::Fixed(320.0))
                .into(),
            );
        } else {
            rows.push(widget::text::body("Preparing comparison…").into());
        }
        if let Some(error) = &preview.error {
            rows.push(widget::text::body(error.clone()).into());
        }
        rows.push(
            widget::row::with_children(vec![
                widget::button::standard("Cancel")
                    .id(control_id(ticket, "cancel"))
                    .on_press_maybe((!preview.validating).then_some(Action::Cancel(ticket)))
                    .into(),
                widget::button::suggested(if preview.validating {
                    "Checking…"
                } else {
                    "Apply to buffers"
                })
                .id(control_id(ticket, "apply"))
                .on_press_maybe((!preview.validating).then_some(Action::Apply(ticket)))
                .into(),
            ])
            .spacing(8)
            .into(),
        );
        Some(widget::column::with_children(rows).spacing(12).into())
    }
}

/// A stale autosave callback cannot promote itself into a fresh Format request.
/// Explicit overwrite retains its existing confirmation and is never replaced
/// with an ordinary non-overwrite save after formatting.
pub fn format_before_save(
    action: &RepositoryAction,
    lsp: &LspManager,
    runtime: &mut RepositoryRuntime,
) -> Option<Action> {
    let card = action.card();
    if !matches!(
        action,
        RepositoryAction::Save {
            overwrite: false,
            ..
        } | RepositoryAction::AutoSaveDue { .. }
    ) {
        return None;
    }
    let doc = runtime.lsp_document()?;
    let caps = lsp.capabilities(doc);
    if !caps.ready
        || !caps.format_document
        || !lsp
            .formatting_settings(doc)
            .is_some_and(|(enabled, _)| enabled)
        || !runtime.can_save()
    {
        return None;
    }
    if let RepositoryAction::AutoSaveDue {
        identity,
        revision,
        ticket,
    } = action
    {
        if !runtime.retire_auto_save_due(identity, *revision, *ticket) {
            return None;
        }
    }
    Some(Action::Format {
        card,
        selection: false,
        save_after: true,
    })
}

pub fn language_action(
    card: CardId,
    action: &crate::repository_widget::EditorLspAction,
) -> Option<Action> {
    use crate::repository_widget::EditorLspAction as Native;
    Some(match action {
        Native::Rename => Action::Rename(card),
        Native::Format { selection } => Action::Format {
            card,
            selection: *selection,
            save_after: false,
        },
        Native::CodeActions => Action::CodeActions(card),
        Native::Signature => Action::Signature {
            card,
            trigger: None,
        },
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Reads the real file through the production worker result and registers
    /// that exact Arc in the framed manager. No constructor can mark a raw
    /// candidate Root as an accepted read or an Editable publication.
    pub(crate) struct Fixture {
        pub directory: tempfile::TempDir,
        pub card: CardId,
        pub runtime: RepositoryRuntime,
        pub manager: LspManager,
    }
    impl Fixture {
        pub(crate) async fn new(card: CardId) -> Self {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("Cargo.toml"),
                "[package]\nname='fixture'\nversion='0.1.0'\n",
            )
            .unwrap();
            std::fs::write(directory.path().join("main.rs"), "foo();\n").unwrap();
            let (mut runtime, mut identity) = RepositoryRuntime::selection_fixture(card, "main.rs");
            runtime.descriptor.worktree = directory.path().to_owned();
            runtime.descriptor.repository = directory.path().join(".git");
            identity.worktree = directory.path().to_owned();
            let path = directory.path().to_owned();
            let read = tokio::task::spawn_blocking(move || {
                crate::editor::read_file_at(
                    &path,
                    std::path::Path::new("main.rs"),
                    crate::editor::PdfTone::Original,
                    None,
                )
            })
            .await
            .unwrap();
            drop(runtime.update(RepositoryAction::FileLoadedAt {
                identity,
                result: read,
            }));
            assert!(runtime.editor_feature_writable());
            let root = runtime.retained_editor_root().unwrap();
            let manager = LspManager::fixture_ready_editor_features(
                LspDocumentId(1),
                &directory.path().join("main.rs"),
                root,
                "foo();\n",
            )
            .await;
            runtime.set_lsp_document(Some(LspDocumentId(1)));
            runtime.set_editor_feature_support(&manager.capabilities(LspDocumentId(1)));
            // The fixture already passed this exact open to the manager.
            runtime.take_lsp_intents();
            runtime.set_auto_save(false);
            Self {
                directory,
                card,
                runtime,
                manager,
            }
        }
    }

    #[test]
    fn an_original_root_replacement_cannot_prepare_a_successors_buffer_edit() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("worktree");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("main.rs"), "foo").unwrap();
        let original = Arc::new(crate::fs_safety::Root::open(&path).unwrap());
        std::fs::rename(&path, parent.path().join("original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("main.rs"), "foo").unwrap();
        let result = workspace::resolve_targets_at(
            original,
            vec![workspace::DocumentEdits {
                path: path.join("main.rs"),
                version: None,
                observed_version: None,
                observed_incarnation: None,
                coordinates: LspCoordinates::Scalar,
                edits: vec![LspTextEdit {
                    range: LspRange::new(LspPosition::default(), LspPosition::new(0, 3)),
                    new_text: "bar".into(),
                }],
            }],
            vec![path.join("main.rs")],
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("main.rs")).unwrap(),
            "foo"
        );
        assert_eq!(
            std::fs::read_to_string(parent.path().join("original/main.rs")).unwrap(),
            "foo"
        );
    }
    #[test]
    fn controller_snapshot_capture_keeps_native_text_off_the_ui_turn() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let card = CardId::new();
                let fixture = Fixture::new(card).await;
                let mut runtime = fixture.runtime;
                let mut manager = fixture.manager;
                let path = runtime.workspace_edit_candidate_source().unwrap();
                crate::repository_widget::text_editor::take_text_call_count();
                let captured =
                    State::snapshots(&[path.clone()], &manager, &[(card, &mut runtime)]).unwrap();
                assert_eq!(
                    crate::repository_widget::text_editor::take_text_call_count(),
                    0
                );
                let open = tokio::task::spawn_blocking(move || materialize_snapshots(captured))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&*open[&path].text, "foo();\n");
                assert_eq!(
                    crate::repository_widget::text_editor::take_text_call_count(),
                    0
                );
                manager.begin_shutdown().await;
            });
    }
}
