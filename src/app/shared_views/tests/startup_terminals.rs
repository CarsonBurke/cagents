//! A relaunched window that views a shared project must qualify every saved
//! terminal it hosts off screen, without anyone opening those panes (#2028).
use super::*;
use crate::{
    app::workspace_model_sync::ModelUpdate,
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, StartupState, TerminalDescriptor, read_frame, write_frame,
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
    attached_with_connection(directory, saved).0
}

/// [`attached`], with the daemon's end of the view's connection.
fn attached_with_connection(
    directory: &Path,
    saved: &TerminalSession,
) -> (StartedTerminal, std::os::unix::net::UnixStream) {
    let socket = directory.join(format!("{}.sock", saved.id));
    let listener = UnixListener::bind(&socket).unwrap();
    let hosted = rehosted(saved);
    let served = hosted.clone();
    let (connection_tx, connection) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stream = listener.accept().unwrap().0;
        connection_tx.send(stream.try_clone().unwrap()).unwrap();
        serve(stream, vec![served]);
    });
    let handle = crate::terminal::HostedTerminalHandle::connect_at(
        socket,
        saved.id,
        Some(replacement_proof()),
        |_| {},
    )
    .unwrap();
    handle.wait_until_attached(Duration::from_secs(5)).unwrap();
    let started = StartedTerminal {
        handle: TerminalSurface::from(handle),
        session: hosted,
        spawned: false,
    };
    (started, connection.recv().unwrap())
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
    reopened_window_saving(with_projected, live_terminal())
}

fn reopened_window_saving(with_projected: bool, saved: TerminalSession) -> Window {
    let (mut app, active, hidden, _) = fixture();
    let identity = app.workspace_model.identity;
    let cached = offscreen(&saved);
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
    durable.cards = vec![cached.clone()].into();
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
fn admit(window: &mut Window) -> crate::workspace_model::WorkspaceModel {
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
    model
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
        window.app.startup_gate.retries.expire_window();
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

/// One gate check, then the cached pane's attach, if one started, refused
/// with `reason`. Whether an attempt started.
fn refused_attempt(window: &mut Window, reason: &str) -> bool {
    drop(window.app.update(Message::VerifyStartupPanes));
    let started = window.app.terminal_start_in_flight(window.cached.id);
    if started {
        let terminal = session(window.app.card_state(window.cached.id).unwrap()).id;
        drop(window.app.update(Message::RestoredAgentAttached {
            card: window.cached.id,
            terminal,
            result: Err(reason.to_owned()),
        }));
    }
    started
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
    assert!(
        app.startup_terminal_gave_up(window.cached.id),
        "a reported pane's tab must not read as still starting"
    );
    assert!(!app.startup_terminal_gave_up(window.projected()));
    let report = app.startup_pane_check().unwrap_err();
    assert!(
        report.starts_with(
            "recovery-incomplete: 1 saved pane could not reconnect: \u{201c}Terminal\u{201d}."
        ),
        "named as its tab is: {report}"
    );
    assert!(!report.contains(&window.cached.id.to_string()), "{report}");
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
    // Spend the cached pane's retries before a gate check reports them.
    for _ in 0..2 {
        window.app.startup_gate.retries.expire_window();
        drop(window.app.qualify_startup_terminals());
        complete_attaches(&mut window, directory.path(), true);
    }
    window.app.startup_gate.retries.expire_window();
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

/// A terminal the daemon has only just restored may refuse at once. The same
/// attach-only path tries again after each pause, and an unchanged refusal is
/// reported only once its window, measured from the first refusal, has passed.
#[test]
fn an_unchanged_refusal_is_retried_through_its_window_and_then_reported() {
    let mut window = reopened_window(false);
    admit(&mut window);
    assert!(refused_attempt(&mut window, REFUSAL));
    assert_eq!(window.app.startup_pane_check(), Ok(false));
    assert!(
        !refused_attempt(&mut window, REFUSAL),
        "no retry before its pause"
    );
    for _ in 0..3 {
        window.app.startup_gate.retries.expire_delay();
        assert!(refused_attempt(&mut window, REFUSAL));
        assert_eq!(
            window.app.startup_pane_check(),
            Ok(false),
            "a refusal that returns at once is not reported within seconds"
        );
    }
    window.app.startup_gate.retries.expire_window();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(REFUSAL), "{report}");
    assert!(
        !refused_attempt(&mut window, REFUSAL),
        "reported, not retried"
    );
}

/// A refusal whose reason keeps changing is not new evidence about the pane,
/// so it is reported, with its latest reason, once the same window passes.
#[test]
fn a_changing_refusal_is_reported_within_its_window() {
    let mut window = reopened_window(false);
    admit(&mut window);
    let reasons = [
        crate::app::SHARED_TERMINAL_DISCONNECTED,
        "could not connect to the daemon: Connection refused (os error 111)",
    ];
    for reason in reasons.iter().cycle().take(5) {
        window.app.startup_gate.retries.expire_delay();
        assert!(refused_attempt(&mut window, reason));
        assert_eq!(window.app.startup_pane_check(), Ok(false), "{reason}");
    }
    window.app.startup_gate.retries.expire_window();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(reasons[0]), "the latest reason: {report}");
}

/// The registry pane with the cached pane's attach fields changed by `edit`.
fn edit_registry_pane(window: &mut Window, edit: impl FnOnce(&mut Pane)) {
    let cached = window.cached.id;
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let view = model
        .views
        .iter_mut()
        .find(|view| view.panes.iter().any(|pane| pane.id == cached))
        .unwrap();
    view.revision += 1;
    edit(
        view.panes
            .iter_mut()
            .find(|pane| pane.id == cached)
            .unwrap(),
    );
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Read(model)),
    );
}

