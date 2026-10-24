use super::*;
use crate::{
    channel_work::Home,
    channel_work_api as api,
    channel_work_ui::{self as ui, Effect, Failure},
    model::WorktreeId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Surface {
    Conversations,
    Work,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Navigation {
    pub(super) home: Home,
    pub(super) generation: uuid::Uuid,
    pub(super) workspace: u64,
    pub(super) surface: Surface,
}

pub(super) struct State {
    pub(super) view: ui::State,
    pub(super) retire_candidates: HashSet<Home>,
    routes: HashMap<Home, Route>,
    main: bool,
    title: String,
    generation: uuid::Uuid,
    restore_card: Option<CardId>,
    deferred: Option<Deferred>,
    navigation_view: Option<ui::ViewOwner>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            view: ui::State::default(),
            retire_candidates: HashSet::new(),
            routes: HashMap::new(),
            main: false,
            title: String::new(),
            generation: uuid::Uuid::new_v4(),
            restore_card: None,
            deferred: None,
            navigation_view: None,
        }
    }
}

enum Deferred {
    Navigation {
        registry: uuid::Uuid,
        repository: Option<PathBuf>,
        message: Box<Message>,
    },
    PullRequest {
        url: Url,
        destination: LinkOpenDestination,
    },
    Pane(PaneDestination),
}

// A palette result can belong to a different scope. Capture that placement
// before its handler has a chance to select a channel or worktree.
struct PaneDestination {
    registry: uuid::Uuid,
    card: CardId,
    kind: CardKind,
    scope: Option<crate::workspace_model::views::Scope>,
    directory: Option<PathBuf>,
    home: Option<Home>,
    worktree: Option<(ProjectId, WorktreeId)>,
}

impl PaneDestination {
    fn matches(&self, other: &Self) -> bool {
        self.registry == other.registry
            && self.card == other.card
            && self.scope == other.scope
            && self.directory == other.directory
            && self.home == other.home
            && self.worktree == other.worktree
            && (crate::workspace_model::views::same_resource(&self.kind, &other.kind)
                || self.kind == other.kind)
    }
}

#[derive(Clone)]
struct Route {
    socket: PathBuf,
    proof: Arc<Mutex<Option<crate::model::HostSessionProof>>>,
}

impl Cagents {
    pub(super) fn channel_work_is_main(&self) -> bool {
        self.channel_work.main && self.channel_work.view.is_open()
    }

    pub(super) fn channel_home(&self) -> Option<(Home, String)> {
        if self.channel_work_is_main() {
            return Some((
                self.channel_work.view.home()?,
                self.channel_work.title.clone(),
            ));
        }
        let channel = self.workspace_model.channel(self.durable.active_channel?)?;
        Some((
            Home {
                registry: self.workspace_model.identity,
                channel: channel.id,
                creation_ordinal: channel.ordinal,
            },
            channel.title.clone(),
        ))
    }

    pub(super) fn channel_navigation(&mut self, navigation: Navigation) -> Task<Message> {
        if navigation.generation != self.channel_work.generation
            || navigation.workspace != self.workspace_view_generation
            || self.channel_home().map(|value| value.0) != Some(navigation.home)
        {
            return Task::none();
        }
        match navigation.surface {
            Surface::Work => self.open_channel_work(navigation.home.channel),
            Surface::Conversations => {
                if !self.channel_work_is_main() {
                    return Task::none();
                }
                let Some(owner) = self.channel_work.view.owner() else {
                    return Task::none();
                };
                self.update_channel_work(ui::Action::Close(owner))
            }
        }
    }

    pub(super) fn channel_home_generation(&self) -> uuid::Uuid {
        self.channel_work.generation
    }

    pub(super) fn open_channel_work(&mut self, channel: ChannelId) -> Task<Message> {
        if self.channel_work.view.has_confirmation() {
            return Task::none();
        }
        let Some(channel) = self.workspace_model.channel(channel) else {
            return Task::none();
        };
        let home = Home {
            registry: self.workspace_model.identity,
            channel: channel.id,
            creation_ordinal: channel.ordinal,
        };
        let title = channel.title.clone();
        let was_main = self.channel_work_is_main();
        let effect = self.channel_work.view.open(home, title);
        if matches!(&effect, Some(Effect::Read { home: accepted, .. }) if *accepted == home) {
            // An explicit accepted open selects its destination even when the
            // existing view owner is unchanged. Coalesced reads and draft
            // Keep/Focus effects do not change navigation ownership.
            self.channel_work.navigation_view = None;
        }
        // The leaf may still own the old Home while a dirty draft asks Keep /
        // Discard. Mount that original view, without changing source or route.
        if self.channel_work.view.is_open() {
            self.channel_work.main = true;
            if !was_main {
                self.channel_work.restore_card = self.workspace.focused();
                self.channel_work.generation = uuid::Uuid::new_v4();
                if let Some(current) = self.channel_work.view.home().and_then(|home| {
                    self.workspace_model
                        .channel(home.channel)
                        .filter(|channel| channel.ordinal == home.creation_ordinal)
                }) {
                    self.channel_work.title = current.title.clone();
                }
                self.set_workspace_surfaces_visible(false);
            }
        }
        effect.map_or_else(Task::none, |effect| self.channel_work_effect(effect))
    }

