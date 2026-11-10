//! A snapshot says what the registry holds, not what anyone closed. These
//! regressions drive the whole first send through whole-model updates that
//! say nothing about the new pane, in every shape they arrive in.
use super::*;
use crate::app::workspace_model_sync::ModelUpdate;
use crate::workspace_model::{
    ModelChannel, ModelEdit, WorkspaceModel,
    views::{Change, Pane, Scope, View},
};

#[derive(Clone, Copy, Debug)]
enum Unrelated {
    /// Another agent starts a channel; this view is untouched.
    NewChannel,
    /// Another channel's view moves on; only whole-model sequence advances here.
    OtherViewEdit,
    /// A peer adds a pane to this same channel, so the view is not empty.
    PeerPaneHere,
}

fn runtime(app: &Cagents, card: CardId) -> &Runtime {
    match app.cards.get(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => panic!("expected a chat runtime"),
    }
}

struct World {
    app: Cagents,
    _directory: tempfile::TempDir,
    repository: std::path::PathBuf,
    channel: crate::model::ChannelId,
    other: crate::model::ChannelId,
    card: CardId,
    target: Target,
    published: async_channel::Receiver<ModelEdit>,
}

fn world() -> World {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1280.0);
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().to_path_buf();
    let project = crate::model::ProjectId::new();
    app.sidebar_model
        .projects
        .push(crate::sidebar::SidebarProject::new(
            project,
            crate::repository::RepositoryId::new(),
            "Absence".into(),
            repository.clone(),
            vec![],
            vec![],
        ));
    let channel = crate::model::ChannelId::new();
    let other = crate::model::ChannelId::new();
    for (id, ordinal, cards) in [(channel, 1, true), (other, 2, false)] {
        let _ = cards;
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id,
                project_id: project,
                title: String::new(),
                title_source: Default::default(),
                ordinal,
                shared: true,
                cards: vec![],
                focused: None,
            });
        app.workspace_model.channels.push(ModelChannel {
            id,
            repository: repository.clone(),
            title: String::new(),
            title_source: Default::default(),
            ordinal,
        });
        app.workspace_model.views.push(View {
            revision: 1,
            ..View::empty(id)
        });
    }
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.active_workspace = Some(repository.clone());
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.sequence = 1;
    app.acknowledge_view_edit(Scope::Channel(channel), None);
    app.acknowledge_view_edit(Scope::Channel(other), None);
    let (sender, published) = async_channel::unbounded();
    app.workspace_model_tx = sender;
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("absence"),
    )
    .unwrap();
    app.session = Some(writer);
    drop(app.new_native_chat_with_lifetime(Provider::Claude, Some(repository.clone()), true));
    let card = app.workspace.focused().unwrap();
    let target = runtime(&app, card).target;
    World {
        app,
        _directory: directory,
        repository,
        channel,
        other,
        card,
        target,
        published,
    }
}

/// The next whole-model snapshot, advanced only by somebody else's change.
fn unrelated_snapshot(world: &World, shape: Unrelated, step: u64) -> WorkspaceModel {
    let mut model = world.app.workspace_model.clone();
    model.sequence += 1;
    match shape {
        Unrelated::NewChannel => {
            let id = crate::model::ChannelId::new();
            model.channels.push(ModelChannel {
                id,
                repository: world.repository.clone(),
                title: format!("Other agent {step}"),
                title_source: Default::default(),
                ordinal: 10 + step as u32,
            });
            model.views.push(View {
                revision: 1,
                ..View::empty(id)
            });
        }
        Unrelated::OtherViewEdit => {
            let view = model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(world.other))
                .unwrap();
            view.revision += 1;
        }
        Unrelated::PeerPaneHere => {
            let view = model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(world.channel))
                .unwrap();
            if view.panes.is_empty() {
                view.revision += 1;
                view.panes.push(Pane {
                    id: CardId::new(),
                    title: "Peer".into(),
                    width_fraction: 1.0,
                    kind: CardKind::Chat(
                        fresh_chat_session(
                            Provider::Claude,
                            TerminalId::new(),
                            world.repository.clone(),
                            &Account::system(Provider::Claude),
                            false,
                        )
                        .unwrap(),
                    ),
                    scope: None,
                    user_title: None,
                    pending_terminal_start: false,
                });
            }
        }
    }
    model
}

