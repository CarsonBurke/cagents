use super::*;
use crate::{
    channel_work::Home,
    channel_work_api as api,
    channel_work_ui::{self as ui, Effect, Failure},
};

#[derive(Default)]
pub(super) struct State {
    pub(super) view: ui::State,
    pub(super) retire_candidates: HashSet<Home>,
    routes: HashMap<Home, Route>,
    requested_home: Option<Home>,
}

#[derive(Clone)]
struct Route {
    socket: PathBuf,
    proof: Arc<Mutex<Option<crate::model::HostSessionProof>>>,
}

impl Cagents {
    /// Context reads stay beside the currently selected channel. They never
    /// replace its workspace, replay navigation or recapture a pane's focus.
    pub(super) fn open_channel_work(&mut self, channel: ChannelId) -> Task<Message> {
        if self.channel_work.view.has_confirmation() || self.durable.active_channel != Some(channel)
        {
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
        self.channel_work.requested_home = Some(home);
        let effect = self.channel_work.view.open(home, channel.title.clone());
        self.context_tab = ContextTab::PullRequest;
        let inspector = self.set_context_inspector_open(true);
        let request = effect.map_or_else(Task::none, |effect| self.channel_work_effect(effect));
        Task::batch([inspector, request])
    }

    pub(super) fn selected_channel_work_home(&self) -> Option<Home> {
        self.workspace_model
            .channel(self.durable.active_channel?)
            .map(|channel| Home {
                registry: self.workspace_model.identity,
                channel: channel.id,
                creation_ordinal: channel.ordinal,
            })
    }

    pub(super) fn sync_channel_pull_request_context(&mut self) -> Task<Message> {
        if !self.context_inspector_open || self.context_tab != ContextTab::PullRequest {
            return Task::none();
        }
        let home = self.selected_channel_work_home();
        if self.channel_work.requested_home == home
            && (self.channel_work.view.home() == home || self.channel_work.view.has_context_draft())
        {
            return Task::none();
        }
        // Remember the attempted scope even when Keep retains a dirty old
        // draft. Its next edit must not immediately reopen the leave dialog.
        self.channel_work.requested_home = home;
        let Some(home) = home else {
            return Task::none();
        };
        if self.channel_work.view.has_confirmation() {
            return Task::none();
        }
        let title = self
            .workspace_model
            .channel(home.channel)
            .unwrap()
            .title
            .clone();
        self.channel_work
            .view
            .open(home, title)
            .map_or_else(Task::none, |effect| self.channel_work_effect(effect))
    }

    pub(super) fn channel_work_subscription(&self) -> Subscription<Message> {
        if self.context_inspector_open
            && self.context_tab == ContextTab::PullRequest
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

    pub(super) fn update_channel_work(&mut self, action: ui::Action) -> Task<Message> {
        let background = action.background();
        if !background
            && (!self.context_inspector_open
                || !self.context_inspector_visible()
                || self.context_tab != ContextTab::PullRequest
                || self.modal_is_open()
                || self.chrome_ui.is_open()
                || self.palette.is_open()
                || self.update_prepared)
        {
            return Task::none();
        }
        if !background
            && self.channel_work.view.home() != self.selected_channel_work_home()
            && !matches!(
                &action,
                ui::Action::Field(..)
                    | ui::Action::Role(..)
                    | ui::Action::Dismiss(..)
                    | ui::Action::Keep(..)
                    | ui::Action::Discard(..)
            )
        {
            return Task::none();
        }
        let discarded = matches!(&action, ui::Action::Discard(..));
        let effect = self.channel_work.view.update(action);
        if discarded && let Some(Effect::Read { home, .. }) = &effect {
            // A dirty draft may have captured B before the workspace moved to
            // C. Discard retires the draft, not the user's newer navigation.
            let current = self
                .durable
                .active_channel
                .and_then(|id| self.workspace_model.channel(id))
                .map(|channel| Home {
                    registry: self.workspace_model.identity,
                    channel: channel.id,
                    creation_ordinal: channel.ordinal,
                });
            if current != Some(*home) {
                if let Some(current) = current {
                    return self.open_channel_work(current.channel);
                }
                if let Some(owner) = self.channel_work.view.owner() {
                    self.channel_work.view.update(ui::Action::Close(owner));
                }
                return Task::none();
            }
        }
        if !background
            && self.channel_work.view.home() != self.selected_channel_work_home()
            && !self.channel_work.view.has_context_draft()
            && matches!(&effect, Some(Effect::Focus { .. } | Effect::Read { .. }))
        {
            if let Some(home) = self.selected_channel_work_home() {
                return self.open_channel_work(home.channel);
            }
            return Task::none();
        }
        effect.map_or_else(Task::none, |effect| {
            self.channel_work_effect_with_focus(effect, background)
        })
    }

    pub(super) fn channel_work_effect(&mut self, effect: Effect) -> Task<Message> {
        self.channel_work_effect_with_focus(effect, false)
    }

    fn channel_work_effect_with_focus(
        &mut self,
        effect: Effect,
        background: bool,
    ) -> Task<Message> {
        let home = match &effect {
            Effect::Read { home, .. }
            | Effect::Refresh { home, .. }
            | Effect::Lookup { home, .. }
            | Effect::OpenPullRequest { home, .. }
            | Effect::Close { home } => *home,
            Effect::Mutate { mutation, .. } => mutation.home,
            Effect::Focus { owner, .. } => owner.home,
        };
        if self.channel_work.view.home() == Some(home)
            && self.workspace_model.identity == home.registry
            && let Some(channel) = self
                .workspace_model
                .channel(home.channel)
                .filter(|channel| channel.ordinal == home.creation_ordinal)
        {
            self.channel_work
                .view
                .set_title(home, channel.title.clone());
        }
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
        match effect {
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
            Effect::OpenPullRequest { url, .. } => {
                let gesture = self.keyboard_modifiers.control().then_some(
                    if self.keyboard_modifiers.shift() {
                        LinkOpenGesture::ControlShiftClick
                    } else {
                        LinkOpenGesture::ControlClick
                    },
                );
                self.open_link_in(url, self.settings.link_destination(gesture))
            }
            Effect::Close { .. } => {
                if self.context_tab == ContextTab::PullRequest && self.context_inspector_open {
                    self.set_context_inspector_open(false)
                } else {
                    Task::none()
                }
            }
            Effect::Focus { owner, id } => {
                if self.channel_work.view.owner() == Some(owner)
                    && self.context_inspector_visible()
                    && self.context_tab == ContextTab::PullRequest
                    && !self.modal_is_open()
                    && !self.chrome_ui.is_open()
                    && !self.palette.is_open()
                    && !self.update_prepared
                {
                    if background {
                        workspace_widget::focus_after_loading(id.clone(), id)
                            .map(cosmic::Action::App)
                    } else {
                        iced::widget::operation::focus(id).map(cosmic::Action::App)
                    }
                } else {
                    Task::none()
                }
            }
        }
    }

    /// Retire only the exact settled empty home; PR history remains daemon-owned.
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
    use crate::model::WorktreeId;
    use crate::workspace_model::{
        ModelChannel, ModelEdit, WorkspaceModel,
        views::{Scope, View},
    };
    use cosmic::iced::futures::StreamExt;
    type Cache = iced::runtime::user_interface::Cache;
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
    fn pr_fixture() -> (Cagents, Home) {
        let mut app = super::super::tests::context_inspector_test_app(true);
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
        app.window_width = 1280.0;
        app.window_height = 900.0;
        app.context_tab = ContextTab::PullRequest;
        app.context_inspector_open = true;
        app.context_inspector_transition = None;
        app.channel_work.requested_home = Some(home);
        app.channel_work.routes.insert(
            home,
            Route {
                socket: "/fixture/never-connected.sock".into(),
                proof: Arc::new(Mutex::new(None)),
            },
        );
        (app, home)
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
    #[test]
    fn related_pr_context_opens_inspector_without_replacing_or_retargeting_workspace() {
        let (mut app, home) = pr_fixture();
        let (other, _, _) = two_channels(&mut app, home);
        let before = (
            app.durable.active_channel,
            app.workspace.order().collect::<Vec<_>>(),
            app.workspace.focused(),
            app.workspace_view_generation,
            app.active_workspace.clone(),
        );
        drop(app.open_channel_work(home.channel));
        assert!(app.context_inspector_visible());
        assert!(app.context_inspector_open);
        assert_eq!(app.context_tab, ContextTab::PullRequest);
        assert_eq!(app.channel_work.view.home().unwrap().channel, home.channel);
        assert_eq!(
            (
                app.durable.active_channel,
                app.workspace.order().collect::<Vec<_>>(),
                app.workspace.focused(),
                app.workspace_view_generation,
                app.active_workspace.clone()
            ),
            before,
            "PR inspection never selects a different conversation or changes its placement"
        );
        let owner = app.channel_work.view.owner();
        drop(app.open_channel_work(other));
        assert_eq!(
            app.channel_work.view.owner(),
            owner,
            "a queued old channel action cannot retarget current PR context"
        );
        let mut element = app.workspace_chrome(widget::Space::new().into());
        let renderer = super::super::tests::headless_renderer();
        let mut tree = iced::advanced::widget::Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &iced::advanced::layout::Limits::new(iced::Size::ZERO, iced::Size::new(800.0, 600.0)),
        );
        let nodes = element.as_widget().a11y_nodes(
            iced::advanced::Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let labels: Vec<_> = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .flat_map(|n| [n.node().label(), n.node().value()])
            .flatten()
            .collect();
        assert!(
            !labels.iter().any(|name| *name == "CHANNEL"
                || name.starts_with("Conversations for channel")
                || name.starts_with("Work for channel")),
            "no duplicate channel banner/navigation: {labels:?}"
        );
    }
    mod full_frame_focus {
        use super::*;
        use iced::advanced::widget::{Operation, operation};

        type Frame<'a> = iced::runtime::UserInterface<
            'a,
            cosmic::Action<Message>,
            cosmic::Theme,
            cosmic::Renderer,
        >;

        #[derive(Debug)]
        struct FocusNode {
            id: Option<widget::Id>,
            bounds: iced::Rectangle,
            focused: bool,
        }

        #[derive(Default, Debug)]
        struct Focus(Vec<FocusNode>);

        impl Operation for Focus {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }

            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                self.0.push(FocusNode {
                    id: id.cloned(),
                    bounds,
                    focused: state.is_focused(),
                });
            }
        }

        fn same(a: &widget::Id, b: &widget::Id) -> bool {
            iced_accessibility::IdEq::eq(a, b)
        }

        fn mount<'a>(app: &'a Cagents, cache: Cache, renderer: &mut cosmic::Renderer) -> Frame<'a> {
            let mut frame = iced::runtime::UserInterface::build(
                cosmic::ApplicationExt::view_main(app),
                iced::Size::new(app.window_width, app.window_height),
                cache,
                renderer,
            );
            draw(&mut frame, renderer);
            frame
        }

