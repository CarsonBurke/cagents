use super::*;
use crate::{
    channel_work::{Page, Role},
    channel_work_api::Request,
    host::channel_work::tests::{fixture, wire},
    repository::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use std::{fs, path::Path, time::Duration};

fn git(path: &Path, args: &[&str]) {
    let command = CommandSpec::new("git")
        .args([
            "-c",
            "user.name=Local fixture",
            "-c",
            "user.email=local@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(path)
        .args(args.iter().copied())
        .timeout(Duration::from_secs(10))
        .output_limits(1024 * 1024, 8192);
    let result = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap();
    assert!(
        result.success,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn project(host: &SharedHost, home: Home) -> PathBuf {
    let registry = agent_execution::registry(host).unwrap();
    let repository = registry
        .lock()
        .unwrap()
        .read_channel_work_repository(&home)
        .unwrap();
    git(&repository, &["init", "--template=", "-b", "main"]);
    git(
        &repository,
        &["commit", "--allow-empty", "-m", "Initial fixture"],
    );
    repository
}

fn prepare(host: &SharedHost, home: Home, directory: PathBuf) -> LocalSelection {
    let Response::Selected(selected) = wire(
        host,
        Authority::default(),
        Request::PrepareLocal {
            home,
            directory,
            caller: None,
        },
    ) else {
        panic!("explicit user checkout inspection")
    };
    selected.validate().unwrap();
    selected
}

fn claim(selected: &LocalSelection) -> Mutation {
    Mutation {
        home: selected.home,
        expected_revision: selected.expected_revision,
        operation: Uuid::new_v4(),
        change: Change::Claim {
            target: Target::Local(selected.context.id),
            role: Role::Contribute,
        },
    }
}

#[test]
fn malformed_wire_paths_are_refused_without_opening_a_checkout_or_changing_work() {
    let (host, _directory, home) = fixture();
    let registry = agent_execution::registry(&host).unwrap();
    let before = registry.lock().unwrap().model().unwrap();
    for directory in [
        PathBuf::from("relative"),
        PathBuf::from("/project/../other"),
        PathBuf::from("/nul\0path"),
        PathBuf::from(format!(
            "/{}",
            "a".repeat(crate::channel_work::MAX_CONTEXT_PATH_BYTES)
        )),
    ] {
        let Response::Failed(Failure::Refused(reason)) = wire(
            &host,
            Authority::default(),
            Request::PrepareLocal {
                home,
                directory,
                caller: None,
            },
        ) else {
            panic!("an invalid wire path must be refused")
        };
        assert!(reason.starts_with("Choose an absolute checkout path"));
    }
    assert_eq!(registry.lock().unwrap().model().unwrap(), before);
    let projection = registry
        .lock()
        .unwrap()
        .read_channel_work(&home, Page::default())
        .unwrap();
    assert!(projection.entries.is_empty() && projection.history.is_empty());
    // Invalid input cannot consume even one of the finite inspection credits.
    let selections = &host.lock().unwrap().channel_work_selections;
    let credits: Vec<_> = (0..4).map(|_| selections.inspect().unwrap()).collect();
    assert!(selections.inspect().is_err());
    drop(credits);
}

#[test]
fn own_launch_inspection_refuses_user_and_unknown_authority_without_creating_native_identity() {
    let (host, _directory, home) = fixture();
    let registry = agent_execution::registry(&host).unwrap();
    let before = registry.lock().unwrap().model().unwrap();
    for (authority, captured) in [
        (Authority::default(), None),
        (Authority::session(Uuid::new_v4().to_string()), None),
        (
            Authority::default(),
            Some(Caller {
                session: crate::model::TerminalId::new(),
                card: crate::model::CardId::new(),
                placement_epoch: 1,
                run: None,
            }),
        ),
    ] {
        assert!(matches!(
            wire(
                &host,
                authority,
                Request::PrepareOwnLocal {
                    home,
                    caller: captured
                }
            ),
            Response::Failed(Failure::Refused(_))
        ));
    }
    let projection = registry
        .lock()
        .unwrap()
        .read_channel_work(&home, Page::default())
        .unwrap();
    assert!(projection.entries.is_empty() && projection.history.is_empty());
    assert_eq!(registry.lock().unwrap().model().unwrap(), before);
    let state = host.lock().unwrap();
    let credits: Vec<_> = (0..4)
        .map(|_| state.channel_work_selections.inspect().unwrap())
        .collect();
    assert!(state.channel_work_selections.inspect().is_err());
    drop(credits);
}

#[test]
fn actual_user_selection_claims_and_consumed_ticket_replays_only_original_operation() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::actual_user_selection_claims_and_consumed_ticket_replays_only_original_operation"
    )) {
        return;
    }
    let (host, _directory, home) = fixture();
    let repository = project(&host, home);
    fs::write(repository.join("unpublished"), "Local edits\n").unwrap();
    let selected = prepare(&host, home, repository);
    assert_eq!(selected.context.dirty, None);
    assert_eq!(selected.expected_revision, 0);
    let mutation = claim(&selected);
    assert!(
        matches!(
            wire(
                &host,
                Authority::default(),
                Request::Mutate {
                    mutation: mutation.clone(),
                    caller: None,
                }
            ),
            Response::Failed(Failure::Refused(_))
        ),
        "a raw UUID is not an inspected checkout"
    );
    let request = Request::ApplySelection {
        selection: selected.selection,
        mutation: mutation.clone(),
        caller: None,
    };
    let Response::Applied(applied) = wire(&host, Authority::default(), request.clone()) else {
        panic!("selected local declaration")
    };
    assert_eq!(applied.snapshot.entries.len(), 1);
    assert_eq!(
        applied.snapshot.entries[0].contexts,
        [selected.context.clone()]
    );
    assert_eq!(
        applied.snapshot.entries[0].claims[0].source,
        crate::channel_work::Source::User
    );
    assert!(applied.snapshot.entries[0].is_ongoing_work());
    let registry = agent_execution::registry(&host).unwrap();
    let before = registry.lock().unwrap().model().unwrap().sequence;
    let Response::Applied(replay) = wire(&host, Authority::default(), request.clone()) else {
        panic!("original receipt must survive ticket consumption")
    };
    assert_eq!(replay.receipt, applied.receipt);
    assert_eq!(registry.lock().unwrap().model().unwrap().sequence, before);
    let consumed_during_attempt = reconcile_result(
        &host,
        &Authority::default(),
        &mutation,
        &None,
        Err(ApplyError::Refused(
            "The selection was consumed after the initial receipt miss".into(),
        )),
    )
    .unwrap();
    assert_eq!(consumed_during_attempt.receipt, applied.receipt);
    assert_eq!(registry.lock().unwrap().model().unwrap().sequence, before);
    let mut fresh = mutation.clone();
    fresh.operation = Uuid::new_v4();
    fresh.expected_revision = 1;
    assert!(
        matches!(
            wire(
                &host,
                Authority::default(),
                Request::ApplySelection {
                    selection: selected.selection,
                    mutation: fresh,
                    caller: None,
                }
            ),
            Response::Failed(Failure::Refused(_))
        ),
        "consumed ticket cannot admit a new operation"
    );
    let mut conflict = mutation.clone();
    conflict.change = Change::Claim {
        target: mutation.change.target().clone(),
        role: Role::Review,
    };
    assert!(matches!(
        wire(
            &host,
            Authority::default(),
            Request::ApplySelection {
                selection: selected.selection,
                mutation: conflict,
                caller: None,
            }
        ),
        Response::Failed(Failure::Refused(_))
    ));
    let Response::Applied(ended) = wire(
        &host,
        Authority::default(),
        Request::Mutate {
            mutation: Mutation {
                home,
                operation: Uuid::new_v4(),
                expected_revision: 1,
                change: Change::End {
                    target: mutation.change.target().clone(),
                },
            },
            caller: None,
        },
    ) else {
        panic!("end known local intent")
    };
    assert_eq!(ended.snapshot.history.len(), 1);
    let Response::Applied(replay) = wire(&host, Authority::default(), request) else {
        panic!("retained original receipt")
    };
    assert_eq!(replay.receipt, applied.receipt);
    assert_eq!(
        replay.snapshot, ended.snapshot,
        "replay cannot resurrect ended local work"
    );
}

#[test]
fn replaced_checkout_and_foreign_or_forged_selection_leave_canonical_work_untouched() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::replaced_checkout_and_foreign_or_forged_selection_leave_canonical_work_untouched"
    )) {
        return;
    }
    let (host, directory, home) = fixture();
    let repository = project(&host, home);
    let selected = prepare(&host, home, repository.clone());
    let request = claim(&selected);
    let registry = agent_execution::registry(&host).unwrap();
    let before = registry.lock().unwrap().model().unwrap();
    let empty = registry
        .lock()
        .unwrap()
        .read_channel_work(&home, Page::default())
        .unwrap();
    assert!(matches!(
        wire(
            &host,
            Authority::default(),
            Request::ApplySelection {
                selection: Uuid::new_v4(),
                mutation: request.clone(),
                caller: None,
            }
        ),
        Response::Failed(Failure::Refused(_))
    ));
    let foreign = Home {
        registry: Uuid::new_v4(),
        ..home
    };
    assert!(matches!(
        wire(
            &host,
            Authority::default(),
            Request::PrepareLocal {
                home: foreign,
                directory: repository.clone(),
                caller: None,
            }
        ),
        Response::Failed(Failure::Refused(_))
    ));
    fs::rename(&repository, directory.path().join("original-retained")).unwrap();
    fs::create_dir(&repository).unwrap();
    fs::write(repository.join("replacement-preimage"), "Preserve\n").unwrap();
    assert!(matches!(
        wire(
            &host,
            Authority::default(),
            Request::ApplySelection {
                selection: selected.selection,
                mutation: request.clone(),
                caller: None,
            }
        ),
        Response::Failed(Failure::Refused(_))
    ));
    assert_eq!(
        fs::read(repository.join("replacement-preimage")).unwrap(),
        b"Preserve\n"
    );
    assert_eq!(registry.lock().unwrap().model().unwrap(), before);
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .read_channel_work(&home, Page::default())
            .unwrap(),
        empty
    );
    let resolved = resolve(&host, &Authority::default()).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .channel_work_receipt(&resolved, &home, request.operation)
            .unwrap()
            .is_none()
    );
}

