use super::*;
use crate::model::WorktreeId;
use crate::work_ui::{Action, CardRef, Destination, Effect, Navigation, OpenCard, OpenKind, Watch};
use cosmic::iced::futures::SinkExt;

/// How often relative times on the board are redrawn.
const CLOCK: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum SessionOwner {
    Channel(ProjectId, ChannelId, CardId),
    Worktree,
    Rejected,
}

/// Built once for a visible board frame, then queried once per card. Paths
/// identify listed projects/worktrees; only exact canonical session membership
/// establishes a channel association.
struct NavigationIndex<'a> {
    app: &'a Cagents,
    projects: HashMap<&'a Path, Option<&'a SidebarProject>>,
    worktrees: HashMap<(&'a Path, &'a Path), Option<&'a sidebar::SidebarWorktree>>,
    sessions: HashMap<TerminalId, SessionOwner>,
    pull_requests: HashSet<(ProjectId, u64, &'a str)>,
    board_projects: HashSet<&'a Path>,
    closed_projects: HashSet<&'a Path>,
}

impl<'a> NavigationIndex<'a> {
    fn new(app: &'a Cagents) -> Self {
        let mut projects = HashMap::new();
        let mut worktrees = HashMap::new();
        for project in &app.sidebar_model.projects {
            projects
                .entry(project.repository.as_path())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(project));
            for worktree in &project.worktrees {
                worktrees
                    .entry((project.repository.as_path(), worktree.path.as_path()))
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(worktree));
            }
        }
        let mut cards = HashMap::new();
        for card in &app.durable.cards {
            cards
                .entry(card.id)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(card));
        }
        let mut local_owners = HashMap::new();
        let mut local_channels = HashMap::new();
        for channel in &app.durable.channels {
            for id in &channel.cards {
                local_owners
                    .entry(*id)
                    .and_modify(|owner| *owner = None)
                    .or_insert(Some(channel.id));
            }
            local_channels
                .entry(channel.id)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(channel));
        }
        let mut shared_channels = HashMap::new();
        for channel in &app.workspace_model.channels {
            shared_channels
                .entry(channel.id)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(channel));
        }
        let listed_channels = app
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .channels
                    .iter()
                    .filter(|channel| channel.deletion.is_none())
                    .map(move |channel| (project.id, channel.id))
            })
            .collect::<HashSet<_>>();
        let mut pane_counts = HashMap::<CardId, usize>::new();
        let mut scope_counts = HashMap::new();
        for view in &app.workspace_model.views {
            *scope_counts.entry(&view.scope).or_insert(0usize) += 1;
            for pane in &view.panes {
                *pane_counts.entry(pane.id).or_default() += 1;
            }
        }
        let mut sessions = HashMap::new();
        let candidate_sessions = app
            .work_ui
            .navigation_board()
            .into_iter()
            .flat_map(|board| &board.items)
            .flat_map(|item| &item.sessions)
            .map(|session| session.id)
            .collect::<HashSet<_>>();
        let pending_transfers = app
            .shared_views
            .pending_transfer_cards()
            .collect::<HashSet<_>>();
        for view in &app.workspace_model.views {
            let retired = view.retired.iter().copied().collect::<HashSet<_>>();
            for pane in &view.panes {
                let session = match &pane.kind {
                    CardKind::Chat(session) => session.id,
                    CardKind::Terminal(session) => session.id,
                    _ => continue,
                };
                if !candidate_sessions.contains(&session) {
                    continue;
                }
                let current = pane_counts[&pane.id] == 1
                    && scope_counts[&view.scope] == 1
                    && !retired.contains(&pane.id)
                    && !pending_transfers.contains(&pane.id)
                    && !app.chat_close_owned(pane.id)
                    && cards
                        .get(&pane.id)
                        .copied()
                        .flatten()
                        .is_some_and(|card| card.existence.is_open() && card.kind == pane.kind);
                let owner = match &view.scope {
                    crate::workspace_model::views::Scope::Channel(id)
                        if app.workspace_model.identity != crate::workspace_model::NO_REGISTRY
                            && app.durable.workspace_model_identity
                                == Some(app.workspace_model.identity)
                            && !app.shared_views.blocked.contains(&view.scope)
                            && current =>
                    {
                        local_channels
                            .get(id)
                            .copied()
                            .flatten()
                            .zip(shared_channels.get(id).copied().flatten())
                            .and_then(|(local, shared)| {
                                let project = projects
                                    .get(shared.repository.as_path())
                                    .copied()
                                    .flatten()?;
                                let card = cards.get(&pane.id).copied().flatten()?;
                                (local.project_id == project.id
                                    && local_owners.get(&pane.id) == Some(&Some(*id))
                                    && card.existence.is_open()
                                    && card.kind == pane.kind
                                    && listed_channels.contains(&(project.id, *id))
                                    && !app.channel_ui.is_deleting(*id))
                                .then_some(SessionOwner::Channel(project.id, *id, pane.id))
                            })
                            .unwrap_or(SessionOwner::Rejected)
                    }
                    crate::workspace_model::views::Scope::Worktree { .. }
                        if current && !local_owners.contains_key(&pane.id) =>
                    {
                        SessionOwner::Worktree
                    }
                    _ => SessionOwner::Rejected,
                };
                sessions
                    .entry(session)
                    .and_modify(|owner| *owner = SessionOwner::Rejected)
                    .or_insert(owner);
            }
        }
        let pull_requests = app
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .pull_requests
                    .iter()
                    .map(move |pr| (project.id, pr.number, pr.url.as_str()))
            })
            .collect();
        let board_projects = app
            .work_ui
            .navigation_board()
            .into_iter()
            .flat_map(|board| &board.projects)
            .map(|project| project.repository.as_path())
            .collect();
        let closed_projects = app
            .workspace_model
            .closed_projects
            .iter()
            .map(PathBuf::as_path)
            .collect();
        Self {
            app,
            projects,
            worktrees,
            sessions,
            pull_requests,
            board_projects,
            closed_projects,
        }
    }

    fn project(&self, item: &crate::work::Item) -> Option<&'a SidebarProject> {
        let project = self
            .projects
            .get(item.project.as_path())
            .copied()
            .flatten()?;
        (self.board_projects.contains(item.project.as_path())
            && project.unreachable.is_none()
            && !self.app.project_ui.closed.contains(&project.repository)
            && !self.app.project_ui.closing.contains_key(&project.id)
            && !self
                .app
                .project_ui
                .removing
                .contains_key(&project.repository)
            && !self.closed_projects.contains(project.repository.as_path()))
        .then_some(project)
    }

    fn pull_request(&self, item: &crate::work::Item) -> Destination {
        if let Some(project) = self.project(item)
            && let Some(pr) = &item.pull_request
            && self
                .pull_requests
                .contains(&(project.id, pr.number, pr.url.as_str()))
        {
            Destination::PullRequest {
                project: project.id,
                number: pr.number,
                url: pr.url.clone(),
            }
        } else if item.pull_request.is_some() {
            Destination::Unavailable(
                "This pull request is no longer in the project's current local listing.",
            )
        } else {
            Destination::Unavailable(
                "This card has no current associated conversation or registered worktree.",
            )
        }
    }

    fn destination(&self, item: &crate::work::Item) -> Destination {
        let Some(project) = self.project(item) else {
            return Destination::Unavailable(
                "This project's current local listing is unavailable or being removed.",
            );
        };
        let worktree = item
            .worktree
            .as_ref()
            .and_then(|source| {
                self.worktrees
                    .get(&(item.project.as_path(), source.path.as_path()))
                    .copied()
                    .flatten()
            })
            .filter(|worktree| worktree.deletion.is_none());
        let mut channel = None;
        let mut panes = Vec::new();
        let mut ambiguous = false;
        for session in &item.sessions {
            match self.sessions.get(&session.id) {
                Some(SessionOwner::Channel(owner_project, owner_channel, pane)) => {
                    if *owner_project != project.id {
                        ambiguous = true;
                        continue;
                    }
                    if channel.is_some_and(|channel| channel != *owner_channel) {
                        ambiguous = true;
                    }
                    channel = Some(*owner_channel);
                    panes.push((session.id, *pane));
                }
                Some(SessionOwner::Rejected) => ambiguous = true,
                Some(SessionOwner::Worktree) | None => {}
            }
        }
        if !ambiguous && let Some(channel) = channel {
            panes.sort_unstable();
            panes.dedup();
            return Destination::Channel {
                registry: self.app.workspace_model.identity,
                project: project.id,
                channel,
                panes,
                worktree: worktree.map(|worktree| (worktree.id, worktree.path.clone())),
            };
        }
        if let Some(worktree) = worktree {
            return Destination::Worktree {
                project: project.id,
                worktree: worktree.id,
                path: worktree.path.clone(),
            };
        }
        if item.worktree.is_some() {
            return Destination::Unavailable(
                "This worktree is no longer in the project's current local listing.",
            );
        }
        if ambiguous {
            return Destination::Unavailable(
                "This card's sessions belong to different channels; choose a channel in the sidebar.",
            );
        }
        self.pull_request(item)
    }
}

