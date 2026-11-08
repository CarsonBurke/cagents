//! First-send ownership belongs to a pane, not its speculative conversation ID.
use super::*;
use crate::{
    model::{CardId, CardKind},
    terminal_ipc::PaneChatStart,
    workspace_model::{
        ModelEdit,
        views::{Change, Pane},
    },
};

pub(in crate::host) struct Claim {
    token: uuid::Uuid,
    session: Session,
}

pub(super) struct Reservation {
    host: SharedHost,
    pane: CardId,
    token: uuid::Uuid,
}

fn resident_owner(state: &HostState, id: TerminalId) -> bool {
    state
        .chats
        .get(&id)
        .is_some_and(|record| record.handle.owns_work() || record.returning_terminal)
        || state.chat_spawning.contains_key(&id)
        || state.native_actor_startups.contains_key(&id)
        || state.native_closing.contains_key(&id)
        || state.chat_handoffs.contains_key(&id)
        || state.historical_chat_retirements.contains_key(&id)
}

impl Reservation {
    /// The writer orders source changes and claims. Persist the pane's candidate
    /// before any provider work so a restarted daemon can discover its owner.
    pub(super) fn new(
        host: &SharedHost,
        request: &PaneChatStart,
        candidate: &Session,
    ) -> Result<Self, ProtocolError> {
        if !request.source.is_unstarted() || !candidate.is_unstarted() {
            return Err(error(
                "Only an unstarted draft may claim a pane's first send",
            ));
        }
        let writer = crate::host::workspace::writer(host)?;
        let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
        let model = registry.model().map_err(unavailable)?;
        if request.registry != crate::workspace_model::NO_REGISTRY
            && model.identity != request.registry
        {
            return Err(error(
                "The shared workspace changed before this conversation started",
            ));
        }
        let canonical = model.views.iter().find_map(|view| {
            view.panes
                .iter()
                .find(|pane| pane.id == request.card)
                .map(|pane| (view, pane))
        });
        if canonical.is_none()
            && model
                .views
                .iter()
                .any(|view| view.retired.contains(&request.card))
        {
            return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT));
        }
        let current = match canonical {
            Some((_, pane)) => match &pane.kind {
                CardKind::Chat(current) => current,
                _ => return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT)),
            },
            None if request.allow_local => &request.source,
            None => {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::QueueFull,
                    "The shared pane has not been published yet",
                    true,
                ));
            }
        };
        if current != &request.source && current != candidate {
            return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT));
        }
        {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.pane_chat_starts.contains_key(&request.card)
                || (current.id != candidate.id && resident_owner(&state, current.id))
            {
                return Err(error("Chat is already starting"));
            }
        }
        // Resident maps disappear on daemon replacement; a retained provider
        // does not. Errors cannot authorize replacement of that owner either.
        if has_native_owner(current)? {
            return Err(error("This pane still has its original provider owner"));
        }
        let token = uuid::Uuid::new_v4();
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .pane_chat_starts
            .insert(
                request.card,
                Claim {
                    token,
                    session: candidate.clone(),
                },
            );
        let reservation = Self {
            host: host.clone(),
            pane: request.card,
            token,
        };
        if let Some((view, pane)) = canonical {
            let mut desired = pane.clone();
            desired.kind = CardKind::Chat(candidate.clone());
            let edit = ModelEdit::ChangeView {
                scope: view.scope.clone(),
                expected_revision: view.revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before: pane.clone(),
                    pane: desired,
                },
            };
            let applied = crate::host::workspace::apply(&mut registry, &edit)?;
            if !applied
                .model
                .views
                .iter()
                .flat_map(|view| &view.panes)
                .any(|pane| {
                    pane.id == request.card && pane.kind == CardKind::Chat(candidate.clone())
                })
            {
                return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT));
            }
            if applied.changed {
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .announce_model_edit(0, applied.sequence, &edit);
            }
        }
        Ok(reservation)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .pane_chat_starts
            .get(&self.pane)
            .is_some_and(|claim| claim.token == self.token)
        {
            state.pane_chat_starts.remove(&self.pane);
        }
    }
}

fn same_launch(source: &Session, desired: &Session) -> bool {
    source.id == desired.id
        && source.launch.provider == desired.launch.provider
        && source.launch.recipe == desired.launch.recipe
}

pub(in crate::host) fn requires_edit_check(edit: &ModelEdit) -> bool {
    match edit {
        ModelEdit::SeedView { .. } => true,
        ModelEdit::ChangeView {
            change: Change::Insert { .. } | Change::InsertIfEmpty { .. },
            ..
        } => true,
        ModelEdit::ChangeView {
            change: Change::Update { before, pane },
            ..
        } => match (&before.kind, &pane.kind) {
            (CardKind::Chat(before), CardKind::Chat(after)) => !same_launch(before, after),
            (before, after) => before != after,
        },
        _ => false,
    }
}

