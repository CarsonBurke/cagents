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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub scope: Scope,
    pub revision: u64,
    pub panes: Vec<Pane>,
    /// Retained across reconnects and restarts. An old cache cannot reintroduce
    /// a pane removed from this view.
    pub retired: Vec<CardId>,
}

/// Current display ownership of one canonical managed pane. This never changes
/// its accepted execution placement, launch directory, or runtime descriptor.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedDisplay {
    pub scope: Scope,
    /// The saved descriptor is authoritative when present. An unpublished actor
    /// has no pane yet and may use its accepted placement for first adoption.
    pub pane: Option<Pane>,
}

/// Resolve against the incoming authoritative snapshot, before local adoption.
/// Canonical identity bounds the lookup; legacy inventory aliases are reconciled
/// separately. A removed, replaced or ambiguous pane grants no fallback authority.
/// A transfer's retired source does not override its one exact current owner.
pub fn managed_display(
    model: &super::WorkspaceModel,
    agent: &crate::agent_execution::Agent,
) -> Option<ManagedDisplay> {
    ManagedDisplayIndex::new(model).resolve(agent)
}

/// Build once per incoming model, then resolve each managed actor in constant
/// time. Repeated agent adoption never rescans every saved pane per actor.
pub struct ManagedDisplayIndex<'a> {
    owners: std::collections::HashMap<CardId, Option<(&'a Scope, &'a Pane)>>,
    retired: std::collections::HashSet<CardId>,
    resources: std::collections::HashMap<
        (u8, crate::model::TerminalId),
        std::collections::HashSet<CardId>,
    >,
}

impl<'a> ManagedDisplayIndex<'a> {
    pub fn new(model: &'a super::WorkspaceModel) -> Self {
        let mut owners = std::collections::HashMap::new();
        let mut retired = std::collections::HashSet::new();
        let mut resources: std::collections::HashMap<_, std::collections::HashSet<CardId>> =
            std::collections::HashMap::new();
        for view in &model.views {
            retired.extend(view.retired.iter().copied());
            for pane in &view.panes {
                if let Some(resource) = pane_resource(&pane.kind) {
                    resources.entry(resource).or_default().insert(pane.id);
                }
                owners
                    .entry(pane.id)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some((&view.scope, pane)));
            }
        }
        Self {
            owners,
            retired,
            resources,
        }
    }

    /// Shared alias reconciliation may retire an alias before canonical
    /// adoption. Fence replaced/ambiguous canonical evidence without blocking
    /// that existing migration path merely because an alias is present.
    pub(crate) fn permits_projection(
        &self,
        agent: &crate::agent_execution::Agent,
        pane: &Pane,
    ) -> bool {
        let id = CardId::from_uuid(agent.id.as_uuid());
        if pane.id != id || !managed_pane_matches(agent, pane) {
            return false;
        }
        match self.owners.get(&id) {
            Some(Some((_, saved))) => managed_pane_matches(agent, saved),
            Some(None) => false,
            None => !self.retired.contains(&id),
        }
    }

    pub fn resolve(&self, agent: &crate::agent_execution::Agent) -> Option<ManagedDisplay> {
        let scope = self.scope(agent)?.clone();
        let id = CardId::from_uuid(agent.id.as_uuid());
        let pane = self
            .owners
            .get(&id)
            .and_then(Option::as_ref)
            .map(|(_, pane)| (*pane).clone());
        Some(ManagedDisplay { scope, pane })
    }

    pub fn scope<'b>(&self, agent: &'b crate::agent_execution::Agent) -> Option<&'b Scope>
    where
        'a: 'b,
    {
        use crate::agent_execution::{HostedSession, Surface};
        let id = CardId::from_uuid(agent.id.as_uuid());
        let owner = match self.owners.get(&id) {
            Some(owner) => Some(owner.as_ref()?),
            None => None,
        };
        let resource = if let Some((_, pane)) = owner {
            pane_resource(&pane.kind)?
        } else if agent.spec.surface == Surface::Terminal {
            (1, agent.console.as_ref()?.id)
        } else {
            match agent.session.as_ref()? {
                HostedSession::Chat(session) => (0, session.id),
                HostedSession::Terminal(session) => (1, session.id),
            }
        };
        // Leave inventory aliases to their existing reconciliation. Adoption
        // cannot manufacture a second canonical pane for an already shown actor.
        if self
            .resources
            .get(&resource)
            .is_some_and(|cards| cards.iter().any(|card| *card != id))
        {
            return None;
        }
        if let Some((scope, pane)) = owner {
            if !managed_pane_matches(agent, pane) {
                return None;
            }
            return Some(*scope);
        }
        if self.retired.contains(&id) {
            return None;
        }
        Some(&agent.placement)
    }
}