        fn draw(frame: &mut Frame<'_>, renderer: &mut cosmic::Renderer) {
            frame.draw(
                renderer,
                &cosmic::theme::active(),
                &iced::advanced::renderer::Style::default(),
                iced::mouse::Cursor::Unavailable,
            );
        }

        fn probe(frame: &mut Frame<'_>, renderer: &cosmic::Renderer) -> Focus {
            let mut focus = Focus::default();
            frame.operate(renderer, &mut focus);
            let focused = focus.0.iter().filter(|node| node.focused).count();
            assert!(
                focused <= 1,
                "full window has {focused} focused widgets: {focus:?}"
            );
            focus
        }

        fn active(frame: &mut Frame<'_>, renderer: &cosmic::Renderer) -> widget::Id {
            let focus = probe(frame, renderer);
            focus
                .0
                .iter()
                .find(|node| node.focused)
                .and_then(|node| node.id.clone())
                .unwrap_or_else(|| panic!("no named native focus after traversal: {focus:?}"))
        }

        fn operate(
            frame: &mut Frame<'_>,
            renderer: &cosmic::Renderer,
            mut operation: Box<dyn Operation>,
        ) {
            // focus_next/previous first count the whole tree, then chain the
            // actual focus change. Applying only the first pass tests nothing.
            for _ in 0..4 {
                frame.operate(renderer, &mut operation);
                match operation.finish() {
                    operation::Outcome::Chain(next) => operation = next,
                    operation::Outcome::None | operation::Outcome::Some(()) => return,
                }
            }
            panic!("unexpected unbounded focus-operation chain");
        }

        fn apply_focus_task(
            frame: &mut Frame<'_>,
            renderer: &cosmic::Renderer,
            task: Task<Message>,
        ) {
            let mut stream = iced::runtime::task::into_stream(task)
                .expect("draft guard must issue its native focus operation");
            let executor = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let mut operations = 0;
            while let Some(action) = executor.block_on(stream.next()) {
                match action {
                    iced::runtime::Action::Widget(operation) => {
                        operate(frame, renderer, operation);
                        operations += 1;
                    }
                    _ => panic!("a local draft focus must not dispatch background work"),
                }
            }
            assert!(operations > 0);
            let _ = active(frame, renderer);
        }

        fn event(
            frame: &mut Frame<'_>,
            renderer: &mut cosmic::Renderer,
            events: &[iced::Event],
            cursor: iced::mouse::Cursor,
        ) -> (Vec<cosmic::Action<Message>>, Vec<iced::event::Status>) {
            let mut messages = Vec::new();
            let (_, statuses) = frame.update(
                events,
                cursor,
                renderer,
                &mut iced::advanced::clipboard::Null,
                &mut messages,
            );
            draw(frame, renderer);
            (messages, statuses)
        }

