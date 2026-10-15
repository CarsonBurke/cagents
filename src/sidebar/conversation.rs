//! Immutable parameters for an existing conversation's sidebar command.
//! This UI token grants no daemon cleanup authority and is never serialized.
use crate::{chat, model::{CardId, ChannelId}, workspace_model::views::Scope};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationEndTarget {
    pub(crate) card: CardId,
    pub(crate) registry: uuid::Uuid,
    pub(crate) issued: Arc<chat::Session>,
    pub(crate) observed: Arc<chat::Session>,
    pub(crate) channel: Option<ChannelId>,
    pub(crate) worktree: Option<PathBuf>,
    pub(crate) shared_scope: Option<Scope>,
}
impl ConversationEndTarget {
    pub fn card(&self) -> CardId { self.card }
}