/// Another window renaming or resizing the pane changes nothing it attaches.
/// Its retries keep their pace and window, and its refusal is reported.
#[test]
fn title_churn_on_the_same_pane_neither_restarts_nor_hurries_its_retries() {
    let mut window = reopened_window(false);
    admit(&mut window);
    assert!(refused_attempt(&mut window, REFUSAL));
    for round in 0..4 {
        edit_registry_pane(&mut window, |pane| {
            pane.title = format!("Renamed elsewhere {round}");
            pane.width_fraction = 0.5 + round as f32 / 10.0;
        });
        assert!(
            !refused_attempt(&mut window, REFUSAL),
            "no retry before its pause"
        );
        assert_eq!(window.app.startup_pane_check(), Ok(false));
        window.app.startup_gate.retries.expire_delay();
        assert!(refused_attempt(&mut window, REFUSAL));
    }
    window.app.startup_gate.retries.expire_window();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(REFUSAL), "{report}");
}

/// An edit to another pane of the same view says nothing about this one.
#[test]
fn an_edit_to_a_neighbouring_pane_does_not_restart_the_retry_window() {
    let mut window = reopened_window(true);
    admit(&mut window);
    for _ in 0..2 {
        window.app.startup_gate.retries.expire_window();
        drop(window.app.update(Message::VerifyStartupPanes));
        let directory = tempfile::tempdir().unwrap();
        complete_attaches(&mut window, directory.path(), true);
    }
    let projected = window.projected();
    let view = window
        .app
        .workspace_model
        .views
        .iter_mut()
        .find(|view| view.panes.iter().any(|pane| pane.id == projected))
        .unwrap();
    view.revision += 1;
    view.panes
        .iter_mut()
        .find(|pane| pane.id == projected)
        .unwrap()
        .title = "Renamed elsewhere".into();
    window.app.startup_gate.retries.expire_window();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(REFUSAL), "{report}");
}

/// The refusals a shared pane's own attach produces, through the choice
/// `attach_restored_agent` makes for it: a daemon that no longer hosts the
/// exact terminal, and one that cannot be reached. Each is retried after its
/// pause, and the latest is reported once the window passes.
#[test]
fn a_shared_attach_refusal_is_retried_and_then_reported() {
    let mut window = reopened_window(false);
    admit(&mut window);
    let shared = window.app.attaches_as_shared(window.cached.id);
    assert!(shared);
    let saved = session(&window.cached);
    let unhosted = || {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        serve(server, Vec::new());
        crate::control::ControlClient::from_stream(client).map_err(|error| error.to_string())
    };
    let disconnected = crate::app::restored_attachment(shared, saved.clone(), unhosted)
        .err()
        .unwrap();
    assert_eq!(disconnected, crate::app::SHARED_TERMINAL_DISCONNECTED);
    let unreachable = crate::app::restored_attachment(shared, saved, || {
        Err("could not connect to the daemon: Connection refused (os error 111)".to_owned())
    })
    .err()
    .unwrap();
    for reason in [&disconnected, &unreachable, &disconnected] {
        window.app.startup_gate.retries.expire_delay();
        assert!(refused_attempt(&mut window, reason));
        assert_eq!(window.app.startup_pane_check(), Ok(false), "{reason}");
    }
    window.app.startup_gate.retries.expire_window();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(&disconnected), "{report}");
}

