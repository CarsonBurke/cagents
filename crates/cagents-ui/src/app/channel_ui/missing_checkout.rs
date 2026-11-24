//! A channel whose worktree is gone from disk.
//!
//! A channel records the worktree it works in. When that worktree is removed
//! outside this window, or a strip whose worktree was already gone was moved
//! into a channel when the daemon started, the channel keeps its panes and
//! their conversations, and they start in the project's main checkout. The
//! window says so while the channel is shown, and offers the two ways out:
//! make the worktree again where it was, or delete the channel.
use super::*;
use crate::app::worktree_dialog::WorktreeDraft;
use crate::app::worktree_ui::listed_worktree;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Opens the new-worktree dialog on the channel's branch and folder.
    Recreate(ChannelId),
    /// Deletes the channel, closing its panes, once confirmed.
    Close(ChannelId),
}

/// What the notice needs about the missing worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct Missing {
    pub(in crate::app) channel: ChannelId,
    pub(in crate::app) project_id: ProjectId,
    pub(in crate::app) project: String,
    pub(in crate::app) repository: PathBuf,
    pub(in crate::app) path: PathBuf,
    /// The branch the worktree was last seen on, when this window listed it.
    /// A channel's name is no guide: it may be the folder's, or the user's.
    pub(in crate::app) branch: Option<String>,
    label: String,
}

impl Missing {
    /// What the worktree is called: its branch, else its folder's name.
    pub(in crate::app) fn label(&self) -> &str {
        &self.label
    }
}

fn label(branch: Option<&str>, path: &Path) -> String {
    branch.map_or_else(
        || {
            path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        },
        str::to_owned,
    )
}

impl Cagents {
    /// Records the branch of each worktree `project_id` lists, for the
    /// channel whose worktree is later removed.
    pub(in crate::app) fn remember_worktree_branches(&mut self, project_id: ProjectId) {
        let Some(project) = self.sidebar_model.project(project_id) else {
            return;
        };
        for worktree in &project.worktrees {
            if let Some(branch) = &worktree.branch {
                let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                self.channel_ui
                    .known_branches
                    .insert(worktree.path.clone(), branch.to_owned());
            }
        }
    }

    /// The worktree channel `id` records, when its project's worktrees have
    /// been listed and none of them is it. A project still loading, or one
    /// whose checkout cannot be read, says nothing yet.
    pub(in crate::app) fn missing_checkout(&self, id: ChannelId) -> Option<Missing> {
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)?;
        let path = channel.checkout.as_deref()?;
        let project = self.sidebar_model.project(channel.project_id)?;
        // A reload keeps the last listing until the new one lands, and that
        // listing still answers: the notice must not lapse for a refresh.
        if project.kind != ProjectKind::Git
            || project.worktrees.is_empty()
            || project.unreachable.is_some()
            || listed_worktree(project, &self.known_worktree_roots, path).is_some()
        {
            return None;
        }
        let branch = self.channel_ui.known_branches.get(path).cloned();
        Some(Missing {
            channel: id,
            project_id: project.id,
            project: project.name.clone(),
            repository: project.repository.clone(),
            path: path.to_path_buf(),
            label: label(branch.as_deref(), path),
            branch,
        })
    }

    /// What the source-control panel binds while worktree channel `id` is
    /// shown: its listed checkout, or nothing while that checkout is missing
    /// or not yet confirmed. `None` for a channel that names no worktree.
    pub(in crate::app) fn channel_checkout_focus(&self, id: ChannelId) -> Option<SidebarFocus> {
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)?;
        let checkout = channel.checkout.as_deref()?;
        let listed = self
            .sidebar_model
            .project(channel.project_id)
            .and_then(|project| listed_worktree(project, &self.known_worktree_roots, checkout));
        Some(
            listed.map_or(SidebarFocus::Removed, |worktree| SidebarFocus::Repository {
                worktree: worktree.to_path_buf(),
            }),
        )
    }

    /// [`Self::missing_checkout`] for the channel holding `card`.
    pub(in crate::app) fn card_missing_checkout(&self, card: CardId) -> Option<Missing> {
        self.durable
            .channels
            .iter()
            .filter(|channel| channel.checkout.is_some())
            .find(|channel| channel.cards.contains(&card))
            .and_then(|channel| self.missing_checkout(channel.id))
    }

    pub(in crate::app) fn update_missing_checkout(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Recreate(id) => {
                let Some(missing) = self.missing_checkout(id) else {
                    return Task::none();
                };
                // A folder deleted without Git still names its branch in
                // Git's records; one Git removed does not, and then the
                // dialog asks for it rather than guess.
                let branch = missing.branch.or_else(|| {
                    cagents::workspace_model::checkout_branch(&missing.repository, &missing.path)
                });
                self.open_worktree_dialog(
                    missing.project_id,
                    missing.repository,
                    WorktreeDraft {
                        branch: branch.unwrap_or_default(),
                        destination: missing.path.display().to_string(),
                        recreates: Some(missing.label),
                        ..WorktreeDraft::default()
                    },
                    None,
                )
            }
            Action::Close(id) => self.update(Message::Sidebar(SidebarAction::DeleteChannel {
                channel_id: id,
            })),
        }
    }

    /// The notice for the channel this window shows, when its worktree is
    /// missing.
    pub(in crate::app) fn missing_checkout_banner(&self) -> Element<'_, Message> {
        let Some(missing) = self
            .durable
            .active_channel
            .and_then(|id| self.missing_checkout(id))
        else {
            return iced::widget::Space::new().height(Length::Shrink).into();
        };
        let severity = NoticeSeverity::Warning;
        let enabled = !self.modal_is_open();
        let text = format!(
            "The worktree {} was removed. New terminals open in {}'s main checkout.",
            missing.label(),
            missing.project
        );
        let actions = vec![
            crate::interaction::tooltip(
                widget::button::text("Recreate worktree")
                    .id(widget::Id::new("missing-checkout.recreate"))
                    .name("Make the worktree again on its branch, where it was")
                    .class(notice_surface_button(severity))
                    .on_press_maybe(
                        enabled
                            .then_some(Message::MissingCheckout(Action::Recreate(missing.channel))),
                    ),
                missing.path.display().to_string(),
            ),
            widget::button::text("Delete channel")
                .id(widget::Id::new("missing-checkout.close"))
                .name("Delete this channel and close its panes")
                .class(notice_surface_button(severity))
                .on_press_maybe(
                    enabled.then_some(Message::MissingCheckout(Action::Close(missing.channel))),
                )
                .into(),
        ];
        notice_surface(
            severity,
            design::body(text).width(Length::Fill).into(),
            Some(
                widget::row::with_children(actions)
                    .spacing(space::XS)
                    .align_y(Alignment::Center)
                    .into(),
            ),
            // Its own padding rather than the bars' fixed row: beside the
            // panes it may be narrow enough to wrap.
            false,
        )
    }
}
