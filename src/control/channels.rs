use super::*;
use crate::{
    model::{CardId, CardKind},
    workspace_model::{
        PaneDestination, PaneMove,
        views::{Pane, Scope, View},
    },
};

fn session_id(pane: &Pane) -> Option<TerminalId> {
    match &pane.kind {
        CardKind::Chat(session) => Some(session.id),
        CardKind::Terminal(session) => Some(session.id),
        _ => None,
    }
}

fn current_agent_pane(agent: &crate::agent_execution::Agent, pane: &Pane) -> bool {
    use crate::agent_execution::HostedSession;
    if pane.id != CardId::from_uuid(agent.id.as_uuid()) {
        return false;
    }
    let source = match &pane.kind {
        CardKind::Chat(session) => HostedSession::Chat(session.clone()),
        CardKind::Terminal(session) => HostedSession::Terminal(session.clone()),
        _ => return false,
    };
    source.has_complete_proof()
        && (agent
            .session
            .as_ref()
            .is_some_and(|current| current.same_process(&source))
            || agent.console.as_ref().is_some_and(|console| {
                HostedSession::Terminal(console.clone()).same_process(&source)
            }))
}

fn find_pane<'a>(
    model: &'a WorkspaceModel,
    target: &str,
) -> Result<(&'a View, &'a Pane), AppError> {
    let target = target.trim().to_ascii_lowercase();
    if target.is_empty() {
        return Err(AppError::new(
            "not_found",
            "A pane or agent handle is empty",
        ));
    }
    let open = model
        .agents
        .iter()
        .filter(|agent| !agent.closed)
        .collect::<Vec<_>>();
    // Match the existing agent commands: exact names, whole IDs, then unique
    // ID prefixes. Never treat a name prefix or substring as a handle.
    let named = open
        .iter()
        .copied()
        .filter(|agent| agent.name.as_deref() == Some(target.as_str()))
        .collect::<Vec<_>>();
    let exact = open
        .iter()
        .copied()
        .filter(|agent| agent.id.to_string() == target)
        .collect::<Vec<_>>();
    let agents = if !named.is_empty() {
        named
    } else if !exact.is_empty() {
        exact
    } else {
        open.into_iter()
            .filter(|agent| agent.id.to_string().starts_with(&target))
            .collect()
    };
    if agents.len() > 1 {
        return Err(AppError::new(
            "ambiguous",
            "The agent handle is ambiguous; use its full ID",
        ));
    }
    // An agent spelling identifies only its authenticated canonical display.
    // A replaced or absent canonical card must not redirect it to a console.
    let requested_agent = agents.first().copied();
    let target = target.as_str();
    let mut matches = model.views.iter().flat_map(|view| {
        view.panes.iter().filter_map(move |pane| {
            let matches = match requested_agent {
                Some(agent) => current_agent_pane(agent, pane),
                None => {
                    pane.id.to_string() == target
                        || session_id(pane).is_some_and(|id| id.to_string() == target)
                }
            };
            matches.then_some((view, pane))
        })
    });
    let found = matches
        .next()
        .ok_or_else(|| AppError::new("not_found", format!("No shared pane matches {target:?}")))?;
    if matches.next().is_some() {
        return Err(AppError::new(
            "ambiguous",
            "More than one pane matches; use the pane's full ID",
        ));
    }
    Ok(found)
}

