//! A relaunched window that views a shared project must qualify every saved
//! terminal it hosts off screen, without anyone opening those panes (#2028).
use super::*;
use crate::{
    app::workspace_model_sync::ModelUpdate,
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, TerminalDescriptor, read_frame, write_frame,
    },
    terminal_ipc::TerminalVisibility,
};
use std::os::unix::net::UnixListener;

fn replacement_proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("replacement-host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("replacement-incarnation"),
    }
}

fn descriptor(session: &TerminalSession) -> TerminalDescriptor {
    TerminalDescriptor {
        screen_lost_after_crash: false,
        session: session.clone(),
        generation: 0,
        event_sequence: 0,
        activity: None,
        agent_activity: None,
        upgrade_sources: Vec::new(),
        upgrade_relaunches: Vec::new(),
        title: None,
        oldest_event_sequence: 1,
        attached: true,
        transcript: None,
    }
}

/// A replacement daemon hosting `hosted`, answering one client connection.
fn serve(mut stream: std::os::unix::net::UnixStream, hosted: Vec<TerminalSession>) {
    let proof = replacement_proof();
    std::thread::spawn(move || {
        while let Ok(Some(request)) = read_frame::<ClientEnvelope>(&mut stream) {
            let response = match request.request {
                ClientRequest::Connect { .. } => ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: Vec::new(),
                },
                ClientRequest::List => ServerResponse::Listed {
                    terminals: hosted.iter().map(descriptor).collect(),
                },
                ClientRequest::Attach { terminal_id, .. } => ServerResponse::Attached {
                    terminal: descriptor(
                        hosted
                            .iter()
                            .find(|session| session.id == terminal_id)
                            .expect("only a hosted terminal is attached"),
                    ),
                },
                _ => ServerResponse::Accepted,
            };
            let reply = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: proof.clone(),
                message: ServerMessage::Response { response },
            };
            if write_frame(&mut stream, &reply).is_err() {
                break;
            }
        }
    });
}

/// `saved` as the replacement daemon restored it: the same terminal under
/// that daemon's proof.
fn rehosted(saved: &TerminalSession) -> TerminalSession {
    let proof = replacement_proof();
    let mut hosted = saved.clone();
    hosted.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    hosted
}

/// What the attach-only path hands back once the replacement daemon accepted
/// `saved`'s exact terminal: a confirmed attachment under that daemon's proof.
fn attached(directory: &Path, saved: &TerminalSession) -> StartedTerminal {
    let socket = directory.join(format!("{}.sock", saved.id));
    let listener = UnixListener::bind(&socket).unwrap();
    let hosted = rehosted(saved);
    let served = hosted.clone();
    std::thread::spawn(move || serve(listener.accept().unwrap().0, vec![served]));
    let handle = crate::terminal::HostedTerminalHandle::connect_at(
        socket,
        saved.id,
        Some(replacement_proof()),
        |_| {},
    )
    .unwrap();
    handle.wait_until_attached(Duration::from_secs(5)).unwrap();
    StartedTerminal {
        handle: TerminalSurface::from(handle),
        session: hosted,
        spawned: false,
    }
}

fn live_terminal() -> TerminalSession {
    let mut session =
        fresh_terminal_session(LaunchRecipe::shell("/fixture-project".into()).unwrap());
    session.continuity = TerminalContinuity::Live {
        host_session_id: HostSessionId::new("previous-host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("previous-incarnation"),
    };
    session
}

fn offscreen(session: &TerminalSession) -> CardState {
    CardState {
        id: CardId::new(),
        generation: 0,
        title: "Terminal".into(),
        width_fraction: 1.0,
        existence: Existence::Open,
        exposure: Exposure::Offscreen,
        residency: Residency::Hibernated,
        kind: CardKind::Terminal(session.clone()),
    }
}

struct Window {
    app: Cagents,
    active: ChannelId,
    hidden: ChannelId,
    /// Saved by this window before the update.
    cached: CardState,
    /// Known only to the registry, so the window first projects it on admission.
    projected: Option<CardState>,
}

impl Window {
    fn terminals(&self) -> Vec<CardState> {
        std::iter::once(&self.cached)
            .chain(&self.projected)
            .cloned()
            .collect()
    }

    fn projected(&self) -> CardId {
        self.projected.as_ref().unwrap().id
    }
}

/// A window reopened by an update whose two saved terminals sit in a channel
/// it is not showing. Nothing is on screen, so no start of a card's own can
/// release or carry the attach queue.
fn reopened_window(with_projected: bool) -> Window {
    let (mut app, active, hidden, _) = fixture();
    let identity = app.workspace_model.identity;
    let cached = offscreen(&live_terminal());
    let projected = with_projected.then(|| offscreen(&live_terminal()));
    let durable = Arc::make_mut(&mut app.durable);
    durable.workspace_model_identity = Some(identity);
    durable.active_channel = Some(active);
    durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == hidden)
        .unwrap()
        .cards = vec![cached.id];
    durable.cards = vec![cached.clone()];
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    app.startup_gate = crate::app::startup_gate::State::fixture_opened();
    drop(app.initialize_runtimes());
    // What opening the window does for the gate.
    drop(app.qualify_startup_terminals());
    Window {
        app,
        active,
        hidden,
        cached,
        projected,
    }
}