        fn click(
            frame: &mut Frame<'_>,
            renderer: &mut cosmic::Renderer,
            point: iced::Point,
        ) -> Vec<cosmic::Action<Message>> {
            event(
                frame,
                renderer,
                &[
                    iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: point }),
                    iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    )),
                    iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                        iced::mouse::Button::Left,
                    )),
                ],
                iced::mouse::Cursor::Available(point),
            )
            .0
        }

        fn tab(
            frame: &mut Frame<'_>,
            renderer: &mut cosmic::Renderer,
            reverse: bool,
        ) -> widget::Id {
            let mut tab = key(
                keyboard::Key::Named(keyboard::key::Named::Tab),
                keyboard::key::Code::Tab,
                None,
            );
            if let iced::Event::Keyboard(keyboard::Event::KeyPressed { modifiers, .. }) = &mut tab {
                *modifiers = if reverse {
                    keyboard::Modifiers::SHIFT
                } else {
                    keyboard::Modifiers::empty()
                };
            }
            let (messages, statuses) =
                event(frame, renderer, &[tab], iced::mouse::Cursor::Unavailable);
            assert!(
                messages.is_empty(),
                "Tab unexpectedly emitted application actions: {messages:?}"
            );
            assert_eq!(
                statuses,
                [iced::event::Status::Ignored],
                "captured Tab cannot reach COSMIC keyboard_nav"
            );
            // The actual keyboard_nav subscription emits precisely this
            // standard operation for an ignored Tab/Shift-Tab. A headless UI
            // does not run the platform event subscription itself.
            operate(
                frame,
                renderer,
                if reverse {
                    Box::new(operation::focusable::focus_previous::<()>())
                } else {
                    Box::new(operation::focusable::focus_next::<()>())
                },
            );
            active(frame, renderer)
        }

        fn seek(frame: &mut Frame<'_>, renderer: &mut cosmic::Renderer, target: &widget::Id) {
            let total = probe(frame, renderer).0.len();
            for _ in 0..=total {
                if probe(frame, renderer)
                    .0
                    .iter()
                    .any(|node| node.focused && node.id.as_ref().is_some_and(|id| same(id, target)))
                {
                    return;
                }
                tab(frame, renderer, false);
            }
            panic!(
                "full-window traversal never reached {target:?}: {:?}",
                probe(frame, renderer)
            );
        }

        fn labelled(
            frame: &mut Frame<'_>,
            renderer: &cosmic::Renderer,
            label: &str,
        ) -> (widget::Id, iced::Rectangle) {
            let nodes = frame.a11y_nodes(iced::mouse::Cursor::Unavailable);
            let matches = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some(label)
                })
                .filter_map(|node| node.node().bounds())
                .collect::<Vec<_>>();
            assert_eq!(
                matches.len(),
                1,
                "one actual full-frame AX control {label}: {matches:?}"
            );
            let b = matches[0];
            let bounds = iced::Rectangle {
                x: b.x0 as f32,
                y: b.y0 as f32,
                width: (b.x1 - b.x0) as f32,
                height: (b.y1 - b.y0) as f32,
            };
            let focus = probe(frame, renderer);
            let controls = focus
                .0
                .iter()
                .filter(|node| node.bounds.contains(bounds.center()))
                .filter_map(|node| node.id.as_ref().map(|id| (id, node.bounds)))
                .collect::<Vec<_>>();
            let (id, _) = controls
                .iter()
                .min_by(|a, b| (a.1.width * a.1.height).total_cmp(&(b.1.width * b.1.height)))
                .unwrap_or_else(|| {
                    panic!(
                        "AX control {label} is absent from full-window focus traversal: {focus:?}"
                    )
                });
            ((*id).clone(), bounds)
        }

        fn input(frame: &mut Frame<'_>, renderer: &cosmic::Renderer, suffix: &str) -> widget::Id {
            let focus = probe(frame, renderer);
            let ids = focus
                .0
                .iter()
                .filter_map(|node| node.id.as_ref())
                .filter(|id| {
                    id.to_string().starts_with("channel-work-input-")
                        && id.to_string().ends_with(suffix)
                })
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(ids.len(), 1, "one mounted Work input {suffix}: {focus:?}");
            ids[0].clone()
        }

        fn fixture(renderer: &mut cosmic::Renderer) -> (Cagents, Home, ui::FormOwner, Cache) {
            let (mut app, home) = pr_fixture();
            app.window_width = 1280.0;
            app.window_height = 900.0;
            assert!(
                !app.core.window.show_headerbar,
                "production uses the custom application header"
            );
            app.core.set_main_window_id(Some(window::Id::RESERVED));
            assert!(
                app.core.keyboard_nav(),
                "ordinary window traversal is enabled by default"
            );
            let owner = app.channel_work.view.owner().unwrap();
            let Some(Effect::Focus { id: host, .. }) =
                app.channel_work.view.update(ui::Action::Attach(owner))
            else {
                panic!("attach must create the real form");
            };
            let mut frame = mount(&app, Cache::new(), renderer);
            let focus = probe(&mut frame, renderer);
            let point = focus
                .0
                .iter()
                .find(|node| node.id.as_ref().is_some_and(|id| same(id, &host)))
                .unwrap_or_else(|| panic!("full frame omitted the attached form: {focus:?}"))
                .bounds
                .center();
            assert!(click(&mut frame, renderer, point).is_empty());
            let (messages, _) = event(
                &mut frame,
                renderer,
                &[key(
                    keyboard::Key::Character("x".into()),
                    keyboard::key::Code::KeyX,
                    Some("x"),
                )],
                iced::mouse::Cursor::Unavailable,
            );
            let [
                cosmic::Action::App(Message::ChannelWork(ui::Action::Field(
                    form,
                    ui::Field::Host,
                    _,
                ))),
            ] = messages.as_slice()
            else {
                panic!("actual full-frame host callback: {messages:?}");
            };
            let form = *form;
            let cache = frame.into_cache();
            // Set a valid target using the owner captured by that native field,
            // without dispatching a transport or directly focusing any role.
            for action in [
                ui::Action::Field(form, ui::Field::Host, "github.com".into()),
                ui::Action::Field(form, ui::Field::Repository, "fixture/work".into()),
                ui::Action::Field(form, ui::Field::Number, "17".into()),
            ] {
                assert!(app.channel_work.view.update(action).is_none());
            }
            (app, home, form, cache)
        }

        fn repository_state(frame: &Frame<'_>) -> (String, usize, usize) {
            let nodes = frame.a11y_nodes(iced::mouse::Cursor::Unavailable);
            let fields = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::TextInput
                        && node.node().label() == Some("owner/repository or PR URL")
                })
                .collect::<Vec<_>>();
            assert_eq!(fields.len(), 1, "one actual Repository field");
            let field = fields[0].node();
            let selection = field.text_selection().expect("native input selection");
            assert_eq!(selection.anchor.node, selection.focus.node);
            (
                field.value().unwrap_or_default().to_owned(),
                selection.anchor.character_index,
                selection.focus.character_index,
            )
        }

        fn updating_caption(frame: &mut Frame<'_>, renderer: &cosmic::Renderer) -> bool {
            #[derive(Default)]
            struct Text(Vec<String>);
            impl Operation for Text {
                fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                    visit(self);
                }
                fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, value: &str) {
                    self.0.push(value.to_owned());
                }
            }
            let mut text = Text::default();
            frame.operate(renderer, &mut text);
            text.0
                .iter()
                .any(|value| value == "Updating the current view…")
        }

        fn refresh_feedback(
            frame: &mut Frame<'_>,
            renderer: &cosmic::Renderer,
            loading: bool,
        ) -> iced::Rectangle {
            let name = if loading {
                "Updating related pull requests…"
            } else {
                "Refresh related pull requests"
            };
            let nodes = frame.a11y_nodes(iced::mouse::Cursor::Unavailable);
            let buttons = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some(name)
                })
                .collect::<Vec<_>>();
            assert_eq!(buttons.len(), 1, "one actual progress/refresh control");
            assert_eq!(buttons[0].node().is_disabled(), loading);
            labelled(frame, renderer, name).1
        }

        fn loaded(home: Home, revision: u64) -> crate::channel_work::Snapshot {
            crate::channel_work::Snapshot {
                home,
                lifecycle: crate::channel_work::HomeLifecycle::Active,
                revision,
                entries: Vec::new(),
                history: Vec::new(),
                next_current: None,
                next_history: None,
            }
        }

        fn repository_characters(
            app: &mut Cagents,
            mut cache: Cache,
            renderer: &mut cosmic::Renderer,
            repository: &widget::Id,
            form: &mut Option<ui::FormOwner>,
            initial: &str,
            characters: &str,
        ) -> Cache {
            let mut value = initial.to_owned();
            for character in characters.chars() {
                let mut frame = mount(app, cache, renderer);
                assert!(same(&active(&mut frame, renderer), repository));
                assert_eq!(
                    repository_state(&frame),
                    (value.clone(), value.len(), value.len())
                );
                let text = character.to_string();
                value.push(character);
                let (messages, _) = event(
                    &mut frame,
                    renderer,
                    &[key(
                        keyboard::Key::Character(text.clone().into()),
                        keyboard::key::Code::KeyA,
                        Some(&text),
                    )],
                    iced::mouse::Cursor::Unavailable,
                );
                let [
                    cosmic::Action::App(Message::ChannelWork(ui::Action::Field(
                        actual,
                        ui::Field::Repository,
                        received,
                    ))),
                ] = messages.as_slice()
                else {
                    panic!(
                        "native character {character:?} must emit one whole Repository value: {messages:?}"
                    );
                };
                assert_eq!(received, &value, "no missing or duplicated character");
                if let Some(original) = *form {
                    assert_eq!(*actual, original);
                } else {
                    *form = Some(*actual);
                }
                assert_eq!(
                    repository_state(&frame),
                    (value.clone(), value.len(), value.len())
                );
                cache = frame.into_cache();
                let cosmic::Action::App(message) = messages.into_iter().next().unwrap() else {
                    unreachable!()
                };
                drop(app.update(message));
            }
            cache
        }

        fn command_key(value: &str, code: keyboard::key::Code) -> iced::Event {
            let mut event = key(keyboard::Key::Character(value.into()), code, None);
            if let iced::Event::Keyboard(keyboard::Event::KeyPressed { modifiers, .. }) = &mut event
            {
                *modifiers = keyboard::Modifiers::COMMAND;
            }
            event
        }

        struct RepositoryClipboard(String);
        impl iced::advanced::Clipboard for RepositoryClipboard {
            fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
                Some(self.0.clone())
            }
            fn write(&mut self, _: iced::advanced::clipboard::Kind, value: String) {
                self.0 = value;
            }
        }

        fn closing_fixture(
            renderer: &mut cosmic::Renderer,
        ) -> (Cagents, Home, ui::FormOwner, Cache, CardId, CardId) {
            use crate::model::{BootId, HostIncarnation, HostSessionId};

            let (mut app, home, form, cache) = fixture(renderer);
            let (other, closing, survivor) = two_channels(&mut app, home);
            app.workspace_model
                .channels
                .retain(|channel| channel.id != other);
            app.workspace_model
                .views
                .retain(|view| view.scope != Scope::Channel(other));
            let durable = Arc::make_mut(&mut app.durable);
            durable.channels.retain(|channel| channel.id != other);
            durable.channels[0].cards.push(survivor);
            for card in &mut durable.cards {
                card.width_fraction = 0.5;
                card.residency = Residency::Hot;
            }
            app.workspace_model.views[0].panes = durable
                .cards
                .iter()
                .map(|card| crate::workspace_model::views::Pane::from_card(card, None).unwrap())
                .collect();
            app.workspace = Cagents::rebuild_workspace(&WorkspaceState {
                cards: durable.cards.clone(),
                focused: Some(closing),
                ..WorkspaceState::default()
            });
            let _ = app.workspace.resize_viewport(1000.0);
            assert_eq!(app.workspace.take_widget_focus_request(), Some(closing));
            // A real native frame is mounted, but no terminal or provider is
            // started. The accepted close's immutable receipt is supplied below.
            for card in [closing, survivor] {
                app.cards
                    .insert(card, CardRuntime::Failed("Fixture terminal".into()));
            }
            let CardKind::Terminal(session) = &mut app.card_state_mut(closing).unwrap().kind else {
                panic!("closing fixture is a terminal");
            };
            session.continuity = TerminalContinuity::Live {
                host_session_id: HostSessionId::new("late-close"),
                boot_id: BootId::new("late-close-boot"),
                host_incarnation: HostIncarnation::new("late-close-incarnation"),
            };
            (app, home, form, cache, closing, survivor)
        }

        fn local_close_effects(
            frame: &mut Frame<'_>,
            renderer: &cosmic::Renderer,
            task: Task<Message>,
        ) -> Vec<Message> {
            let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                return Vec::new();
            };
            let executor = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let mut messages = Vec::new();
            while let Some(action) = executor.block_on(stream.next()) {
                match action {
                    iced::runtime::Action::Widget(operation) => operate(frame, renderer, operation),
                    iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                        messages.push(message)
                    }
                    action => panic!("unexpected nonlocal close effect: {action:?}"),
                }
            }
            messages
        }

        #[test]
        fn open_form_polling_keeps_native_fields_and_roles_stationary_with_header_progress() {
            fn sized<'a>(
                app: &'a Cagents,
                cache: Cache,
                renderer: &mut cosmic::Renderer,
                size: iced::Size,
            ) -> Frame<'a> {
                let mut frame = iced::runtime::UserInterface::build(
                    cosmic::ApplicationExt::view_main(app),
                    size,
                    cache,
                    renderer,
                );
                draw(&mut frame, renderer);
                frame
            }
            fn geometry(
                frame: &mut Frame<'_>,
                renderer: &cosmic::Renderer,
                ids: &[widget::Id],
            ) -> Vec<iced::Rectangle> {
                let focus = probe(frame, renderer);
                ids.iter()
                    .map(|id| {
                        focus
                            .0
                            .iter()
                            .find(|node| node.id.as_ref().is_some_and(|actual| same(actual, id)))
                            .unwrap_or_else(|| panic!("actual control disappeared: {id:?}"))
                            .bounds
                    })
                    .collect()
            }
            fn unchanged(before: &[iced::Rectangle], after: &[iced::Rectangle]) {
                assert_eq!(before.len(), after.len());
                for (before, after) in before.iter().zip(after) {
                    assert!(
                        [
                            before.x - after.x,
                            before.y - after.y,
                            before.width - after.width,
                            before.height - after.height,
                        ]
                        .into_iter()
                        .all(|delta| delta.abs() <= 0.01),
                        "poll must not move a mounted field/role/Submit: {before:?} -> {after:?}"
                    );
                }
            }
            for size in [
                iced::Size::new(1280.0, 900.0),
                iced::Size::new(500.0, 1200.0),
            ] {
                let mut renderer = super::super::super::tests::headless_renderer();
                let (mut app, home, form, cache) = fixture(&mut renderer);
                app.window_width = size.width;
                app.window_height = size.height;
                let mut frame = sized(&app, cache, &mut renderer, size);
                let repository = input(&mut frame, &renderer, "-repo");
                let mut ids = vec![
                    input(&mut frame, &renderer, "-host"),
                    repository.clone(),
                    input(&mut frame, &renderer, "-number"),
                ];
                ids.extend(
                    ["Contributing", "Stewarding", "Review only", "Reference"]
                        .map(|name| labelled(&mut frame, &renderer, name).0),
                );
                ids.push(labelled(&mut frame, &renderer, "Track PR").0);
                let before = geometry(&mut frame, &renderer, &ids);
                if size.width < 560.0 {
                    assert!(
                        before[5].y > before[3].y,
                        "the mounted roles actually use the narrow layout"
                    );
                    assert_eq!(
                        before[3].y, before[4].y,
                        "first row is Contributing/Stewarding"
                    );
                    assert_eq!(before[5].y, before[6].y, "second row is Review/Reference");
                }
                let refresh = refresh_feedback(&mut frame, &renderer, false);
                assert!(iced::Rectangle::with_size(size).contains(before[1].center()));
                assert!(click(&mut frame, &mut renderer, before[1].center()).is_empty());
                assert!(same(&active(&mut frame, &renderer), &repository));
                let (messages, _) = event(
                    &mut frame,
                    &mut renderer,
                    &[command_key("a", keyboard::key::Code::KeyA)],
                    iced::mouse::Cursor::Unavailable,
                );
                assert!(messages.is_empty());
                let selection = repository_state(&frame);
                assert_eq!(selection, ("fixture/work".into(), 0, 12));
                let cache = frame.into_cache();
                let Some(Effect::Read {
                    ticket,
                    home: read_home,
                    ..
                }) = app.channel_work.view.refresh()
                else {
                    panic!("background read");
                };
                assert_eq!(read_home, home);
                let mut frame = sized(&app, cache, &mut renderer, size);
                assert!(!updating_caption(&mut frame, &renderer));
                assert_eq!(refresh_feedback(&mut frame, &renderer, true), refresh);
                unchanged(&before, &geometry(&mut frame, &renderer, &ids));
                assert!(same(&active(&mut frame, &renderer), &repository));
                assert_eq!(repository_state(&frame), selection);
                let cache = frame.into_cache();
                drop(app.update(Message::ChannelWork(ui::Action::Loaded {
                    ticket,
                    result: Ok(loaded(home, 2)),
                })));
                let mut frame = sized(&app, cache, &mut renderer, size);
                assert!(!updating_caption(&mut frame, &renderer));
                assert_eq!(refresh_feedback(&mut frame, &renderer, false), refresh);
                unchanged(&before, &geometry(&mut frame, &renderer, &ids));
                assert!(same(&active(&mut frame, &renderer), &repository));
                assert_eq!(repository_state(&frame), selection);
                let (_, submit) = labelled(&mut frame, &renderer, "Track PR");
                assert!(
                    iced::Rectangle::with_size(size).contains(submit.center()),
                    "actual Submit remains in the test viewport: {submit:?}"
                );
                let messages = click(&mut frame, &mut renderer, submit.center());
                let cache = frame.into_cache();
                let expected = crate::channel_work::Target::PullRequest(
                    crate::github_identity::PullRequestKey::from_url(
                        "https://github.com/fixture/work/pull/17",
                    )
                    .unwrap(),
                )
                .canonicalized()
                .unwrap();
                assert!(
                    matches!(messages.as_slice(), [cosmic::Action::App(Message::ChannelWork(ui::Action::Submit { owner, target, role: crate::channel_work::Role::Contribute, revision: 0 }))]
                    if *owner == form && target == &expected)
                );
                // Do not dispatch the captured mutation. Manual refresh still
                // uses its existing network-typed callback, unlike the Read above.
                let mut frame = sized(&app, cache, &mut renderer, size);
                let messages = click(&mut frame, &mut renderer, refresh.center());
                let _ = frame.into_cache();
                let [
                    cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Refresh(actual))),
                ] = messages.as_slice()
                else {
                    panic!("actual manual refresh: {messages:?}");
                };
                assert_eq!(*actual, app.channel_work.view.owner().unwrap());
                assert!(
                    matches!(app.channel_work.view.update(action.clone()), Some(Effect::Refresh { home: actual, .. }) if actual == home)
                );
                assert!(app.channel_work.view.pending_homes().next().is_none());
            }
            // Without an editor, retain the existing body loading message.
            let (mut app, home) = pr_fixture();
            app.core.set_main_window_id(Some(window::Id::RESERVED));
            let Some(Effect::Read { ticket, .. }) = app.channel_work.view.refresh() else {
                panic!("read");
            };
            let mut renderer = super::super::super::tests::headless_renderer();
            let mut frame = mount(&app, Cache::new(), &mut renderer);
            assert!(!updating_caption(&mut frame, &renderer));
            let refresh = refresh_feedback(&mut frame, &renderer, true);
            assert!(
                click(&mut frame, &mut renderer, refresh.center()).is_empty(),
                "busy refresh has no callback"
            );
            let cache = frame.into_cache();
            drop(app.update(Message::ChannelWork(ui::Action::Loaded {
                ticket,
                result: Ok(loaded(home, 2)),
            })));
            let mut frame = mount(&app, cache, &mut renderer);
            assert!(!updating_caption(&mut frame, &renderer));
            assert_eq!(refresh_feedback(&mut frame, &renderer, false), refresh);
        }
        #[test]
        fn repository_full_url_typing_preserves_every_native_character_and_exact_submit_target() {
            const URL: &str = "https://github.com/NativeFixtureFork/cagents/pull/1568";
            assert_eq!(URL.len(), 54);
            let (mut app, home) = pr_fixture();
            app.window_width = 1280.0;
            app.window_height = 900.0;
            app.core.set_main_window_id(Some(window::Id::RESERVED));
            let owner = app.channel_work.view.owner().unwrap();
            assert!(matches!(
                app.channel_work.view.update(ui::Action::Attach(owner)),
                Some(Effect::Focus { .. })
            ));
            let mut renderer = super::super::super::tests::headless_renderer();
            let mut frame = mount(&app, Cache::new(), &mut renderer);
            let repository = input(&mut frame, &renderer, "-repo");
            let point = probe(&mut frame, &renderer)
                .0
                .iter()
                .find(|node| node.id.as_ref().is_some_and(|id| same(id, &repository)))
                .unwrap()
                .bounds
                .center();
            assert!(click(&mut frame, &mut renderer, point).is_empty());
            let cache = frame.into_cache();
            let mut form = None;
            // Each actual widget callback goes through App::update and the
            // full frame is rebuilt from Cache before typing the next byte.
            // In particular /pull/1 must not replace the field before 568.
            let cache = repository_characters(
                &mut app,
                cache,
                &mut renderer,
                &repository,
                &mut form,
                "",
                URL,
            );
            let mut frame = mount(&app, cache, &mut renderer);
            assert!(same(&active(&mut frame, &renderer), &repository));
            assert_eq!(repository_state(&frame), (URL.into(), 54, 54));
            let (_, submit) = labelled(&mut frame, &renderer, "Track PR");
            let messages = click(&mut frame, &mut renderer, submit.center());
            let _ = frame.into_cache();
            let expected = crate::channel_work::Target::PullRequest(
                crate::github_identity::PullRequestKey::from_url(URL).unwrap(),
            )
            .canonicalized()
            .unwrap();
            assert!(
                matches!(messages.as_slice(), [cosmic::Action::App(Message::ChannelWork(ui::Action::Submit { owner: actual, target, role: crate::channel_work::Role::Contribute, revision: 0 }))] if Some(*actual) == form && target == &expected),
                "the complete typed fork/number must be the only Submit: {messages:?}"
            );
            assert_eq!(app.channel_work.view.home(), Some(home));
            assert!(app.channel_work.view.pending_homes().next().is_none());
            let cosmic::Action::App(message) = messages.into_iter().next().unwrap() else {
                unreachable!()
            };
            drop(app.update(message));
            assert_eq!(
                app.channel_work.view.pending_homes().collect::<Vec<_>>(),
                vec![home]
            );
        }
        #[test]
        fn repository_native_focus_and_selection_survive_poll_loading_and_matching_reply() {
            const URL: &str = "https://github.com/NativeFixtureFork/cagents/pull/1568";
            const PREFIX: &str = "https://github.com/NativeFixtureFork/cagents/p";
            let (mut app, home) = pr_fixture();
            app.window_width = 1280.0;
            app.window_height = 900.0;
            app.core.set_main_window_id(Some(window::Id::RESERVED));
            let owner = app.channel_work.view.owner().unwrap();
            assert!(matches!(
                app.channel_work.view.update(ui::Action::Attach(owner)),
                Some(Effect::Focus { .. })
            ));
            let mut renderer = super::super::super::tests::headless_renderer();
            let mut frame = mount(&app, Cache::new(), &mut renderer);
            let repository = input(&mut frame, &renderer, "-repo");
            let focus = probe(&mut frame, &renderer);
            let point = focus
                .0
                .iter()
                .find(|node| node.id.as_ref().is_some_and(|id| same(id, &repository)))
                .unwrap()
                .bounds
                .center();
            assert!(click(&mut frame, &mut renderer, point).is_empty());
            assert!(same(&active(&mut frame, &renderer), &repository));
            let cache = frame.into_cache();
            let mut form = None;
            let cache = repository_characters(
                &mut app,
                cache,
                &mut renderer,
                &repository,
                &mut form,
                "",
                PREFIX,
            );
            assert_eq!(
                PREFIX.len(),
                46,
                "the observed native interruption boundary"
            );

            // This is the same State::refresh used by Message::ChannelWorkPoll.
            // Keep the ticket rather than running its transport, so the reply
            // is exact and the test stays entirely in the actual cached UI.
            let Some(Effect::Read {
                ticket,
                home: read_home,
                ..
            }) = app.channel_work.view.refresh()
            else {
                panic!("poll must start a fresh read");
            };
            assert_eq!(read_home, home);
            let mut frame = mount(&app, cache, &mut renderer);
            assert!(!updating_caption(&mut frame, &renderer));
            refresh_feedback(&mut frame, &renderer, true);
            assert!(
                same(&active(&mut frame, &renderer), &repository),
                "adding the poll caption must not blur Repository"
            );
            assert_eq!(repository_state(&frame), (PREFIX.into(), 46, 46));
            let cache = frame.into_cache();
            // Keep this poll diagnostic at the incomplete /pull/ boundary.
            // The separate full-URL typing parent covers all numeric digits.
            let cache = repository_characters(
                &mut app,
                cache,
                &mut renderer,
                &repository,
                &mut form,
                PREFIX,
                "ull/",
            );
            drop(app.update(Message::ChannelWork(ui::Action::Loaded {
                ticket,
                result: Ok(loaded(home, 2)),
            })));
            let mut frame = mount(&app, cache, &mut renderer);
            assert!(!updating_caption(&mut frame, &renderer));
            refresh_feedback(&mut frame, &renderer, false);
            assert!(
                same(&active(&mut frame, &renderer), &repository),
                "removing the poll caption must not blur Repository"
            );
            let incomplete = format!("{PREFIX}ull/");
            assert_eq!(repository_state(&frame), (incomplete.clone(), 50, 50));
            let (messages, _) = event(
                &mut frame,
                &mut renderer,
                &[command_key("a", keyboard::key::Code::KeyA)],
                iced::mouse::Cursor::Unavailable,
            );
            assert!(
                messages.is_empty(),
                "select-all must not edit the field: {messages:?}"
            );
            assert_eq!(repository_state(&frame), (incomplete.clone(), 0, 50));
            let cache = frame.into_cache();

            let Some(Effect::Read { ticket, .. }) = app.channel_work.view.refresh() else {
                panic!("second poll");
            };
            let mut frame = mount(&app, cache, &mut renderer);
            assert!(!updating_caption(&mut frame, &renderer));
            refresh_feedback(&mut frame, &renderer, true);
            assert!(same(&active(&mut frame, &renderer), &repository));
            assert_eq!(repository_state(&frame), (incomplete.clone(), 0, 50));
            let cache = frame.into_cache();
            drop(app.update(Message::ChannelWork(ui::Action::Loaded {
                ticket,
                result: Ok(loaded(home, 3)),
            })));
            let mut frame = mount(&app, cache, &mut renderer);
            assert!(!updating_caption(&mut frame, &renderer));
            refresh_feedback(&mut frame, &renderer, false);
            assert!(same(&active(&mut frame, &renderer), &repository));
            assert_eq!(repository_state(&frame), (incomplete, 0, 50));
            let mut messages = Vec::new();
            let mut clipboard = RepositoryClipboard(URL.into());
            let _ = frame.update(
                &[
                    command_key("v", keyboard::key::Code::KeyV),
                    iced::Event::Keyboard(keyboard::Event::KeyReleased {
                        key: keyboard::Key::Character("v".into()),
                        modified_key: keyboard::Key::Character("v".into()),
                        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyV),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::COMMAND,
                    }),
                ],
                iced::mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard,
                &mut messages,
            );
            draw(&mut frame, &mut renderer);
            assert!(
                matches!(messages.as_slice(), [cosmic::Action::App(Message::ChannelWork(ui::Action::Field(actual, ui::Field::Repository, value)))] if Some(*actual) == form && value == URL),
                "native paste must replace the retained full selection: {messages:?}"
            );
            let cache = frame.into_cache();
            let cosmic::Action::App(message) = messages.into_iter().next().unwrap() else {
                unreachable!()
            };
            drop(app.update(message));
            let mut frame = mount(&app, cache, &mut renderer);
            assert!(same(&active(&mut frame, &renderer), &repository));
            assert_eq!(repository_state(&frame).0, URL);
            let (_, submit) = labelled(&mut frame, &renderer, "Track PR");
            let messages = click(&mut frame, &mut renderer, submit.center());
            let _ = frame.into_cache();
            let expected = crate::channel_work::Target::PullRequest(
                crate::github_identity::PullRequestKey::from_url(URL).unwrap(),
            )
            .canonicalized()
            .unwrap();
            assert!(
                matches!(messages.as_slice(), [cosmic::Action::App(Message::ChannelWork(ui::Action::Submit { owner: actual, target, role: crate::channel_work::Role::Contribute, revision: 0 }))] if Some(*actual) == form && target == &expected),
                "native submit must still capture the exact complete target: {messages:?}"
            );
            assert_eq!(app.channel_work.view.home(), Some(home));
            assert!(
                app.channel_work.view.pending_homes().next().is_none(),
                "paste/captured Submit is not dispatched by this focus test"
            );
        }
        #[test]
        fn role_native_focus_and_next_tab_survive_poll_loading_and_matching_reply() {
            for index in 0..4 {
                let mut renderer = super::super::super::tests::headless_renderer();
                let (mut app, home, _, cache) = fixture(&mut renderer);
                let mut frame = mount(&app, cache, &mut renderer);
                let roles = ["Contributing", "Stewarding", "Review only", "Reference"]
                    .map(|label| labelled(&mut frame, &renderer, label).0);
                let submit = labelled(&mut frame, &renderer, "Track PR").0;
                // Ordinary Tab from the real retained input reaches the role;
                // no direct role focus operation or reset masks restoration.
                seek(&mut frame, &mut renderer, &roles[index]);
                let cache = frame.into_cache();
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(
                    same(&active(&mut frame, &renderer), &roles[index]),
                    "unchanged cached frame is a positive control"
                );
                let cache = frame.into_cache();
                let Some(Effect::Read { ticket, .. }) = app.channel_work.view.refresh() else {
                    panic!("poll");
                };
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(!updating_caption(&mut frame, &renderer));
                refresh_feedback(&mut frame, &renderer, true);
                assert!(
                    same(&active(&mut frame, &renderer), &roles[index]),
                    "quiet loading-row insertion must retain role{index} native focus"
                );
                let cache = frame.into_cache();
                drop(app.update(Message::ChannelWork(ui::Action::Loaded {
                    ticket,
                    result: Ok(loaded(home, 2)),
                })));
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(!updating_caption(&mut frame, &renderer));
                refresh_feedback(&mut frame, &renderer, false);
                assert!(
                    same(&active(&mut frame, &renderer), &roles[index]),
                    "quiet matching reply must retain role{index} native focus"
                );
                let next = if index == 3 {
                    &submit
                } else {
                    &roles[index + 1]
                };
                assert!(
                    same(&tab(&mut frame, &mut renderer, false), next),
                    "next Tab must advance inside the form, not restart at global chrome"
                );
                assert_eq!(app.channel_work.view.home(), Some(home));
            }
        }
        #[test]
        fn full_window_tab_cycle_reaches_work_fields_roles_and_submit_after_cached_frames() {
            for number_click in [false, true] {
                let mut renderer = super::super::super::tests::headless_renderer();
                let (mut app, home, form, cache) = fixture(&mut renderer);
                let mut frame = mount(&app, cache, &mut renderer);
                let host = input(&mut frame, &renderer, "-host");
                let repo = input(&mut frame, &renderer, "-repo");
                let number = input(&mut frame, &renderer, "-number");
                let roles = ["Contributing", "Stewarding", "Review only", "Reference"]
                    .map(|label| labelled(&mut frame, &renderer, label).0);
                let submit = labelled(&mut frame, &renderer, "Track PR").0;
                let focus = probe(&mut frame, &renderer);
                assert!(
                    focus.0.len() > 8,
                    "count every global and form focusable, not only WindowClose: {focus:?}"
                );
                if number_click {
                    let point = focus
                        .0
                        .iter()
                        .find(|node| node.id.as_ref().is_some_and(|id| same(id, &number)))
                        .unwrap()
                        .bounds
                        .center();
                    assert!(click(&mut frame, &mut renderer, point).is_empty());
                    assert!(
                        same(&active(&mut frame, &renderer), &number),
                        "real Number click must own native focus exclusively"
                    );
                    assert!(same(&tab(&mut frame, &mut renderer, false), &roles[0]));
                } else {
                    // Identify the actual window-close button by its own native
                    // callback; never dispatch that close in this fixture.
                    let close = focus
                        .0
                        .iter()
                        .filter(|node| {
                            node.bounds.y < design::HEADER_HEIGHT && node.bounds.width <= 48.0
                        })
                        .max_by(|a, b| a.bounds.x.total_cmp(&b.bounds.x))
                        .expect("window chrome close control");
                    let close_id = close.id.clone().unwrap();
                    let close_messages = click(&mut frame, &mut renderer, close.bounds.center());
                    assert!(
                        matches!(
                            close_messages.as_slice(),
                            [cosmic::Action::App(Message::WindowCloseRequested(id))]
                            if *id == window::Id::RESERVED
                        ),
                        "actual window-close callback: {close_messages:?}"
                    );
                    // Keep the actual post-click state: resetting all focus
                    // here would conceal the suspected retained/dual-focus
                    // failure before the next Tab could expose it.
                    let _ = probe(&mut frame, &renderer);
                    seek(&mut frame, &mut renderer, &close_id);
                    seek(&mut frame, &mut renderer, &host);
                    assert!(same(&tab(&mut frame, &mut renderer, false), &repo));
                    assert!(same(&tab(&mut frame, &mut renderer, false), &number));
                    assert!(same(&tab(&mut frame, &mut renderer, false), &roles[0]));
                }
                for role in &roles[1..] {
                    assert!(same(&tab(&mut frame, &mut renderer, false), role));
                }
                assert!(same(&tab(&mut frame, &mut renderer, false), &submit));
                for expected in roles.iter().rev().chain([&number, &repo, &host]) {
                    assert!(same(&tab(&mut frame, &mut renderer, true), expected));
                }
                seek(&mut frame, &mut renderer, &roles[3]);
                let cache = frame.into_cache();
                // Neither an unchanged full frame nor a presentation-only
                // rename may replace native focus with the chrome close button.
                app.channel_work
                    .view
                    .set_title(home, "Renamed channel".into());
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(same(&active(&mut frame, &renderer), &roles[3]));
                let (messages, _) = event(
                    &mut frame,
                    &mut renderer,
                    &[key(
                        keyboard::Key::Named(keyboard::key::Named::Enter),
                        keyboard::key::Code::Enter,
                        None,
                    )],
                    iced::mouse::Cursor::Unavailable,
                );
                assert!(
                    matches!(messages.as_slice(), [cosmic::Action::App(Message::ChannelWork(ui::Action::Role(actual, 3)))] if *actual == form),
                    "Enter must activate the traversed Reference button: {messages:?}"
                );
                let cache = frame.into_cache();
                let cosmic::Action::App(selected) = messages.into_iter().next().unwrap() else {
                    unreachable!()
                };
                drop(app.update(selected));
                assert!(
                    app.core.keyboard_nav(),
                    "selecting a role must not disable global Tab"
                );
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(same(&active(&mut frame, &renderer), &roles[3]));
                // Escape belongs to the focused PR form in the inspector.
                let (navigation, _) = event(
                    &mut frame,
                    &mut renderer,
                    &[key(
                        keyboard::Key::Named(keyboard::key::Named::Escape),
                        keyboard::key::Code::Escape,
                        None,
                    )],
                    iced::mouse::Cursor::Unavailable,
                );
                assert!(
                    matches!(
                        navigation.as_slice(),
                        [cosmic::Action::App(Message::ChannelWork(
                            ui::Action::Dismiss(_)
                        ))]
                    ),
                    "{navigation:?}"
                );
                let cache = frame.into_cache();
                let cosmic::Action::App(owned_escape) = navigation.into_iter().next().unwrap()
                else {
                    unreachable!()
                };
                let focus_keep = app.update(owned_escape);
                assert!(app.context_inspector_visible());
                assert!(app.channel_work.view.has_confirmation());
                assert_eq!(app.channel_work.view.home(), Some(home));
                let mut frame = mount(&app, cache, &mut renderer);
                apply_focus_task(&mut frame, &renderer, focus_keep);
                let (keep_id, keep_bounds) = labelled(&mut frame, &renderer, "Keep editing");
                assert!(same(&active(&mut frame, &renderer), &keep_id));
                let kept = click(&mut frame, &mut renderer, keep_bounds.center());
                let [
                    cosmic::Action::App(Message::ChannelWork(ui::Action::Keep(view, confirmation))),
                ] = kept.as_slice()
                else {
                    panic!("actual Keep callback: {kept:?}");
                };
                let (view, confirmation) = (*view, *confirmation);
                let cache = frame.into_cache();
                let focus_role =
                    app.update(Message::ChannelWork(ui::Action::Keep(view, confirmation)));
                assert!(app.context_inspector_visible());
                assert!(!app.channel_work.view.has_confirmation());
                assert_eq!(app.channel_work.view.home(), Some(home));
                assert_eq!(app.durable.active_channel, Some(home.channel));
                assert!(
                    app.core.keyboard_nav(),
                    "Keep must preserve global traversal admission"
                );
                let mut frame = mount(&app, cache, &mut renderer);
                apply_focus_task(&mut frame, &renderer, focus_role);
                assert!(same(&active(&mut frame, &renderer), &roles[3]));
                let (_, submit_bounds) = labelled(&mut frame, &renderer, "Track PR");
                assert!(
                    iced::Rectangle::with_size(iced::Size::new(1280.0, 900.0))
                        .contains(submit_bounds.center()),
                    "actual native submit must be in the viewport: {submit_bounds:?}"
                );
                let submitted = click(&mut frame, &mut renderer, submit_bounds.center());
                let _ = frame.into_cache();
                let [
                    cosmic::Action::App(Message::ChannelWork(ui::Action::Submit {
                        owner,
                        role: crate::channel_work::Role::Reference,
                        target,
                        revision: 0,
                    })),
                ] = submitted.as_slice()
                else {
                    panic!(
                        "keyboard Reference must not steal the later pointer submit: {submitted:?}"
                    );
                };
                assert_eq!(*owner, form);
                assert_eq!(
                    target,
                    &crate::channel_work::Target::PullRequest(
                        crate::github_identity::PullRequestKey::from_url(
                            "https://github.com/fixture/work/pull/17"
                        )
                        .unwrap()
                    )
                );
                let cosmic::Action::App(submit) = submitted.into_iter().next().unwrap() else {
                    unreachable!()
                };
                // Dispatch the actual full-frame callback through App, then
                // drop its unpolled transport task: no fixture RPC is run.
                drop(app.update(submit));
                assert_eq!(
                    app.channel_work.view.pending_homes().collect::<Vec<_>>(),
                    [home]
                );
            }
        }

        #[test]
        fn pr_focus_effects_require_the_same_visible_inspector_without_higher_input_owner() {
            let mut renderer = super::super::super::tests::headless_renderer();
            for blocked in ["none", "hidden", "files", "modal", "update"] {
                let (mut app, _, _, cache) = fixture(&mut renderer);
                let owner = app.channel_work.view.owner().unwrap();
                let mut frame = mount(&app, cache, &mut renderer);
                let repository = input(&mut frame, &renderer, "-repo");
                let cache = frame.into_cache();
                match blocked {
                    "hidden" => {
                        app.context_inspector_open = false;
                        app.context_inspector_transition = None;
                    }
                    "files" => app.context_tab = ContextTab::Files,
                    "modal" => app.settings_reset_confirm = true,
                    "update" => app.update_prepared = true,
                    _ => {}
                }
                let task = app.channel_work_effect(Effect::Focus {
                    owner,
                    id: repository.clone(),
                });
                if blocked == "none" {
                    let mut frame = mount(&app, cache, &mut renderer);
                    apply_focus_task(&mut frame, &renderer, task);
                    assert!(same(&active(&mut frame, &renderer), &repository));
                } else {
                    assert!(
                        iced::runtime::task::into_stream(task).is_none(),
                        "late focus must not steal input behind {blocked}"
                    );
                }
            }
            let (mut app, home) = pr_fixture();
            let owner = app.channel_work.view.owner().unwrap();
            let Some(Effect::Focus { id, .. }) =
                app.channel_work.view.update(ui::Action::Attach(owner))
            else {
                panic!()
            };
            assert!(
                matches!(
                    app.channel_work.view.update(ui::Action::Close(owner)),
                    Some(Effect::Close { .. })
                ),
                "an unedited native form retires through its real reducer"
            );
            let other = Home {
                channel: ChannelId::new(),
                ..home
            };
            assert!(app.channel_work.view.open(other, "Other".into()).is_some());
            assert_ne!(app.channel_work.view.owner(), Some(owner));
            assert!(
                iced::runtime::task::into_stream(
                    app.channel_work_effect(Effect::Focus { owner, id })
                )
                .is_none(),
                "retired owner focus never targets its replacement"
            );
        }

        #[test]
        fn pr_context_keyboard_escape_keep_and_discard_preserve_exact_form_owner() {
            for width in [500.0, 1280.0] {
                let mut renderer = super::super::super::tests::headless_renderer();
                let (mut app, home, form, cache) = fixture(&mut renderer);
                app.window_width = width;
                app.window_height = 900.0;
                assert_eq!(app.context_inspector_is_overlay(), width < 800.0);
                let mut frame = iced::runtime::UserInterface::build(
                    cosmic::ApplicationExt::view_main(&app),
                    iced::Size::new(width, 900.0),
                    cache,
                    &mut renderer,
                );
                draw(&mut frame, &mut renderer);
                let repository = input(&mut frame, &renderer, "-repo");
                seek(&mut frame, &mut renderer, &repository);
                let (escape, _) = event(
                    &mut frame,
                    &mut renderer,
                    &[key(
                        keyboard::Key::Named(keyboard::key::Named::Escape),
                        keyboard::key::Code::Escape,
                        None,
                    )],
                    iced::mouse::Cursor::Unavailable,
                );
                assert!(matches!(
                    escape.as_slice(),
                    [cosmic::Action::App(Message::ChannelWork(
                        ui::Action::Dismiss(_)
                    ))]
                ));
                let cache = frame.into_cache();
                let cosmic::Action::App(owned_escape) = escape.into_iter().next().unwrap() else {
                    unreachable!()
                };
                let task = app.update(owned_escape);
                assert!(app.channel_work.view.has_confirmation());
                assert_eq!(app.channel_work.view.home(), Some(home));
                let mut frame = iced::runtime::UserInterface::build(
                    cosmic::ApplicationExt::view_main(&app),
                    iced::Size::new(width, 900.0),
                    cache,
                    &mut renderer,
                );
                draw(&mut frame, &mut renderer);
                apply_focus_task(&mut frame, &renderer, task);
                let (_, keep) = labelled(&mut frame, &renderer, "Keep editing");
                let messages = click(&mut frame, &mut renderer, keep.center());
                let [cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Keep(..)))] =
                    messages.as_slice()
                else {
                    panic!("{messages:?}")
                };
                let action = action.clone();
                let cache = frame.into_cache();
                let task = app.update_channel_work(action);
                assert!(!app.channel_work.view.has_confirmation());
                assert_eq!(app.channel_work.view.owner().unwrap().home, home);
                let mut frame = iced::runtime::UserInterface::build(
                    cosmic::ApplicationExt::view_main(&app),
                    iced::Size::new(width, 900.0),
                    cache,
                    &mut renderer,
                );
                draw(&mut frame, &mut renderer);
                apply_focus_task(&mut frame, &renderer, task);
                assert_eq!(repository_state(&frame).0, "fixture/work");
                assert!(same(&active(&mut frame, &renderer), &repository));
                let cache = frame.into_cache();
                let task = app.update_channel_work(app.channel_work.view.escape_action().unwrap());
                let mut frame = iced::runtime::UserInterface::build(
                    cosmic::ApplicationExt::view_main(&app),
                    iced::Size::new(width, 900.0),
                    cache,
                    &mut renderer,
                );
                draw(&mut frame, &mut renderer);
                apply_focus_task(&mut frame, &renderer, task);
                let (_, discard) = labelled(&mut frame, &renderer, "Discard draft");
                let messages = click(&mut frame, &mut renderer, discard.center());
                let [cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Discard(..)))] =
                    messages.as_slice()
                else {
                    panic!("{messages:?}")
                };
                let action = action.clone();
                let _ = frame.into_cache();
                drop(app.update_channel_work(action));
                assert!(!app.channel_work.view.has_confirmation());
                assert_eq!(app.channel_work.view.home(), Some(home));
                assert!(app.context_inspector_open);
                // Dismissing a draft is not navigation. Its queued old field
                // callback cannot re-create a form or start a mutation.
                drop(app.update_channel_work(ui::Action::Field(
                    form,
                    ui::Field::Repository,
                    "wrong/repo".into(),
                )));
                assert!(app.channel_work.view.pending_homes().next().is_none());
            }
        }
        #[test]
        fn late_checked_close_preserves_docked_and_overlay_pr_selection_and_visible_workspace_handoff()
         {
            let mut renderer = super::super::super::tests::headless_renderer();
            for (width, work) in [(640.0, true), (1280.0, true), (1280.0, false)] {
                let (mut app, home, form, cache, closing, survivor) =
                    closing_fixture(&mut renderer);
                app.window_width = width;
                // Start the actual asynchronous close reducer, but do not poll
                // its RPC task. Navigation is permitted while this is pending.
                drop(app.begin_card_close(closing));
                assert!(app.terminal_close_pending(closing));
                assert!(!app.modal_is_open());
                let intent = app.terminal_close.as_ref().unwrap().clone();
                app.context_inspector_open = work;
                app.context_inspector_transition = None;
                let mut frame = mount(&app, cache, &mut renderer);
                let repository = if work {
                    let id = input(&mut frame, &renderer, "-repo");
                    seek(&mut frame, &mut renderer, &id);
                    let (messages, _) = event(
                        &mut frame,
                        &mut renderer,
                        &[command_key("a", keyboard::key::Code::KeyA)],
                        iced::mouse::Cursor::Unavailable,
                    );
                    assert!(messages.is_empty(), "native selection: {messages:?}");
                    assert_eq!(repository_state(&frame), ("fixture/work".into(), 0, 12));
                    Some(id)
                } else {
                    let id = app.card_keyboard_focus(closing);
                    operate(
                        &mut frame,
                        &renderer,
                        Box::new(operation::focusable::focus::<()>(id.clone())),
                    );
                    assert!(same(&active(&mut frame, &renderer), &id));
                    None
                };
                let cache = frame.into_cache();
                let accepted = app.terminal_close_finished(
                    closing,
                    intent.terminal,
                    intent.proof,
                    intent.generation,
                    terminal_close::CloseOutcome::Closed,
                );
                assert!(app.terminal_close.is_none());
                assert!(app.closed_terminal_cards.contains(&closing));
                let mut frame = mount(&app, cache, &mut renderer);
                let messages = local_close_effects(&mut frame, &renderer, accepted);
                assert!(
                    matches!(messages.as_slice(), [Message::CommitClose(card)] if *card == closing),
                    "accepted close must still request its exact commit: {messages:?}"
                );
                let expected = repository
                    .clone()
                    .unwrap_or_else(|| app.card_keyboard_focus(survivor));
                assert!(same(&active(&mut frame, &renderer), &expected));
                let cache = frame.into_cache();
                // This is CommitClose's actual reducer after the checked-close
                // receipt admitted the same original. No transport is polled.
                let committed = app.commit_card_removal(closing, None).unwrap();
                assert!(!app.card_state(closing).unwrap().existence.is_open());
                assert!(!app.cards.contains_key(&closing));
                assert_eq!(app.workspace.focused(), Some(survivor));
                let mut frame = mount(&app, cache, &mut renderer);
                let messages = local_close_effects(&mut frame, &renderer, committed);
                assert!(
                    messages.is_empty(),
                    "unexpected commit callback: {messages:?}"
                );
                assert!(same(&active(&mut frame, &renderer), &expected));
                if work {
                    assert_eq!(app.channel_work.view.home(), Some(home));
                    assert_eq!(repository_state(&frame), ("fixture/work".into(), 0, 12));
                    let (messages, _) = event(
                        &mut frame,
                        &mut renderer,
                        &[key(
                            keyboard::Key::Character("z".into()),
                            keyboard::key::Code::KeyZ,
                            Some("z"),
                        )],
                        iced::mouse::Cursor::Unavailable,
                    );
                    assert!(
                        matches!(messages.as_slice(), [cosmic::Action::App(Message::ChannelWork(ui::Action::Field(owner, ui::Field::Repository, value)))] if *owner == form && value == "z"),
                        "ordinary typing must replace the retained selection: {messages:?}"
                    );
                }
            }
        }

        #[test]
        fn deferred_offscreen_close_preserves_foreign_pr_focus_and_recovers_an_unowned_survivor() {
            use crate::workspace::{
                CardWidth, FocusTarget, Workspace, WorkspaceCard, WorkspaceConfig,
            };

            let mut renderer = super::super::super::tests::headless_renderer();
            for foreign_focus in [true, false] {
                let (mut app, _, _, cache, closing, survivor) = closing_fixture(&mut renderer);
                app.window_width = 1280.0;
                app.context_inspector_open = foreign_focus;
                app.context_inspector_transition = None;
                // Keep the neighbor outside both the workspace's mounted band
                // and the actual full-window native tree until close animates.
                app.workspace = Workspace::new(WorkspaceConfig {
                    overscan: 0.0,
                    ..WorkspaceConfig::default()
                })
                .unwrap();
                app.workspace.resize_viewport(1000.0);
                for (index, card) in [closing, survivor].into_iter().enumerate() {
                    app.workspace
                        .add_card(
                            WorkspaceCard::new(card, "Fixture terminal", CardWidth::Fixed(1920.0))
                                .unwrap(),
                            index,
                            index == 0,
                        )
                        .unwrap();
                }
                assert_eq!(app.workspace.take_widget_focus_request(), Some(closing));
                assert_eq!(
                    app.workspace.visibility(survivor),
                    Some(VisibilityClass::Offscreen)
                );
                let survivor_id = app.card_keyboard_focus(survivor);
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(
                    !probe(&mut frame, &renderer)
                        .0
                        .iter()
                        .any(|node| { node.id.as_ref().is_some_and(|id| same(id, &survivor_id)) }),
                    "the survivor must really be absent before close"
                );
                let repository = if foreign_focus {
                    let id = input(&mut frame, &renderer, "-repo");
                    seek(&mut frame, &mut renderer, &id);
                    let (messages, _) = event(
                        &mut frame,
                        &mut renderer,
                        &[command_key("a", keyboard::key::Code::KeyA)],
                        iced::mouse::Cursor::Unavailable,
                    );
                    assert!(messages.is_empty());
                    Some(id)
                } else {
                    let closing_id = app.card_keyboard_focus(closing);
                    operate(
                        &mut frame,
                        &renderer,
                        Box::new(operation::focusable::focus::<()>(closing_id.clone())),
                    );
                    assert!(same(&active(&mut frame, &renderer), &closing_id));
                    None
                };
                let cache = frame.into_cache();
                drop(app.begin_card_close(closing));
                let intent = app.terminal_close.as_ref().unwrap().clone();
                let accepted = app.terminal_close_finished(
                    closing,
                    intent.terminal,
                    intent.proof,
                    intent.generation,
                    terminal_close::CloseOutcome::Closed,
                );
                assert_eq!(
                    app.workspace.pending_close().unwrap().target,
                    FocusTarget::WorkspaceToolbar
                );
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(
                    matches!(local_close_effects(&mut frame, &renderer, accepted).as_slice(),
                    [Message::CommitClose(card)] if *card == closing)
                );
                if let Some(repository) = &repository {
                    assert!(same(&active(&mut frame, &renderer), repository));
                } else {
                    assert!(same(
                        &active(&mut frame, &renderer),
                        &widget::Id::new(TOOLBAR_FOCUS_ID)
                    ));
                }
                let cache = frame.into_cache();
                let committed = app.commit_card_removal(closing, None).unwrap();
                assert!(app.workspace.pending_widget_focus_is_close_handoff());
                assert_eq!(
                    app.workspace.visibility(survivor),
                    Some(VisibilityClass::Offscreen)
                );
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(local_close_effects(&mut frame, &renderer, committed).is_empty());
                let cache = frame.into_cache();
                // The user may leave the toolbar while the survivor animates
                // into view. An absent native owner permits recovery; a real
                // PR selection must remain authoritative after that same delay.
                let mut frame = mount(&app, cache, &mut renderer);
                if !foreign_focus {
                    operate(
                        &mut frame,
                        &renderer,
                        Box::new(operation::focusable::unfocus::<()>()),
                    );
                    assert!(
                        !probe(&mut frame, &renderer)
                            .0
                            .iter()
                            .any(|node| node.focused)
                    );
                }
                let cache = frame.into_cache();
                let update = app.workspace.tick(Duration::from_secs(1));
                assert!(
                    app.workspace
                        .visibility(survivor)
                        .is_some_and(VisibilityClass::is_mounted)
                );
                let task = app.apply_viewport(update, false, false);
                assert!(!app.workspace.pending_widget_focus_is_close_handoff());
                let mut frame = mount(&app, cache, &mut renderer);
                assert!(
                    probe(&mut frame, &renderer)
                        .0
                        .iter()
                        .any(|node| { node.id.as_ref().is_some_and(|id| same(id, &survivor_id)) }),
                    "the survivor must really mount before consuming its focus request"
                );
                let messages = local_close_effects(&mut frame, &renderer, task);
                assert!(
                    messages
                        .iter()
                        .all(|message| matches!(message, Message::LifecycleSweep))
                );
                if let Some(repository) = repository {
                    assert!(same(&active(&mut frame, &renderer), &repository));
                    assert_eq!(repository_state(&frame), ("fixture/work".into(), 0, 12));
                } else {
                    assert!(same(&active(&mut frame, &renderer), &survivor_id));
                }
                assert_eq!(app.workspace.focused(), Some(survivor));
                assert!(!app.card_state(closing).unwrap().existence.is_open());
            }
        }

        #[test]
        fn unknown_pr_mutation_keeps_its_original_route_across_channel_context_rebinding() {
            let mut renderer = super::super::super::tests::headless_renderer();
            let (mut app, home, _, cache) = fixture(&mut renderer);
            let (other, _, _) = two_channels(&mut app, home);
            let original = app.channel_work.routes.get(&home).unwrap().clone();
            let mut frame = mount(&app, cache, &mut renderer);
            let (_, submit) = labelled(&mut frame, &renderer, "Track PR");
            let messages = click(&mut frame, &mut renderer, submit.center());
            let [cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Submit { .. }))] =
                messages.as_slice()
            else {
                panic!("actual native submit: {messages:?}");
            };
            let action = action.clone();
            let _ = frame.into_cache();
            let Some(effect @ Effect::Mutate { .. }) = app.channel_work.view.update(action) else {
                panic!("native submit must produce exactly one mutation");
            };
            let Effect::Mutate { ticket, mutation } = &effect else {
                unreachable!()
            };
            let (ticket, operation) = (*ticket, mutation.operation);
            drop(app.channel_work_effect(effect)); // never poll fixture RPC
            Arc::make_mut(&mut app.durable).active_channel = Some(other);
            drop(app.sync_channel_pull_request_context());
            let current = app.channel_work.view.owner().unwrap();
            assert_eq!(current.home.channel, other);
            let before = (
                app.durable.active_channel,
                app.workspace.focused(),
                app.workspace_view_generation,
            );
            drop(app.update_channel_work(ui::Action::Applied {
                ticket,
                result: Err(Failure::Unavailable),
            }));
            assert_eq!(app.channel_work.view.owner(), Some(current));
            assert_eq!(
                app.channel_work.view.pending_homes().collect::<Vec<_>>(),
                [home]
            );
            let retained = app.channel_work.routes.get(&home).unwrap();
            assert_eq!(retained.socket, original.socket);
            assert!(Arc::ptr_eq(&retained.proof, &original.proof));
            assert_eq!(
                (
                    app.durable.active_channel,
                    app.workspace.focused(),
                    app.workspace_view_generation
                ),
                before
            );
            Arc::make_mut(&mut app.durable).active_channel = Some(home.channel);
            drop(app.sync_channel_pull_request_context());
            let mut frame = mount(&app, Cache::new(), &mut renderer);
            let (_, check) = labelled(&mut frame, &renderer, "Check result");
            let messages = click(&mut frame, &mut renderer, check.center());
            let [cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Check(_, actual)))] =
                messages.as_slice()
            else {
                panic!("native receipt check: {messages:?}");
            };
            assert_eq!(*actual, operation);
            let action = action.clone();
            let _ = frame.into_cache();
            let Some(effect @ Effect::Lookup { .. }) = app.channel_work.view.update(action) else {
                panic!("receipt lookup only");
            };
            assert!(
                matches!(&effect, Effect::Lookup { home: actual, operation: actual_operation, .. } if *actual == home && *actual_operation == operation)
            );
            drop(app.channel_work_effect(effect)); // never poll fixture RPC
            assert!(Arc::ptr_eq(
                &app.channel_work.routes.get(&home).unwrap().proof,
                &original.proof
            ));
            assert_eq!(
                app.channel_work.view.pending_homes().collect::<Vec<_>>(),
                [home]
            );
        }

        #[test]
        fn compact_context_history_controls_keep_exact_owner_cursor_and_workspace() {
            for history in [false, true] {
                let (mut app, home) = pr_fixture();
                let read = app.channel_work.view.refresh().unwrap();
                let Effect::Read { ticket, .. } = read else {
                    panic!()
                };
                let mut snapshot = history_snapshot(home, Some(42));
                snapshot.next_current = Some(50);
                drop(app.update_channel_work(ui::Action::Loaded {
                    ticket,
                    result: Ok(snapshot),
                }));
                let before = (
                    app.durable.active_channel,
                    app.workspace.focused(),
                    app.workspace_view_generation,
                );
                let mut renderer = super::super::super::tests::headless_renderer();
                let mut frame = mount(&app, Cache::new(), &mut renderer);
                let label = if history {
                    "Older pull requests"
                } else {
                    "More pull requests"
                };
                let (_, target) = labelled(&mut frame, &renderer, label);
                assert!(
                    iced::Rectangle::with_size(iced::Size::new(1280.0, 900.0))
                        .contains(target.center())
                );
                let messages = click(&mut frame, &mut renderer, target.center());
                let [cosmic::Action::App(Message::ChannelWork(action))] = messages.as_slice()
                else {
                    panic!("{messages:?}")
                };
                let action = action.clone();
                let cache = frame.into_cache();
                let Some(Effect::Read {
                    ticket,
                    home: actual,
                    page,
                }) = app.channel_work.view.update(action.clone())
                else {
                    panic!("owned page read")
                };
                assert_eq!(actual, home);
                assert_eq!(
                    (page.current_before, page.before),
                    if history {
                        (None, Some(42))
                    } else {
                        (Some(50), None)
                    }
                );
                assert!(
                    app.channel_work.view.update(action).is_none(),
                    "an in-flight page cannot be queued twice"
                );
                let mut next = history_snapshot(home, None);
                next.revision = 2;
                drop(app.update_channel_work(ui::Action::Loaded {
                    ticket,
                    result: Ok(next),
                }));
                assert_eq!(
                    (
                        app.durable.active_channel,
                        app.workspace.focused(),
                        app.workspace_view_generation
                    ),
                    before
                );
                let mut frame = mount(&app, cache, &mut renderer);
                let label = if history {
                    "Recent history"
                } else {
                    "Latest pull requests"
                };
                let (_, target) = labelled(&mut frame, &renderer, label);
                let messages = click(&mut frame, &mut renderer, target.center());
                let [cosmic::Action::App(Message::ChannelWork(action))] = messages.as_slice()
                else {
                    panic!("{messages:?}")
                };
                let action = action.clone();
                let _ = frame.into_cache();
                let Some(Effect::Read {
                    home: actual, page, ..
                }) = app.channel_work.view.update(action)
                else {
                    panic!("return to current PR context")
                };
                assert_eq!(actual, home);
                assert_eq!((page.current_before, page.before), (None, None));
                assert!(
                    app.channel_work.view.pending_homes().next().is_none(),
                    "paging never mutates PR associations"
                );
            }
        }

        #[test]
        fn active_home_rebind_keeps_old_draft_until_dismiss_then_selects_current_context_only() {
            let mut renderer = super::super::super::tests::headless_renderer();
            let (mut app, home, form, _) = fixture(&mut renderer);
            let (other, _, _) = two_channels(&mut app, home);
            let owner = app.channel_work.view.owner().unwrap();
            Arc::make_mut(&mut app.durable).active_channel = Some(other);
            let task = app.sync_channel_pull_request_context();
            assert!(app.channel_work.view.has_confirmation());
            assert_eq!(app.channel_work.view.owner(), Some(owner));
            let mut frame = mount(&app, Cache::new(), &mut renderer);
            apply_focus_task(&mut frame, &renderer, task);
            let (_, keep) = labelled(&mut frame, &renderer, "Keep editing");
            let messages = click(&mut frame, &mut renderer, keep.center());
            let [cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Keep(..)))] =
                messages.as_slice()
            else {
                panic!("{messages:?}")
            };
            let action = action.clone();
            let cache = frame.into_cache();
            drop(app.update_channel_work(action));
            drop(app.sync_channel_pull_request_context());
            assert!(
                !app.channel_work.view.has_confirmation(),
                "Keep must not immediately reopen the dialog"
            );
            assert_eq!(app.channel_work.view.owner(), Some(owner));
            let mut frame = mount(&app, cache, &mut renderer);
            let (_, submit) = labelled(&mut frame, &renderer, "Track PR");
            let nodes = frame.a11y_nodes(iced::mouse::Cursor::Unavailable);
            assert!(
                nodes
                    .root()
                    .iter()
                    .chain(nodes.children())
                    .any(
                        |n| n.node().role() == iced_accessibility::accesskit::Role::Button
                            && n.node().label() == Some("Track PR")
                            && n.node().is_disabled()
                    )
            );
            assert!(
                click(&mut frame, &mut renderer, submit.center()).is_empty(),
                "old A cannot submit while B is current"
            );
            let repository = input(&mut frame, &renderer, "-repo");
            seek(&mut frame, &mut renderer, &repository);
            let (messages, _) = event(
                &mut frame,
                &mut renderer,
                &[key(
                    keyboard::Key::Named(keyboard::key::Named::Escape),
                    keyboard::key::Code::Escape,
                    None,
                )],
                iced::mouse::Cursor::Unavailable,
            );
            let [cosmic::Action::App(Message::ChannelWork(old_escape @ ui::Action::Dismiss(_)))] =
                messages.as_slice()
            else {
                panic!("{messages:?}")
            };
            let old_escape = old_escape.clone();
            let cache = frame.into_cache();
            let task = app.update_channel_work(old_escape.clone());
            assert!(app.channel_work.view.has_confirmation());
            let mut frame = mount(&app, cache, &mut renderer);
            apply_focus_task(&mut frame, &renderer, task);
            let (_, discard) = labelled(&mut frame, &renderer, "Discard draft");
            let messages = click(&mut frame, &mut renderer, discard.center());
            let [cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Discard(..)))] =
                messages.as_slice()
            else {
                panic!("{messages:?}")
            };
            let action = action.clone();
            let _ = frame.into_cache();
            drop(app.update_channel_work(action));
            drop(app.sync_channel_pull_request_context());
            assert_eq!(app.channel_work.view.home().unwrap().channel, other);
            assert_eq!(app.durable.active_channel, Some(other));
            assert_ne!(app.channel_work.view.owner(), Some(owner));
            drop(app.update_channel_work(old_escape));
            drop(app.update_channel_work(ui::Action::Field(
                form,
                ui::Field::Repository,
                "wrong/repo".into(),
            )));
            assert!(
                !app.channel_work.view.has_context_draft(),
                "stale A callbacks cannot reopen or dismiss B"
            );
            assert!(app.channel_work.view.pending_homes().next().is_none());
        }

        #[test]
        fn discarding_an_old_pr_draft_binds_the_latest_channel_instead_of_its_captured_destination()
        {
            let mut renderer = super::super::super::tests::headless_renderer();
            let (mut app, home, _, cache) = fixture(&mut renderer);
            let (other, _, _) = two_channels(&mut app, home);
            let latest = ChannelId::new();
            let mut metadata = app.workspace_model.channel(other).unwrap().clone();
            metadata.id = latest;
            metadata.ordinal += 1;
            metadata.title = "Latest".into();
            app.workspace_model.channels.push(metadata);
            let mut channel = app
                .durable
                .channels
                .iter()
                .find(|c| c.id == other)
                .unwrap()
                .clone();
            channel.id = latest;
            channel.ordinal += 1;
            channel.cards.clear();
            channel.focused = None;
            Arc::make_mut(&mut app.durable).channels.push(channel);
            Arc::make_mut(&mut app.durable).active_channel = Some(other);
            drop(app.sync_channel_pull_request_context());
            assert!(app.channel_work.view.has_confirmation());
            Arc::make_mut(&mut app.durable).active_channel = Some(latest);
            drop(app.sync_channel_pull_request_context());
            let before = (app.workspace.focused(), app.workspace_view_generation);
            let mut frame = mount(&app, cache, &mut renderer);
            let (_, discard) = labelled(&mut frame, &renderer, "Discard draft");
            let messages = click(&mut frame, &mut renderer, discard.center());
            let [cosmic::Action::App(Message::ChannelWork(action @ ui::Action::Discard(..)))] =
                messages.as_slice()
            else {
                panic!("{messages:?}");
            };
            let action = action.clone();
            let _ = frame.into_cache();
            drop(app.update_channel_work(action));
            assert_eq!(
                app.channel_work.view.home(),
                app.selected_channel_work_home()
            );
            assert_eq!(app.channel_work.view.home().unwrap().channel, latest);
            assert_eq!(app.durable.active_channel, Some(latest));
            assert!(!app.channel_work.view.has_context_draft());
            assert!(app.channel_work.view.pending_homes().next().is_none());
            assert_eq!(
                (app.workspace.focused(), app.workspace_view_generation),
                before
            );
        }

        #[test]
        fn background_pr_focus_preserves_a_foreign_native_field_but_explicit_focus_remains_available()
         {
            let mut renderer = super::super::super::tests::headless_renderer();
            let (mut app, _, _, cache) = fixture(&mut renderer);
            let owner = app.channel_work.view.owner().unwrap();
            let mut frame = mount(&app, cache, &mut renderer);
            let repository = input(&mut frame, &renderer, "-repo");
            seek(&mut frame, &mut renderer, &repository);
            let host = input(&mut frame, &renderer, "-host");
            let cache = frame.into_cache();
            let task = app.channel_work_effect_with_focus(
                Effect::Focus {
                    owner,
                    id: host.clone(),
                },
                true,
            );
            let mut frame = mount(&app, cache, &mut renderer);
            apply_focus_task(&mut frame, &renderer, task);
            assert!(same(&active(&mut frame, &renderer), &repository));
            let cache = frame.into_cache();
            let task = app.channel_work_effect(Effect::Focus {
                owner,
                id: host.clone(),
            });
            let mut frame = mount(&app, cache, &mut renderer);
            apply_focus_task(&mut frame, &renderer, task);
            assert!(same(&active(&mut frame, &renderer), &host));
        }
    }
}
