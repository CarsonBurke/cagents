//! The half of the sidebar model that every window agrees on.
//!
//! A window keeps its own cards, their order, their widths and which one is
//! focused. The projects and channels those cards hang under are shared, so two
//! windows open at once show the same sidebar. These types are the wire and
//! storage shape of that shared half: the daemon owns them durably, the CLI
//! mutates them, and every window mirrors them into its own store.
//!
//! [`ChannelId`] is reused rather than redefined here. `src/model.rs` is
//! declared ungated in `src/lib.rs`, so the CLI, the daemon and the UI already
//! resolve it in every build lane; moving the id types into this module or
//! re-exporting them would give one identity two spellings for no gain.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::ChannelId;

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
    /// Creation rank, assigned by the registry: strictly increasing, never
    /// reused.
    pub ordinal: u32,
}

/// The half of a channel that is not window-local.
///
/// `cards`, `focused` and `active_channel` stay in each window's own store: two
/// windows share which channels exist and disagree about what is open inside
/// them, exactly as two editor windows on one folder do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChannel {
    pub id: ChannelId,
    pub repository: PathBuf,
    pub title: String,
    /// Creation rank, assigned by the registry: strictly increasing, never
    /// reused, and therefore stable across a window restart.
    ///
    /// The registry assigns it rather than the client so two windows creating a
    /// channel at the same moment cannot mint the same rank. The sidebar renders
    /// a project's channels by descending rank, so the newest sits on top.
    pub ordinal: u32,
}

/// The identity of a registry that has never been created.
///
/// A window compares the identity it recorded against the one the model carries
/// to decide whether it has ever shared anything with *this* registry. The nil
/// identity can only come from a [`WorkspaceModel::default`], never from a real
/// registry, and it matches no recorded identity - so it takes the safe branch,
/// where nothing local counts as shared and therefore nothing is retired.
pub const NO_REGISTRY: Uuid = Uuid::nil();

/// The shared sidebar model as the registry currently holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    pub projects: Vec<ModelProject>,
    pub channels: Vec<ModelChannel>,
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
            ModelEdit::OpenProject { repository, name } => {
                match self
                    .projects
                    .iter_mut()
                    .find(|project| project.repository == *repository)
                {
                    Some(project) => project.name = name.clone(),
                    None => self.projects.push(ModelProject {
                        repository: repository.clone(),
                        name: name.clone(),
                        ordinal: PENDING_ORDINAL,
                    }),
                }
            }
            ModelEdit::CloseProject { repository } => {
                self.projects
                    .retain(|project| project.repository != *repository);
                self.channels
                    .retain(|channel| channel.repository != *repository);
            }
            ModelEdit::CreateChannel {
                id,
                repository,
                title,
            } => {
                if self.channel(*id).is_none() {
                    self.channels.push(ModelChannel {
                        id: *id,
                        repository: repository.clone(),
                        title: title.clone(),
                        ordinal: PENDING_ORDINAL,
                    });
                }
            }
            ModelEdit::RetitleChannel { id, title } => {
                if let Some(channel) = self.channels.iter_mut().find(|channel| channel.id == *id) {
                    channel.title = title.clone();
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelEdit {
    /// Upsert by path. A repeat updates the name and keeps the rank.
    OpenProject { repository: PathBuf, name: String },
    /// Removes the project and every channel inside it. A project nobody has
    /// open cannot keep channels reachable.
    CloseProject { repository: PathBuf },
    /// Idempotent by id. The id is minted by the client, the rank by the
    /// registry.
    CreateChannel {
        id: ChannelId,
        repository: PathBuf,
        title: String,
    },
    /// A no-op on an unknown id: a retitle racing a delete must not resurrect
    /// what the delete removed.
    RetitleChannel { id: ChannelId, title: String },
    /// A no-op on an unknown id, so a second delete of one channel is not an
    /// error for the second window to issue it.
    DeleteChannel { id: ChannelId },
}

impl ModelEdit {
    /// Resolves this edit's repository key, once, at the boundary.
    ///
    /// Every producer spells a path in its own terms - a symlinked checkout, a
    /// trailing slash, a `..` - and the key has to be the same string in all of
    /// them or one project becomes two.
    pub fn canonicalized(self) -> Result<Self, String> {
        Ok(match self {
            Self::OpenProject { repository, name } => Self::OpenProject {
                repository: canonical_repository(&repository)?,
                name,
            },
            Self::CloseProject { repository } => Self::CloseProject {
                repository: canonical_repository(&repository)?,
            },
            Self::CreateChannel {
                id,
                repository,
                title,
            } => Self::CreateChannel {
                id,
                repository: canonical_repository(&repository)?,
                title,
            },
            edit @ (Self::RetitleChannel { .. } | Self::DeleteChannel { .. }) => edit,
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
