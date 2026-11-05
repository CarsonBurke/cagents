use super::*;
use crate::{
    agent_execution::{
        Authority, Operation, Placement, Reply,
        directory::{Entry, Scope as DirectoryScope, Surface},
    },
    model::{CardId, CardKind},
    workspace_model::views::{Change, Pane, Scope, View, shared_kind},
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
        let model = client.read_workspace_model().map_err(placement_error)?;
        let (view, pane) = match find_pane(
            &model,
            pane_id
                .map(|id| id.to_string())
                .as_deref()
                .unwrap_or(target),
        ) {
            Ok(found) => found,
            // A session no view holds has nothing to transfer. Having read the
            // pane earlier in this command means it was closed meanwhile, which
            // is not the same request.
            Err(error) if error.kind == "not_found" && pane_id.is_none() => {
                return attach_session(client, target, to, error);
            }
            Err(error) => return Err(error),
        };
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
            result => result.map_err(placement_error)?,
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
        let channel = placed_channel(&result, to)?;
        let human = format!(
            "{}\t{}\t{}\t{}",
            pane.id,
            to,
            channel.repository.display(),
            channel.title
        );
        return placed_output(&result, pane, to, false, human);
    }
    unreachable!("the final revision conflict is returned")
}

fn placed_channel(model: &WorkspaceModel, to: ChannelId) -> Result<&ModelChannel, AppError> {
    model.channel(to).ok_or_else(|| {
        AppError::new(
            "protocol",
            "The daemon did not return the destination channel",
        )
    })
}

fn placed_output(
    model: &WorkspaceModel,
    pane: &Pane,
    to: ChannelId,
    attached: bool,
    human: String,
) -> Result<CommandOutput, AppError> {
    Ok(CommandOutput {
        command: "channel.move",
        data: json!({
            "pane": pane,
            "placement": Scope::Channel(to),
            "channel": placed_channel(model, to)?,
            "sequence": model.sequence,
            "attached": attached,
        }),
        human,
    })
}

/// A hosted session cannot place panes, whatever is running in it: the daemon
/// cannot tell a person at an in-app shell from an agent. Say where this works.
fn placement_error(error: ControlError) -> AppError {
    let denied = matches!(
        error,
        ControlError::Remote {
            code: ProtocolErrorCode::PermissionDenied,
            ..
        }
    );
    let mut error = protocol_error(error);
    if denied {
        error.message = "A session running inside cagents, including a shell in this app, can't \
            place panes. Run this from a terminal outside cagents."
            .to_owned();
    }
    error
}

/// `cagents channel close` only removes the channel: nothing here ends a
/// session, so one still in it would keep running with no pane and no Stop. A
/// window's delete closes them first; a shell has no dialog to say so in, and
/// ending someone's running work is a separate, explicit step.
pub(super) fn refuse_close_holding_live_sessions(
    client: &mut ControlClient,
    channel: ChannelId,
) -> Result<(), AppError> {
    let reply = client.agent(
        Authority::default(),
        Operation::Directory(DirectoryScope::All),
    );
    let directory = match reply {
        Ok(Reply::Directory(directory)) => directory,
        Ok(_) => {
            return Err(AppError::new(
                "protocol",
                "The daemon did not return its session directory",
            ));
        }
        // A session can't read the directory. The delete it asked for gets the
        // daemon's own answer.
        Err(ControlError::Remote {
            code: ProtocolErrorCode::PermissionDenied,
            ..
        }) => return Ok(()),
        Err(error) => return Err(protocol_error(error)),
    };
    let live: Vec<String> = directory
        .caller
        .iter()
        .chain(&directory.sessions)
        .filter(|entry| {
            !entry.history_only
                && matches!(entry.view_scope, Some(Placement::Channel(id)) if id == channel)
        })
        .map(|entry| entry.id.to_string())
        .collect();
    if live.is_empty() {
        return Ok(());
    }
    Err(AppError::new(
        "invalid_target",
        format!(
            "Channel {channel} still has a live session, which would keep running with no pane: {}. \
             Move it with `cagents channel move <session> --to <channel>`, or delete the channel \
             in the app, which closes its sessions.",
            live.join(", ")
        ),
    ))
}

