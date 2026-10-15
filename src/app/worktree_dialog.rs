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
    pub skip_setup: bool,
    pub selected_branch_reference: Option<String>,
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

/// A resolution in flight: the dialog is read-only until it answers, and
/// cancelling the dialog stops it.
#[derive(Debug, Clone)]
struct Resolution {
    id: OperationId,
    token: CancellationToken,
}

#[derive(Debug, Clone)]
enum LinearLookup {
    None,
    Pending { id: OperationId, token: CancellationToken },
    Ready(crate::linear::IssuePreview),
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct WorktreeDialog {
    project_id: ProjectId,
    repository: PathBuf,
    draft: WorktreeDraft,
    advanced: bool,
    linear: LinearLookup,
    submit_after_linear: bool,
    choices: super::worktree_choices::Choices,
    choices_request: Option<(OperationId, CancellationToken)>,
    choices_error: Option<String>,
    submit_after_choices: bool,
    prefix_dirty: bool,
    trust_setup: bool,
    focused_field: Option<WorktreeField>,
    resolving: Option<Resolution>,
    error: Option<WorktreeInputError>,
    /// The failed creation this dialog reopened. Its row stays until this
    /// input resolves, so cancelling the dialog loses nothing.
    replaces: Option<OperationId>,
}

impl WorktreeDialog {
    pub fn new(project_id: ProjectId, repository: PathBuf, draft: WorktreeDraft) -> Self {
        Self {
            project_id,
            repository,
            advanced: draft.uses_options(),
            draft,
            linear: LinearLookup::None,
            submit_after_linear: false,
            choices: Default::default(),
            choices_request: None,
            choices_error: None,
            submit_after_choices: false,
            prefix_dirty: false,
            trust_setup: false,
            focused_field: Some(WorktreeField::Branch),
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
        self.focused_field = Some(field);
        let value = if field == WorktreeField::Branch { super::worktree_choices::sanitize_branch(&value) } else { value };
        self.submit_after_linear = false;
        self.submit_after_choices = false;
        if field == WorktreeField::Branch {
            self.draft.selected_branch_reference = None;
            if let LinearLookup::Pending { token, .. } = &self.linear { token.cancel(); }
            self.linear = LinearLookup::None;
            self.submit_after_linear = false;
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
        self.effective_branch()
            .and_then(crate::repository::ValidatedBranchName::parse)
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

    fn branch_input(&self) -> &str {
        match &self.linear {
            LinearLookup::Ready(issue) => &issue.branch_name,
            _ => &self.draft.branch,
        }
    }

    fn effective_branch(&self) -> Result<String, crate::repository::ValidationError> {
        crate::repository::worktree_branch_input(self.branch_input()).map(|branch| self.choices.effective_branch(&branch))
    }

    pub fn focus_field(&mut self, field: WorktreeField) {
        if self.resolving.is_none() { self.focused_field = Some(field); }
    }

    pub fn dismiss_suggestions(&mut self, field: WorktreeField) {
        if self.focused_field == Some(field) { self.focused_field = None; }
    }

    pub fn edit_prefix(&mut self, value: String) {
        if self.resolving.is_none() {
            self.choices.prefix = super::worktree_choices::sanitize_branch(&value);
            self.prefix_dirty = true;
            self.error = None;
        }
    }

    pub fn edit_setup_skip(&mut self, skip: bool) {
        if self.resolving.is_none() {
            self.draft.skip_setup = skip;
            self.submit_after_choices = false;
            self.submit_after_linear = false;
            self.error = None;
        }
    }

    pub fn edit_setup_trust(&mut self, trust: bool) {
        if self.resolving.is_none() {
            self.trust_setup = trust;
            self.submit_after_choices = false;
            self.submit_after_linear = false;
            self.error = None;
        }
    }

    fn setup_approved(&self) -> bool {
        self.draft.skip_setup || (self.choices.setup_error.is_none() &&
            self.choices.setup_plan.as_ref().is_none_or(|plan| !plan.requires_trust() || self.choices.setup_trusted || self.trust_setup))
    }

    fn choices_task(&mut self) -> Task<Message> {
        let id = OperationId::new();
        let token = CancellationToken::new();
        self.choices_request = Some((id, token.clone()));
        let repository = self.repository.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || super::worktree_choices::Choices::load(&repository, &token))
                .await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::WorktreeChoicesResolved { id, result }
        })
    }

