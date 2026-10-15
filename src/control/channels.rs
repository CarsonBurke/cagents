use super::*;
use crate::{
    model::{CardId, CardKind},
    workspace_model::views::{Change, Pane, Scope, View},
};

fn session_id(pane: &Pane) -> Option<TerminalId> {
    match &pane.kind {
        CardKind::Chat(session) => Some(session.id),
        CardKind::Terminal(session) => Some(session.id),
        _ => None,
    }
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
    let target = target.as_str();
    let mut matches = model.views.iter().flat_map(|view| {
        let agents = &agents;
        view.panes.iter().filter_map(move |pane| {
            let session = session_id(pane);
            (pane.id.to_string() == target
                || session.is_some_and(|id| id.to_string() == target)
                || agents.iter().any(|agent| {
                    session.is_some_and(|id| {
                        agent
                            .session
                            .as_ref()
                            .is_some_and(|session| session.id() == id)
                            || agent
                                .console
                                .as_ref()
                                .is_some_and(|console| console.id == id)
                    })
                }))
            .then_some((view, pane))
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

fn repository<'a>(model: &'a WorkspaceModel, scope: &'a Scope) -> Option<&'a Path> {
    match scope {
        Scope::Channel(id) => model
            .channel(*id)
            .map(|channel| channel.repository.as_path()),
        Scope::Worktree { repository, .. } => Some(repository),
    }
}

pub(super) fn move_pane(
    client: &mut ControlClient,
    target: &str,
    to: ChannelId,
) -> Result<CommandOutput, AppError> {
    let operation = uuid::Uuid::new_v4();
    let mut pane_id: Option<CardId> = None;
    for attempt in 0..3 {
        let model = client.read_workspace_model().map_err(protocol_error)?;
        let (view, pane) = find_pane(
            &model,
            pane_id
                .map(|id| id.to_string())
                .as_deref()
                .unwrap_or(target),
        )?;
        pane_id = Some(pane.id);
        let channel = model
            .channel(to)
            .ok_or_else(|| AppError::new("not_found", format!("Channel {to} is no longer open")))?;
        if repository(&model, &view.scope) != Some(channel.repository.as_path()) {
            return Err(AppError::new(
                "invalid_target",
                "Panes can only move between channels of the same project",
            ));
        }
        let destination = Scope::Channel(to);
        let result = client.edit_workspace_model(ModelEdit::ChangeView {
            scope: view.scope.clone(),
            expected_revision: view.revision,
            operation,
            change: Change::Transfer {
                id: pane.id,
                destination: destination.clone(),
                before: None,
            },
        });
        let result = match result {
            Err(ControlError::Remote {
                code: ProtocolErrorCode::ViewConflict,
                ..
            }) if attempt < 2 => continue,
            result => result.map_err(protocol_error)?,
        };
        let pane = result
            .views
            .iter()
            .find(|view| view.scope == destination)
            .and_then(|view| view.panes.iter().find(|item| item.id == pane.id))
            .ok_or_else(|| {
                AppError::new(
                    "protocol",
                    "The daemon did not confirm the requested pane placement",
                )
            })?;
        let channel = result.channel(to).ok_or_else(|| {
            AppError::new(
                "protocol",
                "The daemon did not return the destination channel",
            )
        })?;
        return Ok(CommandOutput {
            command: "channel.move",
            data: json!({ "pane": pane, "placement": destination, "channel": channel, "sequence": result.sequence }),
            human: format!(
                "{}\t{}\t{}\t{}",
                pane.id,
                to,
                channel.repository.display(),
                channel.title
            ),
        });
    }
    unreachable!("the final revision conflict is returned")
}

#[cfg(test)]
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
        let pane = Pane {
            id: CardId::new(),
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
                id: AgentId::new(),
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
                matches!(cli.command, TopCommand::Channel { command: ChannelCommand::Move { target: asked, to: destination } } if asked == target && destination == to)
            );
            assert_eq!(find_pane(&model, &target).unwrap().1, &pane);
        }
        assert!(Cli::try_parse_from(["cagents", "channel", "move", "worker"]).is_err());
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
        let (mut model, pane, _) = fixture();
        model.agents[0].id = "1a2b3c4d-0000-4000-8000-000000000001".parse().unwrap();
        assert_eq!(find_pane(&model, " 1A2B3C4D ").unwrap().1, &pane);
        let mut other = model.agents[0].clone();
        other.id = "1a2b9999-0000-4000-8000-000000000002".parse().unwrap();
        other.name = None;
        let mut other_pane = pane.clone();
        other_pane.id = CardId::new();
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
    fn model_response(request_id: u64, model: WorkspaceModel) -> ServerEnvelope {
        envelope(
            request_id,
            ServerMessage::Response {
                response: ServerResponse::WorkspaceModel { model },
            },
        )
    }

    #[test]
    fn channel_move_rebases_a_revision_conflict_and_reports_confirmed_placement_without_respawn() {
        let (mut model, pane, to) = fixture();
        let original = pane.clone();
        let target = model.agents[0].id.to_string()[..8].to_owned();
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
            let mut first_operation = None;
            for attempt in 0..2 {
                let read: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert!(matches!(read.request, ClientRequest::ReadWorkspaceModel));
                write_frame(
                    &mut server_stream,
                    &model_response(read.request_id, model.clone()),
                )
                .unwrap();
                let edit: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                let ClientRequest::EditWorkspaceModel {
                    edit:
                        ModelEdit::ChangeView {
                            scope,
                            expected_revision,
                            operation,
                            change:
                                Change::Transfer {
                                    id,
                                    destination,
                                    before,
                                },
                        },
                } = edit.request
                else {
                    panic!("move may only edit shared membership, never close or spawn a session")
                };
                assert_eq!(scope, model.views[0].scope);
                assert_eq!(id, pane.id);
                assert_eq!(expected_revision, model.views[0].revision);
                assert_eq!(destination, Scope::Channel(to));
                assert_eq!(before, None);
                if attempt == 0 {
                    first_operation = Some(operation);
                    model.views[0].revision += 1;
                    write_frame(
                        &mut server_stream,
                        &envelope(
                            edit.request_id,
                            ServerMessage::Error(ProtocolError::new(
                                ProtocolErrorCode::ViewConflict,
                                "concurrent view edit",
                                true,
                            )),
                        ),
                    )
                    .unwrap();
                } else {
                    assert_eq!(Some(operation), first_operation);
                    model.views[0].panes.clear();
                    model.views.push(View {
                        scope: destination.clone(),
                        revision: 1,
                        panes: vec![pane.clone()],
                        retired: vec![],
                    });
                    model.agents[0].placement = destination;
                    model.sequence += 1;
                    write_frame(
                        &mut server_stream,
                        &model_response(edit.request_id, model.clone()),
                    )
                    .unwrap();
                }
            }
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let output = move_pane(&mut client, &target, to).unwrap();
        server.join().unwrap();
        assert_eq!(output.command, "channel.move");
        assert_eq!(output.data["placement"], json!(Scope::Channel(to)));
        assert_eq!(
            output.data["pane"],
            serde_json::to_value(&original).unwrap()
        );
        assert_eq!(
            output.human,
            format!("{}\t{to}\t/repo\tchannel", original.id)
        );
    }

    #[test]
    fn channel_move_rejects_invalid_targets_before_editing_the_daemon() {
        for failure in ["pane", "channel", "project", "unconfirmed"] {
            let (mut model, pane, to) = fixture();
            if failure == "channel" {
                model.channels.retain(|channel| channel.id != to);
            }
            if failure == "project" {
                model
                    .channels
                    .iter_mut()
                    .find(|channel| channel.id == to)
                    .unwrap()
                    .repository = "/other".into();
            }
            let target = if failure == "pane" {
                CardId::new().to_string()
            } else {
                pane.id.to_string()
            };
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let server = thread::spawn(move || {
                let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
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
                let read: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert!(matches!(read.request, ClientRequest::ReadWorkspaceModel));
                write_frame(
                    &mut server_stream,
                    &model_response(read.request_id, model.clone()),
                )
                .unwrap();
                if failure == "unconfirmed" {
                    let edit: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                    assert!(matches!(
                        edit.request,
                        ClientRequest::EditWorkspaceModel { .. }
                    ));
                    write_frame(&mut server_stream, &model_response(edit.request_id, model))
                        .unwrap();
                }
                assert!(
                    read_frame::<ClientEnvelope>(&mut server_stream)
                        .unwrap()
                        .is_none(),
                    "a rejected move must send no mutation"
                );
            });
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let error = move_pane(&mut client, &target, to).err().unwrap();
            assert_eq!(
                error.kind,
                match failure {
                    "project" => "invalid_target",
                    "unconfirmed" => "protocol",
                    _ => "not_found",
                }
            );
            drop(client);
            server.join().unwrap();
        }
    }
}