/// Directory evidence for attention branch labels. A verified directory is still
/// only presentation metadata: callers must match an actually loaded worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionDirectory<'a> {
    /// Older snapshots have no location record; retain their launch-directory
    /// presentation without inventing a relocated path.
    Launch,
    Verified(&'a std::path::Path),
    /// Refused, ambiguous, replaced or incomplete evidence has no branch label.
    Unavailable,
}

/// Build once per sidebar refresh. Location metadata can override an immutable
/// launch recipe only for the exact current canonical saved pane and actor.
/// It never changes shared filing, process ownership or execution policy.
pub struct AttentionDirectoryIndex<'a> {
    displays: ManagedDisplayIndex<'a>,
    agents: std::collections::HashMap<CardId, Option<&'a crate::agent_execution::Agent>>,
    locations: std::collections::HashMap<CardId, Option<&'a super::AgentLocation>>,
}

impl<'a> AttentionDirectoryIndex<'a> {
    pub fn new(model: &'a super::WorkspaceModel) -> Self {
        let mut agents = std::collections::HashMap::new();
        for agent in &model.agents {
            agents.entry(CardId::from_uuid(agent.id.as_uuid()))
                .and_modify(|entry| *entry = None).or_insert(Some(agent));
        }
        let mut locations = std::collections::HashMap::new();
        for location in &model.agent_locations {
            locations.entry(CardId::from_uuid(location.agent.as_uuid()))
                .and_modify(|entry| *entry = None).or_insert(Some(location));
        }
        Self { displays: ManagedDisplayIndex::new(model), agents, locations }
    }

    pub fn directory(&self, card: &CardState) -> AttentionDirectory<'a> {
        let Some(location) = self.locations.get(&card.id) else {
            return AttentionDirectory::Launch;
        };
        let Some(location) = *location else {
            return AttentionDirectory::Unavailable;
        };
        let Some(Some(agent)) = self.agents.get(&card.id) else {
            return AttentionDirectory::Unavailable;
        };
        if !card.existence.is_open() || agent.closed
            || location.blocked_directory.is_some() || !location.directory.is_absolute()
            || self.displays.scope(agent).is_none()
        {
            return AttentionDirectory::Unavailable;
        }
        // No fallback placement: location metadata needs current saved canonical
        // membership, even for actors which could otherwise be freshly adopted.
        let Some(Some((_, pane))) = self.displays.owners.get(&card.id) else {
            return AttentionDirectory::Unavailable;
        };
        let matches = match (&card.kind, &pane.kind) {
            (CardKind::Chat(current), CardKind::Chat(saved)) => current.proof.as_ref()
                .is_some_and(crate::model::HostSessionProof::is_complete)
                && crate::agent_execution::same_chat_process(current, saved),
            (CardKind::Terminal(current), CardKind::Terminal(saved)) => current.continuity
                .live_proof().is_some_and(|proof| proof.is_complete())
                && crate::agent_execution::same_terminal_process(current, saved),
            _ => false,
        };
        if matches {
            AttentionDirectory::Verified(&location.directory)
        } else {
            AttentionDirectory::Unavailable
        }
    }
}

fn pane_resource(kind: &CardKind) -> Option<(u8, crate::model::TerminalId)> {
    match kind {
        CardKind::Chat(session) => Some((0, session.id)),
        CardKind::Terminal(session) => Some((1, session.id)),
        _ => None,
    }
}

/// An ended automation console is display evidence only: its closed owner may
/// convert it to a saved native read view, never hydrate or relaunch the console.
/// All descriptor fields except the observed terminal end state must match.
pub fn archived_console_matches(
    agent: &crate::agent_execution::Agent,
    current: &crate::model::TerminalSession,
) -> bool {
    if !agent.closed
        || !matches!(
            agent.origin,
            crate::agent_execution::Origin::Automation { .. }
        )
        || !matches!(
            current.continuity,
            crate::model::TerminalContinuity::Exited { .. }
                | crate::model::TerminalContinuity::RelaunchAvailable {
                    last_exit: Some(_),
                    ..
                }
        )
    {
        return false;
    }
    let Some(mut original) = agent.console.clone() else {
        return false;
    };
    original.continuity = current.continuity.clone();
    original == *current
}

