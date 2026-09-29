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
    if detail == typed.trim() || !detail.contains(char::is_whitespace) {
        format!("“{detail}” is not a valid {noun}.")
    } else {
        sentence(detail)
    }
}

fn sentence(message: &str) -> String {
    let message = message.trim();
    let mut chars = message.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut sentence: String = first.to_uppercase().chain(chars).collect();
    if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    sentence
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
    resolving: Option<Resolution>,
    error: Option<WorktreeInputError>,
}

impl WorktreeDialog {
    pub fn new(project_id: ProjectId, repository: PathBuf, draft: WorktreeDraft) -> Self {
        Self {
            project_id,
            repository,
            advanced: draft.uses_options(),
            draft,
            resolving: None,
            error: None,
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
                RepositoryService::system("git")
                    .resolve_worktree_request(&input, &token)
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
                self.worktree_dialog = None;
                let start = self.start_worktree_operation(project_id, request, Some(draft));
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
    /// beside it; `submit` runs it again straight away. The dialog now owns
    /// the failure, so the row that reported it goes.
    pub(super) fn reopen_worktree_operation(
        &mut self,
        id: OperationId,
        submit: bool,
    ) -> Task<Message> {
        if self.worktree_dialog.is_some() {
            return Task::none();
        }
        let Some((project_id, draft, detail)) =
            self.worktree_operations.get(&id).and_then(|operation| {
                operation.reopenable_draft().map(|draft| {
                    (
                        operation.project(),
                        draft.clone(),
                        operation.detail().to_owned(),
                    )
                })
            })
        else {
            return Task::none();
        };
        let Some(repository) = self
            .sidebar_model
            .project(project_id)
            .map(|project| project.repository.clone())
        else {
            return Task::none();
        };
        let _ = self.dismiss_worktree_operation(id);
        let error = (!submit).then(|| WorktreeInputError::general(detail));
        let focus = self.open_worktree_dialog(project_id, repository, draft, error);
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
        let spacing = cosmic::theme::spacing();
        let editable = dialog.resolving.is_none();
        let field_error = |field| {
            dialog
                .error
                .as_ref()
                .filter(|error| error.field == Some(field))
                .map(|error| error.message.as_str())
        };
        let input = |field: WorktreeField, placeholder, value, on_input: fn(String) -> Message| {
            let mut input = widget::text_input(placeholder, value)
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
                widget::text::title3(format!("New worktree in {project}")).into(),
                design::caption(
                    "Create a branch, check out an existing one, or start from a Linear issue ID or URL.",
                )
                .into(),
            ])
            .spacing(spacing.space_xxxs)
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
            widget::button::text(if dialog.advanced {
                "Hide options"
            } else {
                "Base branch & destination"
            })
            .leading_icon(crate::ui_controls::symbolic_icon(if dialog.advanced {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            }))
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
                    .size(design::TEXT_BODY)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill)
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            );
        }
        let status: Element<'_, Message> = if dialog.resolving.is_some() {
            widget::row::with_children(vec![
                crate::ui_controls::activity_spinner_tone(14.0, design::muted_icon_color),
                design::caption("Checking the branch and destination…").into(),
            ])
            .spacing(spacing.space_xxs)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into()
        } else {
            iced::widget::Space::new().width(Length::Fill).into()
        };
        rows.push(
            widget::row::with_children(vec![
                status,
                widget::button::standard("Cancel")
                    .id(widget::Id::new("dialog.worktree.cancel"))
                    .on_press(Message::CancelDialog)
                    .into(),
                widget::button::suggested(if dialog.resolving.is_some() {
                    "Creating…"
                } else {
                    "Create worktree"
                })
                .id(widget::Id::new("dialog.worktree.create"))
                .on_press_maybe(dialog.can_submit().then_some(Message::SubmitWorktree))
                .into(),
            ])
            .spacing(spacing.space_xxs)
            .align_y(Alignment::Center)
            .into(),
        );
        Some((
            dialog_ui::surface(
                widget::column::with_children(rows).spacing(3.0 * design::GRID_UNIT),
                520.0,
            ),
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
        .spacing(cosmic::theme::spacing().space_xxxs)
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

    /// Escape reaches the dialog even though its focused field captures the
    /// key to blur itself, and cancelling stops a running resolution.
    #[test]
    fn escape_from_a_focused_field_answers_the_dialog() {
        let Fixture { mut app, .. } = fixture();
        assert!(app.form_dialog_on_top());
        // Captured under a dialog mounted above the form, it is not the
        // form's to answer.
        app.settings_reset_confirm = true;
        assert!(!app.form_dialog_on_top());
        app.settings_reset_confirm = false;

        let _ = app.update(Message::WorktreeBranchChanged("feature/one".into()));
        let _ = app.update(Message::SubmitWorktree);
        let token = dialog(&app).resolving.as_ref().unwrap().token.clone();
        let _ = app.update(Message::CancelDialog);
        assert!(app.worktree_dialog.is_none());
        assert!(token.is_cancelled());
        assert!(!app.form_dialog_on_top());
    }

    /// A resolved request closes the dialog and becomes a creation row; if the
    /// creation then fails, the row reopens the same input (#499).
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
        let request = CreateWorktreeRequest {
            repository: "/repo".into(),
            managed_root: "/".into(),
            destination: "/one".into(),
            branch: crate::repository::ValidatedBranchName::parse("feature/one").unwrap(),
            base: crate::repository::BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: id,
        };
        let _ = app.update(Message::WorktreeInputResolved {
            project_id: project,
            operation_id: id,
            result: Ok(request),
        });
        assert!(app.worktree_dialog.is_none());
        assert!(app.worktree_operations[&id].reopenable_draft().is_none());

        let _ = app.finish_worktree_operation(id, 1, Err("git worktree add failed".into()));
        assert!(app.sidebar_model.projects[0].operations[0].reopenable);

        let _ = app.update(Message::Sidebar(SidebarAction::EditWorktreeOperation(id)));
        let dialog = dialog(&app);
        assert_eq!(dialog.draft, draft);
        assert!(dialog.advanced);
        assert!(dialog.resolving.is_none());
        assert_eq!(
            dialog.error.as_ref().map(|error| error.message.as_str()),
            Some("Git worktree add failed.")
        );
        assert!(!app.worktree_operations.contains_key(&id));
        assert!(app.sidebar_model.projects[0].operations.is_empty());
    }

    #[test]
    fn retrying_a_failed_creation_resubmits_its_input() {
        let Fixture {
            mut app, project, ..
        } = fixture();
        let _ = app.update(Message::CancelDialog);
        let id = OperationId::new();
        let request = CreateWorktreeRequest {
            repository: "/repo".into(),
            managed_root: "/".into(),
            destination: "/one".into(),
            branch: crate::repository::ValidatedBranchName::parse("feature/one").unwrap(),
            base: crate::repository::BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: id,
        };
        let _ = app.start_worktree_operation(
            project,
            request,
            Some(WorktreeDraft::branch("feature/one")),
        );
        let _ = app.finish_worktree_operation(id, 1, Err("network".into()));

        let _ = app.update(Message::Sidebar(SidebarAction::RetryWorktreeOperation(id)));
        let dialog = dialog(&app);
        assert_eq!(dialog.draft, WorktreeDraft::branch("feature/one"));
        assert!(dialog.error.is_none());
        assert!(dialog.resolving.is_some(), "retry submits straight away");
        assert!(!app.worktree_operations.contains_key(&id));
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
            "Main is already checked out at /repo; use that worktree or switch its branch first."
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
