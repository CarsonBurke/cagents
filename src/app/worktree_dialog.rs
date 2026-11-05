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
    pub(super) pr_checkout: Option<super::pr_checkout_ui::PrCheckoutOrigin>,
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
pub struct WorktreeDialog {
    project_id: ProjectId,
    repository: PathBuf,
    draft: WorktreeDraft,
    advanced: bool,
    checkout_notice: Option<String>,
    resolving: Option<Resolution>,
    error: Option<WorktreeInputError>,
    /// The failed creation this dialog reopened. Its row stays until this
    /// input resolves, so cancelling the dialog loses nothing.
    replaces: Option<OperationId>,
}

impl WorktreeDialog {
    pub(super) fn is_resolving(&self) -> bool {
        self.resolving.is_some()
    }

    pub(super) fn set_checkout_notice(&mut self, notice: String) {
        self.checkout_notice = Some(notice);
    }

    pub fn new(project_id: ProjectId, repository: PathBuf, draft: WorktreeDraft) -> Self {
        Self {
            project_id,
            repository,
            advanced: draft.uses_options(),
            checkout_notice: draft
                .pr_checkout
                .as_ref()
                .map(|origin| origin.notice.clone()),
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
        if self.resolving.is_some()
            || (self.checkout_notice.is_some()
                && matches!(field, WorktreeField::Branch | WorktreeField::Base))
        {
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
        if let Some(replaced) = self.worktree_dialog.take() {
            replaced.cancel();
        }
        let dialog = self
            .worktree_dialog
            .insert(WorktreeDialog::new(project_id, repository, draft));
        match error {
            Some(error) => dialog.show_error(error),
            None => WorktreeField::Branch.focus(),
        }
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
        if self
            .worktree_dialog
            .as_ref()
            .and_then(|dialog| dialog.draft.pr_checkout.as_ref())
            .is_some_and(|origin| !self.pr_creation_origin_is_current(origin))
        {
            return self
                .worktree_dialog
                .as_mut()
                .unwrap()
                .show_error(WorktreeInputError::general(
                    "The owning PR changed or closed. Prepare checkout again from its review.",
                ));
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
                let service = RepositoryService::system("git");
                let resolved = match draft.pr_checkout.as_ref() {
                    Some(origin) => {
                        service.resolve_worktree_request_for_pr(&input, &origin.consent, &token)
                    }
                    None => service.resolve_worktree_request(&input, &token),
                };
                resolved.map_err(|error| WorktreeInputError::from_resolution(&error, &draft))
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
        if self
            .worktree_dialog
            .as_ref()
            .filter(|dialog| {
                dialog.project_id == project_id
                    && dialog
                        .resolving
                        .as_ref()
                        .is_some_and(|resolution| resolution.id == operation_id)
            })
            .and_then(|dialog| dialog.draft.pr_checkout.as_ref())
            .is_some_and(|origin| !self.pr_creation_origin_is_current(origin))
        {
            let dialog = self.worktree_dialog.as_mut().unwrap();
            dialog.resolving = None;
            return dialog.show_error(WorktreeInputError::general(
                "The owning PR changed while resolving. Prepare checkout again from its review.",
            ));
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
                let draft = dialog.draft.clone();
                let replaces = dialog.replaces;
                self.worktree_dialog = None;
                if let Some(replaced) = replaces {
                    let _ = self.dismiss_worktree_operation(replaced);
                }
                let start = self.start_worktree_operation(project_id, request, Some(draft));
                let focus = self.restore_dialog_focus(|| self.focus_context_overlay_if_exposed());
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
                .on_submit(|_| Message::SubmitWorktree);
            if editable
                && !(dialog.checkout_notice.is_some()
                    && matches!(field, WorktreeField::Branch | WorktreeField::Base))
            {
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
        if let Some(notice) = &dialog.checkout_notice {
            rows.insert(1, design::caption(notice).into());
        }
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
                    .size(design::TEXT_BODY)
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
        Fixture {
            app,
            project,
            _state: state,
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
    fn new_pr_checkout_retains_original_owner_through_resolution_worker_and_publication() {
        use crate::model::WorktreeId;
        use crate::repository::{
            BaseRevision, PrWorktreeCreationConsent, ValidatedBranchName, WorktreeLock,
            WorktreeRecord,
        };
        for boundary in ["submit", "resolved", "accepted", "published"] {
            let Fixture {
                mut app,
                project,
                _state,
            } = fixture();
            drop(app.update(Message::CancelDialog));
            let (card, task) = app.add_pull_requests_card("/repo".into());
            drop(task);
            let mut loaded = crate::pull_request_review::tests::loaded(Vec::new(), Vec::new());
            loaded.pull_request.head_repository =
                Some(crate::github::GitHubHeadRepository { name: "r".into() });
            loaded.pull_request.head_repository_owner = Some(crate::github::GitHubActor {
                login: "o".into(),
                name: None,
                is_bot: false,
            });
            let mut target = crate::pull_request_review::tests::target(1);
            target.pull_request = Arc::new(loaded.pull_request.clone());
            let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
                panic!("review");
            };
            drop(
                pane.review
                    .update(pull_request_review::Action::Open(target)),
            );
            let generation = pane.review.selection_generation();
            // This app owns a real review cache. Complete its miss before
            // answering the live load that the cache completion starts.
            drop(pane.review.update(pull_request_review::Action::Cached {
                generation,
                result: Ok(None),
            }));
            drop(pane.review.update(pull_request_review::Action::Loaded {
                generation,
                load: 1,
                result: Ok(Arc::new(loaded.clone())),
            }));
            assert!(matches!(
                pane.review.load_state(),
                pull_request_review::LoadState::Ready
            ));
            drop(pane.review.take_effects());
            drop(pane.review.update(pull_request_review::Action::Checkout));
            let effects = pane.review.take_effects();
            let [pull_request_review::Effect::Checkout(target)] = effects.as_slice() else {
                panic!("checkout");
            };
            let target = target.clone();
            let origin = super::super::pr_checkout_ui::PrCheckoutOrigin {
                card,
                project,
                generation,
                consent: PrWorktreeCreationConsent::new(
                    target.head_repository.clone().unwrap(),
                    ValidatedBranchName::parse(&target.head_ref_name).unwrap(),
                    target.head.clone(),
                    "origin".into(),
                    true,
                )
                .unwrap(),
                target,
                notice: "Reviewed PR".into(),
            };
            drop(app.open_worktree_dialog(
                project,
                "/repo".into(),
                WorktreeDraft {
                    branch: "origin/feature".into(),
                    pr_checkout: Some(origin.clone()),
                    ..Default::default()
                },
                None,
            ));
            let mut id = OperationId::new();
            if boundary != "submit" {
                // Accept but do not poll the read-only resolution Task.
                drop(app.update(Message::SubmitWorktree));
                id = resolution(&app);
            }
            let request = CreateWorktreeRequest {
                repository: "/repo".into(),
                managed_root: "/worktrees".into(),
                managed_ignore_root: None,
                destination: "/worktrees/feature".into(),
                branch: ValidatedBranchName::parse("feature").unwrap(),
                base: BaseRevision::remote_ref("refs/remotes/origin/feature").unwrap(),
                track: true,
                operation_id: id,
                setup_intent: None,
            };
            if matches!(boundary, "accepted" | "published") {
                // Accept the actual operation Task and hold it before worker execution.
                drop(app.update(Message::WorktreeInputResolved {
                    project_id: project,
                    operation_id: id,
                    result: Ok(request.clone()),
                }));
                assert!(app.worktree_operations[&id].pr_creation_running(card));
                assert_eq!(app.worktree_operations[&id].pr_origin(), Some(&origin));
                drop(app.update(Message::CloseCardUnasked(card)));
                assert!(app.cards.contains_key(&card));
            }
            // Deliver same-OID metadata adoption through the actual App reducer behind the dialog.
            loaded.pull_request.head_ref_name = "renamed".into();
            drop(app.update(Message::PullRequestReview {
                card,
                action: pull_request_review::Action::Loaded {
                    generation,
                    load: 1,
                    result: Ok(Arc::new(loaded)),
                },
            }));
            match boundary {
                "submit" => {
                    drop(app.update(Message::SubmitWorktree));
                    assert!(dialog(&app).error.is_some());
                    assert!(dialog(&app).resolving.is_none());
                }
                "resolved" => {
                    drop(app.update(Message::WorktreeInputResolved {
                        project_id: project,
                        operation_id: id,
                        result: Ok(request.clone()),
                    }));
                    assert!(dialog(&app).error.is_some());
                }
                _ => assert!(
                    app.worktree_operations[&id]
                        .pr_cancellation_for_test()
                        .unwrap()
                        .is_cancelled()
                ),
            }
            if boundary == "published" {
                // Substitute only a completed journal report; this checks reducer publication, not a live Git worker.
                let outcome = WorktreeCreationOutcome::Created {
                    worktree: WorktreeRecord {
                        id: WorktreeId::new(),
                        path: request.destination.clone(),
                        created_at_unix_ms: None,
                        head_oid: Some(origin.target.head.as_str().into()),
                        branch: Some("refs/heads/feature".into()),
                        detached: false,
                        bare: false,
                        lock: WorktreeLock {
                            locked: false,
                            reason: None,
                        },
                        prunable_reason: None,
                    },
                    warnings: Vec::new(),
                };
                drop(app.finish_worktree_operation(
                    id,
                    1,
                    Ok(crate::repository::WorktreeOperationReport {
                        operation_id: id,
                        generation: 1,
                        request: Some(request.clone()),
                        state: WorktreeOperationState::Created,
                        outcome: Some(outcome.clone()),
                        detail: "Created".into(),
                        created_at_unix_ms: 1,
                    }),
                ));
                let active = app.active_workspace.clone();
                let count = app.sidebar_model.project(project).unwrap().worktrees.len();
                drop(app.update(Message::PrWorktreeCreated {
                    origin,
                    repository: request.repository,
                    outcome,
                }));
                assert_eq!(app.active_workspace, active);
                assert_eq!(
                    app.sidebar_model.project(project).unwrap().worktrees.len(),
                    count
                );
                assert!(
                    app.status
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("journal result is retained")
                );
            }
            if matches!(boundary, "submit" | "resolved") {
                assert!(app.worktree_operations.is_empty());
            }
        }
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
            setup_intent: None,
            repository: "/repo".into(),
            managed_root: "/".into(),
            managed_ignore_root: None,
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
    #[test]
    fn prepared_pr_checkout_keeps_its_head_branch_and_base_while_destination_is_editable() {
        let mut dialog = WorktreeDialog::new(
            ProjectId::new(),
            "/repo".into(),
            WorktreeDraft::branch("origin/feature"),
        );
        dialog.set_checkout_notice("Checks out feature tracking origin/feature.".into());
        dialog.edit(WorktreeField::Branch, "unrelated-local-branch".into());
        dialog.edit(WorktreeField::Base, "HEAD~20".into());
        dialog.edit(WorktreeField::Destination, "chosen-destination".into());
        assert_eq!(dialog.draft.branch, "origin/feature");
        assert!(dialog.draft.base.is_empty());
        assert_eq!(dialog.draft.destination, "chosen-destination");
    }
}
