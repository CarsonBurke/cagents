use super::*;

fn runtime(app: &Cagents, card: CardId) -> &Runtime {
    match app.cards.get(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => panic!("expected a chat draft"),
    }
}

fn fixture() -> (Cagents, tempfile::TempDir, CardId, Target) {
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("first-start-checkpoint"),
    )
    .unwrap();
    app.session = Some(writer);
    let source = fresh_chat_session(
        Provider::Codex,
        TerminalId::new(),
        directory.path().to_owned(),
        &Account::system(Provider::Codex),
        false,
    )
    .unwrap();
    drop(app.insert_chat_card(source, None, None));
    let card = app.workspace.focused().unwrap();
    let target = runtime(&app, card).target;
    let draft = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    draft.replace_draft(text_editor::Content::with_text(
        "First prompt remains local",
    ));
    draft.attachments.push(Attachment {
        path: directory.path().join("first-image.png"),
        reference: Some(chat::ImageReference {
            id: uuid::Uuid::new_v4(),
            number: 1,
            device: uuid::Uuid::new_v4(),
            name: "first-image.png".into(),
            sha256: "a".repeat(64),
            extension: "png".into(),
        }),
    });
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    (app, directory, card, target)
}

/// Drive only the save Task. Provider-start Tasks returned by the reducer
/// below are deliberately not polled against a user's daemon or executable.
pub(super) fn checkpoint(task: Task<Message>) -> (Target, uuid::Uuid, Result<(), String>) {
    use cosmic::iced::futures::StreamExt;
    // Some existing UI parents run inside Tokio. An owned joined thread
    // keeps this save-only executor independent of that caller's runtime.
    std::thread::spawn(move || {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut stream = cosmic::iced::runtime::task::into_stream(task).expect("save Task");
            while let Some(action) = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("real writer must settle")
            {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                    Message::ChatUi(Action::DraftStartCheckpointed(target, attempt, result)),
                )) = action
                {
                    return (target, attempt, result);
                }
            }
            panic!("save Task did not return its checkpoint result")
        })
    })
    .join()
    .expect("save-only fixture executor must finish")
}

fn stored(app: &Cagents, card: CardId) -> chat::Session {
    let card = app
        .session
        .as_ref()
        .unwrap()
        .operation_store()
        .load_card(card, None)
        .unwrap()
        .unwrap();
    match card.kind {
        CardKind::Chat(session) => session,
        _ => panic!("stored card must be chat"),
    }
}

fn no_starter(app: &Cagents, card: CardId) {
    assert!(!matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::Starting(_))
    ));
    assert!(runtime(app, card).handle().is_none());
    assert!(runtime(app, card).queued.is_none());
    assert!(
        app.chat_ui.abandoned.is_empty(),
        "a save is not a provider actor"
    );
}

#[test]
fn first_start_waits_for_real_identity_and_prompt_checkpoint_before_publishing_or_starting() {
    let (mut app, _directory, card, target) = fixture();
    let source = runtime(&app, card).session.clone();
    let images = runtime(&app, card).attachments.clone();
    let channel = crate::model::ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(crate::model::ProjectChannel {
            id: channel,
            project_id: crate::model::ProjectId::new(),
            title: "Checkpoint scope".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
    app.workspace_model.identity = uuid::Uuid::new_v4();
    let (published, requests) = async_channel::unbounded();
    app.workspace_model_tx = published;
    let task = app.start_chat_draft(target);
    let candidate = app.chat_ui.draft_checkpoints[&card].candidate.clone();
    assert_ne!(candidate.id, source.id);
    assert!(matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::SavingDraft { .. })
    ));
    assert_eq!(runtime(&app, card).session, source);
    assert!(matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(s) if s == &source));
    assert_eq!(
        runtime(&app, card).composer.text(),
        "First prompt remains local"
    );
    assert_eq!(runtime(&app, card).attachments, images);
    assert!(runtime(&app, card).queued.is_none());
    // An unrelated autosave still stores B but never publishes B as the
    // window's shared/public source before its writer receipt.
    app.persist();
    let publication = requests
        .try_recv()
        .expect("ordinary persist emits its real SeedView");
    match publication {
        crate::workspace_model::ModelEdit::SeedView { panes, .. } => {
            let pane = panes
                .iter()
                .find(|pane| pane.id == card)
                .expect("real shared pane");
            assert!(matches!(&pane.kind, CardKind::Chat(session) if session == &source));
            assert!(!panes.iter().any(
                |pane| matches!(&pane.kind, CardKind::Chat(session) if session.id == candidate.id)
            ));
        }
        other => panic!("expected real A seed, got {other:?}"),
    }
    let (target, attempt, result) = checkpoint(task);
    assert!(result.is_ok(), "{result:?}");
    let saved = stored(&app, card);
    assert_eq!(saved.id, candidate.id);
    assert!(saved.is_unstarted());
    let store = app.session.as_ref().unwrap().operation_store();
    let restored = store.load_workspace(app.durable.id, None).unwrap().unwrap();
    assert_eq!(restored.chat_drafts[&card], "First prompt remains local");
    assert_eq!(restored.chat_draft_attachments[&card], images);
    assert_eq!(runtime(&app, card).session, source);
    let start = app.draft_start_checkpointed(target, attempt, result);
    assert!(
        matches!(app.chat_ui.phases.get(&card), Some(Phase::Starting(a)) if a.session.id == candidate.id)
    );
    assert_eq!(
        runtime(&app, card).queued.as_ref().unwrap().source.text(),
        "First prompt remains local"
    );
    assert_eq!(
        runtime(&app, card).queued.as_ref().unwrap().attachments,
        images
    );
    assert!(
        matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(s) if s.id == candidate.id)
    );
    // Even while the provider-start Task has not been polled, another save
    // retains the queued first message rather than silently clearing it.
    let recovery = app.persistent_workspace();
    assert_eq!(recovery.chat_drafts[&card], "First prompt remains local");
    assert_eq!(recovery.chat_draft_attachments[&card], images);
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let restored = store.load_workspace(app.durable.id, None).unwrap().unwrap();
    assert!(
        matches!(&restored.cards.iter().find(|state| state.id == card).unwrap().kind,
        CardKind::Chat(session) if session == &candidate)
    );
    assert_eq!(restored.chat_drafts[&card], "First prompt remains local");
    assert_eq!(restored.chat_draft_attachments[&card], images);
    drop(start);
    drop(app.draft_start_checkpointed(target, attempt, Ok(())));
    assert!(
        matches!(app.chat_ui.phases.get(&card), Some(Phase::Starting(a)) if a.session.id == candidate.id)
    );
    // A real start-failure reducer, before any user item was accepted,
    // returns the original captured prompt and images to the composer.
    let started = app.chat_ui.attempt(card).unwrap().target;
    drop(app.handle_chat_ui(Action::Ready(
        started,
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            "fixture start refused",
        )))))),
    )));
    assert_eq!(
        runtime(&app, card).composer.text(),
        "First prompt remains local"
    );
    assert_eq!(runtime(&app, card).attachments, images);
    assert!(runtime(&app, card).queued.is_none());
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let restored = store.load_workspace(app.durable.id, None).unwrap().unwrap();
    assert_eq!(restored.chat_drafts[&card], "First prompt remains local");
    assert_eq!(restored.chat_draft_attachments[&card], images);
}