/// A daemon that cannot open its shared model still hosts the terminals.
/// Shared agent rows then attach from their own descriptor, as they did
/// before sharing, instead of waiting for a registry that is not coming.
#[test]
fn shared_agent_rows_attach_from_their_descriptor_when_the_daemon_has_no_registry() {
    let mut window = reopened_window(false);
    let card = window.cached.id;
    drop(window.app.update(Message::VerifyStartupPanes));
    assert!(!window.app.shared_views_ready_for_startup());
    assert!(window.app.restored_agent_attaches.is_empty());
    window.app.restored_agent_attaches.push_back(card);
    drop(window.app.attach_next_restored_agent());
    assert!(
        !window.app.terminal_start_in_flight(card),
        "held while the registry may still answer"
    );
    assert_eq!(window.app.restored_agent_attaches, [card]);
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Unavailable(
                "daemon rejected the request (Internal): registry is unavailable".into(),
            )),
    );
    assert!(window.app.terminal_start_in_flight(card));
    assert!(window.app.restored_agent_attaches.is_empty());
    assert!(
        !window.app.attaches_as_shared(card),
        "attached from its saved descriptor, not a registry pane"
    );
    let directory = tempfile::tempdir().unwrap();
    let source = session(&window.cached);
    drop(window.app.update(Message::RestoredAgentAttached {
        card,
        terminal: source.id,
        result: Ok(Some(attached(directory.path(), &source))),
    }));
    for _ in 0..50 {
        if window.app.startup_pane_check().is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(
        report.contains(crate::workspace_model::REGISTRY_UNAVAILABLE),
        "reported for the missing registry, not left waiting for it: {report}"
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
    durable.cards = vec![active_pane.clone(), cached.clone()].into();
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
    // This saved row differs from authority, so it still needs its own
    // migration barrier; an exact unchanged row can use the snapshot directly.
    model
        .views
        .iter_mut()
        .flat_map(|view| &mut view.panes)
        .find(|pane| pane.id == window.cached.id)
        .unwrap()
        .title = "Peer updated title".into();
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
        // moved target's scope is acknowledged below; the other has no
        // authoritative view yet and must still wait for its seed receipt.
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
        let unready = ChannelId::new();
        for channel in [destination, unready] {
            model.channels.push(crate::workspace_model::ModelChannel {
                id: channel,
                repository: "/fixture-project".into(),
                title: "new scope".into(),
                title_source: Default::default(),
                ordinal: model.channels.len() as u32,
            });
            if channel == destination {
                model.views.push(View::empty(channel));
            }
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
        assert!(destination_seeds.contains_key(&Scope::Channel(unready)));
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

/// The surface of a pane attached under the replacement daemon, after the
/// gate's first check started it.
fn attach_cached(window: &mut Window, directory: &Path) -> std::os::unix::net::UnixStream {
    drop(window.app.update(Message::VerifyStartupPanes));
    assert!(window.app.terminal_start_in_flight(window.cached.id));
    let saved = session(&window.cached);
    let (started, connection) = attached_with_connection(directory, &saved);
    drop(window.app.update(Message::RestoredAgentAttached {
        card: window.cached.id,
        terminal: saved.id,
        result: Ok(Some(started)),
    }));
    connection
}

fn cached_surface(window: &Window) -> &TerminalSurface {
    match window.app.cards.get(&window.cached.id) {
        Some(CardRuntime::Terminal(surface)) => surface,
        _ => panic!("the cached terminal was never attached"),
    }
}

/// Every window: a pane woken while its attachment was settling still holds
/// the detached marker its hibernated view left, until its first frame
/// arrives. The gate read that marker as a failed attachment and reported
/// every such pane at its first check, without a reason.
#[test]
fn a_woken_terminal_qualifies_before_its_first_frame() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    admit(&mut window);
    let _connection = attach_cached(&mut window, directory.path());
    let surface = cached_surface(&window);
    assert_ne!(
        session(&window.cached).continuity.live_proof(),
        surface.host_proof(),
        "attached under the replacement daemon, not the saved one"
    );
    for wake in [TerminalSurface::reveal, TerminalSurface::hide] {
        wake(surface).unwrap();
        assert!(surface.snapshot().detached, "no frame has arrived yet");
        assert_eq!(window.app.startup_pane_check(), Ok(true));
    }
}

/// A connection lost after the attachment was confirmed is still
/// reconnecting, which the view itself retries and reports if it gives up.
#[test]
fn a_lost_connection_waits_for_its_reconnect_and_a_stopped_one_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    admit(&mut window);
    let connection = attach_cached(&mut window, directory.path());
    assert_eq!(window.app.startup_pane_check(), Ok(true));
    connection.shutdown(std::net::Shutdown::Both).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while cached_surface(&window)
        .current_attachment()
        .is_ok_and(|current| current.is_some())
    {
        assert!(
            Instant::now() < deadline,
            "the view never saw its connection end"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(window.app.startup_pane_check(), Ok(false));
    cached_surface(&window).stop_for_test();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(report.contains(" Details: "), "{report}");
}

/// The cached pane attached under the replacement daemon by the gate, with
/// the edit carrying that attachment this window published to the registry.
fn attach_cached_publishing(
    window: &mut Window,
    directory: &Path,
) -> (std::os::unix::net::UnixStream, Scope, uuid::Uuid, Change) {
    let (publisher, requests) = async_channel::unbounded();
    window.app.workspace_model_tx = publisher;
    let connection = attach_cached(window, directory);
    assert!(window.app.shared_views.has_pending_update(window.cached.id));
    let mut update = None;
    while let Ok(edit) = requests.try_recv() {
        if let ModelEdit::ChangeView {
            scope,
            operation,
            change: change @ Change::Update { .. },
            ..
        } = edit
            && matches!(&change, Change::Update { pane, .. } if pane.id == window.cached.id)
        {
            update = Some((scope, operation, change));
        }
    }
    let (scope, operation, change) = update.expect("the attachment's edit was published");
    (connection, scope, operation, change)
}

/// The registry as it stands once another window's `edit` to the cached pane
/// landed.
fn registry_after(
    window: &Window,
    edit: impl FnOnce(&mut Pane),
) -> crate::workspace_model::WorkspaceModel {
    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let view = model
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(window.hidden))
        .unwrap();
    view.revision += 1;
    edit(
        view.panes
            .iter_mut()
            .find(|pane| pane.id == window.cached.id)
            .unwrap(),
    );
    model
}

fn surface_proof(window: &Window) -> HostSessionProof {
    cached_surface(window)
        .current_attachment()
        .unwrap()
        .unwrap()
}

/// The registry applies the edit carrying the attachment and acknowledges
/// that exact operation; the record it returns names the attachment.
#[test]
fn an_acknowledged_attachment_edit_qualifies_the_pane() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    admit(&mut window);
    let (_connection, scope, operation, change) =
        attach_cached_publishing(&mut window, directory.path());
    // A read from before the edit landed does not take the attachment back.
    let stale = registry_after(&window, |_| {});
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::Read(stale)),
    );
    assert_eq!(window.app.startup_pane_check(), Ok(true));

    let mut model = window.app.workspace_model.clone();
    model.sequence += 1;
    let view = model
        .views
        .iter_mut()
        .find(|view| view.scope == scope)
        .unwrap();
    assert!(view.apply(&change).unwrap());
    view.revision += 1;
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope,
                operation: Some(operation),
                model,
            }),
    );
    assert!(!window.app.shared_views.has_pending_update(window.cached.id));
    assert_eq!(
        session(window.app.card_state(window.cached.id).unwrap())
            .continuity
            .live_proof(),
        Some(surface_proof(&window))
    );
    drop(window.app.update(Message::VerifyStartupPanes));
    assert_eq!(window.app.startup_pane_check(), Ok(true));
}

