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
    state.is_live(&id)
}

fn binding_owns_work(
    host: &SharedHost,
    binding: &crate::host::model::PaneChatBinding,
    candidate: Option<TerminalId>,
) -> Result<bool, ProtocolError> {
    if candidate != Some(binding.current)
        && resident_owner(
            &host.lock().unwrap_or_else(|p| p.into_inner()),
            binding.current,
        )
    {
        return Ok(true);
    }
    if binding.current == binding.candidate.id {
        return has_native_owner(&binding.candidate);
    }
    // Typed succession may have moved this pane to a terminal allocation.
    // Its ID is recorded by the same transaction as Registry::succeed.
    use crate::daemon::holder_registry::{self as registry, Kind};
    for kind in [Kind::Native, Kind::Terminal] {
        let directory = registry::directory(kind).map_err(unavailable)?;
        if registry::recovery_candidate(&registry::socket(&directory, binding.current), kind)
            .map_err(unavailable)?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// An older typed request may still be in flight when a pane accepts a newer
/// first send. Its capacity reservation and the writer make that race atomic.
pub(super) fn validate_known_allocation(
    host: &SharedHost,
    session: &Session,
) -> Result<(), ProtocolError> {
    let writer = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .workspace_registry
            .clone()
            .or_else(|| state.execution_registry.clone())
    };
    // Standalone legacy hosts need no pane namespace. Once present, registry
    // failures are refusals rather than permission to revive an old allocation.
    let Some(writer) = writer else { return Ok(()) };
    let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    if !registry
        .pane_chat_allocation_is_current(session.id)
        .map_err(unavailable)?
    {
        return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT));
    }
    Ok(())
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
        let binding = registry
            .pane_chat_start(request.card)
            .map_err(unavailable)?;
        if binding
            .as_ref()
            .is_some_and(|binding| binding.current != binding.candidate.id)
        {
            return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT));
        }
        let current = match canonical {
            Some((_, pane)) => match &pane.kind {
                CardKind::Chat(current) => current,
                _ => return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT)),
            },
            None if request.allow_local => binding
                .as_ref()
                .map(|binding| &binding.candidate)
                .unwrap_or(&request.source),
            None => {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::QueueFull,
                    "The shared pane has not been published yet",
                    true,
                ));
            }
        };
        let eligible = if let (None, Some(binding)) = (canonical, binding.as_ref()) {
            // After a failed start, provider/account/permission choices may
            // change this accepted ID's recipe. Check the saved launch's owner
            // below before permitting that same draft's next candidate.
            binding.current == request.source.id
                || (binding.current == candidate.id && binding.candidate == *candidate)
        } else {
            current == &request.source || current == candidate
        };
        if !eligible {
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
        if let Some(binding) = &binding
            && binding_owns_work(host, binding, Some(candidate.id))?
        {
            return Err(error("This pane already has a provider owner"));
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
        if !registry
            .bind_pane_chat_start(request.card, binding.as_ref(), candidate)
            .map_err(unavailable)?
        {
            return Err(error(crate::host::model::Registry::VIEW_SOURCE_CONFLICT));
        }
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
    registry: &crate::host::model::Registry,
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
        let binding = registry.pane_chat_start(pane.id).map_err(unavailable)?;
        let desired_id = match &pane.kind {
            CardKind::Chat(session) => Some(session.id),
            CardKind::Terminal(session) => Some(session.id),
            _ => None,
        };
        if let Some(binding) = &binding
            && desired_id != Some(binding.current)
            && (binding.current != binding.candidate.id || binding_owns_work(host, binding, None)?)
        {
            return Err(error(
                "This pane's conversation is starting or running; attach to it before replacing its source",
            ));
        }
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
        let source = claimed
            .or(resident)
            .or_else(|| {
                current.and_then(|current| match &current.kind {
                    CardKind::Chat(session) => Some(session.clone()),
                    _ => None,
                })
            })
            .or_else(|| {
                binding
                    .as_ref()
                    .filter(|binding| binding.current == binding.candidate.id)
                    .map(|binding| binding.candidate.clone())
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
        real_provider_admission(false);
    }

    #[test]
    fn copied_local_first_send_candidates_admit_exactly_one_provider() {
        real_provider_admission(true);
    }

    fn real_provider_admission(local: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new();
        if local {
            fixture.request.card = CardId::new();
            fixture.request.allow_local = true;
        }
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
        if !local {
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
        }
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
    fn copied_local_draft_cannot_replace_the_winner_after_request_or_daemon_ends() {
        let fixture = Fixture::new();
        let mut local = fixture.request.clone();
        local.card = CardId::new();
        local.allow_local = true;
        let first = fixture.candidate();
        let admission = capacity::Reservation::new(&fixture.host, &first).unwrap();
        let claim = Reservation::new(&fixture.host, &local, &first).unwrap();
        drop(claim);
        assert!(
            Reservation::new(&fixture.host, &local, &fixture.candidate()).is_err(),
            "a copied old draft must remain fenced after the request returns"
        );
        let mut retry = local.clone();
        retry.source = first.clone();
        retry
            .source
            .launch
            .recipe
            .environment
            .insert("MODEL".into(), "different".into());
        let mut next = retry.source.clone();
        next.id = TerminalId::new();
        assert!(
            Reservation::new(&fixture.host, &retry, &next).is_err(),
            "edited options cannot evade the original candidate's owner"
        );
        drop(admission);
        drop(Reservation::new(&fixture.host, &retry, &next).unwrap());
        let mut proof = fixture.host.lock().unwrap().proof().clone();
        proof.host_incarnation = HostIncarnation::new("replacement");
        let mut replacement = HostState::new(proof);
        replacement
            .install_model(Registry::at(&fixture._directory.path().join("model.sqlite3")).unwrap());
        let replacement = Arc::new(HostLock::new(replacement));
        assert!(
            Reservation::new(&replacement, &local, &fixture.candidate()).is_err(),
            "daemon replacement must not forget which local candidate won"
        );
        let mut retry_after_restart = local;
        retry_after_restart.source = next;
        let candidate = fixture.candidate();
        drop(Reservation::new(&replacement, &retry_after_restart, &candidate).unwrap());
    }

    #[test]
    fn publishing_a_local_pane_cannot_displace_its_live_winner() {
        let fixture = Fixture::new();
        let mut local = fixture.request.clone();
        local.card = CardId::new();
        local.allow_local = true;
        let first = fixture.candidate();
        let _admission = capacity::Reservation::new(&fixture.host, &first).unwrap();
        drop(Reservation::new(&fixture.host, &local, &first).unwrap());
        let pane = |session| Pane {
            id: local.card,
            title: "Draft".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Chat(session),
            scope: None,
        };
        let seed = |session| ModelEdit::SeedView {
            scope: fixture.scope.clone(),
            closed: vec![],
            panes: vec![pane(session)],
        };
        assert!(
            crate::host::workspace::edit(&fixture.host, 2, seed(local.source.clone())).is_err()
        );
        crate::host::workspace::edit(&fixture.host, 1, seed(first.clone())).unwrap();
        let mut shared = local;
        shared.allow_local = false;
        assert!(Reservation::new(&fixture.host, &shared, &fixture.candidate()).is_err());
    }

    #[test]
    fn typed_succession_fences_both_stale_drafts_and_delayed_legacy_starts() {
        let fixture = Fixture::new();
        let mut local = fixture.request.clone();
        local.card = CardId::new();
        local.allow_local = true;
        let first = fixture.candidate();
        drop(Reservation::new(&fixture.host, &local, &first).unwrap());
        let successor = TerminalId::new();
        crate::host::workspace::writer(&fixture.host)
            .unwrap()
            .lock()
            .unwrap()
            .succeed(successor, first.id, None, |_| false)
            .unwrap();
        let retried = TerminalId::new();
        crate::host::workspace::writer(&fixture.host)
            .unwrap()
            .lock()
            .unwrap()
            .succeed(retried, first.id, None, |_| false)
            .unwrap();
        let mut stale = local;
        stale.source = first.clone();
        assert!(Reservation::new(&fixture.host, &stale, &fixture.candidate()).is_err());
        assert!(validate_known_allocation(&fixture.host, &first).is_err());
        let mut current = first;
        current.id = retried;
        assert!(validate_known_allocation(&fixture.host, &current).is_ok());
        let advanced = TerminalId::new();
        assert!(
            crate::host::workspace::writer(&fixture.host)
                .unwrap()
                .lock()
                .unwrap()
                .succeed(advanced, current.id, None, |_| false)
                .is_ok()
        );
        let competing = TerminalId::new();
        let mut pending = fixture.candidate();
        pending.id = advanced;
        fixture
            .host
            .lock()
            .unwrap()
            .historical_chat_retirements
            .insert(advanced, pending);
        assert!(
            crate::host::workspace::writer(&fixture.host)
                .unwrap()
                .lock()
                .unwrap()
                .succeed(competing, stale.source.id, None, |members| fixture
                    .host
                    .lock()
                    .unwrap()
                    .any_other_live(members, stale.source.id))
                .is_err()
        );
        stale.source = current;
        assert!(
            Reservation::new(&fixture.host, &stale, &fixture.candidate()).is_err(),
            "typed resume/account/terminal operations keep their own lifecycle; a raw copy is not a fresh draft"
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
