//! Daemon-owned pane membership. Runtime residency, focus, scroll and drafts are
//! client state and never cross this boundary.
use crate::model::{CardId, CardKind, CardState, ChannelId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A live view on one daemon. Project identity owns its worktree lifecycle;
/// the path distinguishes linked worktrees and ordinary folder strips.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    Channel(ChannelId),
    Worktree { repository: PathBuf, path: PathBuf },
}

impl Scope {
    pub fn canonicalized(self) -> Result<Self, String> {
        match self {
            Self::Channel(_) => Ok(self),
            Self::Worktree { repository, path } => Ok(Self::Worktree {
                repository: super::canonical_repository(&repository)?,
                path: super::canonical_repository(&path)?,
            }),
        }
    }
}
impl From<ChannelId> for Scope {
    fn from(channel: ChannelId) -> Self {
        Self::Channel(channel)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: CardId,
    pub title: String,
    #[serde(default)]
    pub user_title: Option<String>,
    #[serde(default)]
    pub pending_terminal_start: bool,
    pub width_fraction: f32,
    #[serde(with = "wire_kind")]
    pub kind: CardKind,
    pub scope: Option<PathBuf>,
}

impl Pane {
    pub fn from_card(card: &CardState, scope: Option<PathBuf>) -> Option<Self> {
        (card.existence.is_open() && !card.kind.is_utility()).then(|| Self {
            id: card.id,
            title: card.title.clone(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: card.width_fraction,
            kind: shared_kind(&card.kind),
            scope,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.width_fraction.is_finite() || self.width_fraction <= 0.0 {
            return Err("pane width must be finite and positive".into());
        }
        if self.kind.is_utility() {
            return Err("utility panes belong to the client window".into());
        }
        if self.scope.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err("pane scope must be an absolute device path".into());
        }
        Ok(())
    }
}

/// Recent resurrection hints travel with the view; the daemon keeps the full
/// narrow guard set in SQLite rather than sending history in every payload.
pub const RECENT_RETIRED_LIMIT: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub scope: Scope,
    pub revision: u64,
    pub panes: Vec<Pane>,
    /// Recent removal hints, bounded for reconnect payloads. The daemon's
    /// indexed retirement guards also reject older cached pane identities.
    pub retired: Vec<CardId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Change {
    Insert {
        pane: Pane,
        before: Option<CardId>,
    },
    Update {
        before: Pane,
        pane: Pane,
    },
    Transfer {
        id: CardId,
        destination: Scope,
        before: Option<CardId>,
    },
    Remove {
        id: CardId,
    },
    Move {
        id: CardId,
        before: Option<CardId>,
    },
    InsertIfEmpty {
        pane: Pane,
    },
}

impl View {
    pub fn empty(scope: impl Into<Scope>) -> Self {
        Self {
            scope: scope.into(),
            revision: 0,
            panes: Vec::new(),
            retired: Vec::new(),
        }
    }

    /// Typed changes preserve unrelated edits; the daemon fences them with the
    /// view revision and records operation IDs before acknowledging them.
    pub fn apply(&mut self, change: &Change) -> Result<bool, String> {
        let previous = self.panes.clone();
        let retired = self.retired.clone();
        match change {
            Change::InsertIfEmpty { pane } => {
                pane.validate()?;
                if self.panes.is_empty() && !self.retired.contains(&pane.id) {
                    self.panes.push(pane.clone());
                }
            }
            Change::Insert { pane, before } => {
                pane.validate()?;
                if !self.retired.contains(&pane.id)
                    && !self.panes.iter().any(|item| item.id == pane.id)
                {
                    let index = self.position(*before);
                    self.panes.insert(index, pane.clone());
                }
            }
            Change::Update { before, pane } => {
                if before.id != pane.id {
                    return Err("pane identity cannot change".into());
                }
                pane.validate()?;
                if let Some(existing) = self.panes.iter_mut().find(|item| item.id == pane.id) {
                    if before.title != pane.title {
                        existing.title = pane.title.clone();
                    }
                    if before.user_title != pane.user_title {
                        existing.user_title = pane.user_title.clone();
                    }
                    if before.pending_terminal_start != pane.pending_terminal_start {
                        existing.pending_terminal_start = pane.pending_terminal_start;
                    }
                    if before.width_fraction != pane.width_fraction {
                        existing.width_fraction = pane.width_fraction;
                    }
                    if before.kind != pane.kind {
                        existing.kind = pane.kind.clone();
                    }
                    if before.scope != pane.scope {
                        existing.scope = pane.scope.clone();
                    }
                }
            }
            Change::Transfer { .. } => {
                return Err("a transfer must be applied to both views atomically".into());
            }
            Change::Remove { id } => {
                self.panes.retain(|pane| pane.id != *id);
                if !self.retired.contains(id) {
                    self.retired.push(*id);
                    if self.retired.len() > RECENT_RETIRED_LIMIT {
                        self.retired.drain(..self.retired.len() - RECENT_RETIRED_LIMIT);
                    }
                }
            }
            Change::Move { id, before } => {
                if before != &Some(*id) {
                    if let Some(index) = self.panes.iter().position(|pane| pane.id == *id) {
                        let pane = self.panes.remove(index);
                        let index = self.position(*before);
                        self.panes.insert(index, pane);
                    }
                }
            }
        }
        Ok(self.panes != previous || self.retired != retired)
    }

    fn position(&self, before: Option<CardId>) -> usize {
        before
            .and_then(|id| self.panes.iter().position(|pane| pane.id == id))
            .unwrap_or(self.panes.len())
    }
}

/// Compare two local projections. This is invoked only for local changes;
/// adopting a host snapshot resets the projection baseline first.
pub fn changes(previous: &[Pane], desired: &[Pane]) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut working = previous.to_vec();
    for pane in previous {
        if !desired.iter().any(|item| item.id == pane.id) {
            changes.push(Change::Remove { id: pane.id });
            working.retain(|item| item.id != pane.id);
        }
    }
    for (index, pane) in desired.iter().enumerate() {
        match working.iter().position(|item| item.id == pane.id) {
            None => {
                let before = working.get(index).map(|item| item.id);
                changes.push(Change::Insert {
                    pane: pane.clone(),
                    before,
                });
                working.insert(index.min(working.len()), pane.clone());
            }
            Some(current) => {
                if working[current] != *pane {
                    changes.push(Change::Update {
                        before: working[current].clone(),
                        pane: pane.clone(),
                    });
                    working[current] = pane.clone();
                }
                if current != index {
                    let item = working.remove(current);
                    let before = working.get(index).map(|item| item.id);
                    changes.push(Change::Move {
                        id: pane.id,
                        before,
                    });
                    working.insert(index.min(working.len()), item);
                }
            }
        }
    }
    changes
}

/// Strip presentation coordinates and per-window project identities.
pub fn shared_kind(kind: &CardKind) -> CardKind {
    let mut kind = kind.clone();
    match &mut kind {
        CardKind::Browser(session) => {
            session.scroll_x = 0.0;
            session.scroll_y = 0.0;
        }
        CardKind::Repository(view) => {
            view.project = crate::model::ProjectId::from_uuid(uuid::Uuid::nil());
        }
        _ => {}
    }
    kind
}

/// Apply an optimistic transfer across projections before its host reply.
pub fn transfer(
    views: &mut [View],
    source: &Scope,
    id: CardId,
    destination: &Scope,
    before: Option<CardId>,
) -> Result<(), String> {
    let Some(pane) = views
        .iter()
        .find(|view| &view.scope == source)
        .and_then(|view| view.panes.iter().find(|pane| pane.id == id))
        .cloned()
    else {
        return Ok(());
    };
    if source == destination {
        return Ok(());
    }
    let Some(target) = views.iter().position(|view| &view.scope == destination) else {
        return Ok(());
    };
    views[target].retired.retain(|retired| *retired != id);
    views[target].apply(&Change::Insert { pane, before })?;
    if let Some(source) = views.iter_mut().find(|view| &view.scope == source) {
        source.apply(&Change::Remove { id })?;
    }
    Ok(())
}

/// CardKind's tagged JSON belongs to the session store. The IPC codec needs an
/// externally tagged enum and every optional field present in its tuple.
mod wire_kind {
    use super::*;
    use crate::model::{
        BrowserSession, ProjectId, PullRequestView, RepositoryView, TerminalSession,
    };
    #[derive(Serialize, Deserialize)]
    enum Kind {
        Terminal(TerminalSession),
        Chat(crate::chat::Session),
        Browser(BrowserSession),
        PullRequests {
            repository: PathBuf,
            selected_number: Option<u64>,
            file_panel_width: Option<u16>,
            file_panel_collapsed: Option<bool>,
        },
        Repository {
            project: ProjectId,
            repository: PathBuf,
            worktree: PathBuf,
            #[serde(with = "crate::selected_file_path")]
            selected_file: Option<PathBuf>,
        },
        Settings,
        Accounts,
        Automations,
    }
    pub(super) fn serialize<S: serde::Serializer>(
        kind: &CardKind,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            return kind.serialize(serializer);
        }
        let kind = match kind {
            CardKind::Terminal(session) => Kind::Terminal(session.clone()),
            CardKind::Chat(session) => Kind::Chat(session.clone()),
            CardKind::Browser(session) => Kind::Browser(session.clone()),
            CardKind::PullRequests(view) => Kind::PullRequests {
                repository: view.repository.clone(),
                selected_number: view.selected_number,
                file_panel_width: view.file_panel_width,
                file_panel_collapsed: view.file_panel_collapsed,
            },
            CardKind::Repository(view) => Kind::Repository {
                project: view.project,
                repository: view.repository.clone(),
                worktree: view.worktree.clone(),
                selected_file: view.selected_file.clone(),
            },
            CardKind::Settings => Kind::Settings,
            CardKind::Accounts => Kind::Accounts,
            CardKind::Automations => Kind::Automations,
        };
        kind.serialize(serializer)
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<CardKind, D::Error> {
        if deserializer.is_human_readable() {
            return CardKind::deserialize(deserializer);
        }
        Ok(match Kind::deserialize(deserializer)? {
            Kind::Terminal(session) => CardKind::Terminal(session),
            Kind::Chat(session) => CardKind::Chat(session),
            Kind::Browser(session) => CardKind::Browser(session),
            Kind::PullRequests {
                repository,
                selected_number,
                file_panel_width,
                file_panel_collapsed,
            } => CardKind::PullRequests(PullRequestView {
                repository,
                selected_number,
                file_panel_width,
                file_panel_collapsed,
            }),
            Kind::Repository {
                project,
                repository,
                worktree,
                selected_file,
            } => CardKind::Repository(RepositoryView {
                project,
                repository,
                worktree,
                selected_file,
            }),
            Kind::Settings => CardKind::Settings,
            Kind::Accounts => CardKind::Accounts,
            Kind::Automations => CardKind::Automations,
        })
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::model::*;
    #[test]
    fn nonempty_views_round_trip_through_ipc_and_persistent_json() {
        use std::os::unix::ffi::OsStringExt;
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![],
            working_directory: "/repo".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let terminal = TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let kinds = vec![
            CardKind::Terminal(terminal),
            CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: BrowserRestoreFidelity::UrlOnly,
            }),
            CardKind::Repository(RepositoryView {
                project: ProjectId::new(),
                repository: "/repo/.git".into(),
                worktree: "/repo".into(),
                selected_file: None,
            }),
            CardKind::Repository(RepositoryView {
                project: ProjectId::new(),
                repository: "/repo/.git".into(),
                worktree: "/repo".into(),
                selected_file: Some(PathBuf::from(std::ffi::OsString::from_vec(
                    b"raw-\xff.txt".to_vec(),
                ))),
            }),
            CardKind::PullRequests(PullRequestView {
                repository: "/repo/.git".into(),
                selected_number: None,
                file_panel_width: None,
                file_panel_collapsed: None,
            }),
        ];
        for kind in kinds {
            let pane = Pane {
                id: CardId::new(),
                title: "pane".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 0.5,
                kind,
                scope: None,
            };
            let edit = crate::workspace_model::ModelEdit::SeedView {
                scope: Scope::Channel(ChannelId::new()),
                panes: vec![pane.clone()],
                closed: vec![],
            };
            let bytes = postcard::to_allocvec(&edit).unwrap();
            assert_eq!(
                postcard::from_bytes::<crate::workspace_model::ModelEdit>(&bytes).unwrap(),
                edit
            );
            assert_eq!(
                serde_json::from_str::<Pane>(&serde_json::to_string(&pane).unwrap()).unwrap(),
                pane
            );
        }
    }
}

/// A discovery observation and a shared pane can arrive in either order.
/// They identify one actor even when an old client minted another card ID.
pub fn same_resource(left: &CardKind, right: &CardKind) -> bool {
    match (left, right) {
        (CardKind::Terminal(a), CardKind::Terminal(b)) => a.id == b.id,
        (CardKind::Chat(a), CardKind::Chat(b)) => a.id == b.id,
        (CardKind::Browser(a), CardKind::Browser(b)) => a.id == b.id,
        _ => false,
    }
}
