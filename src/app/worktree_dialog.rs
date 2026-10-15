//! The dialog that starts a worktree creation.
//!
//! The dialog owns the request until Git has accepted it as a creation it can
//! run: it stays open, read-only, while the branch, base and destination are
//! resolved, and a resolution that fails lands back in it with what the user
//! typed and the reason next to the field it concerns. Only a resolved
//! request closes it and becomes a creation row in the sidebar; if that
//! creation fails later, the row reopens this dialog with the same input.

use super::*;
use crate::{
    model::OperationId,
    repository::{RepositoryError, ValidationError, WorktreeInput},
};

/// What the user typed, kept verbatim so a failed attempt never has to be
/// typed again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeDraft {
    pub branch: String,
    pub base: String,
    pub destination: String,
}

impl WorktreeDraft {
    pub fn branch(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            ..Self::default()
        }
    }

    /// Whether an optional field is set, so the options holding it must show.
    fn uses_options(&self) -> bool {
        !self.base.trim().is_empty() || !self.destination.trim().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeField {
    Branch,
    Base,
    Destination,
}

impl WorktreeField {
    fn focus_id(self) -> widget::Id {
        match self {
            Self::Branch => worktree_dialog_focus_id(),
            Self::Base => widget::Id::new("dialog.worktree.base"),
            Self::Destination => widget::Id::new("dialog.worktree.destination"),
        }
    }

    fn optional(self) -> bool {
        !matches!(self, Self::Branch)
    }

    fn focus(self) -> Task<Message> {
        iced::widget::operation::focus(self.focus_id()).map(cosmic::Action::App)
    }
}

/// Why a request was refused, attributed to the field that has to change
/// when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInputError {
    pub field: Option<WorktreeField>,
    pub message: String,
}

impl WorktreeInputError {
    fn at(field: WorktreeField, message: impl Into<String>) -> Self {
        Self {
            field: Some(field),
            message: message.into(),
        }
    }

    pub fn general(message: impl AsRef<str>) -> Self {
        Self {
            field: None,
            message: sentence(message.as_ref()),
        }
    }

    /// A detail recorded elsewhere, such as the journal's, shown as written.
    fn recorded(detail: &str) -> Self {
        Self {
            field: None,
            message: punctuated(detail),
        }
    }

    /// Reads a resolution failure as something the form can show.
    pub fn from_resolution(error: &RepositoryError, draft: &WorktreeDraft) -> Self {
        let RepositoryError::Validation(validation) = error else {
            return Self::general(error.to_string());
        };
        let custom_destination = !draft.destination.trim().is_empty();
        match validation {
            ValidationError::InvalidBranch(detail) => Self::at(
                WorktreeField::Branch,
                invalid_value(detail, &draft.branch, "branch name"),
            ),
            ValidationError::InvalidBase(detail) => Self::at(
                WorktreeField::Base,
                invalid_value(detail, &draft.base, "base revision"),
            ),
            ValidationError::TrackingRequiresBranch => {
                Self::at(WorktreeField::Base, "Tracking needs a branch as the base.")
            }
            ValidationError::DestinationOccupied if custom_destination => Self::at(
                WorktreeField::Destination,
                "Something already exists at this destination. Choose a new folder.",
            ),
            ValidationError::DestinationOccupied => Self::at(
                WorktreeField::Destination,
                "The worktree folder for this branch already exists. Choose another destination.",
            ),
            ValidationError::DestinationOutsideRoot => Self::at(
                WorktreeField::Destination,
                "The destination must be a new folder inside an existing one.",
            ),
            ValidationError::DestinationSymlink => Self::at(
                WorktreeField::Destination,
                "The destination may not be a symbolic link.",
            ),
            ValidationError::InvalidDestinationRoot => Self::at(
                WorktreeField::Destination,
                "The destination's parent is not a folder.",
            ),
            ValidationError::Io(detail) if custom_destination => Self::at(
                WorktreeField::Destination,
                format!("This destination cannot be used: {detail}."),
            ),
            _ => Self::general(error.to_string()),
        }
    }
}

/// A validation detail is either the rejected value itself or a sentence
/// about it. Git names cannot contain whitespace, so a detail without any is
/// the value.
fn invalid_value(detail: &str, typed: &str, noun: &str) -> String {
    if detail.is_empty() {
        format!("“{}” does not name a {noun}.", typed.trim())
    } else if detail == typed.trim() || !detail.contains(char::is_whitespace) {
        format!("“{detail}” is not a valid {noun}.")
    } else {
        // Such a detail usually leads with a branch name, whose case is
        // significant: it is punctuated, never capitalized.
        punctuated(detail)
    }
}

/// A message ending as a sentence does.
fn punctuated(message: &str) -> String {
    let mut message = message.trim().to_owned();
    if !message.is_empty() && !message.ends_with(['.', '!', '?']) {
        message.push('.');
    }
    message
}

/// A fixed message from our own code as a sentence; not for text that can
/// lead with a name the user typed.
fn sentence(message: &str) -> String {
    let message = punctuated(message);
    let mut chars = message.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// One original physical source, shared by resolution, creation and retry.
/// Constructing this handle does no filesystem work on the UI thread. The
/// blocking workers validate the original authority and reuse the same scoped
/// service; a later pathname lookup never supplies replacement authority.
#[derive(Clone)]
pub(super) struct WorktreeCreationSource {
    id: OperationId,
    lifetime: Arc<std::sync::Mutex<CreationSourceLifetime>>,
    authority: crate::repository::WorktreeAuthority,
    service: Arc<std::sync::Mutex<Option<Arc<ScopedCreationService>>>>,
}

#[derive(Default)]
struct CreationSourceLifetime {
    invalidated: bool,
    readers: Vec<std::sync::Weak<CancellationToken>>,
}

type ScopedCreationService = RepositoryService<crate::repository::WorktreeExecutor<crate::repository::SystemProcessExecutor>>;

impl std::fmt::Debug for WorktreeCreationSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("WorktreeCreationSource").field("id", &self.id).field("authority", &self.authority).finish_non_exhaustive()
    }
}

impl WorktreeCreationSource {
    pub(super) fn new(authority: crate::repository::WorktreeAuthority) -> Self {
        Self { id: OperationId::new(), lifetime: Arc::new(std::sync::Mutex::new(CreationSourceLifetime::default())), authority, service: Arc::new(std::sync::Mutex::new(None)) }
    }

    pub(super) fn id(&self) -> OperationId { self.id }

    /// Publication/removal of a project generation invalidates every clone.
    /// Pending workers retain their own cancellation owner until settlement.
    pub(super) fn invalidate(&self) {
        let mut lifetime = self.lifetime.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        lifetime.invalidated = true;
        for token in lifetime.readers.iter().filter_map(std::sync::Weak::upgrade) { token.cancel(); }
        lifetime.readers.retain(|reader| reader.strong_count() != 0);
    }

