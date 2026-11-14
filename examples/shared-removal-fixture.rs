//! Reproduce a delayed Close against a replacement in a native-evidence fixture.
//! The ordinary daemon and UI run; only the competing window is scripted.
//! Usage: shared-removal-fixture PROTOCOL unconditional|conditional
use cagents::{
    model::{
        CardId, CardKind, ChannelId, HostSessionProof, LaunchRecipe, ProjectId, RepositoryView,
        TerminalContinuity, TerminalId, TerminalSession,
    },
    terminal_ipc::{
        ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
        TerminalCloseResult, TerminalDescriptor, TerminalSpawnOptions, daemon_socket_path,
        read_frame, write_frame,
    },
    workspace_model::{
        ModelEdit, WorkspaceModel,
        views::{Change, Pane, Scope},
    },
};
use std::{
    error::Error,
    os::unix::net::UnixStream,
    path::PathBuf,
    time::{Duration, Instant},
};

cagents::stamp_version!();
type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Peer {
    stream: UnixStream,
    version: u16,
    proof: Option<HostSessionProof>,
    next: u64,
}
impl Peer {
    fn request(&mut self, request: ClientRequest) -> Result<ServerResponse> {
        self.next += 1;
        write_frame(
            &mut self.stream,
            &ClientEnvelope {
                version: self.version,
                request_id: self.next,
                proof: self.proof.clone(),
                request,
            },
        )?;
        loop {
            let response: ServerEnvelope =
                read_frame(&mut self.stream)?.ok_or("fixture daemon disconnected")?;
            if response.version != self.version
                || self
                    .proof
                    .as_ref()
                    .is_some_and(|proof| proof != &response.proof)
            {
                return Err("fixture daemon identity changed".into());
            }
            if response.request_id != Some(self.next) {
                continue;
            }
            return match response.message {
                ServerMessage::Response { response } => Ok(response),
                ServerMessage::Error(error) => Err(error.message.into()),
                _ => Err("unexpected fixture response".into()),
            };
        }
    }
    fn model(&mut self) -> Result<WorkspaceModel> {
        match self.request(ClientRequest::ReadWorkspaceModel)? {
            ServerResponse::WorkspaceModel { model } => Ok(model),
            _ => Err("expected fixture model".into()),
        }
    }
    fn edit(&mut self, edit: ModelEdit) -> Result<WorkspaceModel> {
        match self.request(ClientRequest::EditWorkspaceModel { edit })? {
            ServerResponse::WorkspaceModel { model } => Ok(model),
            _ => Err("expected fixture edit receipt".into()),
        }
    }
    fn change(&mut self, scope: &Scope, change: Change) -> Result<WorkspaceModel> {
        let model = self.model()?;
        let revision = model
            .views
            .iter()
            .find(|view| &view.scope == scope)
            .ok_or("missing fixture view")?
            .revision;
        self.edit(ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: revision,
            operation: uuid::Uuid::new_v4(),
            change,
        })
    }
    fn ended(&mut self, id: TerminalId) -> Result<TerminalDescriptor> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ServerResponse::Listed { terminals } = self.request(ClientRequest::List)? else {
                return Err("expected fixture terminal list".into());
            };
            if let Some(terminal) = terminals.into_iter().find(|terminal| {
                terminal.session.id == id
                    && matches!(
                        terminal.session.continuity,
                        TerminalContinuity::Exited { .. }
                            | TerminalContinuity::RelaunchAvailable {
                                last_exit: Some(_),
                                ..
                            }
                    )
            }) {
                return Ok(terminal);
            }
            if Instant::now() >= deadline {
                return Err("fixture shell did not finish".into());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

fn session(directory: PathBuf, text: &str) -> TerminalSession {
    let launch = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), format!("printf '%s\\n' '{text}'")],
        working_directory: directory,
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    }
}