#[test]
fn refused_or_unavailable_first_start_checkpoint_preserves_draft_and_never_starts() {
    for unavailable in [false, true] {
        let (mut app, _directory, card, target) = fixture();
        let source = runtime(&app, card).session.clone();
        let images = runtime(&app, card).attachments.clone();
        if unavailable {
            app.session.as_mut().unwrap().shutdown();
        } else {
            // An actual strict-store mismatch makes the explicit replacement
            // refuse, not a fabricated successful save or relaxed guard.
            let mut foreign = source.clone();
            foreign.id = TerminalId::new();
            app.session
                .as_ref()
                .unwrap()
                .operation_store()
                .replace_chat_identity(card, source.id, &foreign)
                .unwrap();
        }
        let task = app.start_chat_draft(target);
        no_starter(&app, card);
        let (target, attempt, result) = checkpoint(task);
        assert!(result.is_err());
        drop(app.draft_start_checkpointed(target, attempt, result));
        no_starter(&app, card);
        assert!(
            app.chat_ui.draft_checkpoints.is_empty(),
            "completed failed captures are released"
        );
        assert_eq!(runtime(&app, card).session, source);
        assert_eq!(
            runtime(&app, card).composer.text(),
            "First prompt remains local"
        );
        assert_eq!(runtime(&app, card).attachments, images);
        assert!(
            runtime(&app, card)
                .error
                .as_ref()
                .unwrap()
                .contains("nothing was started")
        );
    }
    let (mut app, _directory, card, target) = fixture();
    app.session = None;
    drop(app.start_chat_draft(target));
    no_starter(&app, card);
    assert!(app.chat_ui.draft_checkpoints.is_empty());
}