    /// Only stable navigation is deferred through the Work draft guard. Pane
    /// commands, sends, launches and deletion are never captured or replayed.
    pub(super) fn intercept_channel_work_navigation(
        &mut self,
        message: &Message,
    ) -> Option<Task<Message>> {
        if !self.channel_work_is_main() || self.modal_is_open() {
            return None;
        }
        match message {
            Message::KeyEvent { event, captured } => {
                if self.palette.is_open() || self.chrome_ui.is_open() {
                    return None;
                }
                // Preserve the global clipboard race fence even when a native
                // Work field consumed this copy/cut before the subscription.
                if let keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(key),
                    modifiers,
                    ..
                } = event
                    && modifiers.command()
                    && (key.eq_ignore_ascii_case("c") || key.eq_ignore_ascii_case("x"))
                {
                    chat_ui::invalidate_copy_intent();
                }
                if let keyboard::Event::ModifiersChanged(modifiers) = event {
                    self.keyboard_modifiers = *modifiers;
                }
                if !captured
                    && let Some(command) = workspace_widget::global_shortcut(
                        event,
                        &self.settings.keyboard_shortcuts,
                        workspace_widget::FocusedPane::Other,
                    )
                    && let Some(task) = self.channel_work_global_command(command)
                {
                    return Some(task);
                }
                return Some(if !captured && is_escape_press(event) {
                    self.escape_channel_work()
                } else {
                    Task::none()
                });
            }
            Message::FocusAdjacent(_)
            | Message::TerminalDropped { .. }
            | Message::TerminalFind(
                terminal_ui::Action::PasteClipboard(_) | terminal_ui::Action::Paste(..),
            ) => return Some(Task::none()),
            Message::Repository(action) if !repository_action_allowed_while_modal(action) => {
                return Some(Task::none());
            }
            Message::ChatUi(action) if !action.allowed_while_modal() => {
                return Some(Task::none());
            }
            Message::WorkUi(action) if !action.background() => {
                return Some(Task::none());
            }
            Message::AutomationUi(action) if !action.background() => {
                return Some(Task::none());
            }
            Message::SelectSettingsCategory(_)
            | Message::SettingsSearchChanged(_)
            | Message::SettingsChanged { .. }
            | Message::SettingsDraftChanged { .. }
            | Message::ResetSettings => return Some(Task::none()),
            Message::RunCommand(command) => {
                return Some(
                    self.channel_work_global_command(*command)
                        .unwrap_or_else(Task::none),
                );
            }
            Message::Sidebar(SidebarAction::OpenChannel { channel_id }) => {
                return Some(self.open_channel_work(*channel_id));
            }
            Message::Palette(palette_ui::Action::FocusPane(card)) => {
                if self.channel_work.view.has_confirmation() {
                    return Some(Task::none());
                }
                let Some(destination) = self.channel_pane_destination(*card) else {
                    return Some(Task::none());
                };
                // Closing the palette cannot select its result. Only the
                // accepted Work leave below may perform that navigation.
                drop(self.update_palette(palette_ui::Action::Close));
                self.channel_work.deferred = Some(Deferred::Pane(destination));
                let owner = self.channel_work.view.owner()?;
                return Some(self.update_channel_work(ui::Action::Close(owner)));
            }
            _ => {}
        }
        let navigation = matches!(
            message,
            Message::Sidebar(
                SidebarAction::OpenRepository { .. }
                    | SidebarAction::OpenWorktree { .. }
                    | SidebarAction::OpenSettings
                    | SidebarAction::OpenAutomations
                    | SidebarAction::OpenWork
            ) | Message::FocusCard(_)
                | Message::AccountUi(account_ui::Action::OpenPane)
        );
        if !navigation {
            return None;
        }
        if self.channel_work.view.has_confirmation() {
            return Some(Task::none());
        }
        if let Message::FocusCard(card) = message
            && (!self.workspace.order().any(|id| id == *card)
                || !self
                    .card_state(*card)
                    .is_some_and(|card| card.existence.is_open()))
        {
            return Some(Task::none());
        }
        let repository = match message {
            Message::Sidebar(SidebarAction::OpenRepository {
                project_id,
                repository,
            }) => {
                if !self
                    .sidebar_model
                    .project(*project_id)
                    .is_some_and(|project| project.repository == *repository)
                {
                    return Some(Task::none());
                }
                Some(repository.clone())
            }
            Message::Sidebar(SidebarAction::OpenWorktree {
                project_id,
                worktree_id,
                path,
            }) => {
                let Some(project) = self.sidebar_model.project(*project_id) else {
                    return Some(Task::none());
                };
                if !project
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.id == *worktree_id && worktree.path == *path)
                {
                    return Some(Task::none());
                }
                Some(project.repository.clone())
            }
            _ => None,
        };
        self.channel_work.deferred = Some(Deferred::Navigation {
            registry: self.workspace_model.identity,
            repository,
            message: Box::new(message.clone()),
        });
        let owner = self.channel_work.view.owner()?;
        Some(self.update_channel_work(ui::Action::Close(owner)))
    }

    fn channel_pane_destination(&self, card: CardId) -> Option<PaneDestination> {
        use crate::workspace_model::views::Scope;
        let state = self
            .card_state(card)
            .filter(|card| card.existence.is_open())?;
        let mut owners = self.workspace_model.views.iter().flat_map(|view| {
            view.panes
                .iter()
                .filter(move |pane| pane.id == card)
                .map(move |pane| (view, pane))
        });
        let (scope, directory, home, worktree) = match owners.next() {
            Some((view, pane)) => {
                if owners.next().is_some()
                    || !(crate::workspace_model::views::same_resource(&state.kind, &pane.kind)
                        || state.kind == pane.kind)
                {
                    return None;
                }
                match &view.scope {
                    Scope::Channel(id) => {
                        let channel = self.workspace_model.channel(*id)?;
                        let mut members = self
                            .durable
                            .channels
                            .iter()
                            .filter(|channel| channel.cards.contains(&card));
                        if members.next()?.id != *id || members.next().is_some() {
                            return None;
                        }
                        (
                            Some(view.scope.clone()),
                            pane.scope.clone(),
                            Some(Home {
                                registry: self.workspace_model.identity,
                                channel: *id,
                                creation_ordinal: channel.ordinal,
                            }),
                            None,
                        )
                    }
                    Scope::Worktree { repository, path } => {
                        if self
                            .durable
                            .channels
                            .iter()
                            .any(|channel| channel.cards.contains(&card))
                        {
                            return None;
                        }
                        let mut rows = self
                            .sidebar_model
                            .projects
                            .iter()
                            .filter(|project| project.repository == *repository)
                            .flat_map(|project| {
                                project
                                    .worktrees
                                    .iter()
                                    .filter(|worktree| worktree.path == *path)
                                    .map(move |worktree| (project.id, worktree.id))
                            });
                        let row = rows.next()?;
                        if rows.next().is_some() {
                            return None;
                        }
                        (
                            Some(view.scope.clone()),
                            pane.scope.clone(),
                            None,
                            Some(row),
                        )
                    }
                }
            }
            None if state.kind.is_utility() && self.workspace.order().any(|id| id == card) => {
                (None, None, None, None)
            }
            None => return None,
        };
        Some(PaneDestination {
            registry: self.workspace_model.identity,
            card,
            kind: state.kind.clone(),
            scope,
            directory,
            home,
            worktree,
        })
    }

    fn channel_work_global_command(
        &mut self,
        command: workspace_widget::GlobalShortcut,
    ) -> Option<Task<Message>> {
        use workspace_widget::GlobalShortcut;
        match command {
            GlobalShortcut::CommandPalette => Some(self.update_palette(palette_ui::Action::Open)),
            GlobalShortcut::ToggleProjects => {
                Some(self.update_chrome(chrome_ui::Action::ToggleProjects))
            }
            GlobalShortcut::FocusProjectSearch => Some(self.update(Message::FocusProjectSearch)),
            GlobalShortcut::OpenSettings => self
                .intercept_channel_work_navigation(&Message::Sidebar(SidebarAction::OpenSettings)),
            _ => None,
        }
    }

    fn finish_channel_conversations(&mut self) -> Task<Message> {
        self.channel_work.main = false;
        self.channel_work.generation = uuid::Uuid::new_v4();
        self.set_workspace_surfaces_visible(self.workspace_surfaces_visible());
        if let Some(deferred) = self.channel_work.deferred.take() {
            let Deferred::Navigation {
                registry,
                repository: captured_repository,
                message,
            } = deferred
            else {
                return match deferred {
                    Deferred::PullRequest { url, destination } => {
                        self.open_link_in(url, destination)
                    }
                    Deferred::Pane(captured) => {
                        if self
                            .channel_pane_destination(captured.card)
                            .is_some_and(|current| captured.matches(&current))
                        {
                            self.update(Message::Palette(palette_ui::Action::FocusPane(
                                captured.card,
                            )))
                        } else {
                            self.status = Some(TransientNotice::warning(
                                "That pane changed scope. Choose it again from the current workspace.",
                            ));
                            Task::none()
                        }
                    }
                    Deferred::Navigation { .. } => unreachable!(),
                };
            };
            let valid = registry == self.workspace_model.identity
                && match message.as_ref() {
                    Message::Sidebar(SidebarAction::OpenRepository {
                        project_id,
                        repository,
                    }) => self
                        .sidebar_model
                        .project(*project_id)
                        .is_some_and(|project| {
                            project.repository == *repository
                                && captured_repository.as_ref() == Some(&project.repository)
                        }),
                    Message::Sidebar(SidebarAction::OpenWorktree {
                        project_id,
                        worktree_id,
                        path,
                    }) => self
                        .sidebar_model
                        .project(*project_id)
                        .is_some_and(|project| {
                            captured_repository.as_ref() == Some(&project.repository)
                                && project.worktrees.iter().any(|worktree| {
                                    worktree.id == *worktree_id && worktree.path == *path
                                })
                        }),
                    Message::FocusCard(card) => {
                        self.workspace.order().any(|id| id == *card)
                            && self
                                .card_state(*card)
                                .is_some_and(|card| card.existence.is_open())
                    }
                    Message::Sidebar(
                        SidebarAction::OpenSettings
                        | SidebarAction::OpenAutomations
                        | SidebarAction::OpenWork,
                    )
                    | Message::AccountUi(account_ui::Action::OpenPane) => true,
                    _ => false,
                };
            return if valid {
                self.update(*message)
            } else {
                self.status = Some(TransientNotice::warning(
                    "The navigation destination changed. Choose it again from the current workspace.",
                ));
                Task::none()
            };
        }
        let focused = self
            .channel_work
            .restore_card
            .take()
            .filter(|card| {
                self.workspace.order().any(|id| id == *card)
                    && self
                        .card_state(*card)
                        .is_some_and(|card| card.existence.is_open())
            })
            .or_else(|| {
                self.workspace.focused().filter(|card| {
                    self.card_state(*card)
                        .is_some_and(|card| card.existence.is_open())
                })
            });
        focused.map_or_else(
            || {
                iced::widget::operation::focus(widget::Id::new(TOOLBAR_FOCUS_ID))
                    .map(cosmic::Action::App)
            },
            |card| self.focus_workspace_card(card),
        )
    }

    pub(super) fn channel_work_subscription(&self) -> Subscription<Message> {
        if (self.channel_work_is_main() || self.context_inspector_open)
            && self.refresh_ui.focused
            && !self.modal_is_open()
            && self.channel_work.view.is_open()
            && !self.channel_work.view.has_confirmation()
        {
            cosmic::iced::time::every(Duration::from_secs(5)).map(|_| Message::ChannelWorkPoll)
        } else {
            Subscription::none()
        }
    }

    pub(super) fn escape_channel_work(&mut self) -> Task<Message> {
        let was_confirming = self.channel_work.view.has_confirmation();
        let effect = self.channel_work.view.escape();
        if was_confirming && !self.channel_work.view.has_confirmation() {
            self.channel_work.deferred = None;
        }
        effect.map_or_else(Task::none, |effect| self.channel_work_effect(effect))
    }

    pub(super) fn update_channel_work(&mut self, action: ui::Action) -> Task<Message> {
        let keep = matches!(&action, ui::Action::Keep(..));
        let effect = self.channel_work.view.update(action);
        if keep && !self.channel_work.view.has_confirmation() {
            self.channel_work.deferred = None;
        }
        effect.map_or_else(Task::none, |effect| self.channel_work_effect(effect))
    }

    pub(super) fn channel_work_effect(&mut self, effect: Effect) -> Task<Message> {
        let home = match &effect {
            Effect::Read { home, .. }
            | Effect::Refresh { home, .. }
            | Effect::Lookup { home, .. }
            | Effect::OpenPullRequest { home, .. }
            | Effect::Close { home } => *home,
            Effect::Mutate { mutation, .. } => mutation.home,
            Effect::Focus { owner, .. } => owner.home,
        };
        let changed_view = self.channel_work_is_main()
            && self.channel_work.navigation_view != self.channel_work.view.owner();
        if changed_view {
            self.channel_work.navigation_view = self.channel_work.view.owner();
            self.channel_work.generation = uuid::Uuid::new_v4();
        }
        let selection = if matches!(&effect, Effect::Read { .. })
            && self.channel_work_is_main()
            && self.channel_work.view.home() == Some(home)
        {
            // Only a newly accepted open (including Discard's destination)
            // selects scope. Polls, invalidations and page reads do not revisit
            // channel selection or replace its captured focus destination.
            let current = self
                .workspace_model
                .channel(home.channel)
                .filter(|channel| {
                    self.workspace_model.identity == home.registry
                        && channel.ordinal == home.creation_ordinal
                })
                .map(|channel| channel.title.clone());
            if let Some(title) = current {
                self.channel_work.view.set_title(home, title.clone());
                self.channel_work.title = title;
                if changed_view {
                    let task = self.select_channel(home.channel);
                    self.channel_work.restore_card = self.workspace.focused();
                    task
                } else {
                    Task::none()
                }
            } else {
                Task::none()
            }
        } else {
            Task::none()
        };
        if !self.channel_work.routes.contains_key(&home)
            && let Ok(socket) = crate::terminal::ipc::daemon_socket_path()
        {
            self.channel_work.routes.insert(
                home,
                Route {
                    socket,
                    proof: Arc::new(Mutex::new(None)),
                },
            );
        }
        let keep = self
            .channel_work
            .view
            .pending_homes()
            .chain(self.channel_work.view.home())
            .collect::<HashSet<_>>();
        self.channel_work
            .routes
            .retain(|home, _| keep.contains(home));
        let route = self.channel_work.routes.get(&home).cloned();
        let task = match effect {
            Effect::Read { ticket, home, page } => request(
                route,
                home,
                api::Request::Read { home, page },
                move |result| ui::Action::Loaded {
                    ticket,
                    result: result.and_then(snapshot),
                },
            ),
            Effect::Refresh { ticket, home } => {
                request(route, home, api::Request::Refresh { home }, move |result| {
                    ui::Action::Loaded {
                        ticket,
                        result: result.and_then(snapshot),
                    }
                })
            }
            Effect::Mutate { ticket, mutation } => request(
                route,
                home,
                api::Request::Mutate {
                    mutation,
                    caller: None,
                },
                move |result| ui::Action::Applied {
                    ticket,
                    result: result.and_then(|response| match response {
                        api::Response::Applied(reply) => Ok(reply),
                        _ => Err(Failure::Unavailable),
                    }),
                },
            ),
            Effect::Lookup {
                ticket,
                home,
                operation,
            } => cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let mut client = connect(route.as_ref(), home, true)?;
                    match send(&mut client, api::Request::Receipt { home, operation })? {
                        api::Response::Receipt(receipt) => Ok(receipt),
                        _ => Err(Failure::Unavailable),
                    }
                })
                .await
                .unwrap_or(Err(Failure::Unavailable));
                Message::ChannelWork(ui::Action::LookedUp {
                    ticket,
                    operation,
                    result,
                })
            }),
            Effect::OpenPullRequest {
                home: _,
                key: _,
                url,
            } => {
                let gesture = self.keyboard_modifiers.control().then_some(
                    if self.keyboard_modifiers.shift() {
                        LinkOpenGesture::ControlShiftClick
                    } else {
                        LinkOpenGesture::ControlClick
                    },
                );
                let destination = self.settings.link_destination(gesture);
                if self.channel_work_is_main()
                    && cfg!(feature = "integrated-browser")
                    && destination == LinkOpenDestination::IntegratedBrowser
                {
                    // Keep the exact approved URL/destination through the draft
                    // guard; never open an invisible browser behind Work.
                    self.channel_work.deferred = Some(Deferred::PullRequest { url, destination });
                    let owner = self
                        .channel_work
                        .view
                        .owner()
                        .expect("main Work owns a view");
                    self.update_channel_work(ui::Action::Close(owner))
                } else {
                    self.open_link_in(url, destination)
                }
            }
            Effect::Close { .. } if self.channel_work.main => self.finish_channel_conversations(),
            Effect::Close { .. } => self.set_context_inspector_open(false),
            Effect::Focus { owner, id } => {
                if self.channel_work.view.owner() == Some(owner) {
                    iced::widget::operation::focus(id).map(cosmic::Action::App)
                } else {
                    Task::none()
                }
            }
        };
        Task::batch([selection, task])
    }

    /// View publication supplies the settled revision. The daemon checks that
    /// exact revision and durable work/history together before retiring a home.
    pub(super) fn retire_settled_empty_channels(&mut self) {
        let candidates = self
            .channel_work
            .retire_candidates
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for captured in candidates {
            let channel = captured.channel;
            let Some(metadata) = self.workspace_model.channel(channel) else {
                self.channel_work.retire_candidates.remove(&captured);
                continue;
            };
            if self.workspace_model.identity != captured.registry
                || metadata.ordinal != captured.creation_ordinal
            {
                self.channel_work.retire_candidates.remove(&captured);
                continue;
            }
            let Some(view) =
                self.workspace_model.views.iter().find(|view| {
                    view.scope == crate::workspace_model::views::Scope::Channel(channel)
                })
            else {
                continue;
            };
            if !view.panes.is_empty() {
                continue;
            }
            let edit = crate::workspace_model::ModelEdit::RetireEmptyChannel {
                home: captured,
                expected_view_revision: view.revision,
            };
            if self.workspace_model_tx.try_send(edit).is_ok() {
                self.channel_work.retire_candidates.remove(&captured);
            }
        }
    }
}

fn snapshot(response: api::Response) -> Result<crate::channel_work::Snapshot, Failure> {
    match response {
        api::Response::Snapshot(snapshot) => Ok(snapshot),
        _ => Err(Failure::Unavailable),
    }
}

fn connect(
    route: Option<&Route>,
    home: Home,
    reconcile: bool,
) -> Result<crate::control::ControlClient, Failure> {
    let route = route.ok_or(Failure::Unavailable)?;
    let mut client =
        crate::control::ControlClient::connect(&route.socket).map_err(|_| Failure::Unavailable)?;
    let mut proof = route.proof.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(expected) = proof.as_ref() {
        if client.proof() != expected {
            if !reconcile {
                return Err(Failure::Unavailable);
            }
            // Only the user's explicit Check result can reconcile a restarted
            // endpoint. Prove the original durable home using read-only calls
            // before updating its transport proof; never resend a mutation.
            let model = client
                .read_workspace_model()
                .map_err(|_| Failure::Unavailable)?;
            if model.identity != home.registry {
                return Err(Failure::Unavailable);
            }
            let api::Response::Home { home: current, .. } =
                send(&mut client, api::Request::ReadHome { home })?
            else {
                return Err(Failure::Unavailable);
            };
            current.validate().map_err(|_| Failure::Unavailable)?;
            if current != home {
                return Err(Failure::Unavailable);
            }
            *proof = Some(client.proof().clone());
        }
    } else {
        let model = client
            .read_workspace_model()
            .map_err(|_| Failure::Unavailable)?;
        if model.identity != home.registry {
            return Err(Failure::Unavailable);
        }
        *proof = Some(client.proof().clone());
    }
    Ok(client)
}

