use super::*;
use crate::work_ui::{
    Action, BackendTarget, Effect, PreparedPlan, RemovalAction, RemovalEffect, RemovalOutcome,
    RemovalSubject, Watch,
};
use cosmic::iced::futures::SinkExt;

/// How often relative times on the board are redrawn.
const CLOCK: Duration = Duration::from_secs(30);

impl Cagents {
    /// The board subscription runs while this window shows a Work pane, so a
    /// closed pane stops the daemon assembling boards for it.
    pub(super) fn work_subscription(&self) -> Subscription<Message> {
        let open = self
            .durable
            .cards
            .iter()
            .any(|card| card.kind == CardKind::Work && card.existence.is_open());
        if !open {
            return Subscription::none();
        }
        let watch = Subscription::run(watch_board).map(Message::WorkUi);
        if self.refresh_ui.focused {
            Subscription::batch([
                watch,
                cosmic::iced::time::every(CLOCK).map(|_| Message::WorkUi(Action::Tick)),
            ])
        } else {
            watch
        }
    }

    pub(super) fn update_work_ui(&mut self, action: Action) -> Task<Message> {
        let Some(effect) = self.work_ui.update(action) else {
            return Task::none();
        };
        match effect {
            Effect::Removal(effect) => removal_control(effect),
            Effect::Refresh => control(
                |client| client.refresh_work_board(),
                |result| Action::Refreshed(result),
            ),
            Effect::Pin {
                ticket,
                project,
                item,
                column,
            } => control(
                move |client| client.pin_work_card(project, item, column),
                move |result| Action::Pinned { ticket, result },
            ),
            Effect::OpenWorktree { project, path } => {
                let opened = self
                    .sidebar_model
                    .projects
                    .iter()
                    .find(|candidate| candidate.repository == project)
                    .and_then(|project| {
                        let worktree = project
                            .worktrees
                            .iter()
                            .find(|worktree| worktree.path == path)?;
                        Some(SidebarAction::OpenWorktree {
                            project_id: project.id,
                            worktree_id: worktree.id,
                            path: worktree.path.clone(),
                        })
                    });
                match opened {
                    Some(action) => self.update(Message::Sidebar(action)),
                    // A worktree the sidebar has not listed yet still opens.
                    None => self.select_worktree(path, false),
                }
            }
            Effect::OpenPullRequest {
                project,
                number,
                url,
            } => {
                let Ok(url) = Url::parse(&url) else {
                    return Task::none();
                };
                match self
                    .sidebar_model
                    .projects
                    .iter()
                    .find(|candidate| candidate.repository == project)
                {
                    Some(project) => {
                        self.update(Message::Sidebar(SidebarAction::OpenPullRequest {
                            project_id: project.id,
                            number,
                            url,
                        }))
                    }
                    None => self.open_clicked_link(url),
                }
            }
            Effect::OpenUrl(url) => match Url::parse(&url) {
                Ok(url) if matches!(url.scheme(), "https" | "http") => self.open_clicked_link(url),
                _ => Task::none(),
            },
            // Session links find the session in whichever window shows it.
            Effect::FocusSession(session) => {
                self.handle_chat_ui(chat_ui::Action::OpenSession(session.to_string()))
            }
            Effect::Copy(text) => {
                chat_ui::invalidate_copy_intent();
                self.status = Some(TransientNotice::success(format!("Copied {text}")));
                iced::clipboard::write(text).map(cosmic::Action::App)
            }
        }
    }
}

/// A plan captures the endpoint which inspected it. Neither reinspection nor
/// removal consults a newly selected endpoint after that capture.
fn removal_client(target: &BackendTarget) -> Result<crate::control::ControlClient, String> {
    let client = crate::control::ControlClient::connect(&target.socket)
        .map_err(|error| error.to_string())?;
    if client.proof() != &target.proof {
        return Err("The reviewed device restarted or changed. Inspect the worktree before reviewing removal again.".into());
    }
    Ok(client)
}

fn removal_path(subject: &RemovalSubject) -> Result<String, String> {
    if !subject.path.is_absolute() || !subject.card.project.is_absolute() {
        return Err("The selected worktree has no absolute destination path".into());
    }
    subject
        .path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "The selected worktree path cannot be sent to the device".into())
}