#[test]
fn failed_first_start_save_can_retry_the_same_candidate_after_its_cause_is_repaired() {
    let (mut app, _directory, card, target) = fixture();
    let source = runtime(&app, card).session.clone();
    let images = runtime(&app, card).attachments.clone();
    let channel = crate::model::ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(crate::model::ProjectChannel {
            id: channel,
            project_id: ProjectId::new(),
            title: "Retry scope".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
    app.workspace_model.identity = uuid::Uuid::new_v4();
    let registry = app.workspace_model.identity;
    let store = app.session.as_ref().unwrap().operation_store();
    // The same guarded store seam used by the refusal regression makes the
    // actual writer refuse its first A -> B replacement. Repair only this
    // task-owned fixture's cause through that API; no successful receipt is
    // fabricated and no provider-start Task is polled.
    let mut foreign = source.clone();
    foreign.id = TerminalId::new();
    store
        .replace_chat_identity(card, source.id, &foreign)
        .unwrap();
    let first = app.start_chat_draft(target);
    let candidate = app.chat_ui.draft_checkpoints[&card].candidate.clone();
    let writer = app.chat_ui.draft_checkpoints[&card].writer.clone();
    let (first_target, first_attempt, first_result) = checkpoint(first);
    assert!(first_result.is_err());
    drop(app.draft_start_checkpointed(first_target, first_attempt, first_result.clone()));
    no_starter(&app, card);
    assert_eq!(runtime(&app, card).session, source);
    assert_eq!(
        runtime(&app, card).composer.text(),
        "First prompt remains local"
    );
    assert_eq!(runtime(&app, card).attachments, images);
    assert_eq!(
        app.chat_ui.canceled_draft_starts[&card].candidate,
        candidate
    );

    store
        .replace_chat_identity(card, foreign.id, &source)
        .unwrap();
    let retry = app.start_chat_draft(target);
    let pending = &app.chat_ui.draft_checkpoints[&card];
    assert_eq!(pending.target, target);
    assert_eq!(pending.source, source);
    assert_eq!(pending.candidate, candidate);
    assert_eq!(pending.registry, registry);
    assert_eq!(
        pending.scope,
        Some(crate::workspace_model::views::Scope::Channel(channel))
    );
    assert!(pending.writer.ptr_eq(&writer));
    assert_ne!(pending.attempt, first_attempt);
    let retry_attempt = pending.attempt;
    assert_eq!(runtime(&app, card).session, source);
    assert_eq!(
        runtime(&app, card).composer.text(),
        "First prompt remains local"
    );
    assert_eq!(runtime(&app, card).attachments, images);
    drop(app.draft_start_checkpointed(first_target, first_attempt, first_result));
    no_starter(&app, card);
    assert_eq!(app.chat_ui.draft_checkpoints[&card].attempt, retry_attempt);

    let (retry_target, received_attempt, result) = checkpoint(retry);
    assert_eq!(retry_target, target);
    assert_eq!(received_attempt, retry_attempt);
    assert!(
        result.is_ok(),
        "the repaired writer must accept this exact retry: {result:?}"
    );
    assert_eq!(stored(&app, card), candidate);
    let saved = store.load_workspace(app.durable.id, None).unwrap().unwrap();
    assert_eq!(saved.chat_drafts[&card], "First prompt remains local");
    assert_eq!(saved.chat_draft_attachments[&card], images);
    // Even this real successful receipt carries no authority for the old
    // nonce. Only the current retry can transition the original draft.
    drop(app.draft_start_checkpointed(first_target, first_attempt, result.clone()));
    no_starter(&app, card);
    let start = app.draft_start_checkpointed(retry_target, retry_attempt, result.clone());
    let started = app
        .chat_ui
        .attempt(card)
        .expect("one authorized start")
        .target;
    assert!(
        matches!(app.chat_ui.phases.get(&card), Some(Phase::Starting(attempt))
        if attempt.target == started && attempt.session == candidate)
    );
    assert_eq!(
        runtime(&app, card).queued.as_ref().unwrap().source.text(),
        "First prompt remains local"
    );
    assert_eq!(
        runtime(&app, card).queued.as_ref().unwrap().attachments,
        images
    );
    drop(app.draft_start_checkpointed(retry_target, retry_attempt, result.clone()));
    drop(app.draft_start_checkpointed(first_target, first_attempt, result));
    assert_eq!(app.chat_ui.attempt(card).unwrap().target, started);
    assert_eq!(
        runtime(&app, card).queued.as_ref().unwrap().source.text(),
        "First prompt remains local"
    );
    assert_eq!(
        runtime(&app, card).queued.as_ref().unwrap().attachments,
        images
    );
    drop(start);
}

#[test]
fn failed_first_start_checkpoint_restores_only_the_current_unclaimed_composer() {
    for context in [
        "current",
        "other-control",
        "other-card",
        "other-window",
        "modal",
        "stale",
        "registry",
        "writer",
        "scope",
        "runtime-replaced",
        "source-replaced",
        "replacement-both",
    ] {
        let (mut app, _directory, card, target) = fixture();
        app.context_inspector_open = false;
        app.refresh_ui.focused = true;
        app.workspace.resize_viewport(1200.0);
        let _ = app.workspace.take_widget_focus_request();
        let source = runtime(&app, card).session.clone();
        let mut foreign = source.clone();
        foreign.id = TerminalId::new();
        app.session
            .as_ref()
            .unwrap()
            .operation_store()
            .replace_chat_identity(card, source.id, &foreign)
            .unwrap();
        let save = app.start_chat_draft(target);
        let (target, attempt, result) = checkpoint(save);
        assert!(result.is_err(), "actual writer refusal: {context}");
        match context {
            "other-control" => {
                app.sidebar_model.projects.push(SidebarProject::new(
                    ProjectId::new(),
                    RepositoryId::new(),
                    "Search fixture".into(),
                    PathBuf::from("/synthetic/project/.git"),
                    Vec::new(),
                    Vec::new(),
                ));
                app.sidebar_state.apply(&SidebarAction::ToggleControls);
            }
            "other-card" => {
                let other = app.workspace.order().find(|id| *id != card).unwrap();
                app.workspace.focus(other).unwrap();
                let _ = app.workspace.take_widget_focus_request();
            }
            "other-window" => app.refresh_ui.focused = false,
            "modal" => app.settings_reset_confirm = true,
            "registry" => app.workspace_model.identity = uuid::Uuid::new_v4(),
            "writer" => app.session = None,
            "scope" => {
                Arc::make_mut(&mut app.durable)
                    .channels
                    .push(crate::model::ProjectChannel {
                        id: crate::model::ChannelId::new(),
                        project_id: ProjectId::new(),
                        title: "Moved scope".into(),
                        title_source: Default::default(),
                        ordinal: 1,
                        shared: false,
                        cards: vec![card],
                        focused: Some(card),
                    });
            }
            "runtime-replaced" => {
                let newer = app.chat_ui.next_target(card);
                match app.cards.get_mut(&card).unwrap() {
                    CardRuntime::Chat(runtime) => runtime.retarget(newer),
                    _ => unreachable!(),
                }
            }
            "source-replaced" => {
                let CardKind::Chat(source) = &mut app.card_state_mut(card).unwrap().kind else {
                    unreachable!()
                };
                source.launch.recipe.arguments.push("changed source".into());
            }
            "replacement-both" => {
                let mut replacement = source.clone();
                replacement.launch.recipe.arguments.push("new owner".into());
                app.card_state_mut(card).unwrap().kind = CardKind::Chat(replacement.clone());
                match app.cards.get_mut(&card).unwrap() {
                    CardRuntime::Chat(runtime) => runtime.session = replacement,
                    _ => unreachable!(),
                }
            }
            "stale" => {
                app.chat_ui
                    .draft_checkpoints
                    .get_mut(&card)
                    .unwrap()
                    .attempt = uuid::Uuid::new_v4();
            }
            _ => {}
        }
        let retained_source = runtime(&app, card).session.clone();
        let recovery = app.draft_start_checkpointed(target, attempt, result);
        let chosen = (context == "other-control").then(sidebar::search_focus_id);
        let task = chosen.as_ref().map_or_else(Task::none, |id| {
            iced::widget::operation::focus(id.clone()).map(cosmic::Action::App)
        });
        let expected = if matches!(context, "current" | "registry" | "writer" | "scope") {
            Some(card_control_id(card, "chat-composer"))
        } else {
            chosen
        };
        assert_eq!(
            crate::app::tests::focused(&app, task.chain(recovery)),
            expected,
            "{context}: execute the actual recovery task on the rendered widget tree"
        );
        no_starter(&app, card);
        assert_eq!(runtime(&app, card).session, retained_source);
        assert_eq!(
            runtime(&app, card).composer.text(),
            "First prompt remains local"
        );
    }
}

#[test]
fn changed_draft_or_source_and_stale_checkpoint_cannot_authorize_first_start() {
    for change in [
        "edit",
        "edit-back",
        "source",
        "account",
        "scope",
        "backend",
        "registry",
        "target",
        "closed",
        "writer",
        "nonce",
    ] {
        let (mut app, _directory, card, target) = fixture();
        let task = app.start_chat_draft(target);
        let candidate = app.chat_ui.draft_checkpoints[&card].candidate.id;
        let (target, attempt, result) = checkpoint(task);
        assert!(result.is_ok());
        match change {
            "edit" | "edit-back" => {
                let runtime = match app.cards.get_mut(&card).unwrap() {
                    CardRuntime::Chat(r) => r,
                    _ => unreachable!(),
                };
                runtime.replace_draft(text_editor::Content::with_text("Edited while saving"));
                if change == "edit-back" {
                    runtime.replace_draft(text_editor::Content::with_text(
                        "First prompt remains local",
                    ));
                }
            }
            "source" => {
                let CardKind::Chat(source) = &mut app.card_state_mut(card).unwrap().kind else {
                    unreachable!()
                };
                source
                    .launch
                    .recipe
                    .arguments
                    .push("different source".into());
            }
            "account" => {
                let CardKind::Chat(source) = &mut app.card_state_mut(card).unwrap().kind else {
                    unreachable!()
                };
                source
                    .launch
                    .recipe
                    .environment
                    .insert("TEST_ACCOUNT".into(), "different original account".into());
            }
            "scope" => {
                Arc::make_mut(&mut app.durable)
                    .channels
                    .push(crate::model::ProjectChannel {
                        id: crate::model::ChannelId::new(),
                        project_id: crate::model::ProjectId::new(),
                        title: "Moved scope".into(),
                        title_source: Default::default(),
                        ordinal: 1,
                        shared: false,
                        cards: vec![card],
                        focused: Some(card),
                    });
            }
            "backend" => {
                app.chat_ui
                    .draft_checkpoints
                    .get_mut(&card)
                    .unwrap()
                    .backend = Some((
                    "old device".into(),
                    "old host".into(),
                    "old transport".into(),
                ))
            }
            "registry" => app.workspace_model.identity = uuid::Uuid::new_v4(),
            "target" => {
                let newer = app.chat_ui.next_target(card);
                match app.cards.get_mut(&card).unwrap() {
                    CardRuntime::Chat(r) => r.retarget(newer),
                    _ => unreachable!(),
                }
            }
            "closed" => app.chat_discard(card),
            "writer" => app.session = None,
            "nonce" => {
                app.chat_ui
                    .draft_checkpoints
                    .get_mut(&card)
                    .unwrap()
                    .attempt = uuid::Uuid::new_v4()
            }
            _ => unreachable!(),
        }
        drop(app.draft_start_checkpointed(target, attempt, result));
        assert!(
            !matches!(app.chat_ui.phases.get(&card), Some(Phase::Starting(_))),
            "{change}"
        );
        assert!(
            app.chat_ui.abandoned.is_empty(),
            "{change}: never-started B must not be retired"
        );
        if app.cards.contains_key(&card) {
            assert!(runtime(&app, card).handle().is_none());
            assert!(runtime(&app, card).queued.is_none());
        }
        if change != "nonce" {
            assert!(app.chat_ui.draft_checkpoints.is_empty());
            assert_eq!(
                app.chat_ui.canceled_draft_starts[&card].candidate.id,
                candidate
            );
        } else {
            assert_eq!(
                app.chat_ui.draft_checkpoints[&card].candidate.id, candidate,
                "no new candidate or false rollback"
            );
        }
    }
}

#[test]
fn edited_message_requires_new_save_of_same_candidate_before_starting() {
    let (mut app, _directory, card, target) = fixture();
    let first = app.start_chat_draft(target);
    let candidate = app.chat_ui.draft_checkpoints[&card].candidate.id;
    let (target, old_attempt, result) = checkpoint(first);
    match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => {
            runtime.replace_draft(text_editor::Content::with_text("Second deliberate send"))
        }
        _ => unreachable!(),
    }
    drop(app.draft_start_checkpointed(target, old_attempt, result));
    no_starter(&app, card);
    let next = app.start_chat_draft(target);
    assert_eq!(app.chat_ui.draft_checkpoints[&card].candidate.id, candidate);
    let attempt = app.chat_ui.draft_checkpoints[&card].attempt;
    assert_ne!(attempt, old_attempt);
    drop(app.draft_start_checkpointed(target, old_attempt, Ok(())));
    no_starter(&app, card);
    let (target, attempt, result) = checkpoint(next);
    assert!(result.is_ok());
    let restored = app
        .session
        .as_ref()
        .unwrap()
        .operation_store()
        .load_workspace(app.durable.id, None)
        .unwrap()
        .unwrap();
    assert_eq!(restored.chat_drafts[&card], "Second deliberate send");
    let start = app.draft_start_checkpointed(target, attempt, result);
    assert_eq!(
        runtime(&app, card).queued.as_ref().unwrap().source.text(),
        "Second deliberate send"
    );
    drop(start);
}

#[test]
fn closing_a_checkpoint_keeps_landed_identity_tombstoned_without_an_actor_or_prompt_leak() {
    let (mut app, _directory, card, target) = fixture();
    let task = app.start_chat_draft(target);
    let candidate = app.chat_ui.draft_checkpoints[&card].candidate.clone();
    // Closing is allowed before this result is delivered, even if storage B
    // has landed. The later completion has no authority to reopen or start.
    app.card_state_mut(card).unwrap().existence = crate::model::Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("closed during first save".into()),
        },
    };
    app.chat_discard(card);
    app.cards.remove(&card);
    app.persist();
    assert!(app.chat_ui.draft_checkpoints.is_empty());
    assert!(!app.chat_ui.drafts.contains_key(&card));
    assert!(!app.chat_ui.draft_attachments.contains_key(&card));
    let (target, attempt, result) = checkpoint(task);
    assert!(result.is_ok(), "{result:?}");
    drop(app.draft_start_checkpointed(target, attempt, result));
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let store = app.session.as_ref().unwrap().operation_store();
    let closed = store.load_card(card, None).unwrap().unwrap();
    assert!(!closed.existence.is_open());
    assert!(matches!(&closed.kind, CardKind::Chat(session) if session == &candidate));
    let restored = store.load_workspace(app.durable.id, None).unwrap().unwrap();
    assert!(!restored.chat_drafts.contains_key(&card));
    assert!(!restored.chat_draft_attachments.contains_key(&card));
    assert!(app.chat_ui.abandoned.is_empty());
    assert!(app.chat_ui.retiring.is_empty());
    assert!(!app.chat_ui.phases.contains_key(&card));
}