    pub(super) fn is_available(&self) -> bool {
        !self.lifetime.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).invalidated
    }

    /// Retain this guard through the actual blocking worker. The registry
    /// contains weak owners, so completed operations do not accumulate tokens.
    pub(super) fn link_token(&self, token: &CancellationToken) -> Result<Arc<CancellationToken>, RepositoryError> {
        let mut lifetime = self.lifetime.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if lifetime.invalidated {
            token.cancel();
            return Err(ValidationError::Io("The displayed project source changed. Reload the project before creating a worktree.".into()).into());
        }
        lifetime.readers.retain(|reader| reader.strong_count() != 0);
        let retained = Arc::new(token.clone());
        lifetime.readers.push(Arc::downgrade(&retained));
        Ok(retained)
    }

    pub(super) fn repository(&self) -> &Path { self.authority.path() }

    pub(super) fn service(&self) -> Result<Arc<ScopedCreationService>, RepositoryError> {
        if !self.is_available() {
            return Err(ValidationError::Io("The displayed project source changed. Reload the project before creating a worktree.".into()).into());
        }
        self.authority.validate()?;
        let mut cached = self.service.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(service) = &*cached { return Ok(service.clone()); }
        let service = Arc::new(RepositoryService::system("git").with_worktree_authority(self.authority.clone())?);
        *cached = Some(service.clone());
        Ok(service)
    }
}

/// A resolution in flight: the dialog is read-only until it answers, and
/// cancelling the dialog stops it.
#[derive(Debug, Clone)]
struct Resolution {
    id: OperationId,
    token: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct WorktreeDialog {
    /// Per-open consent identity. The modal surface captures this before
    /// emitting user messages, independently of asynchronous lookup IDs.
    request: OperationId,
    project_id: ProjectId,
    repository: PathBuf,
    source: Option<WorktreeCreationSource>,
    project_creation_id: Option<OperationId>,
    project_generation: Option<u64>,
    draft: WorktreeDraft,
    advanced: bool,
    resolving: Option<Resolution>,
    error: Option<WorktreeInputError>,
    /// The failed creation this dialog reopened. Its row stays until this
    /// input resolves, so cancelling the dialog loses nothing.
    replaces: Option<OperationId>,
}

impl WorktreeDialog {
    pub(super) fn request(&self) -> OperationId {
        self.request
    }

    pub fn new(project_id: ProjectId, repository: PathBuf, draft: WorktreeDraft) -> Self {
        Self {
            request: OperationId::new(),
            project_id,
            repository,
            source: None,
            project_creation_id: None,
            project_generation: None,
            advanced: draft.uses_options(),
            draft,
            resolving: None,
            error: None,
            replaces: None,
        }
    }

    /// Shows `error` beside its field, opening the options when the field is
    /// one of theirs, and moves the cursor there.
    fn show_error(&mut self, error: WorktreeInputError) -> Task<Message> {
        let field = error.field.unwrap_or(WorktreeField::Branch);
        self.advanced |= field.optional();
        self.error = Some(error);
        field.focus()
    }

    /// Edits a field. An error about that field no longer describes it; one
    /// about another field, or about the request as a whole, still does.
    pub fn edit(&mut self, field: WorktreeField, value: String) {
        if self.resolving.is_some() {
            return;
        }
        *match field {
            WorktreeField::Branch => &mut self.draft.branch,
            WorktreeField::Base => &mut self.draft.base,
            WorktreeField::Destination => &mut self.draft.destination,
        } = value;
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.field == Some(field))
        {
            self.error = None;
        }
    }

    pub fn toggle_options(&mut self) {
        if self.resolving.is_none() {
            self.advanced = !self.advanced;
        }
    }

    /// What can be checked without Git: that there is a branch, and that it
    /// is one Git would accept.
    fn validate(&self) -> Result<(), WorktreeInputError> {
        let branch = self.draft.branch.trim();
        if branch.is_empty() {
            return Err(WorktreeInputError::at(
                WorktreeField::Branch,
                "Enter a branch name or a Linear issue.",
            ));
        }
        crate::repository::worktree_branch_input(branch)
            .map(drop)
            .map_err(|error| WorktreeInputError::from_resolution(&error.into(), &self.draft))
    }

    /// The branch a Linear issue URL stands for, shown before it is created.
    fn branch_preview(&self) -> Option<String> {
        let branch = self.draft.branch.trim();
        if !branch.contains("://") {
            return None;
        }
        crate::repository::worktree_branch_input(branch).ok()
    }

    fn can_submit(&self) -> bool {
        self.resolving.is_none() && !self.draft.branch.trim().is_empty()
    }

    /// Stops a resolution the user no longer wants.
    pub fn cancel(&self) {
        if let Some(resolution) = &self.resolving {
            resolution.token.cancel();
        }
    }
}

impl Cagents {
    /// Opens the dialog on `draft`, with `error` beside the field it concerns,
    /// and puts the cursor where the user has to act.
    pub(super) fn open_worktree_dialog(
        &mut self,
        project_id: ProjectId,
        repository: PathBuf,
        draft: WorktreeDraft,
        error: Option<WorktreeInputError>,
    ) -> Task<Message> {
        if !self.sidebar_model.project(project_id).is_some_and(|project| project.repository == repository) {
            self.status = Some(TransientNotice::warning("The project source changed. Reload the project before creating a worktree."));
            return Task::none();
        }
        let Some(source) = self.project_creation_source(project_id) else {
            self.status = Some(TransientNotice::warning("The project source is loading or unavailable. Reload the project before creating a worktree."));
            return Task::none();
        };
        let focus = self.open_worktree_dialog_project_bound(project_id, source, draft, None);
        match error {
            Some(error) => self.worktree_dialog.as_mut().map_or(focus, |dialog| dialog.show_error(error)),
            None => focus,
        }
    }

    pub(super) fn invalidate_project_worktree_creations(&self, project: ProjectId) {
        if let Some(dialog) = self.worktree_dialog.as_ref().filter(|dialog| dialog.project_id == project) {
            if let Some(source) = &dialog.source { source.invalidate(); }
            if let Some(resolution) = &dialog.resolving { resolution.token.cancel(); }
        }
        for operation in self.worktree_operations.values().filter(|operation| operation.project() == project) {
            if let Some(source) = operation.source() { source.invalidate(); }
        }
    }

    pub(super) fn creation_project_is_current(&self, project: ProjectId, generation: Option<u64>) -> bool {
        generation.is_none_or(|generation| self.project_creation_generation(project) == Some(generation)
            && self.sidebar_model.project(project).is_some())
    }

    /// Generic project creation consumes the exact asynchronously published
    /// project capability. The caller's action must already carry this ID.
    pub(super) fn open_worktree_dialog_project_bound(
        &mut self, project_id: ProjectId, source: WorktreeCreationSource,
        draft: WorktreeDraft, replaces: Option<OperationId>,
    ) -> Task<Message> {
        let id = source.id();
        if !source.is_available() || !self.current_project_creation_source(project_id, id) {
            self.status = Some(TransientNotice::warning("The displayed project source changed. Reload the project before creating a worktree."));
            return Task::none();
        }
        let Some(generation) = self.project_creation_generation(project_id) else {
            self.status = Some(TransientNotice::warning("The project generation is unavailable. Reload the project before creating a worktree."));
            return Task::none();
        };
        self.open_worktree_dialog_with_source(project_id, source.repository().to_owned(), draft, None, Some(source), replaces, Some(id), Some(generation))
    }

