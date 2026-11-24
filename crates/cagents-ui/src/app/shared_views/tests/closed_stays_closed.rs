//! A card this window closed stays closed under its identity, whatever a
//! shared view still lists, and one pane storage refuses never stops another
//! chat from starting (#2093, #2444).
use super::*;
use crate::app::chat_ui::Action;
use crate::app::workspace_model_sync::ModelUpdate;

const WAIT: Duration = Duration::from_secs(5);

/// Opens another chat, composes it so it is saved, and has storage refuse
/// it: what storage holds for it is no longer the identity the window
/// offers. A window barrier now fails; only that chat's own save is refused.
fn refuse_another_chat(app: &mut Cagents) -> CardId {
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    let other = app.workspace.focused().expect("another chat opens");
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&other) else {
        panic!("native draft")
    };
    let target = runtime.target;
    compose_default_chat(app, target);
    drop(app.reconcile_shared_views());
    app.persist();
    app.session.as_ref().unwrap().flush(WAIT).unwrap();
    let CardKind::Chat(saved) = app.card_state(other).unwrap().kind.clone() else {
        panic!("a chat card")
    };
    let store = app.session.as_ref().unwrap().operation_store();
    let mut foreign = saved.clone();
    foreign.id = TerminalId::new();
    store
        .replace_chat_identity(other, saved.id, &foreign)
        .unwrap();
    let mut refused = saved.clone();
    refused.id = TerminalId::new();
    let snapshot = app.persistent_workspace();
    app.session
        .as_ref()
        .unwrap()
        .replace_chat_identity(other, saved.id, refused, snapshot);
    assert!(
        app.session.as_ref().unwrap().flush(WAIT).is_err(),
        "storage refuses the other chat"
    );
    other
}

/// The daemon trouble behind #2444: a composed draft is closed while its
/// Insert is still unanswered. The Insert lands, but the Close does not -
/// refused outright, or given up on as the daemon stopped answering. The
/// draft stays closed, the person is told other windows may still show it,
/// and a new chat's first message is saved and started. Before, the view
/// reopened the closed card, storage refused it, and every first message
/// failed on that refusal.
///
/// Whatever copy the registry holds - the one closed, an older one this
/// window published, or one another window changed - the identity this
/// window closed stays closed here.
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

        // A draft is named for its channel while it is open there. Closed,
        // it is in no channel, yet the notice names it as its tab did.
        Arc::make_mut(&mut app.durable)
            .channels
            .iter_mut()
            .find(|candidate| candidate.id == channel)
            .unwrap()
            .title = "Release notes".into();
        let shown = app.pane_notice_title(card).expect("the draft is named");
        assert_eq!(shown, "Release notes", "{refusal}");

        // Closed while its Insert is unanswered.
        drop(app.begin_chat_close(card));
        app.publish_view_changes();
        assert!(
            !app.card_state(card).unwrap().existence.is_open(),
            "{refusal}: the draft closes at once: {:?}",
            app.status.as_ref().map(|notice| notice.message.to_string()),
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
        assert!(
            !resent,
            "{refusal}: nothing is sent behind the person's back"
        );
        assert_eq!(
            told,
            vec![super::super::pane_closed_here_kept_elsewhere(Some(&shown))],
            "{refusal}"
        );
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(WAIT)
            .unwrap_or_else(|error| panic!("{refusal}: nothing is refused: {error}"));

        // A new chat's first message is saved, so it starts, even while
        // storage refuses some other pane.
        let other = refuse_another_chat(&mut app);
        drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
        let fresh = app.workspace.focused().expect("a new chat opens");
        assert_ne!(fresh, card);
        assert_ne!(fresh, other);
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
        assert!(
            app.session.as_ref().unwrap().flush(WAIT).is_err(),
            "{refusal}: the other chat is still refused"
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

    let other = refuse_another_chat(&mut app);
    drop(app.add_default_pane(PathBuf::from("/repo"), settings::DefaultPane::NativeChat));
    let fresh = app
        .workspace
        .order()
        .find(|id| {
            *id != card
                && *id != other
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
        "neither the closed pane back in memory nor another pane's refusal fails this draft's save: \
         {completion:?}"
    );
    assert!(
        app.session.as_ref().unwrap().flush(WAIT).is_err(),
        "the other chat is still refused"
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
    // Storage holding another identity for the pane is not something an
    // edit of the draft repairs, so the reader is not told to try one.
    let identity = app.draft_start_save_failure(
        fresh,
        &crate::session::PaneSaveRefusal::Identity {
            category: crate::session::SaveFailureCategory::InvalidData,
            rule: crate::diagnostics::Check::new("a rule"),
        },
    );
    assert!(identity.contains("Copy it into a new conversation"), "{identity}");
    assert!(!identity.contains("Change the draft"), "{identity}");
    let window = app.draft_start_save_failure(
        fresh,
        &crate::session::PaneSaveRefusal::Refused {
            pane: None,
            category: crate::session::SaveFailureCategory::DiskFull,
            rule: crate::diagnostics::Check::new("a rule"),
        },
    );
    assert!(window.contains("window's saved state"), "{window}");
    assert!(window.contains("Free disk space"), "{window}");
}