/// Moves the named pane, or with no name the calling agent session's own,
/// through the daemon, which authorizes an agent's move and makes a new
/// channel only for the pane it moves.
pub(super) fn move_pane(
    client: &mut ControlClient,
    target: Option<&str>,
    to: PaneDestination,
) -> Result<CommandOutput, AppError> {
    let (pane, opened) = match target {
        Some(target) => {
            let model = client.read_workspace_model().map_err(protocol_error)?;
            match find_pane(&model, target) {
                Ok((_, pane)) => (Some(pane.id), None),
                // A session no saved pane shows, or one named by the prefix
                // `sessions` prints: the daemon moves its pane or gives it one.
                Err(error) if error.kind == "not_found" && !target.trim().is_empty() => {
                    let session = live_chat(client, target)?.ok_or(error)?;
                    let shown = model
                        .views
                        .iter()
                        .flat_map(|view| &view.panes)
                        .any(|pane| session_id(pane) == Some(session));
                    (
                        Some(CardId::from_uuid(session.as_uuid())),
                        (!shown).then_some(session),
                    )
                }
                Err(error) => return Err(error),
            }
        }
        None => (None, None),
    };
    let moved = client.move_pane(pane, to).map_err(refusal)?;
    let placement = Scope::Channel(moved.channel.id);
    // A conversation given a pane was in no channel before.
    let opened = opened.filter(|_| matches!(moved.from, Scope::Worktree { .. }));
    let human = match opened {
        Some(session) => opened_line(&moved, session),
        None => moved_line(&moved, pane.is_none()),
    };
    let unchanged = moved.from == placement;
    Ok(CommandOutput {
        command: "channel.move",
        data: json!({
            "pane": moved.pane,
            "placement": placement,
            "from": moved.from,
            "channel": moved.channel,
            "created": moved.created,
            "moved": !unchanged,
            "opened": opened.is_some(),
            "sequence": moved.sequence,
        }),
        human,
    })
}

/// The live conversation `target` names by its full ID or by a prefix
/// shared with no other, as `sessions` prints it.
fn live_chat(client: &mut ControlClient, target: &str) -> Result<Option<TerminalId>, AppError> {
    let target = target.trim().to_ascii_lowercase();
    let directory =
        super::sessions::read_with(client, crate::agent_execution::directory::Scope::All)?;
    let mut found = directory
        .caller
        .iter()
        .chain(&directory.sessions)
        .map(|member| &member.session)
        .filter(|entry| {
            entry.surface == crate::agent_execution::directory::Surface::Chat
                && entry.id.to_string().starts_with(&target)
        })
        .map(|entry| entry.id);
    let session = found.next();
    if found.next().is_some() {
        return Err(AppError::new(
            "ambiguous",
            "More than one session starts with that; use more of its ID",
        ));
    }
    Ok(session)
}

/// One line saying where a conversation that had no pane now shows.
fn opened_line(moved: &PaneMove, session: TerminalId) -> String {
    let channel = &moved.channel;
    let title = if channel.title.is_empty() {
        "New channel"
    } else {
        channel.title.as_str()
    };
    let made = if moved.created { "new " } else { "" };
    format!(
        "Session {session} had no pane. It now shows in {made}channel \"{title}\" ({}) in {}; open that channel to see it.",
        channel.id,
        channel.repository.display()
    )
}

/// The daemon's refusal says what to do instead; keep it as its sentence.
fn refusal(error: ControlError) -> AppError {
    match error {
        ControlError::Remote {
            code: code @ (ProtocolErrorCode::PermissionDenied | ProtocolErrorCode::InvalidRequest),
            message,
            ..
        } => {
            let kind = if code == ProtocolErrorCode::PermissionDenied {
                "permission_denied"
            } else {
                "invalid_target"
            };
            AppError::new(kind, message)
        }
        // Anything else, such as a busy daemon, keeps its retry guidance.
        error => protocol_error(error),
    }
}