/// Two windows attached the same pane after the update. The other window's
/// edit landed first, so the registry refuses this one through
/// `refuse_conflicting_view_update`. When the record it settles on names the
/// same attachment the pane qualifies; when it names another service, the
/// pane waits through its window and is then reported with that reason.
#[test]
fn a_refused_attachment_edit_settles_on_the_record_that_won() {
    for (other, qualifies) in [(None, true), (Some("another-host"), false)] {
        let directory = tempfile::tempdir().unwrap();
        let mut window = reopened_window(false);
        admit(&mut window);
        let (_connection, scope, operation, change) =
            attach_cached_publishing(&mut window, directory.path());
        let mut won = rehosted(&session(&window.cached));
        if let Some(host) = other {
            won.continuity = TerminalContinuity::Live {
                host_session_id: HostSessionId::new(host),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new(host),
            };
        }
        let model = registry_after(&window, |pane| pane.kind = CardKind::Terminal(won.clone()));
        drop(window.app.update(Message::WorkspaceModelObserved(
            ModelUpdate::ViewConflicted {
                scope,
                operation,
                change,
                model,
                error: "the pane's source changed".into(),
            },
        )));
        assert!(!window.app.shared_views.has_pending_update(window.cached.id));
        assert_eq!(
            window.app.card_state(window.cached.id).unwrap().kind,
            CardKind::Terminal(won)
        );
        // A refusal from before this attach is not this pane's report.
        let terminal = session(window.app.card_state(window.cached.id).unwrap()).id;
        window
            .app
            .startup_attach_failed(window.cached.id, terminal, REFUSAL.to_owned());
        window.app.startup_gate.retries.expire_window();
        for _ in 0..2 {
            drop(window.app.update(Message::VerifyStartupPanes));
        }
        if qualifies {
            assert_eq!(window.app.startup_pane_check(), Ok(true));
            continue;
        }
        assert_eq!(
            window.app.startup_pane_check(),
            Ok(false),
            "a record that may still follow the registry is given its window"
        );
        window.app.startup_gate.retries.expire_window();
        let report = window.app.startup_pane_check().unwrap_err();
        assert!(
            report.contains(
                "this window's record of it names a different terminal service than the one it is attached to"
            ),
            "{report}"
        );
        // It is attached and working, so opening it would change nothing.
        assert!(report.contains("can be used in this window"), "{report}");
        assert!(!report.contains("Open it"), "{report}");
    }
}