    /// Opens History creation against the authority the displayed source already
    /// owns. The caller must never recapture a pathname to create this authority.
    pub(super) fn open_worktree_dialog_bound(
        &mut self,
        project_id: ProjectId,
        authority: crate::repository::WorktreeAuthority,
        draft: WorktreeDraft,
        replaces: Option<OperationId>,
    ) -> Task<Message> {
        let Some(generation) = self.project_creation_generation(project_id).filter(|_| self.sidebar_model.project(project_id).is_some()) else {
            self.status = Some(TransientNotice::warning("The original project generation is unavailable. Reload the project before creating a worktree."));
            return Task::none();
        };
        let source = WorktreeCreationSource::new(authority);
        self.open_worktree_dialog_with_source(project_id, source.repository().to_owned(), draft, None, Some(source), replaces, None, Some(generation))
    }

    fn open_worktree_dialog_with_source(
        &mut self,
        project_id: ProjectId,
        repository: PathBuf,
        draft: WorktreeDraft,
        error: Option<WorktreeInputError>,
        source: Option<WorktreeCreationSource>,
        replaces: Option<OperationId>,
        project_creation_id: Option<OperationId>,
        project_generation: Option<u64>,
    ) -> Task<Message> {
        if let Some(replaced) = self.worktree_dialog.take() {
            replaced.cancel();
        }
        let mut opened = WorktreeDialog::new(project_id, repository, draft);
        opened.source = source;
        opened.project_creation_id = project_creation_id;
        opened.project_generation = project_generation;
        opened.replaces = replaces;
        let dialog = self.worktree_dialog.insert(opened);
        match error {
            Some(error) => dialog.show_error(error),
            None => WorktreeField::Branch.focus(),
        }
    }

