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

/// Exact surface selected for explicit user activation. The daemon's primary
/// session owns first display; saved canonical membership owns later display.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivationDisplay {
    pub display: ManagedDisplay,
    pub session: crate::agent_execution::HostedSession,
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
        let matches = |pane: &Pane| {
            managed_pane_matches(agent, pane)
                || matches!(&pane.kind, CardKind::Terminal(current)
                    if ended_managed_terminal_matches(agent, current))
        };
        if pane.id != id || !matches(pane) {
            return false;
        }
        match self.owners.get(&id) {
            Some(Some((_, saved))) => matches(saved),
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

    /// Explicit activation supplies an exact current proved counterpart. Saved
    /// membership still owns the surface; an unpublished actor uses this typed
    /// request rather than inferring a session from its original launch surface.
    pub fn resolve_activation(
        &self,
        agent: &crate::agent_execution::Agent,
        requested: &crate::agent_execution::HostedSession,
    ) -> Option<ManagedDisplay> {
        use crate::agent_execution::HostedSession;
        if agent.closed || !requested.has_complete_proof() {
            return None;
        }
        let owned = agent
            .session
            .as_ref()
            .is_some_and(|current| current.same_process(requested))
            || agent.console.as_ref().is_some_and(|current| {
                HostedSession::Terminal(current.clone()).same_process(requested)
            });
        if !owned {
            return None;
        }
        let resource = match requested {
            HostedSession::Chat(session) => (0, session.id),
            HostedSession::Terminal(session) => (1, session.id),
        };
        let scope = self.scope_with_resource(agent, Some(resource))?.clone();
        let id = CardId::from_uuid(agent.id.as_uuid());
        let pane = self
            .owners
            .get(&id)
            .and_then(Option::as_ref)
            .map(|(_, pane)| (*pane).clone());
        Some(ManagedDisplay { scope, pane })
    }

    /// Resolve an untyped notification UUID without guessing a surface from a
    /// launch preference. Both descriptors of a live pair must remain proved.
    pub fn activation_display(
        &self,
        agent: &crate::agent_execution::Agent,
        requested_id: crate::model::TerminalId,
    ) -> Option<ActivationDisplay> {
        use crate::agent_execution::HostedSession;
        let console = agent.console.clone().map(HostedSession::Terminal);
        if agent
            .session
            .iter()
            .chain(console.iter())
            .any(|owner| !owner.has_complete_proof())
        {
            return None;
        }
        let requested = agent
            .session
            .iter()
            .chain(console.iter())
            .find(|owner| owner.id() == requested_id)?;
        let display = self.resolve_activation(agent, requested)?;
        let session = match display.pane.as_ref().map(|pane| &pane.kind) {
            Some(CardKind::Chat(session)) => HostedSession::Chat(session.clone()),
            Some(CardKind::Terminal(session)) => HostedSession::Terminal(session.clone()),
            Some(_) => return None,
            None => agent.session.as_ref().or(console.as_ref())?.clone(),
        };
        if !session.has_complete_proof() {
            return None;
        }
        if display.pane.is_none() {
            // A secondary console can already be shown under an inventory
            // alias even when the requested UUID selects the primary chat.
            // Leave every owned typed alias to shared reconciliation before
            // treating this actor as unpublished. Saved canonical panes above
            // retain authority over their actual surface.
            let canonical = CardId::from_uuid(agent.id.as_uuid());
            for owner in agent.session.iter().chain(console.iter()) {
                let resource = match owner {
                    HostedSession::Chat(session) => (0, session.id),
                    HostedSession::Terminal(session) => (1, session.id),
                };
                if self
                    .resources
                    .get(&resource)
                    .is_some_and(|cards| cards.iter().any(|card| *card != canonical))
                {
                    return None;
                }
            }
            let primary = self.resolve_activation(agent, &session)?;
            if primary.scope != display.scope || primary.pane.is_some() {
                return None;
            }
        }
        Some(ActivationDisplay { display, session })
    }

    pub fn scope<'b>(&self, agent: &'b crate::agent_execution::Agent) -> Option<&'b Scope>
    where
        'a: 'b,
    {
        self.scope_with_resource(agent, None)
    }

    fn scope_with_resource<'b>(
        &self,
        agent: &'b crate::agent_execution::Agent,
        requested: Option<(u8, crate::model::TerminalId)>,
    ) -> Option<&'b Scope>
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
        } else if let Some(requested) = requested {
            requested
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

fn pane_resource(kind: &CardKind) -> Option<(u8, crate::model::TerminalId)> {
    match kind {
        CardKind::Chat(session) => Some((0, session.id)),
        CardKind::Terminal(session) => Some((1, session.id)),
        _ => None,
    }
}

/// An observed terminal exit may be displayed without granting live process
/// authority. Every issued descriptor field except that end state must match.
fn ended_managed_terminal_matches(
    agent: &crate::agent_execution::Agent,
    current: &crate::model::TerminalSession,
) -> bool {
    use crate::{agent_execution::HostedSession, model::TerminalContinuity};
    let ended = match &current.continuity {
        TerminalContinuity::Exited { .. } => true,
        TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: Some(_),
        } => recipe == &current.launch,
        _ => false,
    };
    if !ended {
        return false;
    }
    let matches = |original: &crate::model::TerminalSession| {
        let mut original = original.clone();
        original.continuity = current.continuity.clone();
        original == *current
    };
    agent.console.as_ref().is_some_and(matches)
        || matches!(agent.session.as_ref(), Some(HostedSession::Terminal(original)) if matches(original))
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

/// A pane reading the saved transcript of the agent's own provider, lost with
/// an earlier boot, still shows that agent. Grants no process authority.
pub(crate) fn lost_holder_reader_matches(
    session: &crate::chat::Session,
    reader: &crate::chat::Session,
) -> bool {
    crate::chat::is_lost_holder_history(session, reader)
}

pub(crate) fn managed_pane_matches(agent: &crate::agent_execution::Agent, pane: &Pane) -> bool {
    use crate::agent_execution::HostedSession;
    if let CardKind::Terminal(current) = &pane.kind
        && archived_console_matches(agent, current)
    {
        return true;
    }
    if let (CardKind::Chat(reader), Some(HostedSession::Chat(session))) =
        (&pane.kind, &agent.session)
        && lost_holder_reader_matches(session, reader)
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
    /// Stable conversation identity: utilities never displace the first chat.
    pub fn primary_chat(&self) -> Option<CardId> {
        self.panes
            .iter()
            .find(|pane| matches!(pane.kind, CardKind::Chat(_)) && !self.retired.contains(&pane.id))
            .map(|pane| pane.id)
    }

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
        Work,
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
            CardKind::Work => Kind::Work,
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
            Kind::Work => CardKind::Work,
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
    fn a_lost_agents_saved_reader_is_still_its_pane_and_a_stranger_is_not() {
        let (mut agent, native, _) = fixture();
        let crate::agent_execution::HostedSession::Chat(session) = agent.session.as_mut().unwrap()
        else {
            panic!("native fixture")
        };
        session.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
        session.native_continuity =
            crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let mut live = session.proof.clone().unwrap();
        live.boot_id = crate::model::BootId::new("after-restart");
        let reader = session.lost_holder_history(&live).unwrap();
        let mut same_boot = session.clone();
        let session = session.clone();
        let pane = |kind| Pane {
            kind,
            ..native.clone()
        };
        assert!(managed_pane_matches(
            &agent,
            &pane(CardKind::Chat(reader.clone()))
        ));

        let mut other = reader.clone();
        other.launch.session_id = Some("22345678-1234-4234-8234-123456789abc".into());
        assert!(!managed_pane_matches(&agent, &pane(CardKind::Chat(other))));

        // A reader claiming the agent's own boot proves nothing.
        same_boot.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        same_boot.proof.as_mut().unwrap().host_incarnation =
            crate::model::HostIncarnation::new("other-incarnation");
        assert_eq!(session.launch, same_boot.launch);
        assert!(!managed_pane_matches(
            &agent,
            &pane(CardKind::Chat(same_boot))
        ));
    }

    #[test]
    fn attention_explicit_counterpart_activation_preserves_saved_surface_and_proof_fences() {
        let (agent, native, terminal) = fixture();
        let current_native = agent.session.clone().unwrap();
        let current_console = HostedSession::Terminal(agent.console.clone().unwrap());
        for pane in [native.clone(), terminal.clone()] {
            let scope = Scope::Channel(ChannelId::new());
            let mut model = super::super::WorkspaceModel {
                views: vec![View {
                    scope: scope.clone(),
                    revision: 1,
                    panes: vec![pane.clone()],
                    retired: vec![],
                }],
                ..Default::default()
            };
            for requested in [&current_native, &current_console] {
                let display = ManagedDisplayIndex::new(&model)
                    .resolve_activation(&agent, requested)
                    .unwrap();
                assert_eq!(display.scope, scope);
                assert_eq!(
                    display.pane.as_ref(),
                    Some(&pane),
                    "a current counterpart cannot change the saved surface"
                );
            }
            let mut stale = current_native.clone();
            if let HostedSession::Chat(session) = &mut stale {
                session.proof.as_mut().unwrap().host_incarnation =
                    HostIncarnation::new("stale-owner");
            }
            assert!(
                ManagedDisplayIndex::new(&model)
                    .resolve_activation(&agent, &stale)
                    .is_none()
            );
            if let HostedSession::Chat(session) = &mut stale {
                session.proof = None;
            }
            assert!(
                ManagedDisplayIndex::new(&model)
                    .resolve_activation(&agent, &stale)
                    .is_none()
            );
            if let HostedSession::Chat(session) = &mut stale {
                session.proof = Some(HostSessionProof {
                    host_session_id: HostSessionId::new(""),
                    boot_id: BootId::new(""),
                    host_incarnation: HostIncarnation::new(""),
                });
            }
            assert!(
                ManagedDisplayIndex::new(&model)
                    .resolve_activation(&agent, &stale)
                    .is_none()
            );
            let mut unproved_agent = agent.clone();
            unproved_agent.session = Some(stale.clone());
            assert!(
                ManagedDisplayIndex::new(&model)
                    .resolve_activation(&unproved_agent, &stale)
                    .is_none(),
                "matching empty current and requested proofs cannot authorize the proved console pane"
            );
            if let HostedSession::Chat(session) = &mut stale {
                session.proof = None;
            }
            unproved_agent.session = Some(stale.clone());
            assert!(
                ManagedDisplayIndex::new(&model)
                    .resolve_activation(&unproved_agent, &stale)
                    .is_none(),
                "matching absent proofs are equality, not issued live ownership"
            );
            model.views.push(model.views[0].clone());
            assert!(
                ManagedDisplayIndex::new(&model)
                    .resolve_activation(&agent, &current_native)
                    .is_none()
            );
            model.views.truncate(1);
            model.views[0].panes.clear();
            model.views[0].retired.push(pane.id);
            assert!(
                ManagedDisplayIndex::new(&model)
                    .resolve_activation(&agent, &current_console)
                    .is_none()
            );
        }
        let empty = super::super::WorkspaceModel::default();
        let mut native_only = agent.clone();
        native_only.spec.surface = Surface::Terminal;
        native_only.console = None;
        let display = ManagedDisplayIndex::new(&empty)
            .resolve_activation(&native_only, &current_native)
            .unwrap();
        assert_eq!(display.scope, agent.placement);
        assert!(
            display.pane.is_none(),
            "unpublished activation uses exact current request, not historical spec.surface"
        );
        let mut alias = terminal;
        alias.id = CardId::new();
        let aliases = super::super::WorkspaceModel {
            views: vec![View {
                scope: Scope::Channel(ChannelId::new()),
                revision: 1,
                panes: vec![alias],
                retired: vec![],
            }],
            ..Default::default()
        };
        assert!(
            ManagedDisplayIndex::new(&aliases)
                .resolve_activation(&agent, &current_console)
                .is_none(),
            "an unpublished explicit counterpart cannot duplicate an existing inventory alias"
        );
    }

    #[test]
    fn attention_initial_activation_uses_proved_primary_and_preserves_saved_console() {
        let (mut agent, native, terminal) = fixture();
        agent.spec.surface = Surface::Terminal;
        let original = agent.clone();
        let model = super::super::WorkspaceModel::default();
        let id = agent.session.as_ref().unwrap().id();
        let resolved = ManagedDisplayIndex::new(&model)
            .activation_display(&agent, id)
            .unwrap();
        assert!(resolved.display.pane.is_none());
        assert_eq!(
            resolved.session,
            agent.session.clone().unwrap(),
            "typed primary beats historical surface preference and secondary console"
        );
        assert_eq!(agent, original);
        let saved = super::super::WorkspaceModel {
            views: vec![View {
                scope: agent.placement.clone(),
                revision: 1,
                panes: vec![terminal.clone()],
                retired: vec![],
            }],
            ..Default::default()
        };
        let resolved = ManagedDisplayIndex::new(&saved)
            .activation_display(&agent, id)
            .unwrap();
        assert_eq!(resolved.display.pane, Some(terminal.clone()));
        assert_eq!(
            resolved.session,
            HostedSession::Terminal(agent.console.clone().unwrap())
        );
        for incomplete_primary in [true, false] {
            let mut invalid = agent.clone();
            if incomplete_primary {
                if let Some(HostedSession::Chat(session)) = &mut invalid.session {
                    session.proof = None;
                }
            } else {
                let console = invalid.console.as_mut().unwrap();
                console.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: console.launch.clone(),
                    last_exit: None,
                };
            }
            assert!(
                ManagedDisplayIndex::new(&model)
                    .activation_display(&invalid, id)
                    .is_none()
            );
            assert!(
                ManagedDisplayIndex::new(&saved)
                    .activation_display(&invalid, id)
                    .is_none()
            );
        }
        let retired = super::super::WorkspaceModel {
            views: vec![View {
                scope: agent.placement.clone(),
                revision: 2,
                panes: vec![],
                retired: vec![native.id],
            }],
            ..Default::default()
        };
        assert!(
            ManagedDisplayIndex::new(&retired)
                .activation_display(&agent, id)
                .is_none()
        );
        let mut alias = native;
        alias.id = CardId::new();
        let aliases = super::super::WorkspaceModel {
            views: vec![View {
                scope: agent.placement.clone(),
                revision: 1,
                panes: vec![alias],
                retired: vec![],
            }],
            ..Default::default()
        };
        assert!(
            ManagedDisplayIndex::new(&aliases)
                .activation_display(&agent, id)
                .is_none()
        );
        let mut console_only = agent;
        console_only.session = None;
        assert_eq!(
            ManagedDisplayIndex::new(&model)
                .activation_display(&console_only, id)
                .unwrap()
                .session,
            HostedSession::Terminal(console_only.console.unwrap()),
            "only an actually absent primary permits the console-only legacy fallback"
        );
    }

    #[test]
    fn attention_initial_activation_defers_aliases_of_either_shared_uuid_surface() {
        let (agent, native, console) = fixture();
        let requested = agent.session.as_ref().unwrap().id();
        for (mut alias, canonical) in [
            (native.clone(), console.clone()),
            (console.clone(), native.clone()),
        ] {
            alias.id = CardId::new();
            let mut model = super::super::WorkspaceModel {
                views: vec![View {
                    scope: agent.placement.clone(),
                    revision: 1,
                    panes: vec![alias],
                    retired: vec![],
                }],
                ..Default::default()
            };
            assert!(
                ManagedDisplayIndex::new(&model)
                    .activation_display(&agent, requested)
                    .is_none(),
                "an existing secondary alias cannot be treated as an unpublished actor"
            );
            model.views[0].panes.push(canonical.clone());
            assert_eq!(
                ManagedDisplayIndex::new(&model)
                    .activation_display(&agent, requested)
                    .unwrap()
                    .display
                    .pane,
                Some(canonical),
                "the exact canonical surface retains authority beside its counterpart alias"
            );
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