/// Drive the actual initial-model and SeedView acknowledgement reducers.
fn admit(window: &mut Window) {
    let pane = |card: &CardState| Pane::from_card(card, None).unwrap();
    let mut view = View::empty(Scope::Channel(window.hidden));
    view.panes = window.terminals().iter().map(pane).collect();
    let model = crate::workspace_model::WorkspaceModel {
        identity: window.app.durable.workspace_model_identity.unwrap(),
        sequence: 1,
        channels: [window.active, window.hidden]
            .into_iter()
            .map(|id| crate::workspace_model::ModelChannel {
                id,
                repository: PathBuf::from("/fixture-project"),
                title: "view".into(),
                title_source: Default::default(),
                ordinal: 1,
            })
            .collect(),
        views: vec![View::empty(Scope::Channel(window.active)), view],
        ..Default::default()
    };
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Connected(model.clone())),
    );
    for channel in [window.active, window.hidden] {
        drop(
            window
                .app
                .apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: Scope::Channel(channel),
                    operation: None,
                    model: model.clone(),
                }),
        );
    }
    assert!(window.app.shared_views_ready_for_startup());
}

fn session(card: &CardState) -> TerminalSession {
    let CardKind::Terminal(session) = &card.kind else {
        unreachable!("terminal fixture")
    };
    session.clone()
}

const REFUSAL: &str =
    "a different daemon owns this terminal ID; preserving that session instead of retargeting it";

/// Run gate checks, completing each outstanding attach the way the daemon
/// would. The projected pane attaches; the cached one attaches unless
/// `refuse_cached`.
fn run_gate(window: &mut Window, directory: &Path, refuse_cached: bool) {
    for _ in 0..6 {
        window.app.startup_gate.expire_attach_retries();
        drop(window.app.update(Message::VerifyStartupPanes));
        complete_attaches(window, directory, refuse_cached);
    }
}

/// Completes each outstanding attach the way the daemon would, in turn.
fn complete_attaches(window: &mut Window, directory: &Path, refuse_cached: bool) {
    for _ in 0..4 {
        for card in &window.terminals() {
            if !window.app.terminal_start_in_flight(card.id) {
                continue;
            }
            let saved = session(card);
            let terminal = saved.id;
            let result = if refuse_cached && card.id == window.cached.id {
                Err(REFUSAL.to_owned())
            } else {
                Ok(Some(attached(directory, &saved)))
            };
            drop(window.app.update(Message::RestoredAgentAttached {
                card: card.id,
                terminal,
                result,
            }));
        }
    }
}

fn assert_hibernated_attachment(app: &Cagents, card: CardId) {
    match app.cards.get(&card) {
        Some(CardRuntime::Terminal(surface)) => {
            assert_eq!(surface.visibility(), TerminalVisibility::Hibernated);
            assert!(
                surface.snapshot().detached,
                "a hibernated view streams no screen"
            );
        }
        _ => panic!("the offscreen terminal was never attached"),
    }
    assert_eq!(
        app.lifecycle.state(card),
        Some((&Exposure::Offscreen, &Residency::Hibernated))
    );
}