/// Presentation edits remain independent. Changing a claimed launch, including
/// a same-ID account edit, cannot move the pane away from its live provider.
pub(in crate::host) fn validate_edit(
    host: &SharedHost,
    model: &crate::workspace_model::WorkspaceModel,
    edit: &ModelEdit,
) -> Result<(), ProtocolError> {
    let desired: Vec<&Pane> = match edit {
        ModelEdit::ChangeView {
            change:
                Change::Update { pane, .. }
                | Change::Insert { pane, .. }
                | Change::InsertIfEmpty { pane },
            ..
        } => vec![pane],
        ModelEdit::SeedView { panes, .. } => panes.iter().collect(),
        _ => return Ok(()),
    };
    for pane in desired {
        let current = model
            .views
            .iter()
            .flat_map(|view| &view.panes)
            .find(|current| current.id == pane.id);
        let (claimed, resident) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let claimed = state
                .pane_chat_starts
                .get(&pane.id)
                .map(|claim| claim.session.clone());
            let resident = current.and_then(|current| match &current.kind {
                CardKind::Chat(session) if resident_owner(&state, session.id) => {
                    Some(session.clone())
                }
                _ => None,
            });
            (claimed, resident)
        };
        let explicitly_claimed = claimed.is_some();
        let source = claimed.or(resident).or_else(|| {
            current.and_then(|current| match &current.kind {
                CardKind::Chat(session) => Some(session.clone()),
                _ => None,
            })
        });
        let Some(source) = source else {
            continue;
        };
        // A proved conversation follows the existing explicit resume, account
        // switch and terminal handoff contracts. This fence owns only startup.
        if !explicitly_claimed && !source.is_unstarted() {
            continue;
        }
        if matches!(&pane.kind, CardKind::Chat(desired) if same_launch(&source, desired)) {
            continue;
        }
        let resident = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.pane_chat_starts.contains_key(&pane.id) || resident_owner(&state, source.id)
        };
        if resident || has_native_owner(&source)? {
            // An authenticated actor may enrich its launch with its allocated
            // listener. Only the daemon's exact installed descriptor admits it.
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(&pane.kind, CardKind::Chat(desired) if state.chats.get(&source.id)
                .is_some_and(|record| same_launch(&record.session, desired)))
            {
                continue;
            }
            return Err(error(
                "This pane's conversation is starting or running; attach to it before replacing its source",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::{model::Registry, state::HostLock},
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
        terminal_ipc::ClientRequest,
        workspace_model::views::Scope,
    };

    struct Fixture {
        _directory: tempfile::TempDir,
        host: SharedHost,
        request: PaneChatStart,
        scope: Scope,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let source = Session {
                id: TerminalId::new(),
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
                launch: chat::Launch {
                    provider: Provider::Omp,
                    session_id: None,
                    recipe: LaunchRecipe {
                        program: "/missing-pane-start-provider".into(),
                        arguments: vec![],
                        working_directory: directory.path().into(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                },
            };
            let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
            registry
                .apply(&ModelEdit::OpenProject {
                    repository: directory.path().into(),
                    name: "fixture".into(),
                    kind: Default::default(),
                })
                .unwrap();
            let scope = Scope::Worktree {
                repository: directory.path().into(),
                path: directory.path().into(),
            };
            let card = CardId::new();
            registry
                .apply(&ModelEdit::SeedView {
                    scope: scope.clone(),
                    closed: vec![],
                    panes: vec![Pane {
                        id: card,
                        title: "Draft".into(),
                        user_title: None,
                        pending_terminal_start: false,
                        width_fraction: 0.5,
                        kind: CardKind::Chat(source.clone()),
                        scope: None,
                    }],
                })
                .unwrap();
            let request = PaneChatStart {
                registry: registry.model().unwrap().identity,
                card,
                source,
                allow_local: false,
            };
            let mut state = HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("pane-start"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("host"),
            });
            state.install_model(registry);
            state.register_client(1);
            state.register_client(2);
            Self {
                _directory: directory,
                host: Arc::new(HostLock::new(state)),
                request,
                scope,
            }
        }
        fn candidate(&self) -> Session {
            let mut session = self.request.source.clone();
            session.id = TerminalId::new();
            session
        }
        fn model(&self) -> crate::workspace_model::WorkspaceModel {
            crate::host::workspace::snapshot(&self.host, None).unwrap()
        }
    }

    #[test]
    fn distinct_first_send_candidates_admit_exactly_one_provider() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new();
        let root = fixture._directory.path();
        let executable = root.join("omp");
        std::fs::write(&executable, r#"#!/usr/bin/python3
import json, sys, uuid
identity = str(uuid.uuid4())
with open("launches", "a") as launches:
    launches.write(identity + "\n")
print(json.dumps({"type": "ready"}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    data = {"sessionId": identity, "messages": []}
    print(json.dumps({"type": "response", "id": request["id"], "command": request["type"], "success": True, "data": data}), flush=True)
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        fixture.request.source.launch.recipe.program = executable.to_string_lossy().into_owned();
        let model = fixture.model();
        let before = model.views[0].panes[0].clone();
        let mut pane = before.clone();
        pane.kind = CardKind::Chat(fixture.request.source.clone());
        crate::host::workspace::edit(
            &fixture.host,
            1,
            ModelEdit::ChangeView {
                scope: fixture.scope.clone(),
                expected_revision: model.views[0].revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update { before, pane },
            },
        )
        .unwrap();
        let mut admitted = Vec::new();
        let mut outcomes = Vec::new();
        // Deliver neither Ready response to the windows. Both requests retain
        // the same original source, as two windows may genuinely observe it.
        for client in [1, 2] {
            let outcome = super::super::process(
                &fixture.host,
                client,
                ClientRequest::ChatStartInPane {
                    pane: fixture.request.clone(),
                    session: fixture.candidate(),
                    execution_environment: Default::default(),
                },
            );
            if let Ok(ServerResponse::ChatAttached { session, .. }) = &outcome {
                admitted.push((client, session.id));
            }
            outcomes.push(outcome);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut executed = true;
        while std::fs::read_to_string(root.join("launches"))
            .unwrap_or_default()
            .lines()
            .count()
            < admitted.len()
        {
            if Instant::now() >= deadline {
                executed = false;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let launches = std::fs::read_to_string(root.join("launches")).unwrap_or_default();
        let count = admitted.len();
        let live = fixture
            .host
            .lock()
            .unwrap()
            .chats
            .values()
            .filter(|record| record.handle.owns_work())
            .count();
        let mut close_errors = Vec::new();
        for (client, chat_id) in admitted {
            if let Err(error) =
                super::super::process(&fixture.host, client, ClientRequest::ChatClose { chat_id })
            {
                close_errors.push(error);
            }
        }
        assert!(executed, "admitted providers must execute");
        assert!(
            close_errors.is_empty(),
            "provider cleanup failed: {close_errors:?}"
        );
        assert_eq!(
            count, 1,
            "one pane admitted distinct starts: {outcomes:?}; launches={launches}; live={live}"
        );
        assert_eq!(launches.lines().count(), 1);
        assert_eq!(live, 1);
        assert!(
            outcomes[0].is_ok(),
            "first claimant must start: {:?}",
            outcomes[0]
        );
        assert!(
            outcomes[1].is_err(),
            "the stale second claimant must refuse"
        );
    }

    #[test]
    fn a_peer_cannot_start_a_distinct_provider_in_the_unproved_candidate_gap() {
        let fixture = Fixture::new();
        let first = fixture.candidate();
        let admission = capacity::Reservation::new(&fixture.host, &first).unwrap();
        let claim = Reservation::new(&fixture.host, &fixture.request, &first).unwrap();
        let model = fixture.model();
        let pane = &model.views[0].panes[0];
        assert_eq!(pane.kind, CardKind::Chat(first.clone()));
        assert!(
            first.is_unstarted(),
            "hold before any provider or Ready receipt"
        );
        let mut peer = fixture.request.clone();
        peer.source = first.clone(); // B has seen A's candidate, but no proof.
        for _ in 0..2 {
            let second = fixture.candidate();
            let failure = super::super::process(
                &fixture.host,
                2,
                ClientRequest::ChatStartInPane {
                    pane: peer.clone(),
                    session: second,
                    execution_environment: Default::default(),
                },
            )
            .unwrap_err();
            assert_eq!(
                failure.message, "Chat is already starting",
                "the public dispatcher must refuse before provider initialization"
            );
            assert_eq!(
                fixture.model().views[0].panes[0].kind,
                CardKind::Chat(first.clone())
            );
        }
        // A response dropping its request guard does not end the exact startup.
        drop(claim);
        let second = fixture.candidate();
        assert!(Reservation::new(&fixture.host, &peer, &second).is_err());
        drop(admission);
        assert!(
            Reservation::new(&fixture.host, &peer, &second).is_ok(),
            "confirmed absence permits a new first-send attempt"
        );
    }

    #[test]
    fn shared_edits_preserve_a_claimed_launch_and_allow_presentation_changes() {
        let fixture = Fixture::new();
        let first = fixture.candidate();
        let _admission = capacity::Reservation::new(&fixture.host, &first).unwrap();
        let _claim = Reservation::new(&fixture.host, &fixture.request, &first).unwrap();
        for same_id in [false, true] {
            let model = fixture.model();
            let before = model.views[0].panes[0].clone();
            let mut desired = before.clone();
            let mut replacement = first.clone();
            if same_id {
                replacement
                    .launch
                    .recipe
                    .environment
                    .insert("ACCOUNT".into(), "other".into());
            } else {
                replacement.id = TerminalId::new();
            }
            desired.kind = CardKind::Chat(replacement);
            let failure = crate::host::workspace::edit(
                &fixture.host,
                2,
                ModelEdit::ChangeView {
                    scope: fixture.scope.clone(),
                    expected_revision: model.views[0].revision,
                    operation: uuid::Uuid::new_v4(),
                    change: Change::Update {
                        before,
                        pane: desired,
                    },
                },
            )
            .unwrap_err();
            assert!(
                failure.message.contains("starting or running"),
                "{failure:?}"
            );
        }
        let model = fixture.model();
        let before = model.views[0].panes[0].clone();
        let mut desired = before.clone();
        desired.width_fraction = 0.8;
        desired.title = "Manual title".into();
        let result = crate::host::workspace::edit(
            &fixture.host,
            2,
            ModelEdit::ChangeView {
                scope: fixture.scope.clone(),
                expected_revision: model.views[0].revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before,
                    pane: desired.clone(),
                },
            },
        )
        .unwrap();
        assert_eq!(result.views[0].panes[0], desired);
    }

    #[test]
    fn a_transferred_draft_uses_its_canonical_owner_despite_the_old_tombstone() {
        let fixture = Fixture::new();
        let channel = crate::model::ChannelId::new();
        crate::host::workspace::edit(
            &fixture.host,
            1,
            ModelEdit::CreateChannel {
                id: channel,
                repository: fixture._directory.path().into(),
                title: "Destination".into(),
                title_source: Default::default(),
            },
        )
        .unwrap();
        let model = fixture.model();
        let revision = model
            .views
            .iter()
            .find(|view| view.scope == fixture.scope)
            .unwrap()
            .revision;
        let moved = crate::host::workspace::edit(
            &fixture.host,
            1,
            ModelEdit::ChangeView {
                scope: fixture.scope.clone(),
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Transfer {
                    id: fixture.request.card,
                    destination: Scope::Channel(channel),
                    before: None,
                },
            },
        )
        .unwrap();
        assert!(
            moved
                .views
                .iter()
                .find(|view| view.scope == fixture.scope)
                .unwrap()
                .retired
                .contains(&fixture.request.card)
        );
        let candidate = fixture.candidate();
        let _admission = capacity::Reservation::new(&fixture.host, &candidate).unwrap();
        let _claim = Reservation::new(&fixture.host, &fixture.request, &candidate).unwrap();
        let model = fixture.model();
        let destination = model
            .views
            .iter()
            .find(|view| view.scope == Scope::Channel(channel))
            .unwrap();
        assert_eq!(destination.panes[0].kind, CardKind::Chat(candidate));
    }

    #[test]
    fn local_retention_hints_cannot_bypass_an_existing_canonical_owner() {
        let fixture = Fixture::new();
        let first = fixture.candidate();
        let _admission = capacity::Reservation::new(&fixture.host, &first).unwrap();
        let _claim = Reservation::new(&fixture.host, &fixture.request, &first).unwrap();
        let mut peer = fixture.request.clone();
        peer.source = first;
        peer.allow_local = true;
        assert!(Reservation::new(&fixture.host, &peer, &fixture.candidate()).is_err());
    }

    #[test]
    fn an_explicitly_local_draft_can_start_without_a_shared_descriptor() {
        let fixture = Fixture::new();
        let mut local = fixture.request.clone();
        local.card = CardId::new();
        let first = fixture.candidate();
        let missing = Reservation::new(&fixture.host, &local, &first)
            .err()
            .unwrap();
        assert_eq!(missing.code, ProtocolErrorCode::QueueFull);
        assert!(missing.retryable);
        local.allow_local = true;
        let _admission = capacity::Reservation::new(&fixture.host, &first).unwrap();
        let _claim = Reservation::new(&fixture.host, &local, &first).unwrap();
        assert!(Reservation::new(&fixture.host, &local, &fixture.candidate()).is_err());
        assert!(
            !fixture
                .model()
                .views
                .iter()
                .flat_map(|view| &view.panes)
                .any(|pane| pane.id == local.card)
        );
    }

    #[test]
    fn simultaneous_first_sends_compete_for_one_pane_before_provider_work() {
        let fixture = Fixture::new();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let host = fixture.host.clone();
                let request = fixture.request.clone();
                let candidate = fixture.candidate();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    let _admission = capacity::Reservation::new(&host, &candidate).unwrap();
                    barrier.wait();
                    let claim = Reservation::new(&host, &request, &candidate);
                    barrier.wait(); // retain the winning claim until both have answered
                    claim.is_ok()
                })
            })
            .collect();
        let winners = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }
}
