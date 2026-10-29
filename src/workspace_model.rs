//! The half of the sidebar model that every window agrees on.
//!
//! The daemon owns projects, channels and their ordered pane descriptors.
//! Windows project the same live sessions while retaining independent focus,
//! viewport, runtime residency and unsaved drafts.
//!
//! [`ChannelId`] is reused rather than redefined here. `src/model.rs` is
//! declared ungated in `src/lib.rs`, so the CLI, the daemon and the UI already
//! resolve it in every build lane; moving the id types into this module or
//! re-exporting them would give one identity two spellings for no gain.

pub mod views;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{ChannelId, ProjectKind};

/// The rank a channel carries before the registry has answered.
///
/// A window mints a channel's id itself and shows it immediately, so the rank
/// the registry assigns cannot be waited for without blocking a redraw on the
/// daemon. Ranking such a channel above every ranked one is the honest guess:
/// it was created a moment ago, so it is the newest thing the user has, and
/// when the registry's answer arrives it is the highest real rank anyway.
pub const PENDING_ORDINAL: u32 = u32::MAX;

/// A project as every window agrees on it.
///
/// The canonical repository path is the key because a [`crate::model::ProjectId`]
/// is minted per window and re-keying one would cascade through the `worktrees`
/// foreign key. A path is the real identity of a project and needs no migration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProject {
    pub repository: PathBuf,
    pub name: String,
    #[serde(default)]
    pub kind: ProjectKind,
    /// Creation rank, assigned by the registry: strictly increasing, never
    /// reused.
    pub ordinal: u32,
}

/// Shared channel metadata. Ordered pane descriptors live in [`views::View`];
/// the active channel and focused pane remain local to each client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChannel {
    pub id: ChannelId,
    pub repository: PathBuf,
    pub title: String,
    #[serde(default)]
    pub title_source: crate::model::ChannelTitleSource,
    /// Creation rank, assigned by the registry: strictly increasing, never
    /// reused, and therefore stable across a window restart.
    ///
    /// The registry assigns it rather than the client so two windows creating a
    /// channel at the same moment cannot mint the same rank. The sidebar renders
    /// a project's channels by descending rank, so the newest sits on top.
    pub ordinal: u32,
}

mod conversation_titles;
pub use conversation_titles::{channel_title, channel_title_from_prompt};

/// The identity of a registry that has never been created.
///
/// A window compares the identity it recorded against the one the model carries
/// to decide whether it has ever shared anything with *this* registry. The nil
/// identity can only come from a [`WorkspaceModel::default`], never from a real
/// registry, and it matches no recorded identity - so it takes the safe branch,
/// where nothing local counts as shared and therefore nothing is retired.
pub const NO_REGISTRY: Uuid = Uuid::nil();

/// Stable identities in a project's manually arranged navigation list.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NavigationItem {
    Worktree(PathBuf),
    Channel(ChannelId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigationOrder {
    pub repository: PathBuf,
    pub items: Vec<NavigationItem>,
}

/// Complete a saved mixed order using the same placement as the sidebar:
/// newly discovered channels precede saved destinations; new worktrees follow.
/// Existing destinations retain their relative order before an explicit move.
pub fn merge_navigation_fallback(items: &mut Vec<NavigationItem>, fallback: &[NavigationItem]) {
    let mut known = std::collections::HashSet::new();
    items.retain(|item| known.insert(item.clone()));
    let mut channels = Vec::new();
    let mut worktrees = Vec::new();
    for item in fallback {
        if known.insert(item.clone()) {
            match item {
                NavigationItem::Channel(_) => channels.push(item.clone()),
                NavigationItem::Worktree(_) => worktrees.push(item.clone()),
            }
        }
    }
    items.splice(0..0, channels);
    items.extend(worktrees);
}

pub fn reorder_navigation(
    items: &mut Vec<NavigationItem>,
    source: &NavigationItem,
    before: Option<&NavigationItem>,
) -> bool {
    let Some(from) = items.iter().position(|item| item == source) else {
        return false;
    };
    let target = match before {
        Some(item) => match items.iter().position(|candidate| candidate == item) {
            Some(index) => index,
            None => return false,
        },
        None => items.len(),
    };
    let to = target.saturating_sub(usize::from(from < target));
    if from == to {
        return false;
    }
    let item = items.remove(from);
    items.insert(to, item);
    true
}

/// The shared sidebar model as the registry currently holds it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceModel {
    /// Minted when the registry's store is first created and durable from then
    /// on.
    ///
    /// A window records the identity it last reconciled against, which is the
    /// only way to tell "the registry I have been talking to" from a replaced
    /// daemon or a wiped data directory. Without it, an empty model is
    /// indistinguishable from a registry that deliberately holds nothing, and a
    /// window would retire channels - tombstoning the cards inside them - on the
    /// strength of silence that carries no information.
    pub identity: Uuid,
    /// Monotonic, bumped by every applied edit. An edit that changes nothing
    /// does not bump it, so a watcher can tell a real edge from a no-op.
    pub sequence: u64,
    #[serde(default)]
    pub navigation_orders: Vec<NavigationOrder>,
    pub projects: Vec<ModelProject>,
    pub channels: Vec<ModelChannel>,
    #[serde(default)]
    pub views: Vec<views::View>,
    /// Explicit removals survive a missed watch edge and a window restart.
    /// Reopening the repository clears its tombstone.
    #[serde(default)]
    pub closed_projects: Vec<PathBuf>,
    /// Durable execution sessions, adopted into their explicitly owned channel.
    #[serde(default)]
    pub agents: Vec<crate::agent_execution::Agent>,
}