fn assert_live_card_kept(world: &World, moment: &str, shape: Unrelated) {
    let state = world.app.card_state(world.card).unwrap();
    assert!(
        state.existence.is_open(),
        "{moment} ({shape:?}): a whole-model change closed a live card: {:?}",
        state.existence
    );
    assert!(
        world.app.workspace.card(world.card).is_some(),
        "{moment} ({shape:?}): the live card left the strip"
    );
    assert!(
        !world.app.durable.local_only_cards.contains(&world.card),
        "{moment} ({shape:?}): absence demoted a publishable card to this window"
    );
    assert!(
        world
            .app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == world.channel)
            .unwrap()
            .cards
            .contains(&world.card),
        "{moment} ({shape:?}): the live card left its channel"
    );
}

#[test]
fn unrelated_model_changes_never_close_a_first_send_that_is_not_published_yet() {
    for shape in [
        Unrelated::NewChannel,
        Unrelated::OtherViewEdit,
        Unrelated::PeerPaneHere,
    ] {
        let mut world = world();
        let (card, target) = (world.card, world.target);
        drop(world.app.handle_chat_ui(Action::Composer(
            target,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                "one original prompt".into(),
            ))),
        )));
        // The draft has work but no publication yet: nothing has told the
        // registry about it while the reader is still typing.
        let while_typing = unrelated_snapshot(&world, shape, 0);
        drop(
            world
                .app
                .apply_workspace_model_update(ModelUpdate::Read(while_typing)),
        );
        assert_live_card_kept(&world, "while typing the first message", shape);
        let save = world
            .app
            .handle_chat_ui(Action::Submit(target, PromptMode::Send));
        let during_save = unrelated_snapshot(&world, shape, 1);
        drop(
            world
                .app
                .apply_workspace_model_update(ModelUpdate::Read(during_save)),
        );
        assert_live_card_kept(&world, "during the first save", shape);
        let (target, attempt, result) = super::first_start_checkpoint_tests::checkpoint(save);
        assert!(result.is_ok());
        drop(
            world
                .app
                .handle_chat_ui(Action::DraftStartCheckpointed(target, attempt, result)),
        );
        // The provider handshake is in flight, its publication unanswered.
        assert!(world.app.chat_start_pending(card));
        let during_handshake = unrelated_snapshot(&world, shape, 2);
        drop(
            world
                .app
                .apply_workspace_model_update(ModelUpdate::Read(during_handshake)),
        );
        assert_live_card_kept(&world, "during the handshake", shape);

        let ready = super::ready_message_for_pending_switch(
            &world.app,
            card,
            Snapshot {
                ready: true,
                session_id: Some("first-send-native-thread".into()),
                ..Snapshot::default()
            },
        );
        drop(world.app.update(ready));
        let target = runtime(&world.app, card).target;
        let handle = runtime(&world.app, card).handle().unwrap().clone();
        let command = runtime(&world.app, card).pending.as_ref().unwrap().id;
        super::show_chat_snapshot_for_test(
            &mut world.app,
            card,
            Snapshot {
                ready: true,
                busy: true,
                session_id: Some("first-send-native-thread".into()),
                items: vec![Arc::new(chat::Item {
                    id: "original-user-item".into(),
                    role: chat::Role::User,
                    content: chat::ItemContent::Text {
                        text: "one original prompt".into(),
                        thinking: false,
                        phase: Default::default(),
                    },
                    started_at: None,
                    at: None,
                })],
                ..Snapshot::default()
            },
        );
        drop(
            world
                .app
                .handle_chat_ui(Action::CommandFinished(target, command, Ok(()))),
        );
        drop(
            world
                .app
                .update(Message::ChatUi(Action::Wake(target, Wake::Changed))),
        );
        // The actor is working and nothing local protects the card any more.
        assert!(!world.app.shared_chat_has_local_work(card));
        for step in 3..6 {
            let after_acceptance = unrelated_snapshot(&world, shape, step);
            drop(
                world
                    .app
                    .apply_workspace_model_update(ModelUpdate::Read(after_acceptance)),
            );
            assert_live_card_kept(&world, "with the actor running", shape);
        }
        assert!(Arc::ptr_eq(
            runtime(&world.app, card).handle().unwrap(),
            &handle
        ));

        // Exactly one creation intent reached the registry for this pane.
        let edits = std::iter::from_fn(|| world.published.try_recv().ok()).collect::<Vec<_>>();
        let inserts = edits
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::ChangeView {
                    scope,
                    operation,
                    change: Change::Insert { pane, .. },
                    ..
                } if pane.id == card => Some((scope.clone(), *operation, pane.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(inserts.len(), 1, "{shape:?}: {inserts:?}");
        assert_eq!(inserts[0].0, Scope::Channel(world.channel), "{shape:?}");

        // Once the delayed Insert lands, later reads still hold the same card.
        let mut acknowledged = world.app.workspace_model.clone();
        acknowledged.sequence += 1;
        let view = acknowledged
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(world.channel))
            .unwrap();
        view.revision += 1;
        view.panes.push(inserts[0].2.clone());
        drop(
            world
                .app
                .apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: Scope::Channel(world.channel),
                    operation: Some(inserts[0].1),
                    model: acknowledged,
                }),
        );
        let settled = unrelated_snapshot(&world, shape, 7);
        drop(
            world
                .app
                .apply_workspace_model_update(ModelUpdate::Read(settled)),
        );
        assert_live_card_kept(&world, "after acknowledgement", shape);
        assert!(Arc::ptr_eq(
            runtime(&world.app, card).handle().unwrap(),
            &handle
        ));
    }
}

