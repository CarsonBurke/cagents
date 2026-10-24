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
            | Self::Finished { generation, .. } => *generation,
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

#[derive(Debug)]
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
}

#[derive(Debug)]
pub(super) struct State {
    generation: RemovalGeneration,
    operation: Option<Operation>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            generation: Uuid::new_v4(),
            operation: None,
        }
    }
}

impl State {
    pub(super) fn generation(&self) -> RemovalGeneration {
        self.generation
    }
    pub(super) fn can_open(&self) -> bool {
        !self.operation.as_ref().is_some_and(|operation| {
            matches!(operation.phase, Phase::Pending | Phase::Uncertain) || operation.recovering
        })
    }

    /// A pane replacement retires queued user input from its unsubmitted
    /// review. The accepted plan and choices remain available when reopened;
    /// pending requests and in-flight read/completion custody keep their IDs.
    pub(super) fn retire_review_input(&mut self) {
        if self
            .operation
            .as_ref()
            .is_some_and(|operation| operation.phase == Phase::Review)
        {
            self.rotate();
        }
    }

    pub(super) fn subject(&self) -> Option<&RemovalSubject> {
        self.operation.as_ref().map(|operation| &operation.subject)
    }

    fn rotate(&mut self) {
        self.generation = Uuid::new_v4();
    }

    /// `current` means the board still names this exact non-primary worktree,
    /// not merely a branch whose replacement happens to share its CardRef.
    pub(super) fn update(&mut self, action: Action, current: bool) -> Option<Effect> {
        if action.generation() != self.generation {
            return None;
        }
        if let Action::Open { subject, .. } = action {
            if !current
                || self.operation.as_ref().is_some_and(|op| {
                    matches!(op.phase, Phase::Pending | Phase::Uncertain) || op.recovering
                })
            {
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
            });
            return Some(Effect::Plan {
                generation: self.generation,
                subject,
                backend: None,
            });
        }
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
                    Ok(prepared) => {
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
            Action::Keep { keep, .. } if operation.phase == Phase::Review && current => {
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
            Action::DeleteBranch { delete, .. } if operation.phase == Phase::Review && current => {
                let plan = &operation.prepared.as_ref()?.plan;
                if !delete
                    || (plan.inspection.branch.is_some()
                        && matches!(plan.inspection.verdict, Verdict::Landed))
                {
                    operation.delete_branch = delete;
                    self.rotate();
                }
            }
            Action::Confirm { .. } if operation.phase == Phase::Review && current => {
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
                operation.phase = Phase::Pending;
                return Some(Effect::Remove {
                    generation: self.generation,
                    subject: operation.subject.clone(),
                    backend: prepared.backend.clone(),
                    request: Removal {
                        scope: Scope::Project(operation.subject.card.project.clone()),
                        item,
                        token,
                        keep,
                        delete_branch: operation.delete_branch,
                    },
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
                self.rotate();
            }
            Action::Reinspect { .. }
                if matches!(
                    operation.phase,
                    Phase::Review | Phase::Refused | Phase::Uncertain
                ) =>
            {
                let backend = operation
                    .prepared
                    .as_ref()
                    .map(|prepared| prepared.backend.clone());
                operation.recovering = operation.phase == Phase::Uncertain;
                operation.phase = Phase::Planning;
                operation.keep = None;
                operation.delete_branch = false;
                operation.message = None;
                let subject = operation.subject.clone();
                self.rotate();
                return Some(Effect::Plan {
                    generation: self.generation,
                    subject,
                    backend,
                });
            }
            Action::Dismiss { .. }
                if !matches!(operation.phase, Phase::Pending | Phase::Uncertain)
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
        let operation = self.operation.as_ref()?;
        let generation = self.generation;
        let mut content = widget::column::with_children(vec![
            design::heading(format!("Remove worktree · {}", operation.subject.label)).into(),
            design::caption(crate::app::display_path(&operation.subject.path)).into(),
        ])
        .spacing(space::XS);
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
            content = content.push(design::body(message.as_str()));
        }
        content = match operation.phase {
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
        let mut actions = widget::row::with_capacity(2).spacing(space::S);
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
        // Choices rotate the controller generation. Phase also changes on
        // submission without rotating it, so the original completion still
        // owns Pending. Pane replacement retires native input only. Watch
        // UUIDs are excluded; current eligibility may
        // retire controls without changing the controller or scroll position.
        Some(
            widget::container(
                widget::scrollable(crate::ui_controls::owned(
                    (pane, generation, operation.phase, current),
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

    #[test]
    fn opening_and_inspection_never_remove_and_old_a_cannot_replace_b() {
        let mut state = State::default();
        let a = subject("/work/a");
        let a_generation = open(&mut state, a.clone());
        let b = subject("/work/b");
        let b_generation = open(&mut state, b.clone());
        state.update(
            Action::Planned {
                generation: a_generation,
                result: Ok(prepared(&a, Verdict::Landed)),
            },
            true,
        );
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