fn send(
    client: &mut crate::control::ControlClient,
    request: api::Request,
) -> Result<api::Response, Failure> {
    // This is the interactive user's UI. Provider/SDK authority never travels
    // through this adapter; the session CLI preserves its injected authority.
    match client
        .channel_work(
            crate::agent_execution::Authority::default(),
            request,
            Duration::from_secs(160),
        )
        .map_err(|_| Failure::Unavailable)?
    {
        api::Response::Failed(api::Failure::Refused(reason)) => Err(Failure::Refused(reason)),
        api::Response::Failed(api::Failure::Uncertain) => Err(Failure::Unavailable),
        response => Ok(response),
    }
}

fn request(
    route: Option<Route>,
    home: Home,
    request: api::Request,
    answer: impl FnOnce(Result<api::Response, Failure>) -> ui::Action + Send + 'static,
) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            send(&mut connect(route.as_ref(), home, false)?, request)
        })
        .await
        .unwrap_or(Err(Failure::Unavailable));
        Message::ChannelWork(answer(result))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_model::{
        ModelChannel, ModelEdit, WorkspaceModel,
        views::{Scope, View},
    };
    use cosmic::iced::futures::StreamExt;

    fn captured() -> Home {
        Home {
            registry: uuid::Uuid::new_v4(),
            channel: ChannelId::new(),
            creation_ordinal: 4,
        }
    }

    fn model(home: Home) -> WorkspaceModel {
        WorkspaceModel {
            identity: home.registry,
            channels: vec![ModelChannel {
                id: home.channel,
                repository: "/project".into(),
                title: "Work".into(),
                title_source: Default::default(),
                ordinal: home.creation_ordinal,
            }],
            views: vec![View {
                scope: Scope::Channel(home.channel),
                revision: 7,
                panes: vec![],
                retired: vec![],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn deferred_empty_retirement_never_rebinds_to_replacement_registry_or_channel_incarnation() {
        let original = captured();
        for current in [
            Home {
                registry: uuid::Uuid::new_v4(),
                ..original
            },
            Home {
                creation_ordinal: original.creation_ordinal + 1,
                ..original
            },
            original,
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let (send, received) = async_channel::unbounded();
            app.workspace_model_tx = send;
            app.workspace_model = model(current);
            app.channel_work.retire_candidates.insert(original);
            app.retire_settled_empty_channels();
            if current == original {
                assert_eq!(
                    received.try_recv().unwrap(),
                    ModelEdit::RetireEmptyChannel {
                        home: original,
                        expected_view_revision: 7
                    }
                );
            } else {
                assert!(
                    received.try_recv().is_err(),
                    "a delayed old close must not act on a replacement home"
                );
            }
            assert!(app.channel_work.retire_candidates.is_empty());
        }
    }

    #[test]
    fn close_intent_captures_only_a_confirmed_original_home() {
        let original = captured();
        for ordinal in [
            original.creation_ordinal,
            crate::workspace_model::PENDING_ORDINAL,
        ] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            let card = app.durable.cards[0].id;
            app.workspace_model = model(Home {
                creation_ordinal: ordinal,
                ..original
            });
            Arc::make_mut(&mut app.durable)
                .channels
                .push(crate::model::ProjectChannel {
                    id: original.channel,
                    project_id: ProjectId::new(),
                    title: "Work".into(),
                    title_source: Default::default(),
                    ordinal,
                    shared: true,
                    cards: vec![card],
                    focused: Some(card),
                });
            drop(app.close_emptied_channels(&[card]));
            assert_eq!(
                app.channel_work.retire_candidates.contains(&original),
                ordinal != crate::workspace_model::PENDING_ORDINAL
            );
        }
    }

    #[test]
    fn work_invalidation_never_skips_a_delayed_canonical_channel_edit() {
        let home = captured();
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.workspace_model = model(home);
        app.workspace_model.sequence = 8;
        let Effect::Read { ticket, .. } = app.channel_work.view.open(home, "Work".into()).unwrap()
        else {
            panic!("read")
        };
        app.channel_work.view.update(ui::Action::Loaded {
            ticket,
            result: Ok(crate::channel_work::Snapshot {
                home,
                lifecycle: crate::channel_work::HomeLifecycle::Active,
                revision: 0,
                entries: vec![],
                history: vec![],
                next_current: None,
                next_history: None,
            }),
        });
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Edited {
                sequence: 10,
                edit: ModelEdit::ChannelWorkChanged { home },
            },
        ));
        assert_eq!(
            app.workspace_model.sequence, 8,
            "work reads have their own revision; they cannot acknowledge unseen canonical edits"
        );
        assert!(
            app.channel_work.view.refresh().is_none(),
            "the exact open home was invalidated even without advancing the model watermark"
        );
        drop(app.apply_workspace_model_update(
            super::super::workspace_model_sync::ModelUpdate::Edited {
                sequence: 9,
                edit: ModelEdit::DeleteChannel { id: home.channel },
            },
        ));
        assert!(app.workspace_model.channel(home.channel).is_none());
        assert_eq!(app.workspace_model.sequence, 9);
    }

    type Native<'a> = iced::runtime::UserInterface<'a, Message, cosmic::Theme, cosmic::Renderer>;
    type Cache = iced::runtime::user_interface::Cache;

    fn main_fixture() -> (Cagents, Home) {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let home = captured();
        app.workspace_model = model(home);
        Arc::make_mut(&mut app.durable).active_channel = Some(home.channel);
        let Effect::Read { ticket, .. } =
            app.channel_work.view.open(home, "Original".into()).unwrap()
        else {
            panic!()
        };
        app.channel_work.view.update(ui::Action::Loaded {
            ticket,
            result: Ok(crate::channel_work::Snapshot {
                home,
                lifecycle: crate::channel_work::HomeLifecycle::Active,
                revision: 1,
                entries: Vec::new(),
                history: Vec::new(),
                next_current: None,
                next_history: None,
            }),
        });
        app.channel_work.main = true;
        app.channel_work.title = "Original".into();
        app.channel_work.navigation_view = app.channel_work.view.owner();
        app.channel_work.routes.insert(
            home,
            Route {
                socket: "/fixture/never-connected.sock".into(),
                proof: Arc::new(Mutex::new(None)),
            },
        );
        (app, home)
    }

    fn two_channels(app: &mut Cagents, home: Home) -> (ChannelId, CardId, CardId) {
        use crate::workspace_model::views::Pane;
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Fixture".into(),
            "/project".into(),
            vec![sidebar::SidebarWorktree::from_durable(
                WorktreeId::new(),
                "/project".into(),
                Some("main".into()),
            )],
            vec![],
        ));
        let mut first = super::super::tests::terminal_card_state();
        let mut second = super::super::tests::terminal_card_state();
        for card in [&mut first, &mut second] {
            if let CardKind::Terminal(session) = &mut card.kind {
                session.launch.working_directory = "/project".into();
            }
        }
        let (a, b) = (first.id, second.id);
        let other = ChannelId::new();
        app.workspace_model.channels.push(ModelChannel {
            id: other,
            repository: "/project".into(),
            title: "Other".into(),
            title_source: Default::default(),
            ordinal: home.creation_ordinal + 1,
        });
        app.workspace_model.views[0].panes = vec![Pane::from_card(&first, None).unwrap()];
        app.workspace_model.views.push(View {
            scope: Scope::Channel(other),
            revision: 1,
            panes: vec![Pane::from_card(&second, None).unwrap()],
            retired: vec![],
        });
        app.durable = Arc::new(WorkspaceState {
            cards: vec![first.clone(), second],
            focused: Some(a),
            active_channel: Some(home.channel),
            channels: [
                (home.channel, home.creation_ordinal, a),
                (other, home.creation_ordinal + 1, b),
            ]
            .into_iter()
            .map(|(id, ordinal, card)| crate::model::ProjectChannel {
                id,
                project_id: project,
                title: "Fixture".into(),
                title_source: Default::default(),
                ordinal,
                shared: true,
                cards: vec![card],
                focused: Some(card),
            })
            .collect(),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&WorkspaceState {
            cards: vec![first],
            focused: Some(a),
            ..WorkspaceState::default()
        });
        app.active_workspace = Some("/project".into());
        app.cards.insert(a, CardRuntime::Loading);
        app.cards.insert(b, CardRuntime::Loading);
        (other, a, b)
    }

    fn history_snapshot(home: Home, next_history: Option<u64>) -> crate::channel_work::Snapshot {
        crate::channel_work::Snapshot {
            home,
            lifecycle: crate::channel_work::HomeLifecycle::Active,
            revision: 1,
            entries: vec![],
            history: vec![crate::channel_work::Entry {
                id: uuid::Uuid::new_v4(),
                target: crate::channel_work::Target::PullRequest(
                    crate::github_identity::PullRequestKey::new(
                        crate::github_identity::GitHubRepoKey::new(
                            "github.com",
                            None,
                            "fixture",
                            "work",
                        )
                        .unwrap(),
                        1,
                    )
                    .unwrap(),
                ),
                claims: vec![],
                contexts: vec![],
                suppressed: false,
                user_ended: true,
                observation: None,
                last_terminal_generation: 0,
                ever_claimed: true,
                revision: 1,
            }],
            next_current: None,
            next_history,
        }
    }

    #[test]
    fn background_work_poll_invalidation_and_pages_preserve_the_accepted_workspace_scope() {
        let mut renderer = super::super::tests::headless_renderer();
        for source in ["poll", "invalidation", "earlier", "latest"] {
            let (mut app, home) = main_fixture();
            let (other, _, target) = two_channels(&mut app, home);
            let owner = app.channel_work.view.owner().unwrap();
            app.workspace_model.channels[0].title = "Renamed while reading".into();
            let snapshot = history_snapshot(home, Some(42));
            assert!(snapshot.validate().is_ok());
            let Some(Effect::Read { ticket, .. }) = app.channel_work.view.refresh() else {
                panic!("fixture read")
            };
            app.channel_work.view.update(ui::Action::Loaded {
                ticket,
                result: Ok(snapshot.clone()),
            });
            if matches!(source, "earlier" | "latest") {
                app.channel_work
                    .view
                    .update(ui::Action::Tab(owner, crate::channel_work::Bucket::History));
            }
            if source == "latest" {
                let Some(Effect::Read { ticket, .. }) =
                    app.channel_work.view.update(ui::Action::Earlier(owner, 42))
                else {
                    panic!("fixture earlier page")
                };
                let mut earlier = snapshot;
                earlier.next_history = None;
                app.channel_work.view.update(ui::Action::Loaded {
                    ticket,
                    result: Ok(earlier),
                });
            }
            // A canonical scope reconciliation can be accepted while a Work
            // view remains open. Fetching that view must not reinstall its old
            // scope or recapture the pane the user will return to.
            drop(app.select_channel(other));
            assert_eq!(app.durable.active_channel, Some(other));
            assert_eq!(app.workspace.focused(), Some(target));
            let before = (
                app.active_workspace.clone(),
                app.durable.active_channel,
                app.workspace.order().collect::<Vec<_>>(),
                app.workspace.focused(),
                app.workspace_view_generation,
                app.channel_work.generation,
                app.channel_work.restore_card,
            );
            match source {
                "poll" => drop(app.update(Message::ChannelWorkPoll)),
                "invalidation" => drop(app.apply_workspace_model_update(
                    super::super::workspace_model_sync::ModelUpdate::Edited {
                        sequence: app.workspace_model.sequence + 1,
                        edit: ModelEdit::ChannelWorkChanged { home },
                    },
                )),
                "earlier" => drop(app.update(Message::ChannelWork(ui::Action::Earlier(owner, 42)))),
                "latest" => drop(app.update(Message::ChannelWork(ui::Action::Latest(owner)))),
                _ => unreachable!(),
            }
            assert_eq!(
                (
                    app.active_workspace.clone(),
                    app.durable.active_channel,
                    app.workspace.order().collect::<Vec<_>>(),
                    app.workspace.focused(),
                    app.workspace_view_generation,
                    app.channel_work.generation,
                    app.channel_work.restore_card,
                ),
                before,
                "{source} fetch must not select or refocus a channel"
            );
            assert_eq!(app.channel_work.view.owner(), Some(owner));
            assert_eq!(app.channel_home().unwrap().1, "Renamed while reading");
            let labels = leaf_labels(&app, &mut renderer);
            assert!(labels.iter().any(|label| label == "Renamed while reading"));
            assert!(!labels.iter().any(|label| label == "Original"));
            assert!(
                app.channel_work.view.refresh().is_none(),
                "{source} really began a read, and coalesces another"
            );
        }
    }

    #[test]
    fn accepted_new_work_home_and_dirty_discard_still_select_and_reopen_the_destination() {
        let mut renderer = super::super::tests::headless_renderer();
        for dirty in [false, true] {
            let (mut app, home) = main_fixture();
            let (other, original, target) = two_channels(&mut app, home);
            let owner = app.channel_work.view.owner().unwrap();
            if dirty {
                dirty_native_form(&mut app, &mut renderer);
            }
            drop(app.open_channel_work(other));
            if dirty {
                assert!(app.channel_work.view.has_confirmation());
                assert_eq!(app.channel_work.view.owner(), Some(owner));
                assert_eq!(app.durable.active_channel, Some(home.channel));
                assert_eq!(app.workspace.focused(), Some(original));
                let mut ui = native(&app, Cache::new(), &mut renderer, 700.0);
                let point = bounds(&ui, "Keep editing").center();
                let keep = click(&mut ui, &mut renderer, point)
                    .into_iter()
                    .find(|message| matches!(message, Message::ChannelWork(ui::Action::Keep(..))))
                    .unwrap();
                let _ = ui.into_cache();
                drop(app.update(keep));
                assert!(!app.channel_work.view.has_confirmation());
                assert_eq!(app.durable.active_channel, Some(home.channel));
                assert_eq!(app.workspace.focused(), Some(original));
                drop(app.open_channel_work(other));
                discard_native(&mut app, &mut renderer);
            }
            let accepted = app.channel_work.view.owner().unwrap();
            assert_ne!(accepted, owner);
            assert_eq!(accepted.home.channel, other);
            assert_eq!(app.durable.active_channel, Some(other));
            assert_eq!(app.workspace.focused(), Some(target));

            drop(app.update_channel_work(ui::Action::Close(accepted)));
            assert!(!app.channel_work_is_main());
            drop(app.select_channel(home.channel));
            assert_eq!(app.workspace.focused(), Some(original));
            drop(app.open_channel_work(other));
            assert_ne!(app.channel_work.view.owner(), Some(accepted));
            assert_eq!(app.durable.active_channel, Some(other));
            assert_eq!(app.workspace.focused(), Some(target));
        }
        // Same-Home explicit opens keep the leaf/form owner. Unlike a
        // background read, that accepted user navigation must select Home A
        // again after another canonical scope B was installed.
        for dirty in [false, true] {
            let (mut app, home) = main_fixture();
            let (other, original, target) = two_channels(&mut app, home);
            let owner = app.channel_work.view.owner().unwrap();
            let effect = app
                .channel_work
                .view
                .refresh()
                .expect("fixture background read");
            let Effect::Read { ticket, .. } = &effect else {
                panic!("fixture background read")
            };
            let ticket = *ticket;
            drop(app.channel_work_effect(effect));
            drop(app.update_channel_work(ui::Action::Loaded {
                ticket,
                result: Ok(history_snapshot(home, None)),
            }));
            if dirty {
                dirty_native_form(&mut app, &mut renderer);
            }
            drop(app.select_channel(other));
            assert_eq!(app.workspace.focused(), Some(target));
            drop(app.open_channel_work(home.channel));
            assert_eq!(app.channel_work.view.owner(), Some(owner));
            assert_eq!(app.durable.active_channel, Some(home.channel));
            assert_eq!(app.workspace.focused(), Some(original));
            assert!(!app.channel_work.view.has_confirmation());
            // A preserved dirty form still asks for confirmation on close.
            drop(app.update_channel_work(ui::Action::Close(owner)));
            assert_eq!(app.channel_work.view.has_confirmation(), dirty);
        }
    }

    #[test]
    fn exact_home_title_refresh_preserves_dirty_draft_and_inflight_read() {
        let mut renderer = super::super::tests::headless_renderer();
        let (mut app, home) = main_fixture();
        dirty_native_form(&mut app, &mut renderer);
        let owner = app.channel_work.view.owner().unwrap();
        let Some(Effect::Read { ticket, .. }) = app.channel_work.view.refresh() else {
            panic!("fixture inflight read")
        };
        let before = (
            app.channel_work.generation,
            app.workspace_view_generation,
            app.channel_work.restore_card,
            app.durable.active_channel,
        );
        for wrong_home in [
            Home {
                registry: uuid::Uuid::new_v4(),
                ..home
            },
            Home {
                channel: ChannelId::new(),
                ..home
            },
            Home {
                creation_ordinal: home.creation_ordinal + 1,
                ..home
            },
        ] {
            app.channel_work
                .view
                .set_title(wrong_home, "Wrong channel".into());
            let labels = leaf_labels(&app, &mut renderer);
            assert!(labels.iter().any(|label| label == "Original"));
            assert!(!labels.iter().any(|label| label == "Wrong channel"));
        }
        app.channel_work
            .view
            .set_title(home, "Canonical rename".into());
        let labels = leaf_labels(&app, &mut renderer);
        assert!(labels.iter().any(|label| label == "Canonical rename"));
        assert!(!labels.iter().any(|label| label == "Original"));
        assert_eq!(app.channel_work.view.owner(), Some(owner));
        assert!(!app.channel_work.view.has_confirmation());
        assert!(
            app.channel_work.view.refresh().is_none(),
            "the original read is still loading"
        );
        drop(app.update_channel_work(ui::Action::Loaded {
            ticket,
            result: Ok(history_snapshot(home, None)),
        }));
        assert!(
            app.channel_work.view.refresh().is_some(),
            "the original matching reply still settles the read"
        );
        assert_eq!(
            (
                app.channel_work.generation,
                app.workspace_view_generation,
                app.channel_work.restore_card,
                app.durable.active_channel,
            ),
            before,
        );
        // The title update neither discards the real edited draft nor turns
        // closing into an unguarded navigation.
        drop(app.update_channel_work(ui::Action::Close(owner)));
        assert!(app.channel_work.view.has_confirmation());
    }

    fn leaf_labels(app: &Cagents, renderer: &mut cosmic::Renderer) -> Vec<String> {
        // Mount only the Work leaf: Chrome's separate title must not satisfy
        // the rendered heading assertion.
        let mut ui = iced::runtime::UserInterface::build(
            app.channel_work.view.view(700.0, 600.0),
            iced::Size::new(700.0, 600.0),
            Cache::new(),
            renderer,
        );
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect()
    }

    #[test]
    fn main_work_rejects_queued_workboard_query_but_newer_watch_and_visible_input_survive() {
        use crate::work_ui::{Action as BoardAction, Watch};
        let mut renderer = super::super::tests::headless_renderer();
        let (mut app, home) = visible_workboard_fixture();
        let mut tree = workboard_native(&app, &mut renderer);
        let point = bounds(&tree, "List, newest first").center();
        let mut messages = click(&mut tree, &mut renderer, point);
        assert!(matches!(
            messages.as_slice(),
            [Message::WorkUi(BoardAction::Layout(
                crate::work_ui::Layout::List
            ))]
        ));
        let _ = tree.into_cache();
        drop(app.update(messages.remove(0)));
        let queued = workboard_query_callback(&app, &mut renderer);
        drop(app.open_channel_work(home.channel));
        assert!(app.channel_work_is_main());
        let ui = native(&app, Cache::new(), &mut renderer, 1_400.0);
        assert!(
            !native_labels(&ui)
                .iter()
                .any(|label| label == "Filter cards")
        );
        let _ = ui.into_cache();
        drop(app.update(queued));
        let ui = workboard_native(&app, &mut renderer);
        assert_eq!(workboard_query_value(&ui), "");
        assert!(
            native_labels(&ui)
                .iter()
                .any(|label| label == "Open branch")
        );
        let _ = ui.into_cache();

        drop(
            app.update(Message::WorkUi(BoardAction::Watched(Watch::Board {
                revision: 2,
                board: workboard_snapshot("updated-branch"),
                first: false,
            }))),
        );
        drop(app.update(Message::WorkUi(BoardAction::Tick)));
        let ui = workboard_native(&app, &mut renderer);
        assert_eq!(workboard_query_value(&ui), "");
        let labels = native_labels(&ui);
        assert!(labels.iter().any(|label| label == "Open updated-branch"));
        assert!(!labels.iter().any(|label| label == "Open branch"));
        let _ = ui.into_cache();
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        assert!(!app.channel_work_is_main());
        let fresh = workboard_query_callback(&app, &mut renderer);
        drop(app.update(fresh));
        let ui = workboard_native(&app, &mut renderer);
        assert_eq!(workboard_query_value(&ui), "z");
        assert!(
            native_labels(&ui)
                .iter()
                .any(|label| label == "No cards match")
        );
        assert!(
            !native_labels(&ui)
                .iter()
                .any(|label| label == "Open updated-branch")
        );
    }

    #[test]
    fn main_work_rejects_queued_workboard_pin_but_refresh_completion_and_visible_pin_survive() {
        use crate::work_ui::Action as BoardAction;
        let mut renderer = super::super::tests::headless_renderer();
        let (mut app, home) = visible_workboard_fixture();
        let mut ui = workboard_native(&app, &mut renderer);
        let point = bounds(&ui, "Rescan projects and poll GitHub").center();
        let mut refresh = click(&mut ui, &mut renderer, point);
        assert!(matches!(
            refresh.as_slice(),
            [Message::WorkUi(BoardAction::Refresh)]
        ));
        let _ = ui.into_cache();
        // These tasks carry the real control requests but are never polled.
        drop(app.update(refresh.remove(0)));
        let ui = workboard_native(&app, &mut renderer);
        assert!(
            !native_labels(&ui)
                .iter()
                .any(|label| label == "Rescan projects and poll GitHub")
        );
        let before = bounds(&ui, "Open branch");
        let _ = ui.into_cache();
        let pin = workboard_pin_callback(&mut app, &mut renderer);
        drop(app.open_channel_work(home.channel));
        assert!(app.channel_work_is_main());
        drop(app.update(pin));
        let ui = workboard_native(&app, &mut renderer);
        assert_eq!(
            bounds(&ui, "Open branch"),
            before,
            "hidden Pin cannot optimistically move its card"
        );
        let _ = ui.into_cache();
        drop(app.update(Message::WorkUi(BoardAction::Refreshed(Err(
            "fixture refresh completed".into(),
        )))));
        let mut ui = workboard_native(&app, &mut renderer);
        assert!(
            rendered_text(&mut ui, &renderer)
                .iter()
                .any(|text| text == "fixture refresh completed")
        );
        let labels = native_labels(&ui);
        assert!(
            labels
                .iter()
                .any(|label| label == "Rescan projects and poll GitHub")
        );
        assert_eq!(bounds(&ui, "Open branch").x, before.x);
        let _ = ui.into_cache();
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        assert!(!app.channel_work_is_main());
        let fresh = workboard_pin_callback(&mut app, &mut renderer);
        drop(app.update(fresh));
        let ui = workboard_native(&app, &mut renderer);
        assert!(
            bounds(&ui, "Open branch").x > before.x,
            "the same visible native Pin still moves the card to Stale"
        );
    }

    #[test]
    fn main_work_rejects_hidden_automation_fields_and_save_but_preserves_completion_and_visible_save()
     {
        use crate::automation_ui::{Action as AutomationAction, Field};
        let mut renderer = super::super::tests::headless_renderer();
        let (mut app, home) = main_fixture();
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        drop(app.open_utility_card(CardKind::Automations));
        app.automation_ui.set_choices(
            vec![crate::automation_ui::TargetChoice {
                label: "Fixture".into(),
                target: crate::agent_execution::Target {
                    repository: "/fixture".into(),
                    directory: "/fixture".into(),
                    placement: None,
                },
                defaults: Default::default(),
            }],
            vec![crate::automation_ui::AccountChoice {
                label: "Synthetic Claude".into(),
                provider: crate::agent::Provider::Claude,
                id: crate::account::Account::system(crate::agent::Provider::Claude).id(),
            }],
            Default::default(),
        );
        app.automation_ui.update(AutomationAction::New);
        app.automation_ui.update(AutomationAction::Field(
            Field::Name,
            "Fixture automation".into(),
        ));
        app.automation_ui
            .update(AutomationAction::Field(Field::Timezone, "UTC".into()));
        app.automation_ui.update(AutomationAction::Prompt(
            iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(
                Arc::new("Synthetic fixture prompt; never dispatched".into()),
            )),
        ));
        app.automation_ui.update(AutomationAction::Prompt(
            iced::widget::text_editor::Action::SelectAll,
        ));
        let queued_field = utility_input_callbacks(&app, &mut renderer, false, "Name", "z");
        let mut tree = utility_native(&app, &mut renderer, false);
        // Row::Wrapping currently omits AX children (issue1490). Inspect the
        // real button layout through Operation, then activate it by pointer.
        let point = workboard_popup_button(&mut tree, &renderer, "Save automation").center();
        let queued_save = click(&mut tree, &mut renderer, point);
        assert!(
            matches!(
                queued_save.as_slice(),
                [
                    Message::AutomationUi(AutomationAction::Prompt(
                        iced::widget::text_editor::Action::ClearSelection
                    )),
                    Message::AutomationUi(AutomationAction::Save),
                ]
            ),
            "exact queued Save click callbacks at {point:?}: {queued_save:?}; AX names: {:?}",
            native_labels(&tree)
        );
        let before = native_field_value(&tree, "Name");
        let prompt_selection = automation_prompt_selection(&tree);
        assert_ne!(prompt_selection.0, prompt_selection.1);
        let _ = tree.into_cache();
        drop(app.open_channel_work(home.channel));
        let mut tree = native(&app, Cache::new(), &mut renderer, 1_400.0);
        assert!(
            !native_labels(&tree)
                .iter()
                .any(|label| label == "Save automation")
        );
        let hidden_text = rendered_text(&mut tree, &renderer);
        assert!(
            !hidden_text.iter().any(|text| text == "Save automation"),
            "hidden utility footer must not be mounted: {hidden_text:?}"
        );
        let _ = tree.into_cache();
        for message in queued_field {
            drop(app.update(message));
        }
        for message in queued_save {
            drop(app.update(message));
        }
        drop(app.update(Message::AutomationUi(AutomationAction::Tick)));
        let tree = utility_native(&app, &mut renderer, false);
        assert_eq!(native_field_value(&tree, "Name"), before);
        assert_eq!(automation_prompt_selection(&tree), prompt_selection);
        let _ = tree.into_cache();
        assert_eq!(
            app.automation_ui.draft_close_blocker(),
            Some(
                "The automation has unsaved changes. Keep editing or discard the draft before closing the window."
            )
        );
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        let fresh_field = utility_input_callbacks(&app, &mut renderer, false, "Name", "z");
        for message in fresh_field {
            drop(app.update(message));
        }
        let mut tree = utility_native(&app, &mut renderer, false);
        assert_ne!(native_field_value(&tree, "Name"), before);
        let prompt_selection = automation_prompt_selection(&tree);
        assert_eq!(prompt_selection.0, prompt_selection.1);
        // Row::Wrapping currently omits AX children (issue1490). Inspect the
        // real button layout through Operation, then activate it by pointer.
        let point = workboard_popup_button(&mut tree, &renderer, "Save automation").center();
        let messages = click(&mut tree, &mut renderer, point);
        assert!(
            matches!(
                messages.as_slice(),
                [
                    Message::AutomationUi(AutomationAction::Prompt(
                        iced::widget::text_editor::Action::ClearSelection
                    )),
                    Message::AutomationUi(AutomationAction::Save),
                ]
            ),
            "exact visible Save click callbacks at {point:?}: {messages:?}; AX names: {:?}",
            native_labels(&tree)
        );
        let _ = tree.into_cache();
        // Accepted tasks are dropped: no daemon/provider request is polled.
        for message in messages {
            drop(app.update(message));
        }
        assert_eq!(
            app.automation_ui.draft_close_blocker(),
            Some("Wait for the automation save to finish before closing the window.")
        );

        // A request accepted while the pane was visible can finish hidden.
        let (mut app, home) = main_fixture();
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        let Some(crate::automation_ui::Effect::Request { sequence, .. }) =
            app.automation_ui.update(AutomationAction::Refresh)
        else {
            panic!("accepted fixture read")
        };
        drop(app.open_channel_work(home.channel));
        drop(
            app.update(Message::AutomationUi(AutomationAction::Received {
                sequence,
                result: Arc::new(Err("fixture automation read completed".into())),
            })),
        );
        let mut tree = utility_native(&app, &mut renderer, false);
        let text = rendered_text(&mut tree, &renderer);
        assert!(text.iter().any(|text| text == "Couldn't load automations"));
        assert!(
            text.iter()
                .any(|text| text == "fixture automation read completed")
        );
        assert!(native_labels(&tree).iter().any(|label| label == "Retry"));
    }

    #[test]
    fn main_work_rejects_hidden_settings_fields_and_search_but_preserves_accepted_autosave() {
        let mut renderer = super::super::tests::headless_renderer();
        let (mut app, home) = main_fixture();
        drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
        drop(app.update(Message::SelectSettingsCategory(SettingsCategory::Terminal)));
        let first = utility_input_callbacks(&app, &mut renderer, true, "Font family", "x");
        assert!(matches!(
            first.as_slice(),
            [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(),
                Message::SettingsDraftChanged { field: SettingsField::TerminalFontFamily, .. })
        ));
        for message in first {
            drop(app.update(message));
        }
        let accepted = app.settings_draft.terminal_font_family.clone();
        let (generation, _) = app.settings_pending[&SettingsField::TerminalFontFamily].clone();
        let queued_field = utility_input_callbacks(&app, &mut renderer, true, "Font family", "z");
        let queued_search =
            utility_input_callbacks(&app, &mut renderer, true, "Search settings", "z");
        assert!(
            matches!(queued_search.as_slice(), [Message::UtilityPaneUi(inner)]
            if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "z"))
        );
        drop(app.open_channel_work(home.channel));
        let tree = native(&app, Cache::new(), &mut renderer, 1_400.0);
        assert!(
            !native_labels(&tree)
                .iter()
                .any(|label| label == "Search settings")
        );
        let _ = tree.into_cache();
        for message in queued_field {
            drop(app.update(message));
        }
        for message in queued_search {
            drop(app.update(message));
        }
        let tree = utility_native(&app, &mut renderer, true);
        assert_eq!(native_field_value(&tree, "Font family"), accepted);
        assert_eq!(native_field_value(&tree, "Search settings"), "");
        let _ = tree.into_cache();
        assert_eq!(
            app.settings_pending[&SettingsField::TerminalFontFamily].0,
            generation
        );
        assert!(app.settings_path.is_none());
        // This debounce belongs to the earlier accepted visible field, not
        // the refused hidden edit. Drop its save task without filesystem IO.
        drop(app.update(Message::SettingsAutosave {
            field: SettingsField::TerminalFontFamily,
            generation,
        }));
        assert!(
            !app.settings_pending
                .contains_key(&SettingsField::TerminalFontFamily)
        );
        assert_eq!(app.settings.terminal_font_family, accepted);
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        let fresh = utility_input_callbacks(&app, &mut renderer, true, "Font family", "z");
        for message in fresh {
            drop(app.update(message));
        }
        assert_ne!(app.settings_draft.terminal_font_family, accepted);
        let fresh = utility_input_callbacks(&app, &mut renderer, true, "Search settings", "z");
        for message in fresh {
            drop(app.update(message));
        }
        assert_eq!(app.settings_search, "z");
    }

    #[test]
    fn main_work_keeps_global_shortcut_recording_but_real_dialog_owns_the_key() {
        let (mut app, _) = main_fixture();
        let field = SettingsField::ShortcutCommandPalette;
        let mut renderer = super::super::tests::headless_renderer();
        let before = app
            .settings_draft
            .keyboard_shortcuts
            .command_palette
            .clone();
        drop(app.update(Message::Palette(palette_ui::Action::Record(field))));
        let tree = native(&app, Cache::new(), &mut renderer, 1_400.0);
        assert!(
            native_labels(&tree)
                .iter()
                .any(|label| label == "Press a shortcut · Escape cancels")
        );
        let _ = tree.into_cache();
        let mut event = match key(
            keyboard::Key::Character("j".into()),
            keyboard::key::Code::KeyJ,
            None,
        ) {
            iced::Event::Keyboard(event) => event,
            _ => unreachable!(),
        };
        if let keyboard::Event::KeyPressed { modifiers, .. } = &mut event {
            *modifiers = keyboard::Modifiers::ALT;
        }
        drop(app.update(Message::KeyEvent {
            event: event.clone(),
            captured: false,
        }));
        assert!(!app.palette.is_open());
        assert_ne!(
            app.settings_draft.keyboard_shortcuts.command_palette,
            before
        );
        assert!(
            app.settings_draft
                .keyboard_shortcuts
                .command_palette
                .contains("Alt+J")
        );
        assert!(app.settings_pending.contains_key(&field));

        drop(app.update(Message::Palette(palette_ui::Action::Record(field))));
        app.settings_reset_confirm = true;
        assert!(app.modal_is_open());
        assert!(app.dialog().is_some());
        let before = app
            .settings_draft
            .keyboard_shortcuts
            .command_palette
            .clone();
        let pending = app.settings_pending[&field].clone();
        drop(app.update(Message::KeyEvent {
            event,
            captured: false,
        }));
        assert!(
            app.palette.is_open(),
            "the blocked key cannot consume the recording"
        );
        assert_eq!(
            app.settings_draft.keyboard_shortcuts.command_palette,
            before
        );
        assert_eq!(app.settings_pending[&field], pending);
        drop(app.update(Message::CancelResetSettings));
        assert!(!app.modal_is_open());
    }

    #[test]
    fn visible_account_pop_out_leaves_work_only_after_the_dirty_draft_guard_accepts() {
        let mut renderer = super::super::tests::headless_renderer();
        for dirty in [false, true] {
            let (mut app, _) = main_fixture();
            if dirty {
                dirty_native_form(&mut app, &mut renderer);
            }
            let id = app
                .account_ui
                .add_managed_fixture(crate::agent::Provider::Claude, 989);
            drop(app.update(Message::AccountUi(account_ui::Action::Popover(Some(id)))));
            let mut tree = native(&app, Cache::new(), &mut renderer, 1_400.0);
            // Build the real overlay layout before asking for its AX controls.
            let initial = events(
                &mut tree,
                &mut renderer,
                &[],
                iced::mouse::Cursor::Unavailable,
            );
            assert!(
                initial.is_empty(),
                "popup frame emitted messages: {initial:?}"
            );
            let rect = bounds(&tree, "Open in the Accounts pane");
            let candidates = {
                let nodes = tree.a11y_nodes(iced::mouse::Cursor::Unavailable);
                nodes
                    .root()
                    .iter()
                    .chain(nodes.children())
                    .filter(|node| node.node().label() == Some("Open in the Accounts pane"))
                    .map(|node| (node.id().clone(), node.node().bounds()))
                    .collect::<Vec<_>>()
            };
            let point = rect.center();
            let mut messages = click(&mut tree, &mut renderer, point);
            assert!(
                matches!(
                    messages.as_slice(),
                    [Message::AccountUi(account_ui::Action::OpenPane)]
                ),
                "actual popout dirty={dirty} rect={rect:?} point={point:?} viewport=1400x600 candidates({})={candidates:?}; all callbacks: {messages:?}",
                candidates.len()
            );
            let open = messages.remove(0);
            let _ = tree.into_cache();
            drop(app.update(open.clone()));
            if dirty {
                assert!(app.channel_work_is_main());
                assert!(app.utility_card(&CardKind::Accounts).is_none());
                let mut tree = native(&app, Cache::new(), &mut renderer, 1_400.0);
                let point = bounds(&tree, "Keep editing").center();
                let mut messages = click(&mut tree, &mut renderer, point);
                assert!(
                    matches!(
                        messages.as_slice(),
                        [Message::ChannelWork(ui::Action::Keep(..))]
                    ),
                    "actual Keep callbacks after popout dirty={dirty} at {point:?}: {messages:?}"
                );
                let _ = tree.into_cache();
                drop(app.update(messages.remove(0)));
                assert!(app.channel_work_is_main());
                assert!(app.utility_card(&CardKind::Accounts).is_none());
                drop(app.update(open));
                discard_native(&mut app, &mut renderer);
            }
            assert!(!app.channel_work_is_main());
            let card = app
                .utility_card(&CardKind::Accounts)
                .expect("visible Accounts destination");
            assert_eq!(app.workspace.focused(), Some(card));
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Accounts)));
        }
    }

    #[test]
    fn main_work_rejects_real_account_pane_inline_callbacks_even_under_a_global_modal() {
        use account_ui::Action as AccountAction;
        let mut renderer = super::super::tests::headless_renderer();
        for modal in [false, true] {
            let link_heading = format!(
                "Use an existing {} home",
                crate::agent::Provider::Claude.label(),
            );
            let (mut app, home) = main_fixture();
            drop(app.update(Message::AccountUi(AccountAction::OpenPane)));
            assert!(!app.channel_work_is_main());
            let id = app
                .account_ui
                .add_managed_fixture(crate::agent::Provider::Claude, 992);
            let mut tree = accounts_native(&app, &mut renderer);
            let point = bounds(&tree, "Add account").center();
            let mut messages = click(&mut tree, &mut renderer, point);
            assert!(matches!(
                messages.as_slice(),
                [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(), Message::AccountUi(AccountAction::AddMenu(true)))
            ));
            let _ = tree.into_cache();
            drop(app.update(messages.remove(0)));
            let mut tree = accounts_native(&app, &mut renderer);
            let point =
                workboard_popup_button(&mut tree, &renderer, "Existing Claude home").center();
            let mut messages = click(&mut tree, &mut renderer, point);
            assert!(matches!(
                messages.as_slice(),
                [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(), Message::AccountUi(AccountAction::LinkStart(crate::agent::Provider::Claude)))
            ));
            let _ = tree.into_cache();
            drop(app.update(messages.remove(0)));
            let queued_input = account_pane_input_callback(&app, &mut renderer);
            assert!(
                matches!(&queued_input, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::AccountUi(AccountAction::LinkInput(value)) if value == "z"))
            );
            let mut tree = accounts_native(&app, &mut renderer);
            let point = bounds(&tree, "Cancel").center();
            let mut messages = click(&mut tree, &mut renderer, point);
            assert!(matches!(
                messages.as_slice(),
                [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(), Message::AccountUi(AccountAction::LinkCancel))
            ));
            let queued_cancel = messages.remove(0);
            let _ = tree.into_cache();
            // Start a real adapter probe but never poll its returned task.
            drop(app.update(Message::AccountUi(AccountAction::Refresh(id.clone()))));
            let probe_owner = app.account_ui.probe_owner(&id).unwrap();
            drop(app.open_channel_work(home.channel));
            if modal {
                drop(app.update(Message::DaemonHealth(
                    crate::daemon_health::State::Unreachable {
                        since: std::time::SystemTime::UNIX_EPOCH,
                    },
                )));
                drop(app.update(Message::DaemonHealthUi(daemon_health_ui::Action::Open)));
                assert!(app.modal_is_open());
                assert!(app.dialog().is_some());
            }
            let before = (
                app.workspace.focused(),
                app.channel_work.view.owner(),
                app.default_account(crate::agent::Provider::Claude).id(),
            );
            drop(app.update(queued_input));
            drop(app.update(queued_cancel));
            let mut tree = accounts_native(&app, &mut renderer);
            assert_eq!(
                native_field_value(&tree, "Absolute path to the provider home"),
                ""
            );
            assert!(
                rendered_text(&mut tree, &renderer)
                    .iter()
                    .any(|text| text == &link_heading)
            );
            let _ = tree.into_cache();
            assert_eq!(
                (
                    app.workspace.focused(),
                    app.channel_work.view.owner(),
                    app.default_account(crate::agent::Provider::Claude).id()
                ),
                before
            );
            // Accepted completion remains AccountUi, not pane-origin input.
            drop(app.update(Message::AccountUi(AccountAction::Probed {
                owner: probe_owner,
                id: id.clone(),
                outcome: crate::account::probe::Outcome::Answered {
                    identity: None,
                    catalog: None,
                    snapshot: crate::account::UsageSnapshot::state(
                        crate::account::UsageStatus::Unavailable,
                        "fixture completion",
                        1_000,
                    ),
                },
            })));
            let snapshot = app.account_ui.snapshot_for_test(&id).unwrap();
            assert_eq!(snapshot.observed_at, 1_000);
            assert_eq!(snapshot.detail.as_deref(), Some("fixture completion"));
            if modal {
                drop(app.update(Message::DaemonHealthUi(daemon_health_ui::Action::Cancel)));
                assert!(!app.modal_is_open());
            }
            let owner = app.channel_work.view.owner().unwrap();
            drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
            assert!(!app.channel_work_is_main());
            let fresh = account_pane_input_callback(&app, &mut renderer);
            drop(app.update(fresh));
            let mut tree = accounts_native(&app, &mut renderer);
            assert_eq!(
                native_field_value(&tree, "Absolute path to the provider home"),
                "z"
            );
            let point = bounds(&tree, "Cancel").center();
            let mut messages = click(&mut tree, &mut renderer, point);
            assert!(matches!(
                messages.as_slice(),
                [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(), Message::AccountUi(AccountAction::LinkCancel))
            ));
            let _ = tree.into_cache();
            drop(app.update(messages.remove(0)));
            let mut tree = accounts_native(&app, &mut renderer);
            assert!(
                !rendered_text(&mut tree, &renderer)
                    .iter()
                    .any(|text| text == &link_heading)
            );
        }
    }

    #[test]
    fn main_work_rejects_real_settings_account_and_record_callbacks_but_visible_controls_survive() {
        use account_ui::Action as AccountAction;
        let mut renderer = super::super::tests::headless_renderer();
        let (mut app, home) = main_fixture();
        let provider = crate::agent::Provider::Claude;
        let id = app.account_ui.add_managed_fixture(provider, 993);
        drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
        drop(app.update(Message::SelectSettingsCategory(SettingsCategory::Accounts)));
        let before = app.default_account(provider).id();
        assert_ne!(before, id);
        let queued_choice = settings_account_choice_callback(&app, &mut renderer, &id);
        assert!(
            matches!(&queued_choice, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::AccountUi(AccountAction::SetDefault(actual)) if *actual == id))
        );
        let mut tree = utility_native(&app, &mut renderer, true);
        let point = bounds(&tree, "Open Accounts").center();
        let mut messages = click(&mut tree, &mut renderer, point);
        assert!(matches!(
            messages.as_slice(),
            [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(), Message::AccountUi(AccountAction::OpenPane))
        ));
        let queued_open = messages.remove(0);
        let _ = tree.into_cache();
        drop(app.open_channel_work(home.channel));
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(queued_choice));
        drop(app.update(queued_open));
        assert_eq!(app.default_account(provider).id(), before);
        assert!(app.channel_work_is_main());
        assert_eq!(app.channel_work.view.owner(), Some(owner));
        assert!(app.utility_card(&CardKind::Accounts).is_none());
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        let fresh = settings_account_choice_callback(&app, &mut renderer, &id);
        drop(app.update(fresh));
        // The standard App fixture has no account Store: this changes only
        // its synthetic in-memory registry, never a real default/config file.
        assert_eq!(app.default_account(provider).id(), id);

        // The same pane root also owns shared global-message constructors.
        // Record is a Palette action, not a Settings or Account action.
        drop(app.update(Message::SelectSettingsCategory(SettingsCategory::Keyboard)));
        drop(app.update(Message::SettingsSearchChanged("Command palette".into())));
        let mut tree = utility_native(&app, &mut renderer, true);
        let point = bounds(&tree, "Record shortcut for Open command palette").center();
        let mut messages = click(&mut tree, &mut renderer, point);
        assert!(
            matches!(messages.as_slice(), [Message::UtilityPaneUi(inner)]
            if matches!(inner.as_ref(), Message::Palette(palette_ui::Action::Record(SettingsField::ShortcutCommandPalette))))
        );
        let queued_record = messages.remove(0);
        let _ = tree.into_cache();
        drop(app.open_channel_work(home.channel));
        drop(app.update(queued_record));
        assert!(!app.palette.is_open());
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update(Message::ChannelWork(ui::Action::Close(owner))));
        let mut tree = utility_native(&app, &mut renderer, true);
        let point = bounds(&tree, "Record shortcut for Open command palette").center();
        let mut messages = click(&mut tree, &mut renderer, point);
        assert!(
            matches!(messages.as_slice(), [Message::UtilityPaneUi(inner)]
            if matches!(inner.as_ref(), Message::Palette(palette_ui::Action::Record(SettingsField::ShortcutCommandPalette))))
        );
        let _ = tree.into_cache();
        drop(app.update(messages.remove(0)));
        assert!(app.palette.is_open());
        drop(app.update(Message::Palette(palette_ui::Action::Close)));
        let before = app.settings_search.clone();
        drop(
            app.update(Message::UtilityPaneUi(Box::new(Message::UtilityPaneUi(
                Box::new(Message::SettingsSearchChanged(
                    "nested envelope must not dispatch".into(),
                )),
            )))),
        );
        assert_eq!(app.settings_search, before);
    }

    fn accounts_native<'a>(app: &'a Cagents, renderer: &mut cosmic::Renderer) -> Native<'a> {
        let mut tree = iced::runtime::UserInterface::build(
            app.accounts_pane(),
            iced::Size::new(1_400.0, 1_600.0),
            Cache::new(),
            renderer,
        );
        tree.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
        tree
    }

    fn account_pane_input_callback(app: &Cagents, renderer: &mut cosmic::Renderer) -> Message {
        let mut tree = accounts_native(app, renderer);
        let point = bounds(&tree, "Absolute path to the provider home").center();
        let focus_messages = click(&mut tree, renderer, point);
        assert!(
            focus_messages.is_empty(),
            "native focus click at {point:?} emitted: {focus_messages:?}"
        );
        let mut messages = events(
            &mut tree,
            renderer,
            &[key(
                keyboard::Key::Character("z".into()),
                keyboard::key::Code::KeyZ,
                Some("z"),
            )],
            iced::mouse::Cursor::Unavailable,
        );
        assert!(
            matches!(
                messages.as_slice(),
                [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(), Message::AccountUi(account_ui::Action::LinkInput(_)))
            ),
            "actual account input callbacks: {messages:?}"
        );
        messages.remove(0)
    }

    fn settings_account_choice_callback(
        app: &Cagents,
        renderer: &mut cosmic::Renderer,
        id: &crate::account::AccountId,
    ) -> Message {
        let choices = app.default_account_choices();
        let (_, choices) = choices
            .iter()
            .find(|(provider, _)| *provider == crate::agent::Provider::Claude)
            .unwrap();
        let index = choices.iter().position(|choice| choice.id == *id).unwrap() + 1;
        let mut tree = utility_native(app, renderer, true);
        let label = bounds(&tree, "Claude");
        // The pinned COSMIC Dropdown exposes neither AX nor Operation bounds.
        // Derive its adjacent native row from the actual heading and the
        // shared field geometry, then require the exact real emitted callback.
        let inset = (design::DENSE_ROW_HEIGHT - 18.0) / 2.0;
        let row_height = 1.2 * design::TEXT_BODY + 2.0 * inset;
        let top = label.y + label.height + space::XS;
        let point = iced::Point::new(label.x + space::S, top + row_height / 2.0);
        let focus_messages = click(&mut tree, renderer, point);
        assert!(
            focus_messages.is_empty(),
            "native focus click at {point:?} emitted: {focus_messages:?}"
        );
        // Dropdown's overlay List has the same row height and a native
        // container padding. Its index0 is Automatic; account indices follow.
        let point = iced::Point::new(
            point.x + space::S,
            top + row_height + inset + (index as f32 + 0.5) * row_height,
        );
        let mut messages = click(&mut tree, renderer, point);
        assert!(
            matches!(messages.as_slice(), [Message::UtilityPaneUi(inner)] if matches!(inner.as_ref(), Message::AccountUi(account_ui::Action::SetDefault(actual)) if actual == id)),
            "actual account choice {id:?} callbacks at {point:?}: {messages:?}"
        );
        messages.remove(0)
    }

    fn utility_native<'a>(
        app: &'a Cagents,
        renderer: &mut cosmic::Renderer,
        settings: bool,
    ) -> Native<'a> {
        let content = if settings {
            settings_ui::settings_pane(
                app.settings_category,
                &app.settings_draft,
                &app.settings_search,
                settings_ui::SettingsContext {
                    feedback: app.settings_feedback.as_ref(),
                    invalid: &app.settings_invalid,
                    accounts: app.default_account_choices(),
                    updates: app.update_summary(),
                    testing_notification: app.notification_ui.testing,
                },
            )
        } else {
            app.automation_ui.view().map(Message::AutomationUi)
        };
        let mut tree = iced::runtime::UserInterface::build(
            content,
            iced::Size::new(1_400.0, 1_600.0),
            Cache::new(),
            renderer,
        );
        tree.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
        tree
    }

    fn rendered_text(tree: &mut Native<'_>, renderer: &cosmic::Renderer) -> Vec<String> {
        use iced::advanced::widget::Operation;
        #[derive(Default)]
        struct Text(Vec<String>);
        impl Operation for Text {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
                self.0.push(text.to_owned());
            }
        }
        let mut text = Text::default();
        tree.operate(renderer, &mut text);
        text.0
    }

    fn native_field_value(tree: &Native<'_>, label: &str) -> String {
        let nodes = tree.a11y_nodes(iced::mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(label) && node.node().value().is_some())
            .unwrap_or_else(|| {
                panic!(
                    "missing mounted native field {label}; available AX names: {:?}",
                    native_labels(tree)
                )
            })
            .node()
            .value()
            .unwrap()
            .to_owned()
    }

    fn automation_prompt_selection(tree: &Native<'_>) -> (usize, usize) {
        let nodes = tree.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let selection = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some("Automation instructions"))
            .unwrap_or_else(|| {
                panic!(
                    "missing mounted instructions editor; AX names: {:?}",
                    native_labels(tree)
                )
            })
            .node()
            .text_selection()
            .unwrap_or_else(|| {
                panic!(
                    "instructions editor has no AX selection; AX names: {:?}",
                    native_labels(tree)
                )
            });
        assert_eq!(
            selection.anchor.node, selection.focus.node,
            "single-line fixture prompt"
        );
        (
            selection.anchor.character_index,
            selection.focus.character_index,
        )
    }

    fn utility_input_callbacks(
        app: &Cagents,
        renderer: &mut cosmic::Renderer,
        settings: bool,
        label: &str,
        value: &str,
    ) -> Vec<Message> {
        let mut tree = utility_native(app, renderer, settings);
        let expected_value = format!("{}{value}", native_field_value(&tree, label));
        let nodes = tree.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let id = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(label) && node.node().value().is_some())
            .unwrap_or_else(|| {
                panic!(
                    "missing mounted native input {label}; available AX names: {:?}",
                    native_labels(&tree)
                )
            })
            .id()
            .clone();
        let iced_accessibility::A11yId::Widget(id) = id else {
            panic!("mounted input must own a native widget ID: {label}");
        };
        tree.operate(
            renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        let messages = events(
            &mut tree,
            renderer,
            &[key(
                keyboard::Key::Character(value.into()),
                keyboard::key::Code::KeyZ,
                Some(value),
            )],
            iced::mouse::Cursor::Unavailable,
        );
        let expected = if settings {
            match label {
                "Font family" => matches!(messages.as_slice(), [Message::UtilityPaneUi(inner)]
                    if matches!(inner.as_ref(), Message::SettingsDraftChanged {
                        field: SettingsField::TerminalFontFamily, value
                    } if value == &expected_value)),
                "Search settings" => matches!(messages.as_slice(), [Message::UtilityPaneUi(inner)]
                    if matches!(inner.as_ref(), Message::SettingsSearchChanged(value)
                        if value == &expected_value)),
                _ => panic!("unsupported mounted Settings field {label}"),
            }
        } else {
            assert_eq!(label, "Name");
            // Focusing Name also unfocuses the instructions editor. Its next
            // update publishes the pending ClearSelection; keep that callback.
            matches!(messages.as_slice(), [
                Message::AutomationUi(crate::automation_ui::Action::Field(
                    crate::automation_ui::Field::Name, value)),
                Message::AutomationUi(crate::automation_ui::Action::Prompt(
                    iced::widget::text_editor::Action::ClearSelection)),
            ] if value == &expected_value)
        };
        assert!(
            expected,
            "exact actual callbacks for {label}, typed {value:?}; all emitted messages: {messages:?}"
        );
        messages
    }

    fn workboard_snapshot(branch: &str) -> Arc<crate::work::Board> {
        use crate::work::{Board, Column, Comparison, Dirty, Forge, Item, ItemKey, Project};
        let project = PathBuf::from("/fixture/project/.git");
        Arc::new(Board {
            generated_at_unix_ms: 1_000,
            stale_after_days: 7,
            projects: vec![Project {
                repository: project.clone(),
                name: "Fixture".into(),
                root: None,
                base: None,
                scanned_at_unix_ms: None,
                scan_error: None,
                forge: Forge::NotGitHub,
            }],
            items: vec![Item {
                project,
                project_name: "Fixture".into(),
                key: ItemKey::Branch(branch.into()),
                branch: Some(branch.into()),
                worktree: None,
                head: None,
                comparison: Some(Comparison {
                    base: "main".into(),
                    ahead: 1,
                    behind: 0,
                }),
                dirty: Some(Dirty::default()),
                pull_request: None,
                linear: None,
                sessions: Vec::new(),
                last_activity: None,
                error: None,
                column: Column::InProgress,
                derived: Column::InProgress,
                pinned: false,
            }],
            linear_unavailable: None,
        })
    }

    fn visible_workboard_fixture() -> (Cagents, Home) {
        let (mut app, home) = main_fixture();
        drop(app.update(Message::Sidebar(SidebarAction::OpenWork)));
        assert!(!app.channel_work_is_main());
        assert!(
            app.cards
                .values()
                .any(|card| matches!(card, CardRuntime::Work))
        );
        drop(app.update(Message::WorkUi(crate::work_ui::Action::Watched(
            crate::work_ui::Watch::Board {
                revision: 1,
                board: workboard_snapshot("branch"),
                first: true,
            },
        ))));
        (app, home)
    }

    fn workboard_native<'a>(app: &'a Cagents, renderer: &mut cosmic::Renderer) -> Native<'a> {
        let mut ui = iced::runtime::UserInterface::build(
            app.work_ui.view().map(Message::WorkUi),
            iced::Size::new(1_800.0, 900.0),
            Cache::new(),
            renderer,
        );
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
        ui
    }

    fn native_labels(ui: &Native<'_>) -> Vec<String> {
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect()
    }

    fn workboard_query_value(ui: &Native<'_>) -> String {
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some("Filter cards"))
            .expect("mounted Workboard search")
            .node()
            .value()
            .unwrap_or_default()
            .to_owned()
    }

    fn workboard_query_callback(app: &Cagents, renderer: &mut cosmic::Renderer) -> Message {
        let mut ui = workboard_native(app, renderer);
        let point = bounds(&ui, "Filter cards").center();
        let focus_messages = click(&mut ui, renderer, point);
        assert!(
            focus_messages.is_empty(),
            "native focus click at {point:?} emitted: {focus_messages:?}"
        );
        let mut messages = events(
            &mut ui,
            renderer,
            &[key(
                keyboard::Key::Character("z".into()),
                keyboard::key::Code::KeyZ,
                Some("z"),
            )],
            iced::mouse::Cursor::Unavailable,
        );
        assert!(
            matches!(messages.as_slice(), [Message::WorkUi(crate::work_ui::Action::Query(value))] if value == "z"),
            "actual Workboard query callbacks: {messages:?}"
        );
        messages.remove(0)
    }

    fn workboard_pin_callback(app: &mut Cagents, renderer: &mut cosmic::Renderer) -> Message {
        use crate::work_ui::Action as BoardAction;
        // An earlier refused hidden Pin leaves its menu open. Toggle only by
        // the actual anchor until a fresh popup is open; never inject Menu.
        let mut opened = false;
        for _ in 0..2 {
            let mut ui = workboard_native(app, renderer);
            let point = bounds(&ui, "Card actions").center();
            let mut messages = click(&mut ui, renderer, point);
            assert!(
                matches!(messages.as_slice(), [Message::WorkUi(BoardAction::Menu(_))]),
                "actual Workboard menu callbacks at {point:?}: {messages:?}"
            );
            opened = matches!(
                messages.as_slice(),
                [Message::WorkUi(BoardAction::Menu(Some(_)))]
            );
            let _ = ui.into_cache();
            drop(app.update(messages.remove(0)));
            if opened {
                break;
            }
        }
        assert!(opened);
        let mut ui = workboard_native(app, renderer);
        let point = workboard_popup_button(&mut ui, renderer, "Stale").center();
        let mut messages = click(&mut ui, renderer, point);
        assert!(
            matches!(
                messages.as_slice(),
                [Message::WorkUi(BoardAction::Pin(
                    _,
                    Some(crate::work::Column::Stale)
                ))]
            ),
            "actual Workboard Pin callbacks at {point:?}: {messages:?}"
        );
        messages.remove(0)
    }

    fn workboard_popup_button(
        ui: &mut Native<'_>,
        renderer: &cosmic::Renderer,
        label: &str,
    ) -> iced::Rectangle {
        use iced::advanced::widget::{Operation, operation};
        #[derive(Default)]
        struct Outline {
            text: Vec<(String, iced::Rectangle)>,
            buttons: Vec<iced::Rectangle>,
        }
        impl Operation for Outline {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                self.text.push((text.into(), bounds));
            }
            fn focusable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                self.buttons.push(bounds);
            }
        }
        let mut outline = Outline::default();
        ui.operate(renderer, &mut outline);
        outline
            .buttons
            .iter()
            .filter(|button| {
                outline
                    .text
                    .iter()
                    .any(|(text, bounds)| text == label && button.contains(bounds.center()))
            })
            .copied()
            .min_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)))
            .unwrap_or_else(|| panic!(
                "missing real text button {label}; rendered text/bounds: {:?}; focusable bounds: {:?}; AX names: {:?}",
                outline.text, outline.buttons, native_labels(ui)
            ))
    }

    fn discard_native(app: &mut Cagents, renderer: &mut cosmic::Renderer) {
        let mut ui = native(app, Cache::new(), renderer, 700.0);
        let point = bounds(&ui, "Discard draft").center();
        let messages = click(&mut ui, renderer, point);
        assert!(
            matches!(
                messages.as_slice(),
                [Message::ChannelWork(ui::Action::Discard(..))]
            ),
            "actual Discard callbacks at {point:?}: {messages:?}"
        );
        let _ = ui.into_cache();
        for message in messages {
            drop(app.update(message));
        }
    }

    fn native<'a>(
        app: &'a Cagents,
        cache: Cache,
        renderer: &mut cosmic::Renderer,
        width: f32,
    ) -> Native<'a> {
        let ids = super::super::tests::widget_id_counts(&mut app.view());
        assert!(
            ids.values().all(|count| *count == 1),
            "main Work must not mount duplicate named native state"
        );
        assert!(
            !ids.contains_key(&context_inspector_toggle_id().to_string()),
            "main Work does not expose a toggle for the hidden Conversations inspector"
        );
        let mut ui = iced::runtime::UserInterface::build(
            app.view(),
            iced::Size::new(width, 600.0),
            cache,
            renderer,
        );
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
        ui
    }
    fn bounds(ui: &Native<'_>, name: &str) -> iced::Rectangle {
        let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let node = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(name))
            .unwrap_or_else(|| {
                panic!(
                    "missing actual native control {name}; available AX: {:?}",
                    nodes
                        .root()
                        .iter()
                        .chain(nodes.children())
                        .map(|node| (node.id(), node.node().label(), node.node().bounds()))
                        .collect::<Vec<_>>()
                )
            });
        let b = node.node().bounds().unwrap_or_else(|| {
            panic!("actual native control {name} has no bounds; node: {node:?}")
        });
        iced::Rectangle {
            x: b.x0 as f32,
            y: b.y0 as f32,
            width: (b.x1 - b.x0) as f32,
            height: (b.y1 - b.y0) as f32,
        }
    }
    fn events(
        ui: &mut Native<'_>,
        renderer: &mut cosmic::Renderer,
        events: &[iced::Event],
        cursor: iced::mouse::Cursor,
    ) -> Vec<Message> {
        let mut messages = Vec::new();
        let _ = ui.update(
            events,
            cursor,
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut messages,
        );
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            cursor,
        );
        messages
    }
    fn click(
        ui: &mut Native<'_>,
        renderer: &mut cosmic::Renderer,
        point: iced::Point,
    ) -> Vec<Message> {
        events(
            ui,
            renderer,
            &[
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: point }),
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
            ],
            iced::mouse::Cursor::Available(point),
        )
    }
    fn key(key: keyboard::Key, code: keyboard::key::Code, text: Option<&str>) -> iced::Event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: text.map(Into::into),
            repeat: false,
        })
    }
    fn dirty_native_form(app: &mut Cagents, renderer: &mut cosmic::Renderer) {
        let owner = app.channel_work.view.owner().unwrap();
        let Effect::Focus { id, .. } = app
            .channel_work
            .view
            .update(ui::Action::Attach(owner))
            .unwrap()
        else {
            panic!()
        };
        let mut ui = native(app, Cache::new(), renderer, 700.0);
        ui.operate(
            renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        let input = key(
            keyboard::Key::Character("x".into()),
            keyboard::key::Code::KeyX,
            Some("x"),
        );
        let messages = events(
            &mut ui,
            renderer,
            &[input],
            iced::mouse::Cursor::Unavailable,
        );
        let _ = ui.into_cache();
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Message::ChannelWork(ui::Action::Field(..)))),
            "actual dirty Work form callbacks: {messages:?}"
        );
        for message in messages {
            drop(app.update(message));
        }
    }

    #[test]
    fn main_channel_header_retained_press_and_queued_navigation_cannot_survive_a_b_a() {
        for width in [300.0, 900.0] {
            let (mut app, home) = main_fixture();
            app.window_width = width;
            let mut renderer = super::super::tests::headless_renderer();
            let mut ui = native(&app, Cache::new(), &mut renderer, width);
            let area = bounds(&ui, "Conversations for channel Original");
            assert!(area.width > 0.0 && area.x >= 0.0 && area.x + area.width <= width + 0.1);
            let point = area.center();
            let old_action = click(&mut ui, &mut renderer, point)
                .into_iter()
                .find(|message| matches!(message, Message::ChannelWorkNavigation(_)))
                .unwrap();
            assert!(
                events(
                    &mut ui,
                    &mut renderer,
                    &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left
                    ))],
                    iced::mouse::Cursor::Available(point)
                )
                .is_empty()
            );
            let cache = ui.into_cache();
            let mut other = model(home).channels.remove(0);
            other.id = ChannelId::new();
            other.ordinal += 1;
            other.title = "Other".into();
            app.workspace_model.channels.push(other.clone());
            drop(app.open_channel_work(other.id));
            drop(app.open_channel_work(home.channel));
            let mut ui = native(&app, cache, &mut renderer, width);
            let current = bounds(&ui, "Conversations for channel Work");
            assert!(
                current.contains(point),
                "same physical control slot must exercise the owner replacement"
            );
            assert!(
                events(
                    &mut ui,
                    &mut renderer,
                    &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                        iced::mouse::Button::Left
                    ))],
                    iced::mouse::Cursor::Available(point)
                )
                .is_empty()
            );
            let fresh = click(&mut ui, &mut renderer, point)
                .into_iter()
                .find(|message| matches!(message, Message::ChannelWorkNavigation(_)))
                .unwrap();
            let _ = ui.into_cache();
            drop(app.update(old_action));
            assert!(app.channel_work_is_main());
            drop(app.update(fresh));
            assert!(!app.channel_work_is_main());
        }
    }

    #[test]
    fn native_escape_keep_retires_deferred_navigation_without_later_replay() {
        let (mut app, home) = main_fixture();
        let mut renderer = super::super::tests::headless_renderer();
        dirty_native_form(&mut app, &mut renderer);
        let focused = app.workspace.focused();
        let focus = app
            .intercept_channel_work_navigation(&Message::Sidebar(SidebarAction::OpenSettings))
            .unwrap();
        assert!(app.channel_work.view.has_confirmation() && app.channel_work.deferred.is_some());
        assert_eq!(app.channel_work.view.home(), Some(home));
        assert_eq!(app.workspace.focused(), focused);
        let mut ui = native(&app, Cache::new(), &mut renderer, 700.0);
        // Apply the real adapter's focus effect to this mounted tree, exactly
        // as the runtime does before Escape reaches the focused wrapper.
        let mut stream = iced::runtime::task::into_stream(focus).unwrap();
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut applied_focus = false;
        while let Some(action) = executor.block_on(stream.next()) {
            if let iced::runtime::Action::Widget(mut operation) = action {
                ui.operate(&renderer, &mut operation);
                applied_focus = true;
            }
        }
        assert!(
            applied_focus,
            "confirmation must issue an actual native focus operation"
        );
        let escape = key(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            keyboard::key::Code::Escape,
            None,
        );
        let messages = events(
            &mut ui,
            &mut renderer,
            &[escape],
            iced::mouse::Cursor::Unavailable,
        );
        let _ = ui.into_cache();
        let action = messages
            .into_iter()
            .find(|message| matches!(message, Message::ChannelWorkEscape))
            .expect("the actual mounted focused_escape wrapper must own Escape");
        drop(app.update(action));
        assert!(!app.channel_work.view.has_confirmation());
        assert!(app.channel_work.deferred.is_none());
        assert!(app.channel_work_is_main());
        assert_eq!(app.workspace.focused(), focused);
        // Later explicit leave is a new decision, not the cancelled Settings request.
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update_channel_work(ui::Action::Close(owner)));
        let mut ui = native(&app, Cache::new(), &mut renderer, 700.0);
        let point = bounds(&ui, "Discard draft").center();
        let action = click(&mut ui, &mut renderer, point)
            .into_iter()
            .find(|message| matches!(message, Message::ChannelWork(ui::Action::Discard(..))))
            .unwrap();
        let _ = ui.into_cache();
        drop(app.update(action));
        assert!(!app.channel_work_is_main());
        assert!(
            !app.durable
                .cards
                .iter()
                .any(|card| matches!(card.kind, CardKind::Settings))
        );

        for rebound in [false, true] {
            // Palette pane selection must ask before its existing handler selects
            // an inactive channel, not merely before the later FocusCard message.
            let (mut app, home) = main_fixture();
            let (other, original, target) = two_channels(&mut app, home);
            dirty_native_form(&mut app, &mut renderer);
            let select = || {
                Message::Palette(palette_ui::Action::Run(palette_ui::MessageBox(Box::new(
                    Message::Palette(palette_ui::Action::FocusPane(target)),
                ))))
            };
            drop(app.update(Message::RunCommand(
                workspace_widget::GlobalShortcut::CommandPalette,
            )));
            drop(app.update(select()));
            assert!(app.channel_work.view.has_confirmation());
            assert_eq!(app.durable.active_channel, Some(home.channel));
            assert_eq!(app.workspace.focused(), Some(original));
            drop(app.escape_channel_work());
            assert!(app.channel_work.deferred.is_none());
            assert_eq!(app.durable.active_channel, Some(home.channel));
            assert_eq!(app.workspace.focused(), Some(original));
            drop(app.update(select()));
            if rebound {
                // The card ID survives, but its canonical source has been replaced
                // while Discard is visible. It is no longer the chosen result.
                let view = app
                    .workspace_model
                    .views
                    .iter_mut()
                    .find(|view| view.scope == Scope::Channel(other))
                    .unwrap();
                if let CardKind::Terminal(session) = &mut view.panes[0].kind {
                    session.id = TerminalId::new();
                }
            }
            discard_native(&mut app, &mut renderer);
            assert!(!app.channel_work_is_main());
            assert_eq!(
                app.durable.active_channel,
                Some(if rebound { home.channel } else { other })
            );
            assert_eq!(
                app.workspace.focused(),
                Some(if rebound { original } else { target })
            );
        }
    }

    #[test]
    fn main_work_blocks_hidden_pane_commands_but_preserves_global_modal_and_palette_priority() {
        let (mut app, home) = main_fixture();
        // Two real, open cards make the negative meaningful: the same input
        // must move focus when Conversations is visible.
        let first = super::super::tests::terminal_card_state();
        let second = super::super::tests::terminal_card_state();
        let (first_id, second_id) = (first.id, second.id);
        app.durable = Arc::new(WorkspaceState {
            cards: vec![first, second],
            focused: Some(first_id),
            active_channel: Some(home.channel),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards.insert(first_id, CardRuntime::Loading);
        app.cards.insert(second_id, CardRuntime::Loading);
        app.channel_work.main = false;
        app.context_inspector_open = false;
        assert!(!app.workspace_inputs_blocked());
        drop(app.update(Message::FocusAdjacent(
            crate::workspace::FocusDirection::Right,
        )));
        assert_eq!(app.workspace.focused(), Some(second_id));
        app.workspace.focus(first_id).unwrap();
        app.channel_work.main = true;
        assert!(!app.workspace_surfaces_visible() && app.workspace_inputs_blocked());
        drop(app.update(Message::FocusAdjacent(
            crate::workspace::FocusDirection::Right,
        )));
        assert_eq!(app.workspace.focused(), Some(first_id));
        drop(app.update(Message::RunCommand(
            workspace_widget::GlobalShortcut::FindInPane,
        )));
        assert!(app.channel_work_is_main());
        drop(app.update(Message::RunCommand(
            workspace_widget::GlobalShortcut::CommandPalette,
        )));
        assert!(app.palette.is_open(), "safe shell command remains usable");
        drop(app.update_palette(palette_ui::Action::Close));
        app.daemon_health_ui.confirmation = true;
        let iced::Event::Keyboard(event) = key(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            keyboard::key::Code::Escape,
            None,
        ) else {
            panic!()
        };
        drop(app.update(Message::KeyEvent {
            event,
            captured: false,
        }));
        assert!(
            !app.daemon_health_ui.confirmation,
            "global modal Escape takes priority"
        );
        assert!(app.channel_work_is_main());
        assert_eq!(app.channel_work.view.home(), Some(home));
    }

    #[test]
    fn update_freeze_precedes_work_navigation_and_changed_deferred_registry_cannot_retarget() {
        let (mut app, home) = main_fixture();
        let mut other = model(home).channels.remove(0);
        other.id = ChannelId::new();
        other.ordinal += 1;
        app.workspace_model.channels.push(other.clone());
        app.update_prepared = true;
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: other.id,
        })));
        assert_eq!(app.channel_work.view.home(), Some(home));
        assert!(app.channel_work.deferred.is_none());
        app.update_prepared = false;
        let mut renderer = super::super::tests::headless_renderer();
        dirty_native_form(&mut app, &mut renderer);
        app.intercept_channel_work_navigation(&Message::Sidebar(SidebarAction::OpenSettings))
            .unwrap();
        app.workspace_model.identity = uuid::Uuid::new_v4();
        let mut ui = native(&app, Cache::new(), &mut renderer, 700.0);
        let point = bounds(&ui, "Discard draft").center();
        let action = click(&mut ui, &mut renderer, point)
            .into_iter()
            .find(|message| matches!(message, Message::ChannelWork(ui::Action::Discard(..))))
            .unwrap();
        let _ = ui.into_cache();
        drop(app.update(action));
        assert!(!app.channel_work_is_main());
        assert!(app.channel_work.deferred.is_none());
        assert!(
            !app.durable
                .cards
                .iter()
                .any(|card| matches!(card.kind, CardKind::Settings))
        );

        for replacement in ["unchanged", "id", "path"] {
            let (mut app, home) = main_fixture();
            let (_, original, _) = two_channels(&mut app, home);
            let project = app.sidebar_model.projects[0].id;
            let worktree = WorktreeId::new();
            let path = PathBuf::from("/project/linked");
            app.sidebar_model.projects[0]
                .worktrees
                .push(sidebar::SidebarWorktree::from_durable(
                    worktree,
                    path.clone(),
                    Some("feature".into()),
                ));
            dirty_native_form(&mut app, &mut renderer);
            drop(app.update(Message::Sidebar(SidebarAction::OpenWorktree {
                project_id: project,
                worktree_id: worktree,
                path: path.clone(),
            })));
            assert!(app.channel_work.view.has_confirmation());
            assert_eq!(app.workspace.focused(), Some(original));
            let row = app.sidebar_model.projects[0].worktrees.last_mut().unwrap();
            match replacement {
                "id" => row.id = WorktreeId::new(),
                "path" => row.path = "/project/replacement".into(),
                _ => {}
            }
            discard_native(&mut app, &mut renderer);
            assert!(!app.channel_work_is_main());
            if replacement == "unchanged" {
                assert_eq!(app.active_workspace, Some(path));
                assert_eq!(app.durable.active_channel, None);
            } else {
                assert_eq!(app.active_workspace, Some(PathBuf::from("/project")));
                assert_eq!(app.durable.active_channel, Some(home.channel));
                assert_eq!(app.workspace.focused(), Some(original));
            }
        }
    }
    #[test]
    fn queued_editor_paste_is_refused_in_work_but_owning_load_completion_and_visible_paste_survive()
    {
        use crate::editor::{FileContent, LoadedFile};
        let (mut app, _) = main_fixture();
        let mut state = super::super::tests::terminal_card_state();
        let card = state.id;
        let (runtime, identity) =
            repository_widget::RepositoryRuntime::selection_fixture(card, "notes.txt");
        state.kind = CardKind::Repository(runtime.descriptor.clone());
        app.durable = Arc::new(WorkspaceState {
            cards: vec![state],
            focused: Some(card),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        let completion = RepositoryAction::FileLoaded {
            identity: identity.clone(),
            result: Ok(LoadedFile {
                source: identity.path,
                content: FileContent::Editable {
                    text: "loaded".into(),
                    content_hash: blake3::hash(b"loaded"),
                    markdown: false,
                },
            }),
        };
        assert!(
            app.intercept_channel_work_navigation(&Message::Repository(completion.clone()))
                .is_none()
        );
        drop(app.update(Message::Repository(completion)));
        let text = |app: &Cagents| match &app.cards[&card] {
            CardRuntime::Repository(runtime) => runtime.editor_text().unwrap(),
            _ => panic!(),
        };
        assert_eq!(
            text(&app),
            "loaded",
            "owning I/O completion survives hidden Conversations"
        );
        let paste = || {
            Message::Repository(RepositoryAction::EditorAction {
                card,
                action: repository_widget::EditorAction::Edit(
                    repository_widget::EditorEdit::Paste(Arc::new("clipboard".into())),
                ),
            })
        };
        drop(app.update(paste()));
        assert_eq!(
            text(&app),
            "loaded",
            "queued clipboard delivery cannot edit hidden source"
        );
        assert!(
            app.intercept_channel_work_navigation(&Message::TerminalFind(
                terminal_ui::Action::Paste(card, "clipboard".into())
            ))
            .is_some()
        );
        assert!(
            app.intercept_channel_work_navigation(&Message::TerminalFind(
                terminal_ui::Action::Refresh(card, 7)
            ))
            .is_none()
        );
        app.channel_work.main = false;
        app.context_inspector_open = false;
        drop(app.update(paste()));
        assert!(
            text(&app).contains("clipboard"),
            "identical paste reaches visible Conversations"
        );
    }
}