fn main() -> Result<()> {
    let fixture = PathBuf::from(
        std::env::var_os("CAGENTS_EVIDENCE_FIXTURE").ok_or("requires native-evidence fixture")?,
    );
    if std::env::var_os("CAGENTS_AGENT_AUTHORITY").is_some()
        || std::env::var_os("HOME").map(PathBuf::from) != Some(fixture.join("home"))
    {
        return Err("requires the private native-evidence user environment".into());
    }
    let mut args = std::env::args().skip(1);
    let version = args.next().ok_or("missing protocol")?.parse()?;
    let conditional = match args.next().as_deref() {
        Some("conditional") => true,
        Some("unconditional") => false,
        _ => return Err("expected conditional or unconditional".into()),
    };
    if args.next().is_some() {
        return Err("unexpected fixture arguments".into());
    }
    let stream = UnixStream::connect(daemon_socket_path()?)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    stream.set_write_timeout(Some(Duration::from_secs(20)))?;
    let mut peer = Peer {
        stream,
        version,
        proof: None,
        next: 0,
    };
    let ServerResponse::Connected { proof, .. } = peer.request(ClientRequest::Connect {
        expected_host: None,
        resume: vec![],
    })?
    else {
        return Err("expected fixture handshake".into());
    };
    peer.proof = Some(proof);
    let model = peer.model()?;
    let project = model
        .projects
        .first()
        .ok_or("open the fixture project first")?;
    let repository = project.repository.clone();
    let channel = ChannelId::new();
    peer.edit(ModelEdit::CreateChannel {
        id: channel,
        repository: repository.clone(),
        title: "Shared Close race".into(),
        title_source: cagents::model::ChannelTitleSource::Manual,
    })?;
    let scope = Scope::Channel(channel);
    let anchor = Pane {
        id: CardId::new(),
        title: "Reference pane".into(),
        user_title: Some("Reference pane".into()),
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Repository(RepositoryView {
            project: ProjectId::from_uuid(uuid::Uuid::nil()),
            repository: repository.clone(),
            worktree: fixture.join("project"),
            selected_file: None,
        }),
    };
    let original = session(fixture.join("project"), "Original worker");
    let card = CardId::new();
    let victim = Pane {
        id: card,
        title: "Original terminal".into(),
        user_title: Some("Original terminal".into()),
        pending_terminal_start: true,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Terminal(original.clone()),
    };
    peer.edit(ModelEdit::SeedView {
        scope: scope.clone(),
        panes: vec![anchor, victim],
        closed: vec![],
    })?;
    peer.request(ClientRequest::CompletePendingTerminal {
        pane: card,
        terminal_id: original.id,
        options: TerminalSpawnOptions::default(),
    })?;
    let ended = peer.ended(original.id)?;
    let model = peer.model()?;
    let before = model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .and_then(|view| view.panes.iter().find(|pane| pane.id == card))
        .ok_or("missing original fixture pane")?
        .clone();
    let terminal = if conditional {
        let ServerResponse::TerminalRemovalBasis(basis) =
            peer.request(ClientRequest::ReadTerminalRemovalBasis {
                source: ended.session.clone(),
            })?
        else {
            return Err("expected worker receipt".into());
        };
        Some(basis)
    } else {
        None
    };
    match peer.request(ClientRequest::ReleaseCompletedTerminal {
        session: ended.session,
        allow_active: false,
    })? {
        ServerResponse::TerminalCloseChecked {
            outcome: TerminalCloseResult::Closed,
            ..
        } => {}
        _ => return Err("original fixture worker was not released".into()),
    }
    let replacement = session(
        fixture.join("project"),
        "Replacement worker must survive delayed Close",
    );
    let current = peer
        .model()?
        .views
        .into_iter()
        .find(|view| view.scope == scope)
        .and_then(|view| view.panes.into_iter().find(|pane| pane.id == card))
        .ok_or("missing fixture pane before replacement")?;
    let mut next = current.clone();
    next.kind = CardKind::Terminal(replacement.clone());
    next.title = "Replacement terminal".into();
    next.user_title = Some(next.title.clone());
    next.pending_terminal_start = true;
    peer.change(
        &scope,
        Change::Update {
            before: current,
            pane: next,
        },
    )?;
    peer.request(ClientRequest::CompletePendingTerminal {
        pane: card,
        terminal_id: replacement.id,
        options: TerminalSpawnOptions::default(),
    })?;
    peer.ended(replacement.id)?;
    let removal = if conditional {
        Change::RemoveSource {
            registry: model.identity,
            repository,
            before,
            terminal,
        }
    } else {
        Change::Remove { id: card }
    };
    let result = peer.change(&scope, removal);
    if result.is_ok() == conditional {
        return Err("delayed removal produced the wrong result".into());
    }
    let model = peer.model()?;
    let kept = model
        .views
        .iter()
        .filter(|view| view.scope == scope)
        .flat_map(|view| &view.panes)
        .any(|pane| pane.id == card);
    if kept != conditional {
        return Err("replacement membership did not match the removal result".into());
    }
    println!(
        "Fixture replacement {} after delayed Close",
        if kept { "preserved" } else { "removed" }
    );
    Ok(())
}