fn holder(model: &WorkspaceModel, session: TerminalId) -> Option<(&View, &Pane)> {
    model.views.iter().find_map(|view| {
        view.panes
            .iter()
            .find(|pane| session_id(pane) == Some(session))
            .map(|pane| (view, pane))
    })
}

/// Gives a live session that no view holds a pane in `to`.
///
/// Only membership changes: the pane carries the daemon's own descriptor, and a
/// window that shows it attaches to the running process by session ID. Nothing
/// here starts, closes or sends to the session, so a working turn carries on.
fn attach_session(
    client: &mut ControlClient,
    target: &str,
    to: ChannelId,
    unmatched: AppError,
) -> Result<CommandOutput, AppError> {
    // A name or ID prefix can only ever have meant an agent or a pane.
    let Ok(id) = target.trim().parse::<TerminalId>() else {
        return Err(unmatched);
    };
    // A connection that got this far acts for the user: the daemon refuses this
    // read on one authenticated for a session.
    let Reply::Directory(directory) = client
        .agent(
            Authority::default(),
            Operation::Directory(DirectoryScope::All),
        )
        .map_err(placement_error)?
    else {
        return Err(AppError::new(
            "protocol",
            "The daemon did not return its session directory",
        ));
    };
    let entry = directory
        .caller
        .iter()
        .chain(&directory.sessions)
        .find(|entry| entry.id == id)
        .ok_or_else(|| {
            AppError::new(
                "not_found",
                format!("No shared pane or live session matches {id}"),
            )
        })?;
    if entry.history_only {
        return Err(AppError::new(
            "invalid_target",
            "This is a saved transcript open for reading; nothing is running to attach. Reopen the conversation with Resume conversation in the app.",
        ));
    }
    if entry.delegation.is_some() {
        return Err(AppError::new(
            "invalid_target",
            "This session belongs to an agent. Move it with the agent's name or ID, as listed by `cagents sessions`.",
        ));
    }
    let pane = Pane {
        id: CardId::new(),
        title: attached_title(entry),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        kind: shared_kind(&hosted_kind(client, entry)?),
        // A filing would override where the launch directory already places it.
        scope: None,
    };
    let destination = Scope::Channel(to);
    let operation = uuid::Uuid::new_v4();
    for attempt in 0..3 {
        let model = client.read_workspace_model().map_err(placement_error)?;
        let channel = model
            .channel(to)
            .ok_or_else(|| AppError::new("not_found", format!("Channel {to} is no longer open")))?;
        match entry.project.as_deref() {
            Some(project) if project == channel.repository => {}
            Some(_) => {
                return Err(AppError::new(
                    "invalid_target",
                    format!(
                        "This session belongs to {}; choose a channel in that project",
                        entry.project_name.as_deref().unwrap_or("another project")
                    ),
                ));
            }
            None => {
                return Err(AppError::new(
                    "invalid_target",
                    "This session's directory is not in an open project. Open the project with \
                     `cagents project open`, then try again.",
                ));
            }
        }
        if holder(&model, id).is_some() {
            return Err(placed_meanwhile());
        }
        let expected_revision = model
            .views
            .iter()
            .find(|view| view.scope == destination)
            .map_or(0, |view| view.revision);
        let result = client.edit_workspace_model(ModelEdit::ChangeView {
            scope: destination.clone(),
            expected_revision,
            operation,
            change: Change::Insert {
                pane: pane.clone(),
                before: None,
            },
        });
        let result = match result {
            Err(ControlError::Remote {
                code: ProtocolErrorCode::ViewConflict,
                ..
            }) if attempt < 2 => continue,
            result => result.map_err(placement_error)?,
        };
        // Inserting a session another client placed first changes nothing and
        // still succeeds, so success alone does not mean this pane landed.
        return match holder(&result, id) {
            Some((view, placed)) if view.scope == destination => {
                let channel = placed_channel(&result, to)?;
                let title = match channel.title.as_str() {
                    // The sidebar's label for a channel its first prompt has not named.
                    "" => "New channel",
                    title => title,
                };
                let human = format!(
                    "Attached session {id} to channel \"{title}\" in {}. Open that channel to see it. A window with something still queued in this conversation moves it there once that finishes.",
                    entry.project_name.as_deref().unwrap_or("its project"),
                );
                placed_output(&result, placed, to, true, human)
            }
            Some(_) => Err(placed_meanwhile()),
            None => Err(AppError::new(
                "protocol",
                "The daemon did not confirm the requested pane placement",
            )),
        };
    }
    unreachable!("the final revision conflict is returned")
}