impl WorkspaceModel {
    pub fn project(&self, repository: &Path) -> Option<&ModelProject> {
        self.projects
            .iter()
            .find(|project| project.repository == repository)
    }

    pub fn channel(&self, id: ChannelId) -> Option<&ModelChannel> {
        self.channels.iter().find(|channel| channel.id == id)
    }

    /// The view currently displaying this exact agent, independent of its
    /// execution repository and original placement. Old daemons transferred
    /// panes without updating managed placement records.
    ///
    /// Saved pane membership wins, including a move across project displays.
    /// Ambiguous, removed, or replaced panes must not be recreated at the old
    /// placement. An agent not yet published as a pane retains its launch target.
    pub fn agent_view_scope<'a>(
        &'a self,
        agent: &'a crate::agent_execution::Agent,
    ) -> Option<&'a views::Scope> {
        views::ManagedDisplayIndex::new(self).scope(agent)
    }

    /// Folds an observed edge into this cached copy.
    ///
    /// A watcher keeps the model rather than re-reading it for every edge, so an
    /// edge has to be applicable locally. A create's rank is not in the edit -
    /// the registry assigns it - so it lands as [`PENDING_ORDINAL`] until a read
    /// supplies the real one. That is the honest placeholder: the channel was
    /// created a moment ago, so it does outrank everything already ranked.
    pub fn fold(&mut self, sequence: u64, edit: &ModelEdit) {
        self.sequence = sequence;
        match edit {
            ModelEdit::AgentsChanged
            | ModelEdit::SeedView { .. }
            | ModelEdit::ChangeView { .. } => {}
            ModelEdit::OpenProject {
                repository,
                name,
                kind,
            }
            | ModelEdit::ReopenProject {
                repository,
                name,
                kind,
            } => {
                self.closed_projects.retain(|closed| closed != repository);
                match self
                    .projects
                    .iter_mut()
                    .find(|project| project.repository == *repository)
                {
                    Some(project) => project.name = name.clone(),
                    None => self.projects.push(ModelProject {
                        repository: repository.clone(),
                        name: name.clone(),
                        kind: *kind,
                        ordinal: PENDING_ORDINAL,
                    }),
                }
            }
            ModelEdit::CloseProject { repository } => {
                self.navigation_orders
                    .retain(|order| order.repository != *repository);
                if !self.closed_projects.contains(repository) {
                    self.closed_projects.push(repository.clone());
                }
                self.projects
                    .retain(|project| project.repository != *repository);
                self.channels
                    .retain(|channel| channel.repository != *repository);
            }
            ModelEdit::CreateChannel {
                id,
                repository,
                title,
                title_source,
            } => {
                if self.channel(*id).is_none() {
                    self.channels.push(ModelChannel {
                        title_source: *title_source,
                        id: *id,
                        repository: repository.clone(),
                        title: title.clone(),
                        ordinal: PENDING_ORDINAL,
                    });
                }
            }
            ModelEdit::ResetNavigation { repository } => {
                self.navigation_orders
                    .retain(|order| order.repository != *repository);
            }
            ModelEdit::ReorderNavigation {
                repository,
                source,
                before,
                fallback,
            } => {
                let index = self
                    .navigation_orders
                    .iter()
                    .position(|order| order.repository == *repository)
                    .unwrap_or_else(|| {
                        self.navigation_orders.push(NavigationOrder {
                            repository: repository.clone(),
                            items: fallback.clone(),
                        });
                        self.navigation_orders.len() - 1
                    });
                let items = &mut self.navigation_orders[index].items;
                merge_navigation_fallback(items, fallback);
                reorder_navigation(items, source, before.as_ref());
            }
            ModelEdit::RetitleChannel { id, title } => {
                if let Some(channel) = self.channels.iter_mut().find(|channel| channel.id == *id) {
                    channel.title = title.clone();
                    channel.title_source = crate::model::ChannelTitleSource::Manual;
                }
            }
            ModelEdit::SuggestChannelTitle { id, title, source } => {
                if let Some(channel) = self.channels.iter_mut().find(|channel| channel.id == *id)
                    && channel.title_source.accepts(&channel.title, *source)
                {
                    channel.title = title.clone();
                    channel.title_source = *source;
                }
            }
            ModelEdit::SuggestConversationTitle {
                id,
                card,
                expected_revision,
                title,
                source,
            } => {
                let current = self.views.iter().any(|view| {
                    view.scope == views::Scope::Channel(*id)
                        && view.revision == *expected_revision
                        && view.primary_chat() == Some(*card)
                });
                if current
                    && !title.is_empty()
                    && let Some(channel) =
                        self.channels.iter_mut().find(|channel| channel.id == *id)
                    && channel
                        .title_source
                        .accepts_refresh(&channel.title, *source)
                {
                    channel.title = title.clone();
                    channel.title_source = *source;
                }
            }
            // Requires canonical work and pane evidence; an optimistic client
            // cannot infer whether retirement was accepted.
            ModelEdit::RetireEmptyChannel { .. } | ModelEdit::ChannelWorkChanged { .. } => {}
            ModelEdit::DeleteChannel { id } => {
                self.channels.retain(|channel| channel.id != *id);
            }
        }
    }
}