fn read_removal_plan(
    subject: RemovalSubject,
    backend: Option<BackendTarget>,
) -> Result<PreparedPlan, String> {
    let path = removal_path(&subject)?;
    let (mut client, backend) = match backend {
        Some(backend) => (removal_client(&backend)?, backend),
        None => {
            let socket =
                crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
            let client = crate::control::ControlClient::connect(&socket)
                .map_err(|error| error.to_string())?;
            let backend = BackendTarget {
                socket,
                proof: client.proof().clone(),
            };
            (client, backend)
        }
    };
    // A branch card may later name another checkout. Inspect and remove its
    // captured worktree path, never resolve that mutable branch label again.
    let plan = client
        .plan_worktree_removal(subject.card.project.clone(), path)
        .map_err(|error| error.to_string())?;
    if plan.project != subject.card.project
        || plan.inspection.path != subject.path
        || plan.inspection.branch != subject.branch
    {
        return Err("The device inspected a different worktree. Refresh the board and review the intended worktree again.".into());
    }
    Ok(PreparedPlan { backend, plan })
}

fn remove_reviewed_worktree(
    subject: RemovalSubject,
    backend: BackendTarget,
    request: crate::work::Removal,
) -> RemovalOutcome {
    let path = match removal_path(&subject) {
        Ok(path) => path,
        Err(error) => return RemovalOutcome::Refused(error),
    };
    if request.scope != crate::work::Scope::Project(subject.card.project.clone())
        || request.item != path
        || request.token.is_empty()
    {
        return RemovalOutcome::Refused(
            "The removal request no longer matches the reviewed worktree".into(),
        );
    }
    let mut client = match removal_client(&backend) {
        Ok(client) => client,
        Err(error) => return RemovalOutcome::Refused(error),
    };
    // Every error after submission is potentially after filesystem effects.
    // A timeout, disconnect or partial server failure is never a retry permit.
    match client.remove_worktree_from_plan(request) {
        Ok(removed)
            if removed.project == subject.card.project && removed.path == subject.path =>
        {
            RemovalOutcome::Removed(removed)
        }
        Ok(_) => RemovalOutcome::Uncertain(
            "The device returned a result for a different worktree. Inspect the reviewed path before taking another removal action.".into(),
        ),
        Err(error) => RemovalOutcome::Uncertain(error.to_string()),
    }
}

fn removal_control(effect: RemovalEffect) -> Task<Message> {
    cosmic::task::future(async move {
        let action = match effect {
            RemovalEffect::Plan {
                generation,
                subject,
                backend,
            } => {
                let result =
                    tokio::task::spawn_blocking(move || read_removal_plan(subject, backend))
                        .await
                        .unwrap_or_else(|_| Err("The worktree inspection did not finish".into()));
                RemovalAction::Planned { generation, result }
            }
            RemovalEffect::Remove {
                generation,
                subject,
                backend,
                request,
            } => {
                let result = tokio::task::spawn_blocking(move || {
                    remove_reviewed_worktree(subject, backend, request)
                })
                .await
                .unwrap_or_else(|_| {
                    RemovalOutcome::Uncertain(
                        "The removal result was not returned. Inspect the worktree before trying another removal.".into(),
                    )
                });
                RemovalAction::Finished { generation, result }
            }
        };
        Message::WorkUi(action.into())
    })
}

/// Runs one blocking daemon request off the UI thread.
fn control(
    request: impl FnOnce(&mut crate::control::ControlClient) -> Result<(), crate::control::ControlError>
    + Send
    + 'static,
    answer: impl FnOnce(Result<(), String>) -> Action + Send + 'static,
) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            let socket =
                crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
            let mut client = crate::control::ControlClient::connect(&socket)
                .map_err(|error| error.to_string())?;
            request(&mut client).map_err(|error| error.to_string())
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        Message::WorkUi(answer(result))
    })
}

/// The daemon's boards, from a thread that holds the subscription.
///
/// Dropping the stream, which iced does when the last Work pane closes, hangs
/// up the connection: the thread's blocking read then fails, it finds no one
/// listening, and the daemon forgets the watcher.
fn watch_board() -> impl cosmic::iced::futures::Stream<Item = Action> {
    cosmic::iced::stream::channel(4, async |mut output| {
        let (sender, receiver) = async_channel::bounded(1);
        let hangup = Arc::new(Mutex::new(None));
        let held = hangup.clone();
        let spawned = std::thread::Builder::new()
            .name("cagents-work-board".into())
            .spawn(move || {
                while !sender.is_closed() {
                    let watched = watch(&sender, &held);
                    // A connection this side gave up on must not stay open
                    // through the retry delay, where the daemon would keep
                    // queueing boards for it.
                    if let Some(hangup) = held.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        hangup.hang_up();
                    }
                    if let Err(error) = watched
                        && sender.send_blocking(Watch::Lost(error)).is_err()
                    {
                        break;
                    }
                    std::thread::sleep(crate::daemon_health::retry_delay());
                }
            });
        if let Err(error) = spawned {
            let _ = output
                .send(Action::Watched(Watch::Lost(format!(
                    "the board could not be watched: {error}"
                ))))
                .await;
            return;
        }
        let _hangup = HangUpOnDrop(hangup);
        while let Ok(watch) = receiver.recv().await {
            if output.send(Action::Watched(watch)).await.is_err() {
                break;
            }
        }
    })
}