    /// Resolves the dialog's request, keeping it open until Git answers.
    pub(super) fn submit_worktree_dialog(&mut self) -> Task<Message> {
        let stale = self.worktree_dialog.as_ref().is_some_and(|dialog| {
            !self.creation_project_is_current(dialog.project_id, dialog.project_generation)
                || dialog.project_creation_id.is_some_and(|id| !self.current_project_creation_source(dialog.project_id, id))
        });
        if stale {
            return self.worktree_dialog.as_mut().expect("dialog checked").show_error(WorktreeInputError::general(
                "The displayed project source changed. Reload the project before creating a worktree."));
        }
        if self.session.is_none() {
            if let Some(dialog) = &mut self.worktree_dialog {
                dialog.error = Some(WorktreeInputError::general(
                    "Worktree creation requires an available durable journal.",
                ));
            }
            return Task::none();
        }
        let managed_directory = self.settings.managed_worktree_directory.clone();
        let Some(dialog) = &mut self.worktree_dialog else {
            return Task::none();
        };
        if dialog.resolving.is_some() {
            return Task::none();
        }
        if let Err(error) = dialog.validate() {
            return dialog.show_error(error);
        }
        let operation_id = OperationId::new();
        let token = CancellationToken::new();
        dialog.error = None;
        dialog.resolving = Some(Resolution {
            id: operation_id,
            token: token.clone(),
        });
        let project_id = dialog.project_id;
        let source = dialog.source.clone();
        let draft = dialog.draft.clone();
        let input = WorktreeInput {
            repository: dialog.repository.clone(),
            input: draft.branch.clone(),
            base: draft.base.clone(),
            managed_directory,
            destination: draft.destination.clone(),
            operation_id,
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                match source {
                    Some(source) => {
                        let _source_lifetime = source.link_token(&token)
                            .map_err(|error| WorktreeInputError::general(error.to_string()))?;
                        source.service()
                            .map_err(|error| WorktreeInputError::general(error.to_string()))?
                            .resolve_worktree_request(&input, &token)
                            .map_err(|error| WorktreeInputError::from_resolution(&error, &draft))
                    },
                    None => RepositoryService::system("git")
                        .resolve_worktree_request(&input, &token)
                        .map_err(|error| WorktreeInputError::from_resolution(&error, &draft)),
                }
            })
            .await
            .map_err(|error| WorktreeInputError::general(error.to_string()))
            .and_then(|result| result);
            Message::WorktreeInputResolved {
                project_id,
                operation_id,
                result,
            }
        })
    }

    /// Hands a resolved request to the journal, or puts the refusal back in
    /// front of the user. An answer for a dialog that has since been
    /// cancelled or replaced is dropped: nothing was created for it.
    pub(super) fn finish_worktree_input(
        &mut self,
        project_id: ProjectId,
        operation_id: OperationId,
        result: Result<CreateWorktreeRequest, WorktreeInputError>,
    ) -> Task<Message> {
        let stale = self.worktree_dialog.as_ref().filter(|dialog| {
            dialog.project_id == project_id && dialog.resolving.as_ref().is_some_and(|resolution| resolution.id == operation_id)
        }).is_some_and(|dialog| !self.creation_project_is_current(project_id, dialog.project_generation)
            || dialog.project_creation_id.is_some_and(|id| !self.current_project_creation_source(project_id, id)));
        if stale {
            let dialog = self.worktree_dialog.as_mut().expect("dialog checked");
            dialog.resolving = None;
            return dialog.show_error(WorktreeInputError::general("The project source changed while resolving this request. Reload the project before creating a worktree."));
        }
        let Some(dialog) = self.worktree_dialog.as_mut().filter(|dialog| {
            dialog.project_id == project_id
                && dialog
                    .resolving
                    .as_ref()
                    .is_some_and(|resolution| resolution.id == operation_id)
        }) else {
            return Task::none();
        };
        dialog.resolving = None;
        match result {
            Ok(request) => {
                if dialog.source.as_ref().is_some_and(|source| source.repository() != request.repository) {
                    return dialog.show_error(WorktreeInputError::general("The resolved source differs from the original displayed worktree. No creation was started."));
                }
                let source = dialog.source.clone();
                let project_creation_id = dialog.project_creation_id;
                let project_generation = dialog.project_generation;
                let draft = dialog.draft.clone();
                let replaces = dialog.replaces;
                self.worktree_dialog = None;
                if let Some(replaced) = replaces {
                    let _ = self.dismiss_worktree_operation(replaced);
                }
                let start = self.start_worktree_operation_with_project_source(project_id, request, Some(draft), source, project_creation_id, project_generation);
                let focus = match self.nested_modal_focus_target() {
                    Some(target) => iced::widget::operation::focus(target).map(cosmic::Action::App),
                    None => self.focus_context_overlay_if_exposed(),
                };
                Task::batch([start, focus])
            }
            Err(error) => dialog.show_error(error),
        }
    }

    /// Reopens a failed creation's input in the dialog, with the failure
    /// beside it; `submit` runs it again straight away. The row that
    /// reported the failure goes once the input resolves again.
    pub(super) fn reopen_worktree_operation(
        &mut self,
        id: OperationId,
        submit: bool,
    ) -> Task<Message> {
        if self.worktree_dialog.is_some() {
            return Task::none();
        }
        let Some((project_id, repository, draft, failure, source, project_creation_id, project_generation)) =
            self.worktree_operations.get(&id).and_then(|operation| {
                operation.reopenable_draft().map(|draft| {
                    (
                        operation.project(),
                        operation.repository().map(Path::to_path_buf),
                        draft.clone(),
                        operation.failure().map(WorktreeInputError::recorded),
                        operation.source(),
                        operation.project_creation_id(),
                        operation.project_generation(),
                    )
                })
            })
        else {
            return Task::none();
        };
        // The input resolves again against the root it was first resolved
        // against, which for a pull request checkout is not the project's.
        let Some(repository) = repository.or_else(|| {
            self.sidebar_model
                .project(project_id)
                .map(|project| project.repository.clone())
        }) else {
            return Task::none();
        };
        if !self.creation_project_is_current(project_id, project_generation)
            || project_creation_id.is_some_and(|binding| !self.current_project_creation_source(project_id, binding)) {
            self.status = Some(TransientNotice::warning("The original project source is no longer displayed. Reload the project and create a new request."));
            return Task::none();
        }
        let error = failure.filter(|_| !submit);
        let focus = self.open_worktree_dialog_with_source(project_id, repository, draft, error, source, Some(id), project_creation_id, project_generation);
        if let Some(dialog) = &mut self.worktree_dialog {
            dialog.replaces = Some(id);
        }
        if submit {
            Task::batch([focus, self.submit_worktree_dialog()])
        } else {
            focus
        }
    }

    /// The dialog's surface, and what a click on the backdrop does: nothing
    /// while a resolution is running, so a stray click cannot throw it away.
    pub(super) fn worktree_dialog_view(&self) -> Option<(Element<'_, Message>, Option<Message>)> {
        let dialog = self.worktree_dialog.as_ref()?;
        let editable = dialog.resolving.is_none();
        let field_error = |field| {
            dialog
                .error
                .as_ref()
                .filter(|error| error.field == Some(field))
                .map(|error| error.message.as_str())
        };
        let input = |field: WorktreeField, placeholder, value, on_input: fn(String) -> Message| {
            let mut input = crate::ui_controls::field::text(placeholder, value)
                .id(field.focus_id())
                .on_submit(|_| Message::SubmitWorktree);
            if editable {
                input = input.on_input(on_input);
            }
            if let Some(error) = field_error(field) {
                input = input.error(error);
            }
            input
        };
        let project = self
            .sidebar_model
            .project(dialog.project_id)
            .map_or("project", |project| project.name.as_str());

        let branch_note = field_error(WorktreeField::Branch)
            .map(error_note)
            .or_else(|| {
                dialog
                    .branch_preview()
                    .map(|branch| design::caption(format!("Branch: {branch}")).into())
            });
        let mut rows: Vec<Element<'_, Message>> = vec![
            widget::column::with_children(vec![
                design::title(format!("New worktree in {project}")).into(),
                design::caption(
                    "Create a branch, check out an existing one, or start from a Linear issue ID or URL.",
                )
                .into(),
            ])
            .spacing(space::XS)
            .into(),
            field(
                "Branch or Linear issue",
                input(
                    WorktreeField::Branch,
                    "feature/name, ENG-123, or a Linear URL",
                    &dialog.draft.branch,
                    Message::WorktreeBranchChanged,
                ),
                branch_note,
            ),
            crate::ui_controls::button::quiet(if dialog.advanced {
                "Hide options"
            } else {
                "Base branch & destination"
            })
            .icon(if dialog.advanced {
                Icon::PanDown
            } else {
                Icon::PanEnd
            })
            .id(widget::Id::new("dialog.worktree.options"))
            .on_press_maybe(editable.then_some(Message::ToggleWorktreeAdvanced))
            .into(),
        ];
        if dialog.advanced {
            rows.push(field(
                "Base branch (optional)",
                input(
                    WorktreeField::Base,
                    "Repository default",
                    &dialog.draft.base,
                    Message::WorktreeBaseChanged,
                ),
                field_error(WorktreeField::Base).map(error_note),
            ));
            rows.push(field(
                "Destination (optional)",
                input(
                    WorktreeField::Destination,
                    "Managed worktree folder",
                    &dialog.draft.destination,
                    Message::WorktreeDestinationChanged,
                ),
                Some(field_error(WorktreeField::Destination).map_or_else(
                    || {
                        design::caption(format!(
                            "Default parent: {}. Relative paths start at the repository root.",
                            self.settings.managed_worktree_directory
                        ))
                        .into()
                    },
                    error_note,
                )),
            ));
        }
        if let Some(error) = dialog.error.as_ref().filter(|error| error.field.is_none()) {
            rows.push(
                widget::selectable_text(error.message.as_str())
                    .font(design::interface_font())
                    .size(design::interface_size())
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill)
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            );
        }
        let status: Element<'_, Message> = if dialog.resolving.is_some() {
            widget::row::with_children(vec![
                crate::ui_controls::activity_spinner(14.0, design::muted_icon_color),
                design::caption("Checking the branch and destination…").into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into()
        } else {
            iced::widget::Space::new().width(Length::Fill).into()
        };
        rows.push(
            widget::row::with_children(vec![
                status,
                crate::ui_controls::button::secondary("Cancel")
                    .id(widget::Id::new("dialog.worktree.cancel"))
                    .on_press(Message::CancelDialog)
                    .into(),
                crate::ui_controls::button::primary(if dialog.resolving.is_some() {
                    "Creating…"
                } else {
                    "Create worktree"
                })
                .id(widget::Id::new("dialog.worktree.create"))
                .on_press_maybe(dialog.can_submit().then_some(Message::SubmitWorktree))
                .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
        );
        Some((
            dialog_ui::surface(widget::column::with_children(rows).spacing(space::M), 520.0),
            editable.then_some(Message::CancelDialog),
        ))
    }
}

/// A labelled input with the one line under it that is about it.
fn field<'a>(
    label: &'a str,
    input: impl Into<Element<'a, Message>>,
    note: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::column::with_capacity(3)
        .push(design::caption(label))
        .push(input)
        .push_maybe(note)
        .spacing(space::XS)
        .into()
}

fn error_note(message: &str) -> Element<'_, Message> {
    widget::text::caption(message)
        .size(design::scaled_interface_size(design::TEXT_CAPTION))
        .line_height(iced::widget::text::LineHeight::Absolute(
            design::scaled_interface_size(17.0).into(),
        ))
        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
        .class(cosmic::theme::Text::Custom(design::warning_text))
        .into()
}