#[test]
fn live_terminal_own_launch_selection_keeps_actor_origin_and_exact_replay_after_stale_caller() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::live_terminal_own_launch_selection_keeps_actor_origin_and_exact_replay_after_stale_caller"
    )) {
        return;
    }
    use crate::{
        agent_execution::HostedSession,
        model::{CardId, CardKind, ChannelId, LaunchRecipe, TerminalId},
        terminal_ipc::{TerminalCloseResult, TerminalDescriptor, TerminalSpawnOptions},
        workspace_model::{ModelEdit, views},
    };

    let (host, directory, home) = fixture();
    let repository = project(&host, home);
    let checkout = repository.canonicalize().unwrap();
    let id = TerminalId::new();
    let session = TerminalDescriptor::relaunchable(
        id,
        LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: repository.clone(),
            environment: [
                (
                    "HOME".into(),
                    directory.path().to_string_lossy().into_owned(),
                ),
                (
                    "CODEX_HOME".into(),
                    directory
                        .path()
                        .join("codex")
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    directory
                        .path()
                        .join("claude")
                        .to_string_lossy()
                        .into_owned(),
                ),
            ]
            .into(),
            unset_environment: Default::default(),
        },
        None,
    )
    .session;
    let (descriptor, handle) = crate::host::state::spawn_terminal_view_owned(
        &host,
        1,
        session,
        TerminalSpawnOptions::default(),
        None,
        false,
    )
    .unwrap();
    // Keep the exact published worker owned through every assertion, including
    // panic. Only a confirmed ordinary close may retire that same worker.
    struct CloseOriginal {
        host: SharedHost,
        id: TerminalId,
        handle: crate::terminal::TerminalHandle,
    }
    impl Drop for CloseOriginal {
        fn drop(&mut self) {
            if self.handle.close_checked(true) == TerminalCloseResult::Closed {
                let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
                if state
                    .terminals
                    .get(&self.id)
                    .is_some_and(|record| record.handle.same_worker(&self.handle))
                {
                    state.retire_terminal(self.id);
                }
            }
        }
    }
    let cleanup = CloseOriginal {
        host: host.clone(),
        id,
        handle: handle.clone(),
    };
    assert!(!handle.is_completed());
    let source = HostedSession::Terminal(descriptor.session.clone());
    assert!(source.has_complete_proof());
    assert!(
        host.lock().unwrap().terminals[&id]
            .session
            .continuity
            .live_proof()
            .is_none(),
        "the ordinary stored recipe is not the returned live descriptor"
    );
    let registry = agent_execution::registry(&host).unwrap();
    let authority = registry
        .lock()
        .unwrap()
        .retained_agent_authority(id)
        .unwrap();
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .agent_authority_source(&authority)
            .unwrap(),
        Some(source)
    );
    let scope = views::Scope::Channel(home.channel);
    let card = CardId::new();
    let pane = views::Pane {
        id: card,
        title: "Original native terminal".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        kind: CardKind::Terminal(descriptor.session.clone()),
        scope: None,
    };
    registry
        .lock()
        .unwrap()
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        })
        .unwrap();
    let Response::Context(original) =
        wire(&host, authority.clone(), Request::Context { channel: None })
    else {
        panic!("the live issued actor must resolve its canonical channel")
    };
    assert_eq!(original.home, home);
    let old_caller = original.caller.expect("canonical native placement");
    assert_eq!((old_caller.session, old_caller.card), (id, card));

    // A real canonical transfer round trip preserves the process and card but
    // retires its original placement epoch. Do not fabricate a replacement
    // authority, Host proof or launch-directory descriptor for this refusal.
    let other = ChannelId::new();
    registry
        .lock()
        .unwrap()
        .apply(&ModelEdit::CreateChannel {
            id: other,
            repository,
            title: "Other channel".into(),
            title_source: Default::default(),
        })
        .unwrap();
    let other_scope = views::Scope::Channel(other);
    for (from, destination) in [
        (scope.clone(), other_scope.clone()),
        (other_scope, scope.clone()),
    ] {
        let mut registry = registry.lock().unwrap();
        let revision = registry
            .model()
            .unwrap()
            .views
            .iter()
            .find(|view| view.scope == from)
            .unwrap()
            .revision;
        registry
            .apply(&ModelEdit::ChangeView {
                scope: from,
                expected_revision: revision,
                operation: Uuid::new_v4(),
                change: views::Change::Transfer {
                    id: card,
                    destination,
                    before: None,
                },
            })
            .unwrap();
    }
    let before = registry.lock().unwrap().model().unwrap();
    assert!(matches!(
        wire(
            &host,
            authority.clone(),
            Request::PrepareOwnLocal {
                home,
                caller: Some(old_caller.clone()),
            },
        ),
        Response::Failed(Failure::Refused(_))
    ));
    assert_eq!(registry.lock().unwrap().model().unwrap(), before);
    assert!(
        registry
            .lock()
            .unwrap()
            .read_channel_work(&home, Page::default())
            .unwrap()
            .entries
            .is_empty()
    );
    let Response::Context(current) =
        wire(&host, authority.clone(), Request::Context { channel: None })
    else {
        panic!("the same live actor must resolve the new canonical placement")
    };
    assert_eq!(current.home, home);
    let captured = current.caller.expect("fresh canonical native placement");
    assert_eq!((captured.session, captured.card), (id, card));
    assert!(captured.placement_epoch > old_caller.placement_epoch);
    let Response::Selected(selected) = wire(
        &host,
        authority.clone(),
        Request::PrepareOwnLocal {
            home,
            caller: Some(captured.clone()),
        },
    ) else {
        panic!("inspect the live terminal's actual retained launch checkout")
    };
    selected.validate().unwrap();
    assert_eq!(selected.home, home);
    assert_eq!(selected.context.repository, checkout);
    assert_eq!(selected.context.path, checkout);
    assert_eq!(selected.context.dirty, None);
    let mutation = claim(&selected);
    let request = Request::ApplySelection {
        selection: selected.selection,
        mutation: mutation.clone(),
        caller: Some(captured),
    };
    let Response::Applied(applied) = wire(&host, authority.clone(), request.clone()) else {
        panic!("commit the original actor's inspected local declaration")
    };
    assert_eq!(applied.receipt.operation, mutation.operation);
    assert!(applied.receipt.changed);
    assert_eq!(applied.snapshot.entries.len(), 1);
    assert_eq!(applied.snapshot.entries[0].contexts, [selected.context]);
    let crate::channel_work::Source::Session(contributor) =
        &applied.snapshot.entries[0].claims[0].source
    else {
        panic!("native intent must retain Actor provenance, never User")
    };
    assert_eq!(contributor.session, id);
    assert_eq!(contributor.lineage, crate::agent_execution::Lineage(id));
    assert_eq!((contributor.agent, contributor.run), (None, None));
    let canonical = registry
        .lock()
        .unwrap()
        .read_channel_work(&home, Page::default())
        .unwrap();
    assert_eq!(canonical, applied.snapshot);
    let model = registry.lock().unwrap().model().unwrap();
    let Response::Applied(replay) = wire(&host, authority.clone(), request) else {
        panic!("the consumed selection must reconcile only its original operation")
    };
    assert_eq!(replay.receipt, applied.receipt);
    assert_eq!(replay.snapshot, canonical);
    assert_eq!(registry.lock().unwrap().model().unwrap(), model);
    assert_eq!(
        wire(
            &host,
            authority,
            Request::Receipt {
                home,
                operation: mutation.operation,
            },
        ),
        Response::Receipt(Some(applied.receipt))
    );
    assert_eq!(handle.close_checked(true), TerminalCloseResult::Closed);
    drop(cleanup);
    assert!(!host.lock().unwrap().terminals.contains_key(&id));
}