impl Cagents {
    fn work_navigation_close_pending(&self) -> bool {
        self.workspace.pending_close().is_some()
            || self.terminal_close.is_some()
            || !self.terminal_close_attempts.is_empty()
            || !self.queued_card_closes.is_empty()
            || self.dirty_repository_close.is_some()
            || !self.chrome_ui.closing_group.is_empty()
    }

    fn work_navigation_available(&self, card: CardId) -> bool {
        !crate::device::is_remote()
            && !self.update_prepared
            && !self.work_navigation_close_pending()
            && !self.workspace_inputs_blocked()
            && !self.focused_repository_is_saving()
            && self.workspace_surfaces_visible()
            && self.workspace.visibility(card) == Some(VisibilityClass::Visible)
            && matches!(self.cards.get(&card), Some(CardRuntime::Work))
            && self
                .card_state(card)
                .is_some_and(|state| state.kind == CardKind::Work && state.existence.is_open())
    }

    fn work_navigation(&self, card: CardId) -> Navigation {
        let available =
            self.work_navigation_available(card) && self.work_ui.navigation_board().is_some();
        let destinations = if available {
            let index = NavigationIndex::new(self);
            self.work_ui
                .navigation_board()
                .unwrap()
                .items
                .iter()
                .map(|item| {
                    (
                        CardRef {
                            project: item.project.clone(),
                            key: item.key.clone(),
                        },
                        index.destination(item),
                    )
                })
                .collect()
        } else {
            HashMap::new()
        };
        Navigation {
            origin: card,
            epoch: self.work_ui.listening_epoch().unwrap_or(0),
            available,
            destinations,
        }
    }