/// A pane the attach queue keeps holding back is reported with why, once its
/// window passes, instead of waiting for the gate's overall timeout.
#[test]
fn a_held_restored_pane_is_reported_with_its_hold() {
    let mut window = reopened_window(false);
    admit(&mut window);
    edit_registry_pane(&mut window, |pane| pane.pending_terminal_start = true);
    for _ in 0..2 {
        drop(window.app.update(Message::VerifyStartupPanes));
        assert!(!window.app.terminal_start_in_flight(window.cached.id));
    }
    assert_eq!(window.app.startup_pane_check(), Ok(false));
    window.app.startup_gate.retries.expire_window();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(
        report.contains("the shared workspace is still starting its terminal"),
        "{report}"
    );
    // Opening it completes that start, so that is what the report offers.
    assert!(report.contains("Open it to see why and retry"), "{report}");
    assert!(
        report.contains("Nothing was closed or restarted"),
        "{report}"
    );
}

/// A view the registry never admits holds every pane in it. Past its window
/// the gate reports that, rather than waiting for its timeout.
#[test]
fn a_view_that_is_never_admitted_is_reported() {
    let mut window = reopened_window(false);
    for _ in 0..2 {
        drop(window.app.update(Message::VerifyStartupPanes));
    }
    assert_eq!(window.app.startup_pane_check(), Ok(false));
    window.app.startup_gate.retries.expire_window();
    window.app.startup_gate.expire_views_unready();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(
        report.contains("the shared workspace has not answered this window"),
        "{report}"
    );
}

/// Whatever is still unsettled when the gate's budget runs out is named, with
/// why, rather than reported as conversations still reconnecting.
#[test]
fn the_budget_report_names_each_waiting_pane_and_why() {
    let mut window = reopened_window(false);
    admit(&mut window);
    drop(window.app.update(Message::VerifyStartupPanes));
    assert!(window.app.terminal_start_in_flight(window.cached.id));
    let budget_report = |app: &Cagents| {
        app.reconnecting_reason(&app.startup_pending().unwrap().expect("still waiting"))
    };
    let report = budget_report(&window.app);
    assert!(
        report.starts_with(
            "recovery-incomplete: 1 saved pane did not reconnect in time: \u{201c}Terminal\u{201d}."
        ),
        "{report}"
    );
    assert!(
        report.contains("Details: it was still reconnecting."),
        "{report}"
    );

    drop(window.app.update(Message::RestoredAgentAttached {
        card: window.cached.id,
        terminal: session(&window.cached).id,
        result: Err(REFUSAL.to_owned()),
    }));
    let report = budget_report(&window.app);
    assert!(report.contains(REFUSAL), "the latest refusal: {report}");
}

/// A store refusal does not outlast a connection that delivers the model.
#[test]
fn a_connection_after_a_store_refusal_clears_it() {
    let mut window = reopened_window(false);
    drop(
        window
            .app
            .update(Message::WorkspaceModelObserved(ModelUpdate::Unavailable(
                "store refused".into(),
            ))),
    );
    assert!(window.app.shared_views.registry_unavailable);
    admit(&mut window);
    assert!(!window.app.shared_views.registry_unavailable);
}

/// A failure that lands after the report still names its pane, and an attach
/// that later succeeds takes the reason back.
#[test]
fn a_failure_after_the_report_is_shown_until_a_later_attach_succeeds() {
    let mut window = reopened_window(false);
    let card = window.cached.id;
    admit(&mut window);
    let source = session(&window.cached);
    window.app.startup_gate = crate::app::startup_gate::State::fixture_reported();
    window
        .app
        .startup_attach_failed(card, source.id, "refused".into());
    assert!(window.app.startup_terminal_gave_up(card));
    let directory = tempfile::tempdir().unwrap();
    drop(window.app.update(Message::RestoredAgentAttached {
        card,
        terminal: source.id,
        result: Ok(Some(attached(directory.path(), &source))),
    }));
    assert!(!window.app.startup_terminal_gave_up(card));
    assert_eq!(window.app.startup_terminal_latest(card), None);
}

/// What a window saved when it closed for the update, while the registry still
/// records the same terminal as live under the daemon being replaced.
fn window_seeding_a_saved_terminal() -> (
    Window,
    crate::workspace_model::WorkspaceModel,
    async_channel::Receiver<ModelEdit>,
) {
    let registered = live_terminal();
    let mut saved = registered.clone();
    let proof = saved.continuity.live_proof().unwrap();
    saved.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    let mut window = reopened_window_saving(false, saved);
    let (tx, rx) = async_channel::unbounded();
    window.app.workspace_model_tx = tx;
    let mut view = View::empty(Scope::Channel(window.hidden));
    let mut registered_card = window.cached.clone();
    registered_card.kind = CardKind::Terminal(registered);
    view.panes = vec![Pane::from_card(&registered_card, None).unwrap()];
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
    (window, model, rx)
}