    fn suggestions(&self, field: WorktreeField, editable: bool) -> Option<Element<'_, Message>> {
        if !editable || self.focused_field != Some(field) || self.choices_request.is_some() { return None; }
        let query = match field {
            WorktreeField::Branch => self.draft.branch.as_str(),
            WorktreeField::Base => self.draft.base.as_str(),
            WorktreeField::Destination => return None,
        };
        if query.contains("://") { return None; }
        let choices = self.choices.suggestions(query, field == WorktreeField::Base);
        if choices.is_empty() { return None; }
        let rows = choices.into_iter().map(|choice| {
            let kind = match choice.kind { super::worktree_choices::Kind::Local => "local", super::worktree_choices::Kind::Remote => "remote", super::worktree_choices::Kind::Tag => "tag" };
            crate::ui_controls::button::quiet(format!("{} · {kind}", choice.name))
                .width(Length::Fill)
                .on_press(Message::WorktreeChoiceSelected { field, reference: choice.reference.clone() }).into()
        }).collect::<Vec<Element<'_, Message>>>();
        Some(widget::container(widget::scrollable(widget::column::with_children(rows).spacing(space::XS)))
            .width(432.0).max_height(216.0).into())
    }

    fn linear_note(&self) -> Option<String> {
        match &self.linear {
            LinearLookup::None => self.branch_preview().map(|branch| format!("Branch: {branch}")),
            LinearLookup::Pending { .. } => Some("Fetching Linear issue…".into()),
            LinearLookup::Ready(issue) => Some(format!("{}: {} · {} → branch {}", issue.identifier, issue.title, issue.state, issue.branch_name)),
            LinearLookup::Unavailable(reason) => {
                let branch = crate::repository::worktree_branch_input(&self.draft.branch)
                    .unwrap_or_else(|_| self.draft.branch.clone());
                Some(format!("Issue not fetched: {reason}. Branch: {branch}; the checkout will not be linked."))
            },
        }
    }

