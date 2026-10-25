use super::*;
use crate::{
    agent_execution::Lineage,
    model::{CardId, ChannelId, TerminalId},
    repository::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use std::path::Path;

fn git(path: &Path, arguments: &[&str]) {
    let command = CommandSpec::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Selection fixture",
            "-c",
            "user.email=selection@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(arguments.iter().copied())
        .timeout(Duration::from_secs(10))
        .output_limits(1024 * 1024, 8192);
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap();
    assert!(
        output.success,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> (tempfile::TempDir, Home) {
    let directory = tempfile::tempdir().unwrap();
    git(
        directory.path(),
        &["init", "--template=", "--initial-branch=main"],
    );
    git(
        directory.path(),
        &["commit", "--allow-empty", "-m", "Initial fixture"],
    );
    (
        directory,
        Home {
            registry: Uuid::new_v4(),
            channel: ChannelId::new(),
            creation_ordinal: 1,
        },
    )
}

fn user() -> Owner {
    Owner {
        source: Source::User,
        caller: None,
    }
}

fn capture(path: &Path) -> Arc<CapturedCheckout> {
    Arc::new(
        CapturedCheckout::capture_user(path, path, &crate::repository::CancellationToken::new())
            .unwrap(),
    )
}

#[test]
fn selection_nonce_needs_original_host_home_and_authenticated_source_comparisons() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::selection_nonce_needs_original_host_home_and_authenticated_source_comparisons"
    )) {
        return;
    }
    let (directory, home) = fixture();
    let now = Instant::now();
    let mut selections = Selections::default();
    let candidate = selections
        .issue(home, user(), capture(directory.path()), now)
        .unwrap();
    assert!(
        selections
            .lookup(home, &user(), candidate.selection, now)
            .is_ok()
    );
    for wrong in [
        Home {
            registry: Uuid::new_v4(),
            ..home
        },
        Home {
            channel: ChannelId::new(),
            ..home
        },
        Home {
            creation_ordinal: 2,
            ..home
        },
    ] {
        assert!(
            selections
                .lookup(wrong, &user(), candidate.selection, now)
                .is_err()
        );
        selections.consume(wrong, &user(), candidate.selection);
    }
    let terminal = TerminalId::new();
    let session = Owner {
        source: Source::Session(crate::channel_work::Contributor {
            session: terminal,
            lineage: Lineage(terminal),
            agent: None,
            run: None,
        }),
        caller: Some(Caller {
            session: terminal,
            card: CardId::new(),
            placement_epoch: 1,
            run: None,
        }),
    };
    assert!(
        selections
            .lookup(home, &session, candidate.selection, now)
            .is_err()
    );
    selections.consume(home, &session, candidate.selection);
    assert!(
        selections
            .lookup(home, &user(), candidate.selection, now)
            .is_ok()
    );
    assert!(
        Selections::default()
            .lookup(home, &user(), candidate.selection, now)
            .is_err()
    );
    selections.consume(home, &user(), candidate.selection);
    assert!(
        selections
            .lookup(home, &user(), candidate.selection, now)
            .is_err()
    );
}

