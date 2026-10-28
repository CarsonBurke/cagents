//! Inline, generation-fenced worktree removal. The backend still owns the
//! content token and removal admission; this state owns the user's consent.
use std::path::PathBuf;

use cosmic::{Element, iced::Length, widget};
use uuid::Uuid;

use super::CardRef;
use crate::{
    design::{self, space},
    model::HostSessionProof,
    ui_controls::button,
    work::{Item, Keep, Removal, RemovalPlan, Removed, Scope},
    worktree_content::Verdict,
};

pub type RemovalGeneration = Uuid;

const MAX_UNRESOLVED: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendTarget {
    pub socket: PathBuf,
    pub proof: HostSessionProof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalSubject {
    pub card: CardRef,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub label: String,
}

impl RemovalSubject {
    pub(super) fn of(item: &Item) -> Option<Self> {
        let worktree = item.worktree.as_ref().filter(|tree| !tree.primary)?;
        Some(Self {
            card: CardRef::of(item),
            path: worktree.path.clone(),
            branch: item.branch.clone(),
            label: item.label(),
        })
    }

    pub(super) fn matches(&self, item: &Item) -> bool {
        self.card.names(item)
            && item.branch == self.branch
            && item
                .worktree
                .as_ref()
                .is_some_and(|tree| !tree.primary && tree.path == self.path)
    }
}

#[derive(Debug, Clone)]
pub struct PreparedPlan {
    pub backend: BackendTarget,
    pub plan: RemovalPlan,
}

#[derive(Debug, Clone)]
pub enum RemovalOutcome {
    Removed(Removed),
    /// A local failure before the removal request was invoked.
    Refused(String),
    /// The request may have reached the host. Never resend it automatically.
    Uncertain(String),
}

#[derive(Debug, Clone)]
pub enum Action {
    Open {
        generation: RemovalGeneration,
        subject: RemovalSubject,
    },
    Planned {
        generation: RemovalGeneration,
        result: Result<PreparedPlan, String>,
    },
    Keep {
        generation: RemovalGeneration,
        keep: Keep,
    },
    DeleteBranch {
        generation: RemovalGeneration,
        delete: bool,
    },
    Confirm {
        generation: RemovalGeneration,
    },
    Dismiss {
        generation: RemovalGeneration,
    },
    Reinspect {
        generation: RemovalGeneration,
    },
    Details {
        generation: RemovalGeneration,
        open: bool,
    },
    KeepUnresolved {
        generation: RemovalGeneration,
        owner: Uuid,
    },
    ReturnUnresolved {
        generation: RemovalGeneration,
        owner: Uuid,
        id: Uuid,
    },
    CloseUnresolved {
        generation: RemovalGeneration,
        owner: Uuid,
        id: Uuid,
    },
    Finished {
        generation: RemovalGeneration,
        result: RemovalOutcome,
    },
}

impl Action {
    pub(super) fn background(&self) -> bool {
        matches!(self, Self::Planned { .. } | Self::Finished { .. })
    }

    fn generation(&self) -> RemovalGeneration {
        match self {
            Self::Open { generation, .. }
            | Self::Planned { generation, .. }
            | Self::Keep { generation, .. }
            | Self::DeleteBranch { generation, .. }
            | Self::Confirm { generation }
            | Self::Dismiss { generation }
            | Self::Reinspect { generation }
            | Self::Details { generation, .. }
            | Self::Finished { generation, .. }
            | Self::KeepUnresolved { generation, .. }
            | Self::ReturnUnresolved { generation, .. }
            | Self::CloseUnresolved { generation, .. } => *generation,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Effect {
    Plan {
        generation: RemovalGeneration,
        subject: RemovalSubject,
        backend: Option<BackendTarget>,
    },
    Remove {
        generation: RemovalGeneration,
        subject: RemovalSubject,
        backend: BackendTarget,
        request: Removal,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Planning,
    Review,
    Pending,
    Refused,
    Uncertain,
    Removed,
}

#[derive(Debug, Clone)]
struct Submitted {
    subject: RemovalSubject,
    prepared: PreparedPlan,
    request: Removal,
    diagnostic: Option<String>,
}

#[derive(Debug, Clone)]
struct Operation {
    subject: RemovalSubject,
    prepared: Option<PreparedPlan>,
    phase: Phase,
    keep: Option<Keep>,
    delete_branch: bool,
    /// A reinspection must not turn an unresolved mutation into a dismissible
    /// ordinary planning failure.
    recovering: bool,
    message: Option<String>,
    removed: Option<Removed>,
    submitted: Option<Submitted>,
}

#[derive(Debug)]
struct Unresolved {
    id: Uuid,
    operation: Operation,
}

#[derive(Debug)]
pub(super) struct State {
    generation: RemovalGeneration,
    // One outstanding client read, independent of desired review changes.
    // Its completion does not prove a timed-out backend inspection stopped.
    inspection: Option<RemovalGeneration>,
    operation: Option<Operation>,
    unresolved: Vec<Unresolved>,
    viewing_unresolved: Option<Uuid>,
    // Separate input ownership retires new retained-list callbacks on pane
    // reset without invalidating outstanding original worker completions.
    retained_owner: Uuid,
    details_open: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            generation: Uuid::new_v4(),
            inspection: None,
            operation: None,
            unresolved: Vec::new(),
            viewing_unresolved: None,
            retained_owner: Uuid::new_v4(),
            details_open: false,
        }
    }
}

impl State {
    pub(super) fn generation(&self) -> RemovalGeneration {
        self.generation
    }
    pub(super) fn can_open(&self) -> bool {
        self.unresolved.len() < MAX_UNRESOLVED
            && self.viewing_unresolved.is_none()
            && !self.operation.as_ref().is_some_and(|operation| {
                matches!(operation.phase, Phase::Pending | Phase::Uncertain) || operation.recovering
            })
    }

    pub(super) fn can_open_subject(&self, subject: &RemovalSubject) -> bool {
        self.can_open()
            && !self.unresolved.iter().any(|saved| {
                saved.operation.subject.card == subject.card
                    || saved.operation.subject.path == subject.path
            })
    }

    /// A pane replacement retires queued user input from its unsubmitted
    /// review. The accepted plan and choices remain available when reopened;
    /// pending requests and in-flight read/completion custody keep their IDs.
    pub(super) fn retire_review_input(&mut self) {
        self.retained_owner = Uuid::new_v4();
        if self
            .operation
            .as_ref()
            .is_some_and(|operation| operation.phase == Phase::Review)
        {
            self.rotate();
        }
    }

    /// A deferred read has not started. Pane/watch
    /// replacement requires a fresh explicit inspection instead of replaying it.
    /// Already issued reads retain their original completion custody.
    pub(super) fn retire_waiting_inspection(&mut self) {
        self.refuse_waiting_inspection("The pending inspection was not started because this view changed. Inspect the original worktree again to continue.");
    }

    fn refuse_waiting_inspection(&mut self, reason: &str) {
        if let Some(operation) = self.operation.as_mut()
            && operation.phase == Phase::Planning
            && self.inspection != Some(self.generation)
        {
            operation.phase = if operation.recovering || self.viewing_unresolved.is_some() {
                Phase::Uncertain
            } else {
                Phase::Refused
            };
            operation.message = Some(reason.into());
            self.rotate();
        }
    }

    fn start_inspection(&mut self) -> Option<Effect> {
        if self.inspection.is_some() {
            return None;
        }
        let operation = self.operation.as_ref()?;
        if operation.phase != Phase::Planning {
            return None;
        }
        self.inspection = Some(self.generation);
        Some(Effect::Plan {
            generation: self.generation,
            subject: operation.subject.clone(),
            backend: operation
                .prepared
                .as_ref()
                .map(|prepared| prepared.backend.clone()),
        })
    }

    pub(super) fn subject(&self) -> Option<&RemovalSubject> {
        self.operation.as_ref().map(|operation| &operation.subject)
    }

    fn rotate(&mut self) {
        self.generation = Uuid::new_v4();
        self.retained_owner = Uuid::new_v4();
        self.details_open = false;
    }

    /// `current` means the board still names this exact non-primary worktree,
    /// not merely a branch whose replacement happens to share its CardRef.
    pub(super) fn update(&mut self, action: Action, current: bool) -> Option<Effect> {
        if let Action::Planned { generation, result } = &action {
            if self.inspection != Some(*generation) {
                return None;
            }
            // Release the actual call even when its desired review was replaced.
            // A duplicate old completion cannot release a successor's slot.
            self.inspection = None;
            if *generation != self.generation {
                if result.is_err() {
                    // A client timeout/error does not cancel the backend work.
                    // Do not automatically add a second inspection behind it.
                    self.refuse_waiting_inspection("The earlier inspection did not return a plan. Inspect this worktree again to continue.");
                    return None;
                }
                if !current
                    && self
                        .operation
                        .as_ref()
                        .is_some_and(|op| op.prepared.is_none())
                {
                    self.retire_waiting_inspection();
                }
                return self.start_inspection();
            }
        }
        if action.generation() != self.generation {
            return None;
        }
        match &action {
            Action::Details { open, .. } if self.operation.is_some() => {
                self.details_open = *open;
                return None;
            }
            Action::KeepUnresolved { owner, .. }
                if *owner == self.retained_owner
                    && self.viewing_unresolved.is_none()
                    && self.unresolved.len() < MAX_UNRESOLVED
                    && self
                        .operation
                        .as_ref()
                        .is_some_and(|op| op.phase == Phase::Uncertain) =>
            {
                self.unresolved.push(Unresolved {
                    id: Uuid::new_v4(),
                    operation: self.operation.take().expect("checked terminal unknown"),
                });
                self.rotate();
                return None;
            }
            Action::ReturnUnresolved { owner, id, .. }
                if *owner == self.retained_owner && self.operation.is_none() =>
            {
                let saved = self.unresolved.iter().find(|saved| saved.id == *id)?;
                let mut original = saved.operation.clone();
                if let Some(submitted) = &original.submitted {
                    original.subject = submitted.subject.clone();
                    original.prepared = Some(submitted.prepared.clone());
                    original.keep = Some(submitted.request.keep);
                    original.delete_branch = submitted.request.delete_branch;
                    original.message = submitted.diagnostic.clone();
                }
                original.phase = Phase::Uncertain;
                original.recovering = false;
                self.operation = Some(original);
                self.viewing_unresolved = Some(*id);
                self.rotate();
                return None;
            }
            Action::CloseUnresolved { owner, id, .. }
                if *owner == self.retained_owner
                    && self.viewing_unresolved == Some(*id)
                    && self
                        .operation
                        .as_ref()
                        .is_some_and(|op| op.phase == Phase::Uncertain) =>
            {
                self.operation = None;
                self.viewing_unresolved = None;
                self.rotate();
                return None;
            }
            _ => {}
        }
        if let Action::Open { subject, .. } = action {
            if !current || !self.can_open_subject(&subject) {
                return None;
            }
            self.rotate();
            self.operation = Some(Operation {
                subject: subject.clone(),
                prepared: None,
                phase: Phase::Planning,
                keep: None,
                delete_branch: false,
                recovering: false,
                message: None,
                removed: None,
                submitted: None,
            });
            return self.start_inspection();
        }
        let readonly = self.viewing_unresolved.is_some();
        let operation = self.operation.as_mut()?;
        match action {
            Action::Open { .. } => unreachable!(),
            Action::Planned { result, .. } if operation.phase == Phase::Planning => {
                let result = result.and_then(|prepared| {
                    let plan = &prepared.plan;
                    if plan.project != operation.subject.card.project
                        || plan.inspection.path != operation.subject.path
                        || plan.inspection.branch != operation.subject.branch
                        || !prepared.backend.socket.is_absolute()
                        || !prepared.backend.proof.is_complete()
                        || operation.prepared.as_ref().is_some_and(|previous| {
                            previous.backend != prepared.backend
                        })
                    {
                        return Err("The inspection did not match the original worktree and host. No new removal was sent.".into());
                    }
                    Ok(prepared)
                });
                match result {
                    Ok(prepared) if readonly => {
                        // This is an observation, never a receipt for the old
                        // mutation or fresh consent. The stored record is untouched.
                        operation.prepared = Some(prepared);
                        operation.phase = Phase::Uncertain;
                        operation.recovering = false;
                        operation.message = Some("Inspection completed. The original removal result remains unconfirmed.".into());
                    }
                    Ok(prepared) => {
                        if operation.recovering {
                            // A fresh plan is not a receipt for the original
                            // mutation. Preserve that request before a new review
                            // can be dismissed, replaced, or explicitly confirmed.
                            if self.unresolved.len() == MAX_UNRESOLVED {
                                operation.phase = Phase::Uncertain;
                                operation.recovering = false;
                                operation.message = Some("The original removal result remains unconfirmed. All unresolved slots are occupied; its record has not been discarded.".into());
                                self.rotate();
                                return None;
                            }
                            let Some(submitted) = operation.submitted.as_ref() else {
                                operation.phase = Phase::Uncertain;
                                operation.recovering = false;
                                operation.message = Some("The original removal result remains unconfirmed. No new removal was sent.".into());
                                self.rotate();
                                return None;
                            };
                            let mut original = operation.clone();
                            original.subject = submitted.subject.clone();
                            original.prepared = Some(submitted.prepared.clone());
                            original.keep = Some(submitted.request.keep);
                            original.delete_branch = submitted.request.delete_branch;
                            original.message = submitted.diagnostic.clone();
                            original.phase = Phase::Uncertain;
                            original.recovering = false;
                            self.unresolved.push(Unresolved {
                                id: Uuid::new_v4(),
                                operation: original,
                            });
                            operation.submitted = None;
                        }
                        operation.keep = match prepared.plan.inspection.verdict {
                            Verdict::Unsaved { .. } => prepared
                                .plan
                                .permits(Keep::Snapshot, false)
                                .is_ok()
                                .then_some(Keep::Snapshot),
                            _ => Some(Keep::Nothing),
                        };
                        operation.delete_branch = false;
                        operation.prepared = Some(prepared);
                        operation.phase = Phase::Review;
                        operation.recovering = false;
                        operation.message = None;
                    }
                    Err(error) => {
                        operation.phase = if operation.recovering {
                            Phase::Uncertain
                        } else {
                            Phase::Refused
                        };
                        operation.message = Some(error);
                    }
                }
                self.rotate();
            }
            Action::Keep { keep, .. }
                if !readonly && operation.phase == Phase::Review && current =>
            {
                if operation
                    .prepared
                    .as_ref()?
                    .plan
                    .permits(keep, operation.delete_branch)
                    .is_ok()
                {
                    operation.keep = Some(keep);
                    self.rotate();
                }
            }
            Action::DeleteBranch { delete, .. }
                if !readonly && operation.phase == Phase::Review && current =>
            {
                let plan = &operation.prepared.as_ref()?.plan;
                if !delete
                    || (plan.inspection.branch.is_some()
                        && matches!(plan.inspection.verdict, Verdict::Landed))
                {
                    operation.delete_branch = delete;
                    self.rotate();
                }
            }
            Action::Confirm { .. } if !readonly && operation.phase == Phase::Review && current => {
                let prepared = operation.prepared.as_ref()?;
                let keep = operation.keep?;
                prepared.plan.permits(keep, operation.delete_branch).ok()?;
                let token = prepared
                    .plan
                    .inspection
                    .token
                    .clone()
                    .filter(|token| !token.is_empty())?;
                // Absolute path is deliberate: branch CardRefs survive worktree
                // recreation and may otherwise name a different target later.
                let item = operation.subject.path.to_str()?.to_owned();
                if !operation.subject.path.is_absolute() {
                    return None;
                }
                let request = Removal {
                    scope: Scope::Project(operation.subject.card.project.clone()),
                    item,
                    token,
                    keep,
                    delete_branch: operation.delete_branch,
                };
                operation.submitted = Some(Submitted {
                    subject: operation.subject.clone(),
                    prepared: prepared.clone(),
                    request: request.clone(),
                    diagnostic: None,
                });
                operation.phase = Phase::Pending;
                return Some(Effect::Remove {
                    generation: self.generation,
                    subject: operation.subject.clone(),
                    backend: prepared.backend.clone(),
                    request,
                });
            }
            Action::Finished { result, .. } if operation.phase == Phase::Pending => {
                match result {
                    RemovalOutcome::Removed(removed)
                        if removed.project == operation.subject.card.project
                            && removed.path == operation.subject.path =>
                    {
                        operation.phase = Phase::Removed;
                        operation.removed = Some(removed);
                        operation.message = None;
                    }
                    RemovalOutcome::Removed(_) => {
                        operation.phase = Phase::Uncertain;
                        operation.message = Some("The host returned a different removal receipt. Inspect the original worktree before continuing.".into());
                    }
                    RemovalOutcome::Refused(error) => {
                        operation.phase = Phase::Refused;
                        operation.message = Some(error);
                    }
                    RemovalOutcome::Uncertain(error) => {
                        operation.phase = Phase::Uncertain;
                        operation.message = Some(error);
                    }
                }
                if operation.phase == Phase::Uncertain
                    && let Some(submitted) = &mut operation.submitted
                {
                    submitted.diagnostic = operation.message.clone();
                }
                self.rotate();
            }
            Action::Reinspect { .. }
                if matches!(
                    operation.phase,
                    Phase::Review | Phase::Refused | Phase::Uncertain
                ) =>
            {
                operation.recovering = operation.phase == Phase::Uncertain;
                operation.phase = Phase::Planning;
                operation.keep = None;
                operation.delete_branch = false;
                operation.message = None;
                self.rotate();
                return self.start_inspection();
            }
            Action::Dismiss { .. }
                if !readonly
                    && !matches!(operation.phase, Phase::Pending | Phase::Uncertain)
                    && !operation.recovering =>
            {
                self.operation = None;
                self.rotate();
            }
            _ => {}
        }
        None
    }

    pub(super) fn view(
        &self,
        current: bool,
        pane: super::ActivationLifetime,
    ) -> Option<Element<'_, Action>> {
        if self.operation.is_none() && self.unresolved.is_empty() {
            return None;
        }
        let generation = self.generation;
        let owner = self.retained_owner;
        let mut content = widget::column::with_capacity(4).spacing(space::XS);
        if let Some(operation) = &self.operation {
            content = content.push(self.operation_view(operation, current));
        }
        if !self.unresolved.is_empty() {
            content = content.push(design::heading(format!(
                "Unresolved removals · {} / {MAX_UNRESOLVED}",
                self.unresolved.len()
            )));
            content = content.push(design::caption("Kept for the current application lifetime. Original results are still unconfirmed; no removal is resent. The same card or path stays protected."));
            if self.unresolved.len() == MAX_UNRESOLVED {
                content = content.push(design::body("All 16 unresolved slots are occupied. Opening another removal review is disabled; browsing and inspecting retained originals remain available. No records are discarded automatically."));
            }
            for saved in &self.unresolved {
                let id = saved.id;
                let row = widget::column::with_children(vec![
                    design::body(bounded(&saved.operation.subject.label)).into(),
                    design::caption(bounded(&crate::app::display_path(
                        &saved.operation.subject.path,
                    )))
                    .into(),
                    button::secondary("Review original")
                        .on_press_maybe(self.operation.is_none().then_some(
                            Action::ReturnUnresolved {
                                generation,
                                owner,
                                id,
                            },
                        ))
                        .into(),
                ])
                .spacing(space::XS);
                content = content.push(crate::ui_controls::owned(
                    (pane, generation, owner, id),
                    row,
                ));
            }
            if self.operation.is_some() {
                content = content.push(design::caption(
                    "Close this review before opening another retained original.",
                ));
            }
        }
        // Preserve the outer scroll state while retiring native input owners.
        Some(
            widget::container(
                widget::scrollable(crate::ui_controls::owned(
                    (
                        pane,
                        generation,
                        self.operation.as_ref().map(|op| op.phase),
                        current,
                    ),
                    content,
                ))
                .height(Length::Shrink),
            )
            .max_height(360.0)
            .padding(space::M)
            .width(Length::Fill)
            .into(),
        )
    }

    fn operation_view<'a>(
        &'a self,
        operation: &'a Operation,
        current: bool,
    ) -> Element<'a, Action> {
        let generation = self.generation;
        let mut header = widget::column::with_children(vec![
            design::heading(format!(
                "Remove worktree · {}",
                bounded(&operation.subject.label)
            ))
            .into(),
            design::caption(bounded(&crate::app::display_path(&operation.subject.path))).into(),
        ])
        .spacing(space::XS);
        header = header.push(
            button::quiet(if self.details_open {
                "Hide full details"
            } else {
                "Show full details"
            })
            .on_press(Action::Details {
                generation,
                open: !self.details_open,
            }),
        );
        let mut details = widget::column::with_capacity(8).spacing(space::XS);
        if self.details_open {
            details = details
                .push(design::heading("Target path"))
                .push(selectable_detail(operation.subject.path.to_string_lossy()));
            if let Some(branch) = &operation.subject.branch {
                details = details
                    .push(design::heading("Branch"))
                    .push(selectable_detail(branch.as_str()));
            }
            if let Some(message) = &operation.message {
                details = details
                    .push(design::heading("Message"))
                    .push(selectable_detail(message.as_str()));
            }
            if let Some(original) = operation
                .submitted
                .as_ref()
                .and_then(|submitted| submitted.diagnostic.as_deref())
                .filter(|original| Some(*original) != operation.message.as_deref())
            {
                details = details
                    .push(design::heading("Original removal diagnostic"))
                    .push(selectable_detail(original));
            }
        }
        // Keep the disclosure's native state and focus stable when its panel
        // opens. The following controls have separate geometry ownership.
        header = header.push(details);
        let mut content = widget::column::with_capacity(8).spacing(space::XS);
        if self.viewing_unresolved.is_some() {
            content = content.push(design::body("Read-only original. Inspection cannot confirm or settle the original removal. Its full unresolved record remains kept for the current application lifetime."));
        }
        if let Some(prepared) = &operation.prepared
            && operation.phase != Phase::Removed
        {
            let plan = &prepared.plan;
            content = content.push(design::body(explanation(plan)));
            content = content.push(design::caption(format!(
                "{} staged · {} modified · {} untracked",
                plan.inspection.staged, plan.inspection.modified, plan.inspection.untracked
            )));
            if plan.inspection.ignored_total > 0 {
                content = content.push(design::heading(format!(
                    "{} ignored entries will be deleted. A snapshot does not keep them.",
                    plan.inspection.ignored_total
                )));
                for entry in plan.inspection.ignored.iter().take(6) {
                    content = content.push(design::caption(bounded(&entry.path)));
                }
                let shown = plan.inspection.ignored.len().min(6) as u32;
                if plan.inspection.ignored_total > shown {
                    content = content.push(design::caption(format!(
                        "… and {} more ignored entries",
                        plan.inspection.ignored_total - shown
                    )));
                }
            }
            for (label, paths) in [
                (
                    "Filtered files a snapshot cannot preserve",
                    &plan.inspection.filtered,
                ),
                (
                    "Nested repositories a snapshot cannot preserve",
                    &plan.inspection.nested,
                ),
            ] {
                if !paths.is_empty() {
                    content = content.push(design::heading(label));
                    for path in paths.iter().take(4) {
                        content = content.push(design::caption(bounded(path)));
                    }
                    if paths.len() > 4 {
                        content = content.push(design::caption("More paths are affected."));
                    }
                }
            }
            if operation.phase == Phase::Review {
                if matches!(plan.inspection.verdict, Verdict::Unsaved { .. }) {
                    // Stack the choices so the review also fits a narrow Work
                    // split. Discard always requires its own explicit choice.
                    let mut choices = widget::column::with_capacity(2).spacing(space::XS);
                    for (keep, label) in [
                        (Keep::Snapshot, "Save snapshot first"),
                        (Keep::Discard, "Discard unsaved work"),
                    ] {
                        let permitted = plan.permits(keep, operation.delete_branch).is_ok();
                        let label = if operation.keep == Some(keep) {
                            format!("✓ {label}")
                        } else {
                            label.to_owned()
                        };
                        choices = choices.push(button::secondary(label).on_press_maybe(
                            (current && permitted).then_some(Action::Keep { generation, keep }),
                        ));
                    }
                    content = content.push(choices);
                    content = content.push(design::caption("A snapshot preserves staged, working and non-ignored untracked changes. It is not a copy of the entire directory or worktree settings."));
                }
                if plan.inspection.branch.is_some() {
                    content = content.push(design::caption(
                        "The branch is kept unless you explicitly choose to delete it.",
                    ));
                }
                if plan.inspection.branch.is_some()
                    && matches!(plan.inspection.verdict, Verdict::Landed)
                {
                    let mut branch = crate::ui_controls::field::checkbox(operation.delete_branch)
                        .label("Also delete the landed branch");
                    if current {
                        branch = branch
                            .on_toggle(move |delete| Action::DeleteBranch { generation, delete });
                    }
                    content = content.push(branch);
                }
                if !current {
                    content = content.push(design::body("The board no longer identifies this exact worktree. Removal is disabled; inspect it again or close this review."));
                }
                let allowed = current
                    && operation
                        .keep
                        .is_some_and(|keep| plan.permits(keep, operation.delete_branch).is_ok())
                    && plan
                        .inspection
                        .token
                        .as_ref()
                        .is_some_and(|token| !token.is_empty())
                    && operation.subject.path.is_absolute()
                    && operation.subject.path.to_str().is_some();
                content = content.push(
                    button::destructive("Remove worktree")
                        .on_press_maybe(allowed.then_some(Action::Confirm { generation })),
                );
            }
        }
        if let Some(message) = &operation.message {
            content = content.push(design::body(bounded(message)));
        }
        content = match operation.phase {
            Phase::Planning if self.inspection != Some(generation) => content.push(design::body("Waiting for the earlier inspection before inspecting this worktree… Nothing new has been sent.")),
            Phase::Planning if operation.recovering => content.push(design::body("Inspecting the original worktree… The previous removal result is still unconfirmed.")),
            Phase::Planning => content.push(design::body("Inspecting the original worktree… Nothing is being removed.")),
            Phase::Pending => content.push(design::body("Removing this worktree… Closing the pane does not cancel the request.")),
            Phase::Uncertain => content.push(design::body("The removal result is not confirmed. It will not be sent again. Inspect this same worktree and host before making another decision.")),
            Phase::Removed => {
                let removed = operation.removed.as_ref().expect("removed phase has receipt");
                content = content.push(design::heading("Worktree removed"));
                if let Some(snapshot) = &removed.snapshot {
                    content = content.push(design::body(format!("Snapshot saved: {}", snapshot.refname)));
                    content = content.push(design::caption(format!("Restore changes in another worktree of this repository: git stash apply --index {}", snapshot.refname)));
                }
                if let Some(branch) = &removed.deleted_branch { content = content.push(design::caption(format!("Branch deleted: {branch}"))); }
                if let Some(error) = &removed.branch_error { content = content.push(design::body(format!("The worktree was removed, but the branch was kept: {error}"))); }
                content
            }
            Phase::Review | Phase::Refused => content,
        };
        let mut actions = widget::column::with_capacity(3).spacing(space::S);
        if operation.phase == Phase::Uncertain {
            if let Some(id) = self.viewing_unresolved {
                actions = actions.push(button::quiet("Back to unresolved removals").on_press(
                    Action::CloseUnresolved {
                        generation,
                        owner: self.retained_owner,
                        id,
                    },
                ));
            } else {
                actions = actions.push(
                    button::secondary("Keep unresolved; review other work").on_press_maybe(
                        (self.unresolved.len() < MAX_UNRESOLVED).then_some(
                            Action::KeepUnresolved {
                                generation,
                                owner: self.retained_owner,
                            },
                        ),
                    ),
                );
            }
        }
        if matches!(
            operation.phase,
            Phase::Review | Phase::Refused | Phase::Uncertain
        ) {
            actions = actions.push(
                button::secondary("Inspect again").on_press(Action::Reinspect { generation }),
            );
        }
        if !matches!(operation.phase, Phase::Pending | Phase::Uncertain) && !operation.recovering {
            actions = actions
                .push(button::quiet("Close review").on_press(Action::Dismiss { generation }));
        }
        content = content.push(actions);
        widget::column::with_children(vec![
            header.into(),
            // Disclosure moves these consent controls. Retire held native
            // input before mounting their new geometry, retaining the header.
            crate::ui_controls::owned(
                (generation, operation.phase, current, self.details_open),
                content,
            ),
        ])
        .spacing(space::XS)
        .into()
    }
}