/// One mutation of the shared model.
///
/// Every variant is idempotent, because the same edit reaches the registry from
/// a CLI invocation, from the window that made it and - after a reconnect - from
/// a window resynchronising against a registry that already has it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ModelEdit {
    /// Upsert by path. A repeat updates the name and keeps the kind and rank.
    OpenProject {
        repository: PathBuf,
        name: String,
        #[serde(default)]
        kind: ProjectKind,
    },
    /// A deliberate user open may clear an earlier explicit removal. Routine
    /// sync upserts cannot, because one may have been queued before the close.
    ReopenProject {
        repository: PathBuf,
        name: String,
        #[serde(default)]
        kind: ProjectKind,
    },
    /// Removes the project and every channel inside it. A project nobody has
    /// open cannot keep channels reachable.
    CloseProject { repository: PathBuf },
    /// Idempotent by id. The id is minted by the client, the rank by the
    /// registry.
    CreateChannel {
        id: ChannelId,
        repository: PathBuf,
        title: String,
        #[serde(default)]
        title_source: crate::model::ChannelTitleSource,
    },
    /// Return this project to automatic sidebar ordering.
    ResetNavigation { repository: PathBuf },
    /// Merge a stable-successor move against the current shared order.
    ReorderNavigation {
        repository: PathBuf,
        source: NavigationItem,
        before: Option<NavigationItem>,
        fallback: Vec<NavigationItem>,
    },
    /// A no-op on an unknown id: a retitle racing a delete must not resurrect
    /// what the delete removed.
    RetitleChannel { id: ChannelId, title: String },
    /// Conditional inference never overwrites a manual rename, including a
    /// rename committed by another window while this suggestion was queued.
    SuggestChannelTitle {
        id: ChannelId,
        title: String,
        source: crate::model::ChannelTitleSource,
    },
    /// A no-op on an unknown id, so a second delete of one channel is not an
    /// error for the second window to issue it.
    DeleteChannel { id: ChannelId },
    /// Daemon-only invalidation after a committed execution descriptor change.
    AgentsChanged,
    SeedView {
        scope: views::Scope,
        panes: Vec<views::Pane>,
        closed: Vec<crate::model::CardId>,
    },
    ChangeView {
        scope: views::Scope,
        expected_revision: u64,
        operation: Uuid,
        change: views::Change,
    },
    /// A refresh from the current primary conversation. The daemon checks the
    /// original view revision and primary card in the title-update transaction.
    SuggestConversationTitle {
        id: ChannelId,
        card: crate::model::CardId,
        expected_revision: u64,
        title: String,
        source: crate::model::ChannelTitleSource,
    },
    /// Daemon-only invalidation after a committed channel-work transaction.
    ChannelWorkChanged { home: crate::channel_work::Home },
    RetireEmptyChannel {
        home: crate::channel_work::Home,
        expected_view_revision: u64,
    },
}