#[test]
fn deliberate_candidate_retry_does_not_rearm_an_unrelated_refused_identity() {
    let (mut app, _directory, card, target) = fixture();
    let first = app.start_chat_draft(target);
    let candidate = app.chat_ui.draft_checkpoints[&card].candidate.clone();
    let (target, attempt, result) = checkpoint(first);
    assert!(result.is_ok());
    // Editing after the receipt declines activation while leaving the exact
    // already-landed B as the only candidate for the next deliberate Send.
    match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => {
            runtime.replace_draft(text_editor::Content::with_text("Edited first prompt"))
        }
        _ => unreachable!(),
    }
    drop(app.draft_start_checkpointed(target, attempt, result));
    let mut unrelated = runtime(&app, card).session.clone();
    unrelated.id = TerminalId::new();
    drop(app.insert_chat_card(unrelated.clone(), None, None));
    let other = app.workspace.focused().unwrap();
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let store = app.session.as_ref().unwrap().operation_store();
    let mut foreign = unrelated.clone();
    foreign.id = TerminalId::new();
    store
        .replace_chat_identity(other, unrelated.id, &foreign)
        .unwrap();
    let mut refused = unrelated.clone();
    refused.id = TerminalId::new();
    let snapshot = app.persistent_workspace();
    app.session.as_ref().unwrap().replace_chat_identity(
        other,
        unrelated.id,
        refused.clone(),
        snapshot,
    );
    assert!(
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .is_err()
    );
    // Make the unrelated request eligible again in this isolated store. A
    // global retry_save would now replace it; this exact Send must not do so.
    store
        .replace_chat_identity(other, foreign.id, &unrelated)
        .unwrap();
    let retry = app.start_chat_draft(target);
    assert_eq!(app.chat_ui.draft_checkpoints[&card].candidate, candidate);
    let (target, attempt, result) = checkpoint(retry);
    assert!(
        result.is_err(),
        "unrelated refusal is still truthful save health"
    );
    drop(app.draft_start_checkpointed(target, attempt, result));
    assert_eq!(stored(&app, other), unrelated);
    assert_ne!(stored(&app, other), refused);
    assert_eq!(stored(&app, card), candidate);
    no_starter(&app, card);
    assert_eq!(runtime(&app, card).composer.text(), "Edited first prompt");
}