fn acknowledge_seed(
    window: &mut Window,
    model: &crate::workspace_model::WorkspaceModel,
    rx: &async_channel::Receiver<ModelEdit>,
) -> Vec<(Pane, Pane)> {
    while rx.try_recv().is_ok() {}
    drop(
        window
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(window.hidden),
                operation: None,
                model: model.clone(),
            }),
    );
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|edit| match edit {
            ModelEdit::ChangeView {
                change: Change::Update { before, pane },
                ..
            } => Some((before, pane)),
            _ => None,
        })
        .collect()
}

/// An update based on the saved record is refused, which projects the old
/// proof over the attachment; only another window's write then repairs it, and
/// the update's windows open one after another, so the first reads as failed.
#[test]
fn an_attachment_made_while_seeding_updates_the_registry_record() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, model, rx) = window_seeding_a_saved_terminal();
    let card = window.cached.id;
    let started = attached(directory.path(), &session(&window.cached));
    let live = started.session.clone();
    window.app.card_state_mut(card).unwrap().kind = CardKind::Terminal(live.clone());
    window
        .app
        .cards
        .insert(card, CardRuntime::Terminal(started.handle));

    let updates = acknowledge_seed(&mut window, &model, &rx);

    let registered = &model.views[1].panes[0];
    let [(before, pane)] = updates.as_slice() else {
        panic!("expected one update, got {updates:?}");
    };
    assert_eq!(before.kind, registered.kind);
    assert_eq!(pane.kind, CardKind::Terminal(live.clone()));
    assert_eq!(session(window.app.card_state(card).unwrap()), live);
}

/// A saved record is not an edit: the registry's stays authoritative, whichever
/// of the two is newer.
#[test]
fn a_saved_record_with_no_attachment_does_not_update_the_registry() {
    let (mut window, model, rx) = window_seeding_a_saved_terminal();
    let updates = acknowledge_seed(&mut window, &model, &rx);
    assert!(updates.is_empty(), "unexpected updates: {updates:?}");
    assert_eq!(
        window.app.card_state(window.cached.id).unwrap().kind,
        model.views[1].panes[0].kind
    );
}

/// An ended record is newer than an attachment made before the terminal ended,
/// so the update stays on its saved base, which the daemon refuses.
#[test]
fn an_attachment_is_not_rebased_over_an_ended_registry_record() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, mut model, rx) = window_seeding_a_saved_terminal();
    let card = window.cached.id;
    let saved = session(&window.cached);
    let started = attached(directory.path(), &saved);
    let live = started.session.clone();
    window.app.card_state_mut(card).unwrap().kind = CardKind::Terminal(live);
    window
        .app
        .cards
        .insert(card, CardRuntime::Terminal(started.handle));
    let mut ended = saved.clone();
    ended.continuity = TerminalContinuity::Exited {
        last_exit: crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    model.sequence += 1;
    model.views[1].panes[0].kind = CardKind::Terminal(ended);

    let updates = acknowledge_seed(&mut window, &model, &rx);

    let [(before, _)] = updates.as_slice() else {
        panic!("expected one update, got {updates:?}");
    };
    assert_eq!(before.kind, CardKind::Terminal(saved));
}

/// A surface attached under another proof doesn't stand behind what its record
/// claims, so the update stays on its saved base, which the daemon refuses.
#[test]
fn an_attachment_its_surface_disagrees_with_is_not_rebased() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, model, rx) = window_seeding_a_saved_terminal();
    let card = window.cached.id;
    let saved = session(&window.cached);
    let started = attached(directory.path(), &saved);
    let mut claimed = started.session.clone();
    claimed.continuity = TerminalContinuity::Live {
        host_session_id: HostSessionId::new("another-host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("another-host"),
    };
    window.app.card_state_mut(card).unwrap().kind = CardKind::Terminal(claimed);
    window
        .app
        .cards
        .insert(card, CardRuntime::Terminal(started.handle));

    let updates = acknowledge_seed(&mut window, &model, &rx);

    let [(before, _)] = updates.as_slice() else {
        panic!("expected one update, got {updates:?}");
    };
    assert_eq!(before.kind, CardKind::Terminal(saved));
}

/// A window that loses the revision race reads the registry again before
/// retrying. Finding its own result there isn't a refusal worth a notice;
/// finding some other record is.
#[test]
fn a_stale_update_is_refused_only_when_neither_its_source_nor_result_holds() {
    use crate::app::workspace_model_sync::source_update_conflicts;
    use crate::workspace_model::views::Change;
    let (_window, mut model, _rx) = window_seeding_a_saved_terminal();
    let scope = model.views[1].scope.clone();
    let before = model.views[1].panes[0].clone();
    let live = |host: &str| {
        let mut pane = before.clone();
        if let CardKind::Terminal(terminal) = &mut pane.kind {
            terminal.continuity = TerminalContinuity::Live {
                host_session_id: HostSessionId::new(host),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new(host),
            };
        }
        pane
    };
    let change = Change::Update {
        before: before.clone(),
        pane: live("result"),
    };
    assert!(!source_update_conflicts(&model, &scope, &change));
    model.views[1].panes[0] = live("result");
    assert!(!source_update_conflicts(&model, &scope, &change));
    model.views[1].panes[0] = live("another-host");
    assert!(source_update_conflicts(&model, &scope, &change));
}