fn watch(
    sender: &async_channel::Sender<Watch>,
    hangup: &Mutex<Option<crate::control::Hangup>>,
) -> Result<(), String> {
    // Never start a daemon merely to watch it; the window starts one as soon
    // as it needs one, and this reconnects then.
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client =
        crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    *hangup.lock().unwrap_or_else(|p| p.into_inner()) =
        Some(client.hangup().map_err(|error| error.to_string())?);
    // The pane closed while this connected.
    if sender.is_closed() {
        return Ok(());
    }
    let (mut revision, board) = client.watch_work().map_err(|error| error.to_string())?;
    sender
        .send_blocking(Watch::Board {
            revision,
            board: Arc::new(board),
            first: true,
        })
        .map_err(|_| "the Work pane closed".to_owned())?;
    loop {
        let (next, board) = client
            .next_work_board(revision)
            .map_err(|error| error.to_string())?;
        revision = next;
        sender
            .send_blocking(Watch::Board {
                revision,
                board: Arc::new(board),
                first: false,
            })
            .map_err(|_| "the Work pane closed".to_owned())?;
    }
}

struct HangUpOnDrop(Arc<Mutex<Option<crate::control::Hangup>>>);

impl Drop for HangUpOnDrop {
    fn drop(&mut self) {
        if let Some(hangup) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            hangup.hang_up();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::HostSessionProof,
        terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        },
        work::{ItemKey, Keep, Removal, RemovalPlan, Removed, Request, Response, Scope},
        work_ui::CardRef,
        worktree_content::{Inspection, Verdict},
    };
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("removal-adapter"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new(incarnation),
        }
    }

    fn subject() -> RemovalSubject {
        RemovalSubject {
            card: CardRef {
                project: "/device/project/.git".into(),
                key: ItemKey::Branch("feature/reviewed".into()),
            },
            path: "/device/project/.worktrees/reviewed".into(),
            branch: Some("feature/reviewed".into()),
            label: "feature/reviewed".into(),
        }
    }

    fn plan() -> RemovalPlan {
        let subject = subject();
        RemovalPlan {
            project: subject.card.project,
            project_name: "project".into(),
            item: subject.label,
            inspection: Inspection {
                path: subject.path,
                branch: subject.branch,
                head: Some("reviewed-head".into()),
                verdict: Verdict::Landed,
                landed_in: Some("origin/main".into()),
                staged: 0,
                modified: 0,
                untracked: 0,
                untracked_bytes: 0,
                filtered: Vec::new(),
                nested: Vec::new(),
                ignored: Vec::new(),
                ignored_total: 0,
                token: Some("reviewed-token".into()),
            },
            merged_pull_request: Some(123),
        }
    }

    fn removal() -> Removal {
        let subject = subject();
        Removal {
            scope: Scope::Project(subject.card.project),
            item: subject.path.to_str().unwrap().into(),
            token: "reviewed-token".into(),
            keep: Keep::Nothing,
            delete_branch: true,
        }
    }

    fn removed() -> Removed {
        let subject = subject();
        Removed {
            project: subject.card.project,
            path: subject.path,
            snapshot: None,
            deleted_branch: subject.branch,
            branch_error: None,
        }
    }

    fn reply(
        stream: &mut UnixStream,
        proof: &HostSessionProof,
        request_id: u64,
        response: ServerResponse,
    ) {
        write_frame(
            stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request_id),
                proof: proof.clone(),
                message: ServerMessage::Response { response },
            },
        )
        .unwrap();
    }

    /// Only a synthetic, task-owned listener: no real daemon or user authority.
    fn backend(
        served_proof: HostSessionProof,
        reviewed_proof: HostSessionProof,
        exchange: impl FnOnce(&mut UnixStream, &HostSessionProof) + Send + 'static,
    ) -> (tempfile::TempDir, BackendTarget, JoinHandle<()>) {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("s");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "the adapter never connected");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(request.request, ClientRequest::Connect { .. }));
            reply(
                &mut stream,
                &served_proof,
                request.request_id,
                ServerResponse::Connected {
                    proof: served_proof.clone(),
                    resumed: Vec::new(),
                },
            );
            exchange(&mut stream, &served_proof);
            assert!(
                read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none(),
                "the adapter must close without another request"
            );
        });
        (
            root,
            BackendTarget {
                socket,
                proof: reviewed_proof,
            },
            server,
        )
    }

    #[test]
    fn changed_backend_proof_closes_before_any_plan_or_remove_request() {
        for planning in [true, false] {
            let (_root, backend, server) =
                backend(proof("successor"), proof("reviewed"), |_, _| {});
            if planning {
                assert!(read_removal_plan(subject(), Some(backend)).is_err());
            } else {
                assert!(matches!(
                    remove_reviewed_worktree(subject(), backend, removal()),
                    RemovalOutcome::Refused(_)
                ));
            }
            // The server reads EOF immediately after Connect, not a Work frame.
            server.join().unwrap();
        }
    }

    #[test]
    fn captured_plan_checks_project_path_and_branch_but_allows_its_display_label() {
        for changed in [None, Some("project"), Some("path"), Some("branch")] {
            let (_root, backend, server) = backend(
                proof("reviewed"),
                proof("reviewed"),
                move |stream, proof| {
                    let request: ClientEnvelope = read_frame(stream).unwrap().unwrap();
                    assert_eq!(request.proof.as_ref(), Some(proof));
                    assert_eq!(
                        request.request,
                        ClientRequest::Work(Request::RemovalPlan {
                            scope: Scope::Project(subject().card.project),
                            item: subject().path.to_str().unwrap().into(),
                            fetch: false,
                        })
                    );
                    let mut observed = plan();
                    match changed {
                        Some("project") => observed.project = "/device/other/.git".into(),
                        Some("path") => {
                            observed.inspection.path = "/device/project/.worktrees/other".into()
                        }
                        Some("branch") => observed.inspection.branch = Some("other".into()),
                        None => {}
                        _ => unreachable!(),
                    }
                    reply(
                        stream,
                        proof,
                        request.request_id,
                        ServerResponse::Work(Response::RemovalPlan(Box::new(observed))),
                    );
                },
            );
            let expected_backend = backend.clone();
            let result = read_removal_plan(subject(), Some(backend));
            if changed.is_some() {
                assert!(
                    result.is_err(),
                    "a mismatched {changed:?} must not be reviewable"
                );
            } else {
                let prepared = result.unwrap();
                assert_eq!(prepared.backend, expected_backend);
                assert_eq!(prepared.plan, plan());
                assert_ne!(prepared.plan.item, subject().path.to_str().unwrap());
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn submitted_removal_requires_matching_result_and_keeps_missing_or_wrong_results_uncertain() {
        for outcome in [
            "removed",
            "wrong_project",
            "wrong_path",
            "disconnected",
            "wrong_reply",
        ] {
            let (_root, backend, server) = backend(
                proof("reviewed"),
                proof("reviewed"),
                move |stream, proof| {
                    let request: ClientEnvelope = read_frame(stream).unwrap().unwrap();
                    assert_eq!(request.proof.as_ref(), Some(proof));
                    assert_eq!(
                        request.request,
                        ClientRequest::Work(Request::Remove(Box::new(removal())))
                    );
                    if outcome == "disconnected" {
                        stream.shutdown(std::net::Shutdown::Write).unwrap();
                        return;
                    }
                    let mut observed = removed();
                    match outcome {
                        "wrong_project" => observed.project = "/device/other/.git".into(),
                        "wrong_path" => observed.path = "/device/project/.worktrees/other".into(),
                        _ => {}
                    }
                    let response = if outcome == "wrong_reply" {
                        ServerResponse::Accepted
                    } else {
                        ServerResponse::Work(Response::Removed(Box::new(observed)))
                    };
                    reply(stream, proof, request.request_id, response);
                },
            );
            let result = remove_reviewed_worktree(subject(), backend, removal());
            match (outcome, result) {
                ("removed", RemovalOutcome::Removed(observed)) => assert_eq!(observed, removed()),
                (_, RemovalOutcome::Uncertain(_)) if outcome != "removed" => {}
                (_, actual) => panic!("{outcome}: {actual:?}"),
            }
            server.join().unwrap();
        }
    }
}