#[test]
fn old_writer_candidate_is_never_projected_into_a_replacement_store() {
    for forgotten in [false, true] {
        let (mut app, _directory, card, target) = fixture();
        let source = runtime(&app, card).session.clone();
        let task = app.start_chat_draft(target);
        let candidate = app.chat_ui.draft_checkpoints[&card].candidate.clone();
        let (target, attempt, result) = checkpoint(task);
        assert!(result.is_ok());
        let old_writer = app.session.take().unwrap();
        if forgotten {
            app.chat_forget(card);
            assert!(app.chat_ui.draft_checkpoints.is_empty());
        }
        let without_writer = app.persistent_workspace();
        assert!(
            matches!(&without_writer.cards.iter().find(|state| state.id == card).unwrap().kind,
            CardKind::Chat(session) if session == &source)
        );
        let replacement_directory = tempfile::tempdir().unwrap();
        let (replacement, _) = crate::session::SessionRuntime::open(
            replacement_directory.path().join("replacement.sqlite3"),
            crate::model::BootId::new("replacement-writer"),
        )
        .unwrap();
        app.session = Some(replacement);
        drop(app.draft_start_checkpointed(target, attempt, result));
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert_eq!(stored(&app, card), source);
        let old = old_writer
            .operation_store()
            .load_card(card, None)
            .unwrap()
            .unwrap();
        assert!(matches!(&old.kind, CardKind::Chat(session) if session == &candidate));
        no_starter(&app, card);
    }
}