/// A start returns attached, so no current attachment means the connection has
/// ended, and its proof may be older than the registry's.
#[test]
fn an_attachment_whose_connection_ended_is_not_rebased() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, model, rx) = window_seeding_a_saved_terminal();
    let card = window.cached.id;
    let saved = session(&window.cached);
    let (started, connection) = attached_with_connection(directory.path(), &saved);
    connection.shutdown(std::net::Shutdown::Both).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while started
        .handle
        .current_attachment()
        .is_ok_and(|proof| proof.is_some())
    {
        assert!(Instant::now() < deadline, "the connection did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    window.app.card_state_mut(card).unwrap().kind = CardKind::Terminal(started.session.clone());
    window
        .app
        .cards
        .insert(card, CardRuntime::Terminal(started.handle));

    let updates = acknowledge_seed(&mut window, &model, &rx);

    let [(before, _)] = updates.as_slice() else {
        panic!("expected one update, got {updates:?}");
    };
    assert_eq!(before.kind, CardKind::Terminal(saved));
}

/// The panes a gate attaches in the background are hibernated. Hibernating
/// keeps the connection, so they stay under the fence on the daemon's answer.
#[test]
fn a_hibernated_terminal_is_an_owner_the_gate_fences_on() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    let card = window.cached.id;
    let saved = session(&window.cached);
    let started = attached(directory.path(), &saved);
    started.handle.hibernate().unwrap();
    window.app.card_state_mut(card).unwrap().kind = CardKind::Terminal(started.session.clone());
    window
        .app
        .cards
        .insert(card, CardRuntime::Terminal(started.handle));
    assert_hibernated_attachment(&window.app, card);
    assert_eq!(
        window.app.startup_connected_owners(),
        vec![(card, replacement_proof())]
    );
}

/// The fence on the daemon's answer: a terminal attached under a daemon that
/// has since been replaced must not be vouched for by the new one, though it
/// is hibernated and so shows a detached snapshot.
#[test]
fn a_ready_answer_under_another_proof_does_not_vouch_for_a_hibernated_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    let card = window.cached.id;
    admit(&mut window);
    run_gate(&mut window, directory.path(), false);
    assert_hibernated_attachment(&window.app, card);
    assert_eq!(window.app.startup_pane_check(), Ok(true));

    let mut later = replacement_proof();
    later.host_session_id = HostSessionId::new("later-host");
    later.host_incarnation = HostIncarnation::new("later-incarnation");
    let answer = |window: &mut Window, proof| {
        window.app.startup_gate.fixture_ask_again();
        drop(window.app.update(Message::VerifyStartupPanes));
        drop(
            window
                .app
                .fixture_answer_startup_under(proof, StartupState::Ready),
        );
        let (accepted, _, _) = window.app.startup_gate.fixture_verdict();
        drop(window.app.update(Message::VerifyStartupPanes));
        let (_, saving, reported) = window.app.startup_gate.fixture_verdict();
        (accepted, saving, reported)
    };
    assert_eq!(
        answer(&mut window, later),
        (false, false, false),
        "the answer of a daemon this terminal is not attached under is fenced out"
    );
    assert_eq!(
        answer(&mut window, replacement_proof()),
        (true, true, false),
        "the terminal's own daemon does vouch for it"
    );
}

/// A reported pane held for a registry that has not answered, as the notice
/// about it describes.
fn reported_held_terminal(window: &mut Window) -> CardId {
    let card = window.cached.id;
    let source = session(&window.cached);
    window.app.startup_gate = crate::app::startup_gate::State::fixture_reported();
    window.app.startup_attach_failed(
        card,
        source.id,
        "the shared workspace has not answered".into(),
    );
    assert!(window.app.startup_pane_held(card));
    assert!(window.app.startup_terminal_gave_up(card));
    assert_eq!(
        window.app.startup_gave_up_title(card),
        "Waiting for the shared workspace"
    );
    let caption = window.app.startup_gave_up_caption(card);
    assert!(
        caption.starts_with("This terminal reconnects here once the shared workspace is ready."),
        "{caption}"
    );
    card
}

/// What a lifecycle sweep does once the cached pane's tab is on screen.
fn show_cached(window: &mut Window) {
    let visible = HashSet::from([window.cached.id]);
    drop(
        window
            .app
            .reconcile_lifecycle(&visible, &HashSet::new(), &HashSet::new()),
    );
}