/// The primary window's outcome: its offscreen terminals did attach, but the
/// gate read their hibernated snapshot as a failed attachment.
#[test]
fn reopened_shared_window_qualifies_offscreen_terminals_without_opening_them() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(true);
    admit(&mut window);
    assert!(
        matches!(
            window.app.cards.get(&window.projected()),
            Some(CardRuntime::Loading)
        ),
        "the registry's pane is projected for this window"
    );
    run_gate(&mut window, directory.path(), false);
    let app = &window.app;
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert!(app.restored_agent_attaches.is_empty());
    for card in [window.cached.id, window.projected()] {
        assert_hibernated_attachment(app, card);
    }
    assert_eq!(app.startup_pane_check(), Ok(true));
}

/// The relaunched windows' outcome: an offscreen attach that ended detached
/// left its pane loading with nothing retrying it, so the gate waited until
/// someone opened the pane or its budget ran out.
#[test]
fn reopened_shared_window_reports_a_refused_offscreen_attach() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(true);
    admit(&mut window);
    run_gate(&mut window, directory.path(), true);
    let app = &window.app;
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert!(app.restored_agent_attaches.is_empty());
    assert_hibernated_attachment(app, window.projected());
    assert!(matches!(
        app.cards.get(&window.cached.id),
        Some(CardRuntime::Loading)
    ));
    let report = app.startup_pane_check().unwrap_err();
    assert!(
        report.starts_with("recovery-incomplete: 1 saved panes"),
        "{report}"
    );
    assert!(report.contains(&window.cached.id.to_string()), "{report}");
    assert!(report.contains(REFUSAL), "{report}");
    assert_eq!(
        app.card_state(window.cached.id).unwrap().kind,
        window.cached.kind,
        "a refused attach keeps the saved source"
    );
}

#[test]
fn shared_offscreen_terminal_waits_for_registry_admission_before_attaching() {
    let mut window = reopened_window(true);
    for _ in 0..3 {
        drop(window.app.update(Message::VerifyStartupPanes));
    }
    assert_eq!(
        window.app.terminal_starts_in_flight(),
        0,
        "opening the card would not attach a cached shared descriptor either"
    );
    admit(&mut window);
    drop(window.app.update(Message::VerifyStartupPanes));
    assert_eq!(window.app.terminal_starts_in_flight(), 1);
    assert!(
        [window.cached.id, window.projected()]
            .iter()
            .any(|card| window.app.terminal_start_in_flight(*card))
    );
}

/// A new session on the same card is a new owner to qualify, not the
/// outcome recorded for the session it replaced.
#[test]
fn a_replaced_shared_session_is_qualified_again() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(true);
    admit(&mut window);
    // Spend the cached pane's attempts before a gate check reports them.
    for _ in 0..3 {
        window.app.startup_gate.expire_attach_retries();
        drop(window.app.qualify_startup_terminals());
        complete_attaches(&mut window, directory.path(), true);
    }
    assert!(window.app.startup_pane_check().is_err());
    let mut replacement = session(&window.cached);
    replacement.id = TerminalId::new();
    // A replacement is authoritative only when the model actually publishes
    // it; changing just the local descriptor would leave an unadmitted owner.
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let view = model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(window.hidden))
        .unwrap();
    view.revision += 1;
    view.panes
        .iter_mut()
        .find(|pane| pane.id == window.cached.id)
        .unwrap()
        .kind = CardKind::Terminal(replacement.clone());
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Read(model)),
    );
    assert_eq!(window.app.startup_pane_check(), Ok(false));
    drop(window.app.update(Message::VerifyStartupPanes));
    assert!(window.app.terminal_start_in_flight(window.cached.id));
}

/// A terminal the daemon has only just restored may refuse the first
/// attempt. The same attach-only path tries again after a pause, and the gate
/// reports the refusal once those attempts are spent.
#[test]
fn a_detached_attach_is_retried_after_a_pause_and_then_reported() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    admit(&mut window);
    let mut attempts = 0;
    for expected in 1..=3 {
        drop(window.app.update(Message::VerifyStartupPanes));
        assert!(window.app.terminal_start_in_flight(window.cached.id));
        attempts += 1;
        complete_attaches(&mut window, directory.path(), true);
        if expected < 3 {
            assert_eq!(window.app.startup_pane_check(), Ok(false));
            drop(window.app.update(Message::VerifyStartupPanes));
            assert!(
                !window.app.terminal_start_in_flight(window.cached.id),
                "no retry before its pause"
            );
            window.app.startup_gate.expire_attach_retries();
        }
    }
    assert_eq!(attempts, 3);
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(REFUSAL), "{report}");
    drop(window.app.update(Message::VerifyStartupPanes));
    assert_eq!(
        window.app.terminal_starts_in_flight(),
        0,
        "reported, not retried"
    );
}

