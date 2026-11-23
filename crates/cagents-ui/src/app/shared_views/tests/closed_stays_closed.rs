//! A card this window closed stays closed under its identity, whatever a
//! shared view still lists, and one pane storage refuses never stops another
//! chat from starting (#2093, #2444).
use super::*;
use crate::app::chat_ui::Action;
use crate::app::workspace_model_sync::ModelUpdate;

const WAIT: Duration = Duration::from_secs(5);

/// The daemon trouble behind #2444: a composed draft is closed while its
/// Insert is still unanswered. The Insert lands, but the Close does not -
/// refused outright, or given up on as the daemon stopped answering. The
/// draft stays closed, its removal is sent again, and a new chat's first
/// message is saved and started. Before, the view reopened the closed card,
/// storage refused it, and every first message failed on that refusal.
#[test]
fn a_draft_closed_during_daemon_trouble_stays_closed_and_new_chats_start() {
    for refusal in [
        crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT,
        "The pane change waited too long for the daemon to be resent",
    ] {
        let (mut app, channel, card, target, receiver) = unpublished_default_chat();
        let scope = Scope::Channel(channel);
        let storage = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            storage.path().join("state.sqlite3"),
            crate::model::BootId::new("closed-stays-closed"),
        )
        .unwrap();
        app.session = Some(writer);
        compose_default_chat(&mut app, target);
        drop(app.reconcile_shared_views());
        let (insert, pane) = std::iter::from_fn(|| receiver.try_recv().ok())
            .find_map(|edit| match edit {
                ModelEdit::ChangeView {
                    operation,
                    change: Change::Insert { pane, .. },
                    ..
                } if pane.id == card => Some((operation, pane)),
                _ => None,
            })
            .expect("the composed draft is published");

        // Closed while its Insert is unanswered.
        drop(app.begin_chat_close(card));
        app.publish_view_changes();
        assert!(
            !app.card_state(card).unwrap().existence.is_open(),
            "{refusal}: the draft closes at once"
        );
        let close = app
            .shared_views
            .pending
            .iter()
            .find_map(|(operation, _, change)| {
                matches!(change, Change::RemoveSource { before, .. } if before.id == card)
                    .then_some(*operation)
            })
            .unwrap_or_else(|| {
                panic!(
                    "{refusal}: the Close follows the Insert: {:?}",
                    app.shared_views.pending
                )
            });
        app.session.as_ref().unwrap().flush(WAIT).unwrap();

        // The Insert lands; the Close does not.
        let mut accepted = View::empty(channel);
        accepted.revision = 1;
        accepted.panes.push(pane.clone());
        app.workspace_model.views = vec![accepted];
        app.acknowledge_view_edit(scope.clone(), Some(insert));
        drop(app.apply_workspace_model_update(ModelUpdate::ViewRefused {
            scope: scope.clone(),
            operation: close,
            error: refusal.into(),
        }));
        for _ in 0..2 {
            drop(app.reconcile_shared_views());
        }
        assert!(
            !app.card_state(card).unwrap().existence.is_open(),
            "{refusal}: the view does not reopen the closed draft"
        );
        assert!(!app.workspace.order().any(|id| id == card), "{refusal}");
        assert!(!app.cards.contains_key(&card), "{refusal}");
        assert!(
            app.shared_views.pending.iter().any(|(operation, _, change)| {
                *operation != close
                    && matches!(change, Change::RemoveSource { before, .. } if before.id == card)
            }),
            "{refusal}: the removal is sent again: {:?}",
            app.shared_views.pending
        );
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(WAIT)
            .unwrap_or_else(|error| panic!("{refusal}: nothing is refused: {error}"));

        // A new chat's first message is saved, so it starts.
        drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
        let fresh = app.workspace.focused().expect("a new chat opens");
        assert_ne!(fresh, card);
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&fresh) else {
            panic!("native draft")
        };
        let fresh_target = runtime.target;
        compose_default_chat(&mut app, fresh_target);
        drop(app.reconcile_shared_views());
        let save = app.handle_chat_ui(Action::Submit(fresh_target, crate::chat::PromptMode::Send));
        let completion = first_start_save_completion(save);
        assert!(
            matches!(&completion, Action::DraftStartCheckpointed(_, _, Ok(()))),
            "{refusal}: {completion:?}"
        );
        drop(app.handle_chat_ui(completion));
        assert!(app.chat_start_pending(fresh), "{refusal}");
    }
}

/// One pane storage refuses - the closed card brought back, as the view did
/// before this fix - fails nothing but its own saves. A new chat's first
/// message still saves and starts, and a draft that does fail says why,
/// naming the pane storage refused rather than pointing at a notice.
#[test]
fn a_pane_storage_refuses_does_not_stop_another_chat_starting() {
    let (mut app, channel, card, target, _receiver) = unpublished_default_chat();
    let storage = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        storage.path().join("state.sqlite3"),
        crate::model::BootId::new("stuck-pane"),
    )
    .unwrap();
    app.session = Some(writer);
    compose_default_chat(&mut app, target);
    drop(app.reconcile_shared_views());
    app.persist();
    app.session.as_ref().unwrap().flush(WAIT).unwrap();
    drop(app.begin_chat_close(card));
    app.publish_view_changes();
    app.persist();
    app.session.as_ref().unwrap().flush(WAIT).unwrap();
    // The stuck state #2444 found: a tombstoned card open again in memory.
    let durable = Arc::make_mut(&mut app.durable);
    let stuck = durable
        .cards
        .iter_mut()
        .find(|state| state.id == card)
        .unwrap();
    stuck.existence = Existence::Open;
    stuck.title = "Stuck draft".into();
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(WAIT)
        .expect_err("storage refuses the closed card");

    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    let fresh = app
        .workspace
        .order()
        .find(|id| {
            *id != card
                && app
                    .card_state(*id)
                    .is_some_and(|state| state.existence.is_open())
        })
        .expect("a new chat opens");
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&fresh) else {
        panic!("native draft")
    };
    let fresh_target = runtime.target;
    compose_default_chat(&mut app, fresh_target);
    drop(app.reconcile_shared_views());
    let save = app.handle_chat_ui(Action::Submit(fresh_target, crate::chat::PromptMode::Send));
    let completion = first_start_save_completion(save);
    assert!(
        matches!(&completion, Action::DraftStartCheckpointed(_, _, Ok(()))),
        "another pane's refusal must not fail this draft's save: {completion:?}"
    );
    let _ = channel;

    let refused = crate::session::PaneSaveRefusal::Refused {
        pane: Some(card),
        category: crate::session::SaveFailureCategory::InvalidData,
        rule: crate::diagnostics::Check::new("tombstoned card … cannot be resurrected"),
    };
    let message = app.draft_start_save_failure(fresh, &refused);
    assert!(message.contains("\u{201c}Stuck draft\u{201d}"), "{message}");
    assert!(message.contains("cannot be resurrected"), "{message}");
    assert!(!message.contains("save notice"), "{message}");
    let own = crate::session::PaneSaveRefusal::Refused {
        pane: Some(fresh),
        category: crate::session::SaveFailureCategory::InvalidData,
        rule: crate::diagnostics::Check::new("a rule"),
    };
    assert!(
        app.draft_start_save_failure(fresh, &own)
            .contains("this pane"),
        "a draft's own refusal speaks of this pane"
    );
}