fn placed_meanwhile() -> AppError {
    AppError::new(
        "conflict",
        "Another window placed this session while it was being attached. Run the command again to move it here.",
    )
}

fn attached_title(entry: &Entry) -> String {
    if let Some(title) = entry.title.as_deref().filter(|title| !title.is_empty()) {
        return title.to_owned();
    }
    match (entry.surface, entry.provider) {
        (Surface::Chat, Some(provider)) => provider.chat_title().to_owned(),
        (Surface::Terminal, Some(provider)) => provider.label().to_owned(),
        (_, None) => "Terminal".to_owned(),
    }
}

/// The daemon's exact descriptor of a listed session, refused unless this
/// daemon is the one hosting it: a window would refuse to attach anything else.
fn hosted_kind(client: &mut ControlClient, entry: &Entry) -> Result<CardKind, AppError> {
    let (kind, proof) = match entry.surface {
        Surface::Chat => {
            let session = client
                .list_chats()
                .map_err(placement_error)?
                .into_iter()
                .find(|session| session.id == entry.id);
            let proof = session.as_ref().and_then(|session| session.proof.clone());
            (session.map(CardKind::Chat), proof)
        }
        Surface::Terminal => {
            let session = client
                .list()
                .map_err(placement_error)?
                .into_iter()
                .find(|descriptor| descriptor.session.id == entry.id)
                .map(|descriptor| descriptor.session);
            let proof = session
                .as_ref()
                .and_then(|session| session.continuity.live_proof());
            (session.map(CardKind::Terminal), proof)
        }
    };
    let kind = kind.ok_or_else(|| {
        AppError::new(
            "not_found",
            format!("Session {} is no longer running", entry.id),
        )
    })?;
    if proof.as_ref() != Some(client.proof()) {
        return Err(AppError::new(
            "invalid_target",
            format!(
                "Session {} belongs to an earlier daemon run and can't be attached.",
                entry.id
            ),
        ));
    }
    Ok(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_execution::directory;
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
            },
            proof: terminal.continuity.live_proof(),
            terminal: Some(terminal.clone()),
            owned_codex_listener: None,
            native_continuity: Default::default(),
        });
        let CardKind::Chat(session) = &canonical.kind else {
            unreachable!()
        };
        model.agents[0].session = Some(HostedSession::Chat(session.clone()));
        model.agents[0].console = session.terminal.clone();
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
                if failure == "pane" {
                    let directory: ClientEnvelope =
                        read_frame(&mut server_stream).unwrap().unwrap();
                    assert!(matches!(directory.request, ClientRequest::Agent { .. }));
                    write_frame(
                        &mut server_stream,
                        &directory_response(directory.request_id, vec![]),
                    )
                    .unwrap();
                }
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

    fn directory_response(request_id: u64, sessions: Vec<Entry>) -> ServerEnvelope {
        envelope(
            request_id,
            ServerMessage::Response {
                response: ServerResponse::Agent(Reply::Directory(
                    crate::agent_execution::directory::Directory {
                        caller: None,
                        scope: DirectoryScope::All,
                        sessions,
                    },
                )),
            },
        )
    }

    fn unplaced_chat() -> crate::chat::Session {
        crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: "/bin/claude".into(),
                    arguments: vec![],
                    working_directory: "/repo/tree".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(proof()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        }
    }

    fn unplaced_entry(id: TerminalId, surface: directory::Surface) -> Entry {
        Entry {
            history_only: false,
            id,
            surface,
            provider: Some(Provider::Claude),
            title: None,
            channel: None,
            view_scope: None,
            channel_repository: None,
            state: crate::agent::ActivityState::Working,
            directory: Some("/repo/tree".into()),
            worktree: Some("/repo/tree".into()),
            project: Some("/repo".into()),
            project_name: Some("repo".into()),
            delegation: None,
            relation: crate::agent_execution::directory::Relation::Peer,
            lineage: crate::agent_execution::Lineage(id),
        }
    }

    /// Everything an attach may ask the daemon. Any other request, such as a
    /// start, attach, send or close, fails the test: attaching changes
    /// membership and nothing else. Returns the edits received.
    struct FakeDaemon {
        models: Vec<WorkspaceModel>,
        sessions: Vec<Entry>,
        chats: Vec<crate::chat::Session>,
        terminals: Vec<crate::terminal::ipc::TerminalDescriptor>,
        /// What each successive edit answers; `None` is a revision conflict.
        edits: Vec<Option<WorkspaceModel>>,
    }

    impl FakeDaemon {
        fn serve(self, mut stream: UnixStream) -> thread::JoinHandle<Vec<ModelEdit>> {
            let Self {
                mut models,
                sessions,
                chats,
                terminals,
                mut edits,
            } = self;
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut received = Vec::new();
                while let Some(frame) = read_frame::<ClientEnvelope>(&mut stream).unwrap() {
                    let id = frame.request_id;
                    let response = |response| envelope(id, ServerMessage::Response { response });
                    let reply = match frame.request {
                        ClientRequest::Connect { .. } => response(ServerResponse::Connected {
                            proof: proof(),
                            resumed: vec![],
                        }),
                        ClientRequest::ReadWorkspaceModel => {
                            let model = if models.len() > 1 {
                                models.remove(0)
                            } else {
                                models[0].clone()
                            };
                            model_response(id, model)
                        }
                        ClientRequest::Agent {
                            operation: Operation::Directory(DirectoryScope::All),
                            ..
                        } => directory_response(id, sessions.clone()),
                        ClientRequest::ChatList => response(ServerResponse::ChatListed {
                            sessions: chats.clone(),
                        }),
                        ClientRequest::List => response(ServerResponse::Listed {
                            terminals: terminals.clone(),
                        }),
                        ClientRequest::EditWorkspaceModel { edit } => {
                            received.push(edit);
                            match edits.remove(0) {
                                Some(model) => model_response(id, model),
                                None => envelope(
                                    id,
                                    ServerMessage::Error(ProtocolError::new(
                                        ProtocolErrorCode::ViewConflict,
                                        "concurrent view edit",
                                        true,
                                    )),
                                ),
                            }
                        }
                        other => panic!("attaching may not send {other:?}"),
                    };
                    write_frame(&mut stream, &reply).unwrap();
                }
                received
            })
        }
    }

    fn insert_of(edit: &ModelEdit) -> (&Scope, u64, uuid::Uuid, &Pane) {
        let ModelEdit::ChangeView {
            scope,
            expected_revision,
            operation,
            change: Change::Insert { pane, before: None },
        } = edit
        else {
            panic!("attaching may only insert one pane at the end: {edit:?}")
        };
        (scope, *expected_revision, *operation, pane)
    }

    fn with_pane(mut model: WorkspaceModel, scope: Scope, pane: Pane) -> WorkspaceModel {
        match model.views.iter_mut().find(|view| view.scope == scope) {
            Some(view) => {
                view.revision += 1;
                view.panes.push(pane);
            }
            None => model.views.push(View {
                scope,
                revision: 1,
                panes: vec![pane],
                retired: vec![],
            }),
        }
        model.sequence += 1;
        model
    }

    fn pane_for(session: &crate::chat::Session) -> Pane {
        Pane {
            id: CardId::new(),
            title: "elsewhere".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Chat(session.clone()),
            scope: None,
        }
    }

    /// Closing only removes the channel, so with a session in it nothing is
    /// sent: the channel and its pane stay. A session elsewhere doesn't count,
    /// and neither does a reader of a saved transcript.
    #[test]
    fn channel_close_is_refused_while_a_live_session_is_in_the_channel() {
        let (model, _, to) = fixture();
        let chat = unplaced_chat();
        let mut inside = unplaced_entry(chat.id, directory::Surface::Chat);
        inside.view_scope = Some(Placement::Channel(to));
        let mut reading = unplaced_entry(TerminalId::new(), directory::Surface::Chat);
        reading.view_scope = Some(Placement::Channel(to));
        reading.history_only = true;
        let mut elsewhere = unplaced_entry(TerminalId::new(), directory::Surface::Chat);
        elsewhere.view_scope = Some(Placement::Channel(ChannelId::new()));

        let check = |sessions: Vec<Entry>| {
            let (client_stream, server_stream) = UnixStream::pair().unwrap();
            let server = FakeDaemon {
                models: vec![model.clone()],
                sessions,
                chats: vec![],
                terminals: vec![],
                edits: vec![],
            }
            .serve(server_stream);
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let result = refuse_close_holding_live_sessions(&mut client, to);
            drop(client);
            assert!(server.join().unwrap().is_empty());
            result
        };

        let refused = check(vec![inside, reading.clone(), elsewhere.clone()]).unwrap_err();
        assert_eq!(refused.kind, "invalid_target");
        assert!(
            refused.message.contains(&chat.id.to_string()),
            "{}",
            refused.message
        );
        assert!(refused.message.contains("cagents channel move"));
        assert!(!refused.message.contains(&reading.id.to_string()));
        assert!(!refused.message.contains(&elsewhere.id.to_string()));
        assert!(check(vec![reading, elsewhere]).is_ok());
    }

    #[test]
    fn channel_move_attaches_a_live_session_without_a_pane_using_the_daemons_descriptor() {
        let (model, _, to) = fixture();
        let chat = unplaced_chat();
        let placed = |pane| with_pane(model.clone(), Scope::Channel(to), pane);
        let expected = Pane {
            id: CardId::new(),
            title: String::new(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Chat(chat.clone()),
            scope: None,
        };
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = FakeDaemon {
            models: vec![model.clone()],
            sessions: vec![unplaced_entry(chat.id, directory::Surface::Chat)],
            chats: vec![chat.clone()],
            terminals: vec![],
            edits: vec![None, Some(placed(expected.clone()))],
        }
        .serve(server_stream);
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let output = move_pane(&mut client, &chat.id.to_string(), to).unwrap();
        drop(client);
        let edits = server.join().unwrap();

        // A conflict retries the same request, so a lost reply cannot place twice.
        let (scope, revision, first_operation, first) = insert_of(&edits[0]);
        let (_, _, second_operation, second) = insert_of(&edits[1]);
        assert_eq!(edits.len(), 2);
        assert_eq!((first_operation, first), (second_operation, second));
        assert_eq!(scope, &Scope::Channel(to));
        assert_eq!(revision, 0, "the destination channel has never held a pane");
        assert_eq!(first.kind, CardKind::Chat(chat), "descriptor is unchanged");
        assert_eq!(first.title, "Claude chat");
        assert_eq!(first.scope, None);
        assert_eq!(output.data["attached"], json!(true));
        assert_eq!(output.data["placement"], json!(Scope::Channel(to)));
        assert_eq!(
            output.human,
            format!(
                "Attached session {} to channel \"channel\" in repo. Open that channel to see it. A window with something still queued in this conversation moves it there once that finishes.",
                match &expected.kind {
                    CardKind::Chat(session) => session.id,
                    _ => unreachable!(),
                }
            )
        );
    }

    #[test]
    fn channel_move_attaches_a_live_provider_terminal_and_keeps_its_title() {
        let (model, pane, to) = fixture();
        let CardKind::Terminal(mut session) = pane.kind.clone() else {
            unreachable!()
        };
        session.id = TerminalId::new();
        let descriptor = crate::terminal::ipc::TerminalDescriptor {
            screen_lost_after_crash: false,
            session: session.clone(),
            generation: 1,
            event_sequence: 0,
            oldest_event_sequence: 0,
            attached: false,
            transcript: None,
            activity: None,
            agent_activity: None,
            upgrade_sources: vec![],
            upgrade_relaunches: vec![],
            title: None,
        };
        let mut entry = unplaced_entry(session.id, directory::Surface::Terminal);
        entry.title = Some("deploy".into());
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let answer = with_pane(
            model.clone(),
            Scope::Channel(to),
            Pane {
                id: CardId::new(),
                title: "deploy".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 0.5,
                kind: CardKind::Terminal(session.clone()),
                scope: None,
            },
        );
        let server = FakeDaemon {
            models: vec![model],
            sessions: vec![entry],
            chats: vec![],
            terminals: vec![descriptor],
            edits: vec![Some(answer)],
        }
        .serve(server_stream);
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        move_pane(&mut client, &session.id.to_string(), to).unwrap();
        drop(client);
        let edits = server.join().unwrap();
        let (_, _, _, inserted) = insert_of(&edits[0]);
        assert_eq!(inserted.kind, CardKind::Terminal(session));
        assert_eq!(inserted.title, "deploy");
    }

    #[test]
    fn channel_move_refuses_a_session_it_cannot_truthfully_attach_before_editing() {
        for (failure, kind) in [
            ("unknown", "not_found"),
            ("ended", "not_found"),
            ("agent", "invalid_target"),
            ("other project", "invalid_target"),
            ("outside every project", "invalid_target"),
            ("other daemon", "invalid_target"),
            ("channel closed", "not_found"),
            ("placed meanwhile", "conflict"),
        ] {
            let (mut model, _, to) = fixture();
            let chat = unplaced_chat();
            let mut entry = unplaced_entry(chat.id, directory::Surface::Chat);
            let mut listed = chat.clone();
            let mut sessions = vec![];
            let mut chats = vec![];
            let mut models = vec![model.clone()];
            match failure {
                "unknown" => {}
                "ended" => sessions.push(entry.clone()),
                _ => {
                    match failure {
                        "agent" => {
                            entry.delegation = Some(crate::agent_execution::directory::Delegation {
                                agent: model.agents[0].id,
                                owner: crate::agent_execution::directory::Owner::User,
                                run: None,
                                run_state: None,
                            })
                        }
                        "other project" => entry.project = Some("/other".into()),
                        "outside every project" => entry.project = None,
                        "other daemon" => {
                            listed.proof = Some(HostSessionProof {
                                host_session_id: HostSessionId::new("earlier"),
                                ..proof()
                            })
                        }
                        "channel closed" => {
                            model.channels.retain(|channel| channel.id != to);
                            models = vec![model.clone()];
                        }
                        "placed meanwhile" => {
                            let scope = model.views[0].scope.clone();
                            models.push(with_pane(model.clone(), scope, pane_for(&chat)));
                        }
                        _ => unreachable!(),
                    }
                    sessions.push(entry.clone());
                    chats.push(listed);
                }
            }
            let (client_stream, server_stream) = UnixStream::pair().unwrap();
            let server = FakeDaemon {
                models,
                sessions,
                chats,
                terminals: vec![],
                edits: vec![],
            }
            .serve(server_stream);
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let error = move_pane(&mut client, &chat.id.to_string(), to)
                .err()
                .unwrap_or_else(|| panic!("{failure} must be refused"));
            assert_eq!(error.kind, kind, "{failure}: {}", error.message);
            drop(client);
            assert!(server.join().unwrap().is_empty(), "{failure} sent an edit");
        }
    }

    #[test]
    fn channel_move_reports_an_insert_the_daemon_did_not_place_here() {
        for (placed_elsewhere, kind) in [(false, "protocol"), (true, "conflict")] {
            let (model, _, to) = fixture();
            let chat = unplaced_chat();
            // Inserting a session that another client already placed is an
            // accepted no-op, so the reply has to be checked, not trusted.
            let answer = if placed_elsewhere {
                let scope = model.views[0].scope.clone();
                with_pane(model.clone(), scope, pane_for(&chat))
            } else {
                model.clone()
            };
            let (client_stream, server_stream) = UnixStream::pair().unwrap();
            let server = FakeDaemon {
                models: vec![model],
                sessions: vec![unplaced_entry(chat.id, directory::Surface::Chat)],
                chats: vec![chat.clone()],
                terminals: vec![],
                edits: vec![Some(answer)],
            }
            .serve(server_stream);
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let error = move_pane(&mut client, &chat.id.to_string(), to)
                .err()
                .unwrap();
            assert_eq!(error.kind, kind);
            drop(client);
            server.join().unwrap();
        }
    }

    #[test]
    fn channel_move_treats_a_handle_that_is_not_a_session_id_as_an_agent_or_pane() {
        let (model, _, to) = fixture();
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = FakeDaemon {
            models: vec![model],
            sessions: vec![],
            chats: vec![],
            terminals: vec![],
            edits: vec![],
        }
        .serve(server_stream);
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let error = move_pane(&mut client, "no-such-agent", to).err().unwrap();
        assert_eq!(error.kind, "not_found");
        assert!(error.message.contains("No shared pane matches"));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn channel_move_succeeds_when_another_client_places_the_session_in_this_channel_first() {
        let (model, _, to) = fixture();
        let chat = unplaced_chat();
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let theirs = pane_for(&chat);
        let answer = with_pane(model.clone(), Scope::Channel(to), theirs.clone());
        let server = FakeDaemon {
            models: vec![model],
            sessions: vec![unplaced_entry(chat.id, directory::Surface::Chat)],
            chats: vec![chat.clone()],
            terminals: vec![],
            edits: vec![Some(answer)],
        }
        .serve(server_stream);
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let output = move_pane(&mut client, &chat.id.to_string(), to).unwrap();
        drop(client);
        server.join().unwrap();
        assert_eq!(
            output.data["pane"],
            serde_json::to_value(&theirs).unwrap(),
            "the reported pane is the one the channel holds, not the one offered"
        );
    }

    #[test]
    fn channel_move_refuses_a_saved_transcript_with_nothing_running() {
        let (model, _, to) = fixture();
        let chat = unplaced_chat();
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let mut reader = unplaced_entry(chat.id, directory::Surface::Chat);
        reader.history_only = true;
        let server = FakeDaemon {
            models: vec![model],
            sessions: vec![reader],
            chats: vec![chat.clone()],
            terminals: vec![],
            edits: vec![],
        }
        .serve(server_stream);
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let error = move_pane(&mut client, &chat.id.to_string(), to)
            .err()
            .unwrap();
        assert_eq!(error.kind, "invalid_target");
        assert!(
            error.message.contains("saved transcript"),
            "{}",
            error.message
        );
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn channel_move_returns_the_third_straight_conflict_without_placing_anything() {
        let (model, _, to) = fixture();
        let chat = unplaced_chat();
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = FakeDaemon {
            models: vec![model],
            sessions: vec![unplaced_entry(chat.id, directory::Surface::Chat)],
            chats: vec![chat.clone()],
            terminals: vec![],
            edits: vec![None, None, None],
        }
        .serve(server_stream);
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let error = move_pane(&mut client, &chat.id.to_string(), to)
            .err()
            .unwrap();
        assert_eq!(error.kind, "protocol");
        drop(client);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn a_denied_placement_says_where_it_can_be_run() {
        let error = placement_error(ControlError::Remote {
            code: ProtocolErrorCode::PermissionDenied,
            message: "An agent session's connection may only read terminals".into(),
            retryable: false,
        });
        assert_eq!(error.kind, "protocol");
        assert!(error.message.contains("terminal outside cagents"));
        assert_eq!(
            error.details.unwrap(),
            json!({"code": ProtocolErrorCode::PermissionDenied, "retryable": false})
        );
    }
}