/// The attach-only path for a shared pane finds that exact hosted terminal,
/// under the daemon's current proof, as opening the card does.
#[test]
fn a_shared_pane_attaches_its_exact_hosted_terminal_under_the_current_proof() {
    let saved = live_terminal();
    let mut other = live_terminal();
    other.launch = saved.launch.clone();
    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    serve(server, vec![rehosted(&other), rehosted(&saved)]);
    let mut client = crate::control::ControlClient::from_stream(client).unwrap();
    let (recovered, mode) = crate::app::hosted_shared_terminal(&mut client, saved.id).unwrap();
    assert_eq!(recovered.session, rehosted(&saved));
    assert_eq!(recovered.expected_host, Some(replacement_proof()));
    assert!(recovered.recovery_lease.is_none());
    assert!(!recovered.attach_completed);
    assert!(
        matches!(mode, crate::app::TerminalStartMode::Attach(proof) if proof == replacement_proof())
    );

    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    serve(server, vec![rehosted(&other)]);
    let mut client = crate::control::ControlClient::from_stream(client).unwrap();
    assert_eq!(
        crate::app::hosted_shared_terminal(&mut client, saved.id)
            .err()
            .as_deref(),
        Some(crate::app::SHARED_TERMINAL_DISCONNECTED),
        "a different terminal is never attached in its place"
    );
}

/// The primary window: the registry already knew every saved pane, and its
/// offscreen terminals attached while their surfaces slept. The gate read
/// that sleeping surface as a failed attachment.
#[test]
fn a_hibernated_offscreen_attachment_qualifies() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    admit(&mut window);
    run_gate(&mut window, directory.path(), false);
    assert_hibernated_attachment(&window.app, window.cached.id);
    assert_eq!(window.app.startup_pane_check(), Ok(true));
}

/// The relaunched windows: the offscreen attach ended detached, and the pane
/// stayed loading with nothing in flight or queued to attach it.
#[test]
fn a_detached_offscreen_attach_is_reported_rather_than_awaited() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    admit(&mut window);
    run_gate(&mut window, directory.path(), true);
    assert_eq!(window.app.terminal_starts_in_flight(), 0);
    assert!(window.app.restored_agent_attaches.is_empty());
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(REFUSAL), "{report}");
}

/// Restore an actual observed-agent row so initialize_runtimes, rather than
/// a fixture queue insertion, owns the pre-admission attach intent.
fn reopened_observed_window(with_projected: bool) -> Window {
    let (mut app, active, hidden, pane) = fixture();
    let identity = app.workspace_model.identity;
    let active_pane = CardState {
        id: pane.id,
        generation: 0,
        title: pane.title,
        width_fraction: pane.width_fraction,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Hot,
        kind: pane.kind,
    };
    let mut cached = offscreen(&live_terminal());
    let CardKind::Terminal(source) = &mut cached.kind else {
        unreachable!()
    };
    source.launch.program = "/usr/bin/claude".into();
    let projected = with_projected.then(|| offscreen(&live_terminal()));
    let durable = Arc::make_mut(&mut app.durable);
    durable.workspace_model_identity = Some(identity);
    durable.active_channel = Some(active);
    durable.focused = Some(active_pane.id);
    let active_channel = durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == active)
        .unwrap();
    active_channel.cards = vec![active_pane.id];
    active_channel.focused = Some(active_pane.id);
    durable
        .channels
        .iter_mut()
        .find(|channel| channel.id == hidden)
        .unwrap()
        .cards = vec![cached.id];
    durable.cards = vec![active_pane.clone(), cached.clone()];
    // The visible strip belongs to the active channel; the cached terminal
    // remains durable offscreen content, not the focus/order baseline.
    let mut visible = (*app.durable).clone();
    visible.cards.retain(|card| card.id == active_pane.id);
    app.workspace = Cagents::rebuild_workspace(&visible);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    app.startup_gate = crate::app::startup_gate::State::fixture_opened();
    drop(app.initialize_runtimes());
    assert_eq!(app.workspace.focused(), Some(active_pane.id));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [active_pane.id]);
    assert_eq!(app.restored_agent_attaches, [cached.id]);
    assert_eq!(app.terminal_starts_in_flight(), 0);
    Window {
        app,
        active,
        hidden,
        cached,
        projected,
    }
}

