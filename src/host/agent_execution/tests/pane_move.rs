//! An agent session moves its own pane, and the panes of what it started,
//! into another channel of their project, and nothing else.
use super::*;
use crate::{
    host::client::process_request,
    model::{
        BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, ChannelId,
        LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession,
    },
    terminal_ipc::{ProtocolError, ProtocolErrorCode, TerminalSpawnOptions},
    workspace_model::{
        PaneDestination, PaneMove, WorkspaceModel,
        views::{Change, Pane, Scope},
    },
};
use std::{cell::Cell, rc::Rc};

fn pane(kind: CardKind) -> Pane {
    Pane {
        id: CardId::new(),
        title: "pane".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        kind,
        scope: None,
    }
}

fn page() -> Pane {
    let url = url::Url::parse("https://example.com/").unwrap();
    pane(CardKind::Browser(BrowserSession {
        id: BrowserId::new(),
        current_url: url.clone(),
        history: vec![url],
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: BrowserRestoreFidelity::UrlOnly,
    }))
}

/// The person's window adds a page to `scope` at its current revision.
fn crowd(host: &SharedHost, client: u64, scope: Scope) {
    let ServerResponse::WorkspaceModel { model } =
        process_request(host, client, ClientRequest::ReadWorkspaceModel)
            .unwrap()
            .0
    else {
        panic!("unexpected read reply")
    };
    let expected_revision = model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .map_or(0, |view| view.revision);
    let edit = ModelEdit::ChangeView {
        scope,
        expected_revision,
        operation: uuid::Uuid::new_v4(),
        change: Change::Insert {
            pane: page(),
            before: None,
        },
    };
    process_request(host, client, ClientRequest::EditWorkspaceModel { edit }).unwrap();
}