#[test]
fn unchanged_branch_observations_keep_identity_while_branch_intervals_never_rebind_old_selection() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::unchanged_branch_observations_keep_identity_while_branch_intervals_never_rebind_old_selection"
    )) {
        return;
    }
    let (directory, home) = fixture();
    let now = Instant::now();
    let mut selections = Selections::default();
    let original = selections
        .issue(home, user(), capture(directory.path()), now)
        .unwrap();
    let prepared = selections
        .lookup(home, &user(), original.selection, now)
        .unwrap();
    std::fs::write(directory.path().join("unpublished.txt"), "Local work\n").unwrap();
    let edited = selections
        .issue(home, user(), capture(directory.path()), now)
        .unwrap();
    assert_eq!(edited.context.id, original.context.id);
    assert_eq!(edited.context.generation, original.context.generation);
    assert_eq!(edited.context.dirty, None);
    assert_eq!(original.context.dirty, None);
    assert_eq!(prepared.context, original.context);
    git(directory.path(), &["add", "unpublished.txt"]);
    git(
        directory.path(),
        &["commit", "-m", "Unpublished local change"],
    );
    let committed = selections
        .issue(home, user(), capture(directory.path()), now)
        .unwrap();
    assert_eq!(committed.context.id, original.context.id);
    assert_ne!(committed.context.head, original.context.head);
    assert!(committed.context.generation > original.context.generation);
    assert_eq!(committed.context.dirty, None);
    git(directory.path(), &["switch", "-c", "next"]);
    let next = selections
        .issue(home, user(), capture(directory.path()), now)
        .unwrap();
    assert_ne!(next.context.id, original.context.id);
    git(directory.path(), &["switch", "main"]);
    let returned = selections
        .issue(home, user(), capture(directory.path()), now)
        .unwrap();
    assert_ne!(
        returned.context.id, original.context.id,
        "observed branch ABA starts a fresh interval"
    );
    assert!(
        prepared
            .pin
            .revalidate(&crate::repository::CancellationToken::new())
            .is_err()
    );
    assert_eq!(
        prepared.context, original.context,
        "old callbacks cannot become the returned branch"
    );
}

#[test]
fn selection_capacity_preserves_pending_choices_and_expiry_requires_fresh_selection() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::selection_capacity_preserves_pending_choices_and_expiry_requires_fresh_selection"
    )) {
        return;
    }
    let (directory, home) = fixture();
    let now = Instant::now();
    let mut selections = Selections::default();
    let pin = capture(directory.path());
    let first = selections
        .issue(home, user(), Arc::clone(&pin), now)
        .unwrap();
    for _ in 1..MAX_SELECTIONS {
        selections
            .issue(home, user(), Arc::clone(&pin), now)
            .unwrap();
    }
    assert!(
        selections
            .issue(home, user(), Arc::clone(&pin), now)
            .is_err()
    );
    assert!(
        selections
            .lookup(home, &user(), first.selection, now)
            .is_ok()
    );
    let expired = now + SELECTION_LIFETIME;
    assert!(
        selections
            .lookup(home, &user(), first.selection, expired)
            .is_err()
    );
    assert!(selections.selections.is_empty() && selections.intervals.is_empty());
    let fresh = selections.issue(home, user(), pin, expired).unwrap();
    assert_ne!(fresh.selection, first.selection);
    assert_ne!(fresh.context.id, first.context.id);
}

#[test]
fn delayed_old_inspection_cannot_become_a_newer_context_observation() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::delayed_old_inspection_cannot_become_a_newer_context_observation"
    )) {
        return;
    }
    let (directory, home) = fixture();
    let old = capture(directory.path());
    std::fs::write(directory.path().join("new-work"), "Unpublished\n").unwrap();
    git(directory.path(), &["add", "new-work"]);
    git(
        directory.path(),
        &["commit", "-m", "New metadata observation"],
    );
    let newer = capture(directory.path());
    assert_ne!(newer.facts().head, old.facts().head);
    assert!(newer.observed_at() > old.observed_at());
    let now = Instant::now();
    let mut selections = Selections::default();
    let selected = selections.issue(home, user(), newer, now).unwrap();
    assert_eq!(selected.context.dirty, None);
    assert!(selections.issue(home, user(), old, now).is_err());
    let current = selections
        .issue(home, user(), capture(directory.path()), Instant::now())
        .unwrap();
    assert_eq!(
        current.context, selected.context,
        "late completion cannot replace the newer commit observation"
    );
}

#[test]
fn inspection_credits_are_host_scoped_and_return_after_normal_owner_drop() {
    let selections = Selections::default();
    let mut readers = (0..MAX_INSPECTIONS)
        .map(|_| selections.inspect().unwrap())
        .collect::<Vec<_>>();
    assert!(selections.inspect().is_err());
    let other_host = Selections::default();
    let independent = other_host.inspect().unwrap();
    readers.pop();
    let replacement = selections.inspect().unwrap();
    assert!(selections.inspect().is_err());
    drop(readers);
    drop(replacement);
    assert_eq!(selections.inspections.load(Ordering::Acquire), 0);
    assert_eq!(other_host.inspections.load(Ordering::Acquire), 1);
    drop(independent);
    assert_eq!(other_host.inspections.load(Ordering::Acquire), 0);
}

