//! A channel whose worktree is gone from disk.
//!
//! A channel records the worktree it works in. When that worktree is removed
//! outside this window, or a strip whose worktree was already gone was moved
//! into a channel when the daemon started, the channel keeps its panes and
//! their conversations, and they start in the project's main checkout. The
//! window says so while the channel is shown, and offers the two ways out:
//! make the worktree again where it was, or close the channel.
use super::*;
use crate::app::worktree_dialog::WorktreeDraft;
use crate::app::worktree_ui::listed_worktree;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Opens the new-worktree dialog on the channel's branch and folder.
    Recreate(ChannelId),
    /// Closes the channel and its panes.
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
    /// The branch to check out again: the channel's name, which is the
    /// branch it was named after.
    pub(in crate::app) branch: String,
}

impl Cagents {
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
        if project.kind != ProjectKind::Git
            || project.worktrees.is_empty()
            || project.reloading
            || project.unreachable.is_some()
            || listed_worktree(project, &self.known_worktree_roots, path).is_some()
        {
            return None;
        }
        Some(Missing {
            channel: id,
            project_id: project.id,
            project: project.name.clone(),
            repository: project.repository.clone(),
            path: path.to_path_buf(),
            branch: channel.title.clone(),
        })
    }

    pub(in crate::app) fn update_missing_checkout(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Recreate(id) => {
                let Some(missing) = self.missing_checkout(id) else {
                    return Task::none();
                };
                self.open_worktree_dialog(
                    missing.project_id,
                    missing.repository,
                    WorktreeDraft {
                        branch: missing.branch,
                        destination: missing.path.display().to_string(),
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
        let folder = missing.path.file_name().map_or_else(
            || missing.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let text = format!(
            "The worktree {folder} is missing, so this channel's panes start in {}'s main \
             checkout.",
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
            widget::button::text("Close channel")
                .id(widget::Id::new("missing-checkout.close"))
                .name("Close this channel and its panes")
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
            true,
        )
    }
}