fn observed_window_model(window: &Window) -> crate::workspace_model::WorkspaceModel {
    let mut hidden = View::empty(window.hidden);
    hidden.panes = window
        .terminals()
        .iter()
        .map(|card| Pane::from_card(card, None).unwrap())
        .collect();
    crate::workspace_model::WorkspaceModel {
        identity: window.app.durable.workspace_model_identity.unwrap(),
        sequence: 1,
        channels: [window.active, window.hidden]
            .into_iter()
            .map(|id| crate::workspace_model::ModelChannel {
                id,
                repository: "/fixture-project".into(),
                title: "view".into(),
                title_source: Default::default(),
                ordinal: 1,
            })
            .collect(),
        views: vec![View::empty(window.active), hidden],
        ..Default::default()
    }
}

fn emitted_seeds(requests: &async_channel::Receiver<ModelEdit>) -> HashMap<Scope, ModelEdit> {
    let mut seeds = HashMap::new();
    while let Ok(edit) = requests.try_recv() {
        let ModelEdit::SeedView { scope, .. } = &edit else {
            panic!("unexpected model edit before the fixture SeedView ACK: {edit:?}");
        };
        assert!(seeds.insert(scope.clone(), edit).is_none());
    }
    seeds
}

/// The isolated UI lane cannot use the Host's SQL registry. For these ordinary
/// unmanaged panes, use its public View reducer in the same closed-before-insert
/// order as host::model::views::apply, then return that model in the real UI ACK.
fn apply_emitted_seed(
    model: &mut crate::workspace_model::WorkspaceModel,
    seeds: &mut HashMap<Scope, ModelEdit>,
    scope: Scope,
) -> crate::workspace_model::WorkspaceModel {
    assert!(
        model.agents.is_empty(),
        "this fixture has no managed issuers"
    );
    let ModelEdit::SeedView {
        scope: issued_scope,
        panes,
        closed,
    } = seeds
        .remove(&scope)
        .expect("ACK requires an emitted SeedView")
    else {
        unreachable!()
    };
    assert_eq!(issued_scope, scope);
    let index = model
        .views
        .iter()
        .position(|view| view.scope == scope)
        .unwrap();
    assert_eq!(
        model
            .views
            .iter()
            .filter(|view| view.scope == scope)
            .count(),
        1
    );
    // A revision-zero view is the fixture's not-yet-registered migration view.
    let mut changed = model.views[index].revision == 0;
    for id in closed {
        changed |= model.views[index].apply(&Change::Remove { id }).unwrap();
    }
    for pane in panes {
        let another_owner = model.views.iter().any(|view| {
            view.panes.iter().any(|current| {
                (current.id == pane.id && view.scope != scope)
                    || (current.id != pane.id
                        && crate::workspace_model::views::same_resource(&current.kind, &pane.kind))
            })
        });
        if !another_owner {
            changed |= model.views[index]
                .apply(&Change::Insert { pane, before: None })
                .unwrap();
        }
    }
    if changed {
        model.views[index].revision = model.views[index].revision.checked_add(1).unwrap();
        model.sequence = model.sequence.checked_add(1).unwrap();
    }
    model.clone()
}