#[test]
fn concurrent_distinct_operations_cannot_share_one_selection_before_commit() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::concurrent_distinct_operations_cannot_share_one_selection_before_commit"
    )) {
        return;
    }
    let (directory, home) = fixture();
    let mut selections = Selections::default();
    let selected = selections
        .issue(home, user(), capture(directory.path()), Instant::now())
        .unwrap();
    let first = Mutation {
        home,
        operation: Uuid::new_v4(),
        expected_revision: 0,
        change: Change::Claim {
            target: Target::Local(selected.context.id),
            role: crate::channel_work::Role::Contribute,
        },
    };
    let second = Mutation {
        operation: Uuid::new_v4(),
        expected_revision: 1,
        change: Change::Claim {
            target: Target::Local(selected.context.id),
            role: crate::channel_work::Role::Review,
        },
        ..first.clone()
    };
    let selections = Arc::new(std::sync::Mutex::new(selections));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let handles = [first, second]
            .into_iter()
            .map(|mutation| {
                let selections = Arc::clone(&selections);
                let barrier = Arc::clone(&barrier);
                let selection = selected.selection;
                scope.spawn(move || {
                    barrier.wait();
                    let result = selections.lock().unwrap().begin_operation(
                        home,
                        &user(),
                        selection,
                        &mutation,
                        Instant::now(),
                    );
                    (mutation, result)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    let winner = results
        .iter()
        .find(|(_, result)| result.is_ok())
        .unwrap()
        .0
        .clone();
    let mut selections = selections.lock().unwrap();
    assert!(
        matches!(
            selections.begin_operation(home, &user(), selected.selection, &winner, Instant::now()),
            Err(BeginError::InFlight)
        ),
        "an identical concurrent request cannot start another reader or commit"
    );
    drop(results);
    let retry = selections
        .begin_operation(home, &user(), selected.selection, &winner, Instant::now())
        .unwrap();
    assert!(
        selections
            .confirm_operation(home, &user(), selected.selection, &retry, Instant::now())
            .is_ok()
    );
    let mut changed_cas = winner.clone();
    changed_cas.expected_revision += 1;
    assert!(
        selections
            .begin_operation(
                home,
                &user(),
                selected.selection,
                &changed_cas,
                Instant::now()
            )
            .is_err()
    );
    let mut changed_role = winner.clone();
    changed_role.change = Change::Claim {
        target: Target::Local(selected.context.id),
        role: crate::channel_work::Role::Steward,
    };
    assert!(
        selections
            .begin_operation(
                home,
                &user(),
                selected.selection,
                &changed_role,
                Instant::now()
            )
            .is_err()
    );
}

#[test]
fn expired_active_attempt_keeps_duplicate_unknown_until_normal_drop_and_cannot_publish() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::expired_active_attempt_keeps_duplicate_unknown_until_normal_drop_and_cannot_publish"
    )) {
        return;
    }
    let (directory, home) = fixture();
    let now = Instant::now();
    let mut selections = Selections::default();
    let selected = selections
        .issue(home, user(), capture(directory.path()), now)
        .unwrap();
    let mutation = Mutation {
        home,
        expected_revision: 0,
        operation: Uuid::new_v4(),
        change: Change::Claim {
            target: Target::Local(selected.context.id),
            role: crate::channel_work::Role::Contribute,
        },
    };
    let active = selections
        .begin_operation(home, &user(), selected.selection, &mutation, now)
        .unwrap();
    let expired = now + SELECTION_LIFETIME;
    assert!(matches!(
        selections.begin_operation(home, &user(), selected.selection, &mutation, expired),
        Err(BeginError::InFlight)
    ));
    assert!(
        selections
            .confirm_operation(home, &user(), selected.selection, &active, expired)
            .is_err()
    );
    assert_eq!(selections.selections.len(), 1);
    drop(active);
    assert!(matches!(
        selections.begin_operation(home, &user(), selected.selection, &mutation, expired),
        Err(BeginError::Refused(_))
    ));
    assert!(selections.selections.is_empty());
}