#[test]
fn saved_first_prompt_keeps_exact_whitespace_and_original_images_before_later_edits() {
    for later in ["independent later text\t  \n", " \t\n\n"] {
        let (mut app, directory, card, target) = fixture();
        let first = "  original first prompt\t \n\n";
        match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => {
                runtime.replace_draft(text_editor::Content::with_text(first))
            }
            _ => unreachable!(),
        }
        let original_image = runtime(&app, card).attachments[0].clone();
        let save = app.start_chat_draft(target);
        let (target, attempt, result) = checkpoint(save);
        assert!(result.is_ok());
        let starter = app.draft_start_checkpointed(target, attempt, result);

        let later_image = Attachment {
            path: directory.path().join("later-image.png"),
            reference: Some(chat::ImageReference {
                id: uuid::Uuid::new_v4(),
                number: 2,
                device: uuid::Uuid::new_v4(),
                name: "later-image.png".into(),
                sha256: "b".repeat(64),
                extension: "png".into(),
            }),
        };
        match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => {
                runtime.replace_draft(text_editor::Content::with_text(later));
                runtime.attachments.push(later_image.clone());
                runtime.attachments.push(original_image.clone());
            }
            _ => unreachable!(),
        }
        assert_eq!(
            runtime(&app, card).queued.as_ref().unwrap().source.text(),
            first
        );
        assert_eq!(runtime(&app, card).composer.text(), later);
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let store = app.session.as_ref().unwrap().operation_store();
        let restored = store.load_workspace(app.durable.id, None).unwrap().unwrap();
        assert_eq!(restored.chat_drafts[&card], format!("{first}\n{later}"));
        assert_eq!(
            restored.chat_draft_attachments[&card],
            vec![original_image, later_image]
        );
        // Loading this field is recovery data, not a provider submission, and
        // preparing the snapshot has not changed either independent source.
        assert_eq!(
            runtime(&app, card).queued.as_ref().unwrap().source.text(),
            first
        );
        assert_eq!(runtime(&app, card).composer.text(), later);
        drop(starter);
        let started = app.chat_ui.attempt(card).unwrap().target;
        drop(app.handle_chat_ui(Action::Ready(
            started,
            Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
                "fixture refused before accepting a user item",
            )))))),
        )));
        assert_eq!(
            runtime(&app, card).composer.text(),
            format!("{first}\n{later}")
        );
        assert_eq!(
            runtime(&app, card).attachments,
            restored.chat_draft_attachments[&card]
        );
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let failed = store.load_workspace(app.durable.id, None).unwrap().unwrap();
        assert_eq!(failed.chat_drafts[&card], format!("{first}\n{later}"));
        assert_eq!(
            failed.chat_draft_attachments[&card],
            restored.chat_draft_attachments[&card]
        );
    }
}
/// Reducer/real-writer regression only. The fixture supplies Ready and the
/// provider receipt; provider futures are never polled. Native Claude is a
/// separate acceptance requirement.
#[test]
fn accepted_first_prompt_keeps_default_channel_pane_across_empty_reads() {
    // Fresh Claude emits Ready before a native Session frame. Keep the original
    // already-confirmed case, and also clear the first command while identity
    // is still absent: neither an ID nor a pending command protects that pane.
    for (late_native_identity, wake_before_ack, refuse_creation, disconnect_before_refusal) in [
        (false, true, false, false),
        (false, false, false, false),
        (true, true, false, false),
        (true, false, false, false),
        (false, true, true, false),
        (false, false, true, false),
        (true, true, true, false),
        (true, false, true, false),
        (false, true, true, true),
        (false, false, true, true),
        (true, true, true, true),
        (true, false, true, true),
    ] {
        use crate::app::workspace_model_sync::ModelUpdate;
        use crate::workspace_model::{
            ModelChannel, ModelEdit,
            views::{Change, Pane, Scope, View},
        };
        let mut app = crate::app::tests::context_inspector_test_app(false);
        // Model the mounted foreground strip; a zero-width fixture intentionally
        // drops transcript bodies as offscreen presentation.
        app.workspace.resize_viewport(1280.0);
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().to_path_buf();
        let project = crate::model::ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "First send".into(),
                repository.clone(),
                vec![],
                vec![],
            ));
        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: project,
                title: String::new(),
                title_source: Default::default(),
                ordinal: 1,
                shared: true,
                cards: vec![],
                focused: None,
            });
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        app.active_workspace = Some(repository.clone());
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.sequence = 1;
        app.workspace_model.channels.push(ModelChannel {
            id: channel,
            repository: repository.clone(),
            title: String::new(),
            title_source: Default::default(),
            ordinal: 1,
        });
        app.workspace_model.views.push(View {
            revision: 1,
            ..View::empty(channel)
        });
        let scope = Scope::Channel(channel);
        app.acknowledge_view_edit(scope.clone(), None);
        let (sender, published) = async_channel::unbounded();
        app.workspace_model_tx = sender;
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("first-acceptance-view"),
        )
        .unwrap();
        app.session = Some(writer);
        drop(app.new_native_chat_with_lifetime(Provider::Claude, Some(repository), true));
        let card = app.workspace.focused().unwrap();
        let target = runtime(&app, card).target;
        assert!(app.chat_is_unused_default(card));
        let empty = app.workspace_model.clone();
        drop(app.handle_chat_ui(Action::Composer(
            target,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                "one original prompt".into(),
            ))),
        )));
        drop(app.apply_workspace_model_update(ModelUpdate::Read(empty.clone())));
        let save = app.handle_chat_ui(Action::Submit(target, PromptMode::Send));
        drop(app.apply_workspace_model_update(ModelUpdate::Read(empty.clone())));
        let (target, attempt, result) = checkpoint(save);
        assert!(result.is_ok());
        drop(app.handle_chat_ui(Action::DraftStartCheckpointed(target, attempt, result)));
        let actor = app.chat_ui.attempt(card).unwrap().session.id;
        let ready = super::ready_message_for_pending_switch(
            &app,
            card,
            Snapshot {
                ready: true,
                session_id: (!late_native_identity).then(|| "first-send-native-thread".into()),
                ..Snapshot::default()
            },
        );
        drop(app.update(ready));
        let target = runtime(&app, card).target;
        let handle = runtime(&app, card).handle().unwrap().clone();
        let pending = runtime(&app, card).pending.as_ref().unwrap();
        assert_eq!(pending.queued, Some(QueuedStep::Prompt));
        assert_eq!(pending.sent.as_deref(), Some("one original prompt"));
        let command = pending.id;
        drop(app.apply_workspace_model_update(ModelUpdate::Read(empty.clone())));
        assert_eq!(runtime(&app, card).pending.as_ref().unwrap().id, command);
        let accepted_snapshot = Snapshot {
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
        };
        super::show_chat_snapshot_for_test(
            &mut app,
            card,
            if late_native_identity {
                Snapshot {
                    ready: true,
                    busy: true,
                    ..Snapshot::default()
                }
            } else {
                accepted_snapshot.clone()
            },
        );
        if wake_before_ack {
            drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
        }
        drop(app.handle_chat_ui(Action::CommandFinished(target, command, Ok(()))));
        if !wake_before_ack {
            drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
        }
        assert!(runtime(&app, card).pending.is_none());
        assert!(runtime(&app, card).queued.is_none());
        assert!(!app.shared_chat_has_local_work(card));
        let mut edits = std::iter::from_fn(|| published.try_recv().ok()).collect::<Vec<_>>();
        if refuse_creation {
            if disconnect_before_refusal {
                drop(app.update(Message::ChatUi(Action::Wake(
                    target,
                    Wake::Disconnected("fixture reader connection lost after acceptance".into()),
                ))));
                assert!(runtime(&app, card).disconnected);
                assert!(!app.shared_chat_has_local_work(card));
            }
            let operation = edits
                .iter()
                .find_map(|edit| match edit {
                    ModelEdit::ChangeView {
                        operation,
                        change: Change::Insert { pane, .. },
                        ..
                    } if pane.id == card => Some(*operation),
                    _ => None,
                })
                .unwrap();
            // Unknown callbacks cannot retain a pane or consume its real intent.
            drop(app.apply_workspace_model_update(ModelUpdate::ViewRefused {
                scope: scope.clone(),
                operation: uuid::Uuid::new_v4(),
                error: "unrelated refused edit".into(),
            }));
            assert!(!app.durable.local_only_cards.contains(&card));
            drop(app.apply_workspace_model_update(ModelUpdate::ViewRefused {
                scope: Scope::Channel(crate::model::ChannelId::new()),
                operation,
                error: "same nonce in the wrong scope".into(),
            }));
            assert!(!app.durable.local_only_cards.contains(&card));
            assert!(Arc::ptr_eq(runtime(&app, card).handle().unwrap(), &handle));
            assert_eq!(runtime(&app, card).session.id, actor);
            drop(app.apply_workspace_model_update(ModelUpdate::ViewRefused {
                scope: scope.clone(),
                operation,
                error: "first publication refused after prompt acceptance".into(),
            }));
            assert!(app.durable.local_only_cards.contains(&card));
            assert!(
                runtime(&app, card)
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("could not be shared")
            );
            assert!(Arc::ptr_eq(runtime(&app, card).handle().unwrap(), &handle));
            assert_eq!(runtime(&app, card).session.id, actor);
            if disconnect_before_refusal {
                assert!(
                    runtime(&app, card)
                        .error
                        .as_deref()
                        .unwrap()
                        .contains("Reconnect")
                );
                assert_eq!(
                    app.chat_retry_intent(card, &runtime(&app, card).session),
                    chat::Intent::Restore
                );
            }
            assert!(!app.shared_chat_has_local_work(card));
        }
        if late_native_identity {
            assert!(runtime(&app, card).session.launch.session_id.is_none());
            drop(app.apply_workspace_model_update(ModelUpdate::Read(empty.clone())));
            let unconfirmed = runtime(&app, card);
            assert!(Arc::ptr_eq(unconfirmed.handle().unwrap(), &handle));
            assert_eq!(unconfirmed.session.id, actor);
            assert!(unconfirmed.session.launch.session_id.is_none());
            assert!(unconfirmed.pending.is_none() && unconfirmed.queued.is_none());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert_eq!(app.workspace.focused(), Some(card));
            let saved = app.persistent_workspace();
            assert_eq!(saved.local_only_cards.contains(&card), refuse_creation);
            assert_eq!(
                saved
                    .channels
                    .iter()
                    .find(|entry| entry.id == channel)
                    .unwrap()
                    .cards,
                vec![card]
            );
            // Only the later first-turn fixture snapshot confirms native ID;
            // no second submission or replacement actor may be needed.
            super::show_chat_snapshot_for_test(&mut app, card, accepted_snapshot);
            drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
        }
        // This is the second failure predicate: the first command no longer
        // protects the pane, and the provider is still busy with the original turn.
        drop(app.apply_workspace_model_update(ModelUpdate::Read(empty.clone())));
        drop(app.handle_chat_ui(Action::CommandFinished(target, command, Ok(()))));
        drop(app.apply_workspace_model_update(ModelUpdate::Read(empty)));
        let current = runtime(&app, card);
        assert!(Arc::ptr_eq(current.handle().unwrap(), &handle));
        assert_eq!(current.session.id, actor);
        assert_eq!(
            current.session.launch.session_id.as_deref(),
            Some("first-send-native-thread")
        );
        assert!(current.snapshot.busy);
        assert_eq!(
            current
                .snapshot
                .items
                .iter()
                .filter(|item| item.role == chat::Role::User)
                .count(),
            1
        );
        assert!(current.pending.is_none() && current.queued.is_none());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.workspace.focused(), Some(card));
        let saved = app.persistent_workspace();
        assert_eq!(saved.local_only_cards.contains(&card), refuse_creation);
        assert_eq!(
            saved
                .channels
                .iter()
                .find(|entry| entry.id == channel)
                .unwrap()
                .cards,
            vec![card]
        );
        edits.extend(std::iter::from_fn(|| published.try_recv().ok()));
        assert_eq!(edits.iter().filter(|edit| matches!(edit, ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card)).count(), 1);
        assert!(!edits.iter().any(|edit| matches!(edit, ModelEdit::ChangeView { change: Change::Remove { id }, .. } if *id == card)));
        let pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
        let mut accepted = app.workspace_model.clone();
        accepted.sequence += 1;
        accepted.views[0].panes = vec![pane];
        accepted.views[0].revision += 1;
        if refuse_creation {
            // UI-only reconstruction carries the exact saved local source;
            // it may neither republish the refusal nor lose the live reader.
            app.shared_views = crate::app::shared_views::State::for_workspace(&saved);
            app.acknowledge_view_edit(scope.clone(), None);
            for _ in 0..3 {
                drop(
                    app.apply_workspace_model_update(ModelUpdate::Read(
                        app.workspace_model.clone(),
                    )),
                );
                assert!(Arc::ptr_eq(runtime(&app, card).handle().unwrap(), &handle));
                assert_eq!(runtime(&app, card).session.id, actor);
                assert!(app.card_state(card).unwrap().existence.is_open());
                assert_eq!(app.workspace.focused(), Some(card));
            }
        }
        for edit in edits {
            if !refuse_creation && let ModelEdit::ChangeView { operation, .. } = edit {
                drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
                    scope: scope.clone(),
                    operation: Some(operation),
                    model: accepted.clone(),
                }));
            }
        }
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert_eq!(stored(&app, card).id, actor);
        assert_eq!(
            stored(&app, card).launch.session_id.as_deref(),
            Some("first-send-native-thread")
        );
        assert!(
            !std::iter::from_fn(|| published.try_recv().ok()).any(|edit| matches!(edit,
        ModelEdit::ChangeView { change: Change::Insert { pane, .. }, .. } if pane.id == card))
        );
        assert!(!app.chat_start_pending(card));
        if refuse_creation {
            // A separate app reads the actual saved workspace. This exercises
            // only projection/custody; native actor reattachment is not polled.
            let loaded = app
                .session
                .as_ref()
                .unwrap()
                .operation_store()
                .load_workspace(saved.id, None)
                .unwrap()
                .unwrap();
            let mut reopened = crate::app::tests::context_inspector_test_app(false);
            reopened.durable = Arc::new(loaded);
            reopened.sidebar_model.projects = app.sidebar_model.projects.clone();
            reopened.active_workspace = app.active_workspace.clone();
            reopened.workspace_model = app.workspace_model.clone();
            reopened.shared_views =
                crate::app::shared_views::State::for_workspace(&reopened.durable);
            reopened.acknowledge_view_edit(scope.clone(), None);
            for _ in 0..3 {
                drop(
                    reopened.apply_workspace_model_update(ModelUpdate::Read(
                        app.workspace_model.clone(),
                    )),
                );
                let state = reopened.card_state(card).unwrap();
                assert!(state.existence.is_open());
                assert!(matches!(&state.kind, CardKind::Chat(session) if session.id == actor));
                assert!(reopened.durable.local_only_cards.contains(&card));
                assert_eq!(
                    reopened
                        .durable
                        .channels
                        .iter()
                        .find(|entry| entry.id == channel)
                        .unwrap()
                        .cards,
                    vec![card]
                );
            }
            assert!(!reopened.chat_start_pending(card));
            // Retaining a refused creation grants no authority to override a
            // later canonical retirement, even while the provider is busy.
            let mut retired = app.workspace_model.clone();
            retired.sequence += 1;
            retired.views[0].retired.push(card);
            drop(app.apply_workspace_model_update(ModelUpdate::Read(retired)));
            assert!(app.cards.get(&card).is_none());
            assert!(!app.card_state(card).unwrap().existence.is_open());
        }
    }
}
