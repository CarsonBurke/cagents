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


/// Captured parameters are cached with their navigation row, never reconstructed
/// from a CardId after the user activates an old menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub title: String,
    pub target: Arc<ConversationEndTarget>,
}

#[derive(Clone, Debug, Default)]
pub struct Rows(Arc<[Entry]>);
impl PartialEq for Rows {
    fn eq(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
}
impl Eq for Rows {}
impl Rows {
    pub(crate) fn update(&mut self, entries: Vec<Entry>) {
        if self.0.as_ref() != entries.as_slice() { self.0 = entries.into(); }
    }
    pub fn entries(&self) -> &[Entry] { &self.0 }
    pub fn identity(&self) -> usize { Arc::as_ptr(&self.0) as *const Entry as usize }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EndCommand(Arc<ConversationEndTarget>);
impl cosmic::widget::menu::Action for EndCommand {
    type Message = super::SidebarAction;
    fn message(&self) -> Self::Message {
        super::commands::registered(super::SidebarAction::EndConversation(Arc::clone(&self.0)))
    }
}

/// Native menus construct their tree only when opened. Bound each folder's
/// fanout, preserve durable order, and retain every conversation at any scale.
/// Total construction is linear in this one scope, not in the entire workspace.
const MENU_FANOUT: usize = 24;
fn entries(rows: &[Entry], offset: usize) -> Vec<cosmic::widget::menu::Item<EndCommand, String>> {
    use cosmic::widget::menu::Item;
    if rows.len() <= MENU_FANOUT {
        return rows.iter().enumerate().map(|(index, row)| Item::Button(format!("{}. {}", offset + index + 1, row.title), None, EndCommand(Arc::clone(&row.target)))).collect();
    }
    let mut chunk = MENU_FANOUT;
    while rows.len().div_ceil(chunk) > MENU_FANOUT { chunk = chunk.saturating_mul(MENU_FANOUT); }
    rows.chunks(chunk).enumerate().map(|(index, group)| {
        let start = offset + index * chunk;
        Item::Folder(format!("Conversations {}–{}", start + 1, start + group.len()), entries(group, start))
    }).collect()
}

pub(super) fn append_menu(
    menu: &mut Vec<cosmic::widget::menu::Tree<super::SidebarAction>>,
    rows: &Rows,
    shortcuts: &crate::settings::KeyboardShortcuts,
) {
    if rows.0.is_empty() { return; }
    use cosmic::widget::menu::{Item, items};
    // One registered template serves every captured parameter. Each action's
    // hint is derived from current settings; no per-conversation registry IDs.
    let bindings = super::menu_bindings(rows.0.iter().map(|row| EndCommand(Arc::clone(&row.target))), shortcuts, &[]);
    menu.extend(items(&bindings, vec![Item::Divider, Item::Folder(
        super::commands::label(crate::workspace_widget::GlobalShortcut::EndConversation).to_owned(),
        entries(&rows.0, 0),
    )]));
}


#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::widget::menu::Item;

    fn row(index: usize) -> Entry {
        let session = crate::chat::Session {
            id: crate::model::TerminalId::new(), proof: None, terminal: None, owned_codex_listener: None,
            native_continuity: Default::default(), launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex, session_id: Some(format!("conversation-{index}")),
                recipe: crate::model::LaunchRecipe { program: "codex".into(), arguments: Vec::new(),
                    working_directory: "/fixture".into(), environment: Default::default(), unset_environment: Default::default() },
            },
        };
        Entry { title: format!("Conversation {index}"), target: Arc::new(ConversationEndTarget {
            card: CardId::new(), registry: uuid::Uuid::new_v4(),
            issued: Arc::new(session.clone()), observed: Arc::new(session),
            channel: None, channel_project: None, worktree: None,
            shared_scope: None, shared_repository: None,
        }) }
    }
    fn collect(items: &[Item<EndCommand, String>], out: &mut Vec<CardId>) {
        assert!(items.len() <= MENU_FANOUT);
        for item in items {
            match item {
                Item::Folder(_, children) => collect(children, out),
                Item::Button(_, _, command) => {
                    let super::super::SidebarAction::RunCommand { command: template, action } = cosmic::widget::menu::Action::message(command) else { panic!("registered template"); };
                    assert_eq!(template, crate::workspace_widget::GlobalShortcut::EndConversation);
                    let super::super::SidebarAction::EndConversation(target) = *action else { panic!("source-bound parameter"); };
                    out.push(target.card());
                }
                _ => panic!("only bounded folders and captured conversation actions"),
            }
        }
    }
    #[test]
    fn sidebar_end_submenus_bound_fanout_without_dropping_or_retargeting_hidden_rows() {
        for count in [0, 1, 24, 25, 576, 577] {
            let rows: Vec<_> = (0..count).map(row).collect();
            let mut actual = Vec::new();
            collect(&entries(&rows, 0), &mut actual);
            assert_eq!(actual, rows.iter().map(|row| row.target.card()).collect::<Vec<_>>());
        }
    }
    #[test]
    fn sidebar_end_snapshot_identity_changes_only_for_label_or_source_changes() {
        let original = row(1);
        let mut rows = Rows::default(); rows.update(vec![original.clone()]);
        let captured = rows.clone();
        rows.update(vec![original.clone()]); assert_eq!(rows, captured);
        let mut renamed = original.clone(); renamed.title = "Renamed conversation".into();
        rows.update(vec![renamed]); assert_ne!(rows, captured);
        assert_eq!(captured.entries(), &[original]);
    }
}