#[test]
fn an_agent_moves_only_the_panes_it_controls_and_makes_a_channel_only_by_moving_one() {
    if isolated("host::agent_execution::tests::pane_move::an_agent_moves_only_the_panes_it_controls_and_makes_a_channel_only_by_moving_one").is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    let project = root.join("project");
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let shell = |id: TerminalId| {
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "cat".into()],
            working_directory: project.clone(),
            environment: Default::default(),
        };
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id,
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        }
    };
    const USER: u64 = 1;
    const AGENT: u64 = 2;
    const PEER: u64 = 3;
    const LONELY: u64 = 4;
    for client in [USER, AGENT, PEER, LONELY] {
        host.lock().unwrap().register_client(client);
    }
    let request_as = |client: u64, request: ClientRequest| {
        process_request(&host, client, request).map(|(response, _, _)| response)
    };
    let spawn_as = |client: u64, session: TerminalSession| match request_as(
        client,
        ClientRequest::Spawn {
            session,
            options: TerminalSpawnOptions::default(),
        },
    )
    .unwrap()
    {
        ServerResponse::Spawned { terminal } => terminal.session,
        response => panic!("unexpected spawn reply {response:?}"),
    };
    let authenticate = |client: u64, session: TerminalId| {
        let authority = registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .retained_agent_authority(session)
            .unwrap();
        request_as(client, ClientRequest::Authenticate { authority }).unwrap();
    };
    let edit = |edit: ModelEdit| {
        request_as(USER, ClientRequest::EditWorkspaceModel { edit }).unwrap();
    };
    let model = || -> WorkspaceModel {
        match request_as(USER, ClientRequest::ReadWorkspaceModel).unwrap() {
            ServerResponse::WorkspaceModel { model } => model,
            response => panic!("unexpected read reply {response:?}"),
        }
    };
    let placed = |pane: CardId| {
        model()
            .views
            .into_iter()
            .find(|view| view.panes.iter().any(|candidate| candidate.id == pane))
            .map(|view| view.scope)
    };
    let move_as = |client: u64, pane: Option<CardId>, destination: PaneDestination| {
        request_as(
            client,
            ClientRequest::MovePane {
                pane,
                destination,
                operation: uuid::Uuid::new_v4(),
            },
        )
        .map(|response| match response {
            ServerResponse::PaneMoved(moved) => moved,
            response => panic!("unexpected move reply {response:?}"),
        })
    };
    let refused = |result: Result<PaneMove, ProtocolError>, code: ProtocolErrorCode| {
        let error = result.unwrap_err();
        assert_eq!(error.code, code, "{}", error.message);
        error.message
    };

    // Three sessions the person started, and a shell the agent started.
    let agent = spawn_as(USER, shell(TerminalId::new()));
    let peer = spawn_as(USER, shell(TerminalId::new()));
    let lonely = spawn_as(USER, shell(TerminalId::new()));
    authenticate(AGENT, agent.id);
    authenticate(PEER, peer.id);
    authenticate(LONELY, lonely.id);
    let started = spawn_as(AGENT, shell(TerminalId::new()));

    let (home, other, foreign) = (ChannelId::new(), ChannelId::new(), ChannelId::new());
    edit(ModelEdit::OpenProject {
        repository: elsewhere.clone(),
        name: "Elsewhere".into(),
        kind: ProjectKind::Folder,
    });
    for (id, repository, title) in [
        (home, &project, "Home"),
        (other, &project, "Other"),
        (foreign, &elsewhere, "Foreign"),
    ] {
        edit(ModelEdit::CreateChannel {
            id,
            repository: repository.clone(),
            title: title.into(),
            title_source: Default::default(),
        });
    }
    let own = pane(CardKind::Terminal(agent.clone()));
    let peers = pane(CardKind::Terminal(peer.clone()));
    let child = pane(CardKind::Terminal(started.clone()));
    let page = page();
    edit(ModelEdit::SeedView {
        scope: Scope::Channel(home),
        panes: vec![own.clone(), peers.clone(), child.clone(), page.clone()],
        closed: vec![],
    });
    let channels = model().channels.len();

    // Its own pane, named or not, moves within its project.
    let moved = move_as(AGENT, None, PaneDestination::Channel(other)).unwrap();
    assert_eq!(moved.pane.id, own.id);
    assert_eq!(moved.from, Scope::Channel(home));
    assert_eq!(moved.channel.id, other);
    assert!(!moved.created);
    assert_eq!(placed(own.id), Some(Scope::Channel(other)));
    let again = move_as(AGENT, Some(own.id), PaneDestination::Channel(other)).unwrap();
    assert_eq!(
        again.from,
        Scope::Channel(other),
        "already there moves nothing"
    );
    // So does a terminal it started.
    move_as(AGENT, Some(child.id), PaneDestination::Channel(other)).unwrap();
    assert_eq!(placed(child.id), Some(Scope::Channel(other)));

    // Never another session's pane, nor one showing no session.
    for target in [peers.id, page.id] {
        let message = refused(
            move_as(AGENT, Some(target), PaneDestination::Channel(other)),
            ProtocolErrorCode::PermissionDenied,
        );
        assert!(message.contains("belongs to another session"), "{message}");
        assert_eq!(placed(target), Some(Scope::Channel(home)));
    }
    // Never into another project.
    let message = refused(
        move_as(AGENT, None, PaneDestination::Channel(foreign)),
        ProtocolErrorCode::InvalidRequest,
    );
    assert!(
        message.contains("only between channels of its own project"),
        "{message}"
    );
    assert_eq!(placed(own.id), Some(Scope::Channel(other)));
    // A session with no saved pane is told what to do instead.
    let message = refused(
        move_as(LONELY, None, PaneDestination::Channel(other)),
        ProtocolErrorCode::InvalidRequest,
    );
    assert!(message.contains("Ask the person to move it"), "{message}");

    // It makes a channel only for a pane it moves, in that pane's project.
    let made = ChannelId::new();
    let created = move_as(
        AGENT,
        None,
        PaneDestination::NewChannel {
            id: made,
            title: "Split out".into(),
        },
    )
    .unwrap();
    assert!(created.created);
    assert_eq!(created.channel.title, "Split out");
    assert_eq!(created.channel.repository, project);
    assert_eq!(placed(own.id), Some(Scope::Channel(made)));
    assert_eq!(model().channels.len(), channels + 1);
    for (target, title) in [(Some(peers.id), "Taken"), (None, "   ")] {
        let id = ChannelId::new();
        move_as(
            AGENT,
            target,
            PaneDestination::NewChannel {
                id,
                title: title.into(),
            },
        )
        .unwrap_err();
        assert!(
            model().channel(id).is_none(),
            "a refused move leaves no channel"
        );
    }
    assert_eq!(model().channels.len(), channels + 1);
    // And no general right to edit the workspace comes with it.
    assert_eq!(
        request_as(
            AGENT,
            ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::CreateChannel {
                    id: ChannelId::new(),
                    repository: project.clone(),
                    title: "Direct".into(),
                    title_source: Default::default(),
                },
            },
        )
        .unwrap_err()
        .code,
        ProtocolErrorCode::PermissionDenied
    );

    // The peer moves its own pane; the person moves any pane, but names it.
    move_as(PEER, None, PaneDestination::Channel(other)).unwrap();
    assert_eq!(placed(peers.id), Some(Scope::Channel(other)));
    let message = refused(
        move_as(USER, None, PaneDestination::Channel(home)),
        ProtocolErrorCode::InvalidRequest,
    );
    assert!(message.contains("Name the pane to move"), "{message}");
    move_as(USER, Some(page.id), PaneDestination::Channel(other)).unwrap();
    move_as(USER, Some(own.id), PaneDestination::Channel(home)).unwrap();
    assert_eq!(placed(own.id), Some(Scope::Channel(home)));

    // A pane in the project's own checkout moves into its channels too.
    let checkout = Scope::Worktree {
        repository: project.clone(),
        path: project.clone(),
    };
    let console = pane(CardKind::Terminal(spawn_as(
        AGENT,
        shell(TerminalId::new()),
    )));
    edit(ModelEdit::SeedView {
        scope: checkout.clone(),
        panes: vec![console.clone()],
        closed: vec![],
    });
    let moved = move_as(AGENT, Some(console.id), PaneDestination::Channel(other)).unwrap();
    assert_eq!(moved.from, checkout);
    assert_eq!(placed(console.id), Some(Scope::Channel(other)));

    // A window's edit between the read and the transfer is read again and
    // the move re-authorized against it.
    let transfers = Rc::new(Cell::new(0));
    let (counted, shared) = (transfers.clone(), host.clone());
    crate::host::workspace::before_transfer(Some(Box::new(move || {
        if counted.replace(counted.get() + 1) == 0 {
            crowd(&shared, USER, Scope::Channel(other));
        }
    })));
    move_as(AGENT, Some(child.id), PaneDestination::Channel(home)).unwrap();
    assert_eq!(transfers.get(), 2, "the conflict was retried once");
    assert_eq!(placed(child.id), Some(Scope::Channel(home)));

    // A move that keeps conflicting fails, and the channel it made goes
    // with it only while that channel is still the empty one it made.
    let (crowded, quiet) = (ChannelId::new(), ChannelId::new());
    for (made, filled) in [(crowded, true), (quiet, false)] {
        let shared = host.clone();
        crate::host::workspace::before_transfer(Some(Box::new(move || {
            crowd(&shared, USER, Scope::Channel(home));
            if filled {
                crowd(&shared, USER, Scope::Channel(made));
            }
        })));
        let failed = move_as(
            AGENT,
            Some(child.id),
            PaneDestination::NewChannel {
                id: made,
                title: "Busy".into(),
            },
        );
        refused(failed, ProtocolErrorCode::ViewConflict);
        assert_eq!(placed(child.id), Some(Scope::Channel(home)));
        assert_eq!(
            model().channel(made).is_some(),
            filled,
            "a channel a window filled stays; an empty one is retired"
        );
    }
    crate::host::workspace::before_transfer(None);
}
