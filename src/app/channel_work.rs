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
}

#[derive(Clone)]
struct Route {
    socket: PathBuf,
    proof: Arc<Mutex<Option<crate::model::HostSessionProof>>>,
}

impl Cagents {
    pub(super) fn open_channel_work(&mut self, channel: ChannelId) -> Task<Message> {
        // The leaf owns a captured destination while asking about a draft.
        // A second navigation must not prune or replace that destination route.
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
        let keep = self
            .channel_work
            .view
            .pending_homes()
            .chain(self.channel_work.view.home())
            .collect::<HashSet<_>>();
        self.channel_work
            .routes
            .retain(|home, _| keep.contains(home));
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
        let effect = self.channel_work.view.open(home, channel.title.clone());
        let open = self.set_context_inspector_open(true);
        let load = effect.map_or_else(Task::none, |effect| self.channel_work_effect(effect));
        Task::batch([open, load])
    }

    pub(super) fn channel_work_subscription(&self) -> Subscription<Message> {
        if self.context_inspector_open
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
        self.channel_work
            .view
            .update(action)
            .map_or_else(Task::none, |effect| self.channel_work_effect(effect))
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
            Effect::OpenPullRequest {
                home: _,
                key: _,
                url,
            } => self.open_clicked_link(url),
            Effect::Close { .. } => self.set_context_inspector_open(false),
            Effect::Focus { owner, id } => {
                if self.channel_work.view.owner() == Some(owner) {
                    iced::widget::operation::focus(id).map(cosmic::Action::App)
                } else {
                    Task::none()
                }
            }
        }
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
}