/// The notice says a held terminal reconnects when opened once the workspace
/// answers, and needs nothing else from the reader.
#[test]
fn a_reported_held_terminal_reconnects_when_opened_after_the_registry_answers() {
    let mut window = reopened_window(false);
    let card = reported_held_terminal(&mut window);
    assert_eq!(
        window.app.startup_terminal_tab_status(card),
        Some(Some(("Waiting for the shared workspace", true))),
        "waiting needs nothing done, so it carries no warning"
    );
    admit(&mut window);
    assert!(!window.app.startup_pane_held(card));
    assert!(
        !window.app.terminal_start_in_flight(card),
        "an unopened pane waits to be opened once the gate has reported"
    );
    assert!(!window.app.startup_terminal_gave_up(card));
    assert_eq!(
        window.app.startup_terminal_tab_status(card),
        Some(None),
        "the tab reads neither as failed nor as starting while opening it works"
    );
    show_cached(&mut window);
    assert!(window.app.terminal_start_in_flight(card));
    assert!(!window.app.startup_terminal_gave_up(card));
}

/// The caption an opened held terminal shows: it stays held while the registry
/// is silent, and reconnects where it is when the registry answers.
#[test]
fn an_opened_held_terminal_reconnects_when_the_registry_answers() {
    let mut window = reopened_window(false);
    let card = reported_held_terminal(&mut window);
    show_cached(&mut window);
    assert!(
        !window.app.terminal_start_in_flight(card),
        "still held while the registry is silent"
    );
    assert!(window.app.startup_pane_held(card));
    admit(&mut window);
    show_cached(&mut window);
    assert!(window.app.terminal_start_in_flight(card));
    let directory = tempfile::tempdir().unwrap();
    let source = session(&window.cached);
    drop(window.app.update(Message::RestoredAgentAttached {
        card,
        terminal: source.id,
        result: Ok(Some(attached(directory.path(), &source))),
    }));
    assert!(!window.app.startup_terminal_gave_up(card));
    assert_eq!(window.app.startup_terminal_latest(card), None);
}

/// A view held for a managed agent, with no pane to name, says only what is
/// known: nothing shows that it clears by itself.
#[test]
fn a_view_held_for_a_managed_agent_promises_nothing_about_clearing() {
    let directory = tempfile::tempdir().unwrap();
    let mut window = reopened_window(false);
    admit(&mut window);
    run_gate(&mut window, directory.path(), false);
    assert_eq!(window.app.startup_pane_check(), Ok(true));

    window
        .app
        .shared_views
        .blocked
        .insert(Scope::Channel(window.hidden));
    drop(window.app.update(Message::VerifyStartupPanes));
    window.app.startup_gate.expire_views_unready();
    let report = window.app.startup_pane_check().unwrap_err();
    assert!(
        report.contains("its view holds a managed agent this window may not show yet"),
        "{report}"
    );
    assert!(
        report.contains("The update has not been verified."),
        "{report}"
    );
    assert!(!report.contains("connected"), "{report}");
    assert!(!report.contains("on its own"), "{report}");
}

/// A daemon that kept its original owners will not answer until it restarts,
/// so a held terminal must not read as about to reconnect.
#[test]
fn a_held_terminal_under_a_retained_restore_does_not_promise_to_reconnect() {
    let mut window = reopened_window(false);
    let card = window.cached.id;
    let source = session(&window.cached);
    window.app.startup_gate.fixture_ask_again();
    drop(window.app.update(Message::VerifyStartupPanes));
    drop(window.app.fixture_answer_startup_under(
        replacement_proof(),
        StartupState::Retained {
            detail: "retained".into(),
        },
    ));
    let (_, _, reported) = window.app.startup_gate.fixture_verdict();
    assert!(reported);
    window.app.startup_attach_failed(
        card,
        source.id,
        "the shared workspace has not answered".into(),
    );
    assert!(window.app.startup_terminal_gave_up(card));
    assert!(window.app.startup_pane_held(card));
    assert_eq!(
        window.app.startup_gave_up_title(card),
        "Reconnect unavailable"
    );
    let caption = window.app.startup_gave_up_caption(card);
    assert!(
        caption.contains("cannot reconnect until the terminal service restarts"),
        "{caption}"
    );
    assert!(
        !caption.contains("once the shared workspace is ready"),
        "{caption}"
    );
}

/// A record made after the registry answered is a real failure, and an answer
/// to a later edit must not clear it.
#[test]
fn a_failure_once_the_registry_has_answered_outlives_later_model_updates() {
    let mut window = reopened_window(false);
    let card = window.cached.id;
    let source = session(&window.cached);
    admit(&mut window);
    window.app.startup_gate = crate::app::startup_gate::State::fixture_reported();
    window
        .app
        .startup_attach_failed(card, source.id, REFUSAL.into());
    assert!(window.app.startup_terminal_gave_up(card));
    admit(&mut window);
    assert!(window.app.startup_terminal_gave_up(card));
    assert_eq!(
        window.app.startup_terminal_tab_status(card),
        Some(Some(("Reconnect unavailable", false)))
    );
}
