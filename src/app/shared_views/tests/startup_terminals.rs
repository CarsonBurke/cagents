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
    window.app.card_state_mut(window.cached.id).unwrap().kind =
        CardKind::Terminal(replacement.clone());
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