    fn lookup(&mut self) -> Task<Message> {
        let Ok(reference) = crate::linear::issue_reference(&self.draft.branch) else { return Task::none() };
        let id = OperationId::new();
        let token = CancellationToken::new();
        self.linear = LinearLookup::Pending { id, token: token.clone() };
        cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_millis(350)).await;
            let result = if token.is_cancelled() {
                Err("Lookup cancelled".into())
            } else {
                tokio::task::spawn_blocking(move || {
                    if token.is_cancelled() { return Err("Lookup cancelled".into()); }
                    crate::linear::IssuePreview::fetch(&reference, None).map_err(|error| error.message)
                }).await.map_err(|_| "Linear worker unavailable".to_owned()).and_then(|result| result)
            };
            Message::WorktreeLinearResolved { id, result }
        })
    }

    fn can_submit(&self) -> bool {
        self.resolving.is_none() && self.choices_request.is_none() && self.setup_approved() && !matches!(self.linear, LinearLookup::Pending { .. }) && !self.draft.branch.trim().is_empty()
    }

    /// Stops a resolution the user no longer wants.
    pub fn cancel(&self) {
        if let Some((_, token)) = &self.choices_request { token.cancel(); }
        if let LinearLookup::Pending { token, .. } = &self.linear { token.cancel(); }
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
        if let Some(replaced) = self.worktree_dialog.take() {
            replaced.cancel();
        }
        let dialog = self
            .worktree_dialog
            .insert(WorktreeDialog::new(project_id, repository, draft));
        let focus = match error {
            Some(error) => dialog.show_error(error),
            None => WorktreeField::Branch.focus(),
        };
        Task::batch([focus, dialog.lookup(), dialog.choices_task()])
    }

    pub(super) fn edit_worktree_branch(&mut self, value: String) -> Task<Message> {
        let Some(dialog) = self.worktree_dialog.as_mut() else { return Task::none() };
        if dialog.resolving.is_some() { return Task::none() }
        dialog.edit(WorktreeField::Branch, value);
        dialog.lookup()
    }

    pub(super) fn finish_worktree_linear(&mut self, id: OperationId, result: Result<crate::linear::IssuePreview, String>) -> Task<Message> {
        if let Some(dialog) = self.worktree_dialog.as_mut()
            && matches!(&dialog.linear, LinearLookup::Pending { id: pending, .. } if *pending == id)
        {
            dialog.linear = match result {
                Ok(issue) if crate::repository::ValidatedBranchName::parse(&issue.branch_name).is_ok() => LinearLookup::Ready(issue),
                Ok(_) => LinearLookup::Unavailable("Linear suggested an invalid branch name".into()),
                Err(reason) => LinearLookup::Unavailable(reason),
            };
            if std::mem::take(&mut dialog.submit_after_linear)
                && matches!(dialog.linear, LinearLookup::Ready(_))
            {
                return self.submit_worktree_dialog();
            }
        }
        Task::none()
    }

    pub(super) fn finish_worktree_choices(&mut self, id: OperationId, result: Result<super::worktree_choices::Choices, String>) -> Task<Message> {
        let Some(dialog) = self.worktree_dialog.as_mut().filter(|dialog| dialog.choices_request.as_ref().is_some_and(|(pending, _)| *pending == id)) else { return Task::none(); };
        dialog.choices_request = None;
        match result {
            Ok(mut choices) => {
                if dialog.prefix_dirty { choices.prefix.clone_from(&dialog.choices.prefix); }
                dialog.choices = choices;
            }
            Err(error) => {
                dialog.choices_error = Some(error.clone());
                dialog.choices.setup_error = Some(format!("Project setup could not be inspected: {error}. Retry opening the dialog, or explicitly skip setup."));
            }
        }
        if std::mem::take(&mut dialog.submit_after_choices) { return self.submit_worktree_dialog(); }
        Task::none()
    }

    pub(super) fn select_worktree_choice(&mut self, field: WorktreeField, reference: String) -> Task<Message> {
        let Some(dialog) = self.worktree_dialog.as_mut() else { return Task::none(); };
        if dialog.resolving.is_some() { return Task::none(); }
        let Some(choice) = dialog.choices.find(&reference) else { return Task::none(); };
        let value = if field == WorktreeField::Base { choice.reference.clone() } else { choice.name.clone() };
        dialog.edit(field, value);
        if field == WorktreeField::Branch { dialog.draft.base.clear(); dialog.draft.selected_branch_reference = Some(reference); }
        dialog.focused_field = None;
        Task::none()
    }

    /// Resolves the dialog's request, keeping it open until Git answers.
    pub(super) fn submit_worktree_dialog(&mut self) -> Task<Message> {
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
        if dialog.choices_request.is_some() {
            dialog.submit_after_choices = true;
            return Task::none();
        }
        if matches!(dialog.linear, LinearLookup::Pending { .. }) {
            dialog.submit_after_linear = true;
            return Task::none();
        }
        if let Err(error) = dialog.validate() {
            return dialog.show_error(error);
        }
        if !dialog.setup_approved() {
            return dialog.show_error(WorktreeInputError::general(dialog.choices.setup_error.as_deref()
                .unwrap_or("Review and trust this project's setup commands, or skip setup for this creation.")));
        }
        let operation_id = OperationId::new();
        let token = CancellationToken::new();
        dialog.error = None;
        dialog.resolving = Some(Resolution {
            id: operation_id,
            token: token.clone(),
        });
        let project_id = dialog.project_id;
        let draft = dialog.draft.clone();
        let linear_issue = match &dialog.linear {
            LinearLookup::Ready(issue) => Some(crate::linear::context::LinkedIssue { issue_id: issue.id.clone(), workspace: None }),
            _ => None,
        };
        let selected_ref = dialog.draft.selected_branch_reference.clone();
        let prefix = dialog.prefix_dirty.then(|| dialog.choices.prefix.clone());
        let trust = (dialog.trust_setup && !dialog.draft.skip_setup && !dialog.choices.setup_trusted)
            .then(|| dialog.choices.primary.clone().zip(dialog.choices.setup_plan.clone())).flatten();
        let branch = dialog.effective_branch().expect("branch was validated before resolving");
        let input = WorktreeInput {
            repository: dialog.repository.clone(),
            base: dialog.choices.creation_base(&branch, &draft.base, selected_ref.as_deref()),
            input: branch,
            managed_directory,
            destination: draft.destination.clone(),
            operation_id,
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if let Some((primary, plan)) = trust {
                    crate::worktree_setup::trust(&primary, &plan, &token).map_err(|error| WorktreeInputError::general(error.to_string()))?;
                }
                if let Some(prefix) = prefix {
                    super::worktree_choices::save_prefix(&input.repository, &prefix, &token)
                        .map_err(WorktreeInputError::general)?;
                }
                RepositoryService::system("git")
                    .resolve_worktree_request_selected(&input, selected_ref.as_deref(), &token)
                    .map(|mut request| { request.linear_issue = linear_issue; request })
                    .map_err(|error| WorktreeInputError::from_resolution(&error, &draft))
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
                let draft = dialog.draft.clone();
                let setup_intent = (dialog.choices.setup_plan.is_some() || dialog.draft.skip_setup)
                    .then(|| super::worktree_setup_ui::SetupIntent {
                        primary: dialog.choices.primary.clone(), plan: dialog.choices.setup_plan.clone(), skip: dialog.draft.skip_setup,
                    });
                let replaces = dialog.replaces;
                self.worktree_dialog = None;
                if let Some(replaced) = replaces {
                    let _ = self.dismiss_worktree_operation(replaced);
                }
                let id = request.operation_id;
                let start = self.start_worktree_operation(project_id, request, Some(draft));
                if let Some(intent) = setup_intent { self.set_worktree_setup_intent(id, intent); }
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
        let Some((project_id, repository, draft, failure)) =
            self.worktree_operations.get(&id).and_then(|operation| {
                operation.reopenable_draft().map(|draft| {
                    (
                        operation.project(),
                        operation.repository().map(Path::to_path_buf),
                        draft.clone(),
                        operation.failure().map(WorktreeInputError::recorded),
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
        let error = failure.filter(|_| !submit);
        let focus = self.open_worktree_dialog(project_id, repository, draft, error);
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
                .on_focus(Message::WorktreeFieldFocused(field))
                .on_submit(|_| Message::SubmitWorktree);
            if editable {
                input = input.on_input(on_input);
            }
            if let Some(error) = field_error(field) {
                input = input.error(error);
            }
            crate::ui_controls::popover(input, dialog.suggestions(field, editable), widget::popover::Position::Bottom,
                Message::WorktreeSuggestionsDismissed(field))
        };
        let project = self
            .sidebar_model
            .project(dialog.project_id)
            .map_or("project", |project| project.name.as_str());

        let branch_note = field_error(WorktreeField::Branch)
            .map(error_note)
            .or_else(|| {
                dialog
                    .linear_note()
                    .map(|note| design::caption(note).into())
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
        let outcome = if dialog.draft.branch.trim().is_empty() {
            iced::widget::Space::new().into()
        } else {
            let branch = crate::repository::worktree_branch_input(dialog.branch_input()).unwrap_or_else(|_| dialog.draft.branch.clone());
            design::caption(dialog.choices.outcome_selected(&branch, &dialog.draft.base, dialog.draft.selected_branch_reference.as_deref())).into()
        };
        rows.insert(2, outcome);
        rows.push(match &dialog.choices_error {
            Some(error) => design::caption(format!("Branch suggestions unavailable: {error}")).into(),
            None => iced::widget::Space::new().into(),
        });
        rows.push(design::caption("Uses locally available refs; remote defaults are not refreshed.").into());
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
                "Branch prefix (this project)",
                {
                    let input = crate::ui_controls::field::text("agent/ (optional)", &dialog.choices.prefix)
                        .id(widget::Id::new("dialog.worktree.prefix"))
                        .on_focus(Message::WorktreeFieldFocused(WorktreeField::Destination));
                    if editable { input.on_input(Message::WorktreePrefixChanged) } else { input }
                },
                Some(design::caption("Applied to new branches; saved in this repository when you create a worktree.").into()),
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
        if let Some(plan) = &dialog.choices.setup_plan {
            if !plan.copy.is_empty() { rows.push(design::caption(format!("Copies from the primary checkout: {}", plan.copy.join(", "))).into()); }
            if !plan.commands.is_empty() {
                rows.push(design::caption("Setup commands (run in an interactive terminal in the new worktree)").into());
                rows.push(widget::scrollable(widget::selectable_text(plan.commands.join("\n")).size(design::TEXT_BODY))
                    .height(96.0).into());
                if !dialog.choices.setup_trusted {
                    rows.push(crate::ui_controls::field::checkbox(dialog.trust_setup)
                        .label("Trust these setup commands for this project")
                        .on_toggle_maybe(editable.then_some(Message::WorktreeSetupTrustChanged as fn(bool) -> Message)).into());
                } else { rows.push(design::caption("Setup commands are trusted for this project.").into()); }
            }
        }
        if let Some(error) = &dialog.choices.setup_error { rows.push(error_note(error)); }
        if dialog.choices.setup_plan.is_some() || dialog.choices.setup_error.is_some() {
            rows.push(crate::ui_controls::field::checkbox(dialog.draft.skip_setup).label("Skip setup for this worktree")
                .on_toggle_maybe(editable.then_some(Message::WorktreeSetupSkipChanged as fn(bool) -> Message)).into());
        }
        if let Some(error) = dialog.error.as_ref().filter(|error| error.field.is_none()) {
            rows.push(
                widget::selectable_text(error.message.as_str())
                    .size(design::TEXT_BODY)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill)
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            );
        }
        let status: Element<'_, Message> = if dialog.choices_request.is_some() {
            design::caption("Loading local branches…").into()
        } else if dialog.resolving.is_some() {
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
        let actions = dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("dialog.worktree.cancel"))
                .on_press(Message::CancelDialog),
            crate::ui_controls::button::primary(if dialog.resolving.is_some() { "Creating…" } else { "Create worktree" })
                .id(widget::Id::new("dialog.worktree.create"))
                .on_press_maybe(dialog.can_submit().then_some(Message::SubmitWorktree)),
        );
        let form = widget::container(widget::scrollable(widget::column::with_children(rows).spacing(space::M)).height(Length::Shrink))
            .max_height((self.window_height - 220.0).max(120.0));
        let content = widget::column::with_children(vec![form.into(), widget::container(status).height(24.0).width(Length::Fill).into(), actions]).spacing(space::M);
        Some((
            dialog_ui::surface(content, 520.0),
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
        .size(design::TEXT_CAPTION)
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
        let _ = app.update(Message::Sidebar(SidebarAction::NewWorktree {
            project_id: project,
            repository: PathBuf::from("/repo"),
        }));
        // Tasks are not driven in these synchronous handler tests. Supply the
        // one-shot ref read explicitly before testing resolution behavior.
        refs_loaded(&mut app);
        Fixture {
            app,
            project,
            _state: state,
        }
    }

    fn refs_loaded(app: &mut Cagents) {
        if let Some(id) = app.worktree_dialog.as_ref().and_then(|dialog| dialog.choices_request.as_ref().map(|(id, _)| *id)) {
            drop(app.finish_worktree_choices(id, Ok(Default::default())));
        }
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
    fn setup_requires_explicit_trust_or_per_creation_skip() {
        let mut dialog = WorktreeDialog::new(ProjectId::new(), "/repo".into(), WorktreeDraft::branch("feature/setup"));
        dialog.choices.setup_plan = Some(crate::worktree_setup::SetupPlan {
            copy: vec![".env".into()], commands: vec!["pnpm install".into()], fingerprint: "a".repeat(64),
        });
        assert!(!dialog.setup_approved());
        assert!(!dialog.can_submit());
        dialog.edit_setup_trust(true);
        assert!(dialog.setup_approved());
        dialog.edit_setup_trust(false);
        dialog.edit_setup_skip(true);
        assert!(dialog.setup_approved());
        assert!(dialog.draft.skip_setup);
        dialog.edit_setup_skip(false);
        assert!(!dialog.setup_approved());
        dialog.choices.setup_trusted = true;
        assert!(dialog.setup_approved());
    }

    #[test]
    fn branch_input_sanitizes_pasted_titles_applies_prefix_and_rejects_dot_components() {
        let mut dialog = WorktreeDialog::new(ProjectId::new(), "/repo".into(), WorktreeDraft::default());
        dialog.edit_prefix("agent/".into());
        dialog.edit(WorktreeField::Branch, "Fix Login Redirect".into());
        assert_eq!(dialog.draft.branch, "fix-login-redirect");
        assert_eq!(dialog.effective_branch().unwrap(), "agent/fix-login-redirect");
        assert!(dialog.validate().is_ok());
        dialog.edit(WorktreeField::Branch, "feat/.x".into());
        assert!(dialog.validate().is_err());
    }

    #[test]
    fn choices_answers_are_scoped_to_the_current_dialog_and_selection_uses_full_base_ref() {
        let Fixture { mut app, .. } = fixture();
        let id = OperationId::new();
        app.worktree_dialog.as_mut().unwrap().choices_request = Some((id, CancellationToken::new()));
        let mut choices = super::super::worktree_choices::Choices::default();
        choices.prefix = "agent/".into();
        choices.refs.push(super::super::worktree_choices::Choice {
            name: "origin/release".into(), reference: "refs/remotes/origin/release".into(), oid: "a".repeat(40),
            kind: super::super::worktree_choices::Kind::Remote, ahead: 0, behind: 0, remote_branch: Some("release".into()),
        });
        drop(app.finish_worktree_choices(OperationId::new(), Ok(choices.clone())));
        assert!(dialog(&app).choices_request.is_some());
        drop(app.finish_worktree_choices(id, Ok(choices)));
        assert_eq!(dialog(&app).choices.prefix, "agent/");
        drop(app.select_worktree_choice(WorktreeField::Base, "refs/remotes/origin/release".into()));
        assert_eq!(dialog(&app).draft.base, "refs/remotes/origin/release");
        drop(app.select_worktree_choice(WorktreeField::Branch, "refs/remotes/origin/release".into()));
        assert_eq!(dialog(&app).draft.branch, "origin/release");
        assert!(dialog(&app).draft.base.is_empty());
        assert_eq!(dialog(&app).effective_branch().unwrap(), "origin/release");
        assert_eq!(dialog(&app).draft.selected_branch_reference.as_deref(), Some("refs/remotes/origin/release"));
        let saved = dialog(&app).draft.clone();
        assert_eq!(WorktreeDialog::new(ProjectId::new(), "/repo".into(), saved).draft.selected_branch_reference.as_deref(), Some("refs/remotes/origin/release"));
        app.worktree_dialog.as_mut().unwrap().edit(WorktreeField::Branch, "new-local".into());
        assert!(dialog(&app).draft.selected_branch_reference.is_none());
    }

    #[test]
    fn fetched_linear_issue_selects_its_branch_and_stale_results_are_ignored() {
        let Fixture { mut app, .. } = fixture();
        let id = OperationId::new();
        app.worktree_dialog.as_mut().unwrap().linear = LinearLookup::Pending { id, token: CancellationToken::new() };
        let issue = crate::linear::IssuePreview {
            id: "2b498c52-7dc7-4c76-ad59-dfc488c40111".into(),
            identifier: "ENG-123".into(), title: "Fix login redirect".into(),
            branch_name: "user/eng-123-fix-login-redirect".into(),
            url: "https://linear.app/team/issue/ENG-123".into(), state: "Todo".into(),
        };
        let _ = app.finish_worktree_linear(OperationId::new(), Ok(issue.clone()));
        assert!(matches!(dialog(&app).linear, LinearLookup::Pending { .. }));
        let _ = app.finish_worktree_linear(id, Ok(issue));
        assert_eq!(dialog(&app).branch_input(), "user/eng-123-fix-login-redirect");
        assert!(dialog(&app).linear_note().unwrap().contains("Fix login redirect"));
        app.worktree_dialog.as_mut().unwrap().edit(WorktreeField::Branch, "feature/plain".into());
        assert_eq!(dialog(&app).branch_input(), "feature/plain");
        let _ = app.finish_worktree_linear(id, Err("old failure".into()));
        assert!(matches!(dialog(&app).linear, LinearLookup::None));
    }

    #[test]
    fn failed_linear_lookup_keeps_creation_available_with_an_explicit_note() {
        let mut dialog = WorktreeDialog::new(ProjectId::new(), "/repo".into(), WorktreeDraft::branch("ENG-123"));
        dialog.linear = LinearLookup::Unavailable("No saved credential".into());
        assert!(dialog.can_submit());
        assert!(dialog.validate().is_ok());
        assert_eq!(dialog.branch_input(), "ENG-123");
        assert!(dialog.linear_note().unwrap().contains("Issue not fetched"));
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
            linear_issue: None,
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
        refs_loaded(&mut app);
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
        refs_loaded(&mut app);
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
        assert!(dialog(&app).submit_after_choices, "retry waits for the fresh ref read");
        refs_loaded(&mut app);
        let shown = dialog(&app);
        assert_eq!(shown.draft, WorktreeDraft::branch("feature/one"));
        assert!(shown.error.is_none());
        assert!(shown.resolving.is_some(), "retry submits after the fresh ref read");
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
        refs_loaded(&mut app);
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
}