pub(crate) fn managed_pane_matches(agent: &crate::agent_execution::Agent, pane: &Pane) -> bool {
    use crate::agent_execution::HostedSession;
    if let CardKind::Terminal(current) = &pane.kind
        && archived_console_matches(agent, current)
    {
        return true;
    }
    let current = match &pane.kind {
        CardKind::Chat(session)
            if session
                .proof
                .as_ref()
                .is_some_and(crate::model::HostSessionProof::is_complete) =>
        {
            HostedSession::Chat(session.clone())
        }
        CardKind::Terminal(session)
            if session
                .continuity
                .live_proof()
                .is_some_and(|proof| proof.is_complete()) =>
        {
            HostedSession::Terminal(session.clone())
        }
        _ => return false,
    };
    agent
        .session
        .as_ref()
        .is_some_and(|session| session.same_process(&current))
        || agent
            .console
            .as_ref()
            .is_some_and(|session| HostedSession::Terminal(session.clone()).same_process(&current))
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
        let retired = self.retired.len();
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
        Ok(self.panes != previous || self.retired.len() != retired)
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

#[cfg(test)]
mod managed_display_tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target},
        model::*,
    };

    fn fixture() -> (Agent, Pane, Pane) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("display-host"),
            boot_id: BootId::new("display-boot"),
            host_incarnation: HostIncarnation::new("display-incarnation"),
        };
        let recipe = LaunchRecipe {
            program: "claude".into(),
            arguments: vec![],
            working_directory: "/execution/cwd".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let console = TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        };
        let native = crate::chat::Session {
            id: console.id,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                session_id: Some("existing-conversation".into()),
                recipe,
            },
            proof: Some(proof),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let placement = Scope::Channel(ChannelId::new());
        let agent = Agent {
            id: AgentId::new(),
            name: None,
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: "/execution/.git".into(),
                    directory: "/execution/cwd".into(),
                    placement: Some(placement.clone()),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement,
            session: Some(HostedSession::Chat(native.clone())),
            console: Some(console.clone()),
            created_ms: 0,
            closed: false,
        };
        let pane = Pane {
            id: CardId::from_uuid(agent.id.as_uuid()),
            title: "Saved title".into(),
            user_title: Some("User title".into()),
            pending_terminal_start: false,
            width_fraction: 0.6,
            kind: CardKind::Chat(native),
            scope: Some("/display/scope".into()),
        };
        let terminal = Pane {
            kind: CardKind::Terminal(console),
            ..pane.clone()
        };
        (agent, pane, terminal)
    }

    fn location_model(agent: &Agent, pane: &Pane) -> super::super::WorkspaceModel {
        super::super::WorkspaceModel {
            agents: vec![agent.clone()],
            agent_locations: vec![super::super::AgentLocation {
                agent: agent.id,
                repository: "/relocated/repository/.git".into(),
                directory: "/relocated/worktree".into(),
                blocked_directory: None,
            }],
            views: vec![View {
                scope: Scope::Channel(ChannelId::new()),
                revision: 1,
                panes: vec![pane.clone()],
                retired: vec![],
            }],
            ..Default::default()
        }
    }

    fn location_card(pane: &Pane) -> CardState {
        CardState {
            id: pane.id,
            generation: 0,
            title: pane.title.clone(),
            width_fraction: pane.width_fraction,
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind: pane.kind.clone(),
        }
    }

    #[test]
    fn attention_directory_uses_verified_location_for_exact_saved_surface_without_mutation() {
        let (agent, native, terminal) = fixture();
        for pane in [native, terminal] {
            let model = location_model(&agent, &pane);
            let card = location_card(&pane);
            let before = (model.clone(), card.clone());
            assert_eq!(AttentionDirectoryIndex::new(&model).directory(&card),
                AttentionDirectory::Verified(std::path::Path::new("/relocated/worktree")));
            assert_eq!((model, card), before, "display metadata cannot rewrite launch or filing");
        }
    }

    #[test]
    fn attention_directory_refuses_candidates_duplicates_and_missing_membership() {
        let (agent, pane, _) = fixture();
        let base = location_model(&agent, &pane);
        let card = location_card(&pane);
        for case in 0..8 {
            let mut model = base.clone();
            match case {
                0 => model.agent_locations[0].blocked_directory = Some("/candidate/worktree".into()),
                1 => model.agent_locations.push(model.agent_locations[0].clone()),
                2 => model.agents.push(agent.clone()),
                3 => model.agents.clear(),
                4 => model.views[0].panes.clear(),
                5 => model.views.push(model.views[0].clone()),
                6 => model.agents[0].closed = true,
                7 => model.agent_locations[0].directory = "relative".into(),
                _ => unreachable!(),
            }
            assert_eq!(AttentionDirectoryIndex::new(&model).directory(&card),
                AttentionDirectory::Unavailable, "refusal case {case}");
        }
        let mut old = base;
        old.agent_locations.clear();
        assert_eq!(AttentionDirectoryIndex::new(&old).directory(&card), AttentionDirectory::Launch);
        assert_eq!(AttentionDirectoryIndex::new(&Default::default()).directory(&card), AttentionDirectory::Launch);
    }

    #[test]
    fn attention_directory_requires_complete_current_card_proof_and_open_canonical_identity() {
        let (agent, native, terminal) = fixture();
        for pane in [native, terminal] {
            let model = location_model(&agent, &pane);
            for case in 0..4 {
                let mut card = location_card(&pane);
                match case {
                    0 => card.existence = Existence::Closing { operation_id: OperationId::new() },
                    1 => match &mut card.kind {
                        CardKind::Chat(session) => session.proof = None,
                        CardKind::Terminal(session) => session.continuity = TerminalContinuity::RelaunchAvailable {
                            recipe: session.launch.clone(), last_exit: None,
                        },
                        _ => unreachable!(),
                    },
                    2 => match &mut card.kind {
                        CardKind::Chat(session) => session.launch.recipe.arguments.push("different".into()),
                        CardKind::Terminal(session) => session.launch.arguments.push("different".into()),
                        _ => unreachable!(),
                    },
                    3 => card.kind = CardKind::Settings,
                    _ => unreachable!(),
                }
                assert_eq!(AttentionDirectoryIndex::new(&model).directory(&card),
                    AttentionDirectory::Unavailable, "card refusal {case}");
            }
        }
    }

    #[test]
    fn attention_managed_display_uses_exact_saved_native_or_console_membership_without_mutation() {
        let (agent, native, terminal) = fixture();
        let original = agent.clone();
        for pane in [native, terminal] {
            for scope in [
                Scope::Channel(ChannelId::new()),
                Scope::Worktree {
                    repository: "/legacy-display/.git".into(),
                    path: "/legacy-display/worktree".into(),
                },
            ] {
                let mut source = View::empty(agent.placement.clone());
                source.retired.push(pane.id);
                let target = View {
                    scope: scope.clone(),
                    revision: 8,
                    panes: vec![pane.clone()],
                    retired: vec![],
                };
                let model = super::super::WorkspaceModel {
                    views: vec![source, target],
                    ..Default::default()
                };
                let display = managed_display(&model, &agent).unwrap();
                assert_eq!(display.scope, scope);
                assert_eq!(display.pane.as_ref(), Some(&pane));
                assert_eq!(
                    agent, original,
                    "display resolution cannot rewrite accepted execution ownership"
                );
            }
        }
    }

    #[test]
    fn attention_managed_aliases_defer_adoption_without_blocking_exact_shared_reconciliation() {
        let (agent, pane, _) = fixture();
        let mut alias = pane.clone();
        alias.id = CardId::new();
        let model = super::super::WorkspaceModel {
            views: vec![View {
                scope: Scope::Channel(ChannelId::new()),
                revision: 1,
                panes: vec![pane.clone(), alias],
                retired: vec![],
            }],
            ..Default::default()
        };
        let index = ManagedDisplayIndex::new(&model);
        assert!(
            index.resolve(&agent).is_none(),
            "an existing actor alias cannot produce another canonical adoption"
        );
        assert!(
            index.permits_projection(&agent, &pane),
            "existing shared alias migration must still be able to adopt the exact canonical descriptor"
        );
    }

    #[test]
    fn attention_managed_display_requires_complete_native_proof_even_when_both_descriptors_match() {
        let (mut agent, mut pane, _) = fixture();
        let crate::agent_execution::HostedSession::Chat(expected) = agent.session.as_mut().unwrap()
        else {
            unreachable!()
        };
        expected.proof = None;
        let CardKind::Chat(current) = &mut pane.kind else {
            unreachable!()
        };
        current.proof = None;
        let mut model = super::super::WorkspaceModel {
            views: vec![View {
                scope: Scope::Channel(ChannelId::new()),
                revision: 1,
                panes: vec![pane],
                retired: vec![],
            }],
            ..Default::default()
        };
        assert!(managed_display(&model, &agent).is_none());
        let mut proof = agent
            .console
            .as_ref()
            .unwrap()
            .continuity
            .live_proof()
            .unwrap();
        proof.host_incarnation = HostIncarnation::new("");
        let crate::agent_execution::HostedSession::Chat(expected) = agent.session.as_mut().unwrap()
        else {
            unreachable!()
        };
        expected.proof = Some(proof.clone());
        let CardKind::Chat(current) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        current.proof = Some(proof);
        assert!(managed_display(&model, &agent).is_none());
    }

    #[test]
    fn attention_archived_console_display_tolerates_only_its_observed_end_state() {
        let (mut agent, _, mut pane) = fixture();
        agent.closed = true;
        agent.origin = Origin::Automation {
            id: uuid::Uuid::new_v4(),
        };
        let CardKind::Terminal(console) = &mut pane.kind else {
            unreachable!()
        };
        let exit = TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        };
        console.continuity = TerminalContinuity::Exited {
            last_exit: exit.clone(),
        };
        let mut model = super::super::WorkspaceModel {
            views: vec![View {
                scope: Scope::Channel(ChannelId::new()),
                revision: 1,
                panes: vec![pane.clone()],
                retired: vec![],
            }],
            ..Default::default()
        };
        assert!(managed_display(&model, &agent).is_some());
        let CardKind::Terminal(console) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        console.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: console.launch.clone(),
            last_exit: Some(exit),
        };
        assert!(managed_display(&model, &agent).is_some());
        let CardKind::Terminal(console) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        console.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("display-host"),
            boot_id: BootId::new("display-boot"),
            host_incarnation: HostIncarnation::new("replacement"),
        };
        assert!(
            managed_display(&model, &agent).is_none(),
            "archive display cannot accept a conflicting live owner"
        );
        let CardKind::Terminal(console) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        console.continuity = TerminalContinuity::Exited {
            last_exit: TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        console.launch.working_directory = "/different".into();
        assert!(managed_display(&model, &agent).is_none());
        model.views[0].panes[0] = pane;
        agent.closed = false;
        assert!(
            managed_display(&model, &agent).is_none(),
            "live actor lookup always requires its exact live proof"
        );
    }

    #[test]
    fn attention_managed_display_rejects_removed_replaced_ambiguous_and_unproven_canonical_panes() {
        let (agent, pane, _) = fixture();
        let scope = Scope::Channel(ChannelId::new());
        let mut model = super::super::WorkspaceModel {
            views: vec![View::empty(scope.clone())],
            ..Default::default()
        };
        assert_eq!(
            managed_display(&model, &agent).unwrap().scope,
            agent.placement
        );
        model.views[0].retired.push(pane.id);
        assert!(
            managed_display(&model, &agent).is_none(),
            "removal must not recreate the old target"
        );
        model.views[0].panes.push(pane.clone());
        assert!(
            managed_display(&model, &agent).is_some(),
            "one exact current owner beats its retired transfer source"
        );
        model.views.push(View {
            scope: Scope::Channel(ChannelId::new()),
            revision: 1,
            panes: vec![pane.clone()],
            retired: vec![],
        });
        assert!(managed_display(&model, &agent).is_none());
        model.views.pop();
        let CardKind::Chat(session) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        session.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("replacement");
        assert!(
            managed_display(&model, &agent).is_none(),
            "matching session ID never substitutes a replacement process"
        );
        let CardKind::Chat(session) = &mut model.views[0].panes[0].kind else {
            unreachable!()
        };
        session.proof = None;
        assert!(managed_display(&model, &agent).is_none());
        model.views[0].panes[0] = pane.clone();
        model.views[0].panes[0].id = CardId::new();
        model.views[0].retired.clear();
        assert!(
            managed_display(&model, &agent).is_none(),
            "legacy aliases must not manufacture a second canonical managed pane"
        );
    }
}