/// Runs a first send to an accepted prompt, publication still unanswered.
fn accepted_first_send(world: &mut World) -> Arc<ClientHandle> {
    let (card, target) = (world.card, world.target);
    drop(world.app.handle_chat_ui(Action::Composer(
        target,
        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
            "one original prompt".into(),
        ))),
    )));
    let save = world
        .app
        .handle_chat_ui(Action::Submit(target, PromptMode::Send));
    let (target, attempt, result) = super::first_start_checkpoint_tests::checkpoint(save);
    assert!(result.is_ok());
    drop(
        world
            .app
            .handle_chat_ui(Action::DraftStartCheckpointed(target, attempt, result)),
    );
    let ready = super::ready_message_for_pending_switch(
        &world.app,
        card,
        Snapshot {
            ready: true,
            session_id: Some("first-send-native-thread".into()),
            ..Snapshot::default()
        },
    );
    drop(world.app.update(ready));
    let target = runtime(&world.app, card).target;
    let handle = runtime(&world.app, card).handle().unwrap().clone();
    let command = runtime(&world.app, card).pending.as_ref().unwrap().id;
    super::show_chat_snapshot_for_test(
        &mut world.app,
        card,
        Snapshot {
            ready: true,
            busy: true,
            session_id: Some("first-send-native-thread".into()),
            items: vec![Arc::new(chat::Item {
                id: "original-user-item".into(),
                role: chat::Role::User,
                content: chat::ItemContent::Text {
                    text: "one original prompt".into(),
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: None,
                at: None,
            })],
            ..Snapshot::default()
        },
    );
    drop(
        world
            .app
            .handle_chat_ui(Action::CommandFinished(target, command, Ok(()))),
    );
    drop(
        world
            .app
            .update(Message::ChatUi(Action::Wake(target, Wake::Changed))),
    );
    assert!(!world.app.shared_chat_has_local_work(card));
    handle
}

fn first_insert(world: &World) -> (uuid::Uuid, Pane) {
    std::iter::from_fn(|| world.published.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                change: Change::Insert { pane, .. },
                ..
            } if pane.id == world.card => Some((operation, pane)),
            _ => None,
        })
        .expect("the first send publishes its pane")
}

fn acknowledge(world: &mut World, operation: uuid::Uuid, pane: Option<Pane>) {
    let mut model = world.app.workspace_model.clone();
    model.sequence += 1;
    if let Some(pane) = pane {
        let view = model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(world.channel))
            .unwrap();
        view.revision += 1;
        view.panes.push(pane);
    }
    drop(
        world
            .app
            .apply_workspace_model_update(ModelUpdate::ViewApplied {
                scope: Scope::Channel(world.channel),
                operation: Some(operation),
                model,
            }),
    );
}

/// The registry can answer an Insert without listing the pane, as it does when
/// the scope has gone or another pane owns the resource. That answer is still
/// not somebody closing a running conversation.
#[test]
fn an_acknowledged_insert_the_view_does_not_list_keeps_the_running_card_until_retired() {
    let mut world = world();
    let handle = accepted_first_send(&mut world);
    let (operation, _) = first_insert(&world);
    acknowledge(&mut world, operation, None);
    for step in 0..3 {
        let next = unrelated_snapshot(&world, Unrelated::OtherViewEdit, step);
        drop(
            world
                .app
                .apply_workspace_model_update(ModelUpdate::Read(next)),
        );
        let state = world.app.card_state(world.card).unwrap();
        assert!(state.existence.is_open(), "{:?}", state.existence);
        assert!(world.app.durable.local_only_cards.contains(&world.card));
        assert!(Arc::ptr_eq(
            runtime(&world.app, world.card).handle().unwrap(),
            &handle
        ));
    }
    let notice = world
        .app
        .status
        .as_ref()
        .map(|notice| notice.message.as_str());
    assert!(
        notice.is_some_and(|text| text.contains("is open only in this window")),
        "the reader is told where the pane now lives: {notice:?}"
    );
    // A recorded removal is what closes it, and it does.
    let mut removed = world.app.workspace_model.clone();
    removed.sequence += 1;
    removed
        .views
        .iter_mut()
        .find(|view| view.scope == Scope::Channel(world.channel))
        .unwrap()
        .apply(&Change::Remove { id: world.card })
        .unwrap();
    drop(
        world
            .app
            .apply_workspace_model_update(ModelUpdate::Read(removed)),
    );
    assert!(
        !world
            .app
            .card_state(world.card)
            .unwrap()
            .existence
            .is_open()
    );
}

