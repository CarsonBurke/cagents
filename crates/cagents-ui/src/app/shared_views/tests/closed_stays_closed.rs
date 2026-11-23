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
///
/// The registry may also hold an older copy of the draft that this window
/// published, when a later change to its options was lost to the same
/// trouble: still this window's draft, removed again without a notice. A copy
/// this window never published was changed by another client, which keeps
/// it: this window says so and sends nothing.
#[test]
fn a_draft_closed_during_daemon_trouble_stays_closed_and_new_chats_start() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Held {
        AsClosed,
        OlderPublished,
        ChangedElsewhere,
    }
    for (refusal, held_copy) in [
        (
            crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT,
            Held::AsClosed,
        ),
        (
            "The pane change waited too long for the daemon to be resent",
            Held::AsClosed,
        ),
        (
            crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT,
            Held::OlderPublished,
        ),
        (
            crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT,
            Held::ChangedElsewhere,
        ),
    ] {
        let refusal = format!("{refusal} ({held_copy:?})");
        let refusal = refusal.as_str();
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

        if held_copy == Held::OlderPublished {
            // An options change after the Insert, which the registry never saw.
            let durable = Arc::make_mut(&mut app.durable);
            let local = durable
                .cards
                .iter_mut()
                .find(|state| state.id == card)
                .unwrap();
            let CardKind::Chat(session) = &mut local.kind else {
                panic!("a chat card")
            };
            session
                .launch
                .recipe
                .arguments
                .push("--option-the-registry-never-saw".into());
            app.publish_view_changes();
        }

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
        let mut held = pane.clone();
        if held_copy == Held::ChangedElsewhere {
            let CardKind::Chat(session) = &mut held.kind else {
                panic!("a chat pane")
            };
            session
                .launch
                .recipe
                .arguments
                .push("--option-another-window-chose".into());
        }
        accepted.panes.push(held);
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
        let told: Vec<_> = app
            .status
            .iter()
            .chain(app.notices.recent().map(|entry| &entry.notice))
            .map(|notice| notice.message.to_string())
            .collect();
        let resent = app
            .shared_views
            .pending
            .iter()
            .any(|(operation, _, change)| {
                *operation != close
                    && matches!(change, Change::RemoveSource { before, .. } if before.id == card)
            });
        if held_copy == Held::ChangedElsewhere {
            assert!(!resent, "{refusal}: another window's pane is not removed");
            assert_eq!(
                told,
                vec![super::super::PANE_CLOSED_HERE_KEPT_ELSEWHERE.to_owned()],
                "{refusal}"
            );
        } else {
            assert!(
                told.is_empty(),
                "{refusal}: nothing to tell when the removal is sent again: {told:?}"
            );
            assert!(
                resent,
                "{refusal}: the removal is sent again: {:?}",
                app.shared_views.pending
            );
        }
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

/// A closed card back open in memory - as the view brought it back before
/// this fix - blocks nothing: storage keeps it closed, as the person left it,
/// so the save an update makes of every window before restarting them
/// passes, and a new chat's first message saves and starts. A draft whose
/// first save does fail says why, naming the pane storage refused rather
/// than pointing at a notice.
#[test]
fn a_closed_card_back_in_memory_blocks_neither_an_update_nor_a_new_chat() {
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
    // The window barrier an update's Prepare waits on, through
    // `capture_workspace_save`, before it restarts every window.
    app.session
        .as_ref()
        .unwrap()
        .flush(WAIT)
        .expect("the update's save does not fail over a pane the person closed");
    let stored = app
        .session
        .as_ref()
        .unwrap()
        .operation_store()
        .load_card(card, None)
        .unwrap()
        .unwrap();
    assert!(
        !stored.existence.is_open(),
        "storage keeps the closed card closed"
    );

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
    assert!(message.contains("Close or change"), "{message}");
    // The storage rule is for diagnostics, not the reader.
    assert!(!message.contains("cannot be resurrected"), "{message}");
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