#[test]
fn emitted_seed_replies_are_additive_and_apply_closed_before_panes() {
    let scope = Scope::Channel(ChannelId::new());
    let existing = Pane::from_card(&offscreen(&live_terminal()), None).unwrap();
    let closed = Pane::from_card(&offscreen(&live_terminal()), None).unwrap();
    let added = Pane::from_card(&offscreen(&live_terminal()), None).unwrap();
    let mut stale = existing.clone();
    stale.title = "A seed must not overwrite the canonical pane".into();
    let mut view = View::empty(scope.clone());
    view.revision = 1;
    view.panes = vec![existing.clone(), closed.clone()];
    let mut model = crate::workspace_model::WorkspaceModel {
        sequence: 1,
        views: vec![view],
        ..Default::default()
    };
    let edit = ModelEdit::SeedView {
        scope: scope.clone(),
        panes: vec![stale, closed.clone(), added.clone()],
        closed: vec![closed.id],
    };
    let (publisher, requests) = async_channel::unbounded();
    publisher.try_send(edit.clone()).unwrap();
    let mut seeds = emitted_seeds(&requests);
    let reply = apply_emitted_seed(&mut model, &mut seeds, scope.clone());
    assert!(seeds.is_empty());
    assert_eq!(reply, model);
    assert_eq!(model.views[0].panes, [existing, added]);
    assert_eq!(model.views[0].retired, [closed.id]);
    assert_eq!(model.views[0].revision, 2);
    assert_eq!(model.sequence, 2);

    publisher.try_send(edit).unwrap();
    let mut duplicate = emitted_seeds(&requests);
    assert_eq!(apply_emitted_seed(&mut model, &mut duplicate, scope), reply);
    assert!(duplicate.is_empty());
}

#[test]
fn restored_observed_row_waits_for_its_own_seed_ack_after_connected() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_observed_window(false);
    let (publisher, requests) = async_channel::unbounded();
    window.app.workspace_model_tx = publisher;
    let mut model = observed_window_model(&window);
    let focused = window.app.workspace.focused();
    let order = window.app.workspace.order().collect::<Vec<_>>();
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Connected(model.clone())),
    );
    let mut seeds = emitted_seeds(&requests);
    assert!(seeds.contains_key(&Scope::Channel(window.active)));
    assert!(seeds.contains_key(&Scope::Channel(window.hidden)));
    assert!(
        matches!(&seeds[&Scope::Channel(window.active)], ModelEdit::SeedView { panes, .. }
        if panes.iter().map(|pane| pane.id).collect::<Vec<_>>() == order)
    );
    assert_eq!(window.app.terminal_starts_in_flight(), 0);
    assert_eq!(window.app.restored_agent_attaches, [window.cached.id]);
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(window.active),
                operation: None,
                model: apply_emitted_seed(&mut model, &mut seeds, Scope::Channel(window.active)),
            }),
    );
    assert_eq!(
        window.app.terminal_starts_in_flight(),
        0,
        "another scope's ACK cannot admit this row"
    );
    assert_eq!(window.app.restored_agent_attaches, [window.cached.id]);
    assert_eq!(window.app.workspace.focused(), focused);
    assert_eq!(window.app.workspace.order().collect::<Vec<_>>(), order);
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(window.hidden),
                operation: None,
                model: apply_emitted_seed(&mut model, &mut seeds, Scope::Channel(window.hidden)),
            }),
    );
    assert!(window.app.terminal_start_in_flight(window.cached.id));
    assert!(seeds.is_empty());
    let saved = session(&window.cached);
    drop(window.app.update(Message::RestoredAgentAttached {
        card: window.cached.id,
        terminal: saved.id,
        result: Ok(Some(attached(directory.path(), &saved))),
    }));
    assert_hibernated_attachment(&window.app, window.cached.id);
    assert_eq!(
        session(window.app.card_state(window.cached.id).unwrap()),
        rehosted(&saved)
    );
    assert_eq!(window.app.terminal_starts_in_flight(), 0);
    assert!(window.app.restored_agent_attaches.is_empty());
    assert_eq!(window.app.workspace.focused(), focused);
    assert_eq!(window.app.workspace.order().collect::<Vec<_>>(), order);
}