impl ModelEdit {
    /// Resolves this edit's repository key, once, at the boundary.
    ///
    /// Every producer spells a path in its own terms - a symlinked checkout, a
    /// trailing slash, a `..` - and the key has to be the same string in all of
    /// them or one project becomes two.
    pub fn canonicalized(self) -> Result<Self, String> {
        Ok(match self {
            Self::OpenProject {
                repository,
                name,
                kind,
            } => Self::OpenProject {
                repository: canonical_repository(&repository)?,
                name,
                kind,
            },
            Self::ReopenProject {
                repository,
                name,
                kind,
            } => Self::ReopenProject {
                repository: canonical_repository(&repository)?,
                name,
                kind,
            },
            Self::ResetNavigation { repository } => Self::ResetNavigation {
                repository: canonical_repository(&repository)?,
            },
            Self::ReorderNavigation {
                repository,
                source,
                before,
                fallback,
            } => Self::ReorderNavigation {
                repository: canonical_repository(&repository)?,
                source,
                before,
                fallback,
            },
            Self::CloseProject { repository } => Self::CloseProject {
                repository: canonical_repository(&repository)?,
            },
            Self::CreateChannel {
                id,
                repository,
                title,
                title_source,
            } => Self::CreateChannel {
                id,
                repository: canonical_repository(&repository)?,
                title,
                title_source,
            },
            Self::SeedView {
                scope,
                panes,
                closed,
            } => Self::SeedView {
                scope: scope.canonicalized()?,
                panes,
                closed,
            },
            Self::ChangeView {
                scope,
                expected_revision,
                operation,
                mut change,
            } => {
                if let views::Change::Transfer { destination, .. } = &mut change {
                    *destination = destination.clone().canonicalized()?;
                }
                Self::ChangeView {
                    scope: scope.canonicalized()?,
                    expected_revision,
                    operation,
                    change,
                }
            }
            edit @ (Self::RetitleChannel { .. }
            | Self::SuggestChannelTitle { .. }
            | Self::SuggestConversationTitle { .. }
            | Self::DeleteChannel { .. }
            | Self::AgentsChanged
            | Self::ChannelWorkChanged { .. }
            | Self::RetireEmptyChannel { .. }) => edit,
        })
    }
}

/// Resolves `path` into the shared project key.
///
/// A relative path is rejected rather than resolved: the daemon's working
/// directory has nothing to do with the client's, so resolving one against the
/// other would invent a project somewhere nobody asked for.
pub fn canonical_repository(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "a repository key must be absolute, but `{}` is relative",
            path.display()
        ));
    }
    // A checkout that has been removed from disk cannot be resolved at all, and
    // still has to be closable and listable, so its own absolute path stands in
    // for itself rather than failing the edit that retires it.
    if crate::device::is_remote() {
        return Ok(path.to_path_buf());
    }
    Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// The name a repository key presents when no window has supplied one.
