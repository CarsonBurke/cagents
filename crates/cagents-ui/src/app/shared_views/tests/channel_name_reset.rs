use super::*;
use crate::app::chrome_ui::Action;
use crate::app::workspace_model_sync::{EdgeChanges, ModelUpdate};
use crate::model::ChannelTitleSource;
use crate::workspace_model::delta::{EditEffects, EditReceipt, ViewChanges};

fn rename(app: &mut Cagents, channel: ChannelId, title: &str) {
    drop(app.update_chrome(Action::RenameChannel(channel)));
    drop(app.update_chrome(Action::RenameChanged(title.into())));
    drop(app.update_chrome(Action::RenameSubmit));
}

#[test]
fn peer_name_reset_after_last_pane_moves_allows_retirement_on_leave() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let project = app.durable.channels[0].project_id;
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        "/repo/.git".into(),
        Vec::new(),
        Vec::new(),
    ));
    app.channel_ui.sweep = crate::app::channel_ui::Sweep::Done;
    let identity = app.workspace_model.identity;
    let (tx, requests) = test_model_channel();
    app.workspace_model_tx = tx;

    rename(&mut app, source, "Named here");
    let edit = requests.try_recv().unwrap();
    assert!(matches!(&edit, ModelEdit::RetitleChannel { id, title }
        if *id == source && title == "Named here"));
    let mut named = app.workspace_model.channel(source).unwrap().clone();
    named.title = "Named here".into();
    named.title_source = ChannelTitleSource::Manual;
    drop(app.apply_workspace_model_update(ModelUpdate::Committed {
        receipt: EditReceipt {
            identity,
            sequence: 10,
            effects: EditEffects {
                channels: vec![named.clone()],
                ..Default::default()
            },
        },
        edit,
        views: ViewChanges::default(),
    }));

    drop(app.move_pane_to_channel(pane.id, destination));
    let edit = requests.try_recv().unwrap();
    assert!(
        matches!(&edit, ModelEdit::ChangeView { change: Change::Transfer { id, .. }, .. }
        if *id == pane.id)
    );
    let mut moved = app.workspace_model.views.clone();
    views::transfer(
        &mut moved,
        &Scope::Channel(source),
        pane.id,
        &Scope::Channel(destination),
        None,
    )
    .unwrap();
    // The daemon assigns fresh revisions when it commits both changed views.
    for (index, view) in moved.iter_mut().enumerate() {
        view.revision = 18 + index as u64;
    }
    drop(app.apply_workspace_model_update(ModelUpdate::Committed {
        receipt: EditReceipt {
            identity,
            sequence: 11,
            effects: EditEffects::default(),
        },
        edit,
        views: ViewChanges {
            sequence: 11,
            views: moved,
            ..Default::default()
        },
    }));
    assert_eq!(app.durable.active_channel, Some(source));
    assert!(
        app.durable
            .channels
            .iter()
            .find(|c| c.id == source)
            .unwrap()
            .cards
            .is_empty()
    );
    assert!(!app.pane_transfer_pending(pane.id));
    assert!(
        !std::iter::from_fn(|| requests.try_recv().ok())
            .any(|edit| matches!(edit, ModelEdit::RetireEmptyChannel { .. }))
    );

    // The second window resets the name from the sidebar without entering S.
    let mut peer = crate::app::tests::context_inspector_test_app(false);
    Arc::make_mut(&mut peer.durable).channels = app.durable.channels.clone();
    peer.workspace_model = app.workspace_model.clone();
    let (tx, peer_requests) = test_model_channel();
    peer.workspace_model_tx = tx;
    rename(&mut peer, source, "");
    assert_eq!(peer.durable.active_channel, None);
    let edit = peer_requests.try_recv().unwrap();
    assert!(matches!(&edit, ModelEdit::RetitleChannel { id, title }
        if *id == source && title.is_empty()));
    named.title.clear();
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence: 12,
        edit,
        changes: EdgeChanges {
            identity: Some(identity),
            effects: EditEffects {
                channels: vec![named],
                ..Default::default()
            },
            ..Default::default()
        },
    }));
    assert!(
        app.durable
            .channels
            .iter()
            .find(|c| c.id == source)
            .unwrap()
            .title
            .is_empty()
    );
    drop(app.navigate_to_channel(destination));
    assert_eq!(app.durable.active_channel, Some(destination));
    assert!(
        std::iter::from_fn(|| requests.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::RetireEmptyChannel { home, .. } if home.channel == source
        )),
        "leaving the empty channel after its name was cleared should retire it"
    );
}

fn commit_name(app: &mut Cagents, edit: ModelEdit, identity: uuid::Uuid, sequence: u64) {
    let ModelEdit::RetitleChannel { id, title } = &edit else {
        panic!("a rename publication");
    };
    let mut channel = app.workspace_model.channel(*id).unwrap().clone();
    channel.title = title.clone();
    channel.title_source = ChannelTitleSource::Manual;
    drop(app.apply_workspace_model_update(ModelUpdate::Committed {
        receipt: EditReceipt {
            identity,
            sequence,
            effects: EditEffects {
                channels: vec![channel],
                ..Default::default()
            },
        },
        edit,
        views: ViewChanges::default(),
    }));
}

fn observe_name_reset(app: &mut Cagents, id: ChannelId, sequence: u64) {
    let mut channel = app.workspace_model.channel(id).unwrap().clone();
    channel.title.clear();
    channel.title_source = ChannelTitleSource::Manual;
    drop(app.apply_workspace_model_update(ModelUpdate::Changed {
        sequence,
        edit: ModelEdit::RetitleChannel {
            id,
            title: String::new(),
        },
        changes: EdgeChanges {
            identity: Some(app.workspace_model.identity),
            effects: EditEffects {
                channels: vec![channel],
                ..Default::default()
            },
            ..Default::default()
        },
    }));
}

#[test]
fn overlapping_renames_keep_protection_until_all_publications_settle() {
    for first_receipt_from_old_registry in [false, true] {
        let (mut app, source, _, _) = channel_move_fixture();
        let identity = app.workspace_model.identity;
        let mut older = app.workspace_model.clone();
        older.sequence = 9;
        older
            .channels
            .iter_mut()
            .find(|c| c.id == source)
            .unwrap()
            .title
            .clear();
        let (tx, requests) = test_model_channel();
        app.workspace_model_tx = tx;
        // Repeating the first title ensures title equality cannot identify
        // which publication the publisher has acknowledged.
        for title in ["First", "", "First"] {
            rename(&mut app, source, title);
        }
        let edits = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(edits.len(), 3);
        let first_identity = if first_receipt_from_old_registry {
            uuid::Uuid::new_v4()
        } else {
            identity
        };
        commit_name(&mut app, edits[0].clone(), first_identity, 10);
        observe_name_reset(&mut app, source, 11);
        assert!(
            app.channel_is_named(source),
            "the last rename is still pending"
        );
        commit_name(&mut app, edits[1].clone(), identity, 12);
        assert!(
            app.channel_is_named(source),
            "the reset receipt precedes the last rename"
        );
        commit_name(&mut app, edits[2].clone(), identity, 13);
        assert!(
            app.channel_is_named(source),
            "the confirmed manual title protects the channel"
        );
        drop(app.apply_workspace_model_update(ModelUpdate::Read(older)));
        assert_eq!(app.workspace_model.channel(source).unwrap().title, "First");
        assert!(
            app.channel_is_named(source),
            "an older snapshot cannot clear the confirmed name"
        );
        observe_name_reset(&mut app, source, 14);
        assert!(
            !app.channel_is_named(source),
            "a later peer reset ends name protection"
        );
    }
}