#[test]
fn a_queued_transfer_waits_for_target_ack_without_blocking_an_admitted_row() {
    for ack_before_release in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut window = reopened_observed_window(true);
        let (publisher, requests) = async_channel::unbounded();
        window.app.workspace_model_tx = publisher;
        let mut model = observed_window_model(&window);
        drop(
            window
                .app
                .apply_workspace_model_update(ModelUpdate::Connected(model.clone())),
        );
        let mut seeds = emitted_seeds(&requests);
        for channel in [window.active, window.hidden] {
            assert!(seeds.contains_key(&Scope::Channel(channel)));
            drop(
                window
                    .app
                    .apply_workspace_model_update(ModelUpdate::ViewApplied {
                        scope: Scope::Channel(channel),
                        operation: None,
                        model: apply_emitted_seed(&mut model, &mut seeds, Scope::Channel(channel)),
                    }),
            );
        }
        assert!(window.app.terminal_start_in_flight(window.cached.id));
        let other = offscreen(&live_terminal());
        model.sequence += 1;
        model.views[1].revision += 1;
        model.views[1]
            .panes
            .push(Pane::from_card(&other, None).unwrap());
        drop(
            window
                .app
                .apply_workspace_model_update(ModelUpdate::Read(model.clone())),
        );
        drop(window.app.update(Message::VerifyStartupPanes));
        let target = window.projected();
        assert_eq!(window.app.restored_agent_attaches, [target, other.id]);
        // Both new scopes are introduced by a real model Read. Only the
        // moved target's scope is acknowledged below; the other stays unready.
        let project = window.app.durable.channels[0].project_id;
        window
            .app
            .sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                RepositoryId::new(),
                "fixture".into(),
                "/fixture-project".into(),
                vec![],
                vec![],
            ));
        let destination = ChannelId::new();
        for channel in [destination, ChannelId::new()] {
            model.channels.push(crate::workspace_model::ModelChannel {
                id: channel,
                repository: "/fixture-project".into(),
                title: "new scope".into(),
                title_source: Default::default(),
                ordinal: model.channels.len() as u32,
            });
            model.views.push(View::empty(channel));
        }
        let at = model.views[1]
            .panes
            .iter()
            .position(|pane| pane.id == target)
            .unwrap();
        let moved = model.views[1].panes.remove(at);
        model.views[1].revision += 1;
        model.views[2].panes.push(moved);
        model.views[2].revision += 1;
        model.sequence += 1;
        drop(
            window
                .app
                .apply_workspace_model_update(ModelUpdate::Read(model.clone())),
        );
        let mut destination_seeds = emitted_seeds(&requests);
        assert!(destination_seeds.contains_key(&Scope::Channel(destination)));
        assert!(!window.app.shared_views_ready_for_startup());
        assert_eq!(window.app.restored_agent_attaches, [target, other.id]);
        let target_model = apply_emitted_seed(
            &mut model,
            &mut destination_seeds,
            Scope::Channel(destination),
        );
        let target_ack = |app: &mut Cagents| {
            drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(destination),
                operation: None,
                model: target_model.clone(),
            }));
        };
        if ack_before_release {
            target_ack(&mut window.app);
            assert!(!window.app.terminal_start_in_flight(target));
        }
        let cached = session(&window.cached);
        drop(window.app.update(Message::RestoredAgentAttached {
            card: window.cached.id,
            terminal: cached.id,
            result: Ok(Some(attached(directory.path(), &cached))),
        }));
        if !ack_before_release {
            assert!(!window.app.terminal_start_in_flight(target));
            assert!(
                window.app.terminal_start_in_flight(other.id),
                "an admitted row passes the held target"
            );
            assert_eq!(window.app.restored_agent_attaches, [target]);
            let source = session(&other);
            drop(window.app.update(Message::RestoredAgentAttached {
                card: other.id,
                terminal: source.id,
                result: Ok(Some(attached(directory.path(), &source))),
            }));
            assert_eq!(window.app.terminal_starts_in_flight(), 0);
            assert_eq!(window.app.restored_agent_attaches, [target]);
            target_ack(&mut window.app);
        }
        assert!(
            !window.app.shared_views_ready_for_startup(),
            "an unrelated scope is still unready"
        );
        assert!(window.app.terminal_start_in_flight(target));
        let source = session(window.projected.as_ref().unwrap());
        drop(window.app.update(Message::RestoredAgentAttached {
            card: target,
            terminal: source.id,
            result: Ok(Some(attached(directory.path(), &source))),
        }));
        if ack_before_release {
            assert!(window.app.terminal_start_in_flight(other.id));
            let source = session(&other);
            drop(window.app.update(Message::RestoredAgentAttached {
                card: other.id,
                terminal: source.id,
                result: Ok(Some(attached(directory.path(), &source))),
            }));
        }
        assert_eq!(window.app.terminal_starts_in_flight(), 0);
        assert!(window.app.restored_agent_attaches.is_empty());
        for card in [window.cached.id, target, other.id] {
            assert_hibernated_attachment(&window.app, card);
        }
    }
}