///
/// The key and the name deliberately come from different places. The key is the
/// common Git directory, because that is the one form every window and the CLI
/// resolve to identically; its own last component is `.git`, which names
/// nothing. The name is the checkout that directory belongs to, which is what
/// the sidebar shows and what a user typed to get here - the same derivation
/// `load_repository` uses for a project it opened itself.
///
/// A bare repository has no checkout, and its key ends in the repository's own
/// name rather than `.git`, so that component is the name. Taking the parent
/// there would name the directory the repository happens to sit in.
///
/// Either way this is only a fallback: `OpenProject` is an upsert that updates
/// the name, so a window opening the same repository replaces a CLI-supplied
/// name with the one it derived itself.
pub fn repository_display_name(repository: &Path) -> String {
    let component = repository.file_name();
    let checkout = if component.is_some_and(|name| name == ".git") {
        repository.parent().and_then(Path::file_name)
    } else {
        component
    };
    checkout
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repository.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChannelTitleSource;

    #[test]
    fn navigation_folding_keeps_missing_channels_above_the_saved_mixed_order() {
        let repository = PathBuf::from("/repo/.git");
        let worktree = NavigationItem::Worktree("/repo".into());
        let old = NavigationItem::Channel(ChannelId::new());
        let first = NavigationItem::Channel(ChannelId::new());
        let newest = NavigationItem::Channel(ChannelId::new());
        let added_worktree = NavigationItem::Worktree("/repo/.worktrees/new".into());
        let mut model = WorkspaceModel::default();
        model.navigation_orders.push(NavigationOrder {
            repository: repository.clone(),
            items: vec![worktree.clone(), old.clone()],
        });
        let visible = vec![
            newest.clone(),
            first.clone(),
            worktree.clone(),
            old.clone(),
            added_worktree,
        ];
        model.fold(
            1,
            &ModelEdit::ReorderNavigation {
                repository,
                source: worktree,
                before: Some(old),
                fallback: visible.clone(),
            },
        );
        assert_eq!(model.navigation_orders[0].items, visible);
    }

    #[test]
    fn conversation_title_proof_roundtrips_without_changing_existing_edit_prefix() {
        let old = ModelEdit::SuggestChannelTitle {
            id: ChannelId::new(),
            title: "old".into(),
            source: ChannelTitleSource::Provider,
        };
        assert_eq!(postcard::to_allocvec(&old).unwrap()[0], 7);
        let edit = ModelEdit::SuggestConversationTitle {
            id: ChannelId::new(),
            card: crate::model::CardId::new(),
            expected_revision: 23,
            title: "Bundle encryption".into(),
            source: ChannelTitleSource::Provider,
        };
        let bytes = postcard::to_allocvec(&edit).unwrap();
        assert_eq!(bytes[0], 12);
        assert_eq!(postcard::from_bytes::<ModelEdit>(&bytes).unwrap(), edit);
    }

    #[test]
    fn channel_work_edits_preserve_captured_home_and_retirement_revision() {
        let home = crate::channel_work::Home {
            registry: Uuid::new_v4(),
            channel: ChannelId::new(),
            creation_ordinal: 73,
        };
        for (edit, tag) in [
            (ModelEdit::ChannelWorkChanged { home }, 13),
            (
                ModelEdit::RetireEmptyChannel {
                    home,
                    expected_view_revision: u64::MAX,
                },
                14,
            ),
        ] {
            let canonical = edit.clone().canonicalized().unwrap();
            assert_eq!(canonical, edit);
            assert_eq!(canonical.clone().canonicalized().unwrap(), edit);
            let bytes = postcard::to_allocvec(&canonical).unwrap();
            assert_eq!(bytes[0], tag);
            assert_eq!(postcard::from_bytes::<ModelEdit>(&bytes).unwrap(), edit);
        }
    }

    #[test]
    fn agent_display_membership_survives_stale_records_without_changing_execution() {
        use crate::{
            agent::Provider,
            agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target},
            model::{AgentId, CardId, CardKind, LaunchRecipe, TerminalId},
            workspace_model::views::{Pane, Scope, View},
        };
        let original = Scope::Channel(ChannelId::new());
        let moved = Scope::Channel(ChannelId::new());
        let session = crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: Provider::Codex,
                session_id: None,
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: vec![],
                    working_directory: "/execution-repo".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("host"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("incarnation"),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let agent = Agent {
            id: AgentId::new(),
            name: Some("live lead".into()),
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Codex,
                account: None,
                target: Target {
                    repository: "/execution-repo".into(),
                    directory: "/execution-repo".into(),
                    placement: Some(original.clone()),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: original.clone(),
            session: Some(HostedSession::Chat(session.clone())),
            console: None,
            created_ms: 1,
            closed: false,
        };
        let id = CardId::from_uuid(agent.id.as_uuid());
        let pane = Pane {
            id,
            title: "Mailbox lead".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Chat(session),
            scope: Some("/execution-repo".into()),
        };
        let mut model = WorkspaceModel::default();
        assert_eq!(model.agent_view_scope(&agent), Some(&original));
        let mut source = View::empty(original.clone());
        source.retired.push(id);
        model.views.push(source);
        assert_eq!(
            model.agent_view_scope(&agent),
            None,
            "closed panes stay closed"
        );
        let mut destination = View::empty(moved.clone());
        destination.panes.push(pane.clone());
        model.views.push(destination);
        assert_eq!(model.agent_view_scope(&agent), Some(&moved));
        let restored: WorkspaceModel =
            serde_json::from_str(&serde_json::to_string(&model).unwrap()).unwrap();
        assert_eq!(
            restored.agent_view_scope(&agent),
            Some(&moved),
            "reconnect follows durable pane membership"
        );
        let CardKind::Chat(promoted) = &mut model.views[1].panes[0].kind else {
            unreachable!()
        };
        promoted.launch.session_id = Some("confirmed-provider-thread".into());
        assert_eq!(
            model.agent_view_scope(&agent),
            Some(&moved),
            "thread promotion does not replace the actor"
        );
        assert_eq!(agent.placement, original);
        assert_eq!(
            agent.spec.target.repository,
            std::path::Path::new("/execution-repo")
        );
        // A legacy inventory alias can own the exact same actor.
        model.views[1].panes[0].id = CardId::new();
        assert_eq!(
            model.agent_view_scope(&agent),
            None,
            "an inventory alias must not create a second canonical pane"
        );
        model.views[1].panes[0].id = id;
        let mut conflicting = View::empty(Scope::Worktree {
            repository: "/execution-repo".into(),
            path: "/execution-repo/tree".into(),
        });
        conflicting.panes.push(pane);
        model.views.push(conflicting);
        assert_eq!(
            model.agent_view_scope(&agent),
            None,
            "ambiguous legacy membership chooses no view"
        );
        model.views.pop();
        let CardKind::Chat(session) = &mut model.views[1].panes[0].kind else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("replacement");
        assert_eq!(
            model.agent_view_scope(&agent),
            None,
            "same session id cannot substitute a different host"
        );
        let HostedSession::Chat(source) = agent.session.as_ref().unwrap() else {
            unreachable!()
        };
        let proof = source.proof.clone().unwrap();
        let terminal = crate::model::TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: source.launch.recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Live {
                host_session_id: proof.host_session_id,
                boot_id: proof.boot_id,
                host_incarnation: proof.host_incarnation,
            },
        };
        let mut terminal_agent = agent.clone();
        terminal_agent.spec.surface = Surface::Terminal;
        terminal_agent.console = Some(terminal.clone());
        model.views[1].panes[0].kind = CardKind::Terminal(terminal);
        let worktree = Scope::Worktree {
            repository: "/execution-repo".into(),
            path: "/execution-repo/tree".into(),
        };
        model.views[1].scope = worktree.clone();
        assert_eq!(
            model.agent_view_scope(&terminal_agent),
            Some(&worktree),
            "terminal presentation follows its console pane, not its native chat source"
        );
    }

    #[test]
    fn prompt_names_skip_attached_context_and_preserve_word_boundaries() {
        assert_eq!(
            channel_title_from_prompt(
                "# AGENTS.md instructions\n<INSTRUCTIONS>\nDo everything idiomatically\n</INSTRUCTIONS>\n\n## Repair channel navigation\nMore details"
            ),
            Some("Repair channel navigation".into())
        );
        assert_eq!(
            channel_title_from_prompt("```rust\nlet context = 1;\n```\n- Fix history paging"),
            Some("Fix history paging".into())
        );
        assert_eq!(
            channel_title_from_prompt("\u{202e}\nMeaningful task"),
            Some("Meaningful task".into())
        );
        let title = channel_title_from_prompt(&"navigation ".repeat(20)).unwrap();
        assert!(title.chars().count() <= 80);
        assert!(title.ends_with("navigation…"));
        assert!(channel_title(&"界".repeat(100)).unwrap().chars().count() <= 80);
    }

    #[test]
    fn optimistic_title_suggestions_cannot_overwrite_manual_names() {
        let id = ChannelId::new();
        let mut model = WorkspaceModel::default();
        model.fold(
            1,
            &ModelEdit::CreateChannel {
                id,
                repository: "/repo".into(),
                title: String::new(),
                title_source: Default::default(),
            },
        );
        model.fold(
            2,
            &ModelEdit::SuggestChannelTitle {
                id,
                title: "Prompt fallback".into(),
                source: ChannelTitleSource::Prompt,
            },
        );
        model.fold(
            3,
            &ModelEdit::SuggestChannelTitle {
                id,
                title: "Provider name".into(),
                source: ChannelTitleSource::Provider,
            },
        );
        model.fold(
            4,
            &ModelEdit::RetitleChannel {
                id,
                title: "Provider name".into(),
            },
        );
        model.fold(
            5,
            &ModelEdit::SuggestChannelTitle {
                id,
                title: "Stale inferred name".into(),
                source: ChannelTitleSource::Provider,
            },
        );
        assert_eq!(model.channels[0].title, "Provider name");
        assert_eq!(model.channels[0].title_source, ChannelTitleSource::Manual);
    }
}