/// One line saying where the pane is now.
fn moved_line(moved: &PaneMove, own: bool) -> String {
    let channel = &moved.channel;
    let title = if channel.title.is_empty() {
        "New channel"
    } else {
        channel.title.as_str()
    };
    let pane = if own {
        "your pane".to_owned()
    } else {
        let name = moved
            .pane
            .user_title
            .as_deref()
            .unwrap_or(moved.pane.title.as_str());
        format!("pane \"{name}\"")
    };
    let repository = channel.repository.display();
    if moved.from == Scope::Channel(channel.id) {
        let mut pane = pane;
        pane[..1].make_ascii_uppercase();
        format!("{pane} is already in channel \"{title}\" ({}).", channel.id)
    } else if moved.created {
        format!(
            "Created channel \"{title}\" ({}) in {repository} and moved {pane} into it.",
            channel.id
        )
    } else {
        format!(
            "Moved {pane} to channel \"{title}\" ({}) in {repository}.",
            channel.id
        )
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target},
        model::{AgentId, BootId, HostIncarnation, HostSessionId},
        terminal::ipc::ProtocolError,
    };

    fn fixture() -> (WorkspaceModel, Pane, ChannelId) {
        let source = ChannelId::new();
        let to = ChannelId::new();
        let terminal = TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![],
                working_directory: "/repo".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: HostSessionId::new("live"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("host"),
            },
        };
        let agent_id = AgentId::new();
        let pane = Pane {
            id: CardId::from_uuid(agent_id.as_uuid()),
            title: "live worker".into(),
            user_title: Some("my worker".into()),
            pending_terminal_start: false,
            width_fraction: 0.6,
            kind: CardKind::Terminal(terminal.clone()),
            scope: Some("/repo/tree".into()),
        };
        let model = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 5,
            channels: [source, to]
                .into_iter()
                .map(|id| ModelChannel {
                    id,
                    repository: "/repo".into(),
                    title: "channel".into(),
                    title_source: Default::default(),
                    ordinal: 1,
                    checkout: None,
                })
                .collect(),
            views: vec![View {
                scope: Scope::Channel(source),
                revision: 4,
                panes: vec![pane.clone()],
                retired: vec![],
            }],
            agents: vec![Agent {
                name: Some("worker".into()),
                id: agent_id,
                origin: Origin::User,
                spec: Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: "/repo".into(),
                        directory: "/repo/tree".into(),
                        placement: Some(Scope::Channel(source)),
                    },
                    surface: Surface::Terminal,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                placement: Scope::Channel(source),
                session: Some(HostedSession::Terminal(terminal)),
                console: None,
                created_ms: 0,
                closed: false,
            }],
            ..WorkspaceModel::default()
        };
        (model, pane, to)
    }

    #[test]
    fn channel_move_parses_a_required_destination_and_resolves_each_identity() {
        let (model, pane, to) = fixture();
        let targets = [
            pane.id.to_string(),
            pane.id.to_string().to_uppercase(),
            session_id(&pane).unwrap().to_string(),
            model.agents[0].id.to_string(),
            model.agents[0].id.to_string()[..8].to_owned(),
            "WORKER".into(),
        ];
        for target in targets {
            let cli = Cli::try_parse_from([
                "cagents",
                "channel",
                "move",
                &target,
                "--to",
                &to.to_string(),
            ])
            .unwrap();
            assert!(
                matches!(cli.command, TopCommand::Channel { command: ChannelCommand::Move { target: Some(asked), to: Some(destination), new: None } } if asked == target && destination == to)
            );
            assert_eq!(find_pane(&model, &target).unwrap().1, &pane);
        }
        assert!(Cli::try_parse_from(["cagents", "channel", "move", "worker"]).is_err());
        assert!(Cli::try_parse_from(["cagents", "channel", "move"]).is_err());
        assert!(
            Cli::try_parse_from([
                "cagents",
                "channel",
                "move",
                "--to",
                &to.to_string(),
                "--new",
                "Both"
            ])
            .is_err()
        );
        let own =
            Cli::try_parse_from(["cagents", "channel", "move", "--new", "Auth refactor"]).unwrap();
        assert!(matches!(
            own.command,
            TopCommand::Channel {
                command: ChannelCommand::Move { target: None, to: None, new: Some(title) }
            } if title == "Auth refactor"
        ));
        assert_eq!(find_pane(&model, "absent").unwrap_err().kind, "not_found");
        let mut ambiguous = model.clone();
        let mut other = ambiguous.agents[0].clone();
        other.id = AgentId::new();
        ambiguous.agents.push(other);
        assert_eq!(
            find_pane(&ambiguous, "worker").unwrap_err().kind,
            "ambiguous"
        );
        assert_eq!(
            find_pane(&ambiguous, &pane.id.to_string()).unwrap().1,
            &pane
        );
    }

    #[test]
    fn agent_handles_require_unique_prefixes_and_prefer_exact_names() {
        let (mut model, mut pane, _) = fixture();
        model.agents[0].id = "1a2b3c4d-0000-4000-8000-000000000001".parse().unwrap();
        pane.id = CardId::from_uuid(model.agents[0].id.as_uuid());
        model.views[0].panes[0] = pane.clone();
        assert_eq!(find_pane(&model, " 1A2B3C4D ").unwrap().1, &pane);
        let mut other = model.agents[0].clone();
        other.id = "1a2b9999-0000-4000-8000-000000000002".parse().unwrap();
        other.name = None;
        let mut other_pane = pane.clone();
        other_pane.id = CardId::from_uuid(other.id.as_uuid());
        let CardKind::Terminal(session) = &mut other_pane.kind else {
            unreachable!()
        };
        session.id = TerminalId::new();
        other.session = Some(HostedSession::Terminal(session.clone()));
        model.views[0].panes.push(other_pane.clone());
        model.agents.push(other);
        assert_eq!(find_pane(&model, "1a2b").unwrap_err().kind, "ambiguous");
        assert_eq!(find_pane(&model, "1a2b3c4d").unwrap().1, &pane);
        assert_eq!(find_pane(&model, "1a2b9999").unwrap().1, &other_pane);
        assert_eq!(
            find_pane(&model, &model.agents[0].id.to_string())
                .unwrap()
                .1,
            &pane
        );
        for missing in ["1a2bffff", "0000-4000-8000", "work", " "] {
            assert_eq!(find_pane(&model, missing).unwrap_err().kind, "not_found");
        }
        model.agents[1].name = Some("1a2b".into());
        assert_eq!(find_pane(&model, "1a2b").unwrap().1, &other_pane);
        model.agents[1].closed = true;
        assert_eq!(find_pane(&model, "1a2b").unwrap().1, &pane);
    }

    #[test]
    fn agent_handles_choose_the_canonical_pane_without_claiming_its_console() {
        let (mut model, mut canonical, _) = fixture();
        canonical.id = CardId::from_uuid(model.agents[0].id.as_uuid());
        let CardKind::Terminal(terminal) = &canonical.kind else {
            unreachable!()
        };
        let console = Pane {
            id: CardId::new(),
            ..canonical.clone()
        };
        canonical.kind = CardKind::Chat(crate::chat::Session {
            id: terminal.id,
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                recipe: terminal.launch.clone(),
                session_id: None,
                draft_model: None,
            },
            proof: terminal.continuity.live_proof(),
            terminal: Some(Box::new(terminal.clone())),
            owned_codex_listener: None,
            native_continuity: Default::default(),
        });
        let CardKind::Chat(session) = &canonical.kind else {
            unreachable!()
        };
        model.agents[0].session = Some(HostedSession::Chat(session.clone()));
        model.agents[0].console = session.terminal.as_deref().cloned();
        model.views[0].panes = vec![canonical.clone(), console.clone()];
        for handle in [
            "worker".into(),
            model.agents[0].id.to_string(),
            model.agents[0].id.to_string()[..8].to_owned(),
        ] {
            assert_eq!(find_pane(&model, &handle).unwrap().1, &canonical);
        }
        assert_eq!(
            find_pane(&model, &session.id.to_string()).unwrap_err().kind,
            "ambiguous"
        );
        assert_eq!(
            find_pane(&model, &console.id.to_string()).unwrap().1,
            &console
        );
    }

    #[test]
    fn agent_handles_refuse_missing_replaced_or_unproved_canonical_panes() {
        let (model, pane, _) = fixture();
        let mut alias_only = model.clone();
        alias_only.views[0].panes[0].id = CardId::new();
        let alias = alias_only.views[0].panes[0].clone();
        assert_eq!(
            find_pane(&alias_only, &alias.id.to_string()).unwrap().1,
            &alias
        );
        assert_eq!(
            find_pane(&alias_only, &session_id(&alias).unwrap().to_string())
                .unwrap()
                .1,
            &alias
        );
        let mut replaced = model.clone();
        let CardKind::Terminal(session) = &mut replaced.views[0].panes[0].kind else {
            unreachable!()
        };
        session.id = TerminalId::new();
        let mut unproved = model.clone();
        let CardKind::Terminal(session) = &mut unproved.views[0].panes[0].kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new(""),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("host"),
        };
        unproved.agents[0].session = Some(HostedSession::Terminal(session.clone()));
        for invalid in [alias_only, replaced, unproved] {
            for handle in [
                "worker".into(),
                model.agents[0].id.to_string(),
                model.agents[0].id.to_string()[..8].to_owned(),
            ] {
                assert_eq!(find_pane(&invalid, &handle).unwrap_err().kind, "not_found");
            }
        }
        assert_eq!(find_pane(&model, &pane.id.to_string()).unwrap().1, &pane);
    }

    fn envelope(request_id: u64, message: ServerMessage) -> ServerEnvelope {
        ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: proof(),
            message,
        }
    }
    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("live"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("host"),
        }
    }

    /// Serves one connection: answers `Connect`, then each request in turn
    /// with `reply`, and checks that nothing follows.
    fn serve(
        mut reply: impl FnMut(ClientRequest) -> ServerMessage + Send + 'static,
        requests: usize,
    ) -> (ControlClient, thread::JoinHandle<()>) {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        server_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let server = thread::spawn(move || {
            let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            write_frame(
                &mut server_stream,
                &envelope(
                    connect.request_id,
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof(),
                            resumed: vec![],
                        },
                    },
                ),
            )
            .unwrap();
            for _ in 0..requests {
                let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                let message = reply(request.request);
                write_frame(&mut server_stream, &envelope(request.request_id, message)).unwrap();
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none(),
                "the move sent a request it was not expected to"
            );
        });
        (ControlClient::from_stream(client_stream).unwrap(), server)
    }

    fn moved(model: &WorkspaceModel, pane: &Pane, to: ChannelId, created: bool) -> ServerMessage {
        ServerMessage::Response {
            response: ServerResponse::PaneMoved(PaneMove {
                pane: pane.clone(),
                from: model.views[0].scope.clone(),
                channel: model.channel(to).unwrap().clone(),
                created,
                sequence: model.sequence + 1,
            }),
        }
    }

    #[test]
    fn a_named_pane_moves_through_the_daemon_and_says_where_it_is() {
        let (model, pane, to) = fixture();
        let target = model.agents[0].id.to_string()[..8].to_owned();
        let served = model.clone();
        let expected = pane.clone();
        let (mut client, server) = serve(
            move |request| match request {
                ClientRequest::ReadWorkspaceModel => ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel {
                        model: served.clone(),
                    },
                },
                ClientRequest::MovePane {
                    pane, destination, ..
                } => {
                    assert_eq!(pane, Some(expected.id));
                    assert_eq!(destination, PaneDestination::Channel(to));
                    moved(&served, &expected, to, false)
                }
                other => panic!("a move sends only a read and the move, not {other:?}"),
            },
            2,
        );
        let output = move_pane(&mut client, Some(&target), PaneDestination::Channel(to)).unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(output.command, "channel.move");
        assert_eq!(output.data["placement"], json!(Scope::Channel(to)));
        assert_eq!(output.data["from"], json!(model.views[0].scope));
        assert_eq!(output.data["moved"], json!(true));
        assert_eq!(output.data["created"], json!(false));
        assert_eq!(output.data["pane"], serde_json::to_value(&pane).unwrap());
        assert_eq!(
            output.human,
            format!("Moved pane \"my worker\" to channel \"channel\" ({to}) in /repo.")
        );
    }

    #[test]
    fn an_agent_moves_its_own_pane_into_a_channel_it_names() {
        let (mut model, pane, _) = fixture();
        let made = ChannelId::new();
        model.channels.push(ModelChannel {
            id: made,
            repository: "/repo".into(),
            title: "Auth refactor".into(),
            title_source: Default::default(),
            ordinal: 3,
            checkout: None,
        });
        let served = model.clone();
        let expected = pane.clone();
        let (mut client, server) = serve(
            move |request| {
                let ClientRequest::MovePane {
                    pane: None,
                    destination: PaneDestination::NewChannel { title, .. },
                    ..
                } = request
                else {
                    panic!("an agent's own move names no pane and reads nothing first: {request:?}")
                };
                assert_eq!(title, "Auth refactor");
                moved(&served, &expected, made, true)
            },
            1,
        );
        let output = move_pane(
            &mut client,
            None,
            PaneDestination::NewChannel {
                id: made,
                title: "Auth refactor".into(),
            },
        )
        .unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(output.data["created"], json!(true));
        assert_eq!(
            output.human,
            format!(
                "Created channel \"Auth refactor\" ({made}) in /repo and moved your pane into it."
            )
        );
    }

    #[test]
    fn a_pane_already_in_the_channel_says_so() {
        let (model, pane, _) = fixture();
        let Scope::Channel(here) = model.views[0].scope else {
            unreachable!()
        };
        let moved = PaneMove {
            pane,
            from: Scope::Channel(here),
            channel: model.channel(here).unwrap().clone(),
            created: false,
            sequence: 5,
        };
        assert_eq!(
            moved_line(&moved, true),
            format!("Your pane is already in channel \"channel\" ({here}).")
        );
    }

    #[test]
    fn a_refusal_keeps_the_daemons_sentence_and_an_unknown_handle_sends_no_move() {
        let sentence = "Pane 1 belongs to another session. Ask the person to move it";
        let (mut client, server) = serve(
            move |_| {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    sentence,
                    false,
                ))
            },
            1,
        );
        let error = move_pane(
            &mut client,
            None,
            PaneDestination::Channel(ChannelId::new()),
        )
        .err()
        .unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(error.kind, "permission_denied");
        assert_eq!(error.message, sentence);

        // Anything else keeps the daemon's code and whether to try again.
        let (mut client, server) = serve(
            |_| {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::ViewConflict,
                    "The view changed",
                    true,
                ))
            },
            1,
        );
        let error = move_pane(
            &mut client,
            None,
            PaneDestination::Channel(ChannelId::new()),
        )
        .err()
        .unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(error.kind, "protocol");
        assert_eq!(error.details.unwrap()["retryable"], true);

        // Neither a pane nor a live session: the model and the session list
        // are read, and nothing moves.
        let (model, _, to) = fixture();
        let (mut client, server) = serve(
            move |request| match request {
                ClientRequest::ReadWorkspaceModel => ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel {
                        model: model.clone(),
                    },
                },
                ClientRequest::Agent { .. } => sessions_reply(vec![]),
                other => panic!("an unknown handle sends no move: {other:?}"),
            },
            2,
        );
        let error = move_pane(&mut client, Some("absent"), PaneDestination::Channel(to))
            .err()
            .unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(error.kind, "not_found");
    }

    fn sessions_reply(sessions: Vec<crate::agent_execution::directory::Member>) -> ServerMessage {
        use crate::agent_execution::directory::{Context, Sessions};
        ServerMessage::Response {
            response: ServerResponse::Agent(crate::agent_execution::Reply::Context(Context {
                sessions: Sessions {
                    caller: None,
                    scope: crate::agent_execution::directory::Scope::All,
                    sessions,
                },
                lineage_channel: None,
            })),
        }
    }

    /// #2073: a live conversation no saved pane shows, named by the prefix
    /// `sessions` prints, is sent to the daemon by its session ID, and the
    /// reply says where it now shows.
    #[test]
    fn a_session_no_pane_shows_is_named_to_the_daemon_and_said_to_show_in_the_channel() {
        let (model, _, to) = fixture();
        let mut entry = super::super::sessions::tests::entry();
        entry.delegation = None;
        entry.view_scope = None;
        let session = entry.id;
        let target = session.to_string()[..8].to_owned();
        let served = model.clone();
        let (mut client, server) = serve(
            move |request| match request {
                ClientRequest::ReadWorkspaceModel => ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel {
                        model: served.clone(),
                    },
                },
                ClientRequest::Agent { .. } => {
                    sessions_reply(vec![super::super::sessions::tests::live(entry.clone())])
                }
                ClientRequest::MovePane {
                    pane, destination, ..
                } => {
                    assert_eq!(pane, Some(CardId::from_uuid(session.as_uuid())));
                    assert_eq!(destination, PaneDestination::Channel(to));
                    let mut pane = served.views[0].panes[0].clone();
                    pane.id = CardId::new();
                    ServerMessage::Response {
                        response: ServerResponse::PaneMoved(PaneMove {
                            pane,
                            from: Scope::Worktree {
                                repository: "/repo".into(),
                                path: "/repo".into(),
                            },
                            channel: served.channel(to).unwrap().clone(),
                            created: false,
                            sequence: served.sequence + 1,
                        }),
                    }
                }
                other => panic!("unexpected request {other:?}"),
            },
            3,
        );
        let output = move_pane(&mut client, Some(&target), PaneDestination::Channel(to)).unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(output.data["opened"], json!(true));
        assert_eq!(output.data["moved"], json!(true));
        assert_eq!(
            output.human,
            format!(
                "Session {session} had no pane. It now shows in channel \"channel\" ({to}) in /repo; open that channel to see it."
            )
        );
    }
}
