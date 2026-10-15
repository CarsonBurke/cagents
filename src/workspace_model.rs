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
    /// Verified checkout used to discover this project; identity stays repository.
    #[serde(default)]
    pub checkout: Option<PathBuf>,
    /// Verified primary checkout, retained when a linked checkout becomes current.
    #[serde(default)]
    pub primary_checkout: Option<PathBuf>,
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

/// A prompt fallback labels its first substantive line, rather than flattening
/// attached context into the sidebar. Provider titles use `channel_title`.
pub fn channel_title_from_prompt(prompt: &str) -> Option<String> {
    let mut fence = false;
    let mut context = false;
    prompt.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            fence = !fence;
            return None;
        }
        if matches!(
            line,
            "<INSTRUCTIONS>" | "<environment_context>" | "<system-reminder>"
        ) {
            context = true;
            return None;
        }
        if matches!(
            line,
            "</INSTRUCTIONS>" | "</environment_context>" | "</system-reminder>"
        ) {
            context = false;
            return None;
        }
        if fence || context || line.starts_with("# AGENTS.md") || line.starts_with("<image ") {
            return None;
        }
        let line = line.trim_start_matches(['#', '*', '-', '>', ' ']);
        channel_title(line)
    })
}

pub fn channel_title(text: &str) -> Option<String> {
    let title = crate::names::normalize_card_name(text)?;
    if title.chars().count() <= 80 {
        return Some(title);
    }
    let shortened: String = title.chars().take(79).collect();
    let end = shortened
        .rfind(' ')
        .filter(|end| *end > shortened.len() / 2);
    Some(format!(
        "{}…",
        end.map_or(shortened.as_str(), |end| &shortened[..end])
            .trim_end()
    ))
}

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
                checkout,
                primary_checkout,
                name,
                kind,
            }
            | ModelEdit::ReopenProject {
                repository,
                checkout,
                primary_checkout,
                name,
                kind,
            } => {
                self.closed_projects.retain(|closed| closed != repository);
                match self
                    .projects
                    .iter_mut()
                    .find(|project| project.repository == *repository)
                {
                    Some(project) => {
                        project.name = name.clone();
                        if let Some(checkout) = checkout { project.checkout = Some(checkout.clone()); }
                        if let Some(primary) = primary_checkout { project.primary_checkout = Some(primary.clone()); }
                    },
                    None => self.projects.push(ModelProject {
                        checkout: checkout.clone(), primary_checkout: primary_checkout.clone(),
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
                for item in fallback {
                    if !items.contains(item) {
                        items.push(item.clone());
                    }
                }
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
        #[serde(default)]
        checkout: Option<PathBuf>,
        #[serde(default)]
        primary_checkout: Option<PathBuf>,
        name: String,
        #[serde(default)]
        kind: ProjectKind,
    },
    /// A deliberate user open may clear an earlier explicit removal. Routine
    /// sync upserts cannot, because one may have been queued before the close.
    ReopenProject {
        repository: PathBuf,
        #[serde(default)]
        checkout: Option<PathBuf>,
        #[serde(default)]
        primary_checkout: Option<PathBuf>,
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
}

fn verified_primary_checkout(repository: &Path, primary: Option<PathBuf>, checkout: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let repository = canonical_repository(repository)?;
    // Retained discovery metadata may become stale after a checkout is removed,
    // unmounted, or reused. An unavailable primary must not block a valid linked
    // open; absent updates preserve the registry's prior discovery information.
    Ok(primary.as_deref().and_then(|root| crate::project_location::primary_checkout(root, &repository))
        .or_else(|| checkout.and_then(|root| crate::project_location::primary_checkout(root, &repository))))
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
                checkout,
                primary_checkout,
                name,
                kind,
            } => Self::OpenProject {
                repository: canonical_repository(&repository)?,
                primary_checkout: verified_primary_checkout(&repository, primary_checkout, checkout.as_deref())?,
                checkout: checkout.map(|path| std::fs::canonicalize(&path).map_err(|error| error.to_string())).transpose()?,
                name,
                kind,
            },
            Self::ReopenProject {
                repository,
                checkout,
                primary_checkout,
                name,
                kind,
            } => Self::ReopenProject {
                repository: canonical_repository(&repository)?,
                primary_checkout: verified_primary_checkout(&repository, primary_checkout, checkout.as_deref())?,
                checkout: checkout.map(|path| std::fs::canonicalize(&path).map_err(|error| error.to_string())).transpose()?,
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
            | Self::DeleteChannel { .. }
            | Self::AgentsChanged) => edit,
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
    fn legacy_project_metadata_defaults_checkout_and_none_updates_preserve_it() {
        let legacy: ModelProject = serde_json::from_value(serde_json::json!({
            "repository": "/metadata/project.git", "name": "project", "ordinal": 7
        })).unwrap();
        assert_eq!(legacy.checkout, None);
        assert_eq!(legacy.primary_checkout, None);
        let mut model = WorkspaceModel { projects: vec![legacy], ..Default::default() };
        model.fold(1, &ModelEdit::OpenProject {
            repository: "/metadata/project.git".into(), checkout: Some("/checkouts/project".into()), primary_checkout: Some("/checkouts/project".into()),
            name: "project".into(), kind: ProjectKind::Git,
        });
        let legacy_edit: ModelEdit = serde_json::from_value(serde_json::json!({
            "OpenProject": { "repository": "/metadata/project.git", "name": "renamed", "kind": "Git" }
        })).unwrap();
        model.fold(2, &legacy_edit);
        assert_eq!(model.projects[0].checkout.as_deref(), Some(Path::new("/checkouts/project")));
        model.fold(3, &ModelEdit::OpenProject {
            repository: "/metadata/project.git".into(), checkout: Some("/checkouts/linked".into()), primary_checkout: None,
            name: "renamed".into(), kind: ProjectKind::Git,
        });
        assert_eq!(model.projects[0].checkout.as_deref(), Some(Path::new("/checkouts/linked")));
        assert_eq!(model.projects[0].primary_checkout.as_deref(), Some(Path::new("/checkouts/project")));
        assert_eq!(model.projects[0].ordinal, 7);
        assert_eq!(model.projects[0].name, "renamed");
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