/// The worktree dialog's first field, focused when it opens.
pub(super) fn worktree_dialog_focus_id() -> widget::Id {
    widget::Id::new("dialog.worktree.branch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::WorktreeOperationState;

    struct Fixture {
        app: Cagents,
        project: ProjectId,
        _state: tempfile::TempDir,
    }

    /// An app with a durable journal and one Git project in the sidebar.
    fn fixture() -> Fixture {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let state = tempfile::tempdir().unwrap();
        let (session, _) = SessionRuntime::open(
            state.path().join("state.sqlite3"),
            crate::model::BootId::new("test"),
        )
        .unwrap();
        app.session = Some(session);
        let project = ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                RepositoryId::new(),
                "cagents".into(),
                "/repo".into(),
                Vec::new(),
                Vec::new(),
            ));
        drop(app.open_worktree_dialog_with_source(project, PathBuf::from("/repo"), WorktreeDraft::default(), None, None, None, None, None));
        Fixture {
            app,
            project,
            _state: state,
        }
    }

    fn emitted_form_message(app: &Cagents, focus: widget::Id, editing: bool) -> Message {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Tree, operation},
        };
        let renderer = crate::app::tests::headless_renderer();
        let mut surface = app.dialog().expect("visible worktree form");
        let mut tree = Tree::new(&surface);
        let bounds = iced::Rectangle::with_size(iced::Size::new(1000.0, 800.0));
        let node = surface.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        surface.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(focus),
        );
        let key = if editing {
            keyboard::Key::Character("x".into())
        } else {
            keyboard::Key::Named(keyboard::key::Named::Enter)
        };
        let mut messages = Vec::new();
        surface.as_widget_mut().update(
            &mut tree,
            &iced::Event::Keyboard(keyboard::Event::KeyPressed {
                modified_key: key.clone(),
                key,
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: editing.then(|| "x".into()),
                repeat: false,
            }),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        messages.into_iter().find(|message| matches!(message,
            Message::ModalSurface { message, .. } if matches!(&**message,
                Message::WorktreeBranchChanged(_) | Message::SubmitWorktree | Message::CancelDialog)))
            .expect("actual form field/button publishes its instance-bound input")
    }

    #[test]
    fn actual_worktree_form_input_cannot_mutate_submit_or_cancel_a_replacement_instance() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        app.worktree_dialog
            .as_mut()
            .unwrap()
            .edit(WorktreeField::Branch, "feature/original".into());
        drop(app.finish_modal_update(Task::none()));
        let old_request = dialog(&app).request();
        let held = [
            emitted_form_message(&app, worktree_dialog_focus_id(), true),
            emitted_form_message(&app, widget::Id::new("dialog.worktree.create"), false),
            emitted_form_message(&app, widget::Id::new("dialog.worktree.cancel"), false),
        ];
        app.worktree_dialog = Some(WorktreeDialog::new(
            project,
            "/repo".into(),
            WorktreeDraft::branch("feature/replacement"),
        ));
        let replacement = dialog(&app).request();
        assert_ne!(replacement, old_request);
        // Deliberately do not reconcile first: admission must compare the
        // actual current request before any prelude can run stale input.
        for message in held {
            assert!(iced::runtime::task::into_stream(app.update(message)).is_none());
            assert_eq!(dialog(&app).request(), replacement);
            assert_eq!(dialog(&app).draft.branch, "feature/replacement");
            assert!(dialog(&app).resolving.is_none());
        }
        let current = emitted_form_message(&app, worktree_dialog_focus_id(), true);
        drop(app.update(current));
        assert_ne!(dialog(&app).draft.branch, "feature/replacement");
        let current = emitted_form_message(&app, widget::Id::new("dialog.worktree.create"), false);
        drop(app.update(current));
        let token = dialog(&app)
            .resolving
            .as_ref()
            .expect("current form can submit")
            .token
            .clone();
        let cancel = emitted_form_message(&app, widget::Id::new("dialog.worktree.cancel"), false);
        drop(app.update(cancel));
        assert!(app.worktree_dialog.is_none());
        assert!(token.is_cancelled());
    }

    fn dialog(app: &Cagents) -> &WorktreeDialog {
        app.worktree_dialog.as_ref().expect("the dialog is open")
    }

    fn resolution(app: &Cagents) -> OperationId {
        dialog(app)
            .resolving
            .as_ref()
            .expect("the request is resolving")
            .id
    }

    #[test]
    fn submitting_without_a_valid_branch_explains_why_and_starts_nothing() {
        let Fixture { mut app, .. } = fixture();
        let _ = app.update(Message::SubmitWorktree);
        let error = dialog(&app)
            .error
            .clone()
            .expect("empty input is explained");
        assert_eq!(error.field, Some(WorktreeField::Branch));
        assert!(dialog(&app).resolving.is_none());

        let _ = app.update(Message::WorktreeBranchChanged("feature..one".into()));
        assert!(
            dialog(&app).error.is_none(),
            "editing the field clears what was said about it"
        );
        let _ = app.update(Message::SubmitWorktree);
        let error = dialog(&app).error.clone().unwrap();
        assert_eq!(error.field, Some(WorktreeField::Branch));
        assert_eq!(error.message, "“feature..one” is not a valid branch name.");
        assert!(dialog(&app).resolving.is_none());
        assert!(app.worktree_operations.is_empty());
    }

    /// The dialog stays open and read-only while Git resolves the request,
    /// and a refusal comes back into it with everything that was typed
    /// (#499).
    #[test]
    fn a_refused_request_returns_to_the_dialog_with_its_input() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        let _ = app.update(Message::WorktreeBranchChanged("main".into()));
        let _ = app.update(Message::ToggleWorktreeAdvanced);
        let _ = app.update(Message::WorktreeBaseChanged("develop".into()));
        let _ = app.update(Message::ToggleWorktreeAdvanced);
        let _ = app.update(Message::SubmitWorktree);
        let id = resolution(&app);
        assert!(
            app.worktree_operations.is_empty(),
            "no sidebar row before the request resolves"
        );

        let _ = app.update(Message::WorktreeBranchChanged("typed while busy".into()));
        let _ = app.update(Message::SubmitWorktree);
        assert_eq!(dialog(&app).draft.branch, "main");
        assert_eq!(resolution(&app), id, "a second submit starts nothing");

        let refusal = WorktreeInputError::from_resolution(
            &ValidationError::InvalidBase(
                "An existing branch keeps its history; clear the base override or choose a new branch"
                    .into(),
            )
            .into(),
            &dialog(&app).draft,
        );
        let _ = app.update(Message::WorktreeInputResolved {
            project_id: project,
            operation_id: OperationId::new(),
            result: Err(WorktreeInputError::general("stale")),
        });
        assert_eq!(
            resolution(&app),
            id,
            "another resolution's answer is ignored"
        );

        let _ = app.update(Message::WorktreeInputResolved {
            project_id: project,
            operation_id: id,
            result: Err(refusal.clone()),
        });
        let dialog = dialog(&app);
        assert!(dialog.resolving.is_none());
        assert_eq!(dialog.error.as_ref(), Some(&refusal));
        assert_eq!(refusal.field, Some(WorktreeField::Base));
        assert!(dialog.advanced, "the field the error is about is shown");
        assert_eq!(
            dialog.draft,
            WorktreeDraft {
                base: "develop".into(),
                ..WorktreeDraft::branch("main")
            }
        );
        assert!(app.worktree_operations.is_empty());
    }

    #[test]
    fn cancelling_a_resolution_stops_it_and_drops_its_answer() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        let _ = app.update(Message::WorktreeBranchChanged("feature/one".into()));
        let _ = app.update(Message::SubmitWorktree);
        let id = resolution(&app);
        let token = dialog(&app).resolving.as_ref().unwrap().token.clone();
        let _ = app.update(Message::CancelDialog);
        assert!(app.worktree_dialog.is_none());
        assert!(token.is_cancelled());

        // A new dialog is not answered by the old resolution.
        let _ = app.update(Message::Sidebar(SidebarAction::NewWorktree {
            project_id: project,
            repository: PathBuf::from("/repo"),
        }));
        let _ = app.update(Message::WorktreeInputResolved {
            project_id: project,
            operation_id: id,
            result: Err(WorktreeInputError::general("cancelled")),
        });
        assert!(dialog(&app).error.is_none());
        assert!(app.worktree_operations.is_empty());
    }

    fn escape(captured: bool) -> Message {
        let key = keyboard::Key::Named(keyboard::key::Named::Escape);
        Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured,
        }
    }

    /// Escape closes the dialog on the first press although its focused
    /// field captures the key to blur itself, and stops a running
    /// resolution.
    #[test]
    fn escape_from_a_focused_field_closes_the_dialog() {
        let Fixture { mut app, .. } = fixture();
        // Under a dialog mounted above the form, the key is not the form's.
        app.settings_reset_confirm = true;
        let _ = app.update(escape(true));
        assert!(app.worktree_dialog.is_some());
        app.settings_reset_confirm = false;

        let _ = app.update(Message::WorktreeBranchChanged("feature/one".into()));
        let _ = app.update(Message::SubmitWorktree);
        let token = dialog(&app).resolving.as_ref().unwrap().token.clone();
        let _ = app.update(escape(true));
        assert!(app.worktree_dialog.is_none());
        assert!(token.is_cancelled());
    }

    fn request(id: OperationId) -> CreateWorktreeRequest {
        CreateWorktreeRequest {
            repository: "/repo".into(),
            managed_root: "/".into(),
            destination: "/one".into(),
            branch: crate::repository::ValidatedBranchName::parse("feature/one").unwrap(),
            base: crate::repository::BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: id,
        }
    }

    /// Settles a creation as the journal does when Git created nothing.
    fn not_created(app: &mut Cagents, id: OperationId, detail: &str) {
        let _ = app.finish_worktree_operation(
            id,
            1,
            Ok(crate::repository::WorktreeOperationReport {
                operation_id: id,
                generation: 1,
                request: Some(request(id)),
                state: WorktreeOperationState::NotCreated,
                outcome: None,
                detail: detail.into(),
                created_at_unix_ms: 0,
            }),
        );
    }

    /// A resolved request closes the dialog and becomes a creation row; if the
    /// creation then fails cleanly, the row reopens the same input, and goes
    /// only once that input resolves again (#499).
    #[test]
    fn a_failed_creation_reopens_its_input_in_the_dialog() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        let _ = app.update(Message::WorktreeBranchChanged("feature/one".into()));
        let _ = app.update(Message::ToggleWorktreeAdvanced);
        let _ = app.update(Message::WorktreeDestinationChanged("../one".into()));
        let _ = app.update(Message::SubmitWorktree);
        let id = resolution(&app);
        let draft = dialog(&app).draft.clone();
        let _ = app.update(Message::WorktreeInputResolved {
            project_id: project,
            operation_id: id,
            result: Ok(request(id)),
        });
        assert!(app.worktree_dialog.is_none());
        assert!(app.worktree_operations[&id].reopenable_draft().is_none());

        not_created(
            &mut app,
            id,
            "fatal: a branch named 'feature/one' already exists",
        );
        assert!(app.sidebar_model.projects[0].operations[0].reopenable);

        let _ = app.update(Message::Sidebar(SidebarAction::EditWorktreeOperation(id)));
        let shown = dialog(&app);
        assert_eq!(shown.draft, draft);
        assert!(shown.advanced);
        assert!(shown.resolving.is_none());
        assert_eq!(
            shown.error.as_ref().map(|error| error.message.as_str()),
            Some("fatal: a branch named 'feature/one' already exists.")
        );
        let _ = app.update(Message::CancelDialog);
        assert!(
            app.worktree_operations.contains_key(&id),
            "cancelling the reopened dialog keeps the failure"
        );

        let _ = app.update(Message::Sidebar(SidebarAction::EditWorktreeOperation(id)));
        let _ = app.update(Message::WorktreeBranchChanged("feature/two".into()));
        let _ = app.update(Message::SubmitWorktree);
        let retry = resolution(&app);
        let _ = app.update(Message::WorktreeInputResolved {
            project_id: project,
            operation_id: retry,
            result: Ok(request(retry)),
        });
        assert!(!app.worktree_operations.contains_key(&id));
        assert!(app.worktree_operations.contains_key(&retry));
    }

    /// Only a creation the journal settled as not created is offered back:
    /// any other ending may have left something behind.
    #[test]
    fn an_unsettled_failure_is_not_reopened() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        let _ = app.update(Message::CancelDialog);
        let id = OperationId::new();
        let _ = app.start_worktree_operation(
            project,
            request(id),
            Some(WorktreeDraft::branch("feature/one")),
        );
        let _ = app.finish_worktree_operation(id, 1, Err("worker lost".into()));
        assert!(!app.sidebar_model.projects[0].operations[0].reopenable);
        let _ = app.update(Message::Sidebar(SidebarAction::RetryWorktreeOperation(id)));
        assert!(app.worktree_dialog.is_none());
    }

    #[test]
    fn retrying_a_failed_creation_resubmits_its_input() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        let _ = app.update(Message::CancelDialog);
        let id = OperationId::new();
        let _ = app.start_worktree_operation(
            project,
            request(id),
            Some(WorktreeDraft::branch("feature/one")),
        );
        not_created(&mut app, id, "network");

        let _ = app.update(Message::Sidebar(SidebarAction::RetryWorktreeOperation(id)));
        let shown = dialog(&app);
        assert_eq!(shown.draft, WorktreeDraft::branch("feature/one"));
        assert!(shown.error.is_none());
        assert!(shown.resolving.is_some(), "retry submits straight away");
        assert!(app.worktree_operations.contains_key(&id));
    }

    /// A creation started from a pull request checkout reopens against that
    /// checkout, so a relative destination resolves where it first did.
    #[test]
    fn a_reopened_creation_keeps_the_repository_it_was_resolved_against() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        let _ = app.update(Message::CancelDialog);
        let id = OperationId::new();
        let checkout = PathBuf::from("/repo/checkouts/pr-7");
        let _ = app.start_worktree_operation(
            project,
            CreateWorktreeRequest {
                repository: checkout.clone(),
                ..request(id)
            },
            Some(WorktreeDraft::branch("feature/one")),
        );
        not_created(&mut app, id, "network");

        let _ = app.update(Message::Sidebar(SidebarAction::EditWorktreeOperation(id)));
        assert_eq!(dialog(&app).repository, checkout);
    }

    #[test]
    fn resolution_failures_name_the_field_that_has_to_change() {
        let typed = WorktreeDraft::branch("main");
        let read = |error: ValidationError, draft: &WorktreeDraft| {
            WorktreeInputError::from_resolution(&error.into(), draft)
        };
        let checked_out = read(
            ValidationError::InvalidBranch(
                "main is already checked out at /repo; use that worktree or switch its branch first"
                    .into(),
            ),
            &typed,
        );
        assert_eq!(checked_out.field, Some(WorktreeField::Branch));
        assert_eq!(
            checked_out.message,
            "main is already checked out at /repo; use that worktree or switch its branch first."
        );
        let occupied = read(ValidationError::DestinationOccupied, &typed);
        assert_eq!(occupied.field, Some(WorktreeField::Destination));
        assert!(occupied.message.contains("for this branch"));
        let custom = WorktreeDraft {
            destination: "../taken".into(),
            ..typed.clone()
        };
        assert!(
            read(ValidationError::DestinationOccupied, &custom)
                .message
                .contains("at this destination")
        );
        assert_eq!(
            read(ValidationError::Io("No such file".into()), &custom).field,
            Some(WorktreeField::Destination)
        );
        assert_eq!(
            read(ValidationError::Io("No such file".into()), &typed).field,
            None
        );
        let general = WorktreeInputError::from_resolution(
            &RepositoryError::InvalidOutput {
                field: "refs",
                reason: "truncated".into(),
            },
            &typed,
        );
        assert_eq!(general.field, None);
        assert_eq!(general.message, "Invalid git output for refs: truncated.");
    }

    #[test]
    fn a_linear_url_previews_the_branch_it_creates() {
        let mut dialog = WorktreeDialog::new(
            ProjectId::new(),
            "/repo".into(),
            WorktreeDraft::branch("https://linear.app/team/issue/ENG-12/fix-the-thing"),
        );
        assert_eq!(
            dialog.branch_preview().as_deref(),
            Some("ENG-12-fix-the-thing")
        );
        dialog.edit(WorktreeField::Branch, "feature/one".into());
        assert_eq!(dialog.branch_preview(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn project_creation_source_invalidation_cancels_live_workers_and_keeps_clone_identity() {
        const TEST: &str = "app::worktree_dialog::tests::project_creation_source_invalidation_cancels_live_workers_and_keeps_clone_identity";
        if !crate::git_watch::tests::fixture_isolated(TEST) { return; }
        let files = tempfile::tempdir().unwrap();
        isolated_git(files.path(), &["init", "--template=", "-b", "main"]);
        let source = WorktreeCreationSource::new(crate::repository::WorktreeAuthority::capture(files.path()).unwrap());
        let clone = source.clone();
        assert_eq!(source.id(), clone.id());
        for _ in 0..64 { drop(source.link_token(&CancellationToken::new()).unwrap()); }
        let token = CancellationToken::new();
        let lease = source.link_token(&token).unwrap();
        assert_eq!(source.lifetime.lock().unwrap().readers.len(), 1);
        assert!(!token.is_cancelled());
        clone.invalidate();
        assert!(token.is_cancelled());
        assert!(!source.is_available());
        assert!(source.service().is_err());
        let later = CancellationToken::new();
        assert!(source.link_token(&later).is_err());
        assert!(later.is_cancelled());
        drop(lease);
        clone.invalidate();
        assert!(source.lifetime.lock().unwrap().readers.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn actual_history_menu_uses_displayed_linked_source_and_retains_its_authority() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("displayed");
        super::super::git_binding::tests::init(&root, "original\n");
        let (mut app, key) = super::super::git_binding::tests::app_for(&root);
        let identity = crate::repository::CommitIdentity {
            name: "Fixture".into(), email: "fixture@example.invalid".into(),
            timestamp: 0, offset_minutes: 0,
        };
        let oid = crate::repository::Oid::parse("1234567890123456789012345678901234567890").unwrap();
        let source = app.sidebar_model.source_control.as_mut().unwrap();
        source.append_history(&[crate::repository::CommitSummary {
            oid: oid.clone(), parents: vec![], author: identity.clone(), committer: identity,
            subject: "Shown commit".into(), body: "".into(),
        }]);
        let row = source.history.last().unwrap();
        let action = sidebar::commands::registered(sidebar::commands::bound(source.command_binding_id(),
            SidebarAction::HistoryCommand { key: key.clone(), oid, mainline: row.mainline,
                message: row.message.clone(), command: sidebar::HistoryCommand::NewWorktree }));
        app.sidebar_state.apply(&SidebarAction::SetChangesTab {
            key: key.clone(),
            tab: sidebar::ChangesTab::History,
        });
        app.sidebar_model.projects.iter_mut().find(|project| project.id == key.project_id).unwrap()
            .repository = temporary.path().join("different-primary");
        drop(app.update(Message::Sidebar(action)));
        let dialog = app.worktree_dialog.as_ref().expect("registered History menu opens bound dialog");
        assert_eq!(dialog.repository, root);
        let retained = dialog.source.clone().expect("displayed authority remains attached");
        std::fs::rename(&root, temporary.path().join("original")).unwrap();
        super::super::git_binding::tests::init(&root, "replacement\n");
        assert!(retained.service().is_err(), "dialog cannot resolve or create from replacement root");
    }

    #[cfg(target_os = "linux")]
    fn real_history_source() -> (Fixture, tempfile::TempDir, PathBuf, crate::repository::Oid) {
        let mut fixture = fixture();
        let files = tempfile::tempdir().unwrap();
        let root = files.path().join("repository");
        std::fs::create_dir(&root).unwrap();
        isolated_git(&root, &["init", "--template=", "-b", "main"]);
        isolated_git(&root, &["config", "user.name", "Fixture"]);
        isolated_git(&root, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(root.join("tracked"), b"original source\n").unwrap();
        isolated_git(&root, &["add", "tracked"]);
        isolated_git(&root, &["commit", "-m", "Original"]);
        let oid = crate::repository::Oid::parse_object(isolated_git(&root, &["rev-parse", "HEAD"])).unwrap();
        fixture.app.settings.managed_worktree_directory = ".worktrees".into();
        fixture.app.sidebar_model.projects[0].repository = root.clone();
        let authority = crate::repository::WorktreeAuthority::capture(&root).unwrap();
        drop(fixture.app.open_worktree_dialog_bound(fixture.project, authority,
            WorktreeDraft { branch: "history-topic".into(), base: oid.as_str().into(), destination: String::new() }, None));
        (fixture, files, root, oid)
    }

    #[cfg(target_os = "linux")]
    fn isolated_git(root: &Path, arguments: &[&str]) -> String {
        let command = crate::repository::CommandSpec::new("git").cwd(root)
            .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "init.templateDir="])
            .args(arguments.iter().copied());
        let output = crate::git_watch::tests::run_fixture_command(
            crate::git_watch::tests::isolated_fixture_command(command));
        String::from_utf8(output.stdout).unwrap().trim_end_matches('\n').to_owned()
    }

    /// Drive only the creation pipeline's actual returned tasks. Repository
    /// refreshes after settlement are outside this fixture and are not run.
    #[cfg(target_os = "linux")]
    async fn finish_creation_tasks(app: &mut Cagents, task: Task<Message>) -> Vec<crate::repository::WorktreeOperationReport> {
        use cosmic::iced::futures::StreamExt;
        let mut pending = vec![task];
        let mut reports = Vec::new();
        while let Some(task) = pending.pop() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    match action {
                        iced::runtime::Action::Output(cosmic::Action::App(Message::WorktreeInputResolved { project_id, operation_id, result })) => {
                            pending.push(app.finish_worktree_input(project_id, operation_id, result));
                        }
                        iced::runtime::Action::Output(cosmic::Action::App(Message::WorktreeOperationFinished { operation, generation, result })) => {
                            if let Ok(report) = &result { reports.push(report.clone()); }
                            drop(app.finish_worktree_operation(operation, generation, result));
                        }
                        _ => {},
                    }
                }
            }
        }
        reports
    }

    #[cfg(target_os = "linux")]
    async fn resolve_history_input(task: Task<Message>) -> (ProjectId, OperationId, CreateWorktreeRequest) {
        use cosmic::iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("resolution worker exists");
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(Message::WorktreeInputResolved { project_id, operation_id, result })) = action {
                return (project_id, operation_id, result.expect("original source resolves"));
            }
        }
        panic!("resolution worker must deliver its request");
    }

    #[cfg(target_os = "linux")]
    fn replace_history_source(root: &Path) -> PathBuf {
        let parked = root.with_extension("original");
        std::fs::rename(root, &parked).unwrap();
        std::fs::create_dir(root).unwrap();
        isolated_git(root, &["init", "--template=", "-b", "replacement"]);
        std::fs::write(root.join("replacement-only"), b"preserve replacement\n").unwrap();
        parked
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn history_creation_current_source_reuses_one_scoped_service_through_journal() {
        if !crate::git_watch::tests::fixture_isolated("app::worktree_dialog::tests::history_creation_current_source_reuses_one_scoped_service_through_journal") { return; }
        let (mut fixture, _files, root, oid) = real_history_source();
        let source = dialog(&fixture.app).source.clone().unwrap();
        assert!(source.service.lock().unwrap().is_none(), "opening the dialog does not initialize a filesystem service on the UI thread");
        let task = fixture.app.submit_worktree_dialog();
        let reports = finish_creation_tasks(&mut fixture.app, task).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, WorktreeOperationState::Created, "{:?}", reports[0]);
        let request = reports[0].request.as_ref().unwrap();
        assert_eq!(request.repository, root);
        assert_eq!(isolated_git(&request.destination, &["rev-parse", "HEAD"]), oid.as_str());
        assert_eq!(isolated_git(&root, &["rev-parse", "HEAD"]), oid.as_str());
        assert_eq!(std::fs::read(root.join("tracked")).unwrap(), b"original source\n");
        let retained = fixture.app.worktree_operations[&reports[0].operation_id].source().unwrap();
        assert!(Arc::ptr_eq(&source.service, &retained.service));
        assert!(Arc::ptr_eq(&source.service().unwrap(), &retained.service().unwrap()), "resolution and creation share exactly one scoped service");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn history_creation_refuses_a_replaced_displayed_source_before_submit() {
        if !crate::git_watch::tests::fixture_isolated("app::worktree_dialog::tests::history_creation_refuses_a_replaced_displayed_source_before_submit") { return; }
        let (mut fixture, _files, root, _) = real_history_source();
        let parked = replace_history_source(&root);
        let task = fixture.app.submit_worktree_dialog();
        let reports = finish_creation_tasks(&mut fixture.app, task).await;
        assert!(reports.is_empty());
        assert!(dialog(&fixture.app).error.as_ref().unwrap().message.contains("replaced"));
        assert!(fixture.app.worktree_operations.is_empty());
        assert_eq!(std::fs::read(root.join("replacement-only")).unwrap(), b"preserve replacement\n");
        assert!(!root.join(".worktrees").exists());
        assert!(!parked.join(".worktrees").exists());
        assert!(isolated_git(&root, &["for-each-ref", "--format=%(refname)"]).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn history_creation_refuses_replacement_after_resolution_and_retry_keeps_original_authority() {
        if !crate::git_watch::tests::fixture_isolated("app::worktree_dialog::tests::history_creation_refuses_replacement_after_resolution_and_retry_keeps_original_authority") { return; }
        let (mut fixture, _files, root, _) = real_history_source();
        let source = dialog(&fixture.app).source.clone().unwrap();
        let task = fixture.app.submit_worktree_dialog();
        let (project, operation, request) = resolve_history_input(task).await;
        assert!(source.service.lock().unwrap().is_some(), "resolution initialized the original scoped service");
        let parked = replace_history_source(&root);
        let task = fixture.app.finish_worktree_input(project, operation, Ok(request));
        let reports = finish_creation_tasks(&mut fixture.app, task).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, WorktreeOperationState::NotCreated);
        assert!(reports[0].detail.contains("replaced"));
        let retained = fixture.app.worktree_operations[&operation].source().unwrap();
        assert!(Arc::ptr_eq(&source.service, &retained.service));
        drop(fixture.app.reopen_worktree_operation(operation, false));
        assert!(Arc::ptr_eq(&source.service, &dialog(&fixture.app).source.as_ref().unwrap().service));
        let task = fixture.app.submit_worktree_dialog();
        assert!(finish_creation_tasks(&mut fixture.app, task).await.is_empty());
        assert!(dialog(&fixture.app).error.as_ref().unwrap().message.contains("replaced"));
        assert_eq!(fixture.app.worktree_operations.len(), 1, "retry cannot mint replacement authority");
        assert_eq!(std::fs::read(root.join("replacement-only")).unwrap(), b"preserve replacement\n");
        assert!(!root.join(".worktrees").exists());
        assert!(!parked.join(".worktrees").exists());
        assert!(isolated_git(&root, &["for-each-ref", "--format=%(refname)"]).is_empty());
        assert_eq!(isolated_git(&parked, &["for-each-ref", "--format=%(refname)"]), "refs/heads/main");
    }

}