fn explanation(plan: &RemovalPlan) -> String {
    // The backend's Landed summary predates ignored-file disclosure and says
    // "loses nothing". Keep the separate ignored-loss warning unambiguous.
    match plan.inspection.verdict {
        Verdict::Landed => format!(
            "Tracked work landed in {}. The branch can also be deleted if you choose.",
            plan.inspection
                .landed_in
                .as_deref()
                .unwrap_or("the default branch")
        ),
        _ => plan.summary(),
    }
}

fn bounded(text: &str) -> String {
    let mut chars = text.chars();
    let mut shown: String = chars.by_ref().take(180).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

fn selectable_detail<'a>(text: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Action> {
    widget::selectable_text(text)
        .size(design::TEXT_BODY)
        .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
        .width(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        work::ItemKey,
        worktree_content::{IgnoredEntry, Inspection},
    };

    fn subject(path: &str) -> RemovalSubject {
        RemovalSubject {
            card: CardRef {
                project: "/repo/.git".into(),
                key: ItemKey::Branch("feature".into()),
            },
            path: path.into(),
            branch: Some("feature".into()),
            label: "feature".into(),
        }
    }

    fn prepared(subject: &RemovalSubject, verdict: Verdict) -> PreparedPlan {
        PreparedPlan {
            backend: BackendTarget {
                socket: "/run/user/1000/test/control.sock".into(),
                proof: HostSessionProof {
                    host_session_id: HostSessionId::new("host"),
                    boot_id: BootId::new("boot"),
                    host_incarnation: HostIncarnation::new("incarnation"),
                },
            },
            plan: RemovalPlan {
                project: subject.card.project.clone(),
                project_name: "repo".into(),
                // This display label intentionally differs from both the
                // storage key and the absolute mutation operand.
                item: "feature".into(),
                merged_pull_request: None,
                inspection: Inspection {
                    path: subject.path.clone(),
                    branch: subject.branch.clone(),
                    head: Some("head".into()),
                    verdict,
                    landed_in: Some("origin/main".into()),
                    staged: 1,
                    modified: 2,
                    untracked: 3,
                    untracked_bytes: 4,
                    filtered: vec![],
                    nested: vec![],
                    ignored: vec![],
                    ignored_total: 0,
                    token: Some("reviewed-token".into()),
                },
            },
        }
    }

    fn open(state: &mut State, subject: RemovalSubject) -> RemovalGeneration {
        let effect = state.update(
            Action::Open {
                generation: state.generation(),
                subject,
            },
            true,
        );
        let Some(Effect::Plan {
            generation,
            backend: None,
            ..
        }) = effect
        else {
            panic!("read-only initial plan")
        };
        generation
    }

    fn review(state: &mut State, subject: RemovalSubject, verdict: Verdict) {
        let prepared = prepared(&subject, verdict);
        let generation = open(state, subject);
        assert!(
            state
                .update(
                    Action::Planned {
                        generation,
                        result: Ok(prepared)
                    },
                    true
                )
                .is_none()
        );
    }

    fn distinct_subject(number: usize) -> RemovalSubject {
        let mut value = subject(&format!("/work/{number}"));
        value.card.key = ItemKey::Worktree(value.path.clone());
        value.branch = Some(format!("branch-{number}"));
        value.label = format!("work-{number}");
        value
    }

    fn uncertain(state: &mut State, subject: RemovalSubject) -> (RemovalGeneration, Removal) {
        review(state, subject, Verdict::Landed);
        state.update(
            Action::DeleteBranch {
                generation: state.generation(),
                delete: true,
            },
            true,
        );
        let generation = state.generation();
        let Some(Effect::Remove { request, .. }) =
            state.update(Action::Confirm { generation }, true)
        else {
            panic!("one accepted request")
        };
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Uncertain("EOF after submission".into()),
            },
            false,
        );
        (generation, request)
    }

    fn keep_unresolved(state: &mut State) -> Uuid {
        assert!(
            state
                .update(
                    Action::KeepUnresolved {
                        generation: state.generation(),
                        owner: state.retained_owner
                    },
                    false
                )
                .is_none()
        );
        assert!(state.operation.is_none());
        state.unresolved.last().unwrap().id
    }

    fn return_unresolved(state: &mut State, id: Uuid) {
        assert!(
            state
                .update(
                    Action::ReturnUnresolved {
                        generation: state.generation(),
                        owner: state.retained_owner,
                        id
                    },
                    false
                )
                .is_none()
        );
        assert_eq!(state.viewing_unresolved, Some(id));
    }

    #[test]
    fn keeping_missing_unknown_preserves_submitted_consent_and_readonly_original_recovery() {
        let mut state = State::default();
        let mut original = distinct_subject(1);
        original.label = "🦀".repeat(500);
        let (submitted_generation, request) = uncertain(&mut state, original.clone());
        let submitted = state
            .operation
            .as_ref()
            .unwrap()
            .submitted
            .as_ref()
            .unwrap()
            .clone();
        let Some(Effect::Plan {
            generation,
            backend: Some(backend),
            subject,
        }) = state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            false,
        )
        else {
            panic!("exact original read")
        };
        assert_eq!(backend, submitted.prepared.backend);
        assert_eq!(subject, original);
        // Successful removal followed by a lost reply can leave no target for
        // planning. Absence is an error, never proof of success or permission.
        state.update(
            Action::Planned {
                generation,
                result: Err("original worktree is no longer registered".into()),
            },
            false,
        );
        assert_eq!(state.operation.as_ref().unwrap().keep, None);
        let id = keep_unresolved(&mut state);
        let saved = &state.unresolved[0].operation;
        let original_request = saved.submitted.as_ref().unwrap();
        assert_eq!(original_request.request, request);
        assert!(original_request.request.delete_branch);
        assert_eq!(
            original_request.prepared.plan.inspection.token.as_deref(),
            Some("reviewed-token")
        );
        assert_eq!(
            original_request.prepared.backend,
            submitted.prepared.backend
        );
        assert_eq!(original_request.subject, original);
        assert_eq!(
            original_request.diagnostic.as_deref(),
            Some("EOF after submission")
        );
        assert!(
            saved
                .message
                .as_deref()
                .unwrap()
                .contains("no longer registered")
        );
        assert_eq!(saved.phase, Phase::Uncertain);

        let other = distinct_subject(2);
        let pending_other = open(&mut state, other.clone());
        state.update(
            Action::Finished {
                generation: submitted_generation,
                result: RemovalOutcome::Uncertain("late A".into()),
            },
            true,
        );
        state.update(
            Action::Planned {
                generation,
                result: Ok(prepared(&original, Verdict::Landed)),
            },
            true,
        );
        assert_eq!(state.subject(), Some(&other));
        state.update(
            Action::Planned {
                generation: pending_other,
                result: Ok(prepared(&other, Verdict::Landed)),
            },
            true,
        );
        assert!(
            matches!(state.update(Action::Confirm { generation: state.generation() }, true), Some(Effect::Remove { subject, .. }) if subject == other)
        );
        assert_eq!(state.unresolved.len(), 1);
        // Returning A cannot interrupt B's pending request.
        let blocked_owner = state.retained_owner;
        state.update(
            Action::ReturnUnresolved {
                generation: state.generation(),
                owner: blocked_owner,
                id,
            },
            false,
        );
        assert_eq!(state.subject(), Some(&other));
        let pending = state.generation();
        state.update(
            Action::Finished {
                generation: pending,
                result: RemovalOutcome::Refused("before send".into()),
            },
            true,
        );
        state.update(
            Action::Dismiss {
                generation: state.generation(),
            },
            true,
        );
        return_unresolved(&mut state, id);
        assert_eq!(state.operation.as_ref().unwrap().keep, Some(request.keep));
        assert_eq!(
            state.operation.as_ref().unwrap().delete_branch,
            request.delete_branch
        );

        for replacement in [true, false] {
            let Some(Effect::Plan {
                generation,
                backend: Some(backend),
                subject,
            }) = state.update(
                Action::Reinspect {
                    generation: state.generation(),
                },
                false,
            )
            else {
                panic!("readonly original host")
            };
            assert_eq!(backend, submitted.prepared.backend);
            assert_eq!(subject, original);
            let mut observed = prepared(&original, Verdict::Landed);
            observed.plan.inspection.token = Some("new-observation-token".into());
            if replacement {
                observed.backend.proof.host_incarnation = HostIncarnation::new("replacement");
            }
            state.update(
                Action::Planned {
                    generation,
                    result: Ok(observed),
                },
                true,
            );
            assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Uncertain);
            assert!(
                state
                    .update(
                        Action::Confirm {
                            generation: state.generation()
                        },
                        true
                    )
                    .is_none()
            );
            assert_eq!(
                state.unresolved[0]
                    .operation
                    .submitted
                    .as_ref()
                    .unwrap()
                    .request,
                request
            );
        }
        assert!(!outline(&state).iter().any(|text| text == "Remove worktree"));
        assert!(
            outline(&state)
                .iter()
                .any(|text| text == &format!("Remove worktree · {}", bounded(&original.label)))
        );
        assert!(
            !outline(&state)
                .iter()
                .any(|text| text.contains(&original.label))
        );
        assert!(
            outline(&state)
                .iter()
                .any(|text| text.contains("Read-only original"))
        );
        state.update(
            Action::CloseUnresolved {
                generation: state.generation(),
                owner: state.retained_owner,
                id,
            },
            false,
        );
        assert!(state.operation.is_none());
        assert_eq!(state.unresolved.len(), 1);
        assert!(!state.can_open_subject(&original));
    }

    #[test]
    fn unresolved_capacity_fences_card_or_path_and_never_parks_inflight_work() {
        let mut state = State::default();
        let original = distinct_subject(0);
        review(&mut state, original.clone(), Verdict::Landed);
        let generation = state.generation();
        state.update(Action::Confirm { generation }, true);
        state.update(
            Action::KeepUnresolved {
                generation,
                owner: state.retained_owner,
            },
            false,
        );
        assert!(state.unresolved.is_empty());
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Pending);
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Uncertain("lost".into()),
            },
            false,
        );
        let Some(Effect::Plan { generation, .. }) = state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            false,
        ) else {
            panic!("recovery")
        };
        state.update(
            Action::KeepUnresolved {
                generation,
                owner: state.retained_owner,
            },
            false,
        );
        assert!(state.unresolved.is_empty());
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Planning);
        state.update(
            Action::Planned {
                generation,
                result: Err("missing".into()),
            },
            false,
        );
        let first_id = keep_unresolved(&mut state);
        let mut same_card = original.clone();
        same_card.path = "/work/recreated-elsewhere".into();
        same_card.branch = Some("renamed".into());
        let mut same_path = distinct_subject(99);
        same_path.path = original.path.clone();
        same_path.card.project = "/different/root/.git".into();
        for fenced in [same_card, same_path] {
            assert!(!state.can_open_subject(&fenced));
            assert!(
                state
                    .update(
                        Action::Open {
                            generation: state.generation(),
                            subject: fenced
                        },
                        true
                    )
                    .is_none()
            );
            assert!(state.operation.is_none());
        }
        for number in 1..MAX_UNRESOLVED {
            uncertain(&mut state, distinct_subject(number));
            keep_unresolved(&mut state);
        }
        assert_eq!(state.unresolved.len(), MAX_UNRESOLVED);
        assert_eq!(state.unresolved[0].id, first_id);
        assert!(!state.can_open());
        assert!(
            state
                .update(
                    Action::Open {
                        generation: state.generation(),
                        subject: distinct_subject(100)
                    },
                    true
                )
                .is_none()
        );
        assert!(
            outline(&state)
                .iter()
                .any(|text| text.contains("All 16 unresolved slots are occupied"))
        );
        return_unresolved(&mut state, first_id);
        state.update(
            Action::CloseUnresolved {
                generation: state.generation(),
                owner: state.retained_owner,
                id: first_id,
            },
            false,
        );
        assert_eq!(state.unresolved.len(), MAX_UNRESOLVED);
        assert_eq!(
            state.unresolved[0]
                .operation
                .submitted
                .as_ref()
                .unwrap()
                .diagnostic
                .as_deref(),
            Some("lost")
        );
    }

    #[test]
    fn opening_and_inspection_never_remove_and_old_a_cannot_replace_b() {
        let mut state = State::default();
        let a = subject("/work/a");
        let a_generation = open(&mut state, a.clone());
        let b = subject("/work/b");
        assert!(
            state
                .update(
                    Action::Open {
                        generation: state.generation(),
                        subject: b.clone(),
                    },
                    true
                )
                .is_none(),
            "B waits for A's real inspection to finish"
        );
        let b_generation = state.generation();
        assert!(matches!(state.update(
            Action::Planned {
                generation: a_generation,
                result: Ok(prepared(&a, Verdict::Landed)),
            },
            true,
        ), Some(Effect::Plan { generation, subject, backend: None }) if generation == b_generation && subject == b));
        assert_eq!(state.subject(), Some(&b));
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Planning);
        state.update(
            Action::Planned {
                generation: b_generation,
                result: Ok(prepared(&b, Verdict::Landed)),
            },
            true,
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Review);
    }

    #[test]
    fn inspection_churn_keeps_one_physical_flight_and_only_the_latest_desired_review() {
        let mut state = State::default();
        let first = distinct_subject(0);
        let first_token = open(&mut state, first.clone());
        let mut latest = first.clone();
        for number in 1..=64 {
            latest = distinct_subject(number);
            assert!(
                state
                    .update(
                        Action::Open {
                            generation: state.generation(),
                            subject: latest.clone(),
                        },
                        true
                    )
                    .is_none(),
                "no second physical inspection during churn"
            );
            assert_eq!(state.inspection, Some(first_token));
            assert_eq!(state.subject(), Some(&latest));
        }
        let desired = state.generation();
        assert!(
            outline(&state)
                .iter()
                .any(|text| text.contains("Waiting for the earlier inspection"))
        );
        assert!(
            state
                .update(
                    Action::Planned {
                        generation: desired,
                        result: Ok(prepared(&latest, Verdict::Landed)),
                    },
                    true
                )
                .is_none(),
            "an unissued desired token is not a physical completion"
        );
        assert_eq!(state.inspection, Some(first_token));
        let Some(Effect::Plan {
            generation,
            subject,
            backend: None,
        }) = state.update(
            Action::Planned {
                generation: first_token,
                result: Ok(prepared(&first, Verdict::Landed)),
            },
            true,
        )
        else {
            panic!("only the newest desired inspection starts")
        };
        assert_eq!(generation, desired);
        assert_eq!(subject, latest);
        for stale in [first_token, Uuid::new_v4()] {
            assert!(
                state
                    .update(
                        Action::Planned {
                            generation: stale,
                            result: Ok(prepared(&first, Verdict::Landed)),
                        },
                        true
                    )
                    .is_none()
            );
            assert_eq!(
                state.inspection,
                Some(desired),
                "old duplicates cannot release the successor"
            );
        }
        assert!(
            state
                .update(
                    Action::Planned {
                        generation: desired,
                        result: Ok(prepared(&latest, Verdict::Landed)),
                    },
                    true
                )
                .is_none(),
            "finishing inspection never sends Remove"
        );
        assert_eq!(state.inspection, None);
        assert_eq!(state.subject(), Some(&latest));
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Review);
        assert!(
            matches!(state.update(Action::Confirm { generation: state.generation() }, true),
            Some(Effect::Remove { subject, .. }) if subject == latest)
        );
    }

    #[test]
    fn dismissing_or_retiring_desired_inspection_never_replays_it_after_settlement() {
        // Client failure may precede backend settlement, so no automatic successor.
        let mut failed = State::default();
        let original = distinct_subject(0);
        let physical = open(&mut failed, original);
        let desired = distinct_subject(1);
        assert!(
            failed
                .update(
                    Action::Open {
                        generation: failed.generation(),
                        subject: desired.clone(),
                    },
                    true
                )
                .is_none()
        );
        assert!(
            failed
                .update(
                    Action::Planned {
                        generation: physical,
                        result: Err("client deadline expired".into()),
                    },
                    true
                )
                .is_none()
        );
        assert_eq!(failed.inspection, None);
        assert_eq!(failed.subject(), Some(&desired));
        assert_eq!(failed.operation.as_ref().unwrap().phase, Phase::Refused);
        assert!(matches!(failed.update(Action::Reinspect {
            generation: failed.generation(),
        }, true), Some(Effect::Plan { subject, .. }) if subject == desired));

        for retire in [false, true] {
            let mut state = State::default();
            let first = distinct_subject(0);
            let token = open(&mut state, first);
            let desired = distinct_subject(1);
            assert!(
                state
                    .update(
                        Action::Open {
                            generation: state.generation(),
                            subject: desired.clone(),
                        },
                        true
                    )
                    .is_none()
            );
            if retire {
                state.retire_waiting_inspection();
                assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Refused);
            } else {
                assert!(
                    state
                        .update(
                            Action::Dismiss {
                                generation: state.generation()
                            },
                            true
                        )
                        .is_none()
                );
                assert!(state.subject().is_none());
            }
            assert_eq!(state.inspection, Some(token));
            assert!(
                state
                    .update(
                        Action::Planned {
                            generation: token,
                            result: Err("finished".into())
                        },
                        true
                    )
                    .is_none()
            );
            assert_eq!(state.inspection, None);
            if retire {
                let Some(Effect::Plan {
                    generation,
                    subject,
                    backend: None,
                }) = state.update(
                    Action::Reinspect {
                        generation: state.generation(),
                    },
                    true,
                )
                else {
                    panic!("fresh explicit inspection remains available")
                };
                assert_eq!(subject, desired);
                assert!(
                    state
                        .update(
                            Action::Planned {
                                generation: token,
                                result: Err("duplicate".into())
                            },
                            true
                        )
                        .is_none()
                );
                assert_eq!(state.inspection, Some(generation));
                state.update(
                    Action::Planned {
                        generation,
                        result: Err("current failure".into()),
                    },
                    true,
                );
                assert_eq!(state.inspection, None);
                assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Refused);
            } else {
                assert!(
                    state.subject().is_none(),
                    "physical completion cannot reopen a dismissed review"
                );
            }
        }
    }

    #[test]
    fn deferred_original_inspection_keeps_backend_and_unknown_custody() {
        for retire in [false, true] {
            let mut state = State::default();
            let original = distinct_subject(0);
            uncertain(&mut state, original.clone());
            let id = keep_unresolved(&mut state);
            let submitted = state.unresolved[0].operation.submitted.clone().unwrap();
            let other = distinct_subject(1);
            let physical = open(&mut state, other.clone());
            state.update(
                Action::Dismiss {
                    generation: state.generation(),
                },
                true,
            );
            return_unresolved(&mut state, id);
            assert!(
                state
                    .update(
                        Action::Reinspect {
                            generation: state.generation()
                        },
                        false
                    )
                    .is_none()
            );
            let waiting = state.generation();
            assert_eq!(state.inspection, Some(physical));
            assert!(!state.can_open());
            if retire {
                state.retire_waiting_inspection();
                assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Uncertain);
                assert!(
                    state
                        .update(
                            Action::Planned {
                                generation: physical,
                                result: Err("settled".into())
                            },
                            false
                        )
                        .is_none()
                );
                assert_eq!(state.inspection, None);
            } else {
                let Some(Effect::Plan {
                    generation,
                    subject,
                    backend: Some(backend),
                }) = state.update(
                    Action::Planned {
                        generation: physical,
                        result: Ok(prepared(&other, Verdict::Landed)),
                    },
                    false,
                )
                else {
                    panic!("exact original-host read is allowed despite a missing board subject")
                };
                assert_eq!(generation, waiting);
                assert_eq!(subject, original);
                assert_eq!(backend, submitted.prepared.backend);
                let mut replacement = prepared(&original, Verdict::Landed);
                replacement.backend.proof.host_incarnation = HostIncarnation::new("replacement");
                assert!(
                    state
                        .update(
                            Action::Planned {
                                generation,
                                result: Ok(replacement)
                            },
                            false
                        )
                        .is_none()
                );
                assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Uncertain);
                assert_eq!(state.inspection, None);
            }
            assert!(
                state
                    .update(
                        Action::Confirm {
                            generation: state.generation()
                        },
                        true
                    )
                    .is_none()
            );
            assert_eq!(state.unresolved.len(), 1);
            let retained = state.unresolved[0].operation.submitted.as_ref().unwrap();
            assert_eq!(retained.subject, submitted.subject);
            assert_eq!(retained.prepared.backend, submitted.prepared.backend);
            assert_eq!(retained.request, submitted.request);
            assert_eq!(retained.diagnostic, submitted.diagnostic);
        }
    }

    #[test]
    fn final_request_uses_captured_path_token_and_choices_not_branch_alias() {
        let mut state = State::default();
        let original = subject("/work/original");
        review(&mut state, original.clone(), Verdict::Landed);
        let stale = state.generation();
        state.update(
            Action::DeleteBranch {
                generation: stale,
                delete: true,
            },
            true,
        );
        assert!(
            state
                .update(Action::Confirm { generation: stale }, true)
                .is_none()
        );
        let Some(Effect::Remove {
            request,
            subject,
            backend,
            ..
        }) = state.update(
            Action::Confirm {
                generation: state.generation(),
            },
            true,
        )
        else {
            panic!("explicit confirmation removes")
        };
        assert_eq!(subject, original);
        assert_eq!(request.item, "/work/original");
        assert_eq!(request.token, "reviewed-token");
        assert_eq!(request.keep, Keep::Nothing);
        assert!(request.delete_branch);
        assert_eq!(request.scope, Scope::Project("/repo/.git".into()));
        assert_eq!(
            backend,
            state
                .operation
                .as_ref()
                .unwrap()
                .prepared
                .as_ref()
                .unwrap()
                .backend
        );
        assert!(
            state
                .update(
                    Action::Confirm {
                        generation: state.generation()
                    },
                    true
                )
                .is_none(),
            "pending requests never resend"
        );
    }

    #[test]
    fn full_details_distinguish_long_targets_and_preserve_original_diagnostics() {
        let mut state = State::default();
        let prefix = format!("/work/{}", "common-segment/".repeat(15));
        let first = subject(&format!("{prefix}first"));
        let mut second = subject(&format!("{prefix}second"));
        let branch = format!("feature/{}distinct-branch", "long-segment/".repeat(15));
        second.branch = Some(branch.clone());
        assert_eq!(
            bounded(first.path.to_str().unwrap()),
            bounded(second.path.to_str().unwrap()),
            "the compact previews cannot distinguish these valid targets"
        );
        review(&mut state, first.clone(), Verdict::Landed);
        let old_generation = state.generation();
        assert!(
            state
                .update(
                    Action::Details {
                        generation: old_generation,
                        open: true,
                    },
                    true,
                )
                .is_none(),
            "disclosure never dispatches backend work"
        );
        assert!(
            outline(&state)
                .iter()
                .any(|text| text == first.path.to_str().unwrap())
        );
        assert_eq!(state.operation.as_ref().unwrap().keep, Some(Keep::Nothing));
        assert!(!state.operation.as_ref().unwrap().delete_branch);
        state.update(
            Action::Dismiss {
                generation: state.generation(),
            },
            true,
        );
        review(&mut state, second.clone(), Verdict::Landed);
        state.update(
            Action::Details {
                generation: old_generation,
                open: true,
            },
            true,
        );
        assert!(
            !state.details_open,
            "old disclosure cannot act on a new review"
        );
        state.update(
            Action::Details {
                generation: state.generation(),
                open: true,
            },
            true,
        );
        let texts = outline(&state);
        assert!(
            texts
                .iter()
                .any(|text| text == second.path.to_str().unwrap())
        );
        assert!(
            !texts
                .iter()
                .any(|text| text == first.path.to_str().unwrap())
        );
        assert!(texts.iter().any(|text| text == &branch));
        let Some(Effect::Remove {
            generation,
            request,
            ..
        }) = state.update(
            Action::Confirm {
                generation: state.generation(),
            },
            true,
        )
        else {
            panic!("only explicit confirmation dispatches the reviewed removal");
        };
        assert_eq!(request.item, second.path.to_str().unwrap());
        assert_eq!(request.keep, Keep::Nothing);
        assert!(!request.delete_branch);
        let diagnostic = format!(
            "{}Original host did not return a receipt; inspect this same worktree before continuing.",
            "Connection context. ".repeat(15),
        );
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Uncertain(diagnostic.clone()),
            },
            true,
        );
        state.update(
            Action::Details {
                generation: state.generation(),
                open: true,
            },
            true,
        );
        assert!(outline(&state).iter().any(|text| text == &diagnostic));
        let id = keep_unresolved(&mut state);
        return_unresolved(&mut state, id);
        state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            true,
        );
        state.update(
            Action::Planned {
                generation: state.generation(),
                result: Ok(prepared(&second, Verdict::Landed)),
            },
            true,
        );
        state.update(
            Action::Details {
                generation: state.generation(),
                open: true,
            },
            true,
        );
        let texts = outline(&state);
        assert!(
            texts
                .iter()
                .any(|text| text == second.path.to_str().unwrap())
        );
        assert!(
            texts
                .iter()
                .any(|text| text == "Original removal diagnostic")
        );
        assert!(texts.iter().any(|text| text == &diagnostic));
        assert!(!texts.iter().any(|text| text == "Remove worktree"));
        assert_eq!(
            state.unresolved[0]
                .operation
                .submitted
                .as_ref()
                .unwrap()
                .request,
            request,
        );
        assert_eq!(
            state.unresolved[0]
                .operation
                .submitted
                .as_ref()
                .unwrap()
                .diagnostic
                .as_deref(),
            Some(diagnostic.as_str()),
        );
    }

    #[test]
    fn missing_or_recreated_board_target_cannot_confirm() {
        let mut state = State::default();
        review(&mut state, subject("/work/a"), Verdict::Landed);
        assert!(
            state
                .update(
                    Action::Confirm {
                        generation: state.generation()
                    },
                    false
                )
                .is_none()
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Review);
        let generation = state.generation();
        state.update(Action::Reinspect { generation }, false);
        let mut replacement = prepared(&subject("/work/replacement"), Verdict::Landed);
        replacement.plan.item = "feature".into();
        state.update(
            Action::Planned {
                generation: state.generation(),
                result: Ok(replacement),
            },
            false,
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Refused);
        assert!(
            state
                .update(
                    Action::Confirm {
                        generation: state.generation()
                    },
                    true
                )
                .is_none()
        );
    }

    #[test]
    fn snapshot_is_preferred_but_beyond_snapshot_never_defaults_to_discard() {
        let mut state = State::default();
        review(
            &mut state,
            subject("/work/a"),
            Verdict::Unsaved {
                uncommitted: true,
                unreachable: false,
                beyond_snapshot: false,
            },
        );
        assert_eq!(state.operation.as_ref().unwrap().keep, Some(Keep::Snapshot));
        assert!(!state.operation.as_ref().unwrap().delete_branch);
        review(
            &mut state,
            subject("/work/b"),
            Verdict::Unsaved {
                uncommitted: true,
                unreachable: false,
                beyond_snapshot: true,
            },
        );
        assert_eq!(state.operation.as_ref().unwrap().keep, None);
        assert!(
            state
                .update(
                    Action::Confirm {
                        generation: state.generation()
                    },
                    true
                )
                .is_none()
        );
        state.update(
            Action::Keep {
                generation: state.generation(),
                keep: Keep::Snapshot,
            },
            true,
        );
        assert_eq!(state.operation.as_ref().unwrap().keep, None);
        state.update(
            Action::DeleteBranch {
                generation: state.generation(),
                delete: true,
            },
            true,
        );
        assert!(!state.operation.as_ref().unwrap().delete_branch);
        state.update(
            Action::Keep {
                generation: state.generation(),
                keep: Keep::Discard,
            },
            true,
        );
        let Some(Effect::Remove { request, .. }) = state.update(
            Action::Confirm {
                generation: state.generation(),
            },
            true,
        ) else {
            panic!("only explicit discard permits it")
        };
        assert_eq!(request.keep, Keep::Discard);
        assert!(!request.delete_branch);
    }

    #[test]
    fn fresh_review_keeps_original_unknown_custody_when_dismissed_replaced_or_confirmed() {
        for next in ["dismiss", "other", "confirm"] {
            let mut state = State::default();
            let original = distinct_subject(1);
            let (_, original_request) = uncertain(&mut state, original.clone());
            let submitted = state.operation.as_ref().unwrap().submitted.clone().unwrap();
            let Some(Effect::Plan {
                generation,
                backend: Some(backend),
                subject,
            }) = state.update(
                Action::Reinspect {
                    generation: state.generation(),
                },
                true,
            )
            else {
                panic!("explicit original-host inspection only");
            };
            assert_eq!(backend, submitted.prepared.backend);
            assert_eq!(subject, original);
            let mut fresh = prepared(&original, Verdict::Landed);
            fresh.plan.inspection.token = Some("fresh-consent-token".into());
            assert!(
                state
                    .update(
                        Action::Planned {
                            generation,
                            result: Ok(fresh)
                        },
                        true
                    )
                    .is_none()
            );
            // A new explicit decision remains possible. Inspection is not a
            // completion receipt for the earlier submitted mutation.
            assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Review);
            assert!(!state.operation.as_ref().unwrap().delete_branch);
            match next {
                "dismiss" => {
                    assert!(
                        state
                            .update(
                                Action::Dismiss {
                                    generation: state.generation()
                                },
                                true
                            )
                            .is_none()
                    );
                    assert!(state.operation.is_none());
                }
                "other" => {
                    assert!(matches!(
                        state.update(
                            Action::Open {
                                generation: state.generation(),
                                subject: distinct_subject(2),
                            },
                            true
                        ),
                        Some(Effect::Plan { .. })
                    ));
                }
                "confirm" => {
                    let Some(Effect::Remove { request, .. }) = state.update(
                        Action::Confirm {
                            generation: state.generation(),
                        },
                        true,
                    ) else {
                        panic!("fresh explicit confirmation is still allowed");
                    };
                    assert_eq!(request.token, "fresh-consent-token");
                    assert!(!request.delete_branch);
                    assert_ne!(request, original_request);
                }
                _ => unreachable!(),
            }
            let retained = state
                .unresolved
                .iter()
                .find(|saved| {
                    saved
                        .operation
                        .submitted
                        .as_ref()
                        .is_some_and(|old| old.request == original_request)
                })
                .unwrap_or_else(|| {
                    panic!("{next}: a fresh plan cannot erase original unknown custody")
                });
            assert_eq!(state.unresolved.len(), 1);
            assert_eq!(retained.operation.phase, Phase::Uncertain);
            let old = retained.operation.submitted.as_ref().unwrap();
            assert_eq!(old.subject, original);
            assert_eq!(old.request, original_request);
            assert_eq!(old.prepared.backend, submitted.prepared.backend);
            assert_eq!(
                old.prepared.plan.inspection.token,
                submitted.prepared.plan.inspection.token
            );
            assert_eq!(old.diagnostic.as_deref(), Some("EOF after submission"));
            assert!(
                outline(&state)
                    .iter()
                    .any(|text| text.contains("unconfirmed"))
            );
        }
    }

    #[test]
    fn fresh_recovery_retention_never_duplicates_or_discards_at_capacity() {
        let mut state = State::default();
        for number in 0..MAX_UNRESOLVED - 1 {
            uncertain(&mut state, distinct_subject(number));
            keep_unresolved(&mut state);
        }
        let original = distinct_subject(MAX_UNRESOLVED);
        uncertain(&mut state, original.clone());
        state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            true,
        );
        state.update(
            Action::Planned {
                generation: state.generation(),
                result: Ok(prepared(&original, Verdict::Landed)),
            },
            true,
        );
        assert_eq!(state.unresolved.len(), MAX_UNRESOLVED);
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Review);
        assert!(state.operation.as_ref().unwrap().submitted.is_none());
        let ids: Vec<_> = state.unresolved.iter().map(|saved| saved.id).collect();
        // Rechecking a fresh review must not archive the original twice.
        state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            true,
        );
        state.update(
            Action::Planned {
                generation: state.generation(),
                result: Ok(prepared(&original, Verdict::Landed)),
            },
            true,
        );
        assert_eq!(state.unresolved.len(), MAX_UNRESOLVED);
        let Some(Effect::Remove {
            generation,
            request,
            ..
        }) = state.update(
            Action::Confirm {
                generation: state.generation(),
            },
            true,
        )
        else {
            panic!("a new explicit request remains possible");
        };
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Uncertain("second request unknown".into()),
            },
            true,
        );
        state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            true,
        );
        state.update(
            Action::Planned {
                generation: state.generation(),
                result: Ok(prepared(&original, Verdict::Landed)),
            },
            true,
        );
        let operation = state.operation.as_ref().unwrap();
        assert_eq!(operation.phase, Phase::Uncertain);
        assert!(keep_unresolved_button_disabled(&state));
        assert!(
            outline(&state)
                .iter()
                .any(|text| { text.contains("Opening another removal review is disabled") })
        );
        assert_eq!(operation.submitted.as_ref().unwrap().request, request);
        assert_eq!(
            operation.submitted.as_ref().unwrap().diagnostic.as_deref(),
            Some("second request unknown")
        );
        assert_eq!(
            state
                .unresolved
                .iter()
                .map(|saved| saved.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert!(
            outline(&state)
                .iter()
                .any(|text| text.contains("has not been discarded"))
        );
        state.update(
            Action::Dismiss {
                generation: state.generation(),
            },
            true,
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Uncertain);
    }

    #[test]
    fn uncertain_reply_requires_exact_host_inspection_and_never_resends() {
        let mut state = State::default();
        let original = subject("/work/a");
        review(&mut state, original.clone(), Verdict::Landed);
        let generation = state.generation();
        state.update(Action::Confirm { generation }, true);
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Uncertain("connection closed".into()),
            },
            true,
        );
        let uncertain = state.generation();
        assert!(
            state
                .update(
                    Action::Dismiss {
                        generation: uncertain
                    },
                    true
                )
                .is_none()
        );
        assert!(
            state
                .update(
                    Action::Open {
                        generation: uncertain,
                        subject: subject("/work/b")
                    },
                    true
                )
                .is_none()
        );
        assert!(
            state
                .update(
                    Action::Confirm {
                        generation: uncertain
                    },
                    true
                )
                .is_none()
        );
        let expected_backend = state
            .operation
            .as_ref()
            .unwrap()
            .prepared
            .as_ref()
            .unwrap()
            .backend
            .clone();
        let Some(Effect::Plan {
            generation,
            backend: Some(backend),
            subject,
            ..
        }) = state.update(
            Action::Reinspect {
                generation: uncertain,
            },
            true,
        )
        else {
            panic!("explicit read-only inspection")
        };
        assert_eq!(backend, expected_backend);
        assert_eq!(subject, original);
        state.update(
            Action::Planned {
                generation,
                result: Err("not reachable".into()),
            },
            true,
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Uncertain);
        state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            true,
        );
        let mut wrong_host = prepared(&original, Verdict::Landed);
        wrong_host.backend.proof.host_incarnation = HostIncarnation::new("replacement");
        state.update(
            Action::Planned {
                generation: state.generation(),
                result: Ok(wrong_host),
            },
            true,
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Uncertain);
        state.update(
            Action::Reinspect {
                generation: state.generation(),
            },
            true,
        );
        state.update(
            Action::Planned {
                generation: state.generation(),
                result: Ok(prepared(&original, Verdict::Landed)),
            },
            true,
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Review);
        assert!(!state.operation.as_ref().unwrap().delete_branch);
    }

    #[test]
    fn recovering_inspection_does_not_claim_previous_removal_stopped() {
        let mut state = State::default();
        review(&mut state, subject("/work/a"), Verdict::Landed);
        let generation = state.generation();
        assert!(matches!(
            state.update(Action::Confirm { generation }, true),
            Some(Effect::Remove { .. })
        ));
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Uncertain("reply timed out".into()),
            },
            true,
        );
        assert!(matches!(
            state.update(
                Action::Reinspect {
                    generation: state.generation()
                },
                true
            ),
            Some(Effect::Plan {
                backend: Some(_),
                ..
            })
        ));
        let texts = outline(&state);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("The previous removal result is still unconfirmed."))
        );
        assert!(
            !texts
                .iter()
                .any(|text| text.contains("Nothing is being removed."))
        );
    }

    #[test]
    fn successful_removal_with_branch_error_is_a_receipt_not_a_retry() {
        let mut state = State::default();
        let original = subject("/work/a");
        review(&mut state, original.clone(), Verdict::Landed);
        let generation = state.generation();
        state.update(Action::Confirm { generation }, true);
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Removed(Removed {
                    project: original.card.project,
                    path: original.path,
                    snapshot: None,
                    deleted_branch: None,
                    branch_error: Some("branch moved".into()),
                }),
            },
            false,
        );
        assert_eq!(state.operation.as_ref().unwrap().phase, Phase::Removed);
        let texts = outline(&state);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("worktree was removed, but the branch was kept"))
        );
        assert!(!texts.iter().any(|text| text == "Remove worktree"));
        assert!(
            state
                .update(
                    Action::Confirm {
                        generation: state.generation()
                    },
                    true
                )
                .is_none()
        );
        assert!(
            state
                .update(
                    Action::Reinspect {
                        generation: state.generation()
                    },
                    true
                )
                .is_none()
        );
    }

    #[test]
    fn snapshot_receipt_retains_a_restore_path_after_worktree_disappears() {
        let mut state = State::default();
        let original = subject("/work/a");
        review(
            &mut state,
            original.clone(),
            Verdict::Unsaved {
                uncommitted: true,
                unreachable: false,
                beyond_snapshot: false,
            },
        );
        let generation = state.generation();
        let Some(Effect::Remove { request, .. }) =
            state.update(Action::Confirm { generation }, true)
        else {
            panic!("explicit snapshot confirmation");
        };
        assert_eq!(request.keep, Keep::Snapshot);
        state.update(
            Action::Finished {
                generation,
                result: RemovalOutcome::Removed(Removed {
                    project: original.card.project,
                    path: original.path,
                    snapshot: Some(crate::worktree_content::Snapshot {
                        refname: "refs/cagents/snapshots/saved".into(),
                        oid: "snapshot".into(),
                    }),
                    deleted_branch: None,
                    branch_error: None,
                }),
            },
            false,
        );
        let texts = outline(&state);
        assert!(
            texts
                .iter()
                .any(|text| text == "Snapshot saved: refs/cagents/snapshots/saved")
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("git stash apply --index refs/cagents/snapshots/saved"))
        );
        assert!(!texts.iter().any(|text| text == "Remove worktree"));
    }

    #[derive(Default)]
    struct Outline(Vec<String>);
    impl cosmic::iced::advanced::widget::Operation for Outline {
        fn traverse(
            &mut self,
            visit: &mut dyn FnMut(&mut dyn cosmic::iced::advanced::widget::Operation),
        ) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: cosmic::iced::Rectangle, text: &str) {
            self.0.push(text.into());
        }
    }
    fn keep_unresolved_button_disabled(state: &State) -> bool {
        use cosmic::iced::{
            Size,
            advanced::{Layout, layout, widget::Tree},
            mouse,
        };
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = state.view(true, Default::default()).expect("review shown");
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(760.0, 700.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let buttons: Vec<_> = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some("Keep unresolved; review other work")
            })
            .collect();
        assert_eq!(buttons.len(), 1, "one actual keep-unresolved control");
        buttons[0].node().is_disabled()
    }

    fn outline(state: &State) -> Vec<String> {
        use cosmic::iced::{
            Size,
            advanced::{Layout, layout, widget::Tree},
        };
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = state.view(true, Default::default()).expect("review shown");
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(760.0, 700.0)),
        );
        let mut outline = Outline::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut outline);
        outline.0
    }

    #[test]
    fn landed_review_draws_bounded_ignored_loss_independently_of_snapshot() {
        let mut state = State::default();
        let original = subject("/work/a");
        let generation = open(&mut state, original.clone());
        let mut prepared = prepared(&original, Verdict::Landed);
        prepared.plan.inspection.ignored_total = 400;
        prepared.plan.inspection.ignored = (0..20)
            .map(|i| IgnoredEntry {
                path: format!("cache/{i}"),
                directory: true,
            })
            .collect();
        state.update(
            Action::Planned {
                generation,
                result: Ok(prepared),
            },
            true,
        );
        let texts = outline(&state);
        assert!(
            texts.iter().any(|text| text
                == "400 ignored entries will be deleted. A snapshot does not keep them.")
        );
        assert!(
            texts
                .iter()
                .any(|text| text == "… and 394 more ignored entries")
        );
        assert!(texts.iter().any(|text| text == "cache/5"));
        assert!(!texts.iter().any(|text| text == "cache/6"));
        assert!(!texts.iter().any(|text| text.contains("loses nothing")));
        assert!(texts.iter().any(|text| text == "Remove worktree"));
    }
}
