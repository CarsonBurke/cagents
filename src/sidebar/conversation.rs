//! Immutable parameters for an existing conversation's sidebar command.
//! This UI token grants no daemon cleanup authority and is never serialized.
use crate::{chat, model::{CardId, ChannelId, ProjectId}, workspace_model::views::Scope};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, PartialEq, Eq)]
pub struct ConversationEndTarget {
    pub(crate) card: CardId,
    pub(crate) registry: uuid::Uuid,
    pub(crate) issued: Arc<chat::Session>,
    pub(crate) observed: Arc<chat::Session>,
    pub(crate) channel: Option<ChannelId>,
    pub(crate) channel_project: Option<ProjectId>,
    pub(crate) worktree: Option<PathBuf>,
    pub(crate) shared_scope: Option<Scope>,
    pub(crate) shared_repository: Option<PathBuf>,
}
impl ConversationEndTarget {
    pub fn card(&self) -> CardId { self.card }
}

impl std::fmt::Debug for ConversationEndTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Command diagnostics do not need argv/environment from either
        // captured recipe. Those values remain private validation inputs.
        formatter.debug_struct("ConversationEndTarget")
            .field("card", &self.card)
            .field("registry", &self.registry)
            .field("channel", &self.channel)
            .field("worktree", &self.worktree)
            .finish_non_exhaustive()
    }
}
