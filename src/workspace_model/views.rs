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
    /// UI removal consent is tied to this registry and original pane source.
    /// This additive wire variant requires a newly allocated matched protocol.
    RemoveSource {
        registry: uuid::Uuid,
        repository: PathBuf,
        before: Pane,
        execution_instance: Option<uuid::Uuid>,
        /// Future matched wire leaf; validated against host-issued retained evidence.
        handoff: Option<crate::terminal::ipc::TerminalHandoffReceipt>,
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
            Change::RemoveSource { before, handoff, .. } => {
                let matches = self
                    .panes
                    .iter()
                    .filter(|pane| pane.id == before.id)
                    .collect::<Vec<_>>();
                if matches.is_empty() {
                    return Ok(false);
                }
                if matches.len() != 1 || !same_removal_pane_source(before, matches[0], handoff.as_ref()) {
                    return Err("workspace view removal source conflict".into());
                }
                self.panes.retain(|pane| pane.id != before.id);
                if !self.retired.contains(&before.id) {
                    self.retired.push(before.id);
                }
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

/// Display edits can be independent; execution source, startup and placement
/// must remain exactly the source that the removal request captured.
pub fn same_pane_source(expected: &Pane, current: &Pane) -> bool {
    expected.id == current.id
        && same_card_source(&expected.kind, &current.kind)
        && expected.pending_terminal_start == current.pending_terminal_start
        && expected.scope == current.scope
}

/// An authenticated handoff may advance only the execution kind of the same
/// original pane. Registry/revision and retained receipt authority are checked
/// atomically by the host; this pure predicate is shared by projection/retry.
pub fn same_removal_pane_source(expected:&Pane,current:&Pane,handoff:Option<&crate::terminal::ipc::TerminalHandoffReceipt>)->bool {
    let Some(receipt)=handoff else {return same_pane_source(expected,current);};
    if expected.id!=current.id || expected.scope!=current.scope
        || expected.pending_terminal_start!=current.pending_terminal_start
        || expected.pending_terminal_start || !handoff_source_matches(expected,receipt) { return false; }
    same_card_source(&expected.kind,&current.kind)
        || same_card_source(&CardKind::Chat(receipt.source.clone()),&current.kind)
        || same_card_source(&CardKind::Terminal(receipt.terminal.session.clone()),&current.kind)
}
/// Only an unknown issued thread may learn the host-confirmed thread. A known
/// thread, recipe, proof, paired console or listener is never substituted.
pub fn handoff_source_matches(expected:&Pane,receipt:&crate::terminal::ipc::TerminalHandoffReceipt)->bool {
    let CardKind::Chat(issued)=&expected.kind else {return false;};
    !receipt.operation.is_nil() && !receipt.source_execution_instance.is_nil()
        && !receipt.terminal_native_epoch.is_nil()
        && !receipt.source_binding.backend.is_nil()
        && receipt.source_binding.execution_instance==receipt.source_execution_instance
        && handoff_chat_source_matches(&receipt.source_binding.source,&receipt.source)
        && !receipt.terminal.execution_instance.is_nil()
        && receipt.source.proof.as_ref().is_some_and(crate::model::HostSessionProof::is_complete)
        && receipt.terminal.session.continuity.live_proof().is_none_or(|proof|receipt.source.proof.as_ref()==Some(&proof))
        && handoff_chat_source_matches(issued,&receipt.source)
}
/// Receipt-owned thread learning is narrower than ordinary logical-source
/// matching. Known thread IDs and all other issued source fields remain exact.
pub fn handoff_chat_source_matches(expected:&crate::chat::Session,confirmed:&crate::chat::Session)->bool {
    let mut confirmed=confirmed.clone();
    if expected.launch.session_id.is_none() {confirmed.launch.session_id=None;}
    same_card_source(&CardKind::Chat(expected.clone()),&CardKind::Chat(confirmed))
}

/// Logical source equality permits the same execution to finish and the same
/// conversation to retain/adopt its native backend. Per-worker teardown still
/// requires the separate authenticated execution-instance receipt.
pub fn same_card_source(expected: &CardKind, current: &CardKind) -> bool {
    match (expected, current) {
        (CardKind::Terminal(expected), CardKind::Terminal(current)) => {
            let mut source = current.clone();
            source.continuity = expected.continuity.clone();
            source == *expected
                && current
                    .continuity
                    .live_proof()
                    .is_none_or(|proof| expected.continuity.live_proof() == Some(proof))
        }
        (CardKind::Chat(expected), CardKind::Chat(current)) => {
            let mut source = current.clone();
            source.native_continuity = expected.native_continuity.clone();
            source == *expected
        }
        _ => shared_kind(expected) == shared_kind(current),
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
mod removal_source_tests {
    use super::*;
    #[test]
    fn logical_chat_removal_preserves_thread_pair_proof_and_configuration() {
        let launch = crate::model::LaunchRecipe {
            program: "fixture".into(),
            arguments: vec![],
            working_directory: "/repo".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let session = crate::chat::Session {
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: launch,
                session_id: Some("original-thread".into()),
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let before = CardKind::Chat(session.clone());
        let mut retained = session.clone();
        retained.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "restarted".into(),
        };
        assert!(same_card_source(&before, &CardKind::Chat(retained.clone())));
        retained.launch.session_id = Some("replacement-thread".into());
        assert!(!same_card_source(&before, &CardKind::Chat(retained)));
        let mut paired = session.clone();
        paired.terminal = Some(crate::model::TerminalSession {
            id: crate::model::TerminalId::new(),
            launch: session.launch.recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.recipe.clone(),
                last_exit: None,
            },
        });
        assert!(!same_card_source(&before, &CardKind::Chat(paired)));
    }
}

#[cfg(test)]
pub(crate) mod handoff_tests {
    use super::*;
    use crate::{model::{BootId,HostSessionId,HostIncarnation,LaunchRecipe,TerminalId,TerminalContinuity},terminal::ipc::{TerminalDescriptor,TerminalHandoffReceipt,ChatSourceBinding}};
    pub(crate) fn fixture()->(Pane,TerminalHandoffReceipt) {
        let recipe=LaunchRecipe {program:"fixture".into(),arguments:vec![],working_directory:"/repo".into(),environment:Default::default(),unset_environment:Default::default()};
        let mut terminal=TerminalDescriptor::relaunchable(TerminalId::new(),recipe.clone(),None);
        terminal.execution_instance=uuid::Uuid::new_v4();
        terminal.session.continuity=TerminalContinuity::Live {host_session_id:HostSessionId::new("host"),boot_id:BootId::new("boot"),host_incarnation:HostIncarnation::new("incarnation")};
        let chat=crate::chat::Session {id:TerminalId::new(),launch:crate::chat::Launch {provider:crate::agent::Provider::Claude,recipe,session_id:None},proof:terminal.session.continuity.live_proof(),terminal:None,owned_codex_listener:None,native_continuity:Default::default()};
        let before=Pane {id:CardId::new(),title:"Chat".into(),user_title:None,width_fraction:1.0,pending_terminal_start:false,kind:CardKind::Chat(chat.clone()),scope:None};
        let mut confirmed=chat;confirmed.launch.session_id=Some("confirmed".into());
        let execution_instance=uuid::Uuid::new_v4();
        let receipt=TerminalHandoffReceipt {operation:uuid::Uuid::new_v4(),source:confirmed.clone(),source_execution_instance:execution_instance,source_binding:ChatSourceBinding {source:confirmed,execution_instance,backend:uuid::Uuid::new_v4()},retained_binding:None,terminal_native_epoch:uuid::Uuid::new_v4(),terminal};
        (before,receipt)
    }
    #[test]
    fn original_chat_basis_accepts_only_its_authenticated_returned_kind() {
        let (before,receipt)=fixture();let original=before.clone();
        let mut current=before.clone();current.kind=CardKind::Terminal(receipt.terminal.session.clone());
        assert!(!same_pane_source(&before,&current));
        assert!(same_removal_pane_source(&before,&current,Some(&receipt)));
        let mut view=View::empty(Scope::Worktree {repository:"/repo".into(),path:"/repo".into()});view.panes.push(current.clone());
        view.apply(&Change::RemoveSource {registry:uuid::Uuid::new_v4(),repository:"/repo".into(),before:before.clone(),execution_instance:Some(receipt.terminal.execution_instance),handoff:Some(receipt.clone())}).unwrap();
        assert!(view.panes.is_empty());assert_eq!(before,original);
        let mut learned=before.clone();learned.kind=CardKind::Chat(receipt.source.clone());
        assert!(same_removal_pane_source(&before,&learned,Some(&receipt)));
        let mut ended_receipt=receipt.clone();ended_receipt.terminal.session.continuity=TerminalContinuity::Exited {last_exit:crate::model::TerminalExit {code:Some(0),signal:None,occurred_at_unix_ms:0}};
        let mut ended=current.clone();ended.kind=CardKind::Terminal(ended_receipt.terminal.session.clone());
        assert!(same_removal_pane_source(&before,&ended,Some(&ended_receipt)),"completed receipt retains source proof without synthesizing Live");
        assert!(matches!(ended_receipt.terminal.session.continuity,TerminalContinuity::Exited {..}));
        let mut promoted=receipt.clone();promoted.source.native_continuity=crate::chat::NativeContinuity::Durable {provider_version:"same-backend".into()};
        assert!(same_removal_pane_source(&before,&current,Some(&promoted)),"metadata promotion does not replace the concrete source backend");
        promoted.source_binding.source.launch.session_id=Some("other-known-thread".into());
        assert!(!same_removal_pane_source(&before,&current,Some(&promoted)),"known backend conversation remains exact");
        for variant in 0..8 {
            let mut changed=current.clone();let mut proof=receipt.clone();
            match variant {
                0=>changed.id=CardId::new(),
                1=>changed.pending_terminal_start=true,
                2=>if let CardKind::Terminal(source)=&mut changed.kind {source.id=TerminalId::new();},
                3=>proof.source.launch.recipe.program="foreign".into(),
                4=>proof.source_binding.backend=uuid::Uuid::nil(),
                5=>proof.terminal.execution_instance=uuid::Uuid::nil(),
                6=>proof.terminal_native_epoch=uuid::Uuid::nil(),
                _=>if let CardKind::Terminal(source)=&mut changed.kind {source.launch.working_directory="/foreign".into();},
            }
            assert!(!same_removal_pane_source(&before,&changed,Some(&proof)),"variant {variant}");
        }
        let mut known=before;let CardKind::Chat(source)=&mut known.kind else {panic!()};source.launch.session_id=Some("other-conversation".into());
        assert!(!same_removal_pane_source(&known,&current,Some(&receipt)));
    }
}