    pub(super) fn work_view(&self, card: CardId) -> Element<'_, Message> {
        self.work_ui
            .view_with_navigation(self.work_navigation(card))
            .map(Message::WorkUi)
    }

    fn open_work_card(&mut self, target: OpenCard, kind: OpenKind) -> Task<Message> {
        let Some(origin) = target
            .origin
            .filter(|card| self.work_navigation_available(*card))
        else {
            return Task::none();
        };
        let Some(item) = self
            .work_ui
            .navigation_board()
            .and_then(|board| board.items.iter().find(|item| target.names(item)))
        else {
            return Task::none();
        };
        let index = NavigationIndex::new(self);
        let destination = index.destination(item);
        let pull_request = index.pull_request(item);
        let navigation = Navigation {
            origin,
            epoch: self.work_ui.listening_epoch().unwrap_or(0),
            available: true,
            destinations: HashMap::from([(target.card.clone(), destination.clone())]),
        };
        if OpenCard::of(item, Some(&navigation)) != target {
            return Task::none();
        }
        let destination = match kind {
            OpenKind::Primary => destination,
            OpenKind::Worktree => match destination {
                Destination::Channel {
                    project,
                    worktree: Some((worktree, path)),
                    ..
                }
                | Destination::Worktree {
                    project,
                    worktree,
                    path,
                } => Destination::Worktree {
                    project,
                    worktree,
                    path,
                },
                _ => Destination::Unavailable(
                    "This worktree is no longer in the project's current local listing.",
                ),
            },
            OpenKind::PullRequest => pull_request,
        };
        self.work_ui.update(Action::Menu(None));
        match destination {
            Destination::Channel { channel, .. } => {
                self.update(Message::Sidebar(SidebarAction::OpenChannel {
                    channel_id: channel,
                }))
            }
            Destination::Worktree {
                project,
                worktree,
                path,
            } => self.open_work_board_worktree(project, worktree, path),
            Destination::PullRequest {
                project,
                number,
                url,
            } => match Url::parse(&url) {
                Ok(url) => self.update(Message::Sidebar(SidebarAction::OpenPullRequest {
                    project_id: project,
                    number,
                    url,
                })),
                Err(_) => Task::none(),
            },
            Destination::Unavailable(reason) => {
                self.status = Some(TransientNotice::warning(reason));
                Task::none()
            }
        }
    }

    fn open_work_board_worktree(
        &mut self,
        project: ProjectId,
        worktree: WorktreeId,
        path: PathBuf,
    ) -> Task<Message> {
        if crate::device::is_remote()
            || self.workspace_inputs_blocked()
            || self.update_prepared
            || self.focused_repository_is_saving()
            || !self.sidebar_model.project(project).is_some_and(|project| {
                project.worktrees.iter().any(|known| {
                    known.id == worktree && known.path == path && known.deletion.is_none()
                })
            })
            || self.refuse_in_deleting_worktree(&path)
        {
            return Task::none();
        }
        let (selected, task) = self.select_worktree_checked(path, false);
        if selected {
            self.sidebar_state.reveal_project(project);
            self.select_sidebar_rows(
                sidebar::SelectionRow::Worktree(project, worktree),
                sidebar::SelectionIntent::Navigate,
            );
        }
        task
    }
    /// Called before both constructor returns and in the common update
    /// postlude, after the workspace's actual visibility is current.
    pub(super) fn sync_work_watch_visibility(&mut self) {
        let visible = self.workspace_surfaces_visible()
            && self.workspace.mounted_visibility().any(|(id, visibility)| {
                visibility == VisibilityClass::Visible
                    && matches!(self.cards.get(&id), Some(CardRuntime::Work))
                    && self
                        .card_state(id)
                        .is_some_and(|card| card.kind == CardKind::Work && card.existence.is_open())
            });
        self.work_ui.set_listening(visible);
    }

    /// Only an initialized visible listener holds daemon demand. Window and
    /// keyboard focus do not hide a side-by-side Work pane.
    pub(super) fn work_subscription(&self) -> Subscription<Message> {
        let Some(epoch) = self.work_ui.listening_epoch() else {
            return Subscription::none();
        };
        let watch = Subscription::run_with(epoch, watch_board).map(Message::WorkUi);
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
            Effect::Open(target, kind) => self.open_work_card(target, kind),
            Effect::Refresh => control(
                |client| client.refresh_work_board(),
                |result| Action::Refreshed(result),
            ),
            Effect::Pin {
                project,
                item,
                column,
            } => control(
                move |client| client.pin_work_card(project, item, column),
                |result| Action::Pinned(result),
            ),
            Effect::OpenUrl(url) => match Url::parse(&url) {
                Ok(url) if matches!(url.scheme(), "https" | "http") => self.open_clicked_link(url),
                _ => Task::none(),
            },
            // Session links find the session in whichever window shows it.
            Effect::FocusSession(session) => {
                self.handle_chat_ui(chat_ui::Action::OpenSession(session.to_string()))
            }
            Effect::Copy(text) => {
                self.status = Some(TransientNotice::success(format!("Copied {text}")));
                iced::clipboard::write(text).map(cosmic::Action::App)
            }
        }
    }
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
fn watch_board(epoch: &u64) -> impl cosmic::iced::futures::Stream<Item = Action> + use<> {
    let epoch = *epoch;
    cosmic::iced::stream::channel(4, async move |mut output| {
        let (sender, receiver) = async_channel::bounded(1);
        let hangup = Arc::new(Mutex::new(None));
        let held = hangup.clone();
        let spawned = std::thread::Builder::new()
            .name("cagents-work-board".into())
            .spawn(move || {
                while !sender.is_closed() {
                    let watched = watch(epoch, &sender, &held);
                    // A connection this side gave up on must not stay open
                    // through the retry delay, where the daemon would keep
                    // queueing boards for it.
                    if let Some(hangup) = held.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        hangup.hang_up();
                    }
                    if sender.is_closed() {
                        break;
                    }
                    if let Err(error) = watched
                        && sender.send_blocking(Watch::Lost { epoch, error }).is_err()
                    {
                        break;
                    }
                    std::thread::sleep(crate::daemon_health::retry_delay());
                }
            });
        if let Err(error) = spawned {
            let _ = output
                .send(Action::Watched(Watch::Lost {
                    epoch,
                    error: format!("the board could not be watched: {error}"),
                }))
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
    epoch: u64,
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
            epoch,
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
                epoch,
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

    struct NavigationFixture {
        app: Cagents,
        origin: CardId,
        chat: CardId,
        channel: ChannelId,
        project: ProjectId,
        worktree: WorktreeId,
        item: crate::work::Item,
        _root: tempfile::TempDir,
        _runtime: tempfile::TempDir,
    }

    fn navigation_fixture() -> NavigationFixture {
        use crate::workspace_model::{
            ModelChannel, ModelProject,
            views::{Pane, Scope, View},
        };
        let root = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let directory = root.path().join("topic");
        std::fs::create_dir(&directory).unwrap();
        let repository = root.path().join(".git");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.window_width = 1_800.0;
        app.workspace.resize_viewport(1_400.0);
        app.worktree_protections
            .use_fixture_runtime(runtime.path().into());
        drop(app.new_native_chat(crate::agent::Provider::Claude, Some(directory.clone())));
        let chat = app.workspace.focused().unwrap();
        let CardRuntime::Chat(current) = app.cards.get(&chat).unwrap() else {
            panic!("draft fixture")
        };
        let target = current.target;
        let session = current.session.id;
        drop(app.handle_chat_ui(chat_ui::Action::Composer(
            target,
            widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(Arc::new(
                "Keep my unsent draft 首要".into(),
            ))),
        )));
        let project = ProjectId::new();
        let worktree = WorktreeId::new();
        let channel = ChannelId::new();
        let pane = Pane::from_card(app.card_state(chat).unwrap(), None).unwrap();
        let mut listing = SidebarProject::new(
            project,
            RepositoryId(project.as_uuid()),
            "Fixture".into(),
            repository.clone(),
            vec![
                SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    root.path().into(),
                    Some("main".into()),
                ),
                SidebarWorktree::from_durable(worktree, directory.clone(), Some("topic".into())),
            ],
            Vec::new(),
        );
        listing.channels.push(sidebar::SidebarChannel::new(
            channel,
            "Associated conversation".into(),
            false,
            None,
            false,
            1,
            true,
            1,
        ));
        app.sidebar_model.projects = vec![listing];
        let registry = uuid::Uuid::new_v4();
        let durable = Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity = Some(registry);
        durable.channels.push(crate::model::ProjectChannel {
            id: channel,
            project_id: project,
            title: "Associated conversation".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![chat],
            focused: Some(chat),
        });
        app.workspace_model.identity = registry;
        app.workspace_model.projects = vec![ModelProject {
            repository: repository.clone(),
            name: "Fixture".into(),
            kind: ProjectKind::Git,
            ordinal: 1,
        }];
        app.workspace_model.channels = vec![ModelChannel {
            id: channel,
            repository: repository.clone(),
            title: "Associated conversation".into(),
            title_source: Default::default(),
            ordinal: 1,
        }];
        let mut view = View::empty(Scope::Channel(channel));
        view.panes.push(pane);
        app.workspace_model.views = vec![view];
        drop(app.add_utility_card(CardKind::Work));
        let origin = app.workspace.focused().unwrap();
        app.workspace
            .set_card_width(origin, CardWidth::Fixed(600.0))
            .unwrap();
        app.workspace.native_scroll(0.0, 1_400.0);
        app.sync_work_watch_visibility();
        let item = crate::work::Item {
            project: repository.clone(),
            project_name: "Fixture".into(),
            key: crate::work::ItemKey::Branch("topic".into()),
            branch: Some("topic".into()),
            worktree: Some(crate::work::Worktree {
                path: directory,
                primary: false,
                locked: false,
                created_at_unix_ms: None,
            }),
            head: None,
            comparison: None,
            dirty: None,
            pull_request: None,
            linear: None,
            sessions: vec![crate::work::Session {
                id: session,
                provider: Some(crate::agent::Provider::Claude),
                title: Some("Existing conversation".into()),
                state: crate::agent::ActivityState::Working,
            }],
            last_activity: None,
            error: None,
            column: crate::work::Column::Active,
            derived: crate::work::Column::Active,
            pinned: false,
        };
        publish_navigation(&mut app, item.clone(), true);
        assert!(app.work_navigation_available(origin));
        NavigationFixture {
            app,
            origin,
            chat,
            channel,
            project,
            worktree,
            item,
            _root: root,
            _runtime: runtime,
        }
    }

    fn publish_navigation(app: &mut Cagents, item: crate::work::Item, first: bool) {
        let epoch = app.work_ui.listening_epoch().unwrap();
        let board = crate::work::Board {
            generated_at_unix_ms: 1,
            stale_after_days: 7,
            projects: vec![crate::work::Project {
                repository: item.project.clone(),
                name: item.project_name.clone(),
                root: None,
                base: None,
                scanned_at_unix_ms: None,
                scan_error: None,
                forge: crate::work::Forge::NotGitHub,
            }],
            items: vec![item],
            linear_unavailable: None,
        };
        app.work_ui.update(Action::Watched(Watch::Board {
            epoch,
            revision: 1,
            board: Arc::new(board),
            first,
        }));
    }

    #[test]
    fn board_primary_prefers_exact_canonical_channel_and_never_creates_a_pane_or_sends_draft() {
        let mut f = navigation_fixture();
        let navigation = f.app.work_navigation(f.origin);
        let target = OpenCard::of(&f.item, Some(&navigation));
        assert!(
            matches!(&target.destination, Destination::Channel { project, channel, panes, .. } if *project == f.project && *channel == f.channel && panes == &vec![(f.item.sessions[0].id, f.chat)])
        );
        let before = f
            .app
            .durable
            .cards
            .iter()
            .map(|card| card.id)
            .collect::<Vec<_>>();
        assert!(!f.app.chat_start_pending(f.chat));
        drop(f.app.update_work_ui(Action::Open(target)));
        assert_eq!(f.app.durable.active_channel, Some(f.channel));
        assert_eq!(
            f.app
                .durable
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            before
        );
        assert!(!f.app.chat_start_pending(f.chat));
        assert_eq!(
            f.app.persistent_workspace().chat_drafts[&f.chat],
            "Keep my unsent draft 首要"
        );
        f.app.sidebar_model.projects[0].worktrees.clear();
        assert!(
            matches!(NavigationIndex::new(&f.app).destination(&f.item), Destination::Channel { channel, worktree: None, .. } if channel == f.channel),
            "an independently proved conversation remains usable when an optional checkout listing disappears"
        );
    }

    #[test]
    fn board_navigation_revalidates_canonical_reassignment_and_uses_only_registered_worktree_fallback()
     {
        let mut f = navigation_fixture();
        let old = OpenCard::of(&f.item, Some(&f.app.work_navigation(f.origin)));
        f.app.workspace_model.views[0].panes.clear();
        let before = f.app.durable.cards.clone();
        assert_eq!(f.app.update_work_ui(Action::Open(old.clone())).units(), 0);
        assert_eq!(
            f.app.update_work_ui(Action::OpenWorktree(old)).units(),
            0,
            "menu command cannot bypass stale source proof"
        );
        assert!(f.app.durable.active_channel.is_none());
        assert_eq!(f.app.durable.cards, before);
        let current = OpenCard::of(&f.item, Some(&f.app.work_navigation(f.origin)));
        assert!(
            matches!(&current.destination, Destination::Worktree { project, worktree, .. } if *project == f.project && *worktree == f.worktree)
        );
        drop(f.app.update_work_ui(Action::Open(current)));
        assert!(f.app.durable.active_channel.is_none());
        assert_eq!(
            f.app.active_workspace.as_deref(),
            f.item
                .worktree
                .as_ref()
                .map(|worktree| worktree.path.as_path())
        );
        assert_eq!(
            f.app.durable.cards, before,
            "fallback must not create a default pane"
        );
        assert_eq!(
            f.app.persistent_workspace().chat_drafts[&f.chat],
            "Keep my unsent draft 首要"
        );
        f.app.sidebar_model.projects[0].worktrees.clear();
        let destination = NavigationIndex::new(&f.app).destination(&f.item);
        assert!(
            matches!(destination, Destination::Unavailable(_)),
            "missing local registration cannot authorize the same checkout path"
        );
    }

    #[test]
    fn board_navigation_rejects_duplicate_retired_blocked_or_foreign_canonical_membership_and_hidden_origin()
     {
        for case in [
            "duplicate",
            "duplicate-scope",
            "retired",
            "blocked",
            "different-project",
            "missing-channel",
            "hidden-origin",
            "update-freeze",
            "palette",
            "closing-pane",
            "watch-reconnect",
            "registry-replaced",
            "worktree-replaced",
        ] {
            let mut f = navigation_fixture();
            let target = OpenCard::of(&f.item, Some(&f.app.work_navigation(f.origin)));
            match case {
                "duplicate" => {
                    let pane = f.app.workspace_model.views[0].panes[0].clone();
                    f.app.workspace_model.views[0].panes.push(pane);
                }
                "duplicate-scope" => {
                    let mut view = f.app.workspace_model.views[0].clone();
                    view.panes.clear();
                    f.app.workspace_model.views.push(view);
                }
                "retired" => f.app.workspace_model.views[0].retired.push(f.chat),
                "blocked" => {
                    f.app
                        .shared_views
                        .blocked
                        .insert(crate::workspace_model::views::Scope::Channel(f.channel));
                }
                "different-project" => {
                    Arc::make_mut(&mut f.app.durable).channels[0].project_id = ProjectId::new()
                }
                "missing-channel" => f.app.workspace_model.channels.clear(),
                "hidden-origin" => {
                    f.app.workspace.native_scroll(9_000.0, 300.0);
                }
                "update-freeze" => f.app.update_prepared = true,
                "palette" => {
                    drop(f.app.update_palette(palette_ui::Action::Open));
                }
                "closing-pane" => {
                    f.app.workspace.request_close(f.chat).unwrap();
                }
                "watch-reconnect" => {
                    f.app.work_ui.set_listening(false);
                    f.app.work_ui.set_listening(true);
                }
                "registry-replaced" => f.app.workspace_model.identity = uuid::Uuid::new_v4(),
                "worktree-replaced" => {
                    f.app.sidebar_model.projects[0].worktrees[1].id = WorktreeId::new()
                }
                _ => unreachable!(),
            }
            let before = f.app.durable.cards.clone();
            assert_eq!(
                f.app.update_work_ui(Action::Open(target)).units(),
                0,
                "{case}"
            );
            assert!(f.app.durable.active_channel.is_none(), "{case}");
            assert_eq!(f.app.durable.cards, before, "{case}");
            assert_eq!(
                f.app.persistent_workspace().chat_drafts[&f.chat],
                "Keep my unsent draft 首要",
                "{case}"
            );
        }
    }

    #[test]
    fn ambiguous_channels_use_only_verified_worktree_and_unlisted_pull_requests_cannot_redirect() {
        let mut f = navigation_fixture();
        let second_channel = ChannelId::new();
        let second_card = CardId::new();
        let second_session = TerminalId::new();
        let mut card = f.app.card_state(f.chat).unwrap().clone();
        card.id = second_card;
        let CardKind::Chat(session) = &mut card.kind else {
            unreachable!()
        };
        session.id = second_session;
        let pane = crate::workspace_model::views::Pane::from_card(&card, None).unwrap();
        let durable = Arc::make_mut(&mut f.app.durable);
        durable.cards.push(card);
        let mut local = durable.channels[0].clone();
        local.id = second_channel;
        local.cards = vec![second_card];
        local.focused = Some(second_card);
        durable.channels.push(local);
        let mut shared = f.app.workspace_model.channels[0].clone();
        shared.id = second_channel;
        f.app.workspace_model.channels.push(shared);
        let mut view = crate::workspace_model::views::View::empty(
            crate::workspace_model::views::Scope::Channel(second_channel),
        );
        view.panes.push(pane);
        f.app.workspace_model.views.push(view);
        f.app.sidebar_model.projects[0]
            .channels
            .push(sidebar::SidebarChannel::new(
                second_channel,
                "Other conversation".into(),
                false,
                None,
                false,
                1,
                true,
                2,
            ));
        let mut session = f.item.sessions[0].clone();
        session.id = second_session;
        f.item.sessions.push(session);
        publish_navigation(&mut f.app, f.item.clone(), true);
        assert!(
            matches!(NavigationIndex::new(&f.app).destination(&f.item), Destination::Worktree { worktree, .. } if worktree == f.worktree),
            "two independently proved channels cannot choose the first or current conversation"
        );
        f.item.worktree = None;
        publish_navigation(&mut f.app, f.item.clone(), true);
        assert!(
            matches!(NavigationIndex::new(&f.app).destination(&f.item), Destination::Unavailable(reason) if reason.contains("different channels"))
        );
        let pr = crate::work::PullRequest {
            number: 42,
            title: "Public task".into(),
            url: "https://github.com/example/project/pull/42".into(),
            state: crate::work::PullRequestState::Open,
            draft: false,
            head_oid: "abc".into(),
            contains_local_head: false,
            checks: None,
            review: None,
            updated_at: String::new(),
        };
        f.item.sessions.clear();
        f.item.pull_request = Some(pr.clone());
        // A PR fallback needs the same current local listing, not merely a URL in an old board.
        publish_navigation(&mut f.app, f.item.clone(), true);
        assert!(matches!(
            NavigationIndex::new(&f.app).destination(&f.item),
            Destination::Unavailable(_)
        ));
        f.app.sidebar_model.projects[0]
            .pull_requests
            .push(sidebar::SidebarPullRequest::new(
                f.project,
                pr.number,
                pr.title.clone(),
                Url::parse(&pr.url).unwrap(),
                "topic".into(),
                None,
                false,
                sidebar::CheckStatus::Unknown,
            ));
        assert!(
            matches!(NavigationIndex::new(&f.app).destination(&f.item), Destination::PullRequest { project, number: 42, .. } if project == f.project)
        );
        // A claimed checkout which disappeared must not silently become the PR.
        f.item.worktree = Some(crate::work::Worktree {
            path: f._root.path().join("removed"),
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        publish_navigation(&mut f.app, f.item.clone(), true);
        assert!(matches!(
            NavigationIndex::new(&f.app).destination(&f.item),
            Destination::Unavailable(_)
        ));
    }

    #[test]
    fn real_board_native_press_focus_and_keyboard_follow_exact_canonical_channel_without_retargeting()
     {
        use iced::advanced::{
            clipboard,
            widget::{Operation, operation},
        };
        use iced::runtime::{UserInterface, user_interface::Cache};
        #[derive(Default)]
        struct Focus(Option<widget::Id>);
        impl Operation for Focus {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if state.is_focused() {
                    assert!(self.0.is_none());
                    self.0 = id.cloned();
                }
            }
        }
        fn button(
            ui: &UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
        ) -> iced_accessibility::accesskit::Rect {
            let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
            let node = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some("Open topic")
                        && !node.node().is_disabled()
                })
                .expect("actual enabled board primary button");
            node.node().bounds().unwrap()
        }
        for (width, replace) in [
            (620.0, false),
            (620.0, true),
            (1_200.0, false),
            (1_200.0, true),
        ] {
            let mut f = navigation_fixture();
            let mut renderer = crate::app::tests::headless_renderer();
            let size = iced::Size::new(width, 600.0);
            let expected = OpenCard::of(&f.item, Some(&f.app.work_navigation(f.origin)));
            let id: widget::Id = format!(
                "work-open:{}:{}",
                f.item.project.display(),
                f.item.key.storage_key()
            )
            .into();
            let (cache, point) = {
                let mut ui = UserInterface::build(
                    f.app.work_view(f.origin),
                    size,
                    Cache::new(),
                    &mut renderer,
                );
                ui.draw(
                    &mut renderer,
                    &cosmic::theme::active(),
                    &iced::advanced::renderer::Style::default(),
                    iced::mouse::Cursor::Unavailable,
                );
                let bounds = button(&ui);
                assert!(
                    bounds.x0 >= 0.0
                        && bounds.x1 <= f64::from(width)
                        && bounds.x1 > bounds.x0
                        && bounds.y1 > bounds.y0
                );
                let point = iced::Point::new(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                );
                ui.operate(
                    &renderer,
                    &mut operation::focusable::focus::<()>(id.clone()),
                );
                let mut focus = Focus::default();
                ui.operate(&renderer, &mut focus);
                assert_eq!(focus.0, Some(id.clone()));
                let mut messages = Vec::new();
                ui.update(
                    &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    ))],
                    iced::mouse::Cursor::Available(point),
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert!(messages.is_empty(), "row opens only on release");
                (ui.into_cache(), point)
            };
            let current_channel = if replace {
                let channel = ChannelId::new();
                let pane = f.app.workspace_model.views[0].panes.remove(0);
                f.app.workspace_model.views[0].retired.push(pane.id);
                let mut view = crate::workspace_model::views::View::empty(
                    crate::workspace_model::views::Scope::Channel(channel),
                );
                view.panes.push(pane);
                f.app.workspace_model.views.push(view);
                let mut shared = f.app.workspace_model.channels[0].clone();
                shared.id = channel;
                shared.title = "Transferred".into();
                f.app.workspace_model.channels.push(shared);
                let durable = Arc::make_mut(&mut f.app.durable);
                durable.channels[0].cards.clear();
                let mut local = durable.channels[0].clone();
                local.id = channel;
                local.cards = vec![f.chat];
                local.focused = Some(f.chat);
                durable.channels.push(local);
                f.app.sidebar_model.projects[0]
                    .channels
                    .push(sidebar::SidebarChannel::new(
                        channel,
                        "Transferred".into(),
                        false,
                        None,
                        false,
                        1,
                        true,
                        2,
                    ));
                assert_eq!(
                    f.app.update_work_ui(Action::Open(expected.clone())).units(),
                    0,
                    "a queued old callback must not navigate to the transferred owner"
                );
                channel
            } else {
                f.channel
            };
            let current = OpenCard::of(&f.item, Some(&f.app.work_navigation(f.origin)));
            assert!(
                matches!(&current.destination, Destination::Channel { channel, .. } if *channel == current_channel)
            );
            let mut ui =
                UserInterface::build(f.app.work_view(f.origin), size, cache, &mut renderer);
            let bounds = button(&ui);
            assert!(
                f64::from(point.x) >= bounds.x0
                    && f64::from(point.x) <= bounds.x1
                    && f64::from(point.y) >= bounds.y0
                    && f64::from(point.y) <= bounds.y1,
                "same target remains beneath held pointer"
            );
            let mut focus = Focus::default();
            ui.operate(&renderer, &mut focus);
            assert_eq!(focus.0, (!replace).then_some(id.clone()));
            let mut messages = Vec::new();
            ui.update(
                &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                ))],
                iced::mouse::Cursor::Available(point),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            if replace {
                assert!(
                    messages.is_empty(),
                    "held press cannot transfer to another canonical channel"
                );
            } else {
                assert!(
                    matches!(messages.as_slice(), [Message::WorkUi(Action::Open(target))] if target == &current)
                );
            }
            messages.clear();
            ui.operate(&renderer, &mut operation::focusable::focus::<()>(id));
            let key = keyboard::Key::Named(keyboard::key::Named::Enter);
            ui.update(
                &[iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: key.clone(),
                    modified_key: key,
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                })],
                iced::mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                matches!(messages.as_slice(), [Message::WorkUi(Action::Open(target))] if target == &current),
                "fresh keyboard activation uses exact current destination"
            );
            drop(ui);
            let Message::WorkUi(action) = messages.pop().unwrap() else {
                unreachable!()
            };
            drop(f.app.update_work_ui(action));
            assert_eq!(f.app.durable.active_channel, Some(current_channel));
            assert_eq!(
                f.app.persistent_workspace().chat_drafts[&f.chat],
                "Keep my unsent draft 首要"
            );
        }
    }

    #[test]
    fn actual_visible_work_keeps_its_listener_without_keyboard_or_window_focus() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.window_width = 1_800.0;
        app.workspace.resize_viewport(1_400.0);
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        drop(app.add_utility_card(CardKind::Work));
        let work = app.workspace.focused().unwrap();
        app.workspace
            .set_card_width(work, CardWidth::Fixed(600.0))
            .unwrap();
        app.workspace.native_scroll(0.0, 1_400.0);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        // This is the exported constructor hook, before any App message.
        app.sync_work_watch_visibility();
        let original = app.work_ui.listening_epoch().unwrap();

        drop(app.add_utility_card(CardKind::Settings));
        let settings = app.workspace.focused().unwrap();
        app.workspace
            .set_card_width(settings, CardWidth::Fixed(600.0))
            .unwrap();
        app.workspace.native_scroll(0.0, 1_400.0);
        app.workspace.focus_without_scrolling(settings).unwrap();
        assert_ne!(work, settings);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        app.refresh_ui.focused = false;
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), Some(original));
        app.refresh_ui.focused = true;
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), Some(original));

        // A real camera/viewport change leaves the still-open Work offscreen.
        app.workspace.native_scroll(1_000.0, 500.0);
        assert_ne!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        assert!(app.card_state(work).unwrap().existence.is_open());
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        app.workspace.native_scroll(0.0, 1_400.0);
        app.sync_work_watch_visibility();
        let revealed = app.work_ui.listening_epoch().unwrap();
        assert!(revealed > original);

        // Current Carson's overlay helper, not the removed main-view switch.
        app.window_width = 400.0;
        app.context_inspector_open = true;
        assert!(app.context_inspector_is_overlay());
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        app.context_inspector_open = false;
        app.sync_work_watch_visibility();
        assert!(app.work_ui.listening_epoch().unwrap() > revealed);
    }

    #[test]
    fn a_visible_failed_work_placeholder_has_no_listener_until_its_runtime_is_work() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.window_width = 1_800.0;
        app.workspace.resize_viewport(1_400.0);
        drop(app.add_utility_card(CardKind::Work));
        let work = app.workspace.focused().unwrap();
        app.workspace.native_scroll(0.0, 1_400.0);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        assert!(app.card_state(work).unwrap().existence.is_open());
        assert!(matches!(app.cards.get(&work), Some(CardRuntime::Work)));

        // The synthetic latency constructor makes this exact replacement:
        // durable kind and on-screen geometry alone do not render the board.
        app.cards.insert(
            work,
            CardRuntime::Failed("Synthetic failure placeholder".into()),
        );
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        assert_eq!(
            app.workspace.visibility(work),
            Some(VisibilityClass::Visible)
        );
        assert!(app.card_state(work).unwrap().existence.is_open());

        app.cards.insert(work, CardRuntime::Work);
        app.sync_work_watch_visibility();
        let original = app.work_ui.listening_epoch().unwrap();
        app.cards
            .insert(work, CardRuntime::Failed("Runtime replaced".into()));
        app.sync_work_watch_visibility();
        assert_eq!(app.work_ui.listening_epoch(), None);
        app.cards.insert(work, CardRuntime::Work);
        app.sync_work_watch_visibility();
        assert!(app.work_ui.listening_epoch().unwrap() > original);
        app.refresh_ui.focused = false;
        app.sync_work_watch_visibility();
        assert!(app.work_ui.listening_epoch().is_some());
    }
}
