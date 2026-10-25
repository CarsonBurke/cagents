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