#[test]
fn a_peer_deleting_a_channel_only_closes_conversations_it_could_have_seen() {
    for observed in [false, true] {
        let mut world = world();
        accepted_first_send(&mut world);
        let (operation, pane) = first_insert(&world);
        if observed {
            acknowledge(&mut world, operation, Some(pane));
        }
        let sequence = world.app.workspace_model.sequence + 1;
        let channel = world.channel;
        drop(world.app.apply_workspace_model_update(ModelUpdate::Edited {
            sequence,
            edit: ModelEdit::DeleteChannel { id: channel },
        }));
        let open = world
            .app
            .card_state(world.card)
            .unwrap()
            .existence
            .is_open();
        assert_eq!(open, !observed, "observed: {observed}");
        assert_eq!(
            world.app.durable.local_only_cards.contains(&world.card),
            !observed
        );
    }
}

#[test]
fn an_empty_channel_is_not_retired_under_a_conversation_it_has_not_published() {
    use crate::workspace_model::ChannelIncarnation;
    for used in [false, true] {
        let mut world = world();
        let home = ChannelIncarnation {
            registry: world.app.workspace_model.identity,
            channel: world.channel,
            creation_ordinal: 1,
        };
        if used {
            accepted_first_send(&mut world);
        }
        while world.published.try_recv().is_ok() {}
        world.app.channel_ui.retire_candidates.insert(home);
        world.app.retire_settled_empty_channels();
        let retired = std::iter::from_fn(|| world.published.try_recv().ok())
            .any(|edit| matches!(edit, ModelEdit::RetireEmptyChannel { .. }));
        assert_eq!(retired, !used, "used: {used}");
        assert!(world.app.channel_ui.retire_candidates.is_empty());
    }
}

/// A worktree strip has no channel; the same absence must mean the same thing,
/// including when the reader moves to another worktree mid-send.
#[test]
fn unrelated_model_changes_never_close_a_first_send_in_a_worktree_strip() {
    let mut world = world();
    let repository = world.repository.clone();
    let scope = Scope::Worktree {
        repository: repository.clone(),
        path: repository.clone(),
    };
    let channel = world.channel;
    // Rebuild the scene with the draft in the strip instead of the channel.
    world.app.cards.remove(&world.card);
    drop(world.app.workspace.request_close(world.card));
    drop(world.app.workspace.commit_close(world.card));
    {
        let durable = Arc::make_mut(&mut world.app.durable);
        durable.cards.retain(|card| card.id != world.card);
        durable.active_channel = None;
        durable
            .channels
            .iter_mut()
            .find(|entry| entry.id == channel)
            .unwrap()
            .cards
            .clear();
    }
    world.app.workspace_model.views.push(View {
        revision: 1,
        ..View::empty(scope.clone())
    });
    world.app.acknowledge_view_edit(scope.clone(), None);
    drop(
        world
            .app
            .new_native_chat_with_lifetime(Provider::Claude, Some(repository.clone()), true),
    );
    world.card = world.app.workspace.focused().unwrap();
    world.target = runtime(&world.app, world.card).target;
    assert_eq!(world.app.draft_start_scope(world.card), Some(scope.clone()));

    let handle = accepted_first_send(&mut world);
    world.app.active_workspace = Some(repository.join("elsewhere"));
    for step in 0..3 {
        let next = unrelated_snapshot(&world, Unrelated::OtherViewEdit, step);
        drop(
            world
                .app
                .apply_workspace_model_update(ModelUpdate::Read(next)),
        );
        let state = world.app.card_state(world.card).unwrap();
        assert!(state.existence.is_open(), "{:?}", state.existence);
        assert!(!world.app.durable.local_only_cards.contains(&world.card));
        assert!(Arc::ptr_eq(
            runtime(&world.app, world.card).handle().unwrap(),
            &handle
        ));
    }
    let edits = std::iter::from_fn(|| world.published.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        edits
            .iter()
            .filter(|edit| matches!(edit,
                ModelEdit::ChangeView { scope: published, change: Change::Insert { pane, .. }, .. }
                    if pane.id == world.card && *published == scope))
            .count(),
        1
    );
}
