mod automation_work_authority;

#[test]
fn terminal_scrollback_settings_draft_reaches_runtime_as_bytes() {
    let mut app = context_inspector_test_app(false);
    assert_eq!(app.settings.terminal_scrollback_mib, 16);
    for mib in [0, 16, 32, settings::MAX_TERMINAL_SCROLLBACK_MIB] {
        let SettingsDecision::Persist(next) = settings_update(
            &app.settings,
            SettingsField::TerminalScrollbackMib,
            &mib.to_string(),
        ) else {
            panic!("valid MiB setting must persist")
        };
        let draft = SettingsDraft::from_settings(&next);
        assert_eq!(draft.terminal_scrollback_mib, mib.to_string());
        app.settings = next;
        let config = app.terminal_config(false);
        assert!(!config.initially_visible);
        assert_eq!(config.max_scrollback_bytes, mib * 1024 * 1024);
        assert_eq!(
            crate::terminal::TerminalSpawnOptions::from(config).max_scrollback_bytes,
            mib * 1024 * 1024
        );
    }
    for invalid in ["-1", "1025", "18446744073709551615"] {
        assert!(matches!(
            settings_update(&app.settings, SettingsField::TerminalScrollbackMib, invalid),
            SettingsDecision::Invalid(_)
        ));
    }
}

#[test]
fn completed_push_observation_warning_preserves_known_completion() {
    let notice = super::push_report(Ok(crate::repository::PushOutcome::OutcomeUnknown {
        failure: crate::repository::MutationFailure::Reconciliation {
            reason: "The source branch changed during the push".into(),
        },
    }))
    .unwrap();
    assert!(notice.starts_with("Push completed,"));
    assert!(notice.contains("published commit is uncertain"));
    assert!(!notice.contains("may or may not have reached"));
}

use super::*;
use crate::model::WorktreeId;
use cosmic::iced::futures::StreamExt;

#[test]
fn empty_workspace_new_terminal_uses_the_presented_starting_directory() {
    let mut app = context_inspector_test_app(false);
    assert!(app.terminal_directory().is_none());
    let _ = app.update(Message::NewTerminal);
    let card = app.card_state(app.workspace.focused().unwrap()).unwrap();
    let CardKind::Terminal(terminal) = &card.kind else {
        panic!("New terminal must create a terminal without a project");
    };
    assert_eq!(terminal.launch.working_directory, *STARTING_DIRECTORY);
    assert!(app.sidebar_model.projects.is_empty());
}

#[test]
fn update_prepare_vetoes_dirty_editor_without_closing_or_freezing() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    let _ = app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Prepare,
        response,
    }));
    assert!(receiver.try_recv().unwrap().starts_with("error:"));
    assert!(!app.update_prepared);
    assert!(app.cards.contains_key(&card));
    assert!(app.dirty_repository_close.is_none());
}

#[test]
fn cancelled_update_unfreezes_the_existing_window() {
    let mut app = context_inspector_test_app(true);
    app.update_prepared = true;
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    let _ = app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Cancel,
        response,
    }));
    assert_eq!(receiver.try_recv().unwrap(), "ok");
    assert!(!app.update_prepared);
    assert!(app.workspace.focused().is_some());
}

#[test]
fn update_freeze_drains_diff_basis_resolution_without_accepting_new_choices() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "notes.txt", "");
    let choice = repository_widget::CommittedChoice::LastCommit;
    drop(runtime.update(RepositoryAction::SelectCommittedBasis { card, choice }));
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    app.update_prepared = true;
    let basis = DiffBasis::CommitToParent {
        oid: crate::repository::Oid::parse_object("ab".repeat(20)).unwrap(),
        parent: None,
    };
    let task = app.update(Message::Repository(RepositoryAction::DiffBasisResolved {
        identity,
        choice,
        result: Ok(repository_widget::ResolvedBasis::Ready(basis)),
    }));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                        drop(app.update(message));
                    }
                }
            }
        });
    let context = match &app.cards[&card] {
        CardRuntime::Repository(runtime) => runtime.inspector_context(),
        _ => unreachable!(),
    };
    assert_eq!(context.diff_basis, Some("Commit"));
    drop(
        app.update(Message::Repository(RepositoryAction::ApplyDiffBasis {
            card,
            basis: DiffBasis::IndexToWorktree,
        })),
    );
    let CardRuntime::Repository(runtime) = &app.cards[&card] else {
        unreachable!()
    };
    assert_eq!(runtime.inspector_context().diff_basis, Some("Commit"));
}

fn browser_kind(raw: &str) -> CardKind {
    let url = Url::parse(raw).unwrap();
    CardKind::Browser(crate::model::BrowserSession {
        id: crate::model::BrowserId::new(),
        current_url: url.clone(),
        history: vec![url],
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: crate::model::BrowserRestoreFidelity::HistoryAndScroll,
    })
}

#[test]
fn archived_worktree_pr_requires_exact_branch_and_head() {
    let head = GitHubRepoKey::new("github.com", None, "acme", "repo").unwrap();
    let record = WorktreeRecord {
        id: WorktreeId::new(),
        path: PathBuf::from("/repo/worktrees/old"),
        created_at_unix_ms: None,
        head_oid: Some("abc".into()),
        branch: Some("refs/heads/old-feature".into()),
        detached: false,
        bare: false,
        lock: Default::default(),
        prunable_reason: None,
    };
    let pr: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
        "number": 7, "title": "Old feature", "state": "MERGED",
        "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},
        "headRefName": "old-feature", "headRefOid": "abc", "baseRefName": "main",
        "updatedAt": "2026-01-01T00:00:00Z", "url": "https://github.com/acme/repo/pull/7"
    }))
    .unwrap();
    assert_eq!(
        matching_attached_pr(&record, &head, std::slice::from_ref(&pr)).map(|pr| pr.number),
        Some(7)
    );
    let mut duplicate = pr.clone();
    duplicate.number = 8;
    assert!(matching_attached_pr(&record, &head, &[pr.clone(), duplicate]).is_none());
    assert!(
        matching_attached_pr(
            &WorktreeRecord {
                head_oid: Some("def".into()),
                ..record.clone()
            },
            &head,
            std::slice::from_ref(&pr)
        )
        .is_none()
    );
    assert!(
        matching_attached_pr(
            &WorktreeRecord {
                branch: Some("refs/heads/other".into()),
                ..record.clone()
            },
            &head,
            std::slice::from_ref(&pr)
        )
        .is_none()
    );
    assert!(
        matching_attached_pr(
            &WorktreeRecord {
                detached: true,
                ..record
            },
            &head,
            &[pr]
        )
        .is_none()
    );
}

#[test]
fn attached_pr_overrides_reused_branch_without_hiding_unpushed_open_work() {
    let project_id = ProjectId::new();
    let record = WorktreeRecord {
        id: WorktreeId::new(),
        path: "/repo/feature".into(),
        created_at_unix_ms: None,
        head_oid: Some("old".into()),
        branch: Some("refs/heads/feature".into()),
        detached: false,
        bare: false,
        lock: Default::default(),
        prunable_reason: None,
    };
    let open: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
        "number": 9, "title": "New", "state": "OPEN", "headRefName": "feature", "headRefOid": "new",
        "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},
        "baseRefName": "main", "updatedAt": "2026-09-01", "url": "https://github.com/acme/repo/pull/9"
    })).unwrap();
    let merged: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
        "number": 7, "title": "Old", "state": "MERGED", "headRefName": "feature", "headRefOid": "old",
        "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},
        "baseRefName": "main", "updatedAt": "2026-01-01", "url": "https://github.com/acme/repo/pull/7"
    })).unwrap();
    let make_project = |record: &WorktreeRecord| {
        let mut row = SidebarWorktree::from_repository(record, None);
        row.head_repository = open.head_repository_key();
        SidebarProject::new(
            project_id,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![row],
            vec![SidebarPullRequest::from_github(project_id, &open, &[])],
        )
    };
    let mut project = make_project(&record);
    apply_attached_worktree_prs(
        &mut project,
        &[(record.clone(), None)],
        std::slice::from_ref(&open),
        &[merged],
    );
    assert_eq!(
        project.worktrees[0].pull_request_state,
        Some(sidebar::WorktreePullRequestState::Merged)
    );
    let unpushed = WorktreeRecord {
        head_oid: Some("local-only".into()),
        ..record
    };
    let mut project = make_project(&unpushed);
    apply_attached_worktree_prs(&mut project, &[(unpushed, None)], &[open], &[]);
    assert_eq!(
        project.worktrees[0].pull_request_state,
        Some(sidebar::WorktreePullRequestState::Open)
    );
}

#[test]
fn archived_pr_with_moved_tip_requires_ancestry() {
    let directory = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-qm",
        "first",
    ]);
    let pr_head = git(&["rev-parse", "HEAD"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-qm",
        "next",
    ]);
    let advanced = git(&["rev-parse", "HEAD"]);
    assert!(pr_head_is_ancestor(directory.path(), &pr_head, &advanced));
    git(&["checkout", "--orphan", "new-history"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-qm",
        "unrelated",
    ]);
    let unrelated = git(&["rev-parse", "HEAD"]);
    assert!(!pr_head_is_ancestor(directory.path(), &pr_head, &unrelated));
}

#[test]
fn pull_request_head_prefers_the_context_worktree() {
    let project_id = ProjectId::new();
    let selected = PathBuf::from("/repo/selected");
    let project = SidebarProject::new(
        project_id,
        RepositoryId::new(),
        "repo".into(),
        PathBuf::from("/repo"),
        vec![
            SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                PathBuf::from("/repo/first"),
                Some("first".into()),
                false,
                false,
                None,
                None,
                None,
            ),
            SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                selected.clone(),
                Some("selected".into()),
                false,
                false,
                None,
                None,
                None,
            ),
        ],
        Vec::new(),
    );

    assert_eq!(
        pull_request_head(std::slice::from_ref(&project), project_id, None),
        "",
        "a project-level create must not adopt the first worktree's branch"
    );
    assert_eq!(
        pull_request_head(std::slice::from_ref(&project), project_id, Some(&selected)),
        "selected"
    );
    assert_eq!(
        pull_request_head(&[project], project_id, Some(Path::new("/repo/missing"))),
        ""
    );
}

#[test]
fn a_proven_remote_default_is_the_branch_github_would_merge_into() {
    let base = crate::repository::ValidatedRefName::parse("refs/remotes/te/am/trunk").unwrap();
    assert_eq!(
        proven_pull_request_base(
            &sidebar::BranchChanges::Loading(base),
            &["te".into(), "te/am".into()],
        ),
        "trunk"
    );
    assert_eq!(
        proven_pull_request_base(&sidebar::BranchChanges::Unavailable("no base"), &[]),
        ""
    );
}

#[test]
fn opening_a_pull_request_does_not_guess_main_or_the_first_branch() {
    let mut app = context_inspector_test_app(false);
    let project_id = ProjectId::new();
    let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
    app.github_availability.insert(
        project_id,
        GitHubAvailability::Ready {
            repository_key: repository,
            pull_requests: Vec::new(),
            refreshed_at: 1,
        },
    );
    let selected = PathBuf::from("/repo/selected");
    *app.sidebar_model.projects = vec![SidebarProject::new(
        project_id,
        crate::repository::RepositoryId::new(),
        "repo".into(),
        PathBuf::from("/repo"),
        vec![
            SidebarWorktree::new(
                WorktreeId::new(),
                PathBuf::from("/repo/first"),
                Some("first".into()),
                false,
                false,
                None,
                None,
                None,
            ),
            SidebarWorktree::new(
                WorktreeId::new(),
                selected.clone(),
                Some("selected".into()),
                false,
                false,
                None,
                None,
                None,
            ),
        ],
        Vec::new(),
    )];

    let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
        project_id,
        worktree: None,
    }));
    let dialog = app.pull_request_dialog.as_ref().unwrap();
    assert_eq!(dialog.head, "");
    assert_eq!(dialog.base, "");
    assert!(dialog.worktree.is_none());

    let _ = app.update(Message::CancelDialog);
    let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
        project_id,
        worktree: Some(selected.clone()),
    }));
    let dialog = app.pull_request_dialog.as_ref().unwrap();
    assert_eq!(dialog.head, "selected");
    assert_eq!(dialog.base, "");
    assert_eq!(dialog.worktree.as_deref(), Some(selected.as_path()));
}

fn health_event(
    revision: u64,
    health: SessionSaveHealth,
    last_successful_flush_at_unix_ms: Option<i64>,
) -> SessionSaveHealthEvent {
    SessionSaveHealthEvent {
        revision,
        health,
        last_successful_flush_at_unix_ms,
        quarantined: Vec::new(),
    }
}

#[test]
fn save_health_timestamps_are_exact_utc_values() {
    assert_eq!(unix_time_label(0), "1970-01-01 00:00:00.000 UTC");
    assert_eq!(
        unix_time_label(946_684_800_123),
        "2000-01-01 00:00:00.123 UTC"
    );
}

#[test]
fn closed_health_channel_reports_unavailable_once_then_ends() {
    use cosmic::iced::futures::{StreamExt, executor::block_on};

    let (sender, receiver) = async_channel::unbounded();
    drop(sender);
    let source = SessionHealthSource(receiver);
    let mut stream = session_health_stream(&source);

    block_on(async {
        assert_eq!(
            stream.next().await,
            Some(SessionSaveHealthEvent {
                revision: u64::MAX,
                health: SessionSaveHealth::WriterUnavailable,
                last_successful_flush_at_unix_ms: None,
                quarantined: Vec::new(),
            })
        );
        assert_eq!(stream.next().await, None);
    });
}

#[test]
fn acknowledging_the_last_recovery_notice_releases_workspace_input() {
    for other_notice in [false, true] {
        let mut app = context_inspector_test_app(false);
        let mut health = SaveHealthState::new("/session.sqlite3".into());
        health.apply(health_event(
            1,
            SessionSaveHealth::Recovered {
                recovered_at_unix_ms: 1,
            },
            Some(1),
        ));
        app.save_health = Some(health);
        app.status = other_notice.then(|| TransientNotice::error("another failure"));
        let _ = app.update(Message::ChromeUi(chrome_ui::Action::ToggleNotices));
        assert!(app.chrome_ui.is_open());
        let _ = app.update(Message::AcknowledgeSessionRecovery);
        assert_eq!(app.chrome_ui.is_open(), other_notice);
        if !other_notice {
            assert!(!app.workspace_inputs_blocked());
        }
    }
}

#[test]
fn save_health_reducer_preserves_recovery_and_rejects_stale_completion() {
    let mut state = SaveHealthState::new(PathBuf::from("/exact/session.sqlite3"));
    assert!(!state.is_visible());
    assert!(state.apply(health_event(1, SessionSaveHealth::Healthy, Some(10))));
    assert_eq!(state.last_successful_flush_at_unix_ms, Some(10));

    assert!(state.apply(health_event(
        2,
        SessionSaveHealth::Retrying {
            category: crate::session::SaveFailureCategory::Busy,
            first_failed_at_unix_ms: 20,
            attempts: 1,
        },
        Some(10),
    )));
    assert!(state.is_visible());
    assert!(state.apply(health_event(
        3,
        SessionSaveHealth::RetryExhausted {
            category: crate::session::SaveFailureCategory::Busy,
            first_failed_at_unix_ms: 20,
            attempts: 4,
        },
        Some(10),
    )));

    assert!(!state.apply(health_event(
        2,
        SessionSaveHealth::Recovered {
            recovered_at_unix_ms: 25,
        },
        Some(25),
    )));
    assert!(matches!(
        state.health,
        SessionSaveHealth::RetryExhausted { .. }
    ));

    assert!(state.apply(health_event(
        4,
        SessionSaveHealth::Recovered {
            recovered_at_unix_ms: 30,
        },
        Some(30),
    )));
    assert!(state.apply(health_event(5, SessionSaveHealth::Healthy, Some(40))));
    assert!(matches!(state.health, SessionSaveHealth::Recovered { .. }));
    assert_eq!(state.last_successful_flush_at_unix_ms, Some(40));
    assert!(state.acknowledge_recovery());
    assert!(!state.is_visible());

    assert!(!state.apply(health_event(
        4,
        SessionSaveHealth::Retrying {
            category: crate::session::SaveFailureCategory::Database,
            first_failed_at_unix_ms: 30,
            attempts: 9,
        },
        None,
    )));
    assert!(state.apply(health_event(6, SessionSaveHealth::WriterUnavailable, None,)));
    assert_eq!(state.last_successful_flush_at_unix_ms, Some(40));
    assert!(state.is_visible());
}

#[test]
fn terminal_handoff_completion_retires_only_exact_successes_and_reports_only_failures() {
    let mut app = context_inspector_test_app(false);
    let terminal = || {
        let CardKind::Terminal(terminal) = terminal_card_state().kind else {
            unreachable!()
        };
        terminal
    };
    let accepted = terminal();
    let refused = terminal();
    let superseded_success = terminal();
    let superseded_failure = terminal();
    for terminal in [
        &accepted,
        &refused,
        &superseded_success,
        &superseded_failure,
    ] {
        app.pending_terminal_handoffs
            .insert(terminal.id, terminal.clone());
    }
    for terminal in [&superseded_success, &superseded_failure] {
        app.pending_terminal_handoffs
            .get_mut(&terminal.id)
            .unwrap()
            .resume_hint = Some("new descriptor".into());
    }
    app.terminal_handoff_commit_inflight = true;
    let _ = app.update(Message::TerminalHandoffsCommitted {
        result: Ok(vec![
            (accepted.clone(), Ok(())),
            (refused.clone(), Err("receipt refused".into())),
            (superseded_success.clone(), Ok(())),
            (superseded_failure.clone(), Err("obsolete failure".into())),
        ]),
    });
    assert!(!app.terminal_handoff_commit_inflight);
    assert!(!app.pending_terminal_handoffs.contains_key(&accepted.id));
    assert_eq!(app.pending_terminal_handoffs.len(), 3);
    let notices = app
        .notices
        .entries
        .iter()
        .map(|entry| entry.notice.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("1 terminal(s) remain pending")
                && notice.contains(&refused.id.to_string()))
    );
    assert!(
        notices
            .iter()
            .all(|notice| !notice.contains("obsolete failure"))
    );

    // A successful retry removes only the previously failed descriptor;
    // newer receipts that arrived during the batch still need a save.
    let _ = app.update(Message::TerminalHandoffsCommitted {
        result: Ok(vec![(refused.clone(), Ok(()))]),
    });
    assert!(!app.pending_terminal_handoffs.contains_key(&refused.id));
    assert_eq!(app.pending_terminal_handoffs.len(), 2);
}

#[test]
fn only_an_unsaved_return_to_terminal_is_reported_as_one() {
    const UNSAVED: &str = "Return to terminal is not saved";
    let flush_failed = || Message::TerminalHandoffsCommitted {
        result: Err("session flush: refused".into()),
    };
    let reports = |app: &Cagents| {
        app.notices
            .entries
            .iter()
            .filter(|entry| entry.notice.message == UNSAVED)
            .count()
    };

    // Every started terminal waits here too, and it returned from nothing.
    let mut app = context_inspector_test_app(false);
    let card = terminal_card_state();
    let CardKind::Terminal(terminal) = card.kind.clone() else {
        unreachable!()
    };
    Arc::make_mut(&mut app.durable).cards.push(card.clone());
    app.pending_terminal_handoffs
        .insert(terminal.id, terminal.clone());
    let _ = app.update(flush_failed());
    assert_eq!(reports(&app), 0);
    assert!(
        app.notices
            .entries
            .iter()
            .any(|entry| entry.notice.message.contains("session flush: refused"))
    );

    // The report names no pane or storage detail, and is withdrawn as soon
    // as a flush saves the return, even if the daemon refuses the receipt.
    app.unsaved_chat_returns.insert(terminal.id, card.id);
    let _ = app.update(flush_failed());
    assert_eq!(reports(&app), 1);
    let details = app
        .notices
        .entries
        .iter()
        .find(|entry| entry.notice.message == UNSAVED)
        .and_then(|entry| entry.notice.details.clone())
        .unwrap();
    assert!(!details.contains("flush") && !details.contains(&card.id.to_string()));
    let _ = app.update(Message::TerminalHandoffsCommitted {
        result: Ok(vec![(terminal.clone(), Err("receipt refused".into()))]),
    });
    assert_eq!(reports(&app), 0);
    assert!(app.unsaved_chat_returns.is_empty());

    // Closing the pane ends what the report was about.
    app.unsaved_chat_returns.insert(terminal.id, card.id);
    let _ = app.update(flush_failed());
    assert_eq!(reports(&app), 1);
    Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|state| state.id == card.id)
        .unwrap()
        .existence = Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("closed by user".into()),
        },
    };
    let _ = app.update(flush_failed());
    assert_eq!(reports(&app), 0);
}

#[test]
fn terminal_handoff_flush_failure_keeps_all_receipts_pending() {
    let mut app = context_inspector_test_app(false);
    let CardKind::Terminal(terminal) = terminal_card_state().kind else {
        unreachable!()
    };
    app.pending_terminal_handoffs
        .insert(terminal.id, terminal.clone());
    app.terminal_handoff_commit_inflight = true;
    let _ = app.update(Message::TerminalHandoffsCommitted {
        result: Err("session flush: unavailable".into()),
    });
    assert!(!app.terminal_handoff_commit_inflight);
    assert_eq!(
        app.pending_terminal_handoffs.get(&terminal.id),
        Some(&terminal)
    );
}

#[test]
fn refused_writes_keep_save_health_visible_beside_a_healthy_lane() {
    let refused = |subject| QuarantinedWrite {
        subject,
        category: crate::session::SaveFailureCategory::InvalidData,
        check: crate::diagnostics::Check::new(
            "chat card … changed conversation identity (identity)",
        ),
        refused_at_unix_ms: 20,
        held_back: 0,
    };
    let mut state = SaveHealthState::new(PathBuf::from("/exact/session.sqlite3"));
    let mut event = health_event(1, SessionSaveHealth::Healthy, Some(10));
    event.quarantined = vec![refused(WriteSubject::Project {
        id: ProjectId::new(),
        name: "poisoned".into(),
    })];
    assert!(state.apply(event));
    assert!(
        state.is_visible(),
        "a healthy lane still reports what it refused"
    );
    assert!(state.offers_retry());
    assert!(
        !state.window_state_refused(),
        "a refused catalog row leaves terminal handoffs free to commit"
    );
    assert_eq!(save_health_views(&state, |_| None, None).len(), 1);

    let mut event = health_event(
        2,
        SessionSaveHealth::RetryExhausted {
            category: crate::session::SaveFailureCategory::Busy,
            first_failed_at_unix_ms: 30,
            attempts: 4,
        },
        Some(10),
    );
    event.quarantined = vec![refused(WriteSubject::Workspace { card: None })];
    assert!(state.apply(event));
    assert!(state.window_state_refused());
    assert_eq!(
        save_health_views(&state, |_| None, None).len(),
        2,
        "the lane's failure and the refusal are separate notices"
    );

    state.retry_requested = true;
    assert!(!state.offers_retry(), "one retry at a time");
    assert!(state.apply(health_event(3, SessionSaveHealth::Healthy, Some(40))));
    assert!(
        !state.is_visible(),
        "a superseded refusal leaves nothing to show"
    );
    assert!(!state.offers_retry());

    let mut event = health_event(4, SessionSaveHealth::WriterUnavailable, Some(40));
    event.quarantined = vec![refused(WriteSubject::Workspace { card: None })];
    assert!(state.apply(event));
    assert!(
        !state.offers_retry(),
        "a stopped writer cannot answer a retry"
    );
}

#[test]
fn a_refused_card_is_named_with_its_remedy() {
    let card = CardId::new();
    let write = |subject| QuarantinedWrite {
        subject,
        category: crate::session::SaveFailureCategory::InvalidData,
        check: crate::diagnostics::Check::new(
            "chat card … changed conversation identity (identity)",
        ),
        refused_at_unix_ms: 20,
        held_back: 0,
    };
    let titled = |id| (id == card).then_some("build");
    let untitled = |_| None;

    let snapshot = write(WriteSubject::Workspace { card: Some(card) });
    let line = quarantined_write_line(&snapshot, &titled);
    assert!(
        line.starts_with("Window layout and panes (pane “build” was refused)"),
        "{line}"
    );
    assert!(line.contains("closing the pane clears it"), "{line}");
    let line = quarantined_write_line(&snapshot, &untitled);
    assert!(line.contains("(one pane was refused)"), "{line}");

    let pane = write(WriteSubject::Pane { card });
    let line = quarantined_write_line(&pane, &titled);
    assert!(
        line.starts_with("Latest changes to pane “build” (storage keeps its last saved state) —"),
        "{line}"
    );
    assert!(line.contains("closing the pane clears it"), "{line}");
    let line = quarantined_write_line(&pane, &untitled);
    assert!(line.starts_with("Latest changes to a pane ("), "{line}");
    assert!(line.contains("— saved state is invalid · since"), "{line}");
    assert!(
        line.ends_with("\nRule: chat card … changed conversation identity (identity)"),
        "the rule storage refused it for is named on its own line: {line}"
    );

    let relaunch = write(WriteSubject::CardIdentity {
        card,
        kind: ReplacedIdentity::Terminal,
    });
    let line = quarantined_write_line(&relaunch, &titled);
    assert!(line.starts_with("Relaunched terminal “build”"), "{line}");
    assert!(line.contains("closing the pane clears it"), "{line}");

    let line = quarantined_write_line(&write(WriteSubject::Workspace { card: None }), &titled);
    assert!(line.starts_with("Window layout and panes —"), "{line}");
    assert!(!line.contains("closing the pane"), "{line}");
}

fn launch_recipe(working_directory: impl Into<PathBuf>) -> LaunchRecipe {
    LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: Vec::new(),
        working_directory: working_directory.into(),
        environment: Default::default(),
    }
}

#[test]
fn first_pane_width_uses_current_empty_window_and_preserves_measured_panes() {
    // A free right edge keeps `WINDOW_RESIZE_BORDER` of the window from the
    // content, so the third case sits exactly at the narrow threshold.
    // Each pane carries its own inspector, so an open one takes nothing
    // from the window's width.
    for (window, seed, with_project, expected, measured) in [
        (1280.0, 1340.0, false, 1.0, 1020.0),
        (1600.0, 600.0, false, 0.5, 1340.0),
        (1368.0, 1340.0, false, 0.5, 1100.0),
        (1600.0, 1340.0, true, 0.5, 960.0),
        (1360.0, 1340.0, true, 1.0, 960.0),
        (2000.0, 600.0, true, 0.5, 1360.0),
        (900.0, 1340.0, true, 1.0, 640.0),
    ] {
        let mut app = context_inspector_test_app(false);
        app.settings.default_native_pane_fraction = 0.5;
        if with_project {
            app.sidebar_model = sidebar::tests::panel_model();
        }
        app.workspace.resize_viewport(seed);
        drop(app.update(Message::WindowResized(window, 900.0)));
        assert!(app.workspace.cards().is_empty());
        assert_eq!(app.window_width, window);
        assert_eq!(app.workspace.viewport_width(), seed);
        assert_eq!(app.sidebar_model.projects.is_empty(), !with_project);

        // This is a real insertion, not a full-width singleton rendering
        // assertion: the stored preference must survive adding a neighbour.
        drop(app.add_utility_card(CardKind::Settings));
        let first = app.workspace.focused().unwrap();
        assert_eq!(app.card_state(first).unwrap().width_fraction, expected);
        assert_eq!(
            app.workspace.card(first).unwrap().width,
            CardWidth::Fraction(expected)
        );
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(measured),
        }));
        drop(app.add_utility_card(CardKind::Accounts));
        assert_eq!(app.card_state(first).unwrap().width_fraction, expected);
        assert_eq!(
            app.workspace.card(first).unwrap().width,
            CardWidth::Fraction(expected)
        );
    }

    // Once mounted, the measured strip remains authoritative even if the
    // outer window has resized and its next native width event is pending.
    let mut app = context_inspector_test_app(true);
    let first = app.workspace.focused().unwrap();
    app.card_state_mut(first).unwrap().width_fraction = 0.375;
    app.workspace
        .set_card_width(first, CardWidth::Fraction(0.375))
        .unwrap();
    app.workspace.resize_viewport(600.0);
    drop(app.update(Message::WindowResized(2000.0, 900.0)));
    assert_eq!(app.workspace.viewport_width(), 600.0);
    drop(app.add_utility_card(CardKind::Settings));
    let second = app.workspace.focused().unwrap();
    assert_eq!(app.card_state(second).unwrap().width_fraction, 1.0);
    assert_eq!(app.card_state(first).unwrap().width_fraction, 0.375);
    assert_eq!(
        app.workspace.card(first).unwrap().width,
        CardWidth::Fraction(0.375)
    );
}

#[test]
fn new_pane_width_adapts_without_rewriting_existing_panes() {
    let mut app = context_inspector_test_app(true);
    let existing = app.workspace.focused().unwrap();
    let width = app.card_state(existing).unwrap().width_fraction;
    for (viewport, expected) in [(900.0, 1.0), (1100.0, 0.5), (1600.0, 0.5)] {
        app.workspace.resize_viewport(viewport);
        assert_eq!(app.new_pane_fraction(0.5), expected);
        drop(app.add_utility_card(CardKind::Settings));
        let added = app.workspace.focused().unwrap();
        assert_eq!(
            app.card_state(added).unwrap().width_fraction,
            app.new_pane_fraction(app.settings.default_native_pane_fraction)
        );
        assert_eq!(app.card_state(existing).unwrap().width_fraction, width);
    }
}

#[test]
fn browser_chrome_uses_one_sanitized_location_for_visible_and_accessible_text() {
    let kind =
        browser_kind("https://alice:password@EXAMPLE.test:443/private%20path?oauth=secret#token");
    let caption = card_kind_caption(&kind);
    assert_eq!(caption, "https://example.test/private%20path");

    let chrome = CardChrome::new(CardId::new(), "Browser", "Example", &caption);
    assert_eq!(chrome.caption, caption);
    assert_eq!(chrome.resize_name, "Resize Browser pane Example");
    assert_eq!(
        chrome.accessible_name(true),
        "Browser pane Example, https://example.test/private%20path, focused"
    );
    for presented in [chrome.caption.as_str(), chrome.accessible_name(true)] {
        assert!(!presented.contains("alice"));
        assert!(!presented.contains("password"));
        assert!(!presented.contains("oauth"));
        assert!(!presented.contains("secret"));
        assert!(!presented.contains("token"));
    }
}

#[test]
fn system_opener_failure_redaction_contract() {
    let raw = "https://alice:password@example.test:8443/private%20path?oauth=secret#access-token";
    let url = Url::parse(raw).unwrap();
    // Execute a stable interpreter, not a newly written executable whose
    // write descriptor a parallel test's fork may still hold (ETXTBSY).
    // The URL stays a separate argument; the failing opener receives its
    // authoritative contents even though presentation must redact them.
    let mut opener = tokio::process::Command::new("/bin/sh");
    opener
        .args([
            "-c",
            "[ \"$1\" = \"$EXPECTED_URI\" ] || exit 43; printf '%s\\n' \"$1\" >&2; exit 42",
            "test-opener",
        ])
        .env("EXPECTED_URI", raw);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime
        .block_on(open_system_url_with_opener(&url, opener))
        .unwrap_err();

    assert!(
        error.contains("https://example.test:8443/private%20path"),
        "{error}"
    );
    assert!(error.contains("system opener exited"), "{error}");
    assert!(error.contains("42"), "{error}");
    for sensitive in [
        "alice",
        "password",
        "oauth",
        "secret",
        "access-token",
        "?",
        "#",
    ] {
        assert!(!error.contains(sensitive), "leaked {sensitive:?}: {error}");
    }
}

#[cfg(feature = "integrated-browser")]
#[test]
fn reported_navigation_preserves_draft_until_submission() {
    let current = Url::parse("https://example.test/redirect").unwrap();
    let destination = Url::parse("https://destination.test/next").unwrap();
    let mut address = BrowserAddress::user_input(destination.to_string());
    address.report_location(&current);
    assert_eq!(address.as_str(), destination.as_str());
    assert_eq!(address.submit(), Ok(destination));
    address.report_location(&current);
    assert_eq!(address.submit(), Ok(current));
}

#[cfg(feature = "integrated-browser")]
#[test]
fn unchanged_sanitized_address_submits_the_authoritative_location() {
    let url = Url::parse("https://alice:password@example.test:8443/path?api_key=hunter2#fragment")
        .unwrap();
    let mut address = BrowserAddress::location(&url);
    assert_eq!(address.as_str(), "https://example.test:8443/path");
    assert_eq!(address.submit(), Ok(url.clone()));
    assert_eq!(address.as_str(), "https://example.test:8443/path");
}

#[cfg(feature = "integrated-browser")]
#[test]
fn submitted_user_input_keeps_navigation_authority_but_redacts_presentation() {
    let url = Url::parse("https://alice:password@example.test:8443/path?api_key=hunter2#fragment")
        .unwrap();
    let mut typed = BrowserAddress::user_input(url.as_str().to_owned());
    assert_eq!(typed.as_str(), url.as_str());
    assert_eq!(typed.submit(), Ok(url));
    assert_eq!(typed.as_str(), "https://example.test:8443/path");
    for sensitive in ["alice", "password", "api_key", "hunter2", "fragment"] {
        assert!(!typed.as_str().contains(sensitive));
    }

    let mut malformed =
        BrowserAddress::user_input("https://alice:password@example .test/path?token=secret".into());
    assert_eq!(malformed.submit(), Err(()));
    assert_eq!(
        malformed.as_str(),
        "https://alice:password@example .test/path?token=secret"
    );
}
#[test]
fn application_close_sequence_visits_every_unresolved_repository_card() {
    let origin = CardId::new();
    let first = CardId::new();
    let second = CardId::new();
    let third = CardId::new();
    let window = window::Id::unique();
    let mut close = DirtyRepositoryClose {
        card: first,
        remaining: VecDeque::from([second, third]),
        intent: DirtyCloseIntent::Application(window),
        focused: DirtyCloseControl::Save,
        restore_card: Some(origin),
    };
    assert_eq!(close.next_unresolved(|card| card == second), Some(second));
    assert_eq!(close.restore_card, Some(origin));
    assert_eq!(close.next_unresolved(|card| card == third), Some(third));
    assert_eq!(close.restore_card, Some(origin));
    assert_eq!(close.next_unresolved(|_| true), None);
    assert_eq!(close.intent, DirtyCloseIntent::Application(window));
    let replacement = CardId::new();
    assert_eq!(
        resolve_restore_card(Some(origin), Some(replacement), |card| card == origin),
        Some(origin)
    );
    assert_eq!(
        resolve_restore_card(Some(origin), Some(replacement), |_| false),
        Some(replacement)
    );
}

#[test]
fn popup_close_callbacks_and_requests_preserve_the_main_window() {
    let mut app = context_inspector_test_app(true);
    let main = window::Id::unique();
    let popup = window::Id::unique();
    app.core.set_main_window_id(Some(main));
    let order = app.workspace.order().collect::<Vec<_>>();
    let focused = app.workspace.focused();
    let activation_closed = app.activation_rx.is_closed();

    // The real libcosmic SurfaceClosed path calls this callback for
    // popups too; the raw event subscription can also report a close.
    if let Some(message) = app.on_close_requested(popup) {
        drop(app.update(message));
    }
    drop(app.update(Message::WindowCloseRequested(popup)));

    assert!(app.exit.is_none());
    assert!(app.dirty_repository_close.is_none());
    assert!(app.transient_work_close.is_none());
    assert_eq!(app.core.main_window_id(), Some(main));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(app.activation_rx.is_closed(), activation_closed);

    let close = app
        .on_close_requested(main)
        .expect("main close is admitted");
    drop(app.update(close));
    assert_eq!(
        app.exit,
        Some(ApplicationExit {
            closing: Some(main),
            servers_reaped: false,
        })
    );
    assert_eq!(app.core.main_window_id(), None);
}

#[test]
fn popup_close_preserves_unsaved_work_without_opening_application_confirmation() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let main = window::Id::unique();
    let popup = window::Id::unique();
    app.core.set_main_window_id(Some(main));
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );

    if let Some(message) = app.on_close_requested(popup) {
        drop(app.update(message));
    }
    drop(app.update(Message::WindowCloseRequested(popup)));

    assert!(app.exit.is_none());
    assert!(app.dirty_repository_close.is_none());
    assert!(app.transient_work_close.is_none());
    assert_eq!(app.core.main_window_id(), Some(main));
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
        if runtime.blocks_navigation())
    );

    let close = app
        .on_close_requested(main)
        .expect("main close is admitted");
    drop(app.update(close));
    assert!(app.exit.is_none());
    assert_eq!(app.core.main_window_id(), Some(main));
    let confirmation = app.dirty_repository_close.as_ref().unwrap();
    assert_eq!(confirmation.card, card);
    assert_eq!(confirmation.intent, DirtyCloseIntent::Application(main));
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
        if runtime.blocks_navigation())
    );
}

/// The window is the only part of an exit anyone sees, so it closes as
/// soon as the exit is committed; the language-server reap, the final
/// session flush and the process teardown all happen after it has gone.
#[test]
fn committed_exit_closes_the_window_before_the_process_exits() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));

    let _ = app.update(Message::WindowCloseRequested(window));

    let closing = Some(ApplicationExit {
        closing: Some(window),
        servers_reaped: false,
    });
    assert_eq!(app.exit, closing);
    // Released, so libcosmic's report of the closed window cannot end the
    // application under the reap the exit is still waiting on, and the
    // frames iced may draw before it removes the window must not panic.
    assert_eq!(app.core.main_window_id(), None);
    let _ = app.view_window(window);

    // That report arrives as another close request. It must not start the
    // exit again or raise a dialog in a window that no longer exists.
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );
    let _ = app.update(Message::WindowCloseRequested(window));
    assert!(app.dirty_repository_close.is_none());
    assert_eq!(app.exit, closing);
}

#[test]
fn prepared_pr_checkout_rechecks_original_branch_source_and_update_admission() {
    for changed in ["none", "branch", "source", "update"] {
        let (mut app, project) = pull_request_test_app(&[]);
        let (card, task) = app.add_pull_requests_card("/repo".into());
        drop(task);
        let mut loaded = crate::pull_request_review::tests::loaded(Vec::new(), Vec::new());
        loaded.pull_request.head_repository =
            Some(crate::github::GitHubHeadRepository { name: "r".into() });
        loaded.pull_request.head_repository_owner = Some(crate::github::GitHubActor {
            login: "o".into(),
            name: None,
            is_bot: false,
        });
        let mut review = crate::pull_request_review::tests::target(1);
        review.pull_request = Arc::new(loaded.pull_request.clone());
        let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
            panic!("review card")
        };
        drop(
            pane.review
                .update(pull_request_review::Action::Open(review)),
        );
        let generation = pane.review.selection_generation();
        drop(pane.review.update(pull_request_review::Action::Loaded {
            generation,
            load: 1,
            result: Ok(Arc::new(loaded.clone())),
        }));
        drop(pane.review.take_effects());
        drop(pane.review.update(pull_request_review::Action::Checkout));
        let effects = pane.review.take_effects();
        let [pull_request_review::Effect::Checkout(target)] = effects.as_slice() else {
            panic!("checkout effect")
        };
        let target = target.clone();
        assert_eq!(target.head_ref_name, "feature");
        assert_eq!(target.head_repository.as_ref().unwrap().owner, "o");
        if changed == "branch" {
            loaded.pull_request.head_ref_name = "renamed".into();
        }
        if changed == "source" {
            loaded
                .pull_request
                .head_repository_owner
                .as_mut()
                .unwrap()
                .login = "other".into();
        }
        drop(pane.review.update(pull_request_review::Action::Loaded {
            generation,
            load: 1,
            result: Ok(Arc::new(loaded)),
        }));
        // Only forge preparation is substituted. Deliver the original
        // completed message through the actual App guard and dialog path.
        let mut prepared = pull_request_review::checkout::prepare(
            &pull_request_review::CheckoutTarget::read_only(
                target.repository.clone(),
                target.number,
                target.head.clone(),
            ),
            &CancellationToken::new(),
        )
        .unwrap();
        prepared.branch_input = "origin/feature".into();
        prepared.base.clear();
        prepared.creation_consent = Some(
            crate::repository::PrWorktreeCreationConsent::new(
                target.head_repository.clone().unwrap(),
                crate::repository::ValidatedBranchName::parse(&target.head_ref_name).unwrap(),
                target.head.clone(),
                "origin".into(),
                true,
            )
            .unwrap(),
        );
        app.update_prepared = changed == "update";
        drop(app.update(Message::PullRequestCheckoutReady {
            card,
            project,
            generation,
            target,
            result: Ok(prepared),
        }));
        assert_eq!(
            app.worktree_dialog.is_some(),
            changed == "none",
            "{changed}"
        );
    }
}

#[test]
fn pending_pull_request_checkout_fences_actual_window_close_and_update_dispatch() {
    let mut app = context_inspector_test_app(false);
    let (card, task) = app.add_pull_requests_card("/fixture".into());
    drop(task);
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
        panic!("review card")
    };
    let ticket = pane.review.fixture_existing_checkout_pending();
    let generation = pane.review.selection_generation();
    for message in [Message::CloseCard(card), Message::CloseCardUnasked(card)] {
        app.notices.clear();
        drop(app.update(message));
        assert!(app.cards.contains_key(&card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.workspace.pending_close().is_none());
        assert!(
            app.notices
                .recent()
                .any(|entry| entry.notice.message.contains("checkout reconciliation"))
        );
    }
    app.workspace.request_close(card).unwrap();
    drop(app.update(Message::CommitClose(card)));
    assert!(app.cards.contains_key(&card));
    assert!(
        app.workspace.pending_close().is_none(),
        "a new accepted operation must retire an earlier close reservation without removing the card"
    );
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    assert!(
        app.window_update_blocker()
            .unwrap()
            .contains("checkout reconciliation")
    );
    drop(app.update_update_ui(update_ui::Action::StartApply));
    assert!(
        app.status
            .as_ref()
            .unwrap()
            .message
            .contains("checkout reconciliation")
    );
    drop(app.update(Message::WindowCloseRequested(window)));
    assert!(app.exit.is_none());
    assert_eq!(app.core.main_window_id(), Some(window));
    assert_eq!(app.transient_work_close, Some(window));
    assert!(app.cards.contains_key(&card));
    drop(app.update(Message::CancelTransientWorkClose));
    drop(app.update(Message::PullRequestReview {
        card,
        action: pull_request_review::Action::ExistingCheckout(
            pull_request_review::checkout_ui::Action::Finished {
                generation,
                ticket,
                result: Err("Reconciliation refused; checkout preserved.".into()),
            },
        ),
    }));
    assert!(app.window_close_transient_blocker().is_none());
    assert!(app.window_update_blocker().is_none());
}

#[test]
fn automation_draft_reopen_keeps_editor_responsive_without_foreground_refresh() {
    let mut app = context_inspector_test_app(false);
    app.automation_ui.update(crate::automation_ui::Action::New);
    app.automation_ui
        .update(crate::automation_ui::Action::Field(
            crate::automation_ui::Field::Name,
            "Retained draft".into(),
        ));
    assert!(app.automation_ui.draft_dirty());
    drop(app.open_automations());
    drop(
        app.update(Message::AutomationUi(crate::automation_ui::Action::Field(
            crate::automation_ui::Field::Name,
            String::new(),
        ))),
    );
    assert!(app.automation_ui.has_draft());
    assert!(
        !app.automation_ui.draft_dirty(),
        "the first edit after reopening must reach the editor"
    );
}

#[test]
fn automation_draft_window_close_keeps_work_and_restarts_close_after_discard() {
    let mut app = context_inspector_test_app(false);
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    app.automation_ui.update(crate::automation_ui::Action::New);
    app.automation_ui
        .update(crate::automation_ui::Action::Field(
            crate::automation_ui::Field::Name,
            "Unsaved automation".into(),
        ));
    drop(app.update(Message::WindowCloseRequested(window)));
    assert!(app.exit.is_none());
    assert_eq!(app.transient_work_close, Some(window));
    assert!(app.modal_is_open());
    assert!(app.dialog().is_some());
    drop(app.update(Message::CancelTransientWorkClose));
    assert!(app.automation_ui.draft_dirty());
    assert!(app.transient_work_close.is_none());
    drop(app.update(Message::WindowCloseRequested(window)));
    drop(app.update(Message::DiscardAutomationAndClose(
        app.automation_close_generation,
    )));
    assert!(!app.automation_ui.has_draft());
    // The discard schedules a fresh close request so device/repository/chat
    // blockers are checked again instead of entering exit directly.
    assert!(app.exit.is_none());
    drop(app.update(Message::WindowCloseRequested(window)));
    assert!(app.exit.is_some());
}
#[test]
fn automation_draft_window_close_rejects_discard_from_an_older_prompt() {
    let mut app = context_inspector_test_app(false);
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    app.automation_ui.update(crate::automation_ui::Action::New);
    app.automation_ui
        .update(crate::automation_ui::Action::Field(
            crate::automation_ui::Field::Name,
            "First draft".into(),
        ));
    drop(app.update(Message::WindowCloseRequested(window)));
    let old = app.automation_close_generation;
    drop(app.update(Message::CancelTransientWorkClose));
    assert!(app.automation_ui.discard_draft_before_close());
    app.automation_ui.update(crate::automation_ui::Action::New);
    app.automation_ui
        .update(crate::automation_ui::Action::Field(
            crate::automation_ui::Field::Name,
            "Second draft".into(),
        ));
    drop(app.update(Message::WindowCloseRequested(window)));
    assert_ne!(old, app.automation_close_generation);
    drop(app.update(Message::DiscardAutomationAndClose(old)));
    assert!(app.automation_ui.draft_dirty());
    assert_eq!(app.transient_work_close, Some(window));
    assert!(app.exit.is_none());
}
#[test]
fn automation_draft_modal_blocks_workspace_messages_and_keep_preserves_the_draft() {
    let mut app = context_inspector_test_app(false);
    app.automation_ui.update(crate::automation_ui::Action::New);
    app.automation_ui
        .update(crate::automation_ui::Action::Field(
            crate::automation_ui::Field::Name,
            "Draft".into(),
        ));
    drop(app.update(Message::AutomationUi(
        crate::automation_ui::Action::CancelEdit,
    )));
    assert!(app.modal_is_open());
    assert!(app.dialog().is_some());
    let before = app.pane_inspectors.visible().count();
    drop(app.update(Message::ToggleContextInspector));
    assert_eq!(app.pane_inspectors.visible().count(), before);
    let keep = app.automation_ui.keep_draft_action().unwrap();
    drop(app.update(Message::AutomationUi(keep)));
    assert!(!app.modal_is_open());
    assert!(app.automation_ui.draft_dirty());
}

/// Once the window is gone, an activation must be refused rather than
/// acknowledged, so `cagents ui` opens a new window instead of reporting
/// one that is already running.
#[test]
fn committed_exit_stops_accepting_activations() {
    let mut app = context_inspector_test_app(false);
    let (activations, receiver) = async_channel::unbounded();
    app.activation_rx = receiver;
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    assert!(!activations.is_closed());

    let _ = app.update(Message::WindowCloseRequested(window));

    assert!(activations.is_closed());
    assert!(
        activations
            .send_blocking(instance::UiActivation {
                token: None,
                session: None,
                update: None,
            })
            .is_err()
    );
}

/// Neither half of the exit may end the process alone: servers reaped
/// before the window is destroyed would tear iced down with the window
/// still on screen, and a destroyed window before the reap would take the
/// runtime away from servers still stopping.
#[test]
fn exit_waits_for_both_the_window_and_the_language_servers() {
    for window_first in [true, false] {
        let mut app = context_inspector_test_app(false);
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));
        let _ = app.update(Message::WindowCloseRequested(window));

        let (first, second) = if window_first {
            (
                Message::WindowClosed(window),
                Message::LanguageServersReaped,
            )
        } else {
            (
                Message::LanguageServersReaped,
                Message::WindowClosed(window),
            )
        };
        let _ = app.update(first);
        assert!(!app.exit.unwrap().is_complete());
        let _ = app.update(second);
        assert!(app.exit.unwrap().is_complete());
    }
}

/// Only the window the exit closed can finish it.
#[test]
fn another_window_closing_does_not_finish_the_exit() {
    let mut app = context_inspector_test_app(false);
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    let _ = app.update(Message::WindowCloseRequested(window));

    let _ = app.update(Message::LanguageServersReaped);
    let _ = app.update(Message::WindowClosed(window::Id::unique()));

    assert!(!app.exit.unwrap().is_complete());
}

/// iced reports every window it removes; outside an exit that is not half
/// of one.
#[test]
fn window_closed_outside_an_exit_is_ignored() {
    let mut app = context_inspector_test_app(false);
    let _ = app.update(Message::WindowClosed(window::Id::unique()));
    let _ = app.update(Message::LanguageServersReaped);
    assert_eq!(app.exit, None);
}

#[test]
fn unresolved_repository_keeps_the_window_open_on_close() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );

    let _ = app.update(Message::WindowCloseRequested(window));

    assert!(app.dirty_repository_close.is_some());
    assert_eq!(app.exit, None);
    assert_eq!(app.core.main_window_id(), Some(window));
}

#[test]
fn window_close_rejects_discard_for_post_persist_edit_back_fence() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    let runtime = RepositoryRuntime::post_persist_warning_fixture(card);
    assert!(runtime.is_dirty(), "the buffer has edited back from B to A");
    assert!(runtime.has_durability_warning());
    assert!(!runtime.can_abandon_unsaved_changes());
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));

    let _ = app.update(Message::WindowCloseRequested(window));
    let _ = app.update(Message::DiscardAndCloseRepository(card));

    assert!(app.dirty_repository_close.as_ref().is_some_and(
        |close| close.card == card && close.intent == DirtyCloseIntent::Application(window)
    ));
    assert!(app.workspace.order().any(|candidate| candidate == card));
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Repository(runtime))
            if runtime.blocks_navigation()
                && runtime.has_durability_warning()
                && !runtime.can_abandon_unsaved_changes()
    ));
}

#[test]
fn card_close_rejects_discard_for_post_persist_edit_back_fence() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let runtime = RepositoryRuntime::post_persist_warning_fixture(card);
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));

    let _ = app.update(Message::CloseCard(card));
    let _ = app.update(Message::DiscardAndCloseRepository(card));

    assert!(app.dirty_repository_close.as_ref().is_some_and(
        |close| close.card == card && close.intent == DirtyCloseIntent::Card(card)
    ));
    assert!(app.workspace.order().any(|candidate| candidate == card));
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Repository(runtime))
            if runtime.blocks_navigation() && runtime.has_durability_warning()
    ));
}

/// A headless `cosmic::Renderer`. With no graphics device it falls back
/// to tiny-skia, which needs neither a device nor a surface.
pub(super) fn headless_renderer() -> cosmic::Renderer {
    use cosmic::iced::advanced::renderer::Headless;
    use cosmic::iced::{Font, Pixels};

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("executor")
        .block_on(cosmic::Renderer::new(
            Font::default(),
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .expect("a headless renderer")
}

#[test]
fn real_tab_header_drag_survives_repaint_and_cannot_activate_destination_close() {
    use cosmic::iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree, operation::Focusable, tree::NAMED},
    };
    use cosmic::iced::{Event, Point, Rectangle, Size, mouse};
    struct Bounds(HashMap<String, Rectangle>);
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(&mut self, id: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
            if let Some(id) = id {
                self.0.insert(id.to_string(), bounds);
            }
        }
    }
    let mut app = context_inspector_test_app(true);
    let first = app.workspace.focused().unwrap();
    let mut second = app.card_state(first).unwrap().clone();
    second.id = CardId::new();
    second.title = "Second".into();
    let second_id = second.id;
    Arc::make_mut(&mut app.durable).cards.push(second);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(1600.0);
    app.cards.insert(second_id, CardRuntime::Loading);
    drop(app.update_chrome(chrome_ui::Action::HoverTab(Some(second_id))));
    assert!(!app.workspace_inputs_blocked());
    let renderer = headless_renderer();
    let mut tree = Tree::empty();
    let mut origin = Point::ORIGIN;
    let mut destination = Point::ORIGIN;
    let mut dropped = false;
    for phase in 0..3 {
        let mut element = frame(&app);
        NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
        tree.diff(element.as_widget_mut());
        iced::runtime::user_interface::restore_cached_named_states(&mut tree);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
        );
        NAMED.with(|named| named.borrow_mut().clear());
        if phase == 0 {
            let mut bounds = Bounds(HashMap::new());
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
            origin = bounds.0[&app.card_header_widget_id(first).to_string()].center();
            destination = bounds.0[&card_control_id(second_id, "close").to_string()].center();
        }
        let point = if phase == 0 { origin } else { destination };
        let event = Event::Mouse(match phase {
            0 => mouse::Event::ButtonPressed(mouse::Button::Left),
            1 => mouse::Event::CursorMoved { position: point },
            _ => mouse::Event::ButtonReleased(mouse::Button::Left),
        });
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(1600.0, 1200.0)),
        );
        drop(element);
        for message in messages {
            let cosmic::Action::App(message) = message else {
                continue;
            };
            assert!(
                !matches!(message, Message::CloseCard(_)),
                "a title drag must not reach the destination close button"
            );
            dropped |= matches!(message, Message::Viewport { event: WorkspaceViewportEvent::Reordered { card, before: None }, .. } if card == first);
            drop(app.update(message));
        }
    }
    assert!(
        dropped,
        "actual title/tooltip/header composition must intercept the drag"
    );
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        vec![second_id, first]
    );
}

/// The custom `widget::Id`s that name more than one node in `element`.
///
/// A custom id may name at most one node per frame: the pinned runtime
/// lifts every named node's state into one thread-local map keyed by name
/// before it diffs, gives it to the first widget that asks for that name,
/// and leaves any later claimant holding the `State::None` the lift left
/// behind. Nothing downstream can recover from that - the tag still
/// matches, so the node is never rebuilt, and the first draw downcasts a
/// stateless state and aborts the process.
///
/// Shared rather than written per surface, because every surface that
/// mounts ids from a list is exposed to the same invariant and a per-test
/// copy of the walk would drift from the runtime it is modelling.
pub(super) fn widget_id_counts(element: &mut Element<'_, Message>) -> HashMap<String, usize> {
    use cosmic::iced::advanced::widget::Tree;
    use iced_accessibility::id::Internal;

    fn tally(tree: &Tree, out: &mut HashMap<String, usize>) {
        if let Some(widget::Id(Internal::Custom(_, name))) = &tree.id {
            *out.entry(name.to_string()).or_default() += 1;
        }
        for child in &tree.children {
            tally(child, out);
        }
    }

    let mut counts = HashMap::new();
    let mut tree = Tree::new(element.as_widget_mut());
    // Responsive constructs its descendants during layout. Counting only
    // Tree::new silently misses those controls and duplicate named states.
    let renderer = headless_renderer();
    let _layout = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &cosmic::iced::advanced::layout::Limits::new(
            cosmic::iced::Size::ZERO,
            cosmic::iced::Size::new(1600.0, 1200.0),
        ),
    );
    tally(&tree, &mut counts);
    counts
}

fn duplicate_widget_ids(element: &mut Element<'_, Message>) -> Vec<String> {
    let mut repeated = widget_id_counts(element)
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    repeated.sort();
    repeated
}

/// Lays out and draws `frames` frames of `app` the way the pinned runtime
/// does - lifting every named node's state, diffing, laying out, drawing -
/// asserting before each that no custom `widget::Id` names two nodes, and
/// applying `between(app, frame)` once the frame is drawn so the next one
/// sees a transition. The abort a duplicate id causes lives in `draw`, so
/// inspecting the view alone proves nothing; this is the one place every
/// surface that can mount two subtrees at once is driven through it.
fn draw_frames(app: &mut Cagents, frames: usize, mut between: impl FnMut(&mut Cagents, usize)) {
    use cosmic::iced::advanced::widget::{Tree, tree};
    use cosmic::iced::advanced::{Layout, layout, mouse, renderer};
    use cosmic::iced::{Rectangle, Size};

    let renderer = headless_renderer();
    let bounds = Size::new(1600.0, 1000.0);
    let mut state = Tree::empty();
    for frame in 0..frames {
        {
            let repeated = duplicate_widget_ids(&mut app.view());
            assert!(
                repeated.is_empty(),
                "frame {frame} mounts {repeated:?} more than once",
            );
        }

        let mut element = app.view();
        tree::NAMED.with(|named| {
            *named.borrow_mut() = state.take_all_named();
        });
        state.diff(element.as_widget_mut());
        iced::runtime::user_interface::restore_cached_named_states(&mut state);
        let node = element.as_widget_mut().layout(
            &mut state,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        tree::NAMED.with(|named| named.borrow_mut().clear());
        element.as_widget().draw(
            &state,
            &mut headless_renderer(),
            &cosmic::theme::active(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &Rectangle::with_size(bounds),
        );
        drop(node);
        drop(element);

        between(app, frame);
    }
}

/// Each pane's inspector toggle carries an id the application focuses it
/// by, and it moves between the floating corner and the open
/// inspector's header; the source-control and file panels carry
/// `sidebar.*` ids and are drawn by one inspector at a time. The pinned
/// runtime lifts every named node's state into one thread-local map keyed
/// by name before it diffs, gives it to the first widget that asks for
/// that name, and leaves any later claimant holding the `State::None` the
/// lift left behind, which the first draw downcasts, aborting the
/// process. So this lays out and draws every frame of two panes opening,
/// trading source control and collapsing, rather than only inspecting
/// the view.
#[test]
fn toggling_pane_inspectors_leaves_every_widget_id_naming_one_node() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    app.context_sections
        .set_open(context_sections::Section::Workspace, true);
    // A second pane with an inspector of its own.
    drop(app.clone_focused_pane());
    let other = app.workspace.focused().unwrap();
    assert_ne!(card, other);
    draw_frames(&mut app, 7, |app, frame| match frame {
        0 => {
            let _ = app.update(Message::TogglePaneInspector(card));
        }
        1 => {
            let _ = app.update(Message::TogglePaneInspector(other));
        }
        2 => {
            app.settle_pane_inspectors();
            assert!(app.pane_shows_source_control(other));
            let _ = app.update(Message::FocusCard(card));
            assert!(app.pane_shows_source_control(card));
        }
        3 => {
            let _ = app.update(Message::TogglePaneInspector(card));
            assert!(
                !app.pane_inspectors.is_open(card) && app.pane_inspectors.is_visible(card),
                "the collapse leaves the panel mounted while the toggle waits to float"
            );
        }
        4 => app.settle_pane_inspectors(),
        _ => {
            let _ = app.update(Message::TogglePaneInspector(other));
        }
    });
}

/// The inspector's Files and Changes tabs mount the same file and source
/// control panels the rail mounts for the worktree it is bound to, with
/// the same `sidebar.*` ids on their controls. The rail and the inspector
/// are on screen together the whole time, so every tab switch is a frame
/// in which two subtrees could claim one id. Drawn rather than reasoned
/// about, because the abort is in `draw` and points at the wrong widget.
#[test]
fn inspector_sections_leave_every_widget_id_naming_one_node() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    app.open_pane_inspector_now(card);
    assert!(
        sidebar::bound_source_control(&app.sidebar_model, &app.sidebar_state).is_some(),
        "the fixture binds a worktree, or the panels draw their empty chrome and prove nothing"
    );
    use context_sections::{Action, Section, WorkspaceView};
    let steps = [
        Action::Show(WorkspaceView::Files),
        Action::Show(WorkspaceView::SourceControl),
        Action::Toggle(Section::Work),
        Action::Toggle(Section::Workspace),
        Action::Show(WorkspaceView::Files),
        Action::Toggle(Section::Work),
    ];
    draw_frames(&mut app, steps.len(), |app, frame| {
        let _ = app.update(Message::ContextSection(steps[frame]));
    });
}

/// Channel rows carry a custom `widget::Id` keyed on the channel, and they
/// now appear, disappear and change rank from a daemon edge rather than
/// from a local click. Every such frame must still leave each id naming one
/// node, or the pinned runtime aborts the process on the next draw.
///
/// Two ways this could go wrong are worth drawing rather than reasoning
/// about: a row arriving while another leaves in the same frame, and a
/// reorder that moves a row past one already mounted. Both are laid out and
/// drawn here, because the failure is a panic inside `draw` that inspecting
/// the view cannot see.
#[test]
fn shared_channel_edges_leave_every_channel_id_naming_one_node() {
    let mut app = context_inspector_test_app(true);
    let project = crate::model::ProjectId::new();
    let repository = std::path::PathBuf::from("/repo/.git");
    app.sidebar_model
        .projects
        .push(sidebar::SidebarProject::new(
            project,
            crate::repository::RepositoryId(uuid::Uuid::new_v4()),
            "repo".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));
    app.sidebar_state.reveal_project(project);
    let older = crate::model::ChannelId::new();
    let newer = crate::model::ChannelId::new();
    let identity = uuid::Uuid::new_v4();
    let shared = |id, title: &str, ordinal| crate::workspace_model::ModelChannel {
        title_source: Default::default(),
        id,
        repository: repository.clone(),
        title: title.to_owned(),
        ordinal,
    };

    draw_frames(&mut app, 4, |app, frame| {
        let model = |channels| crate::workspace_model::WorkspaceModel {
            navigation_orders: Vec::new(),
            identity,
            sequence: frame as u64 + 1,
            projects: Vec::new(),
            channels,
            closed_projects: Vec::new(),
            views: Vec::new(),
            agents: Vec::new(),
        };
        match frame {
            // One adopted row.
            0 => {
                let _ =
                    app.apply_workspace_model_update(workspace_model_sync::ModelUpdate::Connected(
                        model(vec![shared(older, "older", 1)]),
                    ));
            }
            // A second row arrives above the first, so the mounted row is
            // pushed down past a newly mounted one.
            1 => {
                let _ = app.apply_workspace_model_update(workspace_model_sync::ModelUpdate::Read(
                    model(vec![shared(older, "older", 1), shared(newer, "newer", 2)]),
                ));
            }
            // One leaves while the other is reranked past where it was, in
            // a single update.
            _ => {
                let _ = app.apply_workspace_model_update(workspace_model_sync::ModelUpdate::Read(
                    model(vec![shared(newer, "newer", 9)]),
                ));
            }
        }
        app.refresh_sidebar_channels();
    });

    // The rows really were mounted, or the walk above proved nothing.
    assert_eq!(
        app.sidebar_model.projects[0]
            .channels
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![newer],
        "the departed channel is gone and the survivor kept its identity"
    );
}

pub(super) fn settle_settings_task(app: &mut Cagents, task: Task<Message>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::SettingsSaved { .. },
                    )) = action
                    {
                        let _ = app.update(message);
                    }
                }
            }
        });
}

#[test]
fn a_window_view_saves_each_open_inspector_and_reads_one_from_before_them() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = context_inspector_test_app(true);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let card = app.workspace.focused().unwrap();
    app.open_pane_inspector_now(card);
    let view = app.capture_settings_save().unwrap().view;
    assert_eq!(view.open_inspectors, Some(vec![card]));
    assert!(
        view.context_inspector_open,
        "an older version reads the focused pane's as the window's"
    );

    assert_eq!(
        view.inspector_choices,
        Some(pane_inspector::Choices {
            opened: vec![card],
            closed: Vec::new(),
        })
    );

    let mut json = serde_json::to_value(&view).unwrap();
    let read: WindowView = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(read.open_inspectors, Some(vec![card]));
    assert_eq!(read.inspector_choices, view.inspector_choices);
    // A view saved while inspectors were closed until opened keeps no
    // choices; its open ones read back as opened.
    json.as_object_mut().unwrap().remove("inspector_choices");
    let unchosen: WindowView = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(unchosen.inspector_choices, None);
    assert_eq!(unchosen.open_inspectors, Some(vec![card]));
    // A view saved before each pane had its own inspector.
    json.as_object_mut().unwrap().remove("open_inspectors");
    let legacy: WindowView = serde_json::from_value(json).unwrap();
    assert_eq!(legacy.open_inspectors, None);
    let mut inspectors = pane_inspector::PaneInspectors::restore(
        legacy.open_inspectors,
        legacy.context_inspector_open,
    );
    inspectors.sync(
        &[card],
        Some(card),
        |_| pane_inspector::Availability::Available,
        |_| false,
        |_| true,
    );
    assert!(inspectors.is_open(card));
}

#[test]
fn out_of_order_settings_saves_and_acknowledgments_preserve_latest_edits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    app.settings.terminal_font_size = 18.0;
    let older = app.capture_settings_save().unwrap();
    app.settings.terminal_font_size = 20.0;
    let newer = app.capture_settings_save().unwrap();
    let newer_ack = newer.run().unwrap();
    let older_ack = older.run().unwrap();
    app.acknowledge_settings_save(newer_ack);
    app.settings.terminal_font_size = 22.0;
    app.acknowledge_settings_save(older_ack);
    assert_eq!(settings::load(&path).unwrap().terminal_font_size, 20.0);
    assert_eq!(app.settings.terminal_font_size, 22.0);
    app.save_settings_now().unwrap();
    assert_eq!(settings::load(&path).unwrap().terminal_font_size, 22.0);
}

#[test]
fn reverting_an_edit_before_save_acknowledgment_persists_the_revert() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    let original = app.settings.terminal_font_size;
    app.settings.terminal_font_size = 18.0;
    let first = app.capture_settings_save().unwrap();
    app.settings.terminal_font_size = original;
    let second = app.capture_settings_save().unwrap();
    let first_ack = first.run().unwrap();
    let second_ack = second.run().unwrap();
    app.acknowledge_settings_save(second_ack);
    app.acknowledge_settings_save(first_ack);
    assert_eq!(settings::load(&path).unwrap().terminal_font_size, original);
}

#[test]
fn settings_autosave_rejects_stale_debounce_and_flushes_each_valid_field() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    let field = SettingsField::TerminalFontFamily;
    let _ = app.queue_setting_input(field, "Fira".into());
    let stale = app.settings_generation;
    let _ = app.queue_setting_input(field, "Fira Code".into());
    let latest = app.settings_generation;
    let _ = app.autosave_setting(field, stale);
    assert!(!path.exists());
    assert!(app.settings.terminal_font_family.is_empty());
    let task = app.autosave_setting(field, latest);
    settle_settings_task(&mut app, task);
    assert_eq!(
        settings::load(&path).unwrap().terminal_font_family,
        "Fira Code"
    );
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    let pane = app.utility_card(&CardKind::Settings).unwrap();
    let _ = app.queue_setting_input(SettingsField::ManagedWorktreeDirectory, "../trees/".into());
    // Closing the pane commits the draft still waiting out its debounce.
    let _ = app.update(Message::CloseCard(pane));
    let task = app.update(Message::CommitClose(pane));
    assert!(app.settings_pending.is_empty());
    settle_settings_task(&mut app, task);
    assert_eq!(
        settings::load(&path).unwrap().managed_worktree_directory,
        "../trees/"
    );
}

#[test]
fn settings_releasing_a_conflict_retries_the_other_rows_refused_draft() {
    let mut app = context_inspector_test_app(false);
    let chord = app.settings.keyboard_shortcuts.command_palette[0].clone();
    let field = SettingsField::ShortcutToggleProjects;
    drop(app.apply_setting_input(field, chord.clone()));
    assert!(app.settings_invalid.contains_key(&field));
    drop(app.apply_setting_input(SettingsField::ShortcutCommandPalette, String::new()));
    assert!(app.settings_invalid.is_empty());
    assert_eq!(app.settings.keyboard_shortcuts.toggle_projects, [chord]);
    assert!(app.settings.keyboard_shortcuts.command_palette.is_empty());
}

#[test]
fn invalid_autosaved_draft_survives_dismissal_and_other_valid_edits() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    app.save_settings_now().unwrap();
    let before = std::fs::read(&path).unwrap();
    let field = SettingsField::ShortcutNewTerminal;
    let _ = app.queue_setting_input(field, "Ctrl+".into());
    let _ = app.autosave_setting(field, app.settings_generation);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let task = app.apply_setting_input(SettingsField::TerminalFontSize, "18".into());
    settle_settings_task(&mut app, task);
    assert!(matches!(
        app.settings_feedback,
        Some(SettingsFeedback::Invalid)
    ));
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    let pane = app.utility_card(&CardKind::Settings).unwrap();
    let _ = app.update(Message::CloseCard(pane));
    let _ = app.update(Message::CommitClose(pane));
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    assert_ne!(app.utility_card(&CardKind::Settings), Some(pane));
    assert_eq!(app.settings_draft.keyboard_shortcuts.new_terminal, "Ctrl+");
    assert_eq!(
        settings::load(&path).unwrap().keyboard_shortcuts,
        KeyboardShortcuts::default()
    );
    assert_eq!(settings::load(&path).unwrap().terminal_font_size, 18.0);
}

fn open_settings_panes(app: &Cagents) -> Vec<CardId> {
    app.durable
        .cards
        .iter()
        .filter(|card| card.existence.is_open() && card.kind == CardKind::Settings)
        .map(|card| card.id)
        .collect()
}

#[test]
fn settings_open_as_one_live_pane_that_reopening_focuses() {
    let mut app = context_inspector_test_app(true);
    let terminal = app.durable.cards[0].id;
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    let [pane] = open_settings_panes(&app)[..] else {
        panic!("opening settings adds exactly one pane");
    };
    assert_eq!(app.workspace.focused(), Some(pane));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [terminal, pane]);
    assert!(matches!(app.cards.get(&pane), Some(CardRuntime::Settings)));
    let state = app.card_state(pane).unwrap();
    assert_eq!(state.title, "Settings");
    assert_eq!(*app.residency(pane), Residency::Hot);
    assert!(
        app.lifecycle.state(pane).is_none(),
        "a settings pane never enters the hibernation budget"
    );
    // A pane, not a dialog: the terminal beside it keeps its keystrokes.
    assert!(!app.modal_is_open());
    assert!(!app.workspace_inputs_blocked());
    assert_eq!(
        app.card_keyboard_focus(pane).to_string(),
        crate::settings_widgets::search_id().to_string()
    );

    let _ = app.update(Message::FocusCard(terminal));
    assert_eq!(app.workspace.focused(), Some(terminal));
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    assert_eq!(open_settings_panes(&app), [pane]);
    assert_eq!(app.workspace.focused(), Some(pane));

    // Neither a clone nor a second request makes another copy.
    let _ = app.clone_focused_pane();
    assert_eq!(open_settings_panes(&app), [pane]);
    assert_eq!(app.workspace.order().count(), 2);
}

#[test]
fn leaving_the_settings_pane_commits_its_pending_drafts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(true);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    let terminal = app.durable.cards[0].id;
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    let _ = app.update(Message::SettingsDraftChanged {
        field: SettingsField::TerminalFontFamily,
        value: "Fira Code".into(),
    });
    assert!(!app.settings_pending.is_empty());
    // Unrelated work while the pane keeps focus leaves the draft pending.
    let _ = app.update(Message::SelectSettingsCategory(SettingsCategory::Terminal));
    assert!(!app.settings_pending.is_empty());

    let task = app.update(Message::FocusCard(terminal));
    assert!(app.settings_pending.is_empty());
    assert_eq!(app.settings.terminal_font_family, "Fira Code");
    settle_settings_task(&mut app, task);
    assert_eq!(
        settings::load(&path).unwrap().terminal_font_family,
        "Fira Code"
    );
}

#[test]
fn dropping_the_window_saves_settings_drafts_still_debouncing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    let _ = app.queue_setting_input(SettingsField::TerminalFontFamily, "Iosevka".into());
    drop(app);
    assert_eq!(
        settings::load(&path).unwrap().terminal_font_family,
        "Iosevka"
    );
}

#[test]
fn a_dialog_over_the_settings_pane_withholds_edits_behind_it() {
    let mut app = context_inspector_test_app(false);
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    app.settings_reset_confirm = true;
    let before = app.settings_draft.clone();
    let _ = app.update(Message::SettingsDraftChanged {
        field: SettingsField::TerminalFontFamily,
        value: "typed at the dialog".into(),
    });
    let _ = app.update(Message::SettingsSearchChanged("typed".into()));
    assert_eq!(app.settings_draft, before);
    assert!(app.settings_search.is_empty());
    assert!(app.settings_pending.is_empty());
}

#[test]
fn asking_for_settings_while_its_pane_closes_brings_it_back() {
    let mut app = context_inspector_test_app(true);
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    let closing = app.utility_card(&CardKind::Settings).unwrap();
    let _ = app.request_card_removal(closing);
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    assert_eq!(open_settings_panes(&app), [closing], "still mid-close");
    let _ = app.update(Message::CommitClose(closing));
    let [reopened] = open_settings_panes(&app)[..] else {
        panic!("the request survives the close");
    };
    assert_ne!(reopened, closing);
    assert_eq!(app.workspace.focused(), Some(reopened));
    assert!(app.reopen_utility.is_none());
}

#[test]
fn a_restored_snapshot_keeps_one_settings_pane() {
    let panes: Vec<_> = (0..2)
        .map(|_| {
            let mut pane = terminal_card_state();
            pane.kind = CardKind::Settings;
            pane
        })
        .collect();
    let (first, second) = (panes[0].id, panes[1].id);
    let mut state = WorkspaceState {
        focused: Some(second),
        cards: panes.into(),
        ..WorkspaceState::default()
    };
    retire_duplicate_utility_cards(&mut state);
    assert!(state.cards[0].existence.is_open());
    assert!(!state.cards[1].existence.is_open());
    assert_eq!(state.focused, Some(first));
}

#[test]
fn restored_settings_pane_is_live_without_a_lifecycle_slot() {
    let mut app = context_inspector_test_app(false);
    let mut pane = terminal_card_state();
    pane.kind = CardKind::Settings;
    pane.title = "Settings".into();
    let id = pane.id;
    app.durable = Arc::new(WorkspaceState {
        focused: Some(id),
        cards: vec![pane].into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    let _ = app.initialize_runtimes();
    assert!(matches!(app.cards.get(&id), Some(CardRuntime::Settings)));
    assert_eq!(*app.residency(id), Residency::Hot);
    assert!(app.lifecycle.state(id).is_none());
    // Retrying a pane that cannot fail only reinstalls it.
    app.cards.insert(id, CardRuntime::Failed("fixture".into()));
    let _ = app.update(Message::RetryCard(id));
    assert!(matches!(app.cards.get(&id), Some(CardRuntime::Settings)));
    assert!(app.lifecycle.state(id).is_none());
}

#[test]
fn corrupt_settings_protection_retains_save_error_and_reset_cancels_pending_drafts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    std::fs::write(&path, "not valid toml").unwrap();
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(path.clone());
    let task = app.apply_setting_input(SettingsField::TerminalFontSize, "18".into());
    settle_settings_task(&mut app, task);
    assert!(matches!(
        app.settings_feedback,
        Some(SettingsFeedback::Error(_))
    ));
    let _ = app.queue_setting_input(SettingsField::TerminalFontFamily, "Unfinished".into());
    let stale = app.settings_generation;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not valid toml");
    assert!(matches!(
        app.settings_feedback,
        Some(SettingsFeedback::Error(_))
    ));
    let task = app.reset_settings();
    settle_settings_task(&mut app, task);
    let _ = app.autosave_setting(SettingsField::TerminalFontFamily, stale);
    assert_eq!(settings::load(&path).unwrap(), AppSettings::default());
}

#[test]
fn resetting_settings_sends_restored_notification_preferences_to_the_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    app.settings.notifications.enabled = false;
    app.settings.notifications.while_focused = true;
    assert_eq!(app.notification_ui.pending_preferences(), None);

    let task = app.reset_settings();
    settle_settings_task(&mut app, task);

    // The daemon, not the pane, decides which alerts arrive; a reset that
    // only redraws the toggles leaves it applying the old policy.
    assert_eq!(
        app.notification_ui.pending_preferences(),
        Some(crate::notifications::Preferences::default())
    );
}

#[test]
fn resetting_unchanged_notification_preferences_sends_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    app.settings.terminal_font_size = 20.0;

    let task = app.reset_settings();
    settle_settings_task(&mut app, task);

    assert_eq!(app.notification_ui.pending_preferences(), None);
    assert_eq!(app.settings, AppSettings::default());
}

/// A palette or font family change drops comparisons styled the old
/// way; whichever settings path makes it must request them again, or
/// the diff on screen reads "Nothing to compare".
#[test]
fn presentation_settings_request_the_comparison_on_screen_again() {
    let fields = [
        (SettingsField::EditorSyntaxTheme, "mocha"),
        (SettingsField::EditorFontFamily, "DejaVu Serif"),
    ];
    for (field, value) in fields {
        let mut app = context_inspector_test_app(false);
        let card = CardId::new();
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::shown_diff_fixture(card))),
        );
        let CardRuntime::Repository(runtime) = &app.cards[&card] else {
            unreachable!()
        };
        assert!(
            !runtime.requests_shown_diff(),
            "already requested: {field:?}"
        );
        drop(app.apply_setting_input(field, value.into()));
        let CardRuntime::Repository(runtime) = &app.cards[&card] else {
            unreachable!()
        };
        assert!(runtime.requests_shown_diff(), "{field:?}");
    }
}

#[test]
fn resetting_settings_restores_autosave_for_an_already_open_editor() {
    if crate::repository_widget::retained_save_tests::isolated_test(
        "app::tests::resetting_settings_restores_autosave_for_an_already_open_editor",
    ) {
        return;
    }

    #[derive(Debug, Default)]
    struct SaveDiagnostics {
        auto_save_due_delivered: usize,
        events: Vec<String>,
    }

    fn editor_state(app: &Cagents, card: CardId) -> String {
        let editor = match app.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) => {
                let context = runtime.inspector_context();
                format!(
                    "document={:?}, document_error={:?}, text={:?}, dirty={}, saving={}, save_error={:?}, editor_recovery={}, blocks_navigation={}",
                    context.document,
                    context.document_error,
                    runtime.editor_text(),
                    runtime.is_dirty(),
                    runtime.is_saving(),
                    runtime.save_error(),
                    runtime.has_editor_recovery(),
                    runtime.blocks_navigation(),
                )
            }
            Some(_) => "non-repository runtime".into(),
            None => "runtime missing".into(),
        };
        format!(
            "{editor}, window_custody_pending={}",
            app.operation_recovery_pending(),
        )
    }

    impl SaveDiagnostics {
        fn before_message(&mut self, app: &Cagents, card: CardId, phase: &str, message: &Message) {
            let detail = match message {
                Message::Repository(RepositoryAction::FileLoadedAt { identity, result }) => {
                    let disposition = match result {
                        Err(error) => format!("Err({error:?})"),
                        Ok(read) => {
                            let same_worktree = read.worktree() == identity.worktree;
                            // Inspect a clone of this fixture's actual read
                            // result; leave the original message/root intact.
                            // Cloning performs no new file read or admission.
                            let (_, loaded) = read.clone().into_parts();
                            let content = match loaded {
                                Err(error) => format!("inner Err({error:?})"),
                                Ok(loaded) => match loaded.content {
                                    crate::editor::FileContent::Editable { text, .. } => {
                                        format!("Editable {{ bytes: {} }}", text.len())
                                    }
                                    crate::editor::FileContent::Image { width, height, .. } => {
                                        format!("Image {{ width: {width}, height: {height} }}")
                                    }
                                    crate::editor::FileContent::Pdf { pages, .. } => {
                                        format!("Pdf {{ pages: {pages} }}")
                                    }
                                    crate::editor::FileContent::ReadOnly(reason) => {
                                        format!("ReadOnly({reason:?})")
                                    }
                                },
                            };
                            format!(
                                "Ok(FileRead {{ same_worktree: {same_worktree}, content: {content} }})"
                            )
                        }
                    };
                    Some(format!(
                        "FileLoadedAt card_matches={}, generation={}, disposition={disposition}",
                        identity.card == card,
                        identity.generation,
                    ))
                }
                Message::Repository(RepositoryAction::AutoSaveDue {
                    identity,
                    revision,
                    ticket,
                }) => {
                    self.auto_save_due_delivered += 1;
                    Some(format!(
                        "AutoSaveDue delivered={} card_matches={}, generation={}, revision={revision}, ticket={ticket}",
                        self.auto_save_due_delivered,
                        identity.card == card,
                        identity.generation,
                    ))
                }
                Message::Repository(RepositoryAction::RetainedSaveFinished {
                    identity,
                    ticket,
                    revision,
                    saved_text,
                    result,
                }) => {
                    // Match before the runtime flattens retained errors.
                    // Never Debug-print a live recovery authority/owner.
                    let disposition = match result {
                        Ok(outcome) => format!(
                            "Ok({:?}), recovery_present={}",
                            outcome.outcome(),
                            outcome.recovery_authority().is_some(),
                        ),
                        Err(crate::editor::retained_save::RetainedSaveError::Io(error)) => {
                            format!("Err(Io({error:?})), recovery_present=false")
                        }
                        Err(crate::editor::retained_save::RetainedSaveError::Recovery {
                            message,
                            ..
                        }) => {
                            format!("Err(Recovery({message:?})), recovery_present=true")
                        }
                    };
                    Some(format!(
                        "RetainedSaveFinished card_matches={}, generation={}, ticket={ticket}, revision={revision}, saved_text={saved_text:?}, result={disposition}",
                        identity.card == card,
                        identity.generation,
                    ))
                }
                Message::SettingsSaved { result, .. } => Some(format!(
                    "SettingsSaved success={}, error={:?}",
                    result.is_ok(),
                    result.as_ref().err(),
                )),
                _ => None,
            };
            if let Some(detail) = detail {
                self.events.push(format!(
                    "{phase}: before consumption: {detail}; {}",
                    editor_state(app, card),
                ));
            }
        }

        fn checkpoint(&mut self, app: &Cagents, card: CardId, phase: &str) {
            self.events
                .push(format!("{phase}: {}", editor_state(app, card)));
        }
    }

    async fn settle_repository_tasks(
        app: &mut Cagents,
        task: Task<Message>,
        diagnostics: &mut SaveDiagnostics,
        card: CardId,
        phase: &str,
    ) {
        let mut pending = VecDeque::from([task]);
        while let Some(task) = pending.pop_front() {
            let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                continue;
            };
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(
                    message @ (Message::Repository(_) | Message::SettingsSaved { .. }),
                )) = action
                {
                    diagnostics.before_message(app, card, phase, &message);
                    pending.push_back(app.update(message));
                    diagnostics.checkpoint(app, card, "after message consumption");
                }
            }
        }
        diagnostics.checkpoint(app, card, phase);
    }

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("notes.txt");
            std::fs::write(&path, "").unwrap();
            let mut app = context_inspector_test_app(false);
            app.settings_path = Some(directory.path().join("settings.toml"));
            app.settings_write_blocked = false;
            let card = CardId::new();
            let mut diagnostics = SaveDiagnostics::default();
            app.cards.insert(
                card,
                CardRuntime::Repository(Box::new(RepositoryRuntime::file_list_fixture(
                    RepositoryView {
                        selected_file: None,
                        project: ProjectId::new(),
                        repository: directory.path().to_path_buf(),
                        worktree: directory.path().to_path_buf(),
                    },
                    &["notes.txt"],
                ))),
            );
            let task = app.update(Message::Repository(RepositoryAction::OpenFile {
                card,
                path: PathBuf::from("notes.txt"),
            }));
            settle_repository_tasks(&mut app, task, &mut diagnostics, card, "after open").await;

            let task = app.apply_setting_input(SettingsField::EditorAutoSave, "false".into());
            settle_repository_tasks(
                &mut app,
                task,
                &mut diagnostics,
                card,
                "after disabling autosave",
            )
            .await;
            let task = app.update(Message::Repository(RepositoryAction::EditorAction {
                card,
                action: cosmic::widget::text_editor::Action::Edit(
                    cosmic::widget::text_editor::Edit::Insert('a'),
                ),
            }));
            settle_repository_tasks(&mut app, task, &mut diagnostics, card, "after typing a").await;
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "",
                "save diagnostics: {diagnostics:#?}",
            );

            // Reset schedules the dirty buffer without requiring another edit.
            let task = app.reset_settings();
            settle_repository_tasks(&mut app, task, &mut diagnostics, card, "after reset").await;
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "a",
                "save diagnostics: {diagnostics:#?}",
            );
            assert!(
                settings::load(app.settings_path.as_ref().unwrap())
                    .unwrap()
                    .editor_auto_save,
                "save diagnostics: {diagnostics:#?}",
            );

            // The same open editor must keep saving subsequent changes.
            let task = app.update(Message::Repository(RepositoryAction::EditorAction {
                card,
                action: cosmic::widget::text_editor::Action::Edit(
                    cosmic::widget::text_editor::Edit::Insert('b'),
                ),
            }));
            settle_repository_tasks(&mut app, task, &mut diagnostics, card, "after typing b").await;
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "ab",
                "save diagnostics: {diagnostics:#?}",
            );
        });
}

#[test]
fn modal_backdrop_dismisses_without_leaking_clicks_or_dismissing_surface_padding() {
    use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
    use cosmic::iced::{Event, Point, Rectangle, Size};
    let renderer = headless_renderer();
    for (point, enabled, dismisses) in [
        (Point::new(4.0, 4.0), true, true),
        (Point::new(400.0, 300.0), true, false),
        (Point::new(4.0, 4.0), false, false),
    ] {
        let underlying: Element<'_, Message> = widget::mouse_area(
            widget::container(iced::widget::Space::new())
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Message::NewTerminal)
        .into();
        let modal = dialog_ui::overlay(
            dialog_ui::surface(iced::widget::Space::new().height(120), 320.0),
            enabled.then_some(Message::CancelDialog),
            1.0,
        );
        let mut element: Element<'_, Message> = crate::interaction::layer(underlying, modal);
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(800.0, 600.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
        drop(shell);
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::CancelDialog))
                .count(),
            usize::from(dismisses)
        );
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::NewTerminal))
        );
    }
}

/// A key names one operation on one object: raising it again replaces
/// its report, a success replaces its failure, and resolving it withdraws
/// the report without touching anything else.
#[test]
fn keyed_notices_replace_and_resolve_their_own_report() {
    let card = CardId::new();
    let key = NoticeKey::ChatAttach(card);
    let mut log = NoticeLog::default();
    log.record(TransientNotice::warning("unrelated"));
    log.record(
        TransientNotice::error("Could not reopen “Fix parser”.")
            .with_details("Exact native chat not found")
            .with_key(key.clone()),
    );
    let first = log.recent().next().unwrap().id;
    log.record(
        TransientNotice::error("Could not reopen “Fix parser”.")
            .with_details("daemon refused")
            .with_key(key.clone()),
    );
    assert_eq!(log.len(), 2, "re-raising a key replaces its report");
    let replaced = log.recent().next().unwrap();
    assert_eq!(replaced.notice.details.as_deref(), Some("daemon refused"));
    assert_ne!(replaced.id, first, "a Dismiss for the old report is stale");
    // An identical report under the same key is a repeat, as unkeyed.
    log.record(
        TransientNotice::error("Could not reopen “Fix parser”.")
            .with_details("daemon refused")
            .with_key(key.clone()),
    );
    assert_eq!(log.recent().next().unwrap().repeats, 2);

    // Another object's report under the same operation is its own.
    let other = NoticeKey::ChatAttach(CardId::new());
    log.record(TransientNotice::error("Could not reopen “Other”.").with_key(other.clone()));
    assert_eq!(log.len(), 3);

    log.record(TransientNotice::success("Reopened.").with_key(key.clone()));
    assert_eq!(log.len(), 3, "the success takes the failure's place");
    assert!(
        log.recent()
            .filter(|entry| entry.notice.key.as_ref() == Some(&key))
            .all(|entry| entry.notice.severity == NoticeSeverity::Success)
    );

    log.resolve(other);
    log.resolve(key);
    let remaining = log
        .recent()
        .map(|entry| entry.notice.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(remaining, ["unrelated"]);
}

/// Confirmations retire on their own; warnings and errors wait. With
/// nothing left to retire there is no deadline, so no timer runs.
#[test]
fn successes_expire_and_failures_remain() {
    let now = Instant::now();
    let mut log = NoticeLog::default();
    assert_eq!(log.next_expiry(), None);
    log.record_at(TransientNotice::error("Could not reopen"), now);
    log.record_at(TransientNotice::warning("May be stale"), now);
    assert_eq!(log.next_expiry(), None, "nothing here retires on a timer");
    log.record_at(TransientNotice::success("Account added."), now);
    let later = now + Duration::from_secs(2);
    log.record_at(TransientNotice::success("Copied."), later);
    assert_eq!(log.next_expiry(), Some(now + NOTICE_SUCCESS_LIFETIME));

    log.expire(now + NOTICE_SUCCESS_LIFETIME - Duration::from_millis(1));
    assert_eq!(log.len(), 4, "nothing retires early");
    log.expire(now + NOTICE_SUCCESS_LIFETIME);
    assert_eq!(log.len(), 3);
    assert_eq!(log.next_expiry(), Some(later + NOTICE_SUCCESS_LIFETIME));

    // A repeat of a confirmation is a fresh one, and waits afresh.
    log.record_at(
        TransientNotice::success("Copied."),
        later + Duration::from_secs(5),
    );
    log.expire(later + NOTICE_SUCCESS_LIFETIME);
    assert_eq!(log.len(), 3);

    log.expire(now + Duration::from_secs(3600));
    assert_eq!(log.len(), 2);
    assert_eq!(log.severity(), Some(NoticeSeverity::Error));
    assert_eq!(log.next_expiry(), None);
}

/// A burst of confirmations stays bounded without evicting a failure or a
/// warning that nothing has resolved.
#[test]
fn a_success_flood_preserves_unresolved_failures() {
    let now = Instant::now();
    let mut log = NoticeLog::default();
    log.record_at(TransientNotice::error("Still disconnected"), now);
    log.record_at(TransientNotice::warning("May be stale"), now);
    for index in 0..NOTICE_LOG_CAPACITY * 2 {
        log.record_at(TransientNotice::success(format!("Copied {index}.")), now);
        assert!(log.len() <= NOTICE_LOG_CAPACITY);
    }
    log.expire(now + NOTICE_SUCCESS_LIFETIME);
    let remaining = log
        .recent()
        .map(|entry| entry.notice.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(remaining, ["May be stale", "Still disconnected"]);
}

/// Overflow never takes the notice that caused it: that is what the
/// reader is being told now, whatever is already waiting.
#[test]
fn the_newest_notice_survives_a_full_log_of_worse_ones() {
    let mut log = NoticeLog::default();
    for index in 0..NOTICE_LOG_CAPACITY {
        log.record(TransientNotice::error(format!("error {index}")));
    }
    log.record(TransientNotice::warning("May be stale"));
    assert_eq!(log.len(), NOTICE_LOG_CAPACITY);
    let messages = log
        .recent()
        .map(|entry| entry.notice.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(messages[0], "May be stale");
    assert!(!messages.contains(&"error 0"), "the oldest error made room");
    assert!(messages.contains(&"error 1"));

    log.record(TransientNotice::success("Copied."));
    let messages = log
        .recent()
        .map(|entry| entry.notice.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(messages[0], "Copied.");
    // Once no longer newest, the warning is the lowest severity left.
    assert!(!messages.contains(&"May be stale"));
    assert!(messages.contains(&"error 1"));
}

/// A paused countdown neither retires anything nor arms a timer, and
/// resumes where it stopped: a confirmation gets its whole lifetime of
/// unpaused time, including one raised during the pause.
#[test]
fn a_paused_countdown_resumes_where_it_stopped() {
    let start = Instant::now();
    let at = |seconds| start + Duration::from_secs(seconds);
    let mut log = NoticeLog::default();
    log.record_at(TransientNotice::success("Account added."), start);
    assert_eq!(log.next_expiry(), Some(at(6)));

    log.pause(at(2));
    log.pause(at(3));
    assert_eq!(log.next_expiry(), None, "no timer runs while paused");
    log.record_at(TransientNotice::success("Copied."), at(4));
    log.expire(at(100));
    assert_eq!(log.len(), 2, "nothing retires while paused");

    // Paused from 2 s to 10 s: the first had 4 s left, the second all 6.
    log.resume(at(10));
    assert_eq!(log.next_expiry(), Some(at(14)));
    log.expire(at(14));
    assert_eq!(log.len(), 1);
    assert_eq!(log.next_expiry(), Some(at(16)));
    log.resume(at(20));
    assert_eq!(log.next_expiry(), Some(at(16)), "resuming twice is a no-op");
}

/// The log is what keeps a report from being overwritten by the next one,
/// and capacity is the only thing bounding it: the shell raises notices
/// from a per-poll path, so a repeating failure must not evict the history.
#[test]
fn the_notice_log_collapses_repeats_and_bounds_itself() {
    let mut log = NoticeLog::default();
    for message in ["first failure", "second failure"] {
        log.record(TransientNotice::error(message.to_owned()));
    }
    // An identical message collapses onto the entry it repeats rather than
    // adding a second one, and it does so wherever that entry already sits:
    // a poll raising two failures in turn alternates them, which is exactly
    // the case a tail-only comparison got wrong.
    for _ in 0..5 {
        log.record(TransientNotice::error("first failure"));
        log.record(TransientNotice::error("second failure"));
    }
    assert_eq!(log.len(), 2);
    let mut recent = log.recent();
    // The collapse moves the repeat to the front, so the newest entry is
    // whichever message was raised last.
    let newest = recent.next().unwrap();
    assert_eq!(newest.notice.message, "second failure");
    assert_eq!(newest.repeats, 6);
    assert_eq!(recent.next().unwrap().repeats, 6);
    drop(recent);
    assert_eq!(log.severity(), Some(NoticeSeverity::Error));

    // A dismissed entry names exactly one; a stale id is a no-op.
    let newest = log.recent().next().unwrap().id;
    log.dismiss(newest);
    log.dismiss(newest);
    assert_eq!(log.len(), 1);

    // Distinct messages past capacity drop the oldest of the lowest
    // severity, never grow, and never push out the unresolved error.
    for index in 0..NOTICE_LOG_CAPACITY * 2 {
        log.record(TransientNotice::warning(format!("failure {index}")));
    }
    assert_eq!(log.len(), NOTICE_LOG_CAPACITY);
    assert_eq!(log.severity(), Some(NoticeSeverity::Error));
    let messages = log
        .recent()
        .map(|entry| entry.notice.message.as_str())
        .collect::<Vec<_>>();
    assert!(messages.contains(&"first failure"));
    assert!(!messages.contains(&"failure 0"));
    assert_eq!(
        messages[0],
        format!("failure {}", NOTICE_LOG_CAPACITY * 2 - 1)
    );

    log.clear();
    assert!(log.is_empty());
    assert_eq!(log.severity(), None);
}

#[test]
fn empty_workspace_exposes_provider_status_on_real_account_controls() {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    let app = context_inspector_test_app(false);
    let renderer = headless_renderer();
    let mut element = app.view();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(1200.0, 800.0)),
    );
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    for provider in crate::account::Account::REPORTED {
        let expected = format!(
            "{} · {} · Manage accounts",
            provider.label(),
            app.account_ui.provider_status(provider)
        );
        assert!(
            nodes.root().iter().chain(nodes.children()).any(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some(expected.as_str())
            }),
            "provider identity and status must remain accessible: {expected}"
        );
    }
}

#[test]
fn modal_controls_remain_available_to_assistive_technology() {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    let renderer = headless_renderer();
    let mut element = dialog_ui::overlay(
        dialog_ui::surface(
            crate::ui_controls::button::quiet("Confirm").on_press(Message::CancelDialog),
            320.0,
        ),
        Some(Message::CancelDialog),
        1.0,
    );
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(800.0, 600.0)),
    );
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    assert!(
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .any(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some("Confirm")
            })
    );
}

#[test]
fn review_file_panel_control_remains_accessible_across_pane_widths() {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let mut pane = app.pull_request_pane(&crate::model::PullRequestView {
        repository: PathBuf::from("/repo"),
        selected_number: None,
        file_panel_width: None,
        file_panel_collapsed: None,
    });
    let renderer = headless_renderer();
    for (width, collapsed, label) in [
        (260.0, None, "Show review files"),
        (260.0, Some(false), "Hide review files"),
        (500.0, None, "Show review files"),
        (900.0, None, "Hide review files"),
    ] {
        pane.file_panel_collapsed = collapsed;
        drop(app.workspace.resize_viewport(width));
        let mut element = app.pull_request_card_view(card, &pane);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 600.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let control = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| {
                node.node().role() == iced_accessibility::accesskit::Role::Button
                    && node.node().label() == Some(label)
            })
            .unwrap_or_else(|| {
                panic!("the review panel toggle must remain named at width {width}")
            });
        assert!(
            control.node().bounds().unwrap().x1 <= f64::from(width),
            "the review panel toggle must remain visible at width {width}"
        );
    }
}

/// The frame the runtime actually builds: libcosmic composes the header,
/// the application's own view and any dialog into a single element, and
/// reconciles all of it against one tree.
///
/// Driving `Application::view` alone would miss the dialog entirely, and
/// driving `Application::dialog` alone would miss that a dialog *joins and
/// leaves* the popover that holds it - a membership change in the widest
/// list there is. libcosmic itself flags the hazard where it composes them
/// ("we have to use a popover even without a dialog to keep the tree from
/// changing"), and only stops the popover node from coming and going: the
/// popup beneath it still does.
fn frame(app: &Cagents) -> cosmic::Element<'_, cosmic::Action<Message>> {
    cosmic::ApplicationExt::view_main(app)
}

/// Presents one frame against a carried-over widget tree, exactly the way
/// `iced_runtime::UserInterface::build` does: the previous frame's named
/// widget states are lifted into the thread-local map that reconciliation
/// restores them from, then the tree is reconciled, laid out and drawn.
///
/// Reproduced here rather than driven through `UserInterface` because that
/// type keeps its reconciled tree private, and the tree is what these
/// tests are about.
fn present(
    app: &Cagents,
    renderer: &mut cosmic::Renderer,
    tree: &mut cosmic::iced::advanced::widget::Tree,
) {
    use cosmic::iced::advanced::widget::tree::NAMED;
    use cosmic::iced::advanced::{Layout, layout, mouse, renderer::Style};
    use cosmic::iced::{Rectangle, Size};

    let bounds = Size::new(1600.0, 1200.0);
    let mut element = frame(app);
    NAMED.with(|named| {
        *named.borrow_mut() = tree.take_all_named();
    });
    tree.diff(element.as_widget_mut());
    iced::runtime::user_interface::restore_cached_named_states(tree);
    let node =
        element
            .as_widget_mut()
            .layout(tree, renderer, &layout::Limits::new(Size::ZERO, bounds));
    NAMED.with(|named| named.borrow_mut().clear());
    element.as_widget().draw(
        tree,
        renderer,
        &cosmic::theme::active(),
        &Style::default(),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &Rectangle::with_size(bounds),
    );
}

/// The name a node was given, if it was given one.
fn node_name(tree: &cosmic::iced::advanced::widget::Tree) -> Option<String> {
    let rendered = format!("{:?}", tree.id.as_ref()?);
    // `Id`'s debug carries a per-process counter that differs between two
    // builds of the same frame; the name does not.
    rendered
        .split_once("Custom(")
        .and_then(|(_, rest)| rest.split_once('"'))
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name.to_owned())
}

/// A reconciled tree must describe the frame it was reconciled against.
///
/// Naming a widget - `widget::Id::new(some_string)` - opts it into
/// libcosmic's named-state mechanism: every build lifts such a node's
/// state out of the tree into a thread-local map and puts it back only for
/// the names the new frame still asks for. Surviving children are then
/// claimed by name *in place* and never repositioned, so a list whose
/// membership changes leaves the departed nodes stranded at their old
/// indices with their state gone. Whichever widget now occupies such a
/// slot is then laid out and drawn against a stranger's state - and a
/// button drawn against emptied state aborts the process from inside
/// `draw`, where nothing can catch it.
///
/// Comparing against a tree built fresh for the same frame is the whole
/// invariant. Both halves of it matter, and each catches a failure the
/// other cannot see: the names must match, because a stranded node is a
/// name in the wrong place; and the shape must match, because a list that
/// lost an *unnamed* widget keeps its state at the wrong index without
/// renaming anything, and every container zips its children against their
/// trees - so the tail of that row is silently not laid out, not drawn and
/// not clickable for the frame.
fn assert_tree_is_faithful(
    reconciled: &cosmic::iced::advanced::widget::Tree,
    fresh: &cosmic::iced::advanced::widget::Tree,
) {
    fn walk(
        reconciled: &cosmic::iced::advanced::widget::Tree,
        fresh: &cosmic::iced::advanced::widget::Tree,
        path: &mut Vec<usize>,
    ) {
        assert_eq!(
            node_name(reconciled),
            node_name(fresh),
            "at {path:?}: the reconciled tree names a different widget than the frame it was reconciled against"
        );
        assert_eq!(
            reconciled.children.len(),
            fresh.children.len(),
            "at {path:?} ({:?}): the reconciled tree holds a different number of children than the frame it was reconciled against, so its parent zips widgets against strangers' state",
            node_name(fresh)
        );
        for (index, (reconciled, fresh)) in reconciled
            .children
            .iter()
            .zip(fresh.children.iter())
            .enumerate()
        {
            path.push(index);
            walk(reconciled, fresh, path);
            path.pop();
        }
    }

    walk(reconciled, fresh, &mut Vec::new());
}

fn assert_frame_is_faithful(app: &Cagents, tree: &cosmic::iced::advanced::widget::Tree) {
    // Responsive widgets construct their children during layout. Compare
    // two presented frames so their complete widget trees are available.
    let mut fresh = cosmic::iced::advanced::widget::Tree::empty();
    present(app, &mut headless_renderer(), &mut fresh);
    assert_tree_is_faithful(tree, &fresh);
}

/// A repository card whose durable state and runtime both say repository,
/// with an unsaved buffer and a language server reporting on it - the
/// state a user is in while editing a file.
pub(super) fn editing_repository_app() -> (Cagents, CardId) {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().expect("a focused card");
    {
        let durable = Arc::make_mut(&mut app.durable);
        let state = durable
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .expect("the focused card is durable");
        state.kind = CardKind::Repository(RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        });
    }
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );
    let state = lsp_state(&mut app, card);
    state.attachment.document = Some(LspDocumentId(1));
    state.attachment.context.status = Some(crate::lsp::LspStatus::Ready);
    state.attachment.context.diagnostics = vec![diagnostic("first"), diagnostic("second")];
    state.attachment.diagnostics_version = Some(0);
    app.lsp_documents.insert(LspDocumentId(1), card);
    (app, card)
}

/// Own both the checkout and its private outside parent for every lazy Save.
struct CloseSaveFixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

impl CloseSaveFixture {
    fn install(app: &mut Cagents, card: CardId) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let path = checkout.join("notes.txt");
        std::fs::write(&path, "A\n").unwrap();
        let descriptor = RepositoryView {
            project: ProjectId::new(),
            repository: checkout.clone(),
            worktree: checkout,
            selected_file: None,
        };
        let mut runtime = RepositoryRuntime::new_directory(descriptor.clone());
        runtime.set_auto_save(false);
        let open = runtime.update(RepositoryAction::OpenFile {
            card,
            path: "notes.txt".into(),
        });
        assert!(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(runtime.settle_document_read(open)),
            "actual file read callback"
        );
        assert_eq!(runtime.editor_text().as_deref(), Some("A\n"));
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(descriptor);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        Self {
            _directory: directory,
            path,
        }
    }

    fn edit(app: &mut Cagents, card: CardId, text: &str) {
        let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
            panic!("repository remains installed");
        };
        for action in [
            repository_widget::EditorAction::SelectAll,
            repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Paste(Arc::new(
                text.to_owned(),
            ))),
        ] {
            drop(runtime.update(RepositoryAction::EditorAction { card, action }));
        }
        assert_eq!(runtime.editor_text().as_deref(), Some(text));
    }

    fn assert_saving(app: &Cagents, card: CardId) {
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
            if runtime.is_saving() && !runtime.can_abandon_unsaved_changes())
        );
        assert!(app.operation_recovery_pending());
    }

    fn post_persist_warning(&self, app: &mut Cagents, card: CardId) -> Message {
        Self::edit(app, card, "B\n");
        assert!(app.prepare_editor_save(card));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
            panic!("repository remains installed");
        };
        let task = repository_task(runtime.request_save_with_sync_warning_for_test(card));
        Self::assert_saving(app, card);
        let completed = task_app_message(task);
        assert!(matches!(&completed,
            Message::Repository(RepositoryAction::RetainedSaveFinished { result: Ok(outcome), .. })
                if matches!(outcome.outcome(), crate::editor::SaveOutcome::Saved {
                    durability: crate::editor::SaveDurability::Unconfirmed { .. }, ..
                }) && outcome.recovery_authority().is_none()));
        assert_eq!(std::fs::read_to_string(&self.path).unwrap(), "B\n");
        assert!(!app.operation_recovery_pending());
        // The callback must retain B as the disk baseline, while a newer
        // buffer A remains dirty and protected by the durability fence.
        Self::edit(app, card, "A\n");
        assert_eq!(std::fs::read_to_string(&self.path).unwrap(), "B\n");
        completed
    }

    fn failed_retry(&self, app: &mut Cagents, card: CardId) -> String {
        let task = app.update(Message::SaveAndCloseRepository(card));
        Self::assert_saving(app, card);
        assert_eq!(std::fs::read_to_string(&self.path).unwrap(), "B\n");
        std::fs::remove_file(&self.path).unwrap();
        let completed = task_app_message(task);
        let Message::Repository(RepositoryAction::RetainedSaveFinished {
            result: Err(error @ crate::editor::retained_save::RetainedSaveError::Io(_)),
            ..
        }) = &completed
        else {
            panic!("removing the target causes an actual pre-publication failure");
        };
        let error = error.to_string();
        assert!(!self.path.exists(), "failed Save must not publish A");
        assert!(!app.operation_recovery_pending());
        drop(app.update(completed));
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
            if !runtime.is_saving() && runtime.is_dirty() && runtime.blocks_navigation()
                && runtime.editor_text().as_deref() == Some("A\n")
                && runtime.save_error().is_some_and(|message|
                    message.contains(&error) && message.contains("replaced on disk")))
        );
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .unwrap();
        std::io::Write::write_all(&mut target, b"B\n").unwrap();
        drop(target);
        assert_eq!(std::fs::read_to_string(&self.path).unwrap(), "B\n");
        error
    }

    fn durable_retry(&self, app: &mut Cagents, card: CardId) -> Message {
        assert_eq!(std::fs::read_to_string(&self.path).unwrap(), "B\n");
        let task = app.update(Message::SaveAndCloseRepository(card));
        Self::assert_saving(app, card);
        let completed = task_app_message(task);
        assert!(matches!(&completed,
            Message::Repository(RepositoryAction::RetainedSaveFinished { result: Ok(outcome), .. })
                if matches!(outcome.outcome(), crate::editor::SaveOutcome::Saved {
                    durability: crate::editor::SaveDurability::Durable, ..
                }) && outcome.recovery_authority().is_none()));
        assert_eq!(std::fs::read_to_string(&self.path).unwrap(), "A\n");
        assert!(!app.operation_recovery_pending());
        completed
    }
}

#[tokio::test]
async fn reviewed_update_prepare_flushes_folder_editor_selection_before_handoff() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("window.sqlite3");
    let boot = crate::model::BootId::new("editor-update-test");
    let (writer, _) = SessionRuntime::open(&path, boot).unwrap();
    let store = writer.operation_store();
    let (mut app, card) = editing_repository_app();
    let (runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "saved");
    let expected = runtime.navigation().unwrap();
    app.card_state_mut(card).unwrap().kind = CardKind::Repository(runtime.descriptor.clone());
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    app.session = Some(writer);
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    let task = app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Prepare,
        response,
    }));
    let mut stream = iced::runtime::task::into_stream(task).expect("update flush");
    let mut flushed = false;
    while let Some(action) = stream.next().await {
        if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
            let finished = matches!(message, Message::UiUpdateFlushed { .. });
            drop(app.update(message));
            if finished {
                flushed = true;
                break;
            }
        }
    }
    assert!(flushed);
    assert_eq!(receiver.try_recv().unwrap(), "ok");
    let restored = store.load_workspace(app.durable.id, None).unwrap().unwrap();
    assert_eq!(restored.repository_navigation.get(&card), Some(&expected));
    assert!(
        matches!(&restored.cards.iter().find(|state| state.id == card).unwrap().kind, CardKind::Repository(view) if view.selected_file.is_none())
    );
    let (response, _) = std::sync::mpsc::sync_channel(1);
    drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Cancel,
        response,
    })));
    assert!(!app.update_prepared);
}

#[test]
fn window_snapshots_capture_editor_selection_without_changing_shared_recipe() {
    let (mut app, card) = editing_repository_app();
    let CardRuntime::Repository(runtime) = &app.cards[&card] else {
        panic!("editor");
    };
    let expected = runtime.navigation().unwrap();
    let descriptor = runtime.descriptor.clone();
    app.card_state_mut(card).unwrap().kind = CardKind::Repository(descriptor.clone());
    let saved = app.persistent_workspace();
    assert_eq!(saved.repository_navigation.get(&card), Some(&expected));
    let CardKind::Repository(view) = &saved
        .cards
        .iter()
        .find(|state| state.id == card)
        .unwrap()
        .kind
    else {
        panic!("repository");
    };
    assert!(
        view.selected_file.is_none(),
        "the shared pane remains a repository browser"
    );
    let mut second_window = (*saved).clone();
    second_window.id = crate::model::WorkspaceId::new();
    second_window
        .repository_navigation
        .get_mut(&card)
        .unwrap()
        .selected_file = Some("other.txt".into());
    assert_eq!(
        app.persistent_workspace().repository_navigation[&card],
        expected
    );
    let close = crate::model::Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: None,
        },
    };
    Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|state| state.id == card)
        .unwrap()
        .existence = close;
    assert!(
        !app.persistent_workspace()
            .repository_navigation
            .contains_key(&card)
    );
    assert!(!app.durable.repository_navigation.contains_key(&card));
    assert_eq!(
        app.released_repository_navigation.get(&card),
        Some(&expected),
        "the window keeps where the closed editor was"
    );
    // A shared view reopening the same pane attaches it there again.
    Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|state| state.id == card)
        .unwrap()
        .existence = crate::model::Existence::Open;
    app.cards.remove(&card);
    drop(app.attach_repository_pane(card, descriptor));
    assert!(!app.released_repository_navigation.contains_key(&card));
}

#[test]
fn reordered_panes_keep_runtime_focus_and_restore_the_saved_order() {
    let (mut app, editor) = editing_repository_app();
    let mut order = vec![editor];
    for _ in 0..2 {
        let card = terminal_card_state();
        order.push(card.id);
        app.workspace
            .add_card(
                WorkspaceCard::new(
                    card.id,
                    &card.title,
                    CardWidth::Fraction(card.width_fraction),
                )
                .unwrap(),
                order.len() - 1,
                false,
            )
            .unwrap();
        app.cards.insert(card.id, CardRuntime::Loading);
        Arc::make_mut(&mut app.durable).cards.push(card);
    }
    let runtime = match &app.cards[&editor] {
        CardRuntime::Repository(runtime) => &**runtime as *const RepositoryRuntime,
        _ => unreachable!(),
    };
    let event = WorkspaceViewportEvent::Reordered {
        card: editor,
        before: None,
    };
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation + 1,
        event,
    }));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event,
    }));
    let expected = vec![order[1], order[2], editor];
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), expected);
    assert_eq!(app.workspace.focused(), Some(editor));
    let CardRuntime::Repository(current) = &app.cards[&editor] else {
        panic!("editor retained")
    };
    assert_eq!(&**current as *const RepositoryRuntime, runtime);
    assert!(current.is_dirty());
    let saved: WorkspaceState =
        serde_json::from_slice(&serde_json::to_vec(&*app.persistent_workspace()).unwrap()).unwrap();
    let restored = Cagents::rebuild_workspace(&saved);
    assert_eq!(restored.order().collect::<Vec<_>>(), expected);
    assert_eq!(restored.focused(), Some(editor));
    drop(app.reorder_pane(editor, Some(CardId::new())));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), expected);
}

#[test]
fn card_display_title_preserves_plain_names_and_provider_status_semantics() {
    let mut app = context_inspector_test_app(false);
    let mut card = terminal_card_state();
    let mut codex = launch_recipe("/repo");
    codex.program = "codex".into();
    codex.arguments = vec![
        "resume".into(),
        "01234567-89ab-cdef-0123-456789abcdef".into(),
    ];
    let hint = crate::model::agent_recovery::hint_from_launch(&codex).unwrap();
    for via_hint in [false, true] {
        let CardKind::Terminal(session) = &mut card.kind else {
            unreachable!()
        };
        if via_hint {
            session.launch = launch_recipe("/repo");
            session.resume_hint = Some(hint.clone());
        } else {
            session.launch = codex.clone();
            session.resume_hint = None;
        }
        for (saved, expected) in [
            ("Fix parser", "Fix parser"),
            ("⠋ Fix parser", "Fix parser"),
            ("[ ! ] Action Required | Fix parser", "Fix parser"),
            ("[!] Action Required", "Codex"),
            ("", "Codex"),
        ] {
            card.title = saved.into();
            assert_eq!(app.card_display_title(&card), expected);
        }
    }
    let CardKind::Terminal(session) = &mut card.kind else {
        unreachable!()
    };
    session.launch = launch_recipe("/repo");
    session.resume_hint = None;
    for title in ["Fix parser", "⠋ Fix parser", "[!] Action Required", ""] {
        card.title = title.into();
        assert_eq!(app.card_display_title(&card), title);
    }
    app.chrome_ui
        .user_titles
        .insert(card.id, "⠋ My name".into());
    assert_eq!(app.card_display_title(&card), "⠋ My name");
}

#[test]
fn repository_tab_chrome_tracks_the_selected_file_and_restores_the_pane_name() {
    let (mut app, card) = editing_repository_app();
    app.card_state_mut(card).unwrap().title = "My editor".into();
    let (runtime, _) = RepositoryRuntime::open_fixture(card, "src/main.rs", "fn main() {}\n");
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    let _ = app.sync_chrome();

    let chrome = app.card_chrome.get(&card).unwrap();
    assert_eq!(chrome.title, "main.rs");
    assert_eq!(chrome.caption, "/repo/src/main.rs");
    assert!(chrome.accessible_name(true).contains("main.rs"));
    assert!(chrome.close_name.contains("main.rs"));
    assert_eq!(app.card_state(card).unwrap().title, "My editor");

    let _ = app.update(Message::Repository(RepositoryAction::OpenFile {
        card,
        path: PathBuf::from("other.txt"),
    }));
    let chrome = app.card_chrome.get(&card).unwrap();
    assert_eq!(chrome.title, "other.txt");
    assert_eq!(chrome.caption, "/repo/other.txt");
    assert!(chrome.accessible_name(false).contains("other.txt"));
    assert!(chrome.close_name.contains("other.txt"));

    let _ = app.update(Message::Repository(RepositoryAction::BackToFiles { card }));
    let chrome = app.card_chrome.get(&card).unwrap();
    assert_eq!(chrome.title, "My editor");
    assert_eq!(
        chrome.caption,
        card_kind_caption(&app.card_state(card).unwrap().kind)
    );
    assert!(chrome.close_name.contains("My editor"));
    assert_eq!(app.card_state(card).unwrap().title, "My editor");
}

/// Discarding retires the Discard and Save commands and offers Back to
/// files in their place. Before this was fixed that repaint aborted the
/// process: the contextual inspector's commands were named, so the two
/// departing buttons stranded their emptied nodes for the surviving
/// buttons to draw against.
#[test]
fn discarding_an_edit_repaints_the_window() {
    use cosmic::iced::advanced::widget::Tree;

    let (mut app, card) = editing_repository_app();
    let mut renderer = headless_renderer();
    let mut tree = Tree::empty();

    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);

    let _ = app.update(Message::Repository(RepositoryAction::DiscardEdits { card }));
    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);
}

/// The same reconciliation, over the transitions that move a repository
/// card between its surfaces. Each one changes which commands the
/// inspector offers, which is the condition that strands a named node.
#[test]
fn moving_between_repository_surfaces_repaints_the_window() {
    use cosmic::iced::advanced::widget::Tree;

    let (mut app, card) = editing_repository_app();
    let mut renderer = headless_renderer();
    let mut tree = Tree::empty();
    present(&app, &mut renderer, &mut tree);

    for action in [
        RepositoryAction::ApplySourceSurface { card, diff: true },
        RepositoryAction::ApplySourceSurface { card, diff: false },
        RepositoryAction::BackToFiles { card },
        RepositoryAction::DiscardEdits { card },
        RepositoryAction::RefreshFiles { card },
    ] {
        let _ = app.update(Message::Repository(action));
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);
    }
}

/// The dirty-close dialog's controls are named too, and the row they sit
/// in changes membership: Discard leaves while a save is in flight and
/// comes back when the save fails, which is exactly the transition a user
/// walks through by answering "Save and close" to a file the disk then
/// refuses.
///
/// The dialog itself joins and leaves the popover libcosmic composes the
/// frame from, so opening one is a membership change in the outermost list
/// there is - which is why this starts from a frame with no dialog on it.
#[test]
fn a_failed_save_repaints_the_dirty_close_dialog() {
    use cosmic::iced::advanced::widget::Tree;

    if crate::repository_widget::retained_save_tests::isolated_test(
        "app::tests::a_failed_save_repaints_the_dirty_close_dialog",
    ) {
        return;
    }
    let (mut app, card) = editing_repository_app();
    let fixture = CloseSaveFixture::install(&mut app, card);
    CloseSaveFixture::edit(&mut app, card, "unsaved\n");
    let mut renderer = headless_renderer();
    let mut tree = Tree::empty();

    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);

    let _ = app.update(Message::CloseCard(card));
    assert!(
        app.dirty_repository_close.is_some(),
        "closing a card with an unsaved buffer asks before destroying it"
    );
    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);

    // The save starts: Discard leaves, because there is nothing coherent
    // to abandon while a write is in flight.
    let task = app.update(Message::SaveAndCloseRepository(card));
    CloseSaveFixture::assert_saving(&app, card);
    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);

    // The save fails: the buffer is dirty again and Discard comes back,
    // in the middle of the row rather than at its end.
    std::fs::remove_file(&fixture.path).unwrap();
    let failed = task_app_message(task);
    assert!(matches!(
        &failed,
        Message::Repository(RepositoryAction::RetainedSaveFinished {
            result: Err(crate::editor::retained_save::RetainedSaveError::Io(_)),
            ..
        })
    ));
    assert!(!fixture.path.exists());
    assert!(!app.operation_recovery_pending());
    let _ = app.update(failed);
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
        if runtime.is_dirty() && runtime.can_abandon_unsaved_changes())
    );
    assert!(
        app.dirty_repository_close.is_some(),
        "a failed save leaves the dialog up"
    );
    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);
}

#[test]
fn card_close_still_discards_ordinary_editor_dirtiness() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let runtime = RepositoryRuntime::dirty_fixture(card);
    assert!(runtime.can_abandon_unsaved_changes());
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));

    let _ = app.update(Message::CloseCard(card));
    drop(app.update(Message::DiscardAndCloseRepository(card)));

    assert!(app.dirty_repository_close.is_none());
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Repository(runtime)) if !runtime.blocks_navigation()
    ));
    assert!(app.pane_close_owned(card));
}

#[test]
fn application_close_advances_only_after_current_durability_fence_resolves() {
    if crate::repository_widget::retained_save_tests::isolated_test(
        "app::tests::application_close_advances_only_after_current_durability_fence_resolves",
    ) {
        return;
    }
    let mut app = context_inspector_test_app(true);
    let first = app.workspace.focused().unwrap();
    let second_state = terminal_card_state();
    let second = second_state.id;
    Arc::make_mut(&mut app.durable)
        .cards
        .push(second_state.clone());
    app.workspace
        .add_card(
            WorkspaceCard::new(
                second,
                second_state.title,
                CardWidth::Fraction(second_state.width_fraction),
            )
            .unwrap(),
            1,
            false,
        )
        .unwrap();
    let fixture = CloseSaveFixture::install(&mut app, first);
    let warning = fixture.post_persist_warning(&mut app, first);
    drop(app.update(warning));
    assert!(
        matches!(app.cards.get(&first), Some(CardRuntime::Repository(runtime))
        if runtime.is_dirty() && !runtime.is_saving() && runtime.blocks_navigation()
            && runtime.save_error().is_some_and(|warning| warning.contains("replaced on disk")))
    );
    app.cards.insert(
        second,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(second))),
    );
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));

    let _ = app.update(Message::WindowCloseRequested(window));
    let _ = app.update(Message::DiscardAndCloseRepository(first));
    assert!(
        app.dirty_repository_close
            .as_ref()
            .is_some_and(|close| close.card == first && close.remaining == [second])
    );

    let completion = fixture.durable_retry(&mut app, first);
    let _ = app.update(completion);

    assert!(
        app.dirty_repository_close
            .as_ref()
            .is_some_and(|close| close.card == second
                && close.remaining.is_empty()
                && close.intent == DirtyCloseIntent::Application(window))
    );
    assert!(matches!(
        app.cards.get(&first),
        Some(CardRuntime::Repository(runtime)) if !runtime.blocks_navigation()
    ));
    assert!(matches!(
        app.cards.get(&second),
        Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation()
    ));
}

#[test]
fn card_close_stays_fenced_after_post_persist_failure_until_retry_succeeds() {
    if crate::repository_widget::retained_save_tests::isolated_test(
        "app::tests::card_close_stays_fenced_after_post_persist_failure_until_retry_succeeds",
    ) {
        return;
    }
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let fixture = CloseSaveFixture::install(&mut app, card);
    let failure = fixture.post_persist_warning(&mut app, card);
    app.dirty_repository_close = Some(DirtyRepositoryClose {
        card,
        remaining: VecDeque::new(),
        intent: DirtyCloseIntent::Card(card),
        focused: DirtyCloseControl::Save,
        restore_card: Some(card),
    });

    let _ = app.update(failure);

    assert!(app.dirty_repository_close.is_some());
    let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
        panic!("repository runtime should remain installed");
    };
    assert!(runtime.is_dirty());
    assert!(!runtime.is_saving());
    assert!(runtime.blocks_navigation());
    assert!(
        runtime
            .save_error()
            .is_some_and(|warning| warning.contains("replaced on disk"))
    );

    let retry_error = fixture.failed_retry(&mut app, card);

    assert!(app.dirty_repository_close.is_some());
    let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
        panic!("repository runtime should remain installed");
    };
    let message = runtime.save_error().expect("close dialog shows the error");
    assert!(message.contains(&retry_error));
    assert!(message.contains("replaced on disk"));
    assert!(runtime.blocks_navigation());
    let context = runtime.inspector_context();
    assert!(context.can_reload);
    assert!(context.can_save);

    let completion = fixture.durable_retry(&mut app, card);
    let _close = app.update(completion);

    assert!(app.dirty_repository_close.is_none());
    let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
        panic!("repository runtime should remain until close is committed");
    };
    assert!(!runtime.blocks_navigation());
    assert!(runtime.save_error().is_none());
}

#[test]
fn window_close_stays_fenced_after_post_persist_failure_until_retry_succeeds() {
    if crate::repository_widget::retained_save_tests::isolated_test(
        "app::tests::window_close_stays_fenced_after_post_persist_failure_until_retry_succeeds",
    ) {
        return;
    }
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let window = window::Id::unique();
    let fixture = CloseSaveFixture::install(&mut app, card);
    let failure = fixture.post_persist_warning(&mut app, card);
    app.dirty_repository_close = Some(DirtyRepositoryClose {
        card,
        remaining: VecDeque::new(),
        intent: DirtyCloseIntent::Application(window),
        focused: DirtyCloseControl::Save,
        restore_card: Some(card),
    });

    let _ = app.update(failure);

    assert!(app.dirty_repository_close.is_some());
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Repository(runtime)) if runtime.is_dirty()
            && !runtime.is_saving() && runtime.blocks_navigation()
    ));

    let retry_error = fixture.failed_retry(&mut app, card);

    assert!(app.dirty_repository_close.is_some());
    let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
        panic!("repository runtime should remain installed");
    };
    let message = runtime.save_error().expect("close dialog shows the error");
    assert!(message.contains(&retry_error));
    assert!(message.contains("replaced on disk"));
    assert!(runtime.blocks_navigation());
    let context = runtime.inspector_context();
    assert!(context.can_reload);
    assert!(context.can_save);

    let completion = fixture.durable_retry(&mut app, card);
    let _exit = app.update(completion);

    assert!(app.dirty_repository_close.is_none());
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Repository(runtime)) if !runtime.blocks_navigation()
            && runtime.save_error().is_none()
    ));
}

#[test]
fn active_save_fences_direct_adjacent_pan_and_wheel_navigation() {
    let focused = CardId::new();
    assert!(focused_save_in_flight(Some(focused), |card| card == focused));
    assert!(!focused_save_in_flight(None, |_| true));
    assert!(!focused_save_in_flight(Some(focused), |_| false));

    let messages = [
        Message::NewTerminal,
        Message::NewBrowser,
        Message::OpenLink(
            focused,
            Url::parse("https://example.com").unwrap(),
            crate::settings::LinkOpenGesture::ControlShiftClick,
        ),
        Message::ChatUi(chat_ui::Action::OpenUrl("https://example.com".into())),
        Message::PullRequestUi(pull_request_ui::Action::Open {
            repository: GitHubRepoKey::new("github.com", None, "o", "r").unwrap(),
            number: 1,
            url: Url::parse("https://github.com/o/r/pull/1").unwrap(),
        }),
        Message::PullRequestUi(pull_request_ui::Action::OpenCheck(
            Url::parse("https://example.com").unwrap(),
        )),
        Message::FocusCard(CardId::new()),
        Message::FocusAdjacent(crate::workspace::FocusDirection::Right),
        Message::ToggleContextInspector,
        Message::Repository(RepositoryAction::NewTerminal { card: focused }),
    ];
    assert!(
        messages
            .iter()
            .all(workspace_navigation_blocked_during_save)
    );

    let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
    let key = crate::github::PullRequestKey::new(repository, 45).unwrap();
    assert!(completion_navigates(&Message::PullRequestCreated {
        dialog: pull_request_test_dialog(),
        generation: 0,
        result: Ok(PullRequestCreationOutcome::Created {
            key,
            url: Url::parse("https://github.com/owner/repo/pull/45").unwrap(),
            pull_request: None,
            normalization_warning: None,
        }),
    }));
    assert!(!completion_navigates(&Message::ExternalLinkOpened(Ok(()))));
}

fn pull_request_test_dialog() -> PullRequestDialog {
    PullRequestDialog {
        project_id: ProjectId::new(),
        repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
        worktree: None,
        base: "release".into(),
        head: "topic/authored-change".into(),
        title: "Keep the authored title".into(),
        body: "A carefully authored description.\n\nWith details for reviewers.".into(),
        draft: true,
        retry_confirmation: None,
    }
}

fn authorize_pull_request_test(app: &mut Cagents, dialog: &PullRequestDialog) {
    app.github_availability.insert(
        dialog.project_id,
        GitHubAvailability::Ready {
            repository_key: dialog.repository.clone(),
            pull_requests: Vec::new(),
            refreshed_at: 0,
        },
    );
}

fn pull_request_test_failure(exit_code: i32) -> crate::repository::ProcessFailure {
    crate::repository::ProcessFailure {
        kind: crate::repository::ProcessFailureKind::Exit,
        program: "gh".into(),
        exit_code: Some(exit_code),
        signal: None,
        stdout: Vec::new(),
        stderr: b"pull request create failed".to_vec(),
        message: "pull request create failed".into(),
        started: true,
    }
}

#[test]
fn pull_request_single_line_fields_submit_on_enter() {
    let mut app = context_inspector_test_app(false);
    let dialog = pull_request_test_dialog();
    authorize_pull_request_test(&mut app, &dialog);
    app.pull_request_dialog = Some(dialog);
    for id in [
        pull_request_dialog_focus_id(),
        widget::Id::new("dialog.pull-request.head"),
        widget::Id::new("dialog.pull-request.base"),
    ] {
        let (messages, _) = dialog_ui::tests::key_control(
            app.dialog().unwrap(),
            id,
            dialog_ui::tests::key_press(keyboard::key::Named::Enter),
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::SubmitPullRequest))
                .count(),
            1
        );
        // Native text_editor::unfocus queues ClearSelection. Focusing any
        // single-line field flushes that pending edit on the next event.
        assert!(
            messages.iter().all(|message| matches!(
                message,
                Message::SubmitPullRequest
                    | Message::PullRequestBodyChanged(widget::text_editor::Action::ClearSelection)
            )),
            "unexpected Enter callbacks: {messages:?}"
        );
    }
}

#[test]
fn pull_request_enter_and_reducer_refuse_blank_fields_without_creator_task() {
    for field in 0..3 {
        for value in ["", "   ", "\t"] {
            let mut app = context_inspector_test_app(false);
            let mut dialog = pull_request_test_dialog();
            authorize_pull_request_test(&mut app, &dialog);
            match field {
                0 => dialog.title = value.into(),
                1 => dialog.head = value.into(),
                _ => dialog.base = value.into(),
            }
            app.pull_request_dialog = Some(dialog.clone());
            assert!(!app.pull_request_submission_allowed());
            for id in [
                pull_request_dialog_focus_id(),
                widget::Id::new("dialog.pull-request.head"),
                widget::Id::new("dialog.pull-request.base"),
            ] {
                let (messages, _) = dialog_ui::tests::key_control(
                    app.dialog().unwrap(),
                    id,
                    dialog_ui::tests::key_press(keyboard::key::Named::Enter),
                );
                assert!(
                    !messages
                        .iter()
                        .any(|message| matches!(message, Message::SubmitPullRequest))
                );
            }
            let task = app.update(Message::SubmitPullRequest);
            assert_eq!(task.units(), 0, "invalid form must schedule no creator");
            assert_eq!(app.pull_request_dialog.as_ref(), Some(&dialog));
        }
    }
}

#[test]
fn pull_request_editor_preserves_multiline_markdown_on_rejection() {
    let mut app = context_inspector_test_app(false);
    let mut dialog = pull_request_test_dialog();
    authorize_pull_request_test(&mut app, &dialog);
    dialog.title.clear();
    app.pull_request_dialog = Some(dialog);
    let markdown = "Summary\n\n- [ ] First task\n- [x] Second task\n\n~~~rust\nlet value = 1;\n~~~";
    let _ = app.update(Message::PullRequestBodyChanged(
        iced::widget::text_editor::Action::SelectAll,
    ));
    let _ = app.update(Message::PullRequestBodyChanged(
        iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(
            std::sync::Arc::new(markdown.into()),
        )),
    ));
    let _ = app.update(Message::SubmitPullRequest);
    assert_eq!(
        app.pull_request_dialog.as_ref().unwrap().body.0.text(),
        markdown
    );
}

#[test]
fn pull_request_local_rejection_preserves_authored_draft() {
    let mut app = context_inspector_test_app(false);
    let mut dialog = pull_request_test_dialog();
    authorize_pull_request_test(&mut app, &dialog);
    dialog.title.clear();
    app.pull_request_dialog = Some(dialog.clone());

    let _ = app.update(Message::SubmitPullRequest);

    assert_eq!(app.pull_request_dialog.as_ref(), Some(&dialog));
}

#[test]
fn pull_request_remote_failure_restores_hidden_draft_for_retry() {
    for result in [
        Ok(PullRequestCreationOutcome::NotCreated {
            failure: pull_request_test_failure(4),
        }),
        Err("pull request creator failed".into()),
    ] {
        let mut app = context_inspector_test_app(false);
        let dialog = pull_request_test_dialog();
        authorize_pull_request_test(&mut app, &dialog);
        app.pull_request_dialog = Some(dialog.clone());

        // Dropping the task avoids invoking gh; the update transition itself
        // must remove the draft until its asynchronous completion arrives.
        let _ = app.update(Message::SubmitPullRequest);
        assert!(app.pull_request_dialog.is_none());
        let _ = app.update(Message::PullRequestCreated {
            dialog: dialog.clone(),
            generation: app.pull_request_dialog_generation,
            result,
        });

        assert_eq!(app.pull_request_dialog.as_ref(), Some(&dialog));
        let _ = app.update(Message::SubmitPullRequest);
        assert!(app.pull_request_dialog.is_none(), "retry should submit");
    }
}

#[test]
fn pull_request_unknown_outcome_keeps_edits_but_requires_reconciliation() {
    let mut app = context_inspector_test_app(false);
    let dialog = pull_request_test_dialog();
    authorize_pull_request_test(&mut app, &dialog);
    app.pull_request_dialog = Some(dialog.clone());
    let _ = app.update(Message::SubmitPullRequest);

    let _ = app.update(Message::PullRequestCreated {
        dialog: dialog.clone(),
        generation: app.pull_request_dialog_generation,
        result: Ok(PullRequestCreationOutcome::OutcomeUnknown {
            failure: pull_request_test_failure(1),
        }),
    });
    let mut expected = dialog;
    expected.retry_confirmation = Some(false);
    assert_eq!(app.pull_request_dialog.as_ref(), Some(&expected));

    let revised_body = "Revised description after checking the remote".to_owned();
    let _ = app.update(Message::PullRequestBodyChanged(
        iced::widget::text_editor::Action::SelectAll,
    ));
    let _ = app.update(Message::PullRequestBodyChanged(
        iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(
            std::sync::Arc::new(revised_body.clone()),
        )),
    ));
    let _ = app.update(Message::SubmitPullRequest);
    expected.body = revised_body.into();
    assert_eq!(app.pull_request_dialog.as_ref(), Some(&expected));

    let _ = app.update(Message::PullRequestRetryConfirmed(true));
    let _ = app.update(Message::SubmitPullRequest);
    assert!(app.pull_request_dialog.is_none());
}

#[test]
fn pull_request_success_does_not_reopen_submitted_draft() {
    let mut app = context_inspector_test_app(false);
    let dialog = pull_request_test_dialog();
    authorize_pull_request_test(&mut app, &dialog);
    app.pull_request_dialog = Some(dialog.clone());
    let _ = app.update(Message::SubmitPullRequest);
    let key = crate::github::PullRequestKey::new(dialog.repository.clone(), 45).unwrap();

    let _ = app.update(Message::PullRequestCreated {
        dialog,
        generation: app.pull_request_dialog_generation,
        result: Ok(PullRequestCreationOutcome::Created {
            key,
            url: Url::parse("https://github.com/owner/repo/pull/45").unwrap(),
            pull_request: None,
            normalization_warning: None,
        }),
    });

    assert!(app.pull_request_dialog.is_none());
}

#[test]
fn pull_request_stale_failure_does_not_replace_a_newer_draft() {
    let mut app = context_inspector_test_app(false);
    let dialog = pull_request_test_dialog();
    authorize_pull_request_test(&mut app, &dialog);
    app.pull_request_dialog = Some(dialog.clone());
    let _ = app.update(Message::SubmitPullRequest);
    let generation = app.pull_request_dialog_generation;
    let project_id = ProjectId::new();
    app.repository_generations.insert(project_id, 0);
    app.github_availability.insert(
        project_id,
        GitHubAvailability::Ready {
            repository_key: dialog.repository.clone(),
            pull_requests: Vec::new(),
            refreshed_at: 0,
        },
    );
    let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
        project_id,
        worktree: None,
    }));
    let _ = app.update(Message::PullRequestTitleChanged("Newer PR".into()));
    let _ = app.update(Message::PullRequestHeadChanged("new-topic".into()));
    let _ = app.update(Message::PullRequestBaseChanged("trunk".into()));
    let _ = app.update(Message::PullRequestBodyChanged(
        iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(
            std::sync::Arc::new("Newer description".into()),
        )),
    ));
    let newer = app.pull_request_dialog.clone().unwrap();
    let failure = Message::PullRequestCreated {
        dialog,
        generation,
        result: Ok(PullRequestCreationOutcome::NotCreated {
            failure: pull_request_test_failure(4),
        }),
    };

    let _ = app.update(failure.clone());
    assert_eq!(app.pull_request_dialog.as_ref(), Some(&newer));

    let _ = app.update(Message::SubmitPullRequest);
    assert!(app.pull_request_dialog.is_none());
    let _ = app.update(failure.clone());
    assert!(
        app.pull_request_dialog.is_none(),
        "newer request is in flight"
    );

    let _ = app.update(Message::CancelDialog);
    let _ = app.update(failure);
    assert!(
        app.pull_request_dialog.is_none(),
        "dismissed drafts stay closed"
    );
}

#[test]
fn pull_request_recovery_survives_cancelling_an_unrelated_worktree_dialog() {
    for complete_before_cancel in [false, true] {
        let mut app = context_inspector_test_app(false);
        let dialog = pull_request_test_dialog();
        authorize_pull_request_test(&mut app, &dialog);
        app.pull_request_dialog = Some(dialog.clone());
        let _ = app.update(Message::SubmitPullRequest);
        let completion = Message::PullRequestCreated {
            dialog: dialog.clone(),
            generation: app.pull_request_dialog_generation,
            result: Ok(PullRequestCreationOutcome::NotCreated {
                failure: pull_request_test_failure(4),
            }),
        };
        let _ = app.update(Message::Sidebar(SidebarAction::NewWorktree {
            project_id: ProjectId::new(),
            repository: PathBuf::from("/repo"),
        }));
        assert!(app.worktree_dialog.is_some());
        if complete_before_cancel {
            let _ = app.update(completion.clone());
        }
        let _ = app.update(Message::CancelDialog);
        if !complete_before_cancel {
            let _ = app.update(completion);
        }
        assert!(app.worktree_dialog.is_none());
        assert_eq!(app.pull_request_dialog.as_ref(), Some(&dialog));
        let _ = app.update(Message::CancelDialog);
        assert!(app.pull_request_dialog.is_none());
    }
}

#[test]
fn surfaces_and_rules_reach_a_free_right_edge_while_controls_keep_out_of_its_border() {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    use iced_accessibility::accesskit::Role;

    fn rules(node: &layout::Node, origin: iced::Point, out: &mut Vec<iced::Rectangle>) {
        let bounds = node.bounds() + iced::Vector::new(origin.x, origin.y);
        if node.children().is_empty() && bounds.height == design::BORDER_WIDTH {
            out.push(bounds);
        }
        for child in node.children() {
            rules(child, bounds.position(), out);
        }
    }

    let width = 1500.0;
    let border = design::WINDOW_RESIZE_BORDER;
    for inspector in [false, true] {
        let mut app = context_inspector_test_app(true);
        app.settings.ui_scale = 1.0;
        app.window_width = width;
        assert_eq!(app.trailing_edge_inset(), border);
        if inspector {
            // A pane measured wide enough to dock its inspector, as the
            // strip beside the sidebar measures itself, so the inspector
            // ends where the window does rather than past it.
            let sidebar =
                sidebar::effective_width(app.projects_sidebar_mode(), app.settings.sidebar_width);
            drop(app.workspace.resize_viewport(width - sidebar));
            let card = app.workspace.focused().unwrap();
            app.open_pane_inspector_now(card);
            assert_eq!(
                app.pane_inspector_layout(card),
                Some(InspectorLayout::Docked)
            );
        }
        let renderer = headless_renderer();
        let mut element = app.view();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 900.0)),
        );
        let mut found = Vec::new();
        rules(&node, iced::Point::ORIGIN, &mut found);
        // Every rule that runs toward the edge reaches it, rather than
        // stopping where the resize border begins: the header rule of the
        // rightmost surface and the status bar rule, at least.
        let toward_edge: Vec<_> = found
            .iter()
            .filter(|rule| rule.x + rule.width > width - border - 1.0)
            .collect();
        assert!(
            toward_edge.len() >= 2,
            "inspector {inspector}: expected the header and status bar rules: {found:?}"
        );
        for rule in toward_edge {
            assert_eq!(
                rule.x + rule.width,
                width,
                "inspector {inspector}: a rule stops short of the window edge: {rule:?}"
            );
        }
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let controls: Vec<_> = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|node| {
                matches!(
                    node.node().role(),
                    Role::Button | Role::Splitter | Role::ScrollBar | Role::Slider
                )
            })
            .filter_map(|node| node.node().bounds())
            .collect();
        assert!(!controls.is_empty());
        for bounds in controls {
            assert!(
                bounds.x1 <= f64::from(width - border) + 0.5,
                "inspector {inspector}: a control reaches into the resize border: {bounds:?}"
            );
        }
    }
}

#[test]
fn content_stays_out_of_the_resize_border_of_a_free_right_edge() {
    use cosmic::cctk::sctk::reexports::csd_frame::WindowState;
    use iced::event::{PlatformSpecific, wayland};

    let state = |state| {
        iced::Event::PlatformSpecific(PlatformSpecific::Wayland(wayland::Event::Window(
            wayland::WindowEvent::WindowState(state),
        )))
    };
    let mut app = context_inspector_test_app(false);
    let main = window::Id::unique();
    app.core.set_main_window_id(Some(main));
    app.settings.ui_scale = 1.0;
    assert_eq!(app.window_edge_inset(), design::WINDOW_RESIZE_BORDER);
    // The border is in compositor pixels; the interface scale enlarges
    // every interface pixel.
    app.settings.ui_scale = 1.25;
    assert_eq!(app.window_edge_inset(), design::WINDOW_RESIZE_BORDER / 1.25);
    app.settings.ui_scale = 1.0;

    // Another window's state does not move the main window's content.
    let other = window_edge_message(&state(WindowState::TILED), window::Id::unique())
        .expect("window state event");
    let _ = app.update(other);
    assert_eq!(app.window_edge_inset(), design::WINDOW_RESIZE_BORDER);

    // A tiled right edge has no border, so content returns to the edge.
    // Other tiled edges leave the right border in place.
    for (flags, inset) in [
        (
            WindowState::TILED_LEFT | WindowState::TILED_TOP,
            design::WINDOW_RESIZE_BORDER,
        ),
        (WindowState::TILED_RIGHT | WindowState::ACTIVATED, 0.0),
        (WindowState::ACTIVATED, design::WINDOW_RESIZE_BORDER),
    ] {
        let message = window_edge_message(&state(flags), main).expect("window state event");
        let _ = app.update(message);
        assert_eq!(app.window_edge_inset(), inset, "{flags:?}");
    }

    // A maximized or fullscreen window has no border at all, whether the
    // Wayland state or the backend query reports it.
    let maximized =
        window_edge_message(&state(WindowState::MAXIMIZED), main).expect("window state event");
    let _ = app.update(maximized);
    assert_eq!(app.window_edge_inset(), 0.0);
    let _ = app.update(Message::WindowFilled(main, false));
    assert_eq!(app.window_edge_inset(), design::WINDOW_RESIZE_BORDER);
    let _ = app.update(Message::WindowFilled(window::Id::unique(), true));
    assert_eq!(app.window_edge_inset(), design::WINDOW_RESIZE_BORDER);
    let _ = app.update(Message::WindowFilled(main, true));
    assert_eq!(app.window_edge_inset(), 0.0);
    assert!(window_edge_message(&iced::Event::Window(window::Event::Focused), main).is_none());
}

#[test]
fn resizes_query_the_window_state_only_until_wayland_reports_it() {
    use cosmic::cctk::sctk::reexports::csd_frame::WindowState;
    use iced::event::{PlatformSpecific, wayland};

    let mut app = context_inspector_test_app(false);
    let resize = |app: &mut Cagents| app.update(Message::WindowResized(1600.0, 900.0)).units();
    let without_query = resize(&mut app);
    let main = window::Id::unique();
    app.core.set_main_window_id(Some(main));
    // X11 sends no window states, so every resize asks the backend.
    assert!(resize(&mut app) > without_query);
    assert!(resize(&mut app) > without_query);

    let state = iced::Event::PlatformSpecific(PlatformSpecific::Wayland(wayland::Event::Window(
        wayland::WindowEvent::WindowState(WindowState::ACTIVATED),
    )));
    let _ = app.update(window_edge_message(&state, main).expect("window state event"));
    assert_eq!(resize(&mut app), without_query);
}

#[test]
fn initial_native_window_size_replaces_the_estimated_geometry() {
    for event in [
        window::Event::Opened {
            size: iced::Size::new(960.0, 600.0),
            position: None,
        },
        window::Event::Resized(iced::Size::new(960.0, 600.0)),
    ] {
        let mut app = context_inspector_test_app(true);
        let message = window_geometry_message(&event).expect("native geometry event");
        let _ = app.update(message);
        assert_eq!(app.window_width, 960.0);
        assert_eq!(app.window_height, 600.0);
        // No window-wide inspector covers the workspace any more.
        assert!(!app.workspace_inputs_blocked());
    }
    assert!(window_geometry_message(&window::Event::Focused).is_none());
}

#[test]
fn reset_settings_dialog_starts_on_cancel_cycles_within_and_escape_restores_origin() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.cards.insert(card, CardRuntime::Settings);
    app.card_state_mut(card).unwrap().kind = CardKind::Settings;
    app.settings_category = SettingsCategory::General;
    app.core.set_keyboard_nav(true);
    let task = app.update(Message::ResetSettings);
    assert_eq!(
        focused_element(cosmic::Application::dialog(&app).unwrap(), task),
        Some(widget::Id::new("dialog.reset-settings.cancel"))
    );
    assert!(!app.core.keyboard_nav());
    let press = |key, backwards| keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(key),
        modified_key: keyboard::Key::Named(key),
        physical_key: keyboard::key::Physical::Code(if key == keyboard::key::Named::Tab {
            keyboard::key::Code::Tab
        } else {
            keyboard::key::Code::Escape
        }),
        location: keyboard::Location::Standard,
        modifiers: if backwards {
            keyboard::Modifiers::SHIFT
        } else {
            keyboard::Modifiers::empty()
        },
        text: None,
        repeat: false,
    };
    // Resetting can't be undone, so Enter on the opening focus cancels,
    // and the reset is one Tab, Shift+Tab or arrow away. Either way focus
    // wraps within the two actions rather than reaching the window.
    let cancel = widget::Id::new("dialog.reset-settings.cancel");
    let reset = widget::Id::new("dialog.reset-settings.confirm");
    assert!(matches!(
        dialog_ui::tests::enter_control(cosmic::Application::dialog(&app).unwrap(), cancel.clone()),
        Message::CancelResetSettings
    ));
    for (from, backwards, to) in [
        (&cancel, false, &reset),
        (&reset, false, &cancel),
        (&cancel, true, &reset),
        (&reset, true, &cancel),
    ] {
        let (focused, captured) = dialog_ui::tests::focus_after_key(
            cosmic::Application::dialog(&app).unwrap(),
            from.clone(),
            press(keyboard::key::Named::Tab, backwards),
        );
        assert_eq!(focused.as_ref(), Some(to));
        assert!(captured);
    }
    let task = app.update(Message::KeyEvent {
        event: press(keyboard::key::Named::Escape, false),
        captured: true,
    });
    assert!(!app.settings_reset_confirm);
    assert!(app.core.keyboard_nav());
    assert_eq!(focused(&app, task), Some(widget::Id::new("settings.reset")));
    // Backdrop and Cancel dispatch the same message and restore the same
    // origin; closing an already closed dialog cannot disturb focus.
    let _ = app.update(Message::ResetSettings);
    let task = app.update(Message::CancelResetSettings);
    assert_eq!(focused(&app, task), Some(widget::Id::new("settings.reset")));
    assert!(app.settings_reset_keyboard_nav.is_none());
}

#[test]
fn dirty_editor_confirmation_starts_on_cancel_and_enter_preserves_edits() {
    let (mut app, card) = editing_repository_app();
    let opening = app.update(Message::CloseCard(card));
    let cancel = dirty_close_control_id(card, DirtyCloseControl::Cancel);
    assert_eq!(
        focused_element(app.dialog().unwrap(), opening),
        Some(cancel.clone())
    );
    let message = dialog_ui::tests::enter_control(app.dialog().unwrap(), cancel);
    assert!(matches!(message, Message::CancelRepositoryClose));
    drop(app.update(message));
    assert!(app.dirty_repository_close.is_none());
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(
        matches!(&app.cards[&card],CardRuntime::Repository(runtime) if runtime.is_dirty() && !runtime.is_saving())
    );
}

#[test]
fn dirty_dialog_focus_is_stable_trapped_and_save_aware() {
    let mut focus = DirtyCloseControl::Cancel;
    focus = next_dirty_close_control(focus, false, false, true);
    assert_eq!(focus, DirtyCloseControl::Discard);
    focus = next_dirty_close_control(focus, false, false, true);
    assert_eq!(focus, DirtyCloseControl::Save);
    focus = next_dirty_close_control(focus, false, false, true);
    assert_eq!(focus, DirtyCloseControl::Cancel);
    assert_eq!(
        next_dirty_close_control(DirtyCloseControl::Save, true, false, true),
        DirtyCloseControl::Discard
    );
    assert_eq!(
        next_dirty_close_control(DirtyCloseControl::Save, false, true, true),
        DirtyCloseControl::Cancel
    );
    assert_eq!(
        next_dirty_close_control(DirtyCloseControl::Cancel, false, false, false),
        DirtyCloseControl::Save
    );
    assert_eq!(
        next_dirty_close_control(DirtyCloseControl::Cancel, true, false, false),
        DirtyCloseControl::Save
    );
    assert_eq!(
        next_dirty_close_control(DirtyCloseControl::Save, false, false, false),
        DirtyCloseControl::Cancel
    );
}

#[test]
fn pending_browser_navigation_waits_for_the_queued_file_list_refresh() {
    for ignored in [false, true] {
        let mut app = context_inspector_test_app(false);
        let card = CardId::new();
        let worktree = PathBuf::from("/repo");
        let mut runtime = RepositoryRuntime::file_list_fixture(
            RepositoryView {
                project: ProjectId::new(),
                repository: worktree.clone(),
                worktree: worktree.clone(),
                selected_file: None,
            },
            &["tracked.rs"],
        );
        drop(runtime.initial_load(card));
        drop(runtime.update(RepositoryAction::RefreshFiles { card }));
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        let pending = PendingPanelOpen {
            card,
            path: PathBuf::from("tracked.rs"),
            ignored,
        };
        app.pending_panel_open = Some(pending.clone());
        let completion = |generation| {
            Message::Repository(RepositoryAction::FilesLoaded {
                identity: repository_widget::AsyncIdentity {
                    card,
                    worktree: worktree.clone(),
                    path: PathBuf::from("."),
                    generation,
                },
                result: Ok(vec![repository_widget::RepositoryFile {
                    path: PathBuf::from("tracked.rs"),
                    staged: true,
                    ..Default::default()
                }]),
            })
        };
        drop(app.update(completion(0)));
        assert_eq!(app.pending_panel_open.as_ref(), Some(&pending));
        drop(app.update(completion(1)));
        assert_eq!(
            app.pending_panel_open.as_ref(),
            Some(&pending),
            "the queued refresh must finish before browser navigation"
        );
        drop(app.update(completion(2)));
        assert!(app.pending_panel_open.is_none());
        let CardRuntime::Repository(runtime) = &app.cards[&card] else {
            unreachable!();
        };
        assert!(runtime.listed_files()[0].staged);
    }
}

#[test]
fn modal_rejects_editor_input_but_accepts_async_repository_completions() {
    let card = CardId::new();
    assert!(repository_action_allowed_while_modal(
        &RepositoryAction::FindReplacementLoaded {
            card,
            ticket: card.as_uuid(),
            result: Ok(None)
        }
    ));
    assert!(repository_action_allowed_while_modal(
        &RepositoryAction::FindLoaded {
            card,
            ticket: card.as_uuid(),
            result: Ok(Some(Default::default())),
        }
    ));
    let editor = RepositoryAction::EditorAction {
        card,
        action: cosmic::widget::text_editor::Action::Edit(
            cosmic::widget::text_editor::Edit::Paste(Arc::new("blocked".into())),
        ),
    };
    assert!(!repository_action_allowed_while_modal(&editor));
    let identity = repository_widget::AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path: PathBuf::from("file.txt"),
        generation: 1,
    };
    assert!(repository_action_allowed_while_modal(
        &RepositoryAction::FileLoaded {
            identity,
            result: Err(crate::editor::FileIoError::Io("fixture".into())),
        }
    ));
    // A dropped page render is not a lost frame: the viewer reads
    // "Rendering page" until the result lands, so refusing it while an
    // inspector or a dialog is open would strand the document viewer on a
    // page it never draws.
    assert!(repository_action_allowed_while_modal(
        &RepositoryAction::PdfPageRendered {
            card,
            request: 1,
            result: Err("fixture".into()),
        }
    ));
    for message in [
        Message::NewTerminal,
        Message::NewBrowser,
        Message::FocusCard(card),
        Message::ResizeCard { card, width: 640.0 },
        Message::ResizeCardFinished(card),
        Message::FocusAdjacent(crate::workspace::FocusDirection::Right),
        Message::Repository(RepositoryAction::NewTerminal { card }),
        Message::ToggleContextInspector,
    ] {
        assert!(user_workspace_message_blocked_while_modal(&message));
    }
    assert!(!user_workspace_message_blocked_while_modal(
        &Message::Repository(RepositoryAction::FileLoaded {
            identity: repository_widget::AsyncIdentity {
                card,
                worktree: PathBuf::from("/repo"),
                path: PathBuf::from("file.txt"),
                generation: 1,
            },
            result: Err(crate::editor::FileIoError::Io("fixture".into())),
        })
    ));
}

/// Where `task` asks the tab strip to scroll to, if it asks at all.
fn tab_strip_scroll(task: Task<Message>) -> Option<f32> {
    use cosmic::iced::advanced::widget::operation::scrollable::{
        AbsoluteOffset, RelativeOffset, Scrollable,
    };

    struct Strip(Option<f32>);
    impl Scrollable for Strip {
        fn snap_to(&mut self, _offset: RelativeOffset<Option<f32>>) {}
        fn scroll_to(&mut self, offset: AbsoluteOffset<Option<f32>>) {
            self.0 = offset.x;
        }
        fn scroll_by(
            &mut self,
            _offset: AbsoluteOffset,
            _bounds: cosmic::iced::Rectangle,
            _content_bounds: cosmic::iced::Rectangle,
        ) {
        }
    }

    let mut strip = Strip(None);
    let tabs = widget::Id::new("workspace.tabs");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let Some(mut stream) = iced::runtime::task::into_stream(task) else {
            return;
        };
        // The task also carries timers that outlive the test. A scroll is
        // ready at once, so a short quiet spell means none is coming.
        while let Ok(Some(action)) =
            tokio::time::timeout(Duration::from_millis(250), stream.next()).await
        {
            if let iced::runtime::Action::Widget(mut operation) = action {
                let bounds = cosmic::iced::Rectangle::default();
                operation.scrollable(
                    Some(&tabs),
                    bounds,
                    bounds,
                    cosmic::iced::Vector::ZERO,
                    &mut strip,
                );
            }
        }
    });
    runtime.shutdown_background();
    strip.0
}

/// Reordering does not change which pane is focused, and the tab strip
/// only followed focus, so moving the active pane along the strip could
/// carry its tab out of sight.
#[test]
fn moving_the_active_pane_keeps_its_tab_in_view() {
    let mut app = context_inspector_test_app(false);
    let cards: Vec<_> = (0..24).map(|_| terminal_card_state()).collect();
    let active = cards[0].id;
    app.durable = Arc::new(WorkspaceState {
        focused: Some(active),
        cards: cards.into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(900.0);
    app.chrome_ui.tabs_focus = Some(active);
    assert_eq!(
        tab_strip_scroll(app.reveal_active_tab()),
        None,
        "the first tab starts in view"
    );

    let task = app.reorder_pane(active, None);
    assert_eq!(app.workspace.order().last(), Some(active));
    assert_eq!(app.workspace.focused(), Some(active));
    assert!(
        tab_strip_scroll(task).is_some_and(|offset| offset > 0.0),
        "the tab moved to the far end of the strip and was not followed"
    );
}

#[test]
fn reordering_an_inactive_pane_preserves_the_users_tab_scroll() {
    let mut app = context_inspector_test_app(false);
    let cards: Vec<_> = (0..24).map(|_| terminal_card_state()).collect();
    let active = cards[0].id;
    let moved = cards[12].id;
    app.durable = Arc::new(WorkspaceState {
        focused: Some(active),
        cards: cards.into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(900.0);
    app.chrome_ui.tabs_focus = Some(active);
    let _ = app.update_chrome(chrome_ui::Action::TabsScrolled(1000.0));
    assert!(tab_strip_scroll(app.reveal_active_tab()).is_some());
    let task = app.reorder_pane(moved, None);
    assert_eq!(app.workspace.order().last(), Some(moved));
    assert_eq!(app.workspace.focused(), Some(active));
    assert_eq!(tab_strip_scroll(task), None);
    assert!(tab_strip_scroll(app.reveal_active_tab()).is_some());
}

#[test]
fn a_sweep_that_left_a_passing_pane_cold_is_repeated_when_the_camera_rests_or_turns() {
    let mut app = context_inspector_test_app(false);
    let cards: Vec<_> = std::iter::repeat_with(terminal_card_state)
        .take(8)
        .collect();
    let origin = cards[0].id;
    let target = cards[7].id;
    let retarget = cards[5].id;
    app.durable = Arc::new(WorkspaceState {
        focused: Some(origin),
        cards: cards.into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(600.0);
    app.workspace.native_scroll(0.0, 600.0);
    let _ = app.update(Message::LifecycleSweep);
    assert!(!app.take_passing_sweep());

    app.workspace.focus(target).unwrap();
    assert!(app.workspace.is_passing(origin));
    let _ = app.update(Message::LifecycleSweep);
    assert!(!app.take_passing_sweep());

    // Heading somewhere else may make a destination of a pane passed.
    app.workspace.focus(retarget).unwrap();
    assert!(app.take_passing_sweep());
    assert!(!app.take_passing_sweep());

    let _ = app.update(Message::LifecycleSweep);
    for _ in 0..240 {
        assert!(!app.take_passing_sweep());
        app.workspace.tick(Duration::from_millis(16));
        if !app.workspace.camera().is_animating() {
            break;
        }
    }
    assert!(!app.workspace.camera().is_animating());
    assert!(app.take_passing_sweep());
    assert!(!app.take_passing_sweep());
}

#[test]
fn a_new_camera_animation_does_not_consume_the_previous_idle_interval() {
    let mut app = context_inspector_test_app(false);
    let cards = vec![
        terminal_card_state(),
        terminal_card_state(),
        terminal_card_state(),
    ];
    let target = cards[2].id;
    app.durable = Arc::new(WorkspaceState {
        focused: Some(cards[0].id),
        cards: cards.into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(600.0);
    app.workspace.native_scroll(0.0, 600.0);
    // The last frame of whatever animated before, two idle seconds ago.
    let previous_frame = Instant::now() - Duration::from_secs(2);
    app.frame_clock.start(previous_frame);

    let _ = app.update(Message::FocusCard(target));
    // Read back rather than taken before the update, which a stalled
    // thread would put a frame or more behind the clock's own start.
    let navigated = app
        .frame_clock
        .frame()
        .expect("navigation starts the clock");
    // The first frame already moves the camera, by the time since the
    // navigation and not since that earlier frame.
    let _ = app.update(Message::Frame(navigated + Duration::from_millis(16)));
    let first = app.workspace.camera().offset();
    assert!(first > 0.0);
    assert!(first < app.workspace.camera().target() / 2.0);
    let _ = app.update(Message::Frame(navigated + Duration::from_millis(32)));
    assert!(app.workspace.camera().offset() > first);
}

#[test]
fn switching_inactive_codex_terminal_reveals_and_focuses_its_card() {
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    for window_focused in [false, true] {
        let mut app = context_inspector_test_app(false);
        let mut cards: Vec<_> = (0..4).map(|_| terminal_card_state()).collect();
        let first = cards[0].id;
        let unrelated = cards[1].id;
        let target = cards[3].id;
        let CardKind::Terminal(terminal) = &mut cards[3].kind else {
            unreachable!();
        };
        terminal.launch.program = "codex".into();
        terminal.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("codex-host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let source = terminal.clone();
        app.durable = Arc::new(WorkspaceState {
            focused: Some(first),
            cards: cards.into(),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(600.0);
        app.workspace.native_scroll(0.0, 600.0);
        let window = window::Id::RESERVED;
        app.core.set_main_window_id(Some(window));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window,
            window_focused,
        ))));
        assert_eq!(app.refresh_ui.focused, window_focused);
        app.observe_agent_update_for_card(target);
        app.observe_agent_update_for_card(unrelated);
        assert_eq!(app.workspace.focused(), Some(first));
        assert_eq!(
            app.workspace.visibility(target),
            Some(VisibilityClass::Offscreen)
        );
        assert!(app.sidebar_state.has_card_update(target));

        let _ = app.update(Message::ChatUi(chat_ui::Action::Switch(target)));

        assert_eq!(app.workspace.focused(), Some(target));
        assert_eq!(app.durable.focused, Some(target));
        assert!(app.workspace.camera().target() > app.workspace.camera().offset());
        // Navigation reveals the card even in an unfocused window, but only
        // viewing it in the focused window acknowledges its unread update.
        assert_eq!(app.sidebar_state.has_card_update(target), !window_focused);
        assert!(app.sidebar_state.has_card_update(unrelated));
        let update = app.workspace.tick(Duration::from_secs(1));
        let _ = app.apply_viewport(update, true, DurableChange::None);
        assert_eq!(
            app.workspace.visibility(target),
            Some(VisibilityClass::Visible)
        );
        assert_eq!(app.sidebar_state.has_card_update(target), !window_focused);
        assert!(app.sidebar_state.has_card_update(unrelated));

        // The source is preserved until the daemon confirms its exact identity.
        assert_eq!(
            app.card_state(target).unwrap().kind,
            CardKind::Terminal(source)
        );
        assert!(app.chat_start_pending(target));
    }
}

pub(super) fn live_agent_app(program: &str) -> (Cagents, CardId, tempfile::TempDir) {
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    let mut app = context_inspector_test_app(false);
    app.window_width = 1600.0;
    let mut card = terminal_card_state();
    let id = card.id;
    let CardKind::Terminal(terminal) = &mut card.kind else {
        unreachable!();
    };
    terminal.launch.program = program.into();
    terminal.continuity = TerminalContinuity::Live {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let terminal_id = terminal.id;
    app.durable = Arc::new(WorkspaceState {
        focused: Some(id),
        cards: vec![card].into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(1600.0);
    let directory = tempfile::tempdir().unwrap();
    let handle = HostedTerminalHandle::connect_at(
        directory.path().join("missing.sock"),
        terminal_id,
        None,
        |_| {},
    )
    .expect("terminal client starts before the socket accepts");
    app.cards
        .insert(id, CardRuntime::Terminal(TerminalSurface::from(handle)));
    (app, id, directory)
}

/// A terminal whose daemon connection has ended for good - its client
/// gives up once a killed daemon stays gone - says so in its own pane,
/// and the attachment that replaces it applies visibility as it starts,
/// so telling it whether it is on screen cannot fail in a way worth a
/// notice.
#[test]
fn a_terminal_whose_connection_ended_raises_no_visibility_notice() {
    let (mut app, card, _directory) = live_agent_app("sh");
    let Some(CardRuntime::Terminal(surface)) = app.cards.get(&card) else {
        unreachable!();
    };
    surface.stop_for_test();
    assert!(matches!(
        surface.hide(),
        Err(crate::terminal::TerminalSendError::Closed)
    ));
    app.set_workspace_surfaces_visible(false);
    app.set_workspace_surfaces_visible(true);
    assert!(
        app.status.is_none(),
        "{:?}",
        app.status.as_ref().map(|notice| &notice.message)
    );
}

/// A visibility failure names the same pane however the runtimes happen
/// to be stored, says how many others failed with it, and reports the
/// panes left showing stale content ahead of those left updating.
#[test]
fn a_visibility_notice_names_panes_in_card_order() {
    let mut app = context_inspector_test_app(false);
    let cards = ["Build", "Logs", "Shell"].map(|title| {
        let mut card = terminal_card_state();
        card.title = title.into();
        card
    });
    let [a, b, c] = cards.each_ref().map(|card| card.id);
    app.durable = Arc::new(WorkspaceState {
        cards: cards.into(),
        ..WorkspaceState::default()
    });
    let name = |app: &Cagents, card| app.notice_pane_name(card).unwrap();
    let reported = |app: &Cagents| {
        let notice = app.status.as_ref().expect("a visibility notice");
        assert_eq!(notice.key, Some(NoticeKey::SurfaceVisibility));
        (notice.message.clone(), notice.details.clone().unwrap())
    };

    for failures in [
        vec![(c, true, "c"), (a, false, "a"), (b, true, "b")],
        vec![(b, true, "b"), (c, true, "c"), (a, false, "a")],
    ] {
        app.report_surface_visibility(
            failures
                .into_iter()
                .map(|(card, visible, error)| (card, visible, error.to_owned()))
                .collect(),
        );
        assert_eq!(
            reported(&app),
            (
                format!(
                    "{} and 1 other pane may not show their latest content.",
                    name(&app, b)
                ),
                "b".to_owned()
            )
        );
    }

    app.report_surface_visibility(vec![
        (c, true, "c".into()),
        (b, true, "b".into()),
        (a, true, "a".into()),
    ]);
    assert_eq!(
        reported(&app).0,
        format!(
            "{} and 2 other panes may not show their latest content.",
            name(&app, a)
        )
    );
    app.report_surface_visibility(vec![(c, true, "c".into())]);
    assert_eq!(
        reported(&app).0,
        format!("{} may not show its latest content.", name(&app, c))
    );
    app.report_surface_visibility(vec![(a, false, "a".into())]);
    assert_eq!(
        reported(&app).0,
        format!("{} may keep updating while hidden.", name(&app, a))
    );
    app.report_surface_visibility(Vec::new());
    assert!(app.status.is_none(), "a clean pass withdraws the notice");
}

#[test]
fn terminal_review_revision_acknowledges_same_card_cancellation_and_focus_switch() {
    let (mut app, card, _directory) = live_agent_app("owned-review-fixture");
    let terminal = match app.cards.get(&card).unwrap() {
        CardRuntime::Terminal(handle) => handle.terminal_id(),
        _ => unreachable!(),
    };
    let initial = app.terminal_review_revision;
    drop(app.update(Message::TerminalReviewMode {
        card,
        terminal,
        active: true,
    }));
    assert_eq!(app.terminal_review_card, Some(card));
    assert_eq!(app.terminal_review_revision, initial + 1);
    // Entry and same-card activation can be coalesced before any widget view
    // observes the intermediate true owner. The final false revision differs.
    drop(app.update(Message::FocusCard(card)));
    assert_eq!(app.workspace.focused(), Some(card));
    assert_eq!(app.terminal_review_card, None);
    assert_eq!(app.terminal_review_revision, initial + 2);
    drop(app.update(Message::TerminalReviewMode {
        card,
        terminal,
        active: true,
    }));
    let acknowledged = app.terminal_review_revision;
    let other =
        WorkspaceCard::new(CardId::new(), "Owned other pane", CardWidth::Fraction(0.5)).unwrap();
    let other_id = other.id;
    app.workspace.add_card(other, 1, false).unwrap();
    let update = app.workspace.focus(other_id).unwrap();
    drop(app.apply_viewport(update, true, DurableChange::None));
    assert_eq!(app.terminal_review_card, None);
    assert_eq!(app.terminal_review_revision, acknowledged + 1);
}

#[test]
fn terminal_review_owner_is_revoked_when_its_runtime_identity_changes() {
    let (mut app, card, _directory) = live_agent_app("owned-review-fixture");
    let terminal = match app.cards.get(&card).unwrap() {
        CardRuntime::Terminal(handle) => handle.terminal_id(),
        _ => unreachable!(),
    };
    drop(app.update(Message::TerminalReviewMode {
        card,
        terminal,
        active: true,
    }));
    let revision = app.terminal_review_revision;
    let identity = app.terminal_review_identity;
    let (mut replacement, replacement_card, _replacement_directory) =
        live_agent_app("owned-replacement-fixture");
    app.cards
        .insert(card, replacement.cards.remove(&replacement_card).unwrap());
    app.reconcile_terminal_review_owner();
    assert!(identity.is_some());
    assert_eq!(app.terminal_review_card, None);
    assert_eq!(app.terminal_review_identity, None);
    assert_eq!(app.terminal_review_revision, revision + 1);
}

#[test]
fn queued_review_requests_cannot_claim_or_cancel_a_replacement_terminal() {
    let (mut app, card, _directory) = live_agent_app("owned-old-fixture");
    let old_terminal = match app.cards.get(&card).unwrap() {
        CardRuntime::Terminal(handle) => handle.terminal_id(),
        _ => unreachable!(),
    };
    let (mut replacement, replacement_card, _replacement_directory) =
        live_agent_app("owned-new-fixture");
    let replacement_runtime = replacement.cards.remove(&replacement_card).unwrap();
    let new_terminal = match &replacement_runtime {
        CardRuntime::Terminal(handle) => handle.terminal_id(),
        _ => unreachable!(),
    };
    app.cards.insert(card, replacement_runtime);
    let revision = app.terminal_review_revision;
    drop(app.update(Message::TerminalReviewMode {
        card,
        terminal: old_terminal,
        active: true,
    }));
    assert_eq!(app.terminal_review_card, None);
    assert_eq!(app.terminal_review_revision, revision);
    drop(app.update(Message::TerminalReviewMode {
        card,
        terminal: new_terminal,
        active: true,
    }));
    let accepted_revision = app.terminal_review_revision;
    assert_eq!(app.terminal_review_card, Some(card));
    assert_eq!(app.terminal_review_identity, Some(new_terminal));
    drop(app.update(Message::TerminalReviewMode {
        card,
        terminal: old_terminal,
        active: false,
    }));
    assert_eq!(app.terminal_review_card, Some(card));
    assert_eq!(app.terminal_review_identity, Some(new_terminal));
    assert_eq!(app.terminal_review_revision, accepted_revision);
    drop(app.update(Message::TerminalReviewMode {
        card,
        terminal: new_terminal,
        active: false,
    }));
    assert_eq!(app.terminal_review_card, None);
    assert_eq!(app.terminal_review_revision, accepted_revision + 1);
}

fn busy_chat_snapshot() -> crate::chat::Snapshot {
    use crate::chat::{
        Capabilities, Choice, Field, Item, ItemContent, Phase, Request, Role, Snapshot, ToolStatus,
    };

    Snapshot {
        generation: 2,
        busy: true,
        waiting: true,
        capabilities: Capabilities {
            steer: true,
            queue: true,
            ..Capabilities::default()
        },
        items: vec![
            Item {
                id: "user-1".into(),
                role: Role::User,
                content: ItemContent::Text {
                    text: "Inspect the switch".into(),
                    thinking: false,
                    phase: Phase::Final,
                },
                started_at: None,
                at: None,
            }
            .into(),
            Item {
                id: "tool-1".into(),
                role: Role::Assistant,
                content: ItemContent::Tool {
                    name: "shell".into(),
                    input: "ls".into(),
                    output: String::new(),
                    status: ToolStatus::Running,
                },
                started_at: None,
                at: None,
            }
            .into(),
            Item {
                id: "assistant-1".into(),
                role: Role::Assistant,
                content: ItemContent::Text {
                    text: "Still working.".into(),
                    thinking: false,
                    phase: Phase::Commentary,
                },
                started_at: None,
                at: None,
            }
            .into(),
        ],
        requests: vec![Request {
            kind: crate::chat::RequestKind::Approval,
            id: "request-1".into(),
            title: "Allow this command?".into(),
            description: "ls".into(),
            fields: vec![Field {
                id: "note".into(),
                label: "Note".into(),
                description: String::new(),
                choices: Vec::new(),
                multiple: false,
                secret: false,
                initial: String::new(),
            }],
            actions: vec![
                Choice {
                    id: "deny".into(),
                    label: "Deny".into(),
                    description: String::new(),
                },
                Choice {
                    id: "allow".into(),
                    label: "Allow".into(),
                    description: String::new(),
                },
            ],
        }],
        ..Snapshot::default()
    }
}

#[test]
fn switching_a_live_terminal_to_native_chat_draws_the_swap() {
    let (mut app, card, _socket) = live_agent_app("codex");
    let focus_id = workspace_widget::card_focus_id(card).to_string();
    let composer_id = card_control_id(card, "chat-composer").to_string();
    assert!(
        widget_id_counts(&mut app.view()).contains_key(&focus_id),
        "the terminal surface has to be mounted before the swap, or the crash frame is not drawn"
    );

    draw_frames(&mut app, 3, |app, frame| match frame {
        0 => {
            let _connect = app.update(Message::ChatUi(chat_ui::Action::Switch(card)));
            let ready = chat_ui::ready_message_for_pending_switch(
                app,
                card,
                crate::chat::Snapshot {
                    generation: 1,
                    title: Some("Live thread".into()),
                    ..crate::chat::Snapshot::default()
                },
            );
            let _focus = app.update(ready);
            assert!(
                matches!(
                    app.card_state(card).map(|state| &state.kind),
                    Some(CardKind::Chat(_))
                ),
                "a confirmed switch replaces the terminal card with native chat"
            );
            assert!(
                matches!(app.cards.get(&card), Some(CardRuntime::Chat(_))),
                "the chat runtime is what the next frame draws"
            );
        }
        1 => {
            assert_eq!(
                chat_ui::show_chat_snapshot_for_test(app, card, busy_chat_snapshot()),
                1,
                "the busy frame has to include the in-progress request"
            );
        }
        _ => {}
    });

    let mounted = widget_id_counts(&mut app.view());
    assert!(
        mounted.contains_key(&composer_id),
        "native chat keeps its composer: {mounted:?}"
    );
    assert!(
        !mounted.contains_key(&focus_id),
        "chat must not adopt the terminal focus id that still holds terminal widget state"
    );
}

#[test]
fn a_refused_switch_keeps_the_terminal_on_screen() {
    let (mut app, card, _socket) = live_agent_app("claude");
    draw_frames(&mut app, 2, |app, frame| {
        if frame == 0 {
            let _connect = app.update(Message::ChatUi(chat_ui::Action::Switch(card)));
            let _ = app.update(chat_ui::refused_switch_message(
                app,
                card,
                "fixture: provider refused the handoff",
            ));
        }
    });
    assert!(
        matches!(
            app.card_state(card).map(|state| &state.kind),
            Some(CardKind::Terminal(_))
        ),
        "a refused handoff leaves the terminal card in place"
    );
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Terminal(_))),
        "the terminal surface is still what the card draws"
    );
    assert!(
        app.notices.entries.iter().any(|entry| entry
            .notice
            .details
            .as_deref()
            .is_some_and(|details| details.contains("refused the handoff"))),
        "the refusal is recorded as a notice, not a process abort: {:?}",
        app.notices.entries
    );
}

#[test]
fn dirty_close_restores_the_actual_underlying_modal_focus_target() {
    let mut app = context_inspector_test_app(true);
    let project = ProjectId::new();
    app.worktree_dialog = Some(WorktreeDialog::new(
        project,
        PathBuf::from("/repo"),
        WorktreeDraft {
            base: "main".into(),
            destination: "/worktrees".into(),
            ..WorktreeDraft::default()
        },
    ));
    assert_eq!(
        app.nested_modal_focus_target().unwrap().to_string(),
        worktree_dialog_focus_id().to_string()
    );

    app.worktree_dialog = None;
    app.pull_request_dialog = Some(PullRequestDialog {
        project_id: ProjectId::new(),
        repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
        worktree: None,
        base: "main".into(),
        head: "topic".into(),
        title: String::new(),
        body: String::new().into(),
        draft: false,
        retry_confirmation: None,
    });
    assert_eq!(
        app.nested_modal_focus_target().unwrap().to_string(),
        pull_request_dialog_focus_id().to_string()
    );

    app.pull_request_dialog = None;
    app.window_width = 900.0;
    assert!(app.nested_modal_focus_target().is_none());
}

#[test]
fn a_narrow_pane_closes_its_inspector_and_one_reopened_there_covers_it_until_escape() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let escape = || Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        },
        captured: false,
    };
    let resize = |app: &mut Cagents, width: f32| {
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(width),
        }));
    };
    resize(&mut app, 1600.0);
    assert!(!app.pane_too_narrow(card));
    let _ = app.update(Message::ToggleContextInspector);
    app.settle_pane_inspectors();
    assert_eq!(
        app.pane_inspector_layout(card),
        Some(InspectorLayout::Docked)
    );
    // Docked, Escape is the pane content's, not the inspector's.
    let _ = app.update(escape());
    assert!(app.pane_inspectors.is_open(card));

    // Too narrow for both, the pane closes it by itself.
    resize(&mut app, 500.0);
    assert!(app.pane_too_narrow(card));
    assert!(!app.pane_inspectors.is_open(card));
    assert_eq!(app.pane_inspector_layout(card), None);
    assert!(!app.workspace_inputs_blocked());

    // Reopened there, it covers the pane's content and Escape closes it.
    let _ = app.update(Message::TogglePaneInspector(card));
    app.settle_pane_inspectors();
    assert_eq!(
        app.pane_inspector_layout(card),
        Some(InspectorLayout::Overlay)
    );
    assert!(
        !app.card_holds_keyboard(card),
        "covered content cannot take input"
    );
    assert!(!app.workspace_inputs_blocked(), "other panes stay usable");
    assert_eq!(
        app.card_keyboard_focus(card),
        card_control_id(card, "inspector.toggle"),
        "a pane focus request lands in the inspector, not its hidden content"
    );
    assert_eq!(app.focus_pane_keyboard_after_press(card).units(), 0);
    assert_eq!(app.focus_pane_keyboard_after_entry(card).units(), 0);
    resize(&mut app, 520.0);
    assert!(
        app.pane_inspectors.is_open(card),
        "narrowness does not close it again"
    );
    let _ = app.update(escape());
    assert!(!app.pane_inspectors.is_open(card));
    assert_eq!(
        app.pane_inspector_layout(card),
        Some(InspectorLayout::Overlay),
        "the closing panel must not squeeze the content into a docked row"
    );
    assert!(
        app.pane_content_inputs_blocked(card),
        "the slide still covers content"
    );
    app.settle_pane_inspectors();
    assert_eq!(app.pane_inspector_layout(card), None);
    assert!(!app.pane_content_inputs_blocked(card));
}

#[test]
fn the_inspector_shortcut_toggles_the_focused_pane_and_honors_bindings_and_modals() {
    let press = |key: &str, modifiers, repeat| keyboard::Event::KeyPressed {
        key: keyboard::Key::Character(key.into()),
        modified_key: keyboard::Key::Character(key.into()),
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyI),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat,
    };
    let default = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    for captured in [false, true] {
        for custom in [false, true] {
            let mut app = context_inspector_test_app(true);
            let card = app.workspace.focused().unwrap();
            let modifiers = if custom {
                app.settings.keyboard_shortcuts.toggle_inspector = vec!["Ctrl+Alt+I".into()];
                keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT
            } else {
                default
            };
            if custom {
                // The retired default no longer toggles it.
                let _ = app.update(Message::KeyEvent {
                    event: press("i", default, false),
                    captured,
                });
                assert!(!app.pane_inspectors.is_open(card));
            }
            let _ = app.update(Message::KeyEvent {
                event: press("i", modifiers, false),
                captured,
            });
            assert!(app.pane_inspectors.is_open(card));
            let _ = app.update(Message::KeyEvent {
                event: press("i", modifiers, true),
                captured,
            });
            assert!(
                app.pane_inspectors.is_open(card),
                "repeat must not toggle twice"
            );

            // A form dialog owns keyboard input.
            app.device_ui.open = true;
            assert!(app.modal_is_open());
            let _ = app.update(Message::KeyEvent {
                event: press("i", modifiers, false),
                captured,
            });
            assert!(app.device_ui.open);
            assert!(app.pane_inspectors.is_open(card));
            app.device_ui.open = false;

            let _ = app.update(Message::KeyEvent {
                event: press("i", modifiers, false),
                captured,
            });
            assert!(!app.pane_inspectors.is_open(card));
            assert_eq!(app.workspace.focused(), Some(card));
        }
    }
}

#[test]
fn context_inspector_resize_persists_only_when_the_gesture_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(true);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;

    let _live_resize = app.update(Message::ContextInspector(InspectorAction::Resize {
        width: 384.0,
    }));
    assert_eq!(app.settings.inspector_width, 384.0);
    assert!(!path.exists(), "live key repeats must not persist settings");

    let finished = app.update(Message::ContextInspector(InspectorAction::ResizeFinished));
    settle_settings_task(&mut app, finished);
    assert_eq!(settings::load(&path).unwrap().inspector_width, 384.0);
}

#[test]
fn embedded_application_icon_is_valid_rgba() {
    let (rgba, size) = application_window_icon().into_raw();
    assert_eq!(size.width, APPLICATION_ICON_SIZE);
    assert_eq!(size.height, APPLICATION_ICON_SIZE);
    assert_eq!(
        rgba.len(),
        (APPLICATION_ICON_SIZE * APPLICATION_ICON_SIZE * 4) as usize
    );
}

#[test]
fn proof_backed_continuity_is_attach_only() {
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    let launch = launch_recipe("/tmp");
    let mut session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        },
    };
    assert!(matches!(
        terminal_start_mode(&session),
        Ok(TerminalStartMode::Attach(_))
    ));

    session.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: launch,
        last_exit: None,
    };
    assert!(matches!(
        terminal_start_mode(&session),
        Ok(TerminalStartMode::Spawn(_))
    ));
    assert_eq!(
        restored_runtime_residency(
            &CardKind::Terminal(session),
            Exposure::Visible,
            &Residency::Cold,
        ),
        Residency::Hibernated
    );
}

#[test]
fn daemon_rejection_preserves_identity_until_safe_recovery() {
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        terminal::ipc::{
            ClientEnvelope, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope,
            ServerMessage, read_frame, write_frame,
        },
    };
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let old_proof = HostSessionProof {
        host_session_id: HostSessionId::new("old-session"),
        boot_id: BootId::new("same-boot"),
        host_incarnation: HostIncarnation::new("old-incarnation"),
    };
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: HostSessionProof {
                    host_session_id: HostSessionId::new("new-session"),
                    boot_id: BootId::new("same-boot"),
                    host_incarnation: HostIncarnation::new("new-incarnation"),
                },
                message: ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::ProofMismatch,
                    "daemon was replaced",
                    false,
                )),
            },
        )
        .unwrap();
    });

    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let state = app.card_state_mut(card).unwrap();
    let CardKind::Terminal(session) = &mut state.kind else {
        panic!("expected terminal fixture");
    };
    let old_id = session.id;
    session.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: old_proof.host_session_id.clone(),
        boot_id: old_proof.boot_id.clone(),
        host_incarnation: old_proof.host_incarnation.clone(),
    };
    let original = session.clone();
    app.lifecycle.register(
        card,
        Exposure::Visible,
        Residency::Hydrating { epoch: 1 },
        Instant::now(),
    );
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = HostedTerminalHandle::connect_at(socket, old_id, Some(old_proof), move |wake| {
        let _ = wake_tx.send(wake);
    })
    .unwrap();
    let wake = wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    // A rejected handshake wakes its consumer before it queues the error,
    // so the wake alone does not mean there is anything to drain yet.
    let events = handle.events();
    let deadline = Instant::now() + Duration::from_secs(5);
    while events.is_empty() {
        assert!(Instant::now() < deadline, "the rejection was never queued");
        std::thread::sleep(Duration::from_millis(1));
    }
    // The subscription can deliver this edge before the start task returns.
    let _ = app.update(Message::TerminalWake {
        card,
        generation: wake.generation,
    });
    let _ = app.update(Message::TerminalStarted {
        card,
        terminal: old_id,
        result: Ok(StartedTerminal {
            handle: TerminalSurface::from(handle),
            session: original.clone(),
            spawned: false,
            save_failure: None,
        }),
    });
    assert_eq!(
        &app.card_state(card).unwrap().kind,
        &CardKind::Terminal(original.clone())
    );
    // Discovery has been scheduled, but no unauthenticated replacement
    // identity or replay recipe may enter the durable state.
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    let _ = app.update(Message::TerminalStarted {
        card,
        terminal: old_id,
        result: Err("replacement ownership cannot be established".into()),
    });
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    let _ = app.retry_card(card);
    assert_eq!(
        &app.card_state(card).unwrap().kind,
        &CardKind::Terminal(original)
    );
}

#[test]
fn recovered_live_terminal_identity_survives_save_exit_and_reload() {
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recovered.sqlite3");
    let boot = BootId::new("recovered-terminal-test");
    let (writer, _) = SessionRuntime::open(&path, boot.clone()).unwrap();
    let store = writer.operation_store();
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(original) = app.card_state(card).unwrap().kind.clone() else {
        panic!("expected terminal");
    };
    writer.snapshot(app.durable.clone());
    writer.flush(Duration::from_secs(5)).unwrap();
    app.session = Some(writer);
    app.cards.insert(card, CardRuntime::Loading);
    app.lifecycle.register(
        card,
        Exposure::Visible,
        Residency::Hydrating { epoch: 1 },
        Instant::now(),
    );
    let recovered = TerminalSession {
        id: TerminalId::new(),
        recovery_origin: Some(original.id),
        predecessor: None,
        continuity: TerminalContinuity::Live {
            host_session_id: HostSessionId::new("replacement-host"),
            boot_id: boot.clone(),
            host_incarnation: HostIncarnation::new("replacement-incarnation"),
        },
        ..original.clone()
    };
    drop(app.terminal_started(
        card,
        original.id,
        Ok(StartedTerminal {
            handle: unconnected_surface(directory.path(), recovered.id),
            session: recovered.clone(),
            spawned: false,
            save_failure: None,
        }),
    ));
    let writer = app.session.as_ref().unwrap();
    writer.flush(Duration::from_secs(5)).unwrap();
    let saved = store
        .load_card(card, recovered.continuity.live_proof().as_ref())
        .unwrap()
        .unwrap();
    assert_eq!(saved.kind, CardKind::Terminal(recovered.clone()));

    // The installed window remained blocked even after its recovered
    // process exited. Its next save and the update flush must succeed too.
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!();
    };
    transition_terminal_to_relaunchable(
        session,
        TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 42,
        },
    );
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(5))
        .unwrap();
    let saved = store.load_card(card, None).unwrap().unwrap();
    let CardKind::Terminal(saved) = saved.kind else {
        unreachable!()
    };
    assert_eq!(saved.id, recovered.id);
    assert_eq!(saved.recovery_origin, Some(original.id));
    assert!(matches!(
        saved.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: Some(_),
            ..
        }
    ));
}

#[test]
fn exited_terminal_retries_with_fresh_spawn_identity_and_exit_metadata() {
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    let launch = launch_recipe("/worktree");
    let mut session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: Some("stale-provider-session".into()),
        continuity: TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        },
    };
    let exited_id = session.id;
    let last_exit = TerminalExit {
        code: Some(17),
        signal: None,
        occurred_at_unix_ms: 42,
    };

    transition_terminal_to_relaunchable(&mut session, last_exit.clone());
    assert_eq!(
        session.continuity,
        TerminalContinuity::RelaunchAvailable {
            recipe: launch.clone(),
            last_exit: Some(last_exit.clone()),
        }
    );

    let relaunched = fresh_terminal_relaunch(&session);
    assert_ne!(relaunched.id, exited_id);
    // It succeeds exactly the session it replaces.
    assert_eq!(relaunched.predecessor, Some(exited_id));
    assert_eq!(relaunched.launch, launch.clone());
    assert_eq!(relaunched.resume_hint, None);
    assert!(matches!(
        terminal_start_mode(&relaunched),
        Ok(TerminalStartMode::Spawn(recipe)) if recipe == launch
    ));
    assert!(matches!(
        relaunched.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: Some(exit),
            ..
        } if exit == last_exit
    ));
}

/// However it ended, a restored terminal is presented the same way it was
/// left: an ended card that states its outcome and offers a relaunch, not
/// a failed runtime.
#[test]
fn restored_child_exit_stays_retryable_without_starting_the_stale_identity() {
    let launch = launch_recipe("/worktree");
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: Some(TerminalExit {
                code: Some(17),
                signal: None,
                occurred_at_unix_ms: 42,
            }),
        },
    };

    assert_eq!(
        restored_runtime_residency(
            &CardKind::Terminal(session),
            Exposure::Visible,
            &Residency::Hot,
        ),
        Residency::Hibernated
    );
}

fn restored_live_terminal(program: &str) -> CardState {
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    let mut card = terminal_card_state();
    let CardKind::Terminal(session) = &mut card.kind else {
        unreachable!("terminal fixture");
    };
    session.launch.program = program.into();
    session.continuity = TerminalContinuity::Live {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    card
}

/// All saved owners, including plain shells, use one metadata-only lane.
#[test]
fn restored_agents_attach_one_at_a_time_after_visible_cards_start() {
    let mut app = context_inspector_test_app(false);
    let visible = restored_live_terminal("/bin/sh");
    let cards = [
        visible.clone(),
        restored_live_terminal("/bin/sh"),
        restored_live_terminal("/usr/bin/claude"),
        restored_live_terminal("/usr/bin/codex"),
    ];
    app.durable = Arc::new(WorkspaceState {
        focused: Some(visible.id),
        cards: cards.to_vec().into(),
        ..WorkspaceState::default()
    });
    app.set_exposure(visible.id, Exposure::Visible);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    let focus = app.workspace.focused();
    let strip = app.workspace.order().collect::<Vec<_>>();
    drop(app.initialize_runtimes());
    let notices = app.notices.entries.len();
    for (index, card) in cards.iter().enumerate() {
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert!(app.terminal_start_in_flight(card.id));
        assert!(app.restored_agent_attaches.is_empty());
        assert_eq!(app.card_state(card.id).unwrap().kind, card.kind);
        let result = if index == cards.len() - 1 {
            Err("the daemon is unavailable".into())
        } else {
            Ok(None)
        };
        saved_terminal::tests::complete_inspection(&mut app, card.id, result);
        assert_eq!(app.saved_terminal_ready(card.id), index != cards.len() - 1);
        assert_eq!(app.card_state(card.id).unwrap().kind, card.kind);
        assert!(matches!(
            app.cards.get(&card.id),
            Some(CardRuntime::Loading)
        ));
        assert!(app.lifecycle.state(card.id).is_none());
        assert_eq!(app.workspace.focused(), focus);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), strip);
    }
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert_eq!(app.notices.entries.len(), notices);
}

#[test]
fn startup_qualification_attaches_offscreen_shell_omp_and_custom_terminal_owners_serially() {
    let mut app = context_inspector_test_app(false);
    let terminals =
        ["/bin/sh", "/usr/bin/omp", "/fixture/custom-wrapper"].map(restored_live_terminal);
    app.durable = Arc::new(WorkspaceState {
        cards: terminals.to_vec().into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    drop(app.initialize_runtimes());
    for card in &terminals {
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert!(app.terminal_start_in_flight(card.id));
        assert!(app.restored_agent_attaches.is_empty());
        assert_ne!(
            app.startup_pane_check(),
            Ok(true),
            "pending inspection cannot qualify"
        );
        saved_terminal::tests::complete_inspection(&mut app, card.id, Ok(None));
        assert!(app.saved_terminal_ready(card.id));
        assert_eq!(app.card_state(card.id).unwrap().kind, card.kind);
    }
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert_eq!(app.startup_pane_check(), Ok(true));
}

#[test]
fn a_queued_agent_attach_yields_to_the_cards_own_start() {
    let mut app = context_inspector_test_app(false);
    let agent = restored_live_terminal("/usr/bin/claude");
    app.durable = Arc::new(WorkspaceState {
        cards: vec![agent.clone()].into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.cards.insert(agent.id, CardRuntime::Loading);
    app.lifecycle.register(
        agent.id,
        Exposure::Visible,
        Residency::Hydrating { epoch: 1 },
        Instant::now(),
    );
    app.restored_agent_attaches.push_back(agent.id);
    drop(app.attach_next_restored_agent());
    assert!(app.restored_agent_attaches.is_empty());
    assert_eq!(app.terminal_starts_in_flight(), 0);
}

/// A restored agent off screen, registered as the queue leaves it.
fn hibernated_agent_app(
    continuity: Option<TerminalContinuity>,
) -> (Cagents, CardId, TerminalSession) {
    let mut app = context_inspector_test_app(false);
    let mut agent = restored_live_terminal("/usr/bin/claude");
    let CardKind::Terminal(session) = &mut agent.kind else {
        unreachable!("terminal fixture");
    };
    if let Some(continuity) = continuity {
        session.continuity = continuity;
    }
    let session = session.clone();
    app.durable = Arc::new(WorkspaceState {
        cards: vec![agent.clone()].into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.cards.insert(agent.id, CardRuntime::Loading);
    app.lifecycle.register(
        agent.id,
        Exposure::Offscreen,
        Residency::Hibernated,
        Instant::now(),
    );
    (app, agent.id, session)
}

pub(super) fn unconnected_surface(directory: &Path, terminal: TerminalId) -> TerminalSurface {
    TerminalSurface::from(
        HostedTerminalHandle::connect_at(directory.join("missing.sock"), terminal, None, |_| {})
            .expect("terminal client starts before the socket accepts"),
    )
}

fn surface_visibility(app: &Cagents, card: CardId) -> crate::terminal_ipc::TerminalVisibility {
    match app.cards.get(&card) {
        Some(CardRuntime::Terminal(surface)) => surface.visibility(),
        _ => panic!("expected an attached terminal"),
    }
}

/// An agent in a worktree being deleted waits in the queue rather than
/// leaving it, so a deletion that fails still has its row attached.
#[test]
fn an_agent_waits_out_its_worktrees_deletion_for_its_row_attach() {
    let (mut app, card, session) = hibernated_agent_app(None);
    let target = app
        .project_ui
        .begin_deleting(session.launch.working_directory.clone());
    app.restored_agent_attaches.push_back(card);
    drop(app.attach_next_restored_agent());
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert_eq!(app.restored_agent_attaches, [card]);

    let _ = app.update(Message::ProjectUi(project_ui::Action::Deleted {
        capture: app.capture_worktree_removal(&target),
        target,
        result: Err("the worktree has changes".into()),
    }));
    // The card is started, by the row attach or by the sweep hydrating it
    // outright, and no hibernated agent is left waiting on the deletion.
    assert_eq!(app.terminal_starts_in_flight(), 1);
    assert!(app.restored_agent_attaches.iter().all(|queued| !matches!(
        app.lifecycle.state(*queued),
        Some((_, Residency::Hibernated))
    )));
}

/// An attach for the row keeps the card hibernated, so it streams no
/// screen, and opening the card later reuses that connection.
#[test]
fn an_attached_agent_stays_hibernated_until_its_card_is_opened() {
    use crate::terminal_ipc::TerminalVisibility;

    let directory = tempfile::tempdir().unwrap();
    let (mut app, card, session) = hibernated_agent_app(None);
    app.restored_agent_attaches.push_back(card);
    drop(app.attach_next_restored_agent());
    assert_eq!(app.terminal_starts_in_flight(), 1);
    let _ = app.update(Message::RestoredAgentAttached {
        card,
        terminal: session.id,
        result: Ok(Some(StartedTerminal {
            handle: unconnected_surface(directory.path(), session.id),
            session: session.clone(),
            spawned: false,
            save_failure: None,
        })),
    });
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert_eq!(
        surface_visibility(&app, card),
        TerminalVisibility::Hibernated
    );
    assert_eq!(
        app.lifecycle.state(card).map(|(_, residency)| residency),
        Some(&Residency::Hibernated)
    );

    drop(app.reconcile_lifecycle(&HashSet::from([card]), &HashSet::new(), &HashSet::new()));
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert_ne!(
        surface_visibility(&app, card),
        TerminalVisibility::Hibernated
    );
    assert!(!matches!(
        app.lifecycle.state(card).map(|(_, residency)| residency),
        Some(Residency::Hibernated)
    ));
}

/// Opening the card while its row's attach is still under way starts the
/// card's own attach too; whichever lands first is kept, and the other is
/// let go without failing the card.
#[test]
fn an_agent_opened_during_its_row_attach_keeps_one_connection() {
    use crate::terminal_ipc::TerminalVisibility;

    let directory = tempfile::tempdir().unwrap();
    let (mut app, card, session) = hibernated_agent_app(None);
    app.restored_agent_attaches.push_back(card);
    drop(app.attach_next_restored_agent());
    drop(app.reconcile_lifecycle(&HashSet::from([card]), &HashSet::new(), &HashSet::new()));
    assert_eq!(app.terminal_starts_in_flight(), 2);
    let started = || StartedTerminal {
        handle: unconnected_surface(directory.path(), session.id),
        session: session.clone(),
        spawned: false,
        save_failure: None,
    };
    let _ = app.update(Message::RestoredAgentAttached {
        card,
        terminal: session.id,
        result: Ok(Some(started())),
    });
    assert_eq!(app.terminal_starts_in_flight(), 1);
    // The card is being opened, so the row's attach is not kept hibernated.
    assert_ne!(
        surface_visibility(&app, card),
        TerminalVisibility::Hibernated
    );
    let _ = app.update(Message::TerminalStarted {
        card,
        terminal: session.id,
        result: Ok(started()),
    });
    assert_eq!(app.terminal_starts_in_flight(), 0);
    assert!(matches!(
        app.cards.get(&card),
        Some(CardRuntime::Terminal(_))
    ));
    assert!(app.notices.entries.is_empty(), "{:?}", app.notices.entries);
}

/// Losing the daemon connection of an agent attached only for its row
/// queues another attach, and never launches the agent again: that is the
/// user's decision, made by opening the card.
#[test]
fn a_hibernated_agent_that_loses_its_daemon_is_only_attached_again() {
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        terminal::ipc::{
            ClientEnvelope, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope,
            ServerMessage, read_frame, write_frame,
        },
    };
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: HostSessionProof {
                    host_session_id: HostSessionId::new("replacement"),
                    boot_id: BootId::new("boot"),
                    host_incarnation: HostIncarnation::new("replacement"),
                },
                message: ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::ProofMismatch,
                    "daemon was replaced",
                    false,
                )),
            },
        )
        .unwrap();
    });
    let (mut app, card, session) = hibernated_agent_app(None);
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = HostedTerminalHandle::connect_at(socket, session.id, Some(proof), move |wake| {
        let _ = wake_tx.send(wake);
    })
    .unwrap();
    app.cards
        .insert(card, CardRuntime::Terminal(TerminalSurface::from(handle)));
    server.join().unwrap();
    let notices = app.notices.entries.len();
    // Connecting can announce a repaint before the protocol error arrives.
    // Drive every wake as the application does, rather than assuming the
    // first notification already contains the failed attachment.
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(app.cards.get(&card), Some(CardRuntime::Terminal(_))) {
        let wake = wake_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the failed attachment wakes the application");
        drop(app.update(Message::TerminalWake {
            card,
            generation: wake.generation,
        }));
    }
    // The attach-only path took it up again; a relaunch would have made
    // the card hydrate.
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert!(app.restored_agent_attaches.is_empty());
    assert_eq!(app.terminal_starts_in_flight(), 1);
    assert_eq!(
        app.lifecycle.state(card),
        Some((&Exposure::Offscreen, &Residency::Hibernated))
    );
    assert_eq!(
        app.card_state(card).map(|_| app.residency(card)),
        Some(&Residency::Hibernated)
    );
    assert_eq!(app.notices.entries.len(), notices);
}

#[test]
fn a_background_read_reselects_only_the_scope_its_project_holds() {
    use crate::model::{ChannelId, ProjectChannel, WorktreeId};
    use crate::sidebar::{SidebarProject, SidebarWorktree};

    let mut app = context_inspector_test_app(false);
    let [holding, other] = [ProjectId::new(), ProjectId::new()];
    for (project, root) in [(holding, "/repo"), (other, "/elsewhere")] {
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            root.into(),
            vec![SidebarWorktree::from_durable(
                WorktreeId::new(),
                format!("{root}/worktrees/feature").into(),
                Some("feature".into()),
            )],
            Vec::new(),
        ));
    }
    let launch = Path::new("/repo/worktrees/feature/src");
    assert!(app.project_owns_active_scope(holding, launch));
    assert!(!app.project_owns_active_scope(other, launch));
    let channel = ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: other,
            title: "channel".into(),
            ordinal: 0,
            shared: false,
            cards: Vec::new(),
            focused: None,
        });
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    assert!(app.project_owns_active_scope(other, launch));
    assert!(!app.project_owns_active_scope(holding, launch));
}

#[test]
fn restored_successful_terminal_is_completed_without_automatic_relaunch() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        panic!("terminal fixture");
    };
    transition_terminal_to_relaunchable(session, terminal_exit(0, None));
    let identity = session.id;
    assert!(terminal_completed(session));
    drop(app.initialize_runtimes());
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
    assert!(app.status.is_none());
    assert!(
        app.lifecycle
            .reconcile(&HashSet::from([card]), &HashSet::new(), Instant::now())
            .is_empty()
    );
    let session = app.ended_terminal(card).unwrap();
    assert_eq!(session.id, identity);
    // Relaunch remains an explicit action with a fresh host identity.
    assert_ne!(fresh_terminal_relaunch(session).id, identity);
}

/// A channel and a repository with two worktrees, each holding a terminal.
pub(super) fn alt_tab_test_app() -> (Cagents, crate::model::ChannelId) {
    use crate::model::{ChannelId, ProjectChannel, WorktreeId};
    use crate::sidebar::{SidebarProject, SidebarWorktree};

    let mut app = context_inspector_test_app(false);
    let project = ProjectId::new();
    let channel = ChannelId::new();
    let cards: Vec<_> = ["/repo", "/repo/feature", "/repo"]
        .into_iter()
        .map(|path| {
            let mut card = terminal_card_state();
            let CardKind::Terminal(session) = &mut card.kind else {
                unreachable!()
            };
            session.launch.working_directory = path.into();
            card
        })
        .collect();
    let channel_card = cards[2].id;
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards = cards.into();
    durable.channels.push(ProjectChannel {
        title_source: Default::default(),
        id: channel,
        project_id: project,
        title: "Migration".into(),
        ordinal: 0,
        shared: false,
        cards: vec![channel_card],
        focused: Some(channel_card),
    });
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "project".into(),
        "/repo".into(),
        vec![
            SidebarWorktree::from_durable(WorktreeId::new(), "/repo".into(), Some("main".into())),
            SidebarWorktree::from_durable(
                WorktreeId::new(),
                "/repo/feature".into(),
                Some("feature".into()),
            ),
        ],
        Vec::new(),
    ));
    app.refresh_sidebar_channels();
    (app, channel)
}

/// Committing a switcher choice is navigation, saved within
/// [`NAVIGATION_SAVE_DELAY`] like a sidebar click rather than at once on
/// the UI thread, and the deferred save carries the visit order.
#[test]
fn committing_a_switcher_choice_saves_with_the_navigation_burst() {
    use super::terminal_output_tests::persists;
    use keyboard::key::{Code, Named};

    let (mut app, _) = alt_tab_test_app();
    let destinations = sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state);
    for destination in &destinations {
        drop(app.update(Message::Sidebar(destination.clone())));
    }
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert!(app.navigation_save.is_none());
    let saved = persists();
    let previous = app.active_workspace_destination();

    let alt = keyboard::Modifiers::ALT;
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Tab),
            modified_key: keyboard::Key::Named(Named::Tab),
            physical_key: keyboard::key::Physical::Code(Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: alt,
            text: None,
            repeat: false,
        },
        captured: true,
    }));
    let since = app.chrome_ui.workspace_preview_since.unwrap();
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::ModifiersChanged(keyboard::Modifiers::empty()),
        captured: false,
    }));
    drop(app.update(Message::WorkspaceSwitcherReleased(since)));
    let chosen = app.active_workspace_destination();
    assert_ne!(chosen, previous, "the switcher opened its choice");
    assert_eq!(persists(), saved, "the switch waits for the burst's save");
    assert_eq!(app.navigation_save, Some(DeferredSave::Views));

    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert_eq!(persists(), saved + 1, "the burst is saved once");
    assert!(app.navigation_save.is_none());
    assert_eq!(
        app.durable.workspace_history.first().copied(),
        chosen,
        "the saved history leads with the workspace chosen"
    );
}

/// Workspace previews freeze visit order until Alt is released, including
/// channel identities that share their project's worktree directory.
#[test]
fn alt_tab_previews_workspace_history_and_commits_only_a_choice() {
    let (mut app, channel) = alt_tab_test_app();

    let destinations = sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state);
    let root = workspace_destination(&destinations[1]).unwrap();
    let feature = workspace_destination(&destinations[2]).unwrap();
    let channel = WorkspaceDestination::Channel(channel);
    // Sidebar order is channel, root, feature; visit order deliberately differs.
    for index in [1, 2, 0, 1] {
        drop(app.update(Message::Sidebar(destinations[index].clone())));
    }
    use keyboard::key::{Code, Named};
    let key = |named, code, modifiers| Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(named),
            modified_key: keyboard::Key::Named(named),
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        },
        captured: true,
    };
    let next = |modifiers| key(Named::Tab, Code::Tab, modifiers);
    let released = || Message::KeyEvent {
        event: keyboard::Event::ModifiersChanged(keyboard::Modifiers::empty()),
        captured: false,
    };
    // The release is decided one message after it is seen.
    let release = |app: &mut Cagents| {
        let since = app.chrome_ui.workspace_preview_since;
        drop(app.update(released()));
        if let Some(since) = since {
            drop(app.update(Message::WorkspaceSwitcherReleased(since)));
        }
    };
    let preview = |app: &Cagents| {
        app.chrome_ui
            .workspace_preview
            .as_ref()
            .and_then(workspace_destination)
    };
    let alt = keyboard::Modifiers::ALT;
    drop(app.update(next(alt)));
    assert_eq!(app.active_workspace_destination(), Some(root));
    assert_eq!(
        app.chrome_ui
            .workspace_preview
            .as_ref()
            .and_then(workspace_destination),
        Some(channel)
    );
    drop(app.update(next(alt)));
    assert_eq!(app.active_workspace_destination(), Some(root));
    assert_eq!(
        app.chrome_ui
            .workspace_preview
            .as_ref()
            .and_then(workspace_destination),
        Some(feature)
    );
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::ModifiersChanged(alt | keyboard::Modifiers::SHIFT),
        captured: false,
    }));
    drop(app.update(next(alt | keyboard::Modifiers::SHIFT)));
    assert_eq!(app.active_workspace_destination(), Some(root));
    release(&mut app);
    assert_eq!(app.active_workspace_destination(), Some(channel));
    // Intermediate feature preview did not become more recent than root.
    drop(app.update(next(alt)));
    assert_eq!(app.active_workspace_destination(), Some(channel));
    assert_eq!(
        app.chrome_ui
            .workspace_preview
            .as_ref()
            .and_then(workspace_destination),
        Some(root)
    );
    drop(app.update(next(alt)));
    assert_eq!(preview(&app), Some(feature));
    // Losing the window is not a choice of workspace, although the seat
    // reports the modifiers released on the way out.
    app.core.set_main_window_id(Some(window::Id::RESERVED));
    let since = app.chrome_ui.workspace_preview_since.unwrap();
    drop(app.update(released()));
    drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
        window::Id::RESERVED,
        false,
    ))));
    drop(app.update(Message::WorkspaceSwitcherReleased(since)));
    assert_eq!(preview(&app), None);
    assert_eq!(app.active_workspace_destination(), Some(channel));
    // Nor is Escape, and neither disturbed the visit order.
    drop(app.update(next(alt)));
    assert_eq!(preview(&app), Some(root));
    drop(app.update(key(Named::Escape, Code::Escape, alt)));
    assert_eq!(preview(&app), None);
    assert_eq!(app.active_workspace_destination(), Some(channel));
    // The arrows walk the same list, and Enter opens the selection.
    drop(app.update(next(alt)));
    drop(app.update(key(Named::ArrowDown, Code::ArrowDown, alt)));
    assert_eq!(preview(&app), Some(feature));
    drop(app.update(key(Named::ArrowUp, Code::ArrowUp, alt)));
    assert_eq!(preview(&app), Some(root));
    drop(app.update(key(Named::ArrowDown, Code::ArrowDown, alt)));
    drop(app.update(key(Named::Enter, Code::Enter, alt)));
    assert_eq!(preview(&app), None);
    assert_eq!(app.active_workspace_destination(), Some(feature));
    drop(app.update(next(alt)));
    // Coming back round to the open workspace and releasing cancels.
    drop(app.update(next(alt)));
    drop(app.update(next(alt)));
    assert_eq!(preview(&app), Some(feature));
    release(&mut app);
    assert_eq!(preview(&app), None);
    assert_eq!(app.active_workspace_destination(), Some(feature));
    // A workspace opened by a click during the hold is the choice, and the
    // release that follows does not undo it.
    drop(app.update(next(alt)));
    assert_eq!(preview(&app), Some(channel));
    drop(app.update(Message::Sidebar(destinations[1].clone())));
    assert_eq!(preview(&app), None);
    assert_eq!(app.active_workspace_destination(), Some(root));
    release(&mut app);
    assert_eq!(app.active_workspace_destination(), Some(root));
    app.settings_reset_confirm = true;
    drop(app.update(next(alt)));
    assert_eq!(preview(&app), None);
    assert_eq!(app.active_workspace_destination(), Some(root));
}

fn workspace_switcher_test_app(count: usize) -> Cagents {
    let mut app = context_inspector_test_app(false);
    *app.sidebar_model.projects = (0..count)
        .map(|index| {
            SidebarProject::new(
                ProjectId::new(),
                RepositoryId::new(),
                format!("Folder {index}"),
                format!("/folder-{index}").into(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder)
        })
        .collect();
    app.active_workspace = Some("/folder-0".into());
    app
}

#[test]
fn workspace_switcher_home_end_hover_and_stale_actions_use_the_frozen_hold() {
    let mut app = workspace_switcher_test_app(20);
    drop(app.cycle_workspace(
        crate::workspace::FocusDirection::Right,
        keyboard::Modifiers::ALT,
    ));
    let first_hold = app.chrome_ui.workspace_preview_since.unwrap();
    assert_eq!(app.chrome_ui.workspace_switcher_rows.len(), 20);
    let key = |key: keyboard::key::Named| Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::End),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::ALT,
            text: None,
            repeat: false,
        },
        captured: true,
    };
    drop(app.update(key(keyboard::key::Named::End)));
    assert_eq!(app.workspace_history.cycle_index(), Some(19));
    assert!(app.workspace_switcher_shows_row(19));
    assert!(app.workspace_switcher_row_clear_of_fades(19));
    assert!(!app.workspace_switcher_shows_row(0));
    drop(app.update(key(keyboard::key::Named::Home)));
    assert_eq!(app.workspace_history.cycle_index(), Some(0));
    assert_eq!(app.chrome_ui.workspace_switcher_scroll, 0.0);
    drop(app.update(Message::WorkspaceSwitcherSelected {
        since: first_hold,
        index: 15,
    }));
    assert_eq!(app.workspace_history.cycle_index(), Some(15));
    assert!(app.chrome_ui.workspace_switcher_scroll > 0.0);
    assert!(app.workspace_switcher_row_clear_of_fades(15));
    let current_scope = app.active_workspace_destination().unwrap();
    let saved_during_preview = app.persistent_workspace();
    assert_eq!(
        saved_during_preview.workspace_history.first(),
        Some(&current_scope)
    );
    assert_eq!(app.workspace_history.cycle_index(), Some(15));
    // Rows describe the same destinations throughout this hold, even if
    // an unrelated sidebar refresh updates their titles.
    app.sidebar_model.projects[15].name = "Changed during hold".into();
    drop(app.cycle_workspace(
        crate::workspace::FocusDirection::Right,
        keyboard::Modifiers::ALT,
    ));
    assert_eq!(app.chrome_ui.workspace_switcher_rows[15].name, "Folder 15");
    app.cancel_workspace_preview();
    drop(app.cycle_workspace(
        crate::workspace::FocusDirection::Right,
        keyboard::Modifiers::ALT,
    ));
    let current = app.chrome_ui.workspace_preview.clone();
    drop(app.update(Message::WorkspaceSwitcherActivate {
        since: first_hold,
        index: 15,
    }));
    assert_eq!(app.chrome_ui.workspace_preview, current);
    assert_eq!(
        app.active_workspace.as_deref(),
        Some(Path::new("/folder-0"))
    );
}

#[test]
fn workspace_switcher_steps_from_where_the_wheel_left_the_list() {
    let mut app = workspace_switcher_test_app(20);
    let step = |app: &mut Cagents| {
        drop(app.cycle_workspace(
            crate::workspace::FocusDirection::Right,
            keyboard::Modifiers::ALT,
        ))
    };
    step(&mut app);
    let since = app.chrome_ui.workspace_preview_since.unwrap();
    drop(app.update(Message::WorkspaceSwitcherScrolled {
        since,
        offset: 40.0,
    }));
    step(&mut app);
    assert_eq!(app.workspace_history.cycle_index(), Some(2));
    assert!(app.workspace_switcher_shows_row(2));
    // Row 2 is in view where the wheel left the list, so it stays put.
    assert_eq!(app.chrome_ui.workspace_switcher_scroll, 40.0);
    // A scroll reported for an earlier hold moves nothing.
    drop(app.update(Message::WorkspaceSwitcherScrolled {
        since: since - Duration::from_secs(1),
        offset: 0.0,
    }));
    assert_eq!(app.chrome_ui.workspace_switcher_scroll, 40.0);
}

#[test]
fn workspace_switcher_tab_steers_a_compositor_safe_fallback_hold() {
    let mut app = workspace_switcher_test_app(3);
    let event = |key, physical| Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Code(physical),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT,
            text: None,
            repeat: false,
        },
        captured: true,
    };
    drop(app.update(event(
        keyboard::key::Named::PageDown,
        keyboard::key::Code::PageDown,
    )));
    assert_eq!(app.workspace_history.cycle_index(), Some(1));
    drop(app.update(event(keyboard::key::Named::Tab, keyboard::key::Code::Tab)));
    assert_eq!(app.workspace_history.cycle_index(), Some(2));
    assert_eq!(
        app.active_workspace.as_deref(),
        Some(Path::new("/folder-0"))
    );
}

#[test]
fn workspace_switcher_cannot_reopen_a_destination_retired_during_the_hold() {
    let mut app = workspace_switcher_test_app(2);
    drop(app.cycle_workspace(
        crate::workspace::FocusDirection::Right,
        keyboard::Modifiers::ALT,
    ));
    assert_eq!(app.workspace_history.cycle_index(), Some(1));
    app.sidebar_model.projects.remove(1);
    drop(app.commit_workspace_preview());
    assert_eq!(
        app.active_workspace.as_deref(),
        Some(Path::new("/folder-0"))
    );
    assert!(
        app.durable.cards.is_empty(),
        "retired or current scopes must not acquire a default pane"
    );
    assert!(app.chrome_ui.workspace_preview.is_none());
    assert!(app.chrome_ui.workspace_switcher_rows.is_empty());
}

#[test]
fn workspace_switcher_does_not_commit_an_old_path_for_a_retargeted_identity() {
    let mut app = workspace_switcher_test_app(2);
    drop(app.cycle_workspace(
        crate::workspace::FocusDirection::Right,
        keyboard::Modifiers::ALT,
    ));
    app.sidebar_model.projects[1].repository = "/new-folder-1".into();
    drop(app.commit_workspace_preview());
    assert_eq!(
        app.active_workspace.as_deref(),
        Some(Path::new("/folder-0"))
    );
    assert!(app.durable.cards.is_empty());
}

/// The switcher is drawn from the first press. Stepping moves only its
/// selection; the preview follows it once it rests, and a quick run of steps
/// arms a single timer.
#[test]
fn workspace_switcher_steps_leave_the_workspace_and_rest_before_previewing() {
    let mut app = workspace_switcher_test_app(4);
    let alt = keyboard::Modifiers::ALT;
    drop(app.cycle_workspace(crate::workspace::FocusDirection::Right, alt));
    let since = app.chrome_ui.workspace_preview_since.unwrap();
    assert!(app.workspace_switcher().is_some());
    // Its pane waits for the selection to rest, so a tap that goes straight
    // back to the last workspace brings nothing up.
    assert_eq!(app.chrome_ui.workspace_switcher_rested, None);
    let moved = app.chrome_ui.workspace_switcher_moved.unwrap();
    drop(app.cycle_workspace(crate::workspace::FocusDirection::Right, alt));
    drop(app.cycle_workspace(crate::workspace::FocusDirection::Right, alt));
    // The later steps reuse the timer the first armed.
    assert!(app.chrome_ui.workspace_switcher_moved.unwrap() >= moved);
    assert_eq!(app.chrome_ui.workspace_switcher_rested, None);
    assert_eq!(
        app.active_workspace.as_deref(),
        Some(Path::new("/folder-0"))
    );
    // A timer that fires before the dwell has passed waits out the rest.
    drop(app.update(Message::WorkspaceSwitcherRested(Some(since))));
    assert_eq!(app.chrome_ui.workspace_switcher_rested, None);
    app.chrome_ui.workspace_switcher_moved = Some(Instant::now() - Duration::from_secs(1));
    drop(app.update(Message::WorkspaceSwitcherRested(Some(since))));
    assert_eq!(
        app.chrome_ui.workspace_switcher_rested,
        app.workspace_switcher_selection()
    );
    // Clicking the scrim cancels, and nothing is left brought up.
    drop(app.update(Message::WorkspaceSwitcherDismissed(since)));
    assert!(app.chrome_ui.workspace_preview.is_none());
    assert_eq!(app.chrome_ui.workspace_switcher_rested, None);
    assert_eq!(app.chrome_ui.workspace_switcher_preview, None);
    assert_eq!(
        app.active_workspace.as_deref(),
        Some(Path::new("/folder-0"))
    );
}

#[test]
fn persisted_workspace_history_keeps_the_current_scope_and_bounds_old_visits() {
    let mut app = workspace_switcher_test_app(1);
    let active = app.active_workspace_destination().unwrap();
    app.workspace_history = crate::tab_history::TabHistory::from_recent(
        (0..300).map(|_| WorkspaceDestination::Folder(ProjectId::new())),
    );
    let saved = app.persistent_workspace();
    assert_eq!(saved.workspace_history.len(), WORKSPACE_HISTORY_LIMIT);
    assert_eq!(
        app.workspace_history.recent().len(),
        WORKSPACE_HISTORY_LIMIT
    );
    assert_eq!(saved.workspace_history[0], active);
    let restarted =
        crate::tab_history::TabHistory::from_recent(saved.workspace_history.iter().copied());
    assert_eq!(restarted.recent(), saved.workspace_history);
}

#[test]
fn workspace_switcher_rows_expose_actions_and_the_selected_workspace() {
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    let mut app = workspace_switcher_test_app(3);
    drop(app.cycle_workspace(
        crate::workspace::FocusDirection::Right,
        keyboard::Modifiers::ALT,
    ));
    let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let mut element = app.workspace_switcher().unwrap();
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 600.0)),
    );
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let buttons = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        .collect::<Vec<_>>();
    assert_eq!(buttons.len(), 3);
    assert_eq!(
        buttons
            .iter()
            .filter(|node| node
                .node()
                .label()
                .is_some_and(|label| label.starts_with("Selected workspace:")))
            .count(),
        1
    );
    assert!(buttons.iter().all(|node| {
        node.node()
            .supports_action(iced_accessibility::accesskit::Action::Click)
    }));
}

/// A killed session used to take its pane with it: `fail_runtime` replaced
/// the terminal runtime with a failure surface, so the scrollback that said
/// why the process died was discarded along with it.
#[test]
fn an_unclean_exit_keeps_the_terminal_surface_it_explains() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.lifecycle
        .register(card, Exposure::Visible, Residency::Hot, Instant::now());
    // A sentinel stands in for the live surface: the retirement must leave
    // whatever runtime the card holds exactly where it is.
    app.cards.insert(card, CardRuntime::Loading);

    app.retire_ended_terminal(card, 0, Some("SIGKILL"), false);

    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert!(app.lifecycle.state(card).is_none());
    assert_eq!(
        app.card_state(card).map(|_| app.residency(card)),
        Some(&Residency::Hibernated)
    );
    assert!(app.status.is_some());
    let session = app.ended_terminal(card).expect("the terminal has ended");
    // A signalled exit is not a clean zero status, and it says so.
    assert!(!terminal_completed(session));
    assert_eq!(
        terminal_end_ui::terminal_end_report(session).status,
        "SIGKILL"
    );
    let recorded = session.continuity.clone();

    // A surface attached to the ended terminal later replays the same
    // end. The first report stands: neither its moment nor its outcome
    // moves, and it is not announced a second time.
    app.status = None;
    app.retire_ended_terminal(card, 3, None, false);
    let session = app.ended_terminal(card).expect("the terminal has ended");
    assert_eq!(session.continuity, recorded);
    assert!(app.status.is_none());

    // A relaunch carries the exit forward as metadata. While the card is
    // hydrating that is a starting terminal, not an ended one, and it must
    // not be read back as read-only scrollback with a Restart button.
    app.set_residency(card, Residency::Hydrating { epoch: 1 });
    assert!(app.ended_terminal(card).is_none());
    assert!(app.completed_terminal(card).is_none());
}

#[test]
fn only_success_without_a_signal_is_normal_completion() {
    let mut state = terminal_card_state();
    let CardKind::Terminal(session) = &mut state.kind else {
        panic!("terminal fixture");
    };
    for (code, signal, completed) in [
        (Some(0), None, true),
        (Some(1), None, false),
        (None, Some("Terminated"), false),
        (None, None, false),
    ] {
        transition_terminal_to_relaunchable(
            session,
            TerminalExit {
                code,
                signal: signal.map(str::to_owned),
                occurred_at_unix_ms: 0,
            },
        );
        assert_eq!(terminal_completed(session), completed);
        assert_eq!(
            terminal_end_ui::terminal_end_report(session).outcome
                == terminal_end_ui::TerminalOutcome::Clean,
            completed
        );
    }
}

#[test]
fn completed_managed_codex_reopens_its_native_owner_without_a_new_terminal_id() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        panic!("terminal fixture");
    };
    let identity = session.id;
    session.launch.program = "codex".into();
    let endpoint = std::path::PathBuf::from("/daemon-temporary-root")
        .join(format!("cagents-codex-{identity}"))
        .join("control.sock");
    session.launch.arguments = vec!["--remote".into(), format!("unix://{}", endpoint.display())];
    session.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
        socket: endpoint,
        allocator: HostSessionProof {
            host_session_id: "allocator".into(),
            boot_id: "boot".into(),
            host_incarnation: "previous-host".into(),
        },
    });
    transition_terminal_to_relaunchable(session, terminal_exit(0, None));
    // The fixture starts a card mid-hydration; this one has finished.
    app.set_residency(card, Residency::Hibernated);
    app.cards.insert(card, CardRuntime::Completed);
    assert!(app.completed_terminal_has_native_owner(card));
    // Dropping the task never connects to or launches a real agent.
    drop(app.retry_card(card));
    assert!(app.chat_start_pending(card));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
    let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
        panic!("native attach has not completed yet");
    };
    assert_eq!(session.id, identity);
    assert_eq!(app.terminal_starts_in_flight(), 0);
}

#[test]
fn sidebar_refresh_targets_every_existing_project_without_opening_cards() {
    let first = ProjectId::new();
    let second = ProjectId::new();
    let projects = vec![
        SidebarProject::new(
            first,
            RepositoryId::new(),
            "first".into(),
            PathBuf::from("/repositories/first"),
            Vec::new(),
            Vec::new(),
        ),
        SidebarProject::new(
            second,
            RepositoryId::new(),
            "second".into(),
            PathBuf::from("/repositories/second"),
            Vec::new(),
            Vec::new(),
        ),
    ];

    assert_eq!(
        repository_refresh_targets(&projects),
        vec![
            (PathBuf::from("/repositories/first"), first),
            (PathBuf::from("/repositories/second"), second),
        ]
    );
}

/// A row for each project, as session restoration builds them before any
/// repository has been read.
fn restored_sidebar_project(id: ProjectId, name: &str, repository: &Path) -> SidebarProject {
    SidebarProject::new(
        id,
        RepositoryId::new(),
        name.to_owned(),
        repository.to_path_buf(),
        Vec::new(),
        Vec::new(),
    )
}

fn repository_load_result(
    project_id: ProjectId,
    result: Result<LoadedRepository, String>,
) -> Message {
    Message::RepositoryLoaded {
        open_cards: false,
        project_id,
        generation: 1,
        result: result.map(Box::new),
    }
}

/// A handler moved out of `update` keeps its early exits: a stale result
/// returns before the projection, as the inline arm did, while a current one
/// still reaches it.
#[test]
fn an_extracted_handler_returning_early_skips_the_projection() {
    let mut app = context_inspector_test_app(true);
    let project = ProjectId::new();
    app.repository_generations.insert(project, 2);
    let loaded = |generation| Message::RepositoryLoaded {
        open_cards: false,
        project_id: project,
        generation,
        result: Err("unreadable".into()),
    };
    drop(app.update(loaded(1)));
    assert!(
        !projection_oracle::projected(),
        "a superseded load returns before projecting"
    );
    assert_eq!(app.repository_generations.get(&project), Some(&2));
    drop(app.update(loaded(2)));
    assert!(
        projection_oracle::projected(),
        "the current load's failure is projected"
    );
}

#[test]
fn project_search_shortcut_does_not_redirect_to_focused_settings_find() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.cards.insert(card, CardRuntime::Settings);
    app.card_state_mut(card).unwrap().kind = CardKind::Settings;
    app.sidebar_model.projects.push(restored_sidebar_project(
        ProjectId::new(),
        "Fixture",
        Path::new("/fixture"),
    ));
    assert!(!app.sidebar_state.controls_expanded());
    drop(app.run_workspace_command(
        workspace_widget::GlobalShortcut::FindInPane,
        keyboard::Modifiers::empty(),
    ));
    assert!(!app.sidebar_state.controls_expanded());
    drop(app.run_workspace_command(
        workspace_widget::GlobalShortcut::FocusProjectSearch,
        keyboard::Modifiers::empty(),
    ));
    assert!(app.sidebar_state.controls_expanded());
    assert_eq!(app.workspace.focused(), Some(card));
}

#[test]
fn one_unreadable_project_keeps_every_other_project_listed() {
    let mut app = context_inspector_test_app(false);
    let broken = ProjectId::new();
    let healthy = ProjectId::new();
    let healthy_path = PathBuf::from("/repositories/healthy");
    *app.sidebar_model.projects = vec![
        restored_sidebar_project(broken, "broken", Path::new("/repositories/broken")),
        restored_sidebar_project(healthy, "healthy", &healthy_path),
    ];
    app.sidebar_model.data_state = SidebarDataState::Loading;
    app.repository_generations.insert(broken, 1);
    app.repository_generations.insert(healthy, 1);
    let loaded = LoadedRepository {
        project: ProjectState {
            kind: Default::default(),
            id: healthy,
            generation: 0,
            name: "healthy".into(),
            repository: healthy_path.clone(),
            existence: Existence::Open,
        },
        repository: Some(RepositoryInfo {
            id: RepositoryId::new(),
            selected_path: healthy_path.clone(),
            common_dir: healthy_path.join(".git"),
            git_dir: healthy_path.join(".git"),
            worktree_root: Some(healthy_path.clone()),
            is_bare: false,
        }),
        worktrees: Vec::new(),
        github: GitHubAvailability::NotAssociated,
        review_repository: None,
        head_repositories: HashMap::new(),
    };

    // The failure lands last on purpose. A failure followed by a success
    // would be rescued by the success setting the pane Ready, so only this
    // order can catch a failure that blanks the pane.
    drop(app.update(repository_load_result(healthy, Ok(loaded))));
    drop(app.update(repository_load_result(
        broken,
        Err("fatal: not a git repository".into()),
    )));

    assert_eq!(app.sidebar_model.data_state, SidebarDataState::Ready);
    assert_eq!(
        app.sidebar_model
            .projects
            .iter()
            .map(|project| (project.id, project.unreachable.clone()))
            .collect::<Vec<_>>(),
        vec![
            (broken, Some("fatal: not a git repository".to_owned())),
            (healthy, None),
        ]
    );
}

#[test]
fn failed_sidebar_refresh_finishes_loading_with_an_error() {
    let mut app = context_inspector_test_app(false);
    let picked = ProjectId::new();
    app.sidebar_model.data_state = SidebarDataState::Loading;
    app.repository_generations.insert(picked, 1);

    drop(app.update(repository_load_result(picked, Err("refresh failed".into()))));

    assert!(app.sidebar_model.projects.is_empty());
    assert_eq!(
        app.sidebar_model.data_state,
        SidebarDataState::Error("refresh failed".into())
    );
}

#[test]
fn layout_scroll_feedback_requests_camera_sync_without_cancelling_reveal() {
    let mut workspace = Workspace::default();
    for index in 0..4 {
        workspace
            .add_card(
                WorkspaceCard::new(CardId::new(), "Pane", CardWidth::Fixed(480.0)).unwrap(),
                index,
                index == 3,
            )
            .unwrap();
    }
    workspace.resize_viewport(600.0);
    workspace.tick(Duration::from_secs(1));
    let target = workspace.camera().target();
    let (_, synchronize) = update_workspace_viewport(
        &mut workspace,
        WorkspaceViewportEvent::Scrolled(crate::workspace_widget::ViewportSample {
            offset: 0.0,
            width: 600.0,
            user_input: false,
        }),
    );
    assert!(synchronize);
    assert_eq!(workspace.camera().target(), target);

    let (_, synchronize) = update_workspace_viewport(
        &mut workspace,
        WorkspaceViewportEvent::Scrolled(crate::workspace_widget::ViewportSample {
            offset: 0.0,
            width: 600.0,
            user_input: true,
        }),
    );
    assert!(!synchronize);
    assert_eq!(workspace.camera().target(), 0.0);
}

#[test]
fn viewport_resize_preserves_focused_card_reveal_and_requests_synchronization() {
    let config = WorkspaceConfig::default();
    let mut workspace = Workspace::new(config).unwrap();
    let cards = [CardId::new(), CardId::new(), CardId::new()];
    for (index, id) in cards.into_iter().enumerate() {
        workspace
            .add_card(
                WorkspaceCard::new(id, format!("card {index}"), CardWidth::Fixed(480.0)).unwrap(),
                index,
                false,
            )
            .unwrap();
    }
    workspace.focus(cards[2]).unwrap();

    let (_, synchronize_scroll) =
        update_workspace_viewport(&mut workspace, WorkspaceViewportEvent::Resized(600.0));
    let expected_target = workspace
        .geometry()
        .focus_offset(cards[2], 600.0, workspace.camera().offset())
        .unwrap();

    assert!(synchronize_scroll);
    assert_eq!(workspace.focused(), Some(cards[2]));
    assert_eq!(workspace.camera().target(), expected_target);
    assert!(workspace.camera().is_animating());
}

pub(super) fn terminal_card_state() -> CardState {
    let launch = launch_recipe("/tmp");
    CardState {
        id: CardId::new(),
        generation: 0,
        title: "Terminal".into(),
        width_fraction: 0.7,
        existence: Existence::Open,
        kind: CardKind::Terminal(TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        }),
    }
}

#[test]
fn pane_clone_is_adjacent_and_never_shares_terminal_continuity() {
    let mut app = context_inspector_test_app(true);
    // The source is itself a relaunch, but its clone succeeds nothing.
    let source = app.durable.cards[0].id;
    if let CardKind::Terminal(session) = &mut app.card_state_mut(source).unwrap().kind {
        session.recovery_origin = Some(TerminalId::new());
        session.predecessor = Some(TerminalId::new());
    }
    let source = app.durable.cards[0].clone();
    let _unpolled = app.clone_focused_pane();
    let cloned = app.workspace.focused().unwrap();
    assert_ne!(cloned, source.id);
    assert_eq!(app.workspace.cards()[1].id, cloned);
    let original_state = app.card_state(source.id).unwrap();
    assert_eq!(original_state.id, source.id);
    assert_eq!(original_state.kind, source.kind);
    assert_eq!(original_state.width_fraction, source.width_fraction);
    assert_eq!(original_state.existence, source.existence);
    let copy = app.card_state(cloned).unwrap();
    assert_eq!(copy.width_fraction, source.width_fraction);
    let (CardKind::Terminal(original), CardKind::Terminal(copy)) = (&source.kind, &copy.kind)
    else {
        panic!("both panes must remain terminals");
    };
    assert_ne!(copy.id, original.id);
    assert_eq!(copy.launch, original.launch);
    assert!(copy.continuity.live_proof().is_none());
    assert!(copy.resume_hint.is_none());
    assert_eq!((copy.recovery_origin, copy.predecessor), (None, None));
}

mod repository_clone_navigation {
    use super::*;
    use crate::model::{ChannelId, ProjectChannel};

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        text: String,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let text = (0..100)
                .map(|line| format!("source line {line:03}\n"))
                .collect::<String>();
            std::fs::write(root.join("current.txt"), &text).unwrap();
            std::fs::write(root.join("other.txt"), "a different file\n").unwrap();
            for arguments in [
                vec!["init", "-q", "--template="],
                vec!["add", "current.txt", "other.txt"],
                vec![
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    "fixture",
                ],
            ] {
                let mut git = tokio::process::Command::new("/usr/bin/git");
                git.env_clear()
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .current_dir(&root)
                    .args([
                        "-c",
                        "core.hooksPath=/dev/null",
                        "-c",
                        "commit.gpgsign=false",
                    ])
                    .args(arguments)
                    .kill_on_drop(true);
                let output = tokio::time::timeout(Duration::from_secs(5), git.output())
                    .await
                    .expect("private Git fixture command finishes")
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Self {
                _directory: directory,
                root,
                text,
            }
        }

        async fn app(&self, selected: bool, fixed_file: bool) -> (Cagents, CardId) {
            let mut app = context_inspector_test_app(true);
            app.settings.editor_auto_save = false;
            let card = app.workspace.focused().unwrap();
            let view = RepositoryView {
                project: ProjectId::new(),
                repository: self.root.join(".git"),
                worktree: self.root.clone(),
                selected_file: fixed_file.then(|| "current.txt".into()),
            };
            app.set_residency(card, Residency::Hot);
            let state = app.card_state_mut(card).unwrap();
            state.kind = CardKind::Repository(view.clone());
            let mut runtime = RepositoryRuntime::new(view.clone());
            runtime.set_auto_save(false);
            if selected {
                runtime.restore_navigation(&crate::model::RepositoryNavigation {
                    project: view.project,
                    worktree: view.worktree,
                    selected_file: Some("current.txt".into()),
                    editor: Some(crate::model::EditorNavigation {
                        caret: crate::model::EditorPosition {
                            line: 80,
                            column: 7,
                        },
                        selection: Some(crate::model::EditorPosition {
                            line: 79,
                            column: 2,
                        }),
                        scroll_line: 70,
                        scroll_vertical: 2.5,
                        scroll_horizontal: 7.0,
                    }),
                });
            }
            let load = repository_task(runtime.initial_load(card));
            app.cards
                .insert(card, CardRuntime::Repository(Box::new(runtime)));
            settle(&mut app, load).await;
            assert_eq!(
                self::runtime(&app, card).selected_path().is_some(),
                selected
            );
            (app, card)
        }
    }

    fn runtime(app: &Cagents, card: CardId) -> &RepositoryRuntime {
        match &app.cards[&card] {
            CardRuntime::Repository(runtime) => runtime,
            _ => panic!("actual repository runtime"),
        }
    }

    async fn settle(app: &mut Cagents, task: Task<Message>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut pending = VecDeque::from([task]);
            let mut tasks = 0;
            while let Some(task) = pending.pop_front() {
                tasks += 1;
                assert!(tasks < 64, "repository loads must settle");
                let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                    continue;
                };
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::Repository(_),
                    )) = action
                    {
                        pending.push_back(app.update(message));
                    }
                }
            }
        })
        .await
        .expect("actual clone file worker and application callbacks finish");
    }

    /// Binds the sidebar to the fixture worktree, as selecting it does.
    fn bind_sidebar(app: &mut Cagents, fixture: &Fixture, card: CardId) -> ActiveWorktreeKey {
        let project = runtime(app, card).descriptor.project;
        app.sidebar_model = SidebarModel::ready(
            sidebar::SidebarMode::Expanded,
            vec![SidebarProject::new(
                project,
                RepositoryId::new(),
                "clone navigation fixture".into(),
                fixture.root.join(".git"),
                vec![SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    fixture.root.clone(),
                    None,
                )],
                Vec::new(),
            )],
        );
        assert!(app.sidebar_state.sync_active_worktree(
            &app.sidebar_model,
            &sidebar::SidebarFocus::Repository {
                worktree: fixture.root.clone(),
            },
        ));
        app.sidebar_state.active_worktree().unwrap().clone()
    }

    fn edit(app: &mut Cagents, card: CardId) {
        drop(app.update(Message::FocusCard(card)));
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card,
                action: repository_widget::EditorAction::Edit(
                    repository_widget::EditorEdit::Insert('!'),
                ),
            })),
        );
        assert!(runtime(app, card).blocks_navigation());
    }

    /// Two browsers on the fixture: the first editing current.txt without
    /// saving and focused, the second clean. other.txt is modified and
    /// new.txt untracked on disk, and Source Control shows both.
    async fn dirty_first_browser(
        fixture: &Fixture,
    ) -> (Cagents, CardId, CardId, ActiveWorktreeKey) {
        let (mut app, first) = fixture.app(true, false).await;
        let task = app.clone_focused_pane();
        let second = app.workspace.focused().unwrap();
        settle(&mut app, task).await;
        assert_ne!(second, first);
        edit(&mut app, first);
        assert_eq!(app.workspace.focused(), Some(first));
        std::fs::write(fixture.root.join("other.txt"), "changed on disk\n").unwrap();
        std::fs::write(fixture.root.join("new.txt"), "untracked\n").unwrap();
        let key = bind_sidebar(&mut app, fixture, first);
        let snapshot = RepositoryService::system("git")
            .snapshot_with_options(
                &fixture.root,
                crate::repository::SnapshotOptions::default(),
                &CancellationToken::default(),
            )
            .unwrap();
        app.sidebar_model.source_control = Some(SourceControlModel::ready(key.clone(), &snapshot));
        (app, first, second, key)
    }

    fn open_change(
        app: &mut Cagents,
        key: &ActiveWorktreeKey,
        path: &str,
        area: ChangeArea,
    ) -> Task<Message> {
        app.update(Message::Sidebar(SidebarAction::OpenChange {
            key: key.clone(),
            path: RepoPath::parse(path).unwrap(),
            area,
        }))
    }

    fn open_file(app: &mut Cagents, key: &ActiveWorktreeKey, path: &str) -> Task<Message> {
        app.update(Message::Sidebar(SidebarAction::OpenFile {
            key: key.clone(),
            path: RepoPath::parse(path).unwrap(),
        }))
    }

    async fn edit_in_editor(app: &mut Cagents, card: CardId) {
        let task = app.update(Message::Repository(RepositoryAction::ApplySourceSurface {
            card,
            diff: false,
        }));
        settle(app, task).await;
        edit(app, card);
    }

    fn assert_shows(app: &Cagents, card: CardId, path: &str, basis: &DiffBasis) {
        let runtime = runtime(app, card);
        assert_eq!(runtime.selected_path(), Some(Path::new(path)));
        assert!(runtime.shows_diff(), "{path} is shown as a diff");
        assert_eq!(runtime.selected_diff_basis(), basis);
    }

    /// Source Control rows must reach a browser that can navigate. The first
    /// browser on the worktree refusing navigation used to take the click,
    /// focus itself and drop the request.
    #[tokio::test]
    async fn source_control_changes_skip_browsers_that_refuse_navigation() {
        let fixture = Fixture::new().await;
        let (mut app, first, second, key) = dirty_first_browser(&fixture).await;

        let task = open_change(&mut app, &key, "other.txt", ChangeArea::Unstaged);
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(second));
        assert_shows(&app, second, "other.txt", &DiffBasis::IndexToWorktree);
        assert_eq!(
            runtime(&app, first).selected_path(),
            Some(Path::new("current.txt"))
        );
        assert!(runtime(&app, first).is_dirty());

        // A change to the file a browser is editing shows that browser's
        // live comparison rather than a second, stale copy of the file.
        edit_in_editor(&mut app, second).await;
        drop(app.update(Message::FocusCard(first)));
        let task = open_change(&mut app, &key, "other.txt", ChangeArea::Unstaged);
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(second));
        assert_shows(&app, second, "other.txt", &DiffBasis::IndexToWorktree);
        assert!(runtime(&app, second).is_dirty());
        assert_eq!(app.repository_browsers(&fixture.root).count(), 2);

        // With every browser refusing navigation, another change opens in a
        // new browser rather than nowhere.
        let task = open_change(&mut app, &key, "new.txt", ChangeArea::Untracked);
        let third = app.workspace.focused().unwrap();
        settle(&mut app, task).await;
        assert_eq!(
            app.repository_browsers(&fixture.root).collect::<Vec<_>>(),
            [first, second, third]
        );
        assert_shows(&app, third, "new.txt", &DiffBasis::IndexToWorktree);
        for card in [first, second] {
            assert!(runtime(&app, card).is_dirty());
        }
    }

    /// Files rows take the same route, parked until the chosen browser's
    /// file list lands.
    #[tokio::test]
    async fn files_rows_skip_browsers_that_refuse_navigation() {
        let fixture = Fixture::new().await;
        let (mut app, first, second, key) = dirty_first_browser(&fixture).await;

        let task = open_file(&mut app, &key, "other.txt");
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(second));
        assert_eq!(
            runtime(&app, second).selected_path(),
            Some(Path::new("other.txt"))
        );
        assert!(runtime(&app, first).is_dirty());

        // A row for the file a browser is editing focuses those edits.
        let task = open_file(&mut app, &key, "current.txt");
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(first));
        assert!(app.pending_panel_open.is_none());
        assert_eq!(app.repository_browsers(&fixture.root).count(), 2);

        // With every browser refusing navigation, the row opens a new one.
        edit_in_editor(&mut app, second).await;
        let task = open_file(&mut app, &key, "new.txt");
        let third = app.workspace.focused().unwrap();
        settle(&mut app, task).await;
        assert_eq!(
            app.repository_browsers(&fixture.root).collect::<Vec<_>>(),
            [first, second, third]
        );
        assert_eq!(
            runtime(&app, third).selected_path(),
            Some(Path::new("new.txt"))
        );
        for card in [first, second] {
            assert!(runtime(&app, card).is_dirty());
        }
    }

    /// A pane opened through a path that is not canonical, such as a file
    /// link through a symlink, still counts as editing the file.
    #[test]
    fn same_file_sees_through_a_non_canonical_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/file.txt"), "").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        assert!(super::super::same_file(
            &root.join("alias/file.txt"),
            &root.join("real/file.txt")
        ));
        std::fs::write(root.join("real/other.txt"), "").unwrap();
        assert!(!super::super::same_file(
            &root.join("alias/other.txt"),
            &root.join("real/file.txt")
        ));
    }

    /// History and branch rows open committed comparisons through the same
    /// route.
    #[tokio::test]
    async fn committed_files_skip_browsers_that_refuse_navigation() {
        let fixture = Fixture::new().await;
        let (mut app, first, second, key) = dirty_first_browser(&fixture).await;
        let head = std::process::Command::new("/usr/bin/git")
            .current_dir(&fixture.root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let head =
            crate::repository::Oid::parse_object(String::from_utf8(head.stdout).unwrap().trim())
                .unwrap();
        let basis = DiffBasis::CommitToParent {
            oid: head,
            parent: None,
        };

        let task = app.open_committed_file(
            key.clone(),
            RepoPath::parse("other.txt").unwrap(),
            None,
            basis.clone(),
        );
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(second));
        assert_shows(&app, second, "other.txt", &basis);
        assert!(runtime(&app, first).is_dirty());

        // The edited file's own commit shows in its browser, beside the
        // buffer.
        drop(app.update(Message::FocusCard(second)));
        let task = app.open_committed_file(
            key.clone(),
            RepoPath::parse("current.txt").unwrap(),
            None,
            basis.clone(),
        );
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(first));
        assert_shows(&app, first, "current.txt", &basis);
        assert!(runtime(&app, first).is_dirty());

        // A rename cannot be compared beside the buffer. The browser keeps
        // its edits, and the notice says what to do.
        let task = app.update(Message::Repository(RepositoryAction::ApplySourceSurface {
            card: first,
            diff: false,
        }));
        settle(&mut app, task).await;
        drop(app.update(Message::FocusCard(second)));
        app.status = None;
        let task = app.open_committed_file(
            key.clone(),
            RepoPath::parse("current.txt").unwrap(),
            Some(RepoPath::parse("previous.txt").unwrap()),
            basis,
        );
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(first));
        assert!(!runtime(&app, first).shows_diff());
        assert!(runtime(&app, first).is_dirty());
        assert_eq!(
            app.status.as_ref().map(|notice| notice.message.as_str()),
            Some("Save or discard your changes to current.txt to open this comparison")
        );
        assert_eq!(app.repository_browsers(&fixture.root).count(), 2);
    }

    #[tokio::test]
    async fn browser_clone_loads_live_file_and_independent_navigation() {
        let fixture = Fixture::new().await;
        let (mut app, source) = fixture.app(true, false).await;
        let source_runtime = runtime(&app, source) as *const RepositoryRuntime;
        let expected = runtime(&app, source).navigation().unwrap();
        let editor = expected.editor.unwrap();
        assert_eq!(editor.caret.line, 80);
        assert_eq!(editor.selection.unwrap().line, 79);
        assert_eq!(editor.scroll_line, 70);
        assert_eq!(editor.scroll_vertical, 2.5);
        assert_eq!(editor.scroll_horizontal, 7.0);
        let mut stale = expected.clone();
        stale.selected_file = Some("other.txt".into());
        stale.editor = None;
        Arc::make_mut(&mut app.durable)
            .repository_navigation
            .insert(source, stale);
        assert_ne!(app.durable.repository_navigation[&source], expected);

        let task = app.clone_focused_pane();
        let clone = app.workspace.focused().unwrap();
        assert_ne!(clone, source);
        settle(&mut app, task).await;
        assert_eq!(app.workspace.focused(), Some(clone));
        assert_eq!(
            runtime(&app, clone).editor_text(),
            Some(fixture.text.clone())
        );
        assert_eq!(runtime(&app, clone).navigation(), Some(expected.clone()));
        assert_eq!(runtime(&app, source).navigation(), Some(expected.clone()));
        assert_eq!(
            runtime(&app, source).editor_text(),
            Some(fixture.text.clone())
        );
        assert_eq!(
            runtime(&app, source) as *const RepositoryRuntime,
            source_runtime
        );
        for card in [source, clone] {
            assert!(runtime(&app, card).descriptor.selected_file.is_none());
            assert!(matches!(&app.card_state(card).unwrap().kind,
                CardKind::Repository(view) if view.selected_file.is_none()));
            assert!(!runtime(&app, card).is_dirty());
        }

        // Drive the sidebar row, not a card-addressed repository action:
        // the sidebar must choose the focused clone rather than the first
        // browser, which is still the source.
        let key = bind_sidebar(&mut app, &fixture, clone);
        let project = runtime(&app, clone).descriptor.project;
        assert_eq!(app.repository_browsers(&fixture.root).next(), Some(source));
        assert_eq!(app.repository_panel_destination(&fixture.root), Some(clone));
        let task = app.update(Message::Sidebar(SidebarAction::OpenFile {
            key,
            path: RepoPath::parse("other.txt").unwrap(),
        }));
        settle(&mut app, task).await;
        assert_eq!(
            runtime(&app, clone).selected_path(),
            Some(Path::new("other.txt"))
        );
        assert_eq!(
            runtime(&app, clone).editor_text().as_deref(),
            Some("a different file\n")
        );
        assert_eq!(runtime(&app, source).navigation(), Some(expected));
        assert_eq!(
            runtime(&app, source).editor_text(),
            Some(fixture.text.clone())
        );

        // Ineligible focus must preserve the existing first-browser
        // fallback. None of these cases may retarget a fixed file or a
        // pane belonging to another worktree/channel.
        let eligible = app.card_state(clone).unwrap().clone();
        for fixed_file in [true, false] {
            let CardKind::Repository(view) = &mut app.card_state_mut(clone).unwrap().kind else {
                panic!("repository clone");
            };
            if fixed_file {
                view.selected_file = Some("other.txt".into());
            } else {
                view.worktree = fixture.root.join("different-worktree");
            }
            assert_eq!(
                app.repository_panel_destination(&fixture.root),
                Some(source)
            );
            *app.card_state_mut(clone).unwrap() = eligible.clone();
        }
        app.card_state_mut(clone).unwrap().kind = CardKind::Settings;
        assert_eq!(
            app.repository_panel_destination(&fixture.root),
            Some(source)
        );
        *app.card_state_mut(clone).unwrap() = eligible.clone();
        app.card_state_mut(clone).unwrap().existence = crate::model::Existence::Closing {
            operation_id: crate::model::OperationId::new(),
        };
        assert_eq!(
            app.repository_panel_destination(&fixture.root),
            Some(source)
        );
        *app.card_state_mut(clone).unwrap() = eligible.clone();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: ChannelId::new(),
                project_id: project,
                title: "other channel".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: false,
                cards: vec![clone],
                focused: Some(clone),
            });
        assert_eq!(
            app.repository_panel_destination(&fixture.root),
            Some(source)
        );
        Arc::make_mut(&mut app.durable).channels.pop();
        let position = app
            .durable
            .cards
            .iter()
            .position(|card| card.id == clone)
            .unwrap();
        Arc::make_mut(&mut app.durable).cards.remove(position);
        assert_eq!(app.workspace.focused(), Some(clone));
        assert_eq!(
            app.repository_panel_destination(&fixture.root),
            Some(source)
        );
        Arc::make_mut(&mut app.durable)
            .cards
            .insert(position, eligible);

        // Cloning location must not move or share the source's unsaved
        // buffer and navigation fence. It does not clone unsaved bytes.
        drop(app.update(Message::FocusCard(source)));
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: source,
                action: repository_widget::EditorAction::Edit(
                    repository_widget::EditorEdit::Insert('!'),
                ),
            })),
        );
        let dirty_text = runtime(&app, source).editor_text();
        let dirty_navigation = runtime(&app, source).navigation();
        assert!(runtime(&app, source).is_dirty());
        assert!(runtime(&app, source).blocks_navigation());
        let task = app.clone_focused_pane();
        let independent = app.workspace.focused().unwrap();
        settle(&mut app, task).await;
        assert_ne!(independent, source);
        assert_eq!(runtime(&app, independent).editor_text(), Some(fixture.text));
        assert!(!runtime(&app, independent).is_dirty());
        assert_eq!(runtime(&app, source).editor_text(), dirty_text);
        assert_eq!(runtime(&app, source).navigation(), dirty_navigation);
        assert_eq!(
            runtime(&app, source) as *const RepositoryRuntime,
            source_runtime
        );
        assert!(runtime(&app, source).is_dirty());
        assert!(runtime(&app, source).blocks_navigation());
    }

    #[tokio::test]
    async fn clone_keeps_empty_and_fixed_file_recipes_and_reopen_separate() {
        let fixture = Fixture::new().await;
        for (selected, fixed_file) in [(false, false), (true, true)] {
            let (mut app, source) = fixture.app(selected, fixed_file).await;
            let expected = runtime(&app, source).navigation();
            // An empty browser must not acquire an old saved selection.
            if !selected {
                let view = runtime(&app, source).descriptor.clone();
                Arc::make_mut(&mut app.durable)
                    .repository_navigation
                    .insert(
                        source,
                        crate::model::RepositoryNavigation {
                            project: view.project,
                            worktree: view.worktree,
                            selected_file: Some("other.txt".into()),
                            editor: None,
                        },
                    );
            }
            let task = app.clone_focused_pane();
            let clone = app.workspace.focused().unwrap();
            settle(&mut app, task).await;
            assert_ne!(clone, source);
            assert_eq!(runtime(&app, clone).navigation(), expected);
            assert_eq!(
                runtime(&app, clone).descriptor.selected_file.is_some(),
                fixed_file
            );
            assert_eq!(
                runtime(&app, source).descriptor,
                runtime(&app, clone).descriptor
            );
            assert_eq!(runtime(&app, clone).selected_path().is_some(), selected);
        }

        let (mut app, source) = fixture.app(true, false).await;
        let original_navigation = runtime(&app, source).navigation();
        // Closed entries store the old Open CardState. Deliberately keep
        // its live runtime here too: neither existence nor an ID match
        // may turn the separate Reopen route into a live-view clone.
        app.recently_closed.push((
            app.card_state(source).unwrap().clone(),
            app.active_workspace_destination(),
        ));
        let task = app.run_workspace_command(
            workspace_widget::GlobalShortcut::ReopenPane,
            keyboard::Modifiers::empty(),
        );
        let reopened = app.workspace.focused().unwrap();
        settle(&mut app, task).await;
        assert_ne!(reopened, source);
        assert_eq!(runtime(&app, reopened).selected_path(), None);
        assert_eq!(runtime(&app, reopened).navigation(), None);
        assert_eq!(runtime(&app, source).navigation(), original_navigation);
    }
}

mod window_restore {
    include!("window_restore_tests.rs");
}

mod home_admission {
    include!("home_admission_tests.rs");
}

mod repository_clone_lsp_ownership {
    include!("repository_clone_lsp_ownership_tests.rs");
}

mod navigation_saves {
    include!("navigation_save_tests.rs");
}

#[test]
fn pane_title_rename_task_persists_the_override_for_window_reload() {
    let directory = tempfile::tempdir().unwrap();
    let (session, _) = SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("title-persistence-test"),
    )
    .unwrap();
    let store = session.operation_store();
    let mut app = context_inspector_test_app(true);
    app.session = Some(session);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let source = app.workspace.focused().unwrap();
    let _ = app.update_chrome(chrome_ui::Action::Rename(source));
    let _ = app.update_chrome(chrome_ui::Action::RenameChanged("Release review".into()));
    let task = app.update_chrome(chrome_ui::Action::RenameSubmit);
    settle_settings_task(&mut app, task);

    // Drive the action's actual returned task. A manual capture would
    // pass even when RenameSubmit forgot to schedule WindowView saving.
    let saved = store
        .load_window_view()
        .unwrap()
        .expect("rename saved its window view");
    let restored: WindowView = serde_json::from_str(&saved).unwrap();
    assert_eq!(restored.user_titles[&source], "Release review");
    app.chrome_ui = chrome_ui::State::default().with_user_titles(restored.user_titles);
    assert!(!app.apply_reported_title(source, "automatic process title".into()));
    assert_eq!(
        app.repository_chrome(app.card_state(source).unwrap()),
        "Release review"
    );
}

#[test]
fn pane_clone_preserves_explicit_titles_across_later_process_reports() {
    for chat in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut app = context_inspector_test_app(true);
        app.settings_path = Some(directory.path().join("settings.toml"));
        app.settings_write_blocked = false;
        let source = app.workspace.focused().unwrap();
        if chat {
            let mut recipe = launch_recipe("/tmp");
            recipe.program = "claude".into();
            app.card_state_mut(source).unwrap().kind = CardKind::Chat(crate::chat::Session {
                native_continuity: Default::default(),
                owned_codex_listener: None,
                id: TerminalId::new(),
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe,
                    session_id: None,
                },
                proof: None,
                terminal: None,
            });
        }
        let _ = app.update_chrome(chrome_ui::Action::Rename(source));
        let _ = app.update_chrome(chrome_ui::Action::RenameChanged("Release review".into()));
        let _ = app.update_chrome(chrome_ui::Action::RenameSubmit);
        app.chrome_ui.tabs_focus = Some(source);
        let before_clone = app.settings_save_generation;
        let _ = app.clone_focused_pane();
        let cloned = app.workspace.focused().unwrap();
        assert_ne!(cloned, source);
        assert_eq!(app.chrome_ui.tabs_focus, Some(cloned));
        assert!(app.settings_save_generation > before_clone);
        assert_eq!(app.card_state(cloned).unwrap().title, "Release review");
        assert_eq!(app.chrome_ui.user_titles[&cloned], "Release review");
        assert!(!app.apply_reported_title(cloned, "automatic process title".into()));
        assert_eq!(app.card_state(cloned).unwrap().title, "Release review");
    }
}

#[test]
fn reopening_a_renamed_pane_keeps_its_title_override() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = context_inspector_test_app(true);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let source = app.workspace.focused().unwrap();
    let _ = app.update_chrome(chrome_ui::Action::Rename(source));
    let _ = app.update_chrome(chrome_ui::Action::RenameChanged("Release logs".into()));
    let _ = app.update_chrome(chrome_ui::Action::RenameSubmit);
    app.recently_closed.push((
        app.card_state(source).unwrap().clone(),
        app.active_workspace_destination(),
    ));
    app.chrome_ui.tabs_focus = Some(source);
    let before_reopen = app.settings_save_generation;
    let _ = app.run_workspace_command(
        workspace_widget::GlobalShortcut::ReopenPane,
        keyboard::Modifiers::empty(),
    );
    let reopened = app.workspace.focused().unwrap();
    assert_ne!(reopened, source);
    assert_eq!(app.chrome_ui.tabs_focus, Some(reopened));
    assert!(app.settings_save_generation > before_reopen);
    assert!(app.recently_closed.is_empty());
    assert_eq!(app.chrome_ui.user_titles[&reopened], "Release logs");
    assert!(!app.apply_reported_title(reopened, "automatic process title".into()));
    assert_eq!(app.card_state(reopened).unwrap().title, "Release logs");
}

#[test]
fn saved_title_overrides_keep_durable_and_recent_panes_without_orphans() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = context_inspector_test_app(true);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let durable = app.durable.cards[0].id;
    let mut recent = app.durable.cards[0].clone();
    recent.id = CardId::new();
    let recent_id = recent.id;
    let orphan = CardId::new();
    app.recently_closed.push((recent, None));
    for id in [durable, recent_id, orphan] {
        app.chrome_ui.user_titles.insert(id, "Custom title".into());
    }
    let saved = app.capture_settings_save().unwrap();
    assert_eq!(saved.view.user_titles.len(), 2);
    assert!(saved.view.user_titles.contains_key(&durable));
    assert!(saved.view.user_titles.contains_key(&recent_id));
    assert!(!app.chrome_ui.user_titles.contains_key(&orphan));
    app.recently_closed.clear();
    assert!(
        !app.capture_settings_save()
            .unwrap()
            .view
            .user_titles
            .contains_key(&recent_id)
    );
}

#[test]
fn pane_clone_starts_a_new_chat_without_reusing_source_conversation() {
    let mut app = context_inspector_test_app(true);
    let source_id = app.workspace.focused().unwrap();
    let mut recipe = launch_recipe("/var/tmp");
    recipe.program = "claude".into();
    let conversation = "a1b2c3d4-1111-4111-8111-123456789abc";
    recipe.arguments = vec!["--resume".into(), conversation.into(), "--bare".into()];
    let original = crate::chat::Session {
        native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe,
            session_id: Some(conversation.into()),
        },
        proof: Some(HostSessionProof {
            host_session_id: "source-holder".into(),
            boot_id: "boot".into(),
            host_incarnation: "source-daemon".into(),
        }),
        terminal: None,
    };
    assert!(original.has_valid_native_continuity());
    app.card_state_mut(source_id).unwrap().kind = CardKind::Chat(original.clone());
    let _unpolled = app.clone_focused_pane();
    let cloned = app.workspace.focused().unwrap();
    assert_ne!(cloned, source_id);
    assert_eq!(
        app.card_state(cloned).unwrap().title,
        crate::app::chat_ui::chat_card_title(crate::agent::Provider::Claude),
        "unnamed chat clones retain the provider's default title"
    );
    let CardKind::Chat(copy) = &app.card_state(cloned).unwrap().kind else {
        panic!("cloned conversation must stay native");
    };
    assert_ne!(copy.id, original.id);
    assert_eq!(copy.launch.provider, original.launch.provider);
    assert_eq!(copy.launch.recipe.arguments, ["--bare"]);
    assert!(copy.launch.session_id.is_none());
    assert!(copy.proof.is_none());
    assert!(copy.terminal.is_none());
    assert!(copy.is_unstarted());
    assert!(copy.has_valid_native_continuity());
    assert_eq!(
        app.card_state(source_id).unwrap().kind,
        CardKind::Chat(original)
    );
}

#[test]
fn pane_clone_releases_owned_and_legacy_codex_endpoints_before_changing_identity() {
    for owned in [true, false] {
        let mut app = context_inspector_test_app(true);
        let source_id = app.workspace.focused().unwrap();
        let id = TerminalId::new();
        let socket = PathBuf::from("/var/tmp")
            .join(format!("cagents-codex-{id}"))
            .join("control.sock");
        let proof = HostSessionProof {
            host_session_id: "source-holder".into(),
            boot_id: "boot".into(),
            host_incarnation: "source-daemon".into(),
        };
        let mut recipe = launch_recipe("/var/tmp");
        recipe.program = "codex".into();
        recipe.arguments = vec!["--remote".into(), format!("unix://{}", socket.display())];
        let original = crate::chat::Session {
            id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe,
                session_id: Some("source-conversation".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            native_continuity: if owned {
                crate::chat::NativeContinuity::Durable {
                    provider_version: "0.157.1".into(),
                }
            } else {
                crate::chat::NativeContinuity::ReconnectOnly
            },
            owned_codex_listener: owned.then_some(crate::chat::OwnedCodexListener {
                socket,
                allocator: proof,
            }),
        };
        assert!(original.has_valid_native_continuity());
        app.card_state_mut(source_id).unwrap().kind = CardKind::Chat(original.clone());
        drop(app.clone_focused_pane());
        let cloned = app.workspace.focused().unwrap();
        assert_ne!(cloned, source_id);
        let CardKind::Chat(copy) = &app.card_state(cloned).unwrap().kind else {
            panic!("cloned conversation must stay native");
        };
        assert_ne!(copy.id, original.id);
        assert_eq!(copy.launch.recipe.arguments, ["--remote", "unix://"]);
        assert!(copy.is_unstarted());
        assert!(copy.has_valid_native_continuity());
        assert_eq!(
            app.card_state(source_id).unwrap().kind,
            CardKind::Chat(original)
        );
    }
}

fn diagnostic(message: &str) -> crate::lsp::LspDiagnostic {
    crate::lsp::LspDiagnostic::new(
        crate::lsp::LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1)),
        crate::lsp::LspSeverity::Error,
        message.into(),
        None,
        None,
    )
}

fn completion_item(label: &str) -> crate::lsp::LspCompletionItem {
    crate::lsp::LspCompletionItem {
        label: label.into(),
        kind: None,
        detail: None,
        documentation: None,
        sort_text: None,
        filter_text: None,
        preselect: false,
        deprecated: false,
        edit: crate::lsp::LspCompletionEdit {
            range: crate::lsp::LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 0)),
            replace_range: None,
            new_text: label.into(),
            snippet: None,
        },
        additional_edits: Vec::new(),
    }
}

fn lsp_test_app() -> Cagents {
    let mut app = context_inspector_test_app(false);
    // No configured server, so nothing in these tests can spawn a process
    // or depend on one being installed on the machine running them.
    app.settings.language_servers = Vec::new();
    app.lsp = LspManager::new(Vec::new());
    app
}

fn repository_card(app: &mut Cagents, worktree: &str) -> CardId {
    let card = CardId::new();
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from(worktree),
        worktree: PathBuf::from(worktree),
    };
    // The durable card names its kind, which decides whether it has an
    // inspector.
    Arc::make_mut(&mut app.durable).cards.push(CardState {
        id: card,
        generation: 0,
        title: "Repository".into(),
        width_fraction: 0.7,
        existence: Existence::Open,
        kind: CardKind::Repository(view.clone()),
    });
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(
            repository_widget::RepositoryRuntime::file_list_fixture(
                view,
                &["src/main.rs", "src/other.rs"],
            ),
        )),
    );
    card
}

#[test]
fn editor_language_results_expand_their_section() {
    use context_sections::Section;
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    for (action, section) in [
        (
            repository_widget::EditorLspAction::Diagnostics,
            Section::Problems,
        ),
        (
            repository_widget::EditorLspAction::References,
            Section::Locations,
        ),
    ] {
        app.context_sections.set_open(section, false);
        let _ = app.update(Message::Repository(RepositoryAction::Language {
            card,
            action,
        }));
        assert!(
            app.pane_inspectors.is_open(card),
            "the editor's own inspector opens"
        );
        assert!(
            app.context_sections.is_open(section),
            "a collapsed section hides the requested language results"
        );
    }
    let doc = LspDocumentId(1);
    let request = crate::lsp::LspRequestId(1);
    app.lsp_documents.insert(doc, card);
    lsp_state(&mut app, card).attachment.editor_navigation = Some(request);
    app.context_sections.set_open(Section::Locations, false);
    let _ = app.update(Message::Lsp(LspEvent::Locations {
        doc,
        request,
        kind: crate::lsp::LocationKind::Definition,
        items: Vec::new(),
    }));
    assert!(app.pane_inspectors.is_open(card));
    assert!(
        app.context_sections.is_open(Section::Locations),
        "a definition response that cannot jump must expose its results"
    );
}

fn lsp_state(app: &mut Cagents, card: CardId) -> &mut LspInspectorState {
    app.lsp_contexts.entry(card).or_default()
}

fn notice(app: &Cagents) -> Option<&str> {
    app.status.as_ref().map(|notice| notice.message.as_str())
}

/// A card is closed far more often than a file is. Leaving its document
/// open would keep a server analysing and publishing for a file nobody can
/// see, and would grow all three maps for the life of the session.
#[test]
fn closing_a_card_retires_everything_it_held_at_the_lsp_boundary() {
    let mut app = lsp_test_app();
    // The fixture's durable card is the one the workspace closes.
    let card = repository_card(&mut app, "/repo");
    app.workspace
        .add_card(
            WorkspaceCard::new(card, "Repository", CardWidth::Fraction(0.7)).unwrap(),
            0,
            true,
        )
        .unwrap();
    app.workspace.request_close(card).unwrap();
    let state = lsp_state(&mut app, card);
    state.attachment.document = Some(LspDocumentId(1));
    state.path = Some(PathBuf::from("/repo/a.rs"));
    app.lsp_documents.insert(LspDocumentId(1), card);
    app.lsp_pending_jumps.insert(
        card,
        PendingLspJump {
            path: PathBuf::from("/repo/b.rs"),
            at: LspPosition::new(0, 0),
            coordinates: LspCoordinates::Scalar,
        },
    );

    let _ = app.update(Message::CommitClose(card));

    assert!(!app.cards.contains_key(&card));
    assert!(!app.lsp_contexts.contains_key(&card));
    assert!(!app.lsp_documents.contains_key(&LspDocumentId(1)));
    assert!(!app.lsp_pending_jumps.contains_key(&card));
}

/// Diagnostics are unsolicited and replace each other wholesale, so a list
/// on screen can outlive the text it describes. Jumping into a stale one
/// puts the caret on a line the server never named.
/// The two versions are equal only when both are known. An application
/// that kept a count of its own would satisfy this predicate while the
/// manager had silently moved on - a restart, a crash-respawn, or a second
/// card opening the same file all bump the manager's version without any
/// edit having been made.
#[test]
fn diagnostics_are_current_only_when_both_versions_are_known_and_equal() {
    assert!(crate::editor_assistance::diagnostics_are_current(
        Some(4),
        Some(4)
    ));
    assert!(!crate::editor_assistance::diagnostics_are_current(
        Some(4),
        Some(5)
    ));
    assert!(!crate::editor_assistance::diagnostics_are_current(
        Some(5),
        Some(4)
    ));
    // No document: nothing published for it can still describe it.
    assert!(!crate::editor_assistance::diagnostics_are_current(
        Some(4),
        None
    ));
    // Nothing published: there is no list to jump into.
    assert!(!crate::editor_assistance::diagnostics_are_current(
        None,
        Some(4)
    ));
    assert!(!crate::editor_assistance::diagnostics_are_current(
        None, None
    ));
}

#[test]
fn inexact_editor_changes_invalidate_diagnostics_through_the_real_drain() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    for (original, published_version, preserves) in [
        ("first\nerror\r", 1, false),
        ("first\nerror\n", 0, false),
        ("first\nerror\n", 1, true),
    ] {
        let mut app = lsp_test_app();
        app.lsp = LspManager::new(AppSettings::default().language_servers);
        let card = repository_card(&mut app, "/repo");
        let (runtime, _) = RepositoryRuntime::open_fixture(card, "a.rs", original);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        drop(app.drain_lsp_intents(card));
        let doc = app.lsp_document(card).unwrap();
        assert_eq!(app.lsp.document_version(doc), Some(1));
        let publish = |version| {
            Message::Lsp(LspEvent::Diagnostics {
                doc,
                version,
                items: vec![diagnostic("standing error")],
            })
        };
        drop(app.update(publish(1)));
        // A manager reopen can advance beyond the standing publication before
        // the next edit. Its old coordinates must not become current again.
        app.lsp_contexts
            .get_mut(&card)
            .unwrap()
            .attachment
            .diagnostics_version = Some(published_version);
        let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
            unreachable!()
        };
        runtime.move_cursor_to(LspPosition::new(0, 0));
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(Arc::new(
                "\n".into(),
            ))),
        }));
        drop(app.drain_lsp_intents(card));
        assert_eq!(app.lsp.document_version(doc), Some(2));
        assert_eq!(
            app.lsp_contexts[&card]
                .attachment
                .context
                .diagnostics
                .is_empty(),
            !preserves
        );
        assert_eq!(
            app.lsp_contexts[&card].attachment.diagnostics_version,
            preserves.then_some(2)
        );
        if preserves {
            assert_eq!(
                app.lsp_contexts[&card].attachment.context.diagnostics[0]
                    .range
                    .start,
                LspPosition::new(1, 0)
            );
        }
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.diagnostic_navigation_available(), preserves);
        // A late publication cannot reinstall marks for the old coordinates.
        drop(app.update(publish(1)));
        assert_eq!(
            app.lsp_contexts[&card]
                .attachment
                .context
                .diagnostics
                .is_empty(),
            !preserves
        );
        drop(app.update(publish(2)));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert!(runtime.diagnostic_navigation_available());
        assert_eq!(
            app.lsp_contexts[&card].attachment.diagnostics_version,
            Some(2)
        );
    }
}

#[test]
fn resource_events_refresh_every_document_even_when_the_routing_document_closed() {
    for close_routing_document in [false, true] {
        let mut app = lsp_test_app();
        let first = repository_card(&mut app, "/repo");
        let last = repository_card(&mut app, "/repo");
        for (card, doc) in [(first, LspDocumentId(1)), (last, LspDocumentId(2))] {
            let state = lsp_state(&mut app, card);
            state.attachment.document = Some(doc);
            state.attachment.context.status = Some(LspStatus::Ready);
            app.lsp_documents.insert(doc, card);
        }
        if close_routing_document {
            app.close_lsp_document(last);
        }
        // This is the state after the watchdog retires the shared session:
        // the manager reports Disabled while the UI still caches Ready.
        assert_eq!(app.lsp.status(LspDocumentId(1)), LspStatus::Disabled);
        drop(app.update(Message::Lsp(LspEvent::Resources {
            doc: LspDocumentId(2),
        })));
        assert_eq!(
            app.lsp_contexts[&first].attachment.context.status,
            Some(app.lsp.status(LspDocumentId(1)))
        );
        if !close_routing_document {
            assert_eq!(
                app.lsp_contexts[&last].attachment.context.status,
                Some(app.lsp.status(LspDocumentId(2)))
            );
        }
    }
}

#[test]
fn resource_updates_clear_refusals_without_a_registered_document() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    let doc = LspDocumentId(1);
    let state = lsp_state(&mut app, card);
    state.attachment.document = Some(doc);
    state.attachment.context.status = Some(LspStatus::Unavailable {
        reason: "admission refused before document registration".into(),
    });
    app.lsp_documents.insert(doc, card);
    assert_eq!(app.lsp.document_version(doc), None);
    app.apply_lsp_event(LspEvent::Resources { doc });
    assert_eq!(
        app.lsp_context(card).unwrap().status,
        Some(LspStatus::Disabled)
    );
}

#[tokio::test]
async fn resource_updates_preserve_cap_refusal_and_retry_after_edit_and_holder_close() {
    let (manager, holder) = crate::lsp::tests::progress_fixture().await;
    let mut app = lsp_test_app();
    app.lsp = manager.with_lifecycle(600, 1);
    app.settings.language_servers = app.lsp.servers().to_vec();
    app.settings.lsp_max_sessions = 1;
    app.next_lsp_document = holder.doc.0 + 1;
    let holder_card = repository_card(&mut app, "/holder");
    lsp_state(&mut app, holder_card).attachment.document = Some(holder.doc);
    app.lsp_documents.insert(holder.doc, holder_card);
    assert!(app.lsp.event_is_current(&holder.ready));
    app.apply_lsp_event(holder.ready.clone());
    assert_eq!(
        app.lsp_context(holder_card).unwrap().status,
        Some(LspStatus::Ready)
    );

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"cap-refused\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/main.rs"), "before\n").unwrap();
    let card = repository_card(&mut app, root.to_str().unwrap());
    let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: "src/main.rs".into(),
    }));
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity: repository_widget::AsyncIdentity {
            card,
            worktree: root.to_owned(),
            path: "src/main.rs".into(),
            generation: 1,
        },
        result: crate::editor::load_file(
            root,
            Path::new("src/main.rs"),
            crate::editor::PdfTone::Original,
        ),
    }));
    assert_eq!(runtime.editor_text().as_deref(), Some("before\n"));
    drop(app.drain_lsp_intents(card));
    let doc = app.lsp_document(card).unwrap();
    let events = app.lsp.events();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            app.apply_lsp_event(events.recv().await.unwrap());
            if matches!(
                app.lsp_context(card).unwrap().status,
                Some(LspStatus::Unavailable { .. })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let refusal = app.lsp_context(card).unwrap().status.clone();
    assert!(matches!(&refusal, Some(LspStatus::Unavailable { reason })
        if reason.contains("limit (1) reached")));
    assert_eq!(app.lsp.status(doc), LspStatus::Disabled);
    assert_eq!(app.lsp.document_version(doc), Some(1));
    let restart_action =
        |app: &Cagents| inspector::restart_action(card, app.lsp_context(card).unwrap());
    // The first telemetry refresh occurs before any edit in the native
    // failure: another live server must not erase this document's refusal.
    app.apply_lsp_event(LspEvent::Resources { doc: holder.doc });
    assert_eq!(app.lsp_context(card).unwrap().status, refusal);
    assert!(restart_action(&app).is_some());
    let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(Arc::new(
            "🌲".into(),
        ))),
    }));
    assert_eq!(runtime.editor_text().as_deref(), Some("🌲before\n"));
    drop(app.drain_lsp_intents(card));
    app.apply_lsp_event(LspEvent::Resources { doc: holder.doc });
    assert_eq!(app.lsp_context(card).unwrap().status, refusal);
    assert_eq!(app.lsp.document_version(doc), Some(2));
    assert_eq!(
        app.lsp
            .scalar_position(doc, LspPosition::new(0, 2))
            .unwrap(),
        LspPosition::new(0, 1)
    );
    app.close_lsp_document(holder_card);
    // Even telemetry routed through the now-closed last holder must keep
    // the refused editor's ordinary inspector action available.
    app.apply_lsp_event(LspEvent::Resources { doc: holder.doc });
    let restart = restart_action(&app).expect("retained refusal offers ordinary Retry");
    drop(app.update(Message::ContextInspector(restart)));
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            app.apply_lsp_event(events.recv().await.unwrap());
            if app.lsp_context(card).unwrap().status == Some(LspStatus::Ready) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(app.lsp.status(doc), LspStatus::Ready);
    assert_eq!(
        app.lsp
            .scalar_position(doc, LspPosition::new(0, 2))
            .unwrap(),
        LspPosition::new(0, 1)
    );
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(runtime.editor_text().as_deref(), Some("🌲before\n"));

    app.settings.language_servers.clear();
    drop(app.synchronize_language_servers());
    assert!(app.lsp_document(card).is_none());
    assert_eq!(
        app.lsp_context(card).unwrap().status,
        Some(LspStatus::Disabled)
    );
    assert!(restart_action(&app).is_none());
    app.close_lsp_document(card);
    app.apply_lsp_event(LspEvent::Resources { doc: holder.doc });
    assert!(app.lsp_context(card).unwrap().status.is_none());
    assert!(restart_action(&app).is_none());
    drop(app.open_lsp_document(
        card,
        &root.join("notes.txt"),
        root,
        1,
        &repository_widget::LspChangeText::Text("plain".into()),
    ));
    app.apply_lsp_event(LspEvent::Resources { doc: holder.doc });
    assert_eq!(
        app.lsp_context(card).unwrap().status,
        Some(LspStatus::Disabled)
    );
    assert!(restart_action(&app).is_none());
    app.lsp.begin_shutdown().await;
}

/// The jump arm must consult that predicate rather than jumping blind.
#[test]
fn a_jump_into_stale_diagnostics_is_refused() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    // A published list, and a document the manager does not hold, which is
    // exactly the shape the predicate refuses.
    lsp_state(&mut app, card).attachment.document = Some(LspDocumentId(1));
    lsp_state(&mut app, card).attachment.diagnostics_version = Some(6);

    let _ = app.update_lsp(InspectorAction::JumpToLspDiagnostic {
        card,
        at: LspPosition::new(3, 0),
    });
    assert_eq!(
        notice(&app),
        Some("these diagnostics describe an earlier version of the file")
    );
}

fn completion_targets_for_test(
    card: CardId,
    items: Vec<crate::lsp::LspCompletionItem>,
) -> Vec<repository_widget::CompletionTarget> {
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "tail\n");
    runtime.set_editor_completions(items);
    runtime.editor_completion_targets(card)
}

fn publish_test_completions(
    app: &mut Cagents,
    card: CardId,
    items: Vec<crate::lsp::LspCompletionItem>,
) -> Vec<repository_widget::CompletionTarget> {
    let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
        panic!("the fixture owns an actual editor");
    };
    runtime.set_editor_completions(items);
    let targets = runtime.editor_completion_targets(card);
    assert!(
        !targets.is_empty(),
        "the fixture has a registered editable document"
    );
    let intent = runtime.editor_completion_intent();
    let state = lsp_state(app, card);
    state.attachment.completion_intent = intent;
    state.attachment.context.completions = targets.clone();
    targets
}

// A current-thread runtime is entered, never driven, by these tests.
// Registration has a real manager version/request ID, while discovery and
// server tasks cannot run; response injection exercises App admission only.
fn completion_intent_fixture() -> (Cagents, CardId, LspDocumentId) {
    completion_intent_fixture_text("tail\n", false)
}

fn completion_intent_fixture_text(text: &str, focused: bool) -> (Cagents, CardId, LspDocumentId) {
    let mut app = if focused {
        context_inspector_test_app(true)
    } else {
        lsp_test_app()
    };
    app.lsp = LspManager::new(AppSettings::default().language_servers);
    let card = if focused {
        app.workspace.focused().unwrap()
    } else {
        repository_card(&mut app, "/repo")
    };
    let (runtime, _) = RepositoryRuntime::open_fixture(card, "main.rs", text);
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    drop(app.drain_lsp_intents(card));
    let doc = app
        .lsp_document(card)
        .expect("the editable fixture is registered");
    assert!(app.lsp.document_version(doc).is_some());
    (app, card, doc)
}

fn request_test_completion(
    app: &mut Cagents,
    card: CardId,
) -> (crate::lsp::LspRequestId, LspPosition) {
    drop(app.request_lsp(card, LspRequestKind::Completion));
    app.lsp_contexts[&card]
        .attachment
        .completion_request
        .unwrap()
}

fn navigate_find_key(app: &mut Cagents, backwards: bool) {
    let key = keyboard::Key::Named(keyboard::key::Named::F3);
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::F3),
            location: keyboard::Location::Standard,
            modifiers: if backwards {
                keyboard::Modifiers::SHIFT
            } else {
                keyboard::Modifiers::empty()
            },
            text: None,
            repeat: false,
        },
        captured: false,
    }));
}

#[test]
fn find_navigation_retires_shown_and_pending_completion_across_caret_round_trip() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    let (mut app, card, doc) = completion_intent_fixture_text("tail tail\n", true);
    drop(app.update(Message::Repository(RepositoryAction::OpenFind { card })));
    drop(
        app.update(Message::Repository(RepositoryAction::FindQueryChanged {
            card,
            query: "tail".into(),
        })),
    );
    let shown = publish_test_completions(&mut app, card, vec![completion_item("shown")]).remove(0);
    let pending = request_test_completion(&mut app, card);
    let version = app.lsp.document_version(doc);
    navigate_find_key(&mut app, false);
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_ne!(runtime.cursor_position(), Some(pending.1));
    assert!(runtime.editor_completion_targets(card).is_empty());
    assert!(
        app.lsp_contexts[&card]
            .attachment
            .completion_request
            .is_none()
    );
    assert!(
        app.lsp_contexts[&card]
            .attachment
            .context
            .completions
            .is_empty()
    );
    navigate_find_key(&mut app, true);
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(runtime.cursor_position(), Some(pending.1));
    assert_eq!(runtime.editor_text().as_deref(), Some("tail tail\n"));
    assert_eq!(app.lsp.document_version(doc), version);
    app.apply_lsp_event(LspEvent::Completion {
        request: pending.0,
        doc,
        items: vec![completion_item("late")],
    });
    drop(app.update_editor_lsp(
        card,
        repository_widget::EditorLspAction::Choose {
            index: 0,
            target: shown,
        },
    ));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(runtime.editor_text().as_deref(), Some("tail tail\n"));
    assert!(runtime.editor_completion_targets(card).is_empty());

    let fresh = request_test_completion(&mut app, card);
    app.apply_lsp_event(LspEvent::Completion {
        request: pending.0,
        doc,
        items: vec![completion_item("late")],
    });
    assert_eq!(
        app.lsp_contexts[&card].attachment.completion_request,
        Some(fresh)
    );
    app.apply_lsp_event(LspEvent::Completion {
        request: fresh.0,
        doc,
        items: vec![completion_item("fresh")],
    });
    let target = app.lsp_contexts[&card].attachment.context.completions[0].clone();
    drop(app.update_editor_lsp(
        card,
        repository_widget::EditorLspAction::Choose { index: 0, target },
    ));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(runtime.editor_text().as_deref(), Some("freshtail tail\n"));
}

#[test]
fn same_text_find_replacement_retires_completion_without_advancing_text_version() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    for all in [false, true] {
        let (mut app, card, doc) = completion_intent_fixture_text("tail tail\n", true);
        drop(app.update(Message::Repository(RepositoryAction::OpenFind { card })));
        drop(
            app.update(Message::Repository(RepositoryAction::FindQueryChanged {
                card,
                query: "tail".into(),
            })),
        );
        drop(
            app.update(Message::Repository(RepositoryAction::ReplaceQueryChanged {
                card,
                query: "tail".into(),
            })),
        );
        let shown = publish_test_completions(&mut app, card, vec![completion_item("shown")]);
        let pending = request_test_completion(&mut app, card);
        let version = app.lsp.document_version(doc);
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.selected_text().as_deref(), Some("tail"));
        let dirty = runtime.is_dirty();
        drop(
            app.update(Message::Repository(RepositoryAction::ReplaceMatch {
                card,
                all,
            })),
        );
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.editor_text().as_deref(), Some("tail tail\n"));
        assert!(runtime.selected_text().is_none());
        assert_eq!(runtime.is_dirty(), dirty);
        assert_eq!(app.lsp.document_version(doc), version);
        assert!(runtime.editor_completion_targets(card).is_empty());
        assert!(
            app.lsp_contexts[&card]
                .attachment
                .completion_request
                .is_none()
        );
        assert!(
            app.lsp_contexts[&card]
                .attachment
                .context
                .completions
                .is_empty()
        );
        app.apply_lsp_event(LspEvent::Completion {
            request: pending.0,
            doc,
            items: vec![completion_item("late")],
        });
        drop(app.update_editor_lsp(
            card,
            repository_widget::EditorLspAction::Choose {
                index: 0,
                target: shown[0].clone(),
            },
        ));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.editor_text().as_deref(), Some("tail tail\n"));
    }
}

#[test]
fn deferred_find_selection_retires_completion_only_for_accepted_visible_callback() {
    use cosmic::iced::futures::{StreamExt, executor::block_on};
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    for hidden in [false, true] {
        let (mut app, card, doc) = completion_intent_fixture_text("tail tail\n", true);
        drop(app.update(Message::Repository(RepositoryAction::OpenFind { card })));
        drop(
            app.update(Message::Repository(RepositoryAction::FindOptionsChanged {
                card,
                option: crate::ui_controls::FindOption::Regex,
                enabled: true,
            })),
        );
        let shown = publish_test_completions(&mut app, card, vec![completion_item("shown")]);
        let pending = request_test_completion(&mut app, card);
        let version = app.lsp.document_version(doc);
        let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
            unreachable!()
        };
        let task = runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "tail".into(),
        });
        let mut stream =
            iced::runtime::task::into_stream(task).expect("regex Find uses its real worker");
        // Poll only the Find future. The entered manager runtime is never
        // driven, so no discovery/server tasks can execute in this fixture.
        let result = block_on(async {
            loop {
                let event = stream.next().await.expect("Find produces a callback");
                if let iced::runtime::Action::Output(result @ RepositoryAction::FindLoaded { .. }) =
                    event
                {
                    break result;
                }
            }
        });
        let mut stale = result.clone();
        let RepositoryAction::FindLoaded { ticket, .. } = &mut stale else {
            unreachable!()
        };
        *ticket = uuid::Uuid::new_v4();
        drop(app.update(Message::Repository(stale)));
        let mut foreign = result.clone();
        let RepositoryAction::FindLoaded {
            card: foreign_card, ..
        } = &mut foreign
        else {
            unreachable!()
        };
        *foreign_card = CardId::new();
        drop(app.update(Message::Repository(foreign)));
        assert_eq!(
            app.lsp_contexts[&card].attachment.completion_request,
            Some(pending)
        );
        assert_eq!(
            app.lsp_contexts[&card].attachment.context.completions,
            shown
        );
        if hidden {
            let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
                unreachable!()
            };
            drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
            assert!(!runtime.editable_surface_visible());
        }
        drop(app.update(Message::Repository(result)));
        assert_eq!(app.lsp.document_version(doc), version);
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.editor_text().as_deref(), Some("tail tail\n"));
        if hidden {
            assert_eq!(runtime.cursor_position(), Some(pending.1));
            assert_eq!(
                app.lsp_contexts[&card].attachment.completion_request,
                Some(pending)
            );
            assert_eq!(
                app.lsp_contexts[&card].attachment.context.completions,
                shown
            );
        } else {
            assert_ne!(runtime.cursor_position(), Some(pending.1));
            assert!(runtime.editor_completion_targets(card).is_empty());
            assert!(
                app.lsp_contexts[&card]
                    .attachment
                    .completion_request
                    .is_none()
            );
            assert!(
                app.lsp_contexts[&card]
                    .attachment
                    .context
                    .completions
                    .is_empty()
            );
            app.apply_lsp_event(LspEvent::Completion {
                request: pending.0,
                doc,
                items: vec![completion_item("late")],
            });
            assert!(
                app.lsp_contexts[&card]
                    .attachment
                    .context
                    .completions
                    .is_empty()
            );
        }
    }
}

#[test]
fn completion_caret_round_trip_retires_old_answer_and_accepts_a_fresh_request() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    let (mut app, card, doc) = completion_intent_fixture();
    let version = app.lsp.document_version(doc);
    let (old, at) = request_test_completion(&mut app, card);
    for (step, motion) in [
        widget::text_editor::Motion::Right,
        widget::text_editor::Motion::Left,
    ]
    .into_iter()
    .enumerate()
    {
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card,
                action: widget::text_editor::Action::Move(motion),
            })),
        );
        if step == 0 {
            let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
                unreachable!()
            };
            assert_ne!(
                runtime.cursor_position(),
                Some(at),
                "the caret really leaves A"
            );
        }
    }
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(runtime.cursor_position(), Some(at));
    assert_eq!(runtime.editor_text().as_deref(), Some("tail\n"));
    assert_eq!(app.lsp.document_version(doc), version);
    let old_answer = || LspEvent::Completion {
        request: old,
        doc,
        items: vec![completion_item("old")],
    };
    app.apply_lsp_event(old_answer());
    assert!(
        app.lsp_contexts[&card]
            .attachment
            .completion_request
            .is_none()
    );
    assert!(
        app.lsp_contexts[&card]
            .attachment
            .context
            .completions
            .is_empty()
    );
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert!(runtime.editor_completion_targets(card).is_empty());

    let pending = request_test_completion(&mut app, card);
    assert_ne!(pending.0, old);
    app.apply_lsp_event(old_answer());
    assert_eq!(
        app.lsp_contexts[&card].attachment.completion_request,
        Some(pending)
    );
    app.apply_lsp_event(LspEvent::Completion {
        request: pending.0,
        doc,
        items: vec![completion_item("fresh")],
    });
    let target = app.lsp_contexts[&card].attachment.context.completions[0].clone();
    drop(app.update_editor_lsp(
        card,
        repository_widget::EditorLspAction::Choose { index: 0, target },
    ));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(runtime.editor_text().as_deref(), Some("freshtail\n"));
    assert!(
        app.lsp_contexts[&card]
            .attachment
            .context
            .completions
            .is_empty()
    );
}

#[test]
fn no_change_editor_intent_retires_shown_inspector_candidates_and_pending_request() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    for action in [
        widget::text_editor::Action::Move(widget::text_editor::Motion::Left),
        widget::text_editor::Action::ClearSelection,
    ] {
        let (mut app, card, doc) = completion_intent_fixture();
        let target =
            publish_test_completions(&mut app, card, vec![completion_item("shown")]).remove(0);
        let version = app.lsp.document_version(doc);
        let pending = request_test_completion(&mut app, card);
        let state = app.lsp_contexts.get_mut(&card).unwrap();
        state.attachment.context.locations =
            Some((crate::lsp::LocationKind::References, Vec::new()));
        state.attachment.context.diagnostics = vec![diagnostic("standing diagnostic")];
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card,
                action,
            })),
        );
        assert_eq!(app.lsp.document_version(doc), version);
        let state = &app.lsp_contexts[&card];
        assert!(state.attachment.completion_request.is_none());
        assert!(state.attachment.context.completions.is_empty());
        assert!(state.attachment.context.locations.is_some());
        assert_eq!(state.attachment.context.diagnostics.len(), 1);
        app.apply_lsp_event(LspEvent::Completion {
            request: pending.0,
            doc,
            items: vec![completion_item("late")],
        });
        drop(app.update_editor_lsp(
            card,
            repository_widget::EditorLspAction::Choose { index: 0, target },
        ));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.cursor_position(), Some(pending.1));
        assert_eq!(runtime.editor_text().as_deref(), Some("tail\n"));
        assert!(runtime.editor_completion_targets(card).is_empty());
        assert!(
            app.lsp_contexts[&card]
                .attachment
                .context
                .completions
                .is_empty()
        );
    }
}

#[test]
fn scroll_preserves_pending_and_shown_completion_and_inspector_activation() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    let (mut app, card, doc) = completion_intent_fixture();
    let pending = request_test_completion(&mut app, card);
    let scroll = || {
        Message::Repository(RepositoryAction::EditorAction {
            card,
            action: widget::text_editor::Action::Scroll { lines: 2 },
        })
    };
    drop(app.update(scroll()));
    assert_eq!(
        app.lsp_contexts[&card].attachment.completion_request,
        Some(pending)
    );
    app.apply_lsp_event(LspEvent::Completion {
        request: pending.0,
        doc,
        items: vec![completion_item("scroll")],
    });
    let target = app.lsp_contexts[&card].attachment.context.completions[0].clone();
    drop(app.update(scroll()));
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.completions,
        vec![target.clone()]
    );
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(
        runtime.editor_completion_targets(card),
        vec![target.clone()]
    );
    drop(app.update_editor_lsp(
        card,
        repository_widget::EditorLspAction::Choose { index: 0, target },
    ));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert_eq!(runtime.editor_text().as_deref(), Some("scrolltail\n"));
}

#[test]
fn rejected_hidden_and_foreign_editor_actions_preserve_completion_intents() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    for control in 0..4 {
        let (mut app, card, _) = completion_intent_fixture();
        let targets = publish_test_completions(&mut app, card, vec![completion_item("current")]);
        let pending = request_test_completion(&mut app, card);
        let action_card = match control {
            0 => {
                app.worktree_dialog = Some(WorktreeDialog::new(
                    ProjectId::new(),
                    PathBuf::from("/repo"),
                    WorktreeDraft::default(),
                ));
                assert!(app.modal_is_open());
                card
            }
            1 => {
                let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&card) else {
                    unreachable!()
                };
                drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
                assert!(runtime.shows_diff());
                assert!(!runtime.editable_surface_visible());
                card
            }
            2 => CardId::new(),
            _ => {
                app.cards.insert(card, CardRuntime::Settings);
                card
            }
        };
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: action_card,
                action: widget::text_editor::Action::Move(widget::text_editor::Motion::Right),
            })),
        );
        assert_eq!(
            app.lsp_contexts[&card].attachment.completion_request,
            Some(pending)
        );
        assert_eq!(
            app.lsp_contexts[&card].attachment.context.completions,
            targets
        );
    }
}

/// Both UI routes must retain the chosen publication and exact edits,
/// including a replacement that reuses the same label.
#[test]
fn a_completion_whose_label_moved_is_not_applied() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    for replacement_kind in 0..3 {
        let mut app = lsp_test_app();
        // Register a real document without driving discovery or a server.
        app.lsp = LspManager::new(AppSettings::default().language_servers);
        let card = repository_card(&mut app, "/repo");
        let (runtime, _) = RepositoryRuntime::open_fixture(card, "main.rs", "tail\n");
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        drop(app.drain_lsp_intents(card));
        let held =
            publish_test_completions(&mut app, card, vec![completion_item("alpha")]).remove(0);
        let mut replacement = completion_item(if replacement_kind == 0 {
            "beta"
        } else {
            "alpha"
        });
        if replacement_kind == 1 {
            replacement.edit.new_text = "different-edit".into();
        }
        let expected = format!("{}tail\n", replacement.edit.new_text);
        let fresh = publish_test_completions(&mut app, card, vec![replacement]).remove(0);
        drop(app.update_editor_lsp(
            card,
            repository_widget::EditorLspAction::Choose {
                index: 0,
                target: held,
            },
        ));
        assert_eq!(
            notice(&app),
            Some("the completion list changed; nothing was applied")
        );
        assert_eq!(
            app.lsp_contexts[&card].attachment.context.completions.len(),
            1
        );
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.editor_text().as_deref(), Some("tail\n"));
        assert_eq!(
            runtime.editor_completion_targets(card).len(),
            1,
            "rejecting an old action must not dismiss the fresh response"
        );
        app.status = None;
        drop(app.update_editor_lsp(
            card,
            repository_widget::EditorLspAction::Choose {
                index: 0,
                target: fresh,
            },
        ));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.editor_text().as_deref(), Some(expected.as_str()));
        assert!(
            app.lsp_contexts[&card]
                .attachment
                .context
                .completions
                .is_empty()
        );
    }
}

/// An index past the end of a list that shrank must not panic or apply.
#[test]
fn a_completion_index_past_the_end_applies_nothing() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    let target = completion_targets_for_test(card, vec![completion_item("alpha")]).remove(0);
    lsp_state(&mut app, card).attachment.context.completions = vec![target.clone()];
    drop(app.update_editor_lsp(
        card,
        repository_widget::EditorLspAction::Choose { index: 4, target },
    ));
    assert_eq!(notice(&app), None);
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.completions.len(),
        1
    );
}

/// A card the saved state could not restore is reported until the card
/// opens here anyway, from a shared view that still holds it (#3062).
#[test]
fn a_card_restore_report_stands_until_the_card_opens() {
    let mut app = context_inspector_test_app(true);
    let card = app.durable.cards[0].clone();
    Arc::make_mut(&mut app.durable)
        .cards
        .retain(|held| held.id != card.id);
    let notice = card_restore_notice(&cagents::persistence::RestoreWarning {
        card: card.id,
        title: "git log".into(),
        problem: cagents::persistence::RestoreProblem::Unreadable(
            "corrupt stored terminal continuity".into(),
        ),
    });
    assert_eq!(
        notice.message,
        "Could not restore “git log”. Its saved data is kept."
    );
    assert_eq!(
        notice.details.as_deref(),
        Some("corrupt stored terminal continuity")
    );
    app.notices.record(notice);
    let key = NoticeKey::CardRestore(card.id);
    app.resolve_card_notices();
    assert!(app.notices.has(key.clone()), "the card is still missing");

    Arc::make_mut(&mut app.durable).cards.push(card);
    app.resolve_card_notices();
    assert!(!app.notices.has(key));

    // A missing record leaves nothing kept to claim.
    let missing = card_restore_notice(&cagents::persistence::RestoreWarning {
        card: CardId::new(),
        title: "git log".into(),
        problem: cagents::persistence::RestoreProblem::Missing,
    });
    assert_eq!(
        missing.message,
        "Could not restore “git log”: its saved record is missing."
    );
    assert_eq!(missing.details, None);
}

/// Two cards can hold two documents at once, and diagnostics arrive
/// unsolicited for either. Routing them by anything but the document id
/// puts one file's errors on another file's card.
#[test]
fn diagnostics_land_on_the_card_that_owns_the_document() {
    // Enter without driving: production open registers a manager-owned
    // version, but discovery and language-server processes never run.
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = executor.enter();
    let mut app = lsp_test_app();
    app.lsp = LspManager::new(AppSettings::default().language_servers);
    let first = repository_card(&mut app, "/repo");
    let second = repository_card(&mut app, "/repo");
    for (card, name, text) in [
        (first, "first.rs", "first\n"),
        (second, "second.rs", "second\n"),
    ] {
        let (runtime, _) = RepositoryRuntime::open_fixture(card, name, text);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        drop(app.drain_lsp_intents(card));
        let doc = app.lsp_document(card).unwrap();
        assert_eq!(app.lsp.document_version(doc), Some(1));
        assert_eq!(app.lsp_documents.get(&doc), Some(&card));
        assert_eq!(app.lsp_contexts[&card].attachment.document, Some(doc));
    }
    let first_doc = app.lsp_document(first).unwrap();
    let second_doc = app.lsp_document(second).unwrap();
    assert_ne!(first_doc, second_doc);
    let publish = |doc, message: &str| {
        Message::Lsp(LspEvent::Diagnostics {
            doc,
            version: 1,
            items: vec![diagnostic(message)],
        })
    };
    drop(app.update(publish(second_doc, "second diagnostic")));
    assert_eq!(
        app.lsp_contexts[&second].attachment.context.diagnostics,
        vec![diagnostic("second diagnostic")]
    );
    assert_eq!(
        app.lsp_contexts[&second].attachment.diagnostics_version,
        Some(1)
    );
    assert!(
        app.lsp_contexts[&first]
            .attachment
            .context
            .diagnostics
            .is_empty()
    );
    assert_eq!(
        app.lsp_contexts[&first].attachment.diagnostics_version,
        None
    );
    for (card, expected) in [(first, false), (second, true)] {
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert_eq!(runtime.diagnostic_navigation_available(), expected);
    }

    drop(app.update(publish(first_doc, "first diagnostic")));
    assert_eq!(
        app.lsp_contexts[&first].attachment.context.diagnostics,
        vec![diagnostic("first diagnostic")]
    );
    assert_eq!(
        app.lsp_contexts[&first].attachment.diagnostics_version,
        Some(1)
    );
    assert_eq!(
        app.lsp_contexts[&second].attachment.context.diagnostics,
        vec![diagnostic("second diagnostic")]
    );
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&first) else {
        unreachable!()
    };
    assert!(runtime.diagnostic_navigation_available());

    app.close_lsp_document(second);
    assert_eq!(app.lsp.document_version(second_doc), None);
    assert!(!app.lsp_documents.contains_key(&second_doc));
    drop(app.update(publish(second_doc, "late closed diagnostic")));
    assert!(
        app.lsp_contexts[&second]
            .attachment
            .context
            .diagnostics
            .is_empty()
    );
    assert_eq!(
        app.lsp_contexts[&second].attachment.diagnostics_version,
        None
    );
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&second) else {
        unreachable!()
    };
    assert!(!runtime.diagnostic_navigation_available());
    assert_eq!(
        app.lsp_contexts[&first].attachment.context.diagnostics,
        vec![diagnostic("first diagnostic")]
    );
}

#[test]
fn diagnostics_for_an_unregistered_document_cannot_install_answers() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    let doc = LspDocumentId(1);
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "first.rs", "first\n");
    runtime.set_lsp_document(Some(doc));
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    // Deliberately retain the old test's incomplete App-only registration.
    // It is not authority to publish when the manager has no document.
    lsp_state(&mut app, card).attachment.document = Some(doc);
    app.lsp_documents.insert(doc, card);
    assert_eq!(app.lsp.document_version(doc), None);
    drop(app.update(Message::Lsp(LspEvent::Diagnostics {
        doc,
        version: 1,
        items: vec![diagnostic("unregistered diagnostic")],
    })));
    assert!(
        app.lsp_contexts[&card]
            .attachment
            .context
            .diagnostics
            .is_empty()
    );
    assert_eq!(app.lsp_contexts[&card].attachment.diagnostics_version, None);
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert!(!runtime.diagnostic_navigation_available());
}

/// An event for a document nobody holds any more has no surface to land on.
#[test]
fn an_event_for_a_closed_document_is_dropped() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    lsp_state(&mut app, card).attachment.document = Some(LspDocumentId(1));
    app.lsp_documents.insert(LspDocumentId(1), card);

    app.apply_lsp_event(LspEvent::Diagnostics {
        doc: LspDocumentId(9),
        version: 1,
        items: vec![diagnostic("undefined name")],
    });

    assert!(
        app.lsp_contexts[&card]
            .attachment
            .context
            .diagnostics
            .is_empty()
    );
}

/// A desync means the two sides disagreed about the text every answer was
/// computed against. The buffer survives; the answers cannot.
#[test]
fn a_desync_drops_the_answers_and_keeps_the_diagnostics() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "tail\n");
    runtime.set_lsp_document(Some(LspDocumentId(1)));
    runtime.set_editor_completions(vec![completion_item("alpha")]);
    let target = runtime.editor_completion_targets(card).remove(0);
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    app.lsp_documents.insert(LspDocumentId(1), card);
    let state = lsp_state(&mut app, card);
    state.attachment.document = Some(LspDocumentId(1));
    state.attachment.context.completions = vec![target.clone()];
    state.attachment.context.locations = Some((crate::lsp::LocationKind::Definition, Vec::new()));
    state.attachment.context.diagnostics = vec![diagnostic("undefined name")];

    app.apply_lsp_event(LspEvent::DocumentDesynced {
        doc: LspDocumentId(1),
        version: 12,
        error: LspError::NoServer,
    });

    let state = &app.lsp_contexts[&card];
    assert!(state.attachment.context.completions.is_empty());
    assert!(state.attachment.context.locations.is_none());
    // Diagnostics are the server's own standing answer, replaced on its
    // schedule, and are refused by version at the point of use instead.
    assert_eq!(state.attachment.context.diagnostics.len(), 1);
    assert!(state.attachment.context.notice.is_some());
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert!(runtime.editor_completion_targets(card).is_empty());
    drop(app.update_editor_lsp(
        card,
        repository_widget::EditorLspAction::Choose { index: 0, target },
    ));
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert!(runtime.editor_completion_targets(card).is_empty());
    assert_eq!(runtime.editor_text().as_deref(), Some("tail\n"));
}

#[test]
fn dismiss_and_restart_retire_native_completion_answers_with_the_inspector() {
    for restart in [false, true] {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "tail\n");
        runtime.set_lsp_document(Some(LspDocumentId(1)));
        runtime.set_editor_completions(vec![completion_item("alpha")]);
        runtime.set_editor_diagnostics(vec![diagnostic("standing")]);
        let target = runtime.editor_completion_targets(card).remove(0);
        assert!(runtime.diagnostic_navigation_available());
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        let state = lsp_state(&mut app, card);
        state.attachment.document = Some(LspDocumentId(1));
        state.attachment.context.completions = vec![target.clone()];
        state.attachment.context.diagnostics = vec![diagnostic("standing")];
        let action = if restart {
            InspectorAction::RestartLanguageServer(card)
        } else {
            InspectorAction::DismissLspResults(card)
        };
        drop(app.update_lsp(action));
        assert!(
            app.lsp_contexts[&card]
                .attachment
                .context
                .completions
                .is_empty()
        );
        assert_eq!(
            app.lsp_contexts[&card]
                .attachment
                .context
                .diagnostics
                .is_empty(),
            restart
        );
        drop(app.update_editor_lsp(
            card,
            repository_widget::EditorLspAction::Choose { index: 0, target },
        ));
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        assert!(runtime.editor_completion_targets(card).is_empty());
        assert_eq!(runtime.editor_text().as_deref(), Some("tail\n"));
        assert_eq!(runtime.diagnostic_navigation_available(), !restart);
    }
}

/// A superseded request is the ordinary result of typing while one is in
/// flight. Reporting it would put a notice on screen for every keystroke.
#[test]
fn a_cancelled_request_is_not_reported() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    lsp_state(&mut app, card).attachment.document = Some(LspDocumentId(1));
    app.lsp_documents.insert(LspDocumentId(1), card);

    app.apply_lsp_event(LspEvent::RequestFailed {
        request: crate::lsp::LspRequestId(1),
        doc: LspDocumentId(1),
        error: LspError::Cancelled,
    });
    assert!(app.lsp_contexts[&card].attachment.context.notice.is_none());

    app.apply_lsp_event(LspEvent::RequestFailed {
        request: crate::lsp::LspRequestId(2),
        doc: LspDocumentId(1),
        error: LspError::NoServer,
    });
    assert!(app.lsp_contexts[&card].attachment.context.notice.is_some());
}

#[tokio::test]
async fn a_queued_ready_cannot_replace_starting_after_restart() {
    let (manager, publisher) = crate::lsp::tests::progress_fixture().await;
    let mut app = lsp_test_app();
    app.lsp = manager;
    let card = repository_card(&mut app, "/repo");
    let doc = publisher.doc;
    lsp_state(&mut app, card).attachment.document = Some(doc);
    app.lsp_documents.insert(doc, card);
    app.lsp.restart(doc);
    lsp_state(&mut app, card).attachment.context.status = Some(LspStatus::Starting);
    app.apply_lsp_event(publisher.ready.clone());
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.status,
        Some(LspStatus::Starting)
    );
    let events = app.lsp.events();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            &event,
            LspEvent::Status {
                status: LspStatus::Ready,
                ..
            }
        ) {
            app.apply_lsp_event(event);
            break;
        }
    }
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.status,
        Some(LspStatus::Ready)
    );
    app.lsp.begin_shutdown().await;
}

#[test]
fn a_current_unbacked_refusal_keeps_its_reason_and_a_closed_one_is_ignored() {
    let mut app = lsp_test_app();
    app.lsp = LspManager::new(AppSettings::default().language_servers);
    let card = repository_card(&mut app, "/repo");
    let doc = LspDocumentId(1);
    lsp_state(&mut app, card).attachment.document = Some(doc);
    app.lsp_documents.insert(doc, card);
    let language = app.lsp.language_for(Path::new("/repo/a.rs")).unwrap();
    // This real admission refusal has no backing session or snapshot.
    let status = app.lsp.open(
        doc,
        Path::new("/repo/a.rs"),
        Path::new("/repo"),
        language,
        1,
        "",
    );
    assert!(matches!(status, LspStatus::Unavailable { .. }));
    assert_eq!(app.lsp.status(doc), LspStatus::Disabled);
    let event = app.lsp.events().try_recv().unwrap();
    app.apply_lsp_event(event.clone());
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.status,
        Some(status)
    );
    app.lsp.close(doc);
    lsp_state(&mut app, card).attachment.context.status = Some(LspStatus::Starting);
    app.apply_lsp_event(event);
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.status,
        Some(LspStatus::Starting)
    );
}

/// Progress occupies the notice slot until its producer marks it done.
#[tokio::test]
async fn progress_occupies_the_notice_slot_until_it_is_done() {
    let (manager, publisher) = crate::lsp::tests::progress_fixture().await;
    let mut app = lsp_test_app();
    app.lsp = manager;
    let card = repository_card(&mut app, "/repo");
    lsp_state(&mut app, card).attachment.document = Some(LspDocumentId(1));
    app.lsp_documents.insert(LspDocumentId(1), card);

    app.apply_lsp_event(
        publisher
            .publish(crate::lsp::LspProgress {
                token: "indexing".into(),
                title: Some("Indexing".into()),
                message: None,
                percentage: Some(40),
                done: false,
            })
            .await,
    );
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.notice.as_deref(),
        Some("Indexing (40%)")
    );

    app.apply_lsp_event(
        publisher
            .publish(crate::lsp::LspProgress {
                token: "indexing".into(),
                title: Some("Indexing".into()),
                message: None,
                percentage: Some(100),
                done: true,
            })
            .await,
    );
    assert!(app.lsp_contexts[&card].attachment.context.notice.is_none());
    app.lsp.begin_shutdown().await;
}

/// A location arrives absolute and the card's file list is
/// worktree-relative. A target the worktree does not contain has no row to
/// open, and must not be guessed at.
#[test]
fn a_jump_outside_the_worktree_opens_nothing() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");

    let _ = app.update_lsp(InspectorAction::JumpToLspLocation {
        card,
        path: PathBuf::from("/elsewhere/other.rs"),
        at: LspPosition::new(1, 0),
        coordinates: LspCoordinates::Scalar,
    });

    assert_eq!(
        notice(&app),
        Some("that definition is outside this worktree")
    );
    assert!(!app.lsp_pending_jumps.contains_key(&card));
}

/// A jump into a file the card is not showing parks until that file is the
/// open buffer: a column is only placeable against the text it indexes.
#[test]
fn a_jump_into_another_file_parks_until_it_is_open() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");

    let task = app.update_lsp(InspectorAction::JumpToLspLocation {
        card,
        path: PathBuf::from("/repo/src/other.rs"),
        at: LspPosition::new(4, 2),
        coordinates: LspCoordinates::Utf16,
    });

    // Parking without asking the card to open the file is a jump that never
    // lands: that open is the only thing that releases it, and the file
    // list it addresses is keyed on the worktree-relative path.
    assert!(matches!(
        task_app_message(task),
        Message::Repository(RepositoryAction::OpenFile { card: at, ref path })
            if at == card && path == Path::new("src/other.rs")
    ));
    let parked = app
        .lsp_pending_jumps
        .get(&card)
        .expect("the jump is parked on the open");
    // Parked absolute, because that is the side the *next* open reports and
    // the only side the two can be compared on.
    assert_eq!(parked.path, PathBuf::from("/repo/src/other.rs"));
    assert_eq!(parked.at, LspPosition::new(4, 2));
    assert_eq!(parked.coordinates, LspCoordinates::Utf16);
    assert_eq!(notice(&app), None);
}

#[test]
fn a_confirmed_git_retry_resolves_its_failed_commit_notice() {
    let mut app = context_inspector_test_app(false);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let key = app.sidebar_state.active_worktree().unwrap().clone();
    app.active_workspace = Some(key.canonical_path.clone());
    let notice_key = NoticeKey::GitCommit(key.clone());
    app.notices
        .record(TransientNotice::warning("Unrelated report"));
    let first = app
        .begin_mutation(&key, MutationKind::Commit, None)
        .unwrap();
    let (report, committed) = commit_report(Ok(CommitOutcome::NotCreated {
        failure: crate::repository::MutationFailure::Rejected {
            reason: "fixture pre-commit refusal".into(),
        },
    }));
    assert!(report.is_some() && !committed);
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: first,
        report,
        committed,
    }));
    assert!(app.notices.has(notice_key.clone()));
    assert!(
        app.sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .error
            .is_some()
    );
    let original_notice = app.notices.recent().next().unwrap().notice.clone();
    let retry = app
        .begin_mutation(&key, MutationKind::Commit, None)
        .unwrap();
    assert!(
        app.notices.has(notice_key.clone()),
        "admission is not success"
    );
    assert_eq!(app.notices.recent().next().unwrap().notice, original_notice);
    let (report, committed) = commit_report(Ok(CommitOutcome::Created {
        oid: crate::repository::Oid::parse_object("a".repeat(40)).unwrap(),
        warnings: Vec::new(),
    }));
    assert!(report.is_none() && committed);
    drop(app.update(Message::SourceControlMutated {
        key,
        operation: retry,
        report,
        committed,
    }));
    assert!(!app.notices.has(notice_key));
    assert!(
        app.sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .error
            .is_none()
    );
    assert_eq!(app.notices.len(), 1);
    assert_eq!(
        app.notices.recent().next().unwrap().notice.message,
        "Unrelated report"
    );
}

#[test]
fn git_notice_resolution_keeps_other_worktrees_and_mutation_kinds() {
    let mut app = context_inspector_test_app(false);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let key = app.sidebar_state.active_worktree().unwrap().clone();
    let sibling = app
        .sidebar_model
        .worktree_at(Path::new("/repo/fix-sidebar"))
        .unwrap();
    let primary_notice = NoticeKey::GitCommit(key.clone());
    let sibling_notice = NoticeKey::GitCommit(sibling.clone());
    for target in [&key, &sibling] {
        app.active_workspace = Some(target.canonical_path.clone());
        drop(app.rebind_source_control());
        let operation = app
            .begin_mutation(target, MutationKind::Commit, None)
            .unwrap();
        let (report, committed) = commit_report(Ok(CommitOutcome::NotCreated {
            failure: crate::repository::MutationFailure::Rejected {
                reason: "fixture refusal".into(),
            },
        }));
        drop(app.update(Message::SourceControlMutated {
            key: target.clone(),
            operation,
            report,
            committed,
        }));
    }
    assert!(app.notices.has(primary_notice.clone()));
    assert!(app.notices.has(sibling_notice.clone()));
    app.active_workspace = Some(key.canonical_path.clone());
    drop(app.rebind_source_control());
    let operation = app.begin_mutation(&key, MutationKind::Stage, None).unwrap();
    let report = mutation_report("Stage", Ok(MutationOutcome::Applied { paths: Vec::new() }));
    assert!(report.is_none());
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation,
        report,
        committed: false,
    }));
    assert!(
        app.notices.has(primary_notice.clone()),
        "Stage cannot resolve Commit"
    );
    assert!(app.notices.has(sibling_notice.clone()));
    app.active_workspace = Some(sibling.canonical_path.clone());
    drop(app.rebind_source_control());
    let operation = app
        .begin_mutation(&sibling, MutationKind::Commit, None)
        .unwrap();
    let (report, committed) = commit_report(Ok(CommitOutcome::Created {
        oid: crate::repository::Oid::parse_object("b".repeat(40)).unwrap(),
        warnings: Vec::new(),
    }));
    drop(app.update(Message::SourceControlMutated {
        key: sibling,
        operation,
        report,
        committed,
    }));
    assert!(!app.notices.has(sibling_notice));
    assert!(app.notices.has(primary_notice));
}

#[test]
fn a_stale_git_completion_cannot_clear_or_replace_a_newer_owners_notice() {
    let mut app = context_inspector_test_app(false);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let key = app.sidebar_state.active_worktree().unwrap().clone();
    app.active_workspace = Some(key.canonical_path.clone());
    let notice_key = NoticeKey::GitCommit(key.clone());
    let first = app
        .begin_mutation(&key, MutationKind::Commit, None)
        .unwrap();
    let (report, committed) = commit_report(Ok(CommitOutcome::NotCreated {
        failure: crate::repository::MutationFailure::Rejected {
            reason: "first refusal".into(),
        },
    }));
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: first,
        report,
        committed,
    }));
    let first_notice = app.notices.recent().next().unwrap().clone();
    let retry = app
        .begin_mutation(&key, MutationKind::Commit, None)
        .unwrap();
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: first,
        report: None,
        committed: true,
    }));
    assert_eq!(app.source_control_mutations[&key.canonical_path].id, retry);
    assert_eq!(app.notices.recent().next().unwrap(), &first_notice);
    let (report, committed) = commit_report(Ok(CommitOutcome::NotCreated {
        failure: crate::repository::MutationFailure::Rejected {
            reason: "newer refusal".into(),
        },
    }));
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: retry,
        report,
        committed,
    }));
    let replacement = app.notices.recent().next().unwrap().clone();
    assert_ne!(replacement.id, first_notice.id);
    assert_eq!(app.notices.len(), 1, "same-key retry replaces its report");
    let newest = app
        .begin_mutation(&key, MutationKind::Commit, None)
        .unwrap();
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: retry,
        report: Some("stale other failure".into()),
        committed: false,
    }));
    drop(app.update(Message::DismissNotice(first_notice.id)));
    assert_eq!(app.source_control_mutations[&key.canonical_path].id, newest);
    assert_eq!(app.notices.recent().next().unwrap(), &replacement);
    assert!(app.notices.has(notice_key.clone()));
    let (report, committed) = commit_report(Ok(CommitOutcome::Created {
        oid: crate::repository::Oid::parse_object("c".repeat(40)).unwrap(),
        warnings: Vec::new(),
    }));
    drop(app.update(Message::SourceControlMutated {
        key,
        operation: newest,
        report,
        committed,
    }));
    assert!(!app.notices.has(notice_key));
}

#[test]
fn successful_stage_of_another_selection_keeps_unkeyed_failure_partial_and_unknown_notices() {
    use crate::repository::{MutationFailure, PathSelection, RepoPath};

    let path_a = RepoPath::parse("src/a.rs").unwrap();
    let path_b = RepoPath::parse("src/b.rs").unwrap();
    let path_c = RepoPath::parse("src/c.rs").unwrap();
    assert_ne!(path_a, path_b);
    for outcome in [
        MutationOutcome::NotApplied {
            failure: MutationFailure::Rejected {
                reason: "A was refused before execution".into(),
            },
        },
        MutationOutcome::PartiallyApplied {
            paths: vec![path_a.clone()],
            failures: vec![MutationFailure::Rejected {
                reason: "another selected path refused".into(),
            }],
        },
        MutationOutcome::OutcomeUnknown {
            failure: MutationFailure::Rejected {
                reason: "A has no completion proof".into(),
            },
        },
    ] {
        let mut app = context_inspector_test_app(false);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        let key = app.sidebar_state.active_worktree().unwrap().clone();
        app.active_workspace = Some(key.canonical_path.clone());

        // Use the public reducer's selected-path action, not a synthetic
        // begin_mutation which could conceal loss of the original subject.
        // Worker tasks are not polled: these are callback custody oracles,
        // not actual Git or filesystem qualification.
        let selected_a = if matches!(&outcome, MutationOutcome::PartiallyApplied { .. }) {
            vec![path_a.clone(), path_c.clone()]
        } else {
            vec![path_a.clone()]
        };
        drop(
            app.update(Message::Sidebar(SidebarAction::Stage {
                key: key.clone(),
                selection: selected_a
                    .into_iter()
                    .map(|path| PathSelection {
                        path,
                        old_path: None,
                    })
                    .collect(),
            })),
        );
        let first = app.source_control_mutations[&key.canonical_path];
        assert_eq!(first.kind, MutationKind::Stage);
        let report = mutation_report("Stage", Ok(outcome));
        assert!(report.is_some());
        drop(app.update(Message::SourceControlMutated {
            key: key.clone(),
            operation: first.id,
            report,
            committed: false,
        }));
        assert_eq!(app.notices.len(), 1);
        let original = app.notices.recent().next().unwrap().clone();
        assert_eq!(original.notice.key, None);
        assert_eq!(original.expires_at, None);

        drop(app.update(Message::Sidebar(SidebarAction::Stage {
            key: key.clone(),
            selection: vec![PathSelection {
                path: path_b.clone(),
                old_path: None,
            }],
        })));
        let second = app.source_control_mutations[&key.canonical_path];
        assert_eq!(second.kind, MutationKind::Stage);
        assert_ne!(second.id, first.id);
        let report = mutation_report(
            "Stage",
            Ok(MutationOutcome::Applied {
                paths: vec![path_b.clone()],
            }),
        );
        assert!(report.is_none());
        drop(app.update(Message::SourceControlMutated {
            key: key.clone(),
            operation: second.id,
            report,
            committed: false,
        }));
        assert!(
            !app.source_control_mutations
                .contains_key(&key.canonical_path)
        );
        assert!(
            app.sidebar_model
                .source_control
                .as_ref()
                .unwrap()
                .error
                .is_none()
        );
        assert_eq!(app.notices.len(), 1);
        assert_eq!(
            app.notices.recent().next().unwrap(),
            &original,
            "Stage(B) success does not resolve Stage(A)'s failure or uncertainty"
        );
    }
}

#[test]
fn git_mutation_ownership_survives_navigation_and_fences_old_completions() {
    let mut app = context_inspector_test_app(false);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let key = app.sidebar_state.active_worktree().unwrap().clone();
    let sibling = app
        .sidebar_model
        .worktree_at(Path::new("/repo/fix-sidebar"))
        .unwrap();
    app.active_workspace = Some(key.canonical_path.clone());
    let first = app.begin_mutation(&key, MutationKind::Stage, None).unwrap();
    app.active_workspace = Some(sibling.canonical_path.clone());
    drop(app.rebind_source_control());
    assert!(!app.sidebar_model.source_control.as_ref().unwrap().busy());
    let other = app
        .begin_mutation(&sibling, MutationKind::Push, None)
        .unwrap();
    app.active_workspace = Some(key.canonical_path.clone());
    drop(app.rebind_source_control());
    assert!(app.sidebar_model.source_control.as_ref().unwrap().busy());
    assert!(
        app.begin_mutation(&key, MutationKind::Commit, None)
            .is_none()
    );
    drop(app.update(Message::SourceControlMutated {
        key: sibling,
        operation: other,
        report: None,
        committed: false,
    }));
    assert!(
        app.sidebar_model.source_control.as_ref().unwrap().busy(),
        "another worktree's completion cannot release this command"
    );
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: first,
        report: None,
        committed: false,
    }));
    assert!(!app.sidebar_model.source_control.as_ref().unwrap().busy());
    let newer = app
        .begin_mutation(&key, MutationKind::Unstage, None)
        .unwrap();
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: first,
        report: Some("stale failure".into()),
        committed: true,
    }));
    assert!(app.sidebar_model.source_control.as_ref().unwrap().busy());
    assert_eq!(app.source_control_mutations[&key.canonical_path].id, newer);
    assert!(
        app.sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .error
            .is_none()
    );
}

#[test]
fn git_commit_completion_preserves_later_edits_and_settles_parked_drafts() {
    let mut app = context_inspector_test_app(false);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let key = app.sidebar_state.active_worktree().unwrap().clone();
    let sibling = app
        .sidebar_model
        .worktree_at(Path::new("/repo/fix-sidebar"))
        .unwrap();
    app.active_workspace = Some(key.canonical_path.clone());
    drop(
        app.update(Message::Sidebar(SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "Submitted subject\n\nSubmitted body".into(),
        })),
    );
    let revision = app
        .sidebar_state
        .submitted_commit_revision(&key, app.sidebar_state.commit_message(), false)
        .unwrap();
    let first = app
        .begin_mutation(&key, MutationKind::Commit, Some(revision))
        .unwrap();
    drop(
        app.update(Message::Sidebar(SidebarAction::CommitEditorAction {
            key: key.clone(),
            action: iced::widget::text_editor::Action::Edit(
                iced::widget::text_editor::Edit::Paste(Arc::new("Later edit ".into())),
            ),
        })),
    );
    let later = app.sidebar_state.commit_message().to_owned();
    app.active_workspace = Some(sibling.canonical_path.clone());
    drop(app.rebind_source_control());
    drop(
        app.update(Message::Sidebar(SidebarAction::CommitMessageChanged {
            key: sibling.clone(),
            message: "Sibling draft".into(),
        })),
    );
    drop(
        app.update(Message::Sidebar(SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "Departed widget".into(),
        })),
    );
    assert_eq!(app.sidebar_state.commit_message(), "Sibling draft");
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: first,
        report: None,
        committed: true,
    }));
    app.active_workspace = Some(key.canonical_path.clone());
    drop(app.rebind_source_control());
    assert_eq!(app.sidebar_state.commit_message(), later);
    let revision = app
        .sidebar_state
        .submitted_commit_revision(&key, &later, false)
        .unwrap();
    let second = app
        .begin_mutation(&key, MutationKind::Commit, Some(revision))
        .unwrap();
    app.active_workspace = Some(sibling.canonical_path.clone());
    drop(app.rebind_source_control());
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: second,
        report: None,
        committed: true,
    }));
    assert_eq!(app.sidebar_state.commit_message(), "Sibling draft");
    app.active_workspace = Some(key.canonical_path.clone());
    drop(app.rebind_source_control());
    assert!(
        app.sidebar_state.commit_message().is_empty(),
        "an unchanged submitted draft clears even when success arrives elsewhere"
    );
}

#[test]
fn files_result_changes_reset_scroll_and_reject_departed_worktrees() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    let mut app = context_inspector_test_app(false);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let key = app.sidebar_state.active_worktree().unwrap().clone();
    let actions = [
        SidebarAction::FilesPage {
            key: key.clone(),
            page: 1,
        },
        SidebarAction::ToggleFilesFilter { key: key.clone() },
        SidebarAction::FilesQueryChanged {
            key: key.clone(),
            query: "file-0".into(),
        },
        SidebarAction::FilesQueryChanged {
            key: key.clone(),
            query: String::new(),
        },
        SidebarAction::FilesQueryChanged {
            key: key.clone(),
            query: "file-1".into(),
        },
        SidebarAction::ToggleFilesFilter { key: key.clone() },
    ];
    for action in actions {
        let task = app.update(Message::Sidebar(action));
        let renderer = headless_renderer();
        let mut element: Element<'_, Message> =
            widget::scrollable(widget::Space::new().height(1000))
                .id(sidebar::files_scroll_id(&key))
                .height(200)
                .into();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(300.0, 200.0)),
        );
        let mut scroll = operation::scrollable::scroll_to(
            sidebar::files_scroll_id(&key),
            operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(500.0),
            },
        );
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut scroll);
        #[derive(Default)]
        struct Offset(f32);
        impl Operation for Offset {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                _: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                self.0 = translation.y;
            }
        }
        let mut before = Offset::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
        assert_eq!(before.0, 500.0);
        let mut stream =
            iced::runtime::task::into_stream(task).expect("result change issues widget operation");
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        while let Some(action) = executor.block_on(stream.next()) {
            if let iced::runtime::Action::Widget(mut operation) = action {
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation,
                );
            }
        }
        let mut after = Offset::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
        assert_eq!(after.0, 0.0, "new results start at their first row");
    }
    assert_eq!(app.sidebar_state.files_query(), "");
    assert!(!app.sidebar_state.files_filter_open());
    let unchanged_sidebar = app.sidebar_state.clone();
    let departed = ActiveWorktreeKey {
        worktree_id: WorktreeId::new(),
        ..key
    };
    for action in [
        SidebarAction::FilesPage {
            key: departed.clone(),
            page: 9,
        },
        SidebarAction::FilesQueryChanged {
            key: departed.clone(),
            query: "stale".into(),
        },
        SidebarAction::ToggleFilesFilter { key: departed },
    ] {
        // The public dispatch postlude may wrap an empty result in a
        // batch. Refused stale input must emit no effects, regardless of
        // whether that batch has a stream allocation.
        if let Some(mut effects) =
            iced::runtime::task::into_stream(app.update(Message::Sidebar(action)))
        {
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert!(executor.block_on(effects.next()).is_none());
        }
        assert_eq!(app.sidebar_state.files_query(), "");
        assert!(!app.sidebar_state.files_filter_open());
        assert_eq!(app.sidebar_state, unchanged_sidebar);
    }
}

/// Who holds the keyboard once a task's widget operations have run.
///
/// Focus arrives as a widget operation rather than a message, so it is
/// read the way the runtime reads one: run the task's operations over the
/// tree the frame actually built, then ask that tree who holds focus.
pub(super) fn focused(app: &Cagents, task: Task<Message>) -> Option<widget::Id> {
    focused_element(cosmic::Application::view(app), task)
}

pub(super) fn focused_element(
    mut element: Element<'_, Message>,
    task: Task<Message>,
) -> Option<widget::Id> {
    use cosmic::iced::advanced::layout;
    use cosmic::iced::advanced::widget::Tree;
    use cosmic::iced::advanced::widget::{Operation, operation};
    use cosmic::iced::{Size, advanced::Layout};

    let renderer = headless_renderer();
    let mut tree = Tree::new(element.as_widget_mut());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
    );

    // An empty task is not an error here: it is precisely the answer a
    // jump that forgot to focus gives, and the assertion that reads this
    // says so far better than an unwrap would.
    if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("executor");
        while let Some(action) = executor.block_on(stream.next()) {
            let cosmic::iced::runtime::Action::Widget(mut operation) = action else {
                continue;
            };
            loop {
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation,
                );
                match operation.finish() {
                    operation::Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        }
    }

    // `find_focused` reports an `Id`; `operate` takes an operation over
    // `()`, so the answer comes back through a cell.
    let found: Arc<std::sync::Mutex<Option<widget::Id>>> = Default::default();
    let sink = Arc::clone(&found);
    let mut probe = operation::map(operation::focusable::find_focused(), move |id| {
        *sink.lock().expect("the probe is not shared across a panic") = Some(id);
    });
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
    let _ = probe.finish();

    found.lock().expect("the probe survived").clone()
}

/// A jump that moves the caret must also hand the editor the keyboard.
///
/// An unfocused `text_editor` draws no caret at all, so without this the
/// application answers a click on a result with a frame indistinguishable
/// from the one the user clicked in - measured live before this was wired:
/// frame differencing over the caret's blink found a 2px column with the
/// editor focused and zero pixel delta after a jump.
#[test]
fn a_jump_gives_the_editor_the_keyboard() {
    let (mut app, card) = editing_repository_app();

    // Into the buffer that is already open, which is the one jump that
    // needs no running server to place its caret.
    lsp_state(&mut app, card).path = Some(PathBuf::from("/repo/notes.txt"));
    let task = app.update_lsp(InspectorAction::JumpToLspLocation {
        card,
        path: PathBuf::from("/repo/notes.txt"),
        at: LspPosition::new(0, 0),
        coordinates: LspCoordinates::Scalar,
    });
    assert_eq!(
        focused(&app, task),
        Some(repository_widget::editor_focus_id(card)),
        "the jump left the editor unfocused, so its caret is invisible"
    );
}

#[test]
fn pane_navigation_past_an_end_returns_focus_from_its_tab() {
    let mut app = context_inspector_test_app(false);
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    app.workspace.resize_viewport(1200.0);
    let pane = app.workspace.focused().unwrap();
    let title = app.card_header_widget_id(pane);
    assert_eq!(
        focused(
            &app,
            iced::widget::operation::focus(title.clone()).map(cosmic::Action::App)
        ),
        Some(title.clone()),
        "the tab must be a real focusable control outside the pane body",
    );
    for direction in [
        crate::workspace::FocusDirection::Left,
        crate::workspace::FocusDirection::Right,
    ] {
        let navigation = app.update(Message::FocusAdjacent(direction));
        let task = iced::widget::operation::focus(title.clone())
            .map(cosmic::Action::App)
            .chain(navigation);
        assert_eq!(
            focused(&app, task),
            Some(crate::settings_widgets::search_id())
        );
        assert_eq!(app.workspace.focused(), Some(pane));
    }
}

#[test]
fn restored_editor_startup_focus_follows_the_loaded_surface() {
    let (mut app, pane) = editing_repository_app();
    let update = app.workspace.resize_viewport(1200.0);
    assert_eq!(
        app.card_keyboard_focus(pane),
        repository_widget::editor_focus_id(pane)
    );
    let startup = app.apply_viewport(update, false, DurableChange::None);
    assert_eq!(
        focused(&app, startup),
        Some(repository_widget::editor_focus_id(pane))
    );
    let _ = app.update(Message::Repository(RepositoryAction::OpenFind {
        card: pane,
    }));
    assert_eq!(
        app.card_keyboard_focus(pane),
        repository_widget::find_focus_id(pane)
    );
    let _ = app.update(Message::Repository(RepositoryAction::ApplyMarkdownMode {
        card: pane,
        preview: true,
    }));
    assert_eq!(
        app.card_keyboard_focus(pane),
        app.card_header_widget_id(pane)
    );
}

#[test]
fn fresh_editor_load_returns_keyboard_without_saved_navigation() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "fresh source\n").unwrap();
    let (mut app, pane) = editing_repository_app();
    let view = RepositoryView {
        project: ProjectId::new(),
        repository: directory.path().into(),
        worktree: directory.path().into(),
        selected_file: None,
    };
    Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|card| card.id == pane)
        .unwrap()
        .kind = CardKind::Repository(view.clone());
    let mut runtime = RepositoryRuntime::new_directory(view);
    // This is the public action emitted after sidebar navigation resolves
    // its file list, without saved caret or viewport metadata.
    let load = runtime.update(RepositoryAction::OpenFile {
        card: pane,
        path: "notes.txt".into(),
    });
    app.cards
        .insert(pane, CardRuntime::Repository(Box::new(runtime)));
    app.window_width = 1900.0;
    let update = app.workspace.resize_viewport(1200.0);
    drop(app.apply_viewport(update, false, DurableChange::None));
    assert!(app.card_holds_keyboard(pane));
    let placeholder = app.card_header_widget_id(pane);
    let (completed_read, tasks) = settle_repository_read(&mut app, pane, load);
    assert!(
        completed_read,
        "the actual file worker must answer the open"
    );
    let initial = iced::widget::operation::focus(placeholder).map(cosmic::Action::App);
    assert_eq!(
        focused(&app, initial.chain(Task::batch(tasks))),
        Some(repository_widget::editor_focus_id(pane)),
        "a freshly opened editor must receive typing without a body click"
    );
}

#[test]
fn restored_editor_focus_waits_for_its_file_in_a_narrow_window() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "saved\n").unwrap();
    let (mut app, pane) = editing_repository_app();
    let view = RepositoryView {
        project: ProjectId::new(),
        repository: directory.path().into(),
        worktree: directory.path().into(),
        selected_file: None,
    };
    Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|card| card.id == pane)
        .unwrap()
        .kind = CardKind::Repository(view.clone());
    let mut runtime = RepositoryRuntime::new_directory(view.clone());
    runtime.restore_navigation(&crate::model::RepositoryNavigation {
        project: view.project,
        worktree: view.worktree,
        selected_file: Some("notes.txt".into()),
        editor: Some(crate::model::EditorNavigation {
            caret: crate::model::EditorPosition { line: 0, column: 0 },
            selection: None,
            scroll_line: 0,
            scroll_vertical: 0.0,
            scroll_horizontal: 0.0,
        }),
    });
    let load = runtime.initial_load(pane);
    app.cards
        .insert(pane, CardRuntime::Repository(Box::new(runtime)));
    let update = app.workspace.resize_viewport(1200.0);
    let initial = app.apply_viewport(update, false, DurableChange::None);
    assert_eq!(
        focused(&app, initial),
        Some(app.card_header_widget_id(pane))
    );
    // A narrow window still shows the pane, whose inspector keeps to the
    // pane, so the editor takes the keyboard as soon as its file loads.
    app.window_width = 900.0;
    let update = app.workspace.resize_viewport(500.0);
    drop(app.apply_viewport(update, false, DurableChange::None));
    let (completed_read, loaded) = settle_repository_read(&mut app, pane, load);
    assert!(
        completed_read,
        "the actual file worker must answer the load"
    );
    assert_eq!(
        focused(&app, Task::batch(loaded)),
        Some(repository_widget::editor_focus_id(pane))
    );
}

#[test]
fn restored_editor_focus_preserves_controls_chosen_during_loading() {
    let (mut app, pane) = editing_repository_app();
    app.workspace.resize_viewport(1200.0);
    let _ = app.workspace.take_widget_focus_request();
    let _ = app.update(Message::Repository(RepositoryAction::OpenFind {
        card: pane,
    }));
    let editor = repository_widget::editor_focus_id(pane);
    let placeholder = app.card_header_widget_id(pane);
    for current in [
        None,
        Some(placeholder.clone()),
        Some(editor.clone()),
        Some(repository_widget::find_focus_id(pane)),
    ] {
        let initial = current.as_ref().map_or_else(Task::none, |id| {
            iced::widget::operation::focus(id.clone()).map(cosmic::Action::App)
        });
        let task = initial.chain(
            workspace_widget::focus_after_loading(placeholder.clone(), editor.clone())
                .map(cosmic::Action::App),
        );
        let expected = current
            .filter(|id| *id != placeholder)
            .unwrap_or_else(|| editor.clone());
        assert_eq!(focused(&app, task), Some(expected));
    }
}

#[test]
fn pane_navigation_past_an_end_preserves_editor_and_find_focus() {
    let (mut app, pane) = editing_repository_app();
    app.workspace.resize_viewport(1200.0);
    // Startup's default focus has already run before a user moves into
    // the editor or its find bar. Do not replay that fixture-only request.
    let _ = app.workspace.take_widget_focus_request();
    let _ = app.update(Message::Repository(RepositoryAction::OpenFind {
        card: pane,
    }));
    for target in [
        repository_widget::editor_focus_id(pane),
        repository_widget::find_focus_id(pane),
    ] {
        for direction in [
            crate::workspace::FocusDirection::Left,
            crate::workspace::FocusDirection::Right,
        ] {
            let navigation = app.update(Message::FocusAdjacent(direction));
            let task = iced::widget::operation::focus(target.clone())
                .map(cosmic::Action::App)
                .chain(navigation);
            assert_eq!(focused(&app, task), Some(target.clone()));
            assert_eq!(app.workspace.focused(), Some(pane));
        }
    }
}

#[test]
fn an_unmounted_pane_focus_query_does_not_blur_its_tab() {
    let mut app = context_inspector_test_app(false);
    let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
    app.workspace.resize_viewport(1200.0);
    let pane = app.workspace.focused().unwrap();
    let title = app.card_header_widget_id(pane);
    let task = iced::widget::operation::focus(title.clone())
        .map(cosmic::Action::App)
        .chain(
            workspace_widget::focus_card_if_outside(
                workspace_widget::card_widget_id(CardId::new()),
                app.card_keyboard_focus(pane),
            )
            .map(cosmic::Action::App),
        );
    assert_eq!(focused(&app, task), Some(title));
}

/// The same, for the jump that has to open a file first.
///
/// Go to definition and Find references usually land in a *different*
/// file, so the caret cannot be placed when the answer arrives: the jump
/// is parked, the file is opened, and the open answers it. That second
/// path reaches the caret through `flush_pending_lsp_jump` rather than
/// directly, and has to hand over the keyboard just the same - a caret
/// placed in a buffer nobody focused is a caret nobody can see.
#[test]
fn a_jump_that_opens_a_file_gives_the_editor_the_keyboard_too() {
    let (mut app, card) = editing_repository_app();

    // Parked the way `jump_to_lsp_location` parks one, on the path the
    // card is about to open.
    let path = PathBuf::from("/repo/notes.txt");
    lsp_state(&mut app, card).path = Some(path.clone());
    app.lsp_pending_jumps.insert(
        card,
        PendingLspJump {
            path: path.clone(),
            at: LspPosition::new(0, 0),
            coordinates: LspCoordinates::Scalar,
        },
    );

    let task = app.flush_pending_lsp_jump(card, &path);

    assert!(
        !app.lsp_pending_jumps.contains_key(&card),
        "the open answered the jump, so it must not stay parked"
    );
    assert_eq!(
        focused(&app, task),
        Some(repository_widget::editor_focus_id(card)),
        "the jump placed its caret in a buffer it never focused, so the caret is invisible"
    );
}

/// A definition can resolve into a file the card has no row for - a
/// generated or ignored source inside the worktree. `OpenFile` answers
/// those by doing nothing, so parking on that open would wait forever and
/// leave the user with no answer at all.
#[test]
fn a_jump_into_a_file_the_card_cannot_open_says_so() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");

    let _ = app.update_lsp(InspectorAction::JumpToLspLocation {
        card,
        path: PathBuf::from("/repo/target/generated.rs"),
        at: LspPosition::new(1, 0),
        coordinates: LspCoordinates::Scalar,
    });

    assert_eq!(
        notice(&app),
        Some("target/generated.rs is not in this worktree's file list")
    );
    assert!(!app.lsp_pending_jumps.contains_key(&card));
}

/// A jump is answered by the open it asked for or by nothing. Surviving an
/// unrelated open would let the caret be yanked minutes later, when the
/// user opens that file themselves, against text that has since changed.
#[test]
fn a_parked_jump_does_not_survive_a_different_file_opening() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    app.lsp_pending_jumps.insert(
        card,
        PendingLspJump {
            path: PathBuf::from("/repo/src/other.rs"),
            at: LspPosition::new(4, 2),
            coordinates: LspCoordinates::Scalar,
        },
    );

    // The flush is driven directly because these fixtures configure no
    // server, so `open_lsp_document` short-circuits into the disabled
    // status before reaching it - the half that
    // `a_file_with_no_server_reports_disabled_and_clears_a_parked_jump`
    // already covers.
    let _ = app.flush_pending_lsp_jump(card, Path::new("/repo/src/main.rs"));

    assert!(!app.lsp_pending_jumps.contains_key(&card));
}

/// The notice slot has two independent producers. An indexing report
/// arriving a second later must not erase the one message telling the user
/// their hover, locations and completions were thrown away.
#[tokio::test]
async fn progress_does_not_erase_a_failure_the_user_has_not_read() {
    let (manager, publisher) = crate::lsp::tests::progress_fixture().await;
    let mut app = lsp_test_app();
    app.lsp = manager;
    let card = repository_card(&mut app, "/repo");
    lsp_state(&mut app, card).attachment.document = Some(LspDocumentId(1));
    app.lsp_documents.insert(LspDocumentId(1), card);

    app.apply_lsp_event(LspEvent::DocumentDesynced {
        doc: LspDocumentId(1),
        version: 3,
        error: LspError::NoServer,
    });
    let failure = app.lsp_contexts[&card]
        .attachment
        .context
        .notice
        .clone()
        .expect("the desync is reported");

    for done in [false, true] {
        app.apply_lsp_event(
            publisher
                .publish(crate::lsp::LspProgress {
                    token: "indexing".into(),
                    title: Some("Indexing".into()),
                    message: None,
                    percentage: None,
                    done,
                })
                .await,
        );
        assert_eq!(
            app.lsp_contexts[&card].attachment.context.notice.as_deref(),
            Some(failure.as_str()),
            "progress with done={done} replaced the failure"
        );
    }

    // Dismissing is the user saying they have read it; only then may
    // progress have the slot.
    let _ = app.update_lsp(InspectorAction::DismissLspResults(card));
    app.apply_lsp_event(
        publisher
            .publish(crate::lsp::LspProgress {
                token: "indexing".into(),
                title: Some("Indexing".into()),
                message: None,
                percentage: Some(10),
                done: false,
            })
            .await,
    );
    assert_eq!(
        app.lsp_contexts[&card].attachment.context.notice.as_deref(),
        Some("Indexing (10%)")
    );
    app.lsp.begin_shutdown().await;
}

/// Opening a file no configured server claims is a supported configuration,
/// not a failure, and it must not leave an earlier file's jump parked.
#[test]
fn a_file_with_no_server_reports_disabled_and_clears_a_parked_jump() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    app.lsp_pending_jumps.insert(
        card,
        PendingLspJump {
            path: PathBuf::from("/repo/gone.rs"),
            at: LspPosition::new(1, 0),
            coordinates: LspCoordinates::Scalar,
        },
    );

    let _ = app.open_lsp_document(
        card,
        Path::new("/repo/notes.unknownext"),
        Path::new("/repo"),
        1,
        &"text\n".into(),
    );

    let state = &app.lsp_contexts[&card];
    assert_eq!(state.attachment.context.status, Some(LspStatus::Disabled));
    assert!(state.attachment.document.is_none());
    // Recorded even with no server: the file *is* open, and a later jump
    // compares against this path to decide whether it must open anything.
    assert_eq!(
        state.path.as_deref(),
        Some(Path::new("/repo/notes.unknownext"))
    );
    assert!(!app.lsp_pending_jumps.contains_key(&card));
}

/// Closing the buffer must retract the document from the routing map, or a
/// later document reusing the card would receive the dead one's events.
#[test]
fn closing_a_document_retracts_it_from_the_routing_map() {
    let mut app = lsp_test_app();
    let card = repository_card(&mut app, "/repo");
    lsp_state(&mut app, card).attachment.document = Some(LspDocumentId(1));
    lsp_state(&mut app, card).path = Some(PathBuf::from("/repo/a.rs"));
    app.lsp_documents.insert(LspDocumentId(1), card);

    app.close_lsp_document(card);

    assert!(app.lsp_documents.is_empty());
    let state = &app.lsp_contexts[&card];
    assert!(state.attachment.document.is_none());
    assert!(state.path.is_none());
    assert_eq!(state.attachment.diagnostics_version, None);
}

/// The shell re-runs one `RedrawRequested` through update and layout while
/// any pass publishes a message, rebuilding the view in between, and warns
/// once a frame has cost three such passes. Animated geometry therefore has
/// to be a function of the frame: when it was a function of the wall clock
/// the inspector was a few hundredths of a pixel wider on every pass, the
/// workspace strip measured a new viewport width each time and reported it,
/// and an animating frame never reached a fixed point.
#[test]
fn inspector_progress_is_the_same_for_every_pass_over_one_frame() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let _ = app.update(Message::ToggleContextInspector);
    assert!(
        app.pane_inspectors.is_animating(),
        "the toggle has to start a transition for this to measure anything",
    );

    let first = app.pane_inspector_progress(card);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        app.pane_inspector_progress(card),
        first,
        "a second pass over the same frame has to measure the same progress",
    );

    let frame = Instant::now() + DIALOG_TRANSITION_DURATION / 2;
    let _ = app.frame_clock.elapsed(frame);
    let advanced = app.pane_inspector_progress(card);
    let target = 1.0;
    assert!(
        (advanced - target).abs() < (first - target).abs(),
        "a later frame does advance the animation towards {target}: {first} then {advanced}",
    );
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        app.pane_inspector_progress(card),
        advanced,
        "and every pass over that frame measures it identically",
    );
}

#[test]
fn sidebar_worktree_collapse_survives_selection_synchronization() {
    let mut app = context_inspector_test_app(true);
    let project = ProjectId::new();
    let selected = WorktreeId::new();
    let other = WorktreeId::new();
    let row = |id, path: &str| {
        sidebar::SidebarWorktree::new(
            id,
            path.into(),
            Some("topic".into()),
            false,
            false,
            None,
            None,
            None,
        )
    };
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId(uuid::Uuid::new_v4()),
        "Example".into(),
        "/repo".into(),
        vec![row(selected, "/tmp"), row(other, "/other")],
        vec![],
    ));
    app.synchronize_sidebar_selection();
    drop(app.rebind_source_control());
    let other_row = sidebar::SelectionRow::Worktree(project, other);
    assert!(
        sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false)
            .contains(&other_row)
    );
    drop(app.update(Message::Sidebar(SidebarAction::ToggleWorktrees(project))));
    app.synchronize_sidebar_selection();
    let visible = sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false);
    assert!(!visible.contains(&other_row));
    assert!(visible.contains(&sidebar::SelectionRow::Worktree(project, selected)));
}

#[test]
fn the_checkouts_toggle_hides_every_checkout_and_returns_to_the_mode_they_had() {
    use settings::SidebarWorktrees::*;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_write_blocked = false;
    app.settings_path = Some(path.clone());
    drop(app.apply_setting_input(SettingsField::SidebarWorktrees, "expanded".into()));
    let project = ProjectId::new();
    drop(
        app.update(Message::Sidebar(SidebarAction::SetWorktreeDisplay {
            project_id: project,
            mode: Some(Grouped),
        })),
    );
    for (shown, expected) in [(true, Hidden), (false, Expanded), (true, Hidden)] {
        assert_eq!(app.settings.sidebar_worktrees.shows_checkouts(), shown);
        let generation = app.settings_save_generation;
        drop(app.update(Message::Sidebar(SidebarAction::ToggleCheckouts)));
        assert_eq!(app.settings.sidebar_worktrees, expected);
        assert_eq!(
            app.settings_draft.sidebar_worktrees, expected,
            "the settings page shows the same default"
        );
        assert_eq!(
            app.sidebar_state.worktree_display(project),
            if expected == Hidden { Hidden } else { Grouped },
            "a hidden default wins over the project's own mode, which then applies again"
        );
        assert!(
            app.settings_save_generation > generation,
            "the toggle saves"
        );
    }
    assert_eq!(app.settings.sidebar_worktrees_when_shown, Expanded);
    app.save_settings_now().unwrap();
    let saved = settings::load(&path).unwrap();
    assert_eq!(saved.sidebar_worktrees, Hidden);
    assert_eq!(saved.sidebar_worktrees_when_shown, Expanded);
    assert_eq!(
        saved.sidebar_worktree_overrides.get(&project),
        Some(&Grouped)
    );
    // The settings page choosing a visible mode is what the toggle returns
    // to after hiding them again.
    drop(app.apply_setting_input(SettingsField::SidebarWorktrees, "grouped".into()));
    drop(app.update(Message::Sidebar(SidebarAction::ToggleCheckouts)));
    drop(app.update(Message::Sidebar(SidebarAction::ToggleCheckouts)));
    assert_eq!(app.settings.sidebar_worktrees, Grouped);
}

#[test]
fn sidebar_preferences_observed_after_local_save_can_reset_external_override() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_write_blocked = false;
    app.settings_path = Some(path.clone());
    app.save_settings_now().unwrap();
    let project = ProjectId::new();
    let mut external = settings::load(&path).unwrap();
    external.sidebar_worktrees = settings::SidebarWorktrees::Hidden;
    external
        .sidebar_worktree_overrides
        .insert(project, settings::SidebarWorktrees::Expanded);
    settings::save(&path, &external).unwrap();
    drop(app.update(Message::SharedPreferencesObserved {
        generation: app.settings_ack_generation,
        settings: external,
    }));
    // A hidden default hides every project's checkouts; the project's
    // own mode is kept for when they show again.
    assert_eq!(
        app.sidebar_state.worktree_display(project),
        settings::SidebarWorktrees::Hidden
    );
    assert_eq!(
        app.settings.sidebar_worktree_overrides.get(&project),
        Some(&settings::SidebarWorktrees::Expanded)
    );
    app.settings.sidebar_worktree_overrides.remove(&project);
    app.save_settings_now().unwrap();
    let saved = settings::load(&path).unwrap();
    assert_eq!(saved.sidebar_worktrees, settings::SidebarWorktrees::Hidden);
    assert!(!saved.sidebar_worktree_overrides.contains_key(&project));
}

#[test]
fn another_windows_skipped_question_is_adopted_and_its_reenable_survives_a_save() {
    use settings::Confirmation::{CloseActiveTerminal, StopWorkingChat};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_write_blocked = false;
    app.settings_path = Some(path.clone());
    app.save_settings_now().unwrap();
    // Another window stops asking both questions.
    let mut external = settings::load(&path).unwrap();
    external.set_asks(StopWorkingChat, false);
    external.set_asks(CloseActiveTerminal, false);
    settings::save(&path, &external).unwrap();
    drop(app.update(Message::SharedPreferencesObserved {
        generation: app.settings_ack_generation,
        settings: external,
    }));
    for question in [StopWorkingChat, CloseActiveTerminal] {
        assert!(!app.asks(question), "{question:?} is adopted");
        assert_eq!(
            app.settings_draft.confirmations.get(&question),
            Some(&false),
            "Settings shows {question:?} off"
        );
    }
    // This window turns one back on; the other's choice is kept.
    let enable = app.update(Message::SettingsChanged {
        field: SettingsField::Confirm(StopWorkingChat),
        value: "true".into(),
    });
    settle_settings_task(&mut app, enable);
    let saved = settings::load(&path).unwrap();
    assert!(saved.asks(StopWorkingChat));
    assert!(!saved.asks(CloseActiveTerminal));
}

pub(super) fn context_inspector_test_app(with_focused_card: bool) -> Cagents {
    let card = with_focused_card.then(terminal_card_state);
    let card_id = card.as_ref().map(|card| card.id);
    let durable = Arc::new(WorkspaceState {
        cards: card.into_iter().collect(),
        focused: card_id,
        ..WorkspaceState::default()
    });
    let workspace = Cagents::rebuild_workspace(&durable);
    let settings = AppSettings::default();
    let (terminal_wake_tx, terminal_wake_rx) = async_channel::unbounded();
    let mut core = Core::default();
    core.window.show_headerbar = false;
    core.window.border_padding = Some(0);

    Cagents {
        startup_gate: startup_gate::State::default(),
        workspace,
        durable,
        terminal_working_directories: HashMap::new(),
        terminal_find: HashMap::new(),
        terminal_context: None,
        terminal_paste: None,
        cards: card_id
            .map(|card_id| HashMap::from([(card_id, CardRuntime::Loading)]))
            .unwrap_or_default(),
        card_chrome: HashMap::new(),
        card_indices: HashMap::new(),
        card_indices_source: std::sync::Weak::new(),
        terminal_ordinals_source: std::sync::Weak::new(),
        navigation_cache: sidebar::NavigationCache::default(),
        place_index: inspector_activity::PlaceIndex::default(),
        palette: palette_ui::State::default(),
        recently_closed: Vec::new(),
        presence: HashMap::new(),
        lifecycle: LifecycleManager::new(LifecycleBudget {
            max_hot_cards: settings.max_hot_cards,
            offscreen_grace: Duration::from_millis(settings.hibernation_delay_ms),
        }),
        lsp: LspManager::new(settings.language_servers.clone())
            .with_lifecycle(settings.lsp_idle_timeout_seconds, settings.lsp_max_sessions),
        lsp_documents: HashMap::new(),
        lsp_contexts: HashMap::new(),
        lsp_pending_jumps: HashMap::new(),
        next_lsp_document: 0,
        lifecycle_sweep_deadline: None,
        lifecycle_sweep_generation: 0,
        navigation_save: None,
        replica_capture: ReplicaCapture::default(),
        navigation_save_generation: 0,
        pending_repository_reconciliation: Vec::new(),
        released_repository_navigation: HashMap::new(),
        keyboard_owed: None,
        reported_title_save: ReportedTitleSave::default(),
        lifecycle_sweep_destination: None,
        input_trace: crate::input_trace::InputTrace::default(),
        terminal_wake_tx,
        terminal_wake_rx,
        hosted_terminals_rx: async_channel::unbounded().1,
        terminal_starts: std::sync::Mutex::new(HashMap::new()),
        terminal_failure: terminal_failure::State::default(),
        terminal_save: terminal_save::State::default(),
        saved_terminals: saved_terminal::State::default(),
        pending_adoptions: Vec::new(),
        restored_agent_attaches: VecDeque::new(),
        relaunched_terminals: HashSet::new(),
        terminal_crash_notices: HashMap::new(),
        shared_views: shared_views::State::default(),
        workspace_model: crate::workspace_model::WorkspaceModel::default(),
        workspace_model_opens: HashSet::new(),
        workspace_model_rx: async_channel::unbounded().1,
        // Unbounded and unread: a fixture publishes into a queue nobody
        // drains, which is what keeps a test off the daemon entirely.
        workspace_model_tx: async_channel::unbounded().0,
        activation_rx: async_channel::unbounded().1,
        notification_ui: notification_ui::State::default(),
        attention_focus_epoch: 0,
        daemon_health_ui: daemon_health_ui::State::default(),
        session: None,
        save_health: None,
        session_recovery: session_recovery::State::default(),
        settings_draft: SettingsDraft::from_settings(&settings),
        settings_base: settings.clone(),
        settings_save_generation: 0,
        settings_ack_generation: 0,
        settings_reset_generation: None,
        settings_save_lane: Arc::new(std::sync::Mutex::new(None)),
        update_prepared: false,
        update_flush_pending: false,
        update_generation: 0,
        update_committed: false,
        exit: None,
        document_tone_stale: false,
        settings,
        #[cfg(feature = "integrated-browser")]
        browser_teardowns: Vec::new(),
        #[cfg(feature = "integrated-browser")]
        browser_import: browser_import_ui::State::default(),
        terminal_teardowns: Vec::new(),
        terminal_review_card: None,
        terminal_review_revision: 0,
        terminal_review_identity: None,
        terminal_review_input: crate::terminal::ReviewInputBarrier::default(),
        #[cfg(feature = "integrated-browser")]
        browser_factory: None,
        #[cfg(feature = "integrated-browser")]
        browser_backend: browser_backend::State::default(),
        #[cfg(feature = "integrated-browser")]
        browser_parent_lifetime: None,
        system_url_opener: file_manager::SystemUrlOpener::default(),
        window_scale_factor: 1.0,
        window_width: 1_600.0,
        window_height: 900.0,
        context_sections_height: None,
        window_filled: false,
        window_state_observed: false,
        right_edge_tiled: false,
        core,
        settings_path: None,
        uncertain_terminal_proofs: HashMap::new(),
        terminal_recovery_attempts: HashSet::new(),
        retained_outputs: HashMap::new(),
        pending_terminal_handoffs: HashMap::new(),
        unsaved_chat_returns: HashMap::new(),
        terminal_handoff_commit_inflight: false,
        settings_write_blocked: true,
        notices: NoticeLog::default(),
        sidebar_model: SidebarModel::ready(SidebarMode::Expanded, Vec::new()),
        sidebar_state: SidebarState::default(),
        project_ui: project_ui::State::default(),
        worktree_protections: Default::default(),
        channel_ui: channel_ui::State::default(),
        sidebar_selection_ui: sidebar_selection_ui::State::default(),
        chat_ui: chat_ui::State::default(),
        resume_ui: resume_ui::State::default(),
        update_ui: update_ui::State::default(),
        chrome_ui: chrome_ui::State::default(),
        pull_request_inspector: pull_request_ui::State::default(),
        branch_ui: branch_ui::State::default(),
        history_ui: history_ui::State::default(),
        refresh_ui: refresh_ui::State::default(),
        source_control_mutations: HashMap::new(),
        operation_recovery: Default::default(),
        device_ui: device_ui::State::default(),
        account_ui: account_ui::Accounts::default(),
        automation_ui: crate::automation_ui::State::default(),
        work_ui: crate::work_ui::State::default(),
        automation_approval: automation_approval::State::default(),
        active_workspace: None,
        worktree_focus: HashMap::new(),
        channel_offsets: HashMap::new(),
        worktree_offsets: HashMap::new(),
        worktree_owners: HashMap::new(),
        #[cfg(any(test, feature = "development"))]
        projected_protections: Default::default(),
        known_worktree_roots: worktree_ui::KnownWorktreeRoots::default(),
        worktree_paths: worktree_ui::WorktreePathIndex::default(),
        pull_request_board: chat_ui::PullRequestBoard::default(),
        workspace_view_generation: 0,
        workspace_scroll_id: widget::Id::new("workspace-strip-0"),
        github_availability: HashMap::new(),
        review_repositories: HashMap::new(),
        repository_generations: HashMap::new(),
        repository_pr_cancellations: HashMap::new(),
        repository_load_sequence: 0,
        worktree_dialog: None,
        settings_reset_confirm: false,
        dont_ask_again: BTreeSet::new(),
        settings_reset_keyboard_nav: None,
        pull_request_dialog: None,
        pull_request_dialog_generation: 0,
        dirty_repository_close: None,
        chat_close_confirm: None,
        transient_work_close: None,
        automation_close_generation: 0,
        terminal_close: None,
        queued_card_closes: VecDeque::new(),
        close_queue_advancing: false,
        close_commit_delivered: None,
        terminal_close_attempts: HashSet::new(),
        completed_release: None,
        terminal_close_generation: 0,
        closed_terminal_cards: HashSet::new(),
        proven_gone_cards: HashSet::new(),
        worktree_operations: HashMap::new(),
        pending_panel_open: None,
        deferred_navigation: VecDeque::new(),
        owed_projection: None,
        dispatching: 0,
        frame_clock: FrameClock::default(),
        latency: crate::performance::InteractionLatency::default(),
        latency_focus: None,
        #[cfg(feature = "latency-harness")]
        latency_fixture: None,
        #[cfg(feature = "latency-harness")]
        latency_fixture_started: false,
        pane_inspectors: pane_inspector::PaneInspectors::default(),
        context_sections: context_sections::Sections::default(),
        settings_category: SettingsCategory::Appearance,
        settings_search: String::new(),
        tab_history: crate::tab_history::TabHistory::default(),
        workspace_history: crate::tab_history::TabHistory::default(),
        workspace_cycle_modifiers: keyboard::Modifiers::empty(),
        keyboard_modifiers: keyboard::Modifiers::empty(),
        settings_feedback: None,
        settings_pane_focused: false,
        reopen_utility: None,
        settings_pending: HashMap::new(),
        settings_generation: 0,
        settings_invalid: HashMap::new(),
        settings_save_error: None,
        status: None,
    }
}

#[test]
fn pane_attention_tracks_semantic_events_and_a_hover_reads_only_the_pane_it_stays_on() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut app = context_inspector_test_app(true);
    let window = window::Id::RESERVED;
    app.core.set_main_window_id(Some(window));
    drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
        window, true,
    ))));
    app.settings.focus_follows_mouse = true;
    let first = app.workspace.focused().unwrap();
    let second = terminal_card_state();
    let third = terminal_card_state();
    let (second_id, third_id) = (second.id, third.id);
    Arc::make_mut(&mut app.durable)
        .cards
        .extend([second, third]);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(1_000.0);
    app.workspace.native_scroll(120.5, 1_000.0);
    let activity = |state| {
        Some(Activity {
            provider: Provider::Omp,
            state,
        })
    };
    app.observe_agent_activity_for_card(first, activity(ActivityState::Working));
    app.observe_agent_activity_for_card(first, activity(ActivityState::Idle));
    assert!(!app.sidebar_state.has_card_update(first));
    app.observe_agent_activity_for_card(second_id, activity(ActivityState::Waiting));
    app.observe_agent_activity_for_card(third_id, activity(ActivityState::Working));
    app.observe_agent_activity_for_card(third_id, activity(ActivityState::Idle));
    assert!(app.sidebar_state.has_card_update(second_id));
    assert!(app.sidebar_state.has_card_update(third_id));
    let offset = app.workspace.camera().offset();
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(second_id),
    });
    assert_eq!(app.workspace.focused(), Some(second_id));
    assert_eq!(app.workspace.camera().offset(), offset);
    // Hover focus is immediate, but passing over a pane does not read it.
    assert!(app.sidebar_state.has_card_update(second_id));
    // The pointer moves on to the third pane before the attention dwell
    // ends, and that dwell's timer, stale now, reads neither.
    let stale = app.attention_focus_epoch;
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(third_id),
    });
    assert_eq!(app.workspace.focused(), Some(third_id));
    drop(app.update(Message::AcknowledgeVisiblePane {
        card: second_id,
        epoch: stale,
    }));
    assert!(app.sidebar_state.has_card_update(second_id));
    assert!(app.sidebar_state.has_card_update(third_id));
    // Back on the second pane, and it stays there for the dwell.
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(second_id),
    });
    drop(app.update(Message::AcknowledgeVisiblePane {
        card: second_id,
        epoch: app.attention_focus_epoch,
    }));
    // Only the pane the pointer stayed on is read.
    assert!(!app.sidebar_state.has_card_update(second_id));
    assert!(app.sidebar_state.has_card_update(third_id));
    app.observe_agent_activity_for_card(second_id, activity(ActivityState::Waiting));
    assert!(!app.sidebar_state.has_card_update(second_id));
    app.settings.focus_follows_mouse = false;
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(third_id),
    });
    assert_eq!(app.workspace.focused(), Some(second_id));
    assert!(app.sidebar_state.has_card_update(third_id));
}

/// Moving focus alone is saved once per burst rather than once per move,
/// but the durable state carries it at once, so any save that happens
/// sooner - or the one on exit - records it.
#[test]
fn focus_only_changes_are_saved_once_per_burst() {
    let mut app = context_inspector_test_app(true);
    app.settings.focus_follows_mouse = true;
    let second = terminal_card_state();
    let third = terminal_card_state();
    let (second_id, third_id) = (second.id, third.id);
    Arc::make_mut(&mut app.durable)
        .cards
        .extend([second, third]);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    // Every pane fits, so moving focus exposes nothing new.
    let update = app.workspace.resize_viewport(3_000.0);
    let _ = app.apply_viewport(update, false, DurableChange::Content);
    assert!(app.navigation_save.is_none());

    for card in [second_id, third_id] {
        let _ = app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::PointerEntered(card),
        });
        assert_eq!(app.durable.focused, Some(card));
        assert!(
            app.navigation_save.is_some(),
            "the save waits for the burst"
        );
    }
    let _ = app.update(Message::NavigationSaveDue(app.navigation_save_generation));
    assert!(app.navigation_save.is_none());

    // A change that must be saved now takes the pending focus with it.
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(second_id),
    });
    assert!(app.navigation_save.is_some());
    let _ = app.apply_viewport(ViewportUpdate::unchanged(), false, DurableChange::Content);
    assert!(app.navigation_save.is_none());
}

/// Seen means seen by the user. The focused pane of a window in the
/// background has not been seen, so its news must still mark it unread.
#[test]
fn an_update_for_the_focused_pane_of_a_background_window_marks_it_unread() {
    let mut app = context_inspector_test_app(true);
    app.core.set_main_window_id(Some(window::Id::RESERVED));
    let card = app.workspace.focused().unwrap();

    app.refresh_ui.focused = true;
    assert!(
        app.pane_is_active(card),
        "the fixture differs from a seen pane only in window focus"
    );
    app.observe_agent_update_for_card(card);
    assert!(!app.sidebar_state.has_card_update(card));

    app.refresh_ui.focused = false;
    app.observe_agent_update_for_card(card);
    assert!(app.sidebar_state.has_card_update(card));
}

#[test]
fn unread_agent_updates_snapshot_only_saved_open_cards_and_persist_reads() {
    let mut app = context_inspector_test_app(true);
    let open = app.workspace.focused().unwrap();
    let owner = (
        crate::model::ProjectId::new(),
        crate::model::WorktreeId::new(),
    );
    let mut closed = terminal_card_state();
    closed.existence = Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("closed unread fixture".into()),
        },
    };
    let closed_id = closed.id;
    Arc::make_mut(&mut app.durable).cards.push(closed);
    let missing = CardId::new();
    app.sidebar_state
        .observe_agent_update(open, owner.0, owner.1);
    app.sidebar_state.observe_card_update(closed_id);
    app.sidebar_state.observe_card_update(missing);

    let saved = app.persistent_workspace();
    assert_eq!(
        saved.agent_updates,
        std::collections::BTreeMap::from([(open, Some(owner))])
    );
    assert_eq!(app.durable.agent_updates, saved.agent_updates);
    let retained = Arc::clone(&app.durable);
    drop(app.persistent_workspace());
    assert!(
        Arc::ptr_eq(&retained, &app.durable),
        "unchanged unread does not clone durable state"
    );

    let mut restored = sidebar::SidebarState::default();
    restored.restore_unread_updates(saved.agent_updates.clone());
    assert!(restored.has_card_update(open));
    assert!(restored.has_agent_update(owner.0, owner.1));
    assert!(!restored.has_card_update(closed_id));
    assert!(!restored.has_card_update(missing));

    app.sidebar_state
        .acknowledge_agent_update(open, &sidebar::AttentionPlacement::default());
    let read = app.persistent_workspace();
    assert!(read.agent_updates.is_empty());
    assert!(app.durable.agent_updates.is_empty());
    assert!(
        saved.agent_updates.contains_key(&open),
        "a queued prior snapshot remains immutable"
    );
}

/// Every snapshot handed to the session writer is one storage accepts,
/// whichever path stopped a pane being open and whatever it left the strip
/// showing.
#[test]
fn a_snapshot_never_focuses_a_pane_the_window_no_longer_holds_open() {
    let closed = || Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("closed fixture".into()),
        },
    };
    let mut app = context_inspector_test_app(true);
    let first_id = app.durable.focused.unwrap();
    let second = terminal_card_state();
    let second_id = second.id;
    Arc::make_mut(&mut app.durable).cards.push(second);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    let update = app.workspace.focus(second_id).unwrap();
    let _ = app.apply_viewport(update, false, DurableChange::None);
    assert_eq!(app.durable.focused, Some(second_id));

    // The strip closes the focused pane and its update goes unapplied.
    app.workspace.request_close(second_id).unwrap();
    app.workspace.commit_close(second_id).unwrap();
    Arc::make_mut(&mut app.durable).cards[1].existence = closed();
    let saved = app.persistent_workspace();
    crate::persistence::validate_workspace(&saved).unwrap();
    assert_eq!(saved.focused, Some(first_id));
    assert_eq!(app.durable.focused, Some(first_id));

    // The strip still shows a pane the window no longer holds open.
    Arc::make_mut(&mut app.durable).cards[0].existence = closed();
    assert_eq!(app.workspace.focused(), Some(first_id));
    let saved = app.persistent_workspace();
    crate::persistence::validate_workspace(&saved).unwrap();
    assert_eq!(saved.focused, None);
}

/// The app under the Attention filter with `card` held by one channel.
fn attention_channel_test_app() -> (Cagents, CardId, crate::model::ChannelId) {
    use crate::model::{ChannelId, ProjectChannel};
    use crate::sidebar::{SidebarFilter, SidebarProject};

    let mut app = context_inspector_test_app(true);
    app.refresh_ui.focused = false;
    let card = app.workspace.focused().unwrap();
    let project = ProjectId::new();
    let channel = ChannelId::new();
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "project".into(),
        "/repo".into(),
        Vec::new(),
        Vec::new(),
    ));
    Arc::make_mut(&mut app.durable)
        .channels
        .push(ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: project,
            title: "channel".into(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
    drop(app.update(Message::Sidebar(SidebarAction::SetFilter(
        SidebarFilter::Attention,
    ))));
    (app, card, channel)
}

fn attention_lists_channel(app: &Cagents, channel: crate::model::ChannelId) -> bool {
    attention_lists(app, sidebar::SelectionRow::Channel(channel))
}

fn attention_lists(app: &Cagents, row: sidebar::SelectionRow) -> bool {
    sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false).contains(&row)
}

/// Moves `card` out of `channel` onto a new worktree of the channel's
/// project, as an unchanneled pane scoped to it, which the Attention list
/// names on a pane row of its own. A channel's own panes never get one.
fn add_attention_worktree(
    app: &mut Cagents,
    card: CardId,
    channel: crate::model::ChannelId,
) -> (ProjectId, crate::model::WorktreeId) {
    let project = app
        .durable
        .channels
        .iter()
        .find(|entry| entry.id == channel)
        .unwrap()
        .project_id;
    let worktree = crate::model::WorktreeId::new();
    let path = PathBuf::from("/repo/feature");
    app.sidebar_model
        .projects
        .iter_mut()
        .find(|entry| entry.id == project)
        .unwrap()
        .worktrees
        .push(sidebar::SidebarWorktree::from_durable(
            worktree,
            path.clone(),
            Some("feature".into()),
        ));
    let durable = Arc::make_mut(&mut app.durable);
    for entry in &mut durable.channels {
        entry.cards.retain(|known| *known != card);
        if entry.focused == Some(card) {
            entry.focused = None;
        }
    }
    durable.card_scopes.insert(card, path);
    (project, worktree)
}

/// Accessible names of everything the expanded sidebar draws.
fn sidebar_names(app: &Cagents) -> Vec<String> {
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};

    let renderer = headless_renderer();
    let mut element = sidebar::view(
        &app.sidebar_model,
        &app.sidebar_state,
        SidebarMode::Expanded,
        320.0,
        false,
    );
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 900.0)),
    );
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| node.node().label())
        .map(ToOwned::to_owned)
        .collect()
}

/// Rows kept under Attention go with their channel or pane on the next
/// ordinary update, so one listed again is not drawn as read.
#[test]
fn attention_forgets_kept_rows_once_their_surface_is_gone() {
    let (mut app, card, channel) = attention_channel_test_app();
    let channel_row = sidebar::SelectionRow::Channel(channel);
    app.sidebar_state.observe_card_update(card);
    app.refresh_sidebar_channels();
    assert!(attention_lists(&app, channel_row));
    drop(app.update(Message::Sidebar(SidebarAction::MarkChannelRead(channel))));
    assert!(!app.sidebar_state.has_card_update(card));
    assert!(
        attention_lists(&app, channel_row),
        "the channel row is kept"
    );
    let saved_channel = app
        .durable
        .channels
        .iter()
        .find(|entry| entry.id == channel)
        .unwrap()
        .clone();
    Arc::make_mut(&mut app.durable)
        .channels
        .retain(|entry| entry.id != channel);
    drop(app.update(Message::Sidebar(
        SidebarAction::SearchChanged(String::new()),
    )));
    Arc::make_mut(&mut app.durable).channels.push(saved_channel);
    drop(app.update(Message::Sidebar(
        SidebarAction::SearchChanged(String::new()),
    )));
    assert!(
        !attention_lists(&app, channel_row),
        "a channel created again is not drawn as read"
    );

    let card_row = sidebar::SelectionRow::Card(card);
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    app.sidebar_state
        .observe_agent_update(card, project, worktree);
    app.refresh_sidebar_channels();
    assert!(attention_lists(&app, card_row));
    app.read_card_update(card);
    assert!(!app.sidebar_state.has_card_update(card));
    app.refresh_sidebar_channels();
    assert!(attention_lists(&app, card_row), "the pane row is kept");
    // The pane joins the channel, whose row now speaks for it, and leaves
    // it again with nothing new to say.
    let durable = Arc::make_mut(&mut app.durable);
    durable
        .channels
        .iter_mut()
        .find(|entry| entry.id == channel)
        .unwrap()
        .cards
        .push(card);
    drop(app.update(Message::Sidebar(
        SidebarAction::SearchChanged(String::new()),
    )));
    assert!(!attention_lists(&app, card_row));
    for entry in &mut Arc::make_mut(&mut app.durable).channels {
        entry.cards.retain(|known| *known != card);
    }
    drop(app.update(Message::Sidebar(
        SidebarAction::SearchChanged(String::new()),
    )));
    assert!(
        !attention_lists(&app, card_row),
        "a pane listed again is not drawn as read"
    );
}

/// Closing a pane whose update is unread is not reading it, so its row
/// leaves the Attention list instead of staying as read.
#[test]
fn attention_does_not_keep_the_row_of_a_closed_unread_pane() {
    let (mut app, card, channel) = attention_channel_test_app();
    let card_row = sidebar::SelectionRow::Card(card);
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    app.sidebar_state
        .observe_agent_update(card, project, worktree);
    app.refresh_sidebar_channels();
    assert!(attention_lists(&app, card_row));
    app.workspace.request_close(card).unwrap();
    drop(app.update(Message::CommitClose(card)));
    assert!(
        app.card_state(card)
            .is_none_or(|state| !state.existence.is_open()),
        "the pane closed"
    );
    assert!(!attention_lists(&app, card_row));
}

/// The Attention list names the pane that needs the user, with the
/// checkout it runs in, however the user groups worktrees elsewhere: it
/// has no checkout rows to group, and hides nothing behind a disclosure.
#[test]
fn attention_lists_unchanneled_panes_without_checkout_rows() {
    let (mut app, card, channel) = attention_channel_test_app();
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    let title = app
        .card_state(card)
        .map(|state| app.card_display_title(state).to_owned())
        .unwrap();
    for display in [
        settings::SidebarWorktrees::Grouped,
        settings::SidebarWorktrees::Expanded,
    ] {
        app.sidebar_state.set_sidebar_worktrees(display);
        app.sidebar_state
            .observe_agent_update(card, project, worktree);
        app.refresh_sidebar_channels();
        let rows = sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false);
        assert_eq!(
            rows,
            vec![
                sidebar::SelectionRow::Project(project),
                sidebar::SelectionRow::Card(card),
            ],
            "{display:?}"
        );
        let names = sidebar_names(&app);
        assert!(
            names
                .iter()
                .any(|name| name
                    == &format!("Open pane {title} in feature, 1 unread conversation")),
            "{display:?}: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.contains("Worktrees")),
            "{display:?}: {names:?}"
        );

        app.read_card_update(card);
        drop(app.update(Message::Sidebar(SidebarAction::HideReadAttention)));
        assert!(!attention_lists(&app, sidebar::SelectionRow::Card(card)));
    }
}

/// Opening a pane row goes to the pane and reads it; the row stays where
/// the user left it, drawn as read, until read rows are hidden.
#[test]
fn opening_a_pane_row_focuses_and_reads_the_pane() {
    let (mut app, card, channel) = attention_channel_test_app();
    let card_row = sidebar::SelectionRow::Card(card);
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    app.sidebar_state
        .observe_agent_update(card, project, worktree);
    app.refresh_sidebar_channels();
    assert!(attention_lists(&app, card_row));
    assert_eq!(
        app.sidebar_navigation_action(card_row),
        Some(SidebarAction::OpenCard { card })
    );
    drop(app.update(Message::Sidebar(SidebarAction::OpenCard { card })));
    assert_eq!(app.workspace.focused(), Some(card));
    assert!(!app.sidebar_state.has_card_update(card));
    assert!(attention_lists(&app, card_row), "the read row is kept");
    assert!(
        sidebar_names(&app)
            .iter()
            .any(|name| name.starts_with("Open pane") && name.ends_with(", read")),
        "the kept row is described as read"
    );
    // Deleting from a pane row closes nothing.
    drop(app.update(Message::Sidebar(SidebarAction::DeleteRow(card_row))));
    assert!(
        app.card_state(card)
            .is_some_and(|state| state.existence.is_open())
    );
    drop(app.update(Message::Sidebar(SidebarAction::HideReadAttention)));
    assert!(!attention_lists(&app, card_row));
}

/// A conversation open in two panes has one row, and opening it reads
/// both cards, not only the one it goes to.
#[test]
fn opening_a_pane_row_reads_every_card_of_its_conversation() {
    let (mut app, card, channel) = attention_channel_test_app();
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    let mut alias = app.card_state(card).unwrap().clone();
    alias.id = CardId::new();
    let alias_id = alias.id;
    let durable = Arc::make_mut(&mut app.durable);
    durable
        .card_scopes
        .insert(alias_id, PathBuf::from("/repo/feature"));
    durable.cards.push(alias);
    for id in [card, alias_id] {
        app.sidebar_state
            .observe_agent_update(id, project, worktree);
    }
    drop(app.update(Message::Sidebar(
        SidebarAction::SearchChanged(String::new()),
    )));
    assert_eq!(
        sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false),
        vec![
            sidebar::SelectionRow::Project(project),
            sidebar::SelectionRow::Card(card),
        ],
        "one row for the conversation"
    );
    drop(app.update(Message::Sidebar(SidebarAction::OpenCard { card })));
    assert_eq!(app.workspace.focused(), Some(card));
    assert!(!app.sidebar_state.has_card_update(card));
    assert!(
        !app.sidebar_state.has_card_update(alias_id),
        "the other card of the conversation is read with it"
    );
}

/// Revealing a pane while the Attention list draws its row goes to that
/// row and keeps the list; a pane the list does not draw is revealed at
/// its checkout, in the unfiltered list.
#[test]
fn revealing_a_pane_under_attention_targets_its_row() {
    let (mut app, card, channel) = attention_channel_test_app();
    app.pane_inspectors = pane_inspector::PaneInspectors::default();
    let card_row = sidebar::SelectionRow::Card(card);
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    app.sidebar_state
        .observe_agent_update(card, project, worktree);
    app.refresh_sidebar_channels();
    app.sidebar_state
        .apply(&SidebarAction::ToggleProject(project));
    assert!(!attention_lists(&app, card_row), "folded away");
    drop(app.update_chrome(chrome_ui::Action::Reveal(card)));
    assert_eq!(app.workspace.focused(), Some(card));
    assert_eq!(
        app.sidebar_state.filter(),
        sidebar::SidebarFilter::Attention
    );
    assert!(attention_lists(&app, card_row), "its project unfolds");
    assert_eq!(app.reveal_card_row(card), Some(card_row));

    app.read_card_update(card);
    drop(app.update(Message::Sidebar(SidebarAction::HideReadAttention)));
    assert!(!attention_lists(&app, card_row));
    Arc::make_mut(&mut app.durable).active_channel = None;
    app.active_workspace = Some(PathBuf::from("/repo/feature"));
    assert_eq!(
        app.reveal_card_row(card),
        Some(sidebar::SelectionRow::Worktree(project, worktree))
    );
    assert_eq!(app.sidebar_state.filter(), sidebar::SidebarFilter::All);

    // A working pane's row is under In progress: revealing it unfolds
    // that section and leaves its folded project alone.
    drop(app.update(Message::Sidebar(SidebarAction::SetFilter(
        sidebar::SidebarFilter::Attention,
    ))));
    app.observe_agent_activity_for_card(card, agent_activity(crate::agent::ActivityState::Working));
    app.refresh_sidebar_channels();
    if app.sidebar_state.project_expanded(project) {
        app.sidebar_state
            .apply(&SidebarAction::ToggleProject(project));
    }
    app.sidebar_state.apply(&SidebarAction::ToggleInProgress);
    assert!(!attention_lists(&app, card_row), "In progress is folded");
    assert_eq!(app.reveal_card_row(card), Some(card_row));
    assert!(!app.sidebar_state.in_progress_collapsed());
    assert!(!app.sidebar_state.project_expanded(project));
    assert_eq!(
        app.sidebar_state.filter(),
        sidebar::SidebarFilter::Attention
    );
}

/// With checkouts hidden, the unfiltered list names every open pane, a
/// quiet one included, and Reveal and Open treat its row as the Attention
/// list does; a checkout's own row never comes back to stand in for it.
#[test]
fn hidden_checkouts_reveal_and_open_a_pane_by_its_row() {
    let (mut app, card, channel) = attention_channel_test_app();
    app.pane_inspectors = pane_inspector::PaneInspectors::default();
    let card_row = sidebar::SelectionRow::Card(card);
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    app.sidebar_state
        .set_sidebar_worktrees(settings::SidebarWorktrees::Hidden);
    drop(app.update(Message::Sidebar(SidebarAction::SetFilter(
        sidebar::SidebarFilter::All,
    ))));
    app.refresh_sidebar_channels();
    assert!(
        attention_lists(&app, card_row),
        "a pane with no news has a row"
    );
    app.sidebar_state
        .apply(&SidebarAction::ToggleProject(project));
    drop(app.update_chrome(chrome_ui::Action::Reveal(card)));
    assert_eq!(app.workspace.focused(), Some(card));
    assert!(attention_lists(&app, card_row), "its project unfolds");
    assert_eq!(app.reveal_card_row(card), Some(card_row));

    drop(app.update(Message::Sidebar(SidebarAction::OpenCard { card })));
    assert_eq!(app.workspace.focused(), Some(card));
    let rows = sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false);
    assert!(
        !rows.contains(&sidebar::SelectionRow::Worktree(project, worktree)),
        "{rows:?}"
    );
    assert!(rows.contains(&card_row), "{rows:?}");
}

/// A plain New terminal records no scope: the workspace files it under
/// the checkout holding its launch directory, and its row goes there too
/// while checkouts are hidden. Without one the pane had no row and
/// nothing in the sidebar led to it.
#[test]
fn hidden_checkouts_list_a_plain_terminal_under_its_launch_checkout() {
    let (app, card, project, worktree, terminal) = hidden_checkouts_plain_terminal();
    assert_ne!(terminal, card);
    assert!(
        matches!(
            &app.card_state(terminal).unwrap().kind,
            CardKind::Terminal(session)
                if session.launch.working_directory == Path::new("/repo/feature")
        ),
        "a terminal launched in the checkout"
    );
    assert!(
        !app.durable.card_scopes.contains_key(&terminal),
        "a plain terminal records no scope"
    );
    assert!(attention_lists(&app, sidebar::SelectionRow::Card(terminal)));
    let row = pane_row(&app, project, terminal).expect("the terminal has a row in its project");
    assert_eq!(row.worktree, Some(worktree));
    assert!(row.terminal);
}

/// With checkouts hidden, an open New terminal in /repo/feature: the
/// app, the pre-existing pane, its project and checkout, and the
/// terminal.
fn hidden_checkouts_plain_terminal()
-> (Cagents, CardId, ProjectId, crate::model::WorktreeId, CardId) {
    let (mut app, card, channel) = attention_channel_test_app();
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    Arc::make_mut(&mut app.durable).active_channel = None;
    app.sidebar_state
        .set_sidebar_worktrees(settings::SidebarWorktrees::Hidden);
    drop(app.update(Message::Sidebar(SidebarAction::SetFilter(
        sidebar::SidebarFilter::All,
    ))));
    // New terminal opens in the selected checkout, through the update
    // path that refreshes the sidebar.
    app.active_workspace = Some(PathBuf::from("/repo/feature"));
    drop(app.update(Message::NewTerminal));
    let terminal = app.workspace.focused().unwrap();
    (app, card, project, worktree, terminal)
}

fn pane_row(app: &Cagents, project: ProjectId, card: CardId) -> Option<&sidebar::SidebarCard> {
    app.sidebar_model
        .projects
        .iter()
        .find(|entry| entry.id == project)?
        .panes
        .iter()
        .find(|row| row.id == card)
}

/// A shell's host title, "command · directory", names its directory,
/// which the row's place already does by its checkout: the row keeps the
/// command, and reads "fish · feature" rather than "fish · feature ·
/// feature". Any other title, and one the user gave, shows whole.
#[test]
fn a_pane_row_titles_a_shell_by_its_command_beside_its_place() {
    let (mut app, _, project, _, terminal) = hidden_checkouts_plain_terminal();
    let retitle = |app: &mut Cagents, title: &str| {
        assert!(app.apply_reported_title(terminal, title.into()));
        app.refresh_attention_card_rows();
        pane_row(app, project, terminal).unwrap().title.clone()
    };
    assert_eq!(retitle(&mut app, "fish · feature"), "fish");
    // The shell moved: the directory it reports comes from where it is.
    app.terminal_working_directories
        .insert(terminal, PathBuf::from("/repo/feature/src"));
    assert_eq!(retitle(&mut app, "cargo · src"), "cargo");
    assert_eq!(retitle(&mut app, "cargo · feature"), "cargo · feature");
    assert_eq!(retitle(&mut app, "main.rs - VIM"), "main.rs - VIM");
    // The tab keeps the whole title; it names no place.
    assert_eq!(
        app.card_display_title(app.card_state(terminal).unwrap()),
        "main.rs - VIM"
    );
    assert!(app.apply_reported_title(terminal, "fish · src".into()));
    app.chrome_ui
        .user_titles
        .insert(terminal, "fish · src".into());
    app.refresh_attention_card_rows();
    assert_eq!(
        pane_row(&app, project, terminal).unwrap().title,
        "fish · src",
        "a title the user gave is theirs"
    );
}

/// Renaming a pane renames its row in the Attention list at once.
#[test]
fn renaming_a_pane_retitles_its_attention_row() {
    let (mut app, card, channel) = attention_channel_test_app();
    let (project, worktree) = add_attention_worktree(&mut app, card, channel);
    app.sidebar_state
        .observe_agent_update(card, project, worktree);
    app.refresh_sidebar_channels();
    let _ = app.update_chrome(chrome_ui::Action::Rename(card));
    let _ = app.update_chrome(chrome_ui::Action::RenameChanged("Release review".into()));
    drop(app.update_chrome(chrome_ui::Action::RenameSubmit));
    let row = app
        .sidebar_model
        .projects
        .iter()
        .flat_map(|project| &project.panes)
        .find(|row| row.id == card)
        .expect("the pane keeps its row");
    assert_eq!(row.title, "Release review");
    assert!(
        sidebar_names(&app)
            .iter()
            .any(|name| name.starts_with("Open pane Release review in feature")),
        "{:?}",
        sidebar_names(&app)
    );
}

/// Folding In progress while one of its rows has keyboard focus hands
/// focus to the section's header, where the user can unfold it again;
/// folding it with focus elsewhere moves nothing.
#[test]
fn folding_in_progress_moves_focus_from_its_rows_to_its_header() {
    use cosmic::iced::advanced::widget::Tree;
    use cosmic::iced::advanced::{Layout, layout};
    use cosmic::iced::runtime::{Action, task};
    use cosmic::iced::{Size, advanced::widget::operation};

    /// Runs `tasks` in turn over one tree of the app's view, as the
    /// runtime runs them over the frame it built, and returns the
    /// messages they answer with.
    fn run(app: &Cagents, tasks: Vec<Task<Message>>) -> Vec<Message> {
        use cosmic::iced::futures::StreamExt;
        let renderer = headless_renderer();
        let mut element = cosmic::Application::view(app);
        let mut tree = Tree::new(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("executor");
        let mut messages = Vec::new();
        for task in tasks {
            let Some(mut stream) = task::into_stream(task) else {
                continue;
            };
            while let Some(action) = executor.block_on(stream.next()) {
                match action {
                    Action::Widget(mut operation) => loop {
                        element.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut operation,
                        );
                        match operation.finish() {
                            operation::Outcome::Chain(next) => operation = next,
                            _ => break,
                        }
                    },
                    Action::Output(cosmic::Action::App(message)) => messages.push(message),
                    _ => {}
                }
            }
        }
        messages
    }

    let (mut app, card, channel) = attention_channel_test_app();
    app.pane_inspectors = pane_inspector::PaneInspectors::default();
    let card_row = sidebar::SelectionRow::Card(card);
    add_attention_worktree(&mut app, card, channel);
    app.observe_agent_activity_for_card(card, agent_activity(crate::agent::ActivityState::Working));
    app.refresh_sidebar_channels();
    assert!(!app.sidebar_state.in_progress_collapsed());
    assert!(
        attention_lists(&app, card_row),
        "the working pane is in progress"
    );

    for (focus, refocus) in [(Some(card_row), true), (None, false)] {
        let mut tasks = Vec::new();
        if let Some(row) = focus {
            tasks.push(
                sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row))
                    .map(cosmic::Action::App),
            );
        }
        tasks.push(app.update(Message::Sidebar(SidebarAction::ToggleInProgress)));
        assert!(
            !app.sidebar_state.in_progress_collapsed(),
            "the fold waits for the probe"
        );
        let answers = run(&app, tasks);
        let [Message::FoldInProgress { refocus: answer }] = answers.as_slice() else {
            panic!("one answer: {answers:?}")
        };
        assert_eq!(*answer, refocus);
        let task = app.update(Message::FoldInProgress { refocus: *answer });
        assert!(app.sidebar_state.in_progress_collapsed());
        assert!(!attention_lists(&app, card_row));
        assert_eq!(
            focused(&app, task),
            refocus.then(sidebar::in_progress_focus_id),
            "focus={focus:?}"
        );
        // Unfolding is immediate.
        drop(app.update(Message::Sidebar(SidebarAction::ToggleInProgress)));
        assert!(!app.sidebar_state.in_progress_collapsed());
    }

    // Two quick presses leave the section as it was, though the first
    // press's probe answers after the second press.
    let first = app.update(Message::Sidebar(SidebarAction::ToggleInProgress));
    let second = app.update(Message::Sidebar(SidebarAction::ToggleInProgress));
    let answers = run(&app, vec![first, second]);
    assert_eq!(answers.len(), 1, "only the fold waits on a probe");
    for answer in answers {
        drop(app.update(answer));
    }
    assert!(!app.sidebar_state.in_progress_collapsed());
    assert!(attention_lists(&app, card_row));
}

fn agent_activity(state: crate::agent::ActivityState) -> Option<crate::agent::Activity> {
    Some(crate::agent::Activity {
        provider: crate::agent::Provider::Claude,
        state,
    })
}

/// Dealing with a row in the Attention list, by reading it or by
/// answering its waiting agent, leaves the row where the pointer left it
/// until the user hides read rows or chooses another filter.
#[test]
fn attention_keeps_channels_the_user_dealt_with_until_hidden() {
    use crate::agent::ActivityState;
    use crate::sidebar::SidebarFilter;

    let (mut app, card, channel) = attention_channel_test_app();
    let hide = || Message::Sidebar(SidebarAction::HideReadAttention);
    app.observe_agent_update_for_card(card);
    app.refresh_sidebar_channels();
    assert!(attention_lists_channel(&app, channel));
    drop(app.update(Message::Sidebar(SidebarAction::MarkChannelRead(channel))));
    assert!(!app.sidebar_state.has_card_update(card));
    assert!(
        attention_lists_channel(&app, channel),
        "a channel read under Attention stays listed"
    );
    let task = app.update(hide());
    assert!(
        !attention_lists_channel(&app, channel),
        "hiding read rows drops it"
    );
    assert_eq!(app.sidebar_state.filter(), SidebarFilter::Attention);
    assert_eq!(
        focused(&app, task),
        Some(sidebar::attention_filter_focus_id()),
        "focus leaves the control hiding removed for the filter it served"
    );

    // A wait the user already read elsewhere lists the channel on its own.
    drop(app.update(Message::Sidebar(SidebarAction::SetFilter(
        SidebarFilter::All,
    ))));
    app.observe_agent_activity_for_card(card, agent_activity(ActivityState::Waiting));
    app.read_card_update(card);
    drop(app.update(Message::Sidebar(SidebarAction::SetFilter(
        SidebarFilter::Attention,
    ))));
    assert!(attention_lists_channel(&app, channel));
    app.observe_agent_activity_for_card(card, agent_activity(ActivityState::Working));
    app.refresh_sidebar_channels();
    assert!(
        attention_lists_channel(&app, channel),
        "answering a waiting agent keeps its channel"
    );
    drop(app.update(hide()));
    assert_eq!(
        sidebar::visible_selection_rows(&app.sidebar_model, &app.sidebar_state, false),
        vec![sidebar::SelectionRow::Channel(channel)],
        "hidden, the working channel is only in progress"
    );
}

/// A waiting agent that disconnects, or whose chat goes away, was not
/// answered: its row leaves the Attention list with the wait.
#[test]
fn attention_drops_a_waiting_chat_that_disconnects_or_closes() {
    use crate::agent::ActivityState;

    let (mut app, card, channel) = attention_channel_test_app();
    // The user is looking at the pane, so its wait is not also unread
    // news: only the wait lists the channel.
    app.refresh_ui.focused = true;
    let ends: [fn(&mut Cagents, CardId); 3] = [
        |app, card| app.shared_chat_detach(card),
        |app, card| app.chat_forget(card),
        |app, card| {
            app.sidebar_state.forget_card_attention(card);
            app.chat_forget(card);
        },
    ];
    for (index, end) in ends.into_iter().enumerate() {
        app.observe_agent_activity_for_card(card, agent_activity(ActivityState::Waiting));
        app.refresh_sidebar_channels();
        assert!(!app.sidebar_state.has_card_update(card), "path {index}");
        assert!(attention_lists_channel(&app, channel), "path {index}");
        end(&mut app, card);
        app.refresh_sidebar_channels();
        // A kept row would stay listed after its wait ended.
        assert!(
            !attention_lists_channel(&app, channel),
            "path {index} was not an answer"
        );
    }
}

#[test]
fn an_agent_finishing_its_turn_marks_its_project_for_a_read() {
    use crate::agent::{Activity, ActivityState, Provider};
    use crate::model::{ChannelId, ProjectChannel, WorktreeId};
    use crate::sidebar::{SidebarProject, SidebarWorktree};

    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let project = ProjectId::new();
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "project".into(),
        "/tmp".into(),
        vec![SidebarWorktree::from_durable(
            WorktreeId::new(),
            "/tmp".into(),
            Some("main".into()),
        )],
        Vec::new(),
    ));
    let activity = |state| {
        Some(Activity {
            provider: Provider::Claude,
            state,
        })
    };
    app.observe_agent_activity_for_card(card, activity(ActivityState::Working));
    app.observe_agent_activity_for_card(card, activity(ActivityState::Waiting));
    assert!(!app.refresh_ui.is_stale(project));
    app.observe_agent_activity_for_card(card, activity(ActivityState::Working));
    app.observe_agent_activity_for_card(card, activity(ActivityState::Idle));
    assert!(app.refresh_ui.is_stale(project));

    // A channel's agent works in its project without a worktree row.
    let channel_project = ProjectId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(ProjectChannel {
            title_source: Default::default(),
            id: ChannelId::new(),
            project_id: channel_project,
            title: "channel".into(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: None,
        });
    app.observe_agent_activity_for_card(card, activity(ActivityState::Working));
    app.observe_agent_activity_for_card(card, activity(ActivityState::Idle));
    assert!(app.refresh_ui.is_stale(channel_project));
}

/// A refresh tick reads one project at a time, only while the window is
/// focused, and leaves a project whose checkout failed to its retry.
#[test]
fn explicit_picker_reads_prevent_focus_refresh_until_failure_or_cancellation() {
    for cancelled in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().canonicalize().unwrap();
        let project = ProjectId::new();
        let mut app = context_inspector_test_app(false);
        app.sidebar_model
            .projects
            .push(restored_sidebar_project(project, "Existing", &repository));
        app.sidebar_state.apply(&SidebarAction::OpenRepository {
            project_id: project,
            repository: repository.clone(),
        });
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        let picked = ProjectId::new();
        drop(app.repository_load_task(repository.clone(), Some(picked), true));
        let generation = app.repository_generations[&picked];
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        assert!(
            !app.repository_generations.contains_key(&project),
            "focus must not outrank the unresolved picker"
        );
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
        assert!(
            !app.repository_generations.contains_key(&project),
            "periodic polling must also defer during a slow picker read"
        );
        if cancelled {
            app.refresh_ui.projects[&picked].cancel();
        } else {
            drop(app.update(Message::RepositoryLoaded {
                open_cards: true,
                project_id: picked,
                generation,
                result: Err("picker failed".into()),
            }));
        }
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
        assert!(
            app.repository_generations.contains_key(&project),
            "normal refresh must resume after the open settles"
        );
        assert!(!app.refresh_ui.projects.contains_key(&picked));
    }
}

#[test]
fn refresh_ticks_read_one_reachable_project_at_a_time_while_focused() {
    use crate::model::WorktreeId;
    use crate::sidebar::{SidebarProject, SidebarWorktree};

    let mut app = context_inspector_test_app(false);
    let projects = [ProjectId::new(), ProjectId::new(), ProjectId::new()];
    for (index, &project) in projects.iter().enumerate() {
        let root = PathBuf::from(format!("/nonexistent/project-{index}"));
        let mut row = SidebarProject::new(
            project,
            RepositoryId::new(),
            format!("project {index}"),
            root.clone(),
            vec![SidebarWorktree::from_durable(
                WorktreeId::new(),
                root,
                Some("main".into()),
            )],
            Vec::new(),
        );
        if index == 0 {
            row.unreachable = Some("fixture".into());
        }
        app.sidebar_model.projects.push(row);
    }
    let tick = |app: &mut Cagents| drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
    tick(&mut app);
    assert!(
        app.refresh_ui.projects.is_empty(),
        "unfocused windows read nothing"
    );

    app.core.set_main_window_id(Some(window::Id::RESERVED));
    drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
        window::Id::RESERVED,
        true,
    ))));
    let reading = app.refresh_ui.projects.keys().copied().collect::<Vec<_>>();
    assert_eq!(reading.len(), 1);
    assert_ne!(reading[0], projects[0]);
    tick(&mut app);
    assert_eq!(
        app.refresh_ui.projects.keys().copied().collect::<Vec<_>>(),
        reading
    );
    // Once that read lands, the next tick moves on to the other one.
    app.refresh_ui.projects.clear();
    tick(&mut app);
    let next = app.refresh_ui.projects.keys().copied().collect::<Vec<_>>();
    assert_eq!(next.len(), 1);
    assert!(!next.contains(&reading[0]) && !next.contains(&projects[0]));
}

#[test]
fn restored_duplicate_projects_preserve_worktrees_and_writer_generations() {
    let directory = tempfile::tempdir().unwrap();
    let common = directory.path().join(".git");
    let alias = directory.path().join("alias");
    std::fs::create_dir(&common).unwrap();
    std::os::unix::fs::symlink(&common, &alias).unwrap();
    let original = ProjectState {
        kind: Default::default(),
        id: ProjectId::new(),
        generation: 7,
        name: "original".into(),
        repository: common.canonicalize().unwrap(),
        existence: Existence::Open,
    };
    let duplicate = ProjectState {
        kind: Default::default(),
        id: ProjectId::new(),
        generation: 3,
        repository: alias,
        ..original.clone()
    };
    let mut projects = vec![duplicate.clone(), original.clone()];
    let mut worktrees = [original.id, duplicate.id].map(|project_id| WorktreeState {
        id: crate::model::WorktreeId::new(),
        project_id,
        generation: 9,
        path: directory.path().join(project_id.to_string()),
        branch: Some("main".into()),
        existence: Existence::Open,
    });
    let expected = worktrees.clone().map(|mut worktree| {
        worktree.project_id = original.id;
        worktree
    });
    reconcile_restored_projects(&mut projects, &mut worktrees, &[original.id]);
    assert_eq!(projects, vec![original]);
    assert_eq!(worktrees, expected);
}

#[test]
fn folder_projects_participate_in_workspace_history_alongside_git_and_channels() {
    let mut app = context_inspector_test_app(false);
    let folder = ProjectId::new();
    let git = ProjectId::new();
    let worktree = crate::model::WorktreeId::new();
    let channel = crate::model::ChannelId::new();
    *app.sidebar_model.projects = vec![
        SidebarProject::new(
            folder,
            RepositoryId::new(),
            "Folder".into(),
            "/folder".into(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
        SidebarProject::new(
            git,
            RepositoryId::new(),
            "Git".into(),
            "/git/.git".into(),
            vec![SidebarWorktree::from_durable(
                worktree,
                "/git".into(),
                Some("main".into()),
            )],
            Vec::new(),
        ),
    ];
    app.sidebar_model.projects[0]
        .channels
        .push(sidebar::SidebarChannel::new(
            channel,
            "Channel".into(),
            false,
            None,
            false,
            1,
            true,
            1,
        ));
    let destinations = sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state);
    let kinds: Vec<_> = destinations
        .iter()
        .filter_map(workspace_destination)
        .collect();
    assert_eq!(
        kinds,
        vec![
            WorkspaceDestination::Folder(folder),
            WorkspaceDestination::Channel(channel),
            WorkspaceDestination::Worktree(worktree)
        ]
    );
    app.workspace_history
        .observe(Some(WorkspaceDestination::Channel(channel)));
    app.workspace_history
        .observe(Some(WorkspaceDestination::Folder(folder)));
    app.workspace_history
        .observe(Some(WorkspaceDestination::Worktree(worktree)));
    app.active_workspace = Some("/git".into());
    let previous = app
        .next_workspace(crate::workspace::FocusDirection::Right)
        .unwrap();
    assert_eq!(
        workspace_destination(&previous),
        Some(WorkspaceDestination::Folder(folder))
    );
    app.active_workspace = Some("/folder".into());
    assert_eq!(
        app.active_workspace_destination(),
        Some(WorkspaceDestination::Folder(folder))
    );
    app.sidebar_model.projects.remove(0);
    assert!(
        app.next_workspace(crate::workspace::FocusDirection::Right)
            .is_some()
    );
}

#[test]
fn folder_projects_select_a_directory_scope_and_keep_their_mode_on_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let loaded = load_project(&root, None, CancellationToken::default(), None).unwrap();
    assert_eq!(loaded.project.kind, ProjectKind::Folder);
    assert!(loaded.repository.is_none());
    assert!(loaded.worktrees.is_empty());
    let project = loaded.project.id;
    let mut app = context_inspector_test_app(false);
    app.repository_generations.insert(project, 1);
    drop(app.update(repository_load_result(project, Ok(loaded))));
    drop(app.select_worktree(root.clone(), false));
    assert_eq!(app.active_workspace.as_ref(), Some(&root));
    assert!(app.active_folder_scope());
    assert_eq!(app.workspace_view(), context_sections::WorkspaceView::Files);
    let key = app.sidebar_model.worktree_at(&root).unwrap();
    assert!(app.sidebar_model.contains_worktree(&key));
    assert_eq!(key.project_id, project);
    let card = CardId::new();
    drop(app.attach_repository_pane(
        card,
        RepositoryView {
            selected_file: None,
            project,
            repository: root.clone(),
            worktree: root.clone(),
        },
    ));
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime))
        if runtime.is_directory_only())
    );
    drop(
        app.update(Message::ContextSection(context_sections::Action::Show(
            context_sections::WorkspaceView::SourceControl,
        ))),
    );
    assert_eq!(app.workspace_view(), context_sections::WorkspaceView::Files);

    std::fs::create_dir(root.join(".git")).unwrap();
    let refreshed = load_project(
        &root,
        Some(project),
        CancellationToken::default(),
        Some(ProjectKind::Folder),
    )
    .unwrap();
    assert_eq!(refreshed.project.kind, ProjectKind::Folder);
    assert_eq!(refreshed.project.repository, root);
    assert!(load_project(&root, None, CancellationToken::default(), None).is_err());
}

#[test]
fn repeated_repository_add_reconciles_aliases_and_concurrent_linked_loads() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    let linked = directory.path().join("linked");
    let alias = directory.path().join("alias");
    let git = |cwd: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(directory.path(), &["init", "-b", "main", "checkout"]);
    git(&root, &["commit", "--allow-empty", "-m", "initial"]);
    git(
        &root,
        &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    std::fs::create_dir(root.join("subdir")).unwrap();
    std::os::unix::fs::symlink(&root, &alias).unwrap();

    // These independent requests all finish before any result is accepted,
    // as when two picker loads race while another add is still in flight.
    let first = load_repository(&root, None, CancellationToken::default()).unwrap();
    let aliased =
        load_repository(&alias.join("subdir"), None, CancellationToken::default()).unwrap();
    let linked_load = load_repository(&linked, None, CancellationToken::default()).unwrap();
    assert_eq!(
        first.repository.as_ref().unwrap().id,
        aliased.repository.as_ref().unwrap().id
    );
    assert_eq!(
        first.repository.as_ref().unwrap().id,
        linked_load.repository.as_ref().unwrap().id
    );
    let canonical_root = root.canonicalize().unwrap();
    let canonical_linked = linked.canonicalize().unwrap();
    assert_eq!(
        aliased.repository.as_ref().unwrap().worktree_root.as_ref(),
        Some(&canonical_root)
    );
    assert_eq!(
        linked_load
            .repository
            .as_ref()
            .unwrap()
            .worktree_root
            .as_ref(),
        Some(&canonical_linked)
    );

    let mut app = context_inspector_test_app(false);
    let cards = [&canonical_root, &canonical_linked]
        .into_iter()
        .map(|path| {
            let mut card = terminal_card_state();
            if let CardKind::Terminal(session) = &mut card.kind {
                session.launch.working_directory = path.clone();
                session.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: session.launch.clone(),
                    last_exit: None,
                };
            }
            app.cards.insert(card.id, CardRuntime::Loading);
            card
        })
        .collect::<Vec<_>>();
    let card_ids = cards.iter().map(|card| card.id).collect::<Vec<_>>();
    Arc::make_mut(&mut app.durable).cards = cards.into();
    let first_id = first.project.id;
    for loaded in [&first, &aliased, &linked_load] {
        app.repository_generations.insert(loaded.project.id, 1);
    }
    let complete = |loaded: LoadedRepository, generation| Message::RepositoryLoaded {
        open_cards: true,
        project_id: loaded.project.id,
        generation,
        result: Ok(Box::new(loaded)),
    };
    drop(app.update(complete(first.clone(), 1)));
    app.sidebar_model.projects[0].github_status = "newer pending refresh".into();
    let expected_project = app.sidebar_model.projects[0].clone();
    for loaded in [aliased, linked_load] {
        drop(app.update(complete(loaded, 1)));
        assert_eq!(*app.sidebar_model.projects, vec![expected_project.clone()]);
        assert_eq!(
            app.durable
                .cards
                .iter()
                .map(|card| card.id)
                .collect::<Vec<_>>(),
            card_ids
        );
    }
    assert_eq!(app.active_workspace.as_ref(), Some(&canonical_linked));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card_ids[1]]);

    // Re-adding is selection, not refresh; neither it nor a stale explicit
    // refresh may clear newer project data or change its generation fence.
    app.repository_generations.insert(first_id, 2);
    drop(app.update(Message::RepositorySelected(Ok(root))));
    assert_eq!(app.repository_generations[&first_id], 2);
    drop(app.update(complete(first, 1)));
    assert_eq!(*app.sidebar_model.projects, vec![expected_project]);
    assert_eq!(app.active_workspace.as_ref(), Some(&canonical_linked));
}

#[test]
fn a_later_launch_can_request_focus_without_a_mapped_window() {
    let mut app = context_inspector_test_app(false);
    let _ = app.update(Message::ActivateUi { token: None });
    let _ = app.update(Message::ActivateUi {
        token: Some("wayland-token".into()),
    });
}

#[test]
fn restored_terminal_retains_its_fraction_when_a_second_pane_opens() {
    let mut card = terminal_card_state();
    card.width_fraction = 0.125;
    let card_id = card.id;
    let durable = WorkspaceState {
        cards: vec![card].into(),
        focused: Some(card_id),
        ..WorkspaceState::default()
    };

    let mut workspace = Cagents::rebuild_workspace(&durable);
    workspace.resize_viewport(3_840.0);

    assert_eq!(
        workspace.card(card_id).unwrap().width,
        CardWidth::Fraction(0.125)
    );
    assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 3_840.0);
    workspace.resize_viewport(800.0);
    assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 800.0);
    workspace.resize_viewport(3_840.0);
    assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 3_840.0);
    workspace
        .add_card(
            WorkspaceCard::new(CardId::new(), "Second", CardWidth::Fraction(0.5)).unwrap(),
            1,
            false,
        )
        .unwrap();
    assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 768.0);
    assert_eq!(
        workspace.card(card_id).unwrap().width,
        CardWidth::Fraction(0.125)
    );
}
#[test]

fn terminal_resize_updates_rendered_and_durable_width_within_bounds() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.workspace.resize_viewport(1_000.0);
    app.workspace
        .add_card(
            WorkspaceCard::new(CardId::new(), "Second", CardWidth::Fraction(0.5)).unwrap(),
            1,
            false,
        )
        .unwrap();
    let limits = app.workspace.fractional_width_limits();

    let _ = app.update(Message::ResizeCard { card, width: 640.0 });
    assert_eq!(app.workspace.geometry().card(card).unwrap().width(), 640.0);
    assert_eq!(app.card_state(card).unwrap().width_fraction, 0.64);

    let _ = app.update(Message::ResizeCard { card, width: 10.0 });
    assert_eq!(
        app.workspace.geometry().card(card).unwrap().width(),
        limits.minimum
    );
    assert!((app.card_state(card).unwrap().width_fraction - 0.461_538_46).abs() < 0.000_001);

    let _ = app.update(Message::ResizeCard {
        card,
        width: 5_000.0,
    });
    assert_eq!(
        app.workspace.geometry().card(card).unwrap().width(),
        limits.maximum
    );
    assert_eq!(
        app.card_state(card).unwrap().width_fraction,
        limits.maximum / 1_000.0
    );
}

#[test]
fn pane_width_cycle_survives_serialization_and_scope_rebuilds_for_every_kind() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let mut terminal = app.card_state(card).unwrap().kind.clone();
    if let CardKind::Terminal(session) = &mut terminal {
        session.launch.working_directory = PathBuf::from("/repo");
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };
    }
    let url = Url::parse("about:blank").unwrap();
    let kinds = [
        terminal,
        CardKind::Repository(RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: PathBuf::from("/repo"),
            worktree: PathBuf::from("/repo"),
        }),
        CardKind::Browser(crate::model::BrowserSession {
            id: crate::model::BrowserId::new(),
            current_url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: crate::model::BrowserRestoreFidelity::ExactLive,
        }),
        CardKind::PullRequests(crate::model::PullRequestView {
            repository: PathBuf::from("/repo"),
            selected_number: None,
            file_panel_width: None,
            file_panel_collapsed: None,
        }),
    ];
    for kind in kinds {
        app.card_state_mut(card).unwrap().kind = kind;
        app.card_state_mut(card).unwrap().width_fraction = 0.61;
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        // Wide enough that every preset resolves to a readable width, so
        // the cycle visits all of them rather than skipping the narrow
        // ones a smaller strip could not show.
        app.workspace.resize_viewport(1500.0);
        for fraction in [2.0 / 3.0, 1.0, 1.0 / 3.0, 0.5, 2.0 / 3.0] {
            let _ = app.update(Message::CyclePaneWidth);
            let saved = serde_json::to_vec(app.durable.as_ref()).unwrap();
            app.durable = Arc::new(serde_json::from_slice(&saved).unwrap());
            let _ = app.select_worktree(PathBuf::from("/other"), false);
            let _ = app.select_worktree(PathBuf::from("/repo"), false);
            assert_eq!(app.workspace.focused(), Some(card));
            assert_eq!(app.card_state(card).unwrap().width_fraction, fraction);
            assert_eq!(
                app.workspace.card(card).unwrap().width,
                CardWidth::Fraction(fraction)
            );
            assert_eq!(app.workspace.geometry().card(card).unwrap().width(), 1500.0);
        }
    }
}

#[test]
fn pointer_activation_keeps_the_clicked_child_keyboard_focus() {
    let mut app = context_inspector_test_app(true);
    let first = app.workspace.focused().unwrap();
    let second = WorkspaceCard::new(CardId::new(), "Editor", CardWidth::Fraction(0.5)).unwrap();
    let id = second.id;
    app.workspace.add_card(second, 1, false).unwrap();
    app.workspace.take_widget_focus_request();
    let (update, _) = update_workspace_viewport(
        &mut app.workspace,
        WorkspaceViewportEvent::PointerFocused {
            card: id,
            primary: true,
        },
    );
    assert_eq!(app.workspace.focused(), Some(id));
    assert_eq!(app.workspace.take_widget_focus_request(), None);
    let _ = app.apply_viewport(update, true, DurableChange::None);
    assert_ne!(app.workspace.focused(), Some(first));
}

#[test]
fn notification_activation_unminimizes_and_focuses_actual_x11_window() {
    use cosmic::app::cosmic::WindowingSystem;
    use iced::runtime::{Action, window::Action as WindowAction};
    let mut app = context_inspector_test_app(true);
    let window = window::Id::RESERVED;
    app.core.set_main_window_id(Some(window));
    for backend in [
        Some(WindowingSystem::Xlib),
        Some(WindowingSystem::Xcb),
        None,
    ] {
        let task = app.activate_main_window_on_backend(backend, Some("portal-token".into()));
        let stream = iced::runtime::task::into_stream(task).unwrap();
        let effects = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(stream.collect::<Vec<_>>());
        assert_eq!(effects.len(), 2);
        assert!(
            matches!(effects[0], Action::Window(WindowAction::Minimize(id, false)) if id == window)
        );
        assert!(matches!(effects[1], Action::Window(WindowAction::GainFocus(id)) if id == window));
    }
}

#[test]
fn notification_activation_keeps_actual_wayland_token_and_refusal_semantics() {
    use cosmic::app::cosmic::WindowingSystem;
    let mut app = context_inspector_test_app(true);
    app.core.set_main_window_id(Some(window::Id::RESERVED));
    for token in [Some("portal-token".to_owned()), None] {
        let expected = token.clone().unwrap_or_default();
        let task = app.activate_main_window_on_backend(Some(WindowingSystem::Wayland), token);
        let stream = iced::runtime::task::into_stream(task).unwrap();
        let effects = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(stream.collect::<Vec<_>>());
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(&effects[0], iced::runtime::Action::Output(cosmic::Action::Cosmic(cosmic::app::Action::Activate(actual))) if actual == &expected)
        );
    }
    app.core.set_main_window_id(None);
    assert!(
        iced::runtime::task::into_stream(app.activate_main_window_on_backend(
            Some(WindowingSystem::Wayland),
            Some("portal-token".into())
        ))
        .is_none()
    );
}

#[test]
fn save_and_close_prepares_original_read_custody_without_prior_save() {
    if crate::repository_widget::retained_save_tests::isolated_test(
        "app::tests::save_and_close_prepares_original_read_custody_without_prior_save",
    ) {
        return;
    }
    for overwrite in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let path = checkout.join("file.txt");
        std::fs::write(&path, "baseline").unwrap();
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
            project: ProjectId::new(),
            repository: checkout.clone(),
            worktree: checkout.clone(),
            selected_file: None,
        });
        runtime.set_auto_save(false);
        let open = runtime.update(RepositoryAction::OpenFile {
            card,
            path: "file.txt".into(),
        });
        assert!(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(runtime.settle_document_read(open)),
            "actual file read callback"
        );
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Insert(
                '!',
            )),
        }));
        let expected = runtime.editor_text().unwrap();
        assert!(runtime.is_dirty());
        let mut app = context_inspector_test_app(false);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        app.dirty_repository_close = Some(DirtyRepositoryClose {
            card,
            remaining: VecDeque::new(),
            intent: DirtyCloseIntent::Card(card),
            focused: DirtyCloseControl::Save,
            restore_card: None,
        });
        let task = app.update(if overwrite {
            Message::OverwriteAndCloseRepository(card)
        } else {
            Message::SaveAndCloseRepository(card)
        });
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving())
        );
        assert!(app.operation_recovery_pending());
        let completed = task_app_message(task);
        assert!(matches!(
            &completed,
            Message::Repository(RepositoryAction::RetainedSaveFinished { result: Ok(_), .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert!(!app.operation_recovery_pending());
        // Completion is intentionally not routed into card-close effects;
        // this regression owns Save admission/publication from the close dialog.
    }
}

/// Drives a repository load through the app as the shell would. A read
/// the runtime discards because its watch saw an event, including one
/// from before the read was armed, is followed by the re-read it starts.
/// Returns whether the real file worker answered, and the remaining
/// follow-up tasks for the test to run or drop.
pub(super) fn settle_repository_read(
    app: &mut Cagents,
    card: CardId,
    load: iced::Task<RepositoryAction>,
) -> (bool, Vec<Task<Message>>) {
    // A fixture whose file keeps changing fails instead of hanging.
    const REREAD_LIMIT: usize = 16;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    fn runtime(app: &Cagents, card: CardId) -> &RepositoryRuntime {
        match app.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime,
            _ => panic!("the repository runtime must stay attached"),
        }
    }
    let mut answered = false;
    let mut rereads = 0;
    let mut remaining = Vec::new();
    let mut pending = vec![repository_task(load)];
    while let Some(task) = pending.pop() {
        let Some(mut stream) = iced::runtime::task::into_stream(task) else {
            continue;
        };
        while let Some(action) = executor.block_on(stream.next()) {
            let iced::runtime::Action::Output(cosmic::Action::App(Message::Repository(action))) =
                action
            else {
                continue;
            };
            let read = runtime(app, card).accepts_document_read(&action);
            answered |= read;
            let follow_up = app.update(Message::Repository(action));
            if read && runtime(app, card).rereading_document() {
                rereads += 1;
                assert!(
                    rereads <= REREAD_LIMIT,
                    "the document kept changing while it was read"
                );
                pending.push(follow_up);
            } else {
                remaining.push(follow_up);
            }
        }
    }
    (answered, remaining)
}

pub(super) fn task_app_message(task: Task<Message>) -> Message {
    let mut stream = iced::runtime::task::into_stream(task).expect("task has an output");
    let action = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(stream.next())
        .expect("task produced an action");
    match action {
        iced::runtime::Action::Output(cosmic::Action::App(message)) => message,
        _ => panic!("task did not produce an application message"),
    }
}

#[test]
fn moving_the_camera_and_exposing_panes_leave_the_workspace_uncopied() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.set_exposure(card, Exposure::Visible);
    // The writer holds the last snapshot, as it does between saves.
    let held = Arc::clone(&app.durable);
    drop(app.apply_viewport(
        ViewportUpdate {
            offset_changed: true,
            transitions: vec![VisibilityTransition {
                id: card,
                from: VisibilityClass::Visible,
                to: VisibilityClass::Offscreen,
            }],
        },
        false,
        DurableChange::None,
    ));
    assert_eq!(app.exposure(card), Exposure::Offscreen);
    assert!(Arc::ptr_eq(&held, &app.durable));
}

#[test]
fn a_change_of_focus_is_kept_in_the_workspace() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    Arc::make_mut(&mut app.durable).focused = None;
    let held = Arc::clone(&app.durable);
    drop(app.apply_viewport(ViewportUpdate::unchanged(), false, DurableChange::None));
    assert_eq!(app.durable.focused, Some(card));
    assert_eq!(held.focused, None);
}

#[test]
fn restored_offscreen_terminal_waits_for_lifecycle_hydration() {
    let state = terminal_card_state();
    let card = state.id;
    let expected_session = match &state.kind {
        CardKind::Terminal(session) => session.clone(),
        _ => unreachable!(),
    };

    let restored = restored_runtime_residency(&state.kind, Exposure::Offscreen, &Residency::Cold);
    assert_eq!(restored, Residency::Hibernated);

    let now = Instant::now();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget::default());
    lifecycle.register(card, Exposure::Offscreen, restored, now);
    assert!(
        lifecycle
            .reconcile(&HashSet::new(), &HashSet::new(), now)
            .iter()
            .all(|action| !matches!(action, LifecycleAction::Hydrate { .. }))
    );

    let actions = lifecycle.reconcile(&HashSet::from([card]), &HashSet::new(), now);
    assert!(
        actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card: id, .. } if *id == card)
        )
    );
    assert_eq!(
        deferred_terminal_session(Some(&state), true),
        Some(expected_session)
    );
    assert!(deferred_terminal_session(Some(&state), false).is_none());
}

#[test]
fn initial_terminal_hydration_prioritizes_visible_cards_within_budget() {
    let visible = CardId::new();
    let prefetch = CardId::new();
    let now = Instant::now();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: 1,
        offscreen_grace: Duration::ZERO,
    });
    lifecycle.register(visible, Exposure::Visible, Residency::Hibernated, now);
    lifecycle.register(prefetch, Exposure::Prefetch, Residency::Hibernated, now);

    let actions = lifecycle.reconcile(&HashSet::from([visible]), &HashSet::from([prefetch]), now);
    assert!(
        actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == visible)
        )
    );
    assert!(actions.iter().all(
        |action| !matches!(action, LifecycleAction::Hydrate { card, .. } if *card == prefetch)
    ));
}

#[test]
fn restored_static_cards_do_not_block_visible_terminal_admission() {
    let static_kind = CardKind::Repository(RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repository"),
        worktree: PathBuf::from("/repository/worktree"),
    });
    let now = Instant::now();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: 1,
        offscreen_grace: Duration::ZERO,
    });
    for static_card in (0..8).map(|_| CardId::new()) {
        if let Some(residency) =
            restored_terminal_residency(&static_kind, Exposure::Visible, &Residency::Hot)
        {
            lifecycle.register(static_card, Exposure::Visible, residency, now);
        }
    }

    let terminal = terminal_card_state();
    let residency = restored_terminal_residency(
        &terminal.kind,
        Exposure::Visible,
        &Residency::Hydrating { epoch: 1 },
    )
    .unwrap();
    lifecycle.register(terminal.id, Exposure::Visible, residency, now);
    let actions = lifecycle.reconcile(&HashSet::from([terminal.id]), &HashSet::new(), now);

    assert!(actions.iter().any(
        |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == terminal.id)
    ));
}

#[test]
fn late_terminal_start_is_rejected_after_card_close() {
    let mut state = terminal_card_state();
    assert!(can_accept_terminal_start(Some(&state), true, true));

    state.existence = Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("test close".into()),
        },
    };
    assert!(!can_accept_terminal_start(Some(&state), true, true));
    assert!(!can_accept_terminal_start(Some(&state), false, true));
}

#[test]
fn completed_terminal_start_uses_current_exposure_for_visibility() {
    let mut app = context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.set_exposure(card, Exposure::Visible);
    assert_eq!(app.terminal_start_visibility(card), Some(true));

    app.set_exposure(card, Exposure::Prefetch);
    assert_eq!(app.terminal_start_visibility(card), Some(false));
    app.set_exposure(card, Exposure::Offscreen);
    assert_eq!(app.terminal_start_visibility(card), Some(false));
    assert_eq!(app.terminal_start_visibility(CardId::new()), None);
}

#[test]
fn failed_terminal_hydration_frees_budget_for_an_immediate_sweep() {
    let failed = CardId::new();
    let waiting = CardId::new();
    let now = Instant::now();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: 1,
        offscreen_grace: Duration::ZERO,
    });
    lifecycle.register(
        failed,
        Exposure::Visible,
        Residency::Hydrating { epoch: 7 },
        now,
    );
    lifecycle.register(waiting, Exposure::Visible, Residency::Hibernated, now);
    assert!(lifecycle.hydration_finished(
        failed,
        7,
        Err(RuntimeError {
            code: "start_failed".into(),
            message: "could not start".into(),
        }),
    ));

    let actions = lifecycle.reconcile(&HashSet::from([failed, waiting]), &HashSet::new(), now);
    assert!(
        actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == waiting)
        )
    );
}

#[cfg(feature = "integrated-browser")]
#[test]
fn browser_created_before_factory_is_requeued_for_hydration() {
    let mut app = context_inspector_test_app(false);
    let _ = app.add_browser(Url::parse("https://example.com").unwrap());
    let card = app.workspace.focused().unwrap();
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert!(matches!(
        app.lifecycle.state(card),
        Some((_, Residency::Hydrating { .. }))
    ));
    app.register_deferred_browsers();
    assert!(matches!(
        app.lifecycle.state(card),
        Some((_, Residency::Hibernated))
    ));
    assert_eq!(*app.residency(card), Residency::Hibernated);
}

#[cfg(feature = "integrated-browser")]
#[test]
fn restored_nonvisible_browser_waits_for_visible_lifecycle_hydration() {
    let url = Url::parse("about:blank").unwrap();
    let kind = CardKind::Browser(BrowserSession {
        id: BrowserId::new(),
        current_url: url.clone(),
        history: vec![url],
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: BrowserRestoreFidelity::ExactLive,
    });
    assert_eq!(
        restored_runtime_residency(&kind, Exposure::Offscreen, &Residency::Cold),
        Residency::Hibernated
    );
    assert_eq!(
        restored_runtime_residency(&kind, Exposure::Prefetch, &Residency::Hot),
        Residency::Hibernated
    );
    assert_eq!(
        restored_runtime_residency(&kind, Exposure::Visible, &Residency::Hot),
        Residency::Hibernated
    );

    let card = CardId::new();
    let state = CardState {
        id: card,
        generation: 0,
        title: "Browser".into(),
        width_fraction: 0.9,
        existence: Existence::Open,
        kind,
    };
    assert!(can_register_deferred_browser(&state, true));

    let now = Instant::now();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget::default());
    lifecycle.register(card, Exposure::Prefetch, Residency::Hibernated, now);
    assert!(
        lifecycle
            .reconcile(&HashSet::new(), &HashSet::new(), now)
            .iter()
            .all(|action| !matches!(action, LifecycleAction::Hydrate { .. }))
    );
    let actions = lifecycle.reconcile(&HashSet::from([card]), &HashSet::new(), now);
    assert!(
        actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card: id, .. } if *id == card)
        )
    );
}

#[test]
fn restored_terminal_and_browser_share_the_startup_hot_budget() {
    let terminal = CardId::new();
    let browser = CardId::new();
    let now = Instant::now();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: 1,
        offscreen_grace: Duration::ZERO,
    });
    lifecycle.register(
        terminal,
        Exposure::Visible,
        Residency::Hydrating { epoch: 1 },
        now,
    );
    lifecycle.register(browser, Exposure::Visible, Residency::Hibernated, now);

    let actions = lifecycle.reconcile(&HashSet::from([terminal, browser]), &HashSet::new(), now);
    assert!(actions.iter().all(
        |action| !matches!(action, LifecycleAction::Hydrate { card, .. } if *card == browser)
    ));
}

#[test]
fn crashed_browser_frees_budget_for_immediate_reconciliation() {
    let crashed = CardId::new();
    let waiting = CardId::new();
    let now = Instant::now();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: 1,
        offscreen_grace: Duration::ZERO,
    });
    lifecycle.register(crashed, Exposure::Visible, Residency::Hot, now);
    lifecycle.register(waiting, Exposure::Visible, Residency::Hibernated, now);
    lifecycle.register(
        crashed,
        Exposure::Visible,
        Residency::Failed {
            typed_error: RuntimeError {
                code: "browser_crashed".into(),
                message: "crashed".into(),
            },
            retryable: true,
        },
        now,
    );

    let actions = lifecycle.reconcile(&HashSet::from([crashed, waiting]), &HashSet::new(), now);
    assert!(
        actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == waiting)
        )
    );
}

#[test]
fn failed_eviction_rolls_back_to_retryable_hot_state() {
    let now = Instant::now();
    let card = CardId::new();
    let mut lifecycle = LifecycleManager::new(LifecycleBudget {
        max_hot_cards: 1,
        offscreen_grace: Duration::ZERO,
    });
    lifecycle.register(card, Exposure::Offscreen, Residency::Hot, now);
    let epoch = lifecycle
        .reconcile(&HashSet::new(), &HashSet::new(), now)
        .into_iter()
        .find_map(|action| match action {
            LifecycleAction::Hibernate {
                card: candidate,
                epoch,
            } if candidate == card => Some(epoch),
            _ => None,
        })
        .unwrap();

    assert_eq!(
        rollback_failed_hibernation(&mut lifecycle, card, epoch, now),
        Some(Residency::Hot)
    );
    assert!(
        lifecycle
            .reconcile(&HashSet::new(), &HashSet::new(), now)
            .iter()
            .any(|action| matches!(
                action,
                LifecycleAction::Hibernate {
                    card: candidate,
                    ..
                } if *candidate == card
            ))
    );
}

#[test]
fn control_tab_uses_history_and_respects_blocking_surfaces() {
    let mut app = context_inspector_test_app(false);
    let cards = vec![
        terminal_card_state(),
        terminal_card_state(),
        terminal_card_state(),
    ];
    let ids: Vec<_> = cards.iter().map(|card| card.id).collect();
    app.durable = Arc::new(WorkspaceState {
        focused: Some(ids[0]),
        cards: cards.into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    for id in &ids {
        drop(app.update(Message::FocusCard(*id)));
    }
    let next = || Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        },
        captured: true,
    };
    drop(app.update(next()));
    assert_eq!(app.workspace.focused(), Some(ids[1]));
    drop(app.update(next()));
    assert_eq!(app.workspace.focused(), Some(ids[0]));
    app.core.set_main_window_id(Some(window::Id::RESERVED));
    drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
        window::Id::RESERVED,
        false,
    ))));
    drop(app.update(next()));
    assert_eq!(app.workspace.focused(), Some(ids[2]));
    app.settings_reset_confirm = true;
    drop(app.update(next()));
    assert_eq!(app.workspace.focused(), Some(ids[2]));
}

#[test]
fn configured_command_takes_priority_over_the_editor_staging_shortcut() {
    let mut app = context_inspector_test_app(true);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let worktree = app.sidebar_state.active_worktree().unwrap().clone();
    let card = app.workspace.focused().unwrap();
    let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "notes.txt");
    runtime.descriptor.worktree = worktree.canonical_path.clone();
    assert!(runtime.selected_staging_selection(true).is_some());
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    let key = || Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("s".into()),
            modified_key: keyboard::Key::Character("S".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyS),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            text: None,
            repeat: false,
        },
        captured: false,
    };
    // The shortcut stages the selection within its own update.
    drop(app.update(key()));
    assert_eq!(
        app.source_control_mutations[&worktree.canonical_path].kind,
        MutationKind::Stage
    );
    assert!(!app.palette.is_open());
    app.source_control_mutations.clear();
    app.settings.keyboard_shortcuts.command_palette = settings::parse_chords("Ctrl+Shift+S");
    drop(app.update(key()));
    assert!(app.palette.is_open());
    assert!(app.source_control_mutations.is_empty(), "nothing is staged");
}

#[test]
fn ctrl_w_closes_the_focused_card_and_is_a_no_op_without_cards() {
    let mut workspace = Workspace::default();
    assert!(close_focused_message(&workspace).is_none());

    let card = CardId::new();
    workspace
        .add_card(
            WorkspaceCard::new(card, "Terminal", CardWidth::Fixed(320.0)).unwrap(),
            0,
            false,
        )
        .unwrap();
    assert!(matches!(
        close_focused_message(&workspace),
        Some(Message::CloseCard(id)) if id == card
    ));
}

#[test]
fn valid_settings_edits_request_persistence_and_unchanged_values_do_not() {
    let settings = AppSettings::default();
    assert_eq!(
        settings_update(
            &settings,
            SettingsField::TerminalFontSize,
            &settings.terminal_font_size.to_string(),
        ),
        SettingsDecision::Unchanged
    );
    let SettingsDecision::Persist(updated) =
        settings_update(&settings, SettingsField::TerminalFontSize, "16")
    else {
        panic!("a valid changed setting must request persistence");
    };
    assert_eq!(updated.terminal_font_size, 16.0);
    assert_eq!(updated.sidebar_width, settings.sidebar_width);
}

#[test]
fn account_usage_display_edits_apply_to_settings_and_draft() {
    let settings = AppSettings::default();
    let field = SettingsField::AccountUsageDisplay;
    assert_eq!(field.category(), SettingsCategory::Accounts);
    assert_eq!(
        settings_update(&settings, field, "left"),
        SettingsDecision::Unchanged
    );
    let SettingsDecision::Persist(updated) = settings_update(&settings, field, "used") else {
        panic!("changing account usage display must request persistence");
    };
    assert_eq!(
        updated.account_usage_display,
        settings::AccountUsageDisplay::Used
    );
    let mut draft = SettingsDraft::from_settings(&settings);
    draft.set(field, "used".into());
    assert_eq!(draft, SettingsDraft::from_settings(&updated));
    assert!(matches!(
        settings_update(&updated, field, "unknown"),
        SettingsDecision::Invalid(_)
    ));
    draft.set(field, "unknown".into());
    assert_eq!(draft, SettingsDraft::from_settings(&updated));
}

#[test]
fn shared_preferences_read_before_a_local_save_cannot_revert_it_after_acknowledgment() {
    use settings::AccountUsageDisplay::{Left, Used};
    use settings::SidebarWorktrees::{Expanded, Grouped, Hidden};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut app = context_inspector_test_app(false);
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    app.save_settings_now().unwrap();
    let stale = Message::SharedPreferencesObserved {
        generation: app.settings_ack_generation,
        settings: settings::load(&path).unwrap(),
    };
    let project = ProjectId::new();
    app.settings
        .sidebar_worktree_overrides
        .insert(project, Expanded);
    drop(app.update(Message::SettingsChanged {
        field: SettingsField::SidebarWorktrees,
        value: "hidden".into(),
    }));
    let task = app.update(Message::SettingsChanged {
        field: SettingsField::AccountUsageDisplay,
        value: "used".into(),
    });
    settle_settings_task(&mut app, task);
    assert_eq!(app.settings_save_generation, app.settings_ack_generation);
    let saved = settings::load(&path).unwrap();
    assert_eq!(saved.account_usage_display, Used);
    assert_eq!(saved.sidebar_worktrees, Hidden);

    // Deliver the old watcher output after the successful save. Neither
    // visible settings nor any baseline used by a later save may regress.
    drop(app.update(stale));
    assert_eq!(app.settings.account_usage_display, Used);
    assert_eq!(app.settings_draft.account_usage_display, Used);
    assert_eq!(app.settings_draft.sidebar_worktrees, Hidden);
    for settings in [&app.settings, &app.settings_base] {
        assert_eq!(settings.account_usage_display, Used);
        assert_eq!(settings.sidebar_worktrees, Hidden);
        assert_eq!(
            settings.sidebar_worktree_overrides.get(&project),
            Some(&Expanded)
        );
    }
    {
        let lane = app.settings_save_lane.lock().unwrap();
        let baseline = &lane.as_ref().unwrap().settings;
        assert_eq!(baseline.account_usage_display, Used);
        assert_eq!(baseline.sidebar_worktrees, Hidden);
        assert_eq!(
            baseline.sidebar_worktree_overrides.get(&project),
            Some(&Expanded)
        );
    }
    // A hidden default hides every project's checkouts, the project
    // with its own mode included; that mode is kept for when they show.
    assert_eq!(app.sidebar_state.worktree_display(project), Hidden);
    assert_eq!(app.sidebar_state.worktree_display(ProjectId::new()), Hidden);

    // The next click back to Left must persist a change, rather than look
    // unchanged because the stale observation already changed local state.
    let task = app.update(Message::SettingsChanged {
        field: SettingsField::AccountUsageDisplay,
        value: "left".into(),
    });
    settle_settings_task(&mut app, task);
    let saved = settings::load(&path).unwrap();
    assert_eq!(saved.account_usage_display, Left);
    assert_eq!(saved.sidebar_worktrees, Hidden);

    // A current-generation external observation is still accepted.
    let mut external = saved;
    external.sidebar_worktrees = Grouped;
    external.account_usage_display = Used;
    drop(app.update(Message::SharedPreferencesObserved {
        generation: app.settings_ack_generation,
        settings: external,
    }));
    assert_eq!(app.settings.account_usage_display, Used);
    assert_eq!(app.settings_draft.sidebar_worktrees, Grouped);
}

#[test]
fn account_usage_display_syncs_between_windows_and_keeps_local_edits_until_saved() {
    use settings::AccountUsageDisplay::{Left, Used};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let mut first = context_inspector_test_app(false);
    let mut second = context_inspector_test_app(false);
    for app in [&mut first, &mut second] {
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;
    }
    let task = first.update(Message::SettingsChanged {
        field: SettingsField::AccountUsageDisplay,
        value: "used".into(),
    });
    settle_settings_task(&mut first, task);
    let observed = settings::load(&path).unwrap();
    assert_eq!(observed.account_usage_display, Used);
    drop(second.update(Message::SharedPreferencesObserved {
        generation: second.settings_ack_generation,
        settings: observed,
    }));
    assert_eq!(second.settings.account_usage_display, Used);
    assert_eq!(second.settings_draft.account_usage_display, Used);
    assert_eq!(second.settings_base.account_usage_display, Used);

    // A later unrelated save must retain the observed preference, including
    // the baseline cached in this window's save lane.
    let task = second.update(Message::SettingsChanged {
        field: SettingsField::TerminalFontSize,
        value: "18".into(),
    });
    settle_settings_task(&mut second, task);
    assert_eq!(settings::load(&path).unwrap().account_usage_display, Used);
    let task = second.update(Message::SettingsChanged {
        field: SettingsField::AccountUsageDisplay,
        value: "left".into(),
    });
    drop(second.update(Message::SharedPreferencesObserved {
        generation: second.settings_ack_generation,
        settings: settings::load(&path).unwrap(),
    }));
    assert_eq!(second.settings.account_usage_display, Left);
    assert_eq!(second.settings_draft.account_usage_display, Left);
    settle_settings_task(&mut second, task);
    drop(first.update(Message::SharedPreferencesObserved {
        generation: first.settings_ack_generation,
        settings: settings::load(&path).unwrap(),
    }));
    assert_eq!(first.settings.account_usage_display, Left);
    assert_eq!(first.settings_draft.account_usage_display, Left);
    assert_eq!(first.settings_base.account_usage_display, Left);
    first.save_settings_now().unwrap();
    let saved = settings::load(&path).unwrap();
    assert_eq!(saved.account_usage_display, Left);
    assert_eq!(saved.terminal_font_size, 18.0);
}

#[test]
fn default_pane_preferences_update_independently_and_reject_invalid_choices() {
    let settings = AppSettings::default();
    for field in [
        SettingsField::DefaultWorktreePane,
        SettingsField::DefaultChannelPane,
    ] {
        assert_eq!(
            settings_update(&settings, field, "native_chat"),
            SettingsDecision::Unchanged
        );
        let SettingsDecision::Persist(updated) = settings_update(&settings, field, "terminal")
        else {
            panic!("changing the default pane must request persistence");
        };
        let mut draft = SettingsDraft::from_settings(&settings);
        draft.set(field, "terminal".into());
        assert_eq!(draft, SettingsDraft::from_settings(&updated));
        if field == SettingsField::DefaultWorktreePane {
            assert_eq!(
                updated.default_worktree_pane,
                settings::DefaultPane::Terminal
            );
            assert_eq!(
                updated.default_channel_pane,
                settings::DefaultPane::NativeChat
            );
        } else {
            assert_eq!(
                updated.default_worktree_pane,
                settings::DefaultPane::NativeChat
            );
            assert_eq!(
                updated.default_channel_pane,
                settings::DefaultPane::Terminal
            );
        }
        assert!(matches!(
            settings_update(&updated, field, "unknown"),
            SettingsDecision::Invalid(_)
        ));
        draft.set(field, "unknown".into());
        assert_eq!(draft, SettingsDraft::from_settings(&updated));
    }
}

#[test]
fn rapid_interface_scale_changes_use_the_last_preference_before_cosmic_catches_up() {
    let mut app = context_inspector_test_app(true);
    let original_width = app.window_width;
    // Deliberately do not execute the returned COSMIC actions: input can
    // arrive before the runtime has applied the previous scale action.
    let _ = app.apply_setting_input(SettingsField::UiScale, "1.25".into());
    let _ = app.apply_setting_input(SettingsField::UiScale, "1.5".into());
    assert!((app.window_width - original_width / 1.5).abs() < 0.01);
    let _ = app.apply_setting_input(SettingsField::UiScale, "NaN".into());
    assert!((app.window_width - original_width / 1.5).abs() < 0.01);
    assert_eq!(app.settings.ui_scale, 1.5);
    let _ = app.apply_setting_input(SettingsField::UiScale, "1.0".into());
    assert!((app.window_width - original_width).abs() < 0.01);
}

#[test]
fn settings_edits_report_field_validation_before_persistence() {
    let settings = AppSettings::default();
    assert!(matches!(
        settings_update(&settings, SettingsField::MaxHotCards, "0"),
        SettingsDecision::Invalid(message)
            if message == "Maximum hot panes must be a whole number, at least 1."
    ));
    assert!(matches!(
        settings_update(&settings, SettingsField::ManagedWorktreeDirectory, " "),
        SettingsDecision::Invalid(_)
    ));
    assert!(matches!(
        settings_update(&settings, SettingsField::DefaultColumnFraction, "not a number"),
        SettingsDecision::Invalid(message) if message.contains("number")
    ));
}

#[test]
fn refused_values_are_explained_with_the_label_and_range_the_pane_shows() {
    let settings = AppSettings::default();
    let refusal = |field, value: &str| match settings_update(&settings, field, value) {
        SettingsDecision::Invalid(message) => message,
        decision => panic!("{value:?} was not refused: {decision:?}"),
    };
    // Out of range and unparsable read the same: both say what fits.
    assert_eq!(
        refusal(SettingsField::TerminalScrollSize, "11"),
        "Scroll size must be 1–10 lines."
    );
    assert_eq!(
        refusal(SettingsField::TerminalScrollSize, "lots"),
        "Scroll size must be 1–10 lines."
    );
    assert_eq!(
        refusal(SettingsField::TerminalScrollbackMib, "1025"),
        "Scrollback must be 0–1,024 MiB."
    );
    assert_eq!(
        refusal(SettingsField::TerminalFontFamily, "F".repeat(129).as_str()),
        "Font family must be at most 128 bytes."
    );
    assert_eq!(
        refusal(SettingsField::ManagedWorktreeDirectory, " "),
        "Enter a directory for managed worktrees."
    );
    for message in [
        refusal(SettingsField::MaxHotCards, "0"),
        refusal(SettingsField::HibernationDelayMs, "-1"),
        refusal(SettingsField::ShortcutNewTerminal, "Ctrl+"),
    ] {
        assert!(!message.contains('_'), "{message} quotes a settings key");
        assert!(message.ends_with('.'), "{message} is not a sentence");
    }
}

#[test]
fn a_shortcut_clash_names_the_chord_and_the_action_that_holds_it() {
    let settings = AppSettings::default();
    let taken = settings.keyboard_shortcuts.new_channel[0].clone();
    // The edited action sorts before its rival here, so the table reports
    // the rival as the fault and the message must look past that.
    assert_eq!(
        settings_update(
            &settings,
            SettingsField::ShortcutNewTerminal,
            &format!("Ctrl+Alt+J, {taken}")
        ),
        SettingsDecision::Invalid(format!(
            "{taken} is already the shortcut for “New channel”."
        ))
    );
    let earlier = settings.keyboard_shortcuts.new_terminal[0].clone();
    assert_eq!(
        settings_update(&settings, SettingsField::ShortcutNewChannel, &earlier),
        SettingsDecision::Invalid(format!(
            "{earlier} is already the shortcut for “New terminal”."
        ))
    );
    assert_eq!(
        settings_update(
            &settings,
            SettingsField::ShortcutNewChannel,
            "Ctrl+Alt+K, Ctrl+Alt+J, ctrl+alt+j"
        ),
        SettingsDecision::Invalid("ctrl+alt+j is listed twice.".into())
    );
}

#[test]
fn shortcut_fields_follow_the_order_of_the_shortcut_table() {
    let settings = AppSettings::default();
    for (index, field) in SettingsField::SHORTCUTS.into_iter().enumerate() {
        let SettingsDecision::Persist(edited) =
            settings_update(&settings, field, "Ctrl+Alt+Shift+9")
        else {
            panic!("{field:?} did not take a free chord");
        };
        assert_eq!(
            edited.keyboard_shortcuts.actions()[index],
            &vec!["Ctrl+Alt+Shift+9".to_owned()],
            "{field:?}"
        );
        assert_eq!(field.category(), SettingsCategory::Keyboard);
    }
    assert_eq!(grouped_digits(1_000_000), "1,000,000");
    assert_eq!(grouped_digits(999), "999");
}

#[test]
fn dialog_transition_is_symmetric_and_reversible() {
    let started_at = Instant::now();
    let midpoint = started_at + DIALOG_TRANSITION_DURATION / 2;
    let finished = started_at + DIALOG_TRANSITION_DURATION;
    let opening = InspectorTransition {
        started_at,
        from: 0.0,
        to: 1.0,
    };
    let closing = InspectorTransition {
        started_at,
        from: 1.0,
        to: 0.0,
    };

    assert_eq!(opening.progress(started_at), 0.0);
    assert_eq!(opening.progress(midpoint), 0.5);
    assert_eq!(opening.progress(finished), 1.0);
    assert_eq!(closing.progress(started_at), 1.0);
    assert_eq!(closing.progress(midpoint), 0.5);
    assert_eq!(closing.progress(finished), 0.0);
    let reversal_started = started_at + DIALOG_TRANSITION_DURATION / 4;
    let reversal_from = opening.progress(reversal_started);
    let reversed = InspectorTransition {
        started_at: reversal_started,
        from: reversal_from,
        to: 0.0,
    };
    assert_eq!(reversed.progress(reversal_started), reversal_from);
    assert_eq!(
        reversed.progress(reversal_started + reversed.duration()),
        0.0
    );
}

#[test]
fn escape_dismisses_dialogs_only_on_the_initial_press() {
    let key = keyboard::Key::Named(keyboard::key::Named::Escape);
    let event = |repeat| keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key.clone(),
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat,
    };

    assert!(is_escape_press(&event(false)));
    assert!(!is_escape_press(&event(true)));
}

#[test]
fn a_sweep_is_rearmed_only_for_an_earlier_wakeup() {
    let now = Instant::now();
    let soon = now + Duration::from_secs(5);
    let later = now + Duration::from_secs(30);

    // Nothing pending: the request is the only wakeup there is.
    assert!(sweep_needs_rearming(None, later));
    // An earlier grace must pull the wakeup in, or the card that owns it
    // waits for a deadline belonging to a card that went offscreen later.
    assert!(sweep_needs_rearming(Some(later), soon));
    // A pending timer already covers a later request and re-arms itself for
    // the remainder, so a second timer would only duplicate it.
    assert!(!sweep_needs_rearming(Some(soon), later));
    assert!(!sweep_needs_rearming(Some(soon), soon));
}

#[test]
fn a_restored_title_is_normalised_before_it_reaches_any_surface() {
    // The state database predates the normaliser, and a process inside a
    // card can rewrite its own title before the write, so a restart must
    // not reinstate an unbounded or reordering one.
    let repository = |title: &str| CardState {
        id: CardId::new(),
        generation: 1,
        title: title.to_owned(),
        width_fraction: 0.7,
        existence: Existence::Open,
        kind: CardKind::Repository(RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: PathBuf::from("/repo"),
            worktree: PathBuf::from("/repo"),
        }),
    };
    let mut state = WorkspaceState {
        cards: vec![
            repository(&"A".repeat(20_000_000)),
            repository("safe\u{202e}gnitiops"),
            repository("\u{202e}\u{7}"),
        ]
        .into(),
        ..WorkspaceState::default()
    };

    normalize_restored_titles(&mut state);

    assert_eq!(
        state.cards[0].title.chars().count(),
        crate::workspace::MAX_CARD_NAME_CHARS
    );
    assert_eq!(state.cards[1].title, "safegnitiops");
    // Nothing legible survived, so the card is named for its kind rather
    // than losing its header, its accessible name and the window title.
    assert_eq!(state.cards[2].title, "Repository");
}

#[test]
fn a_path_outside_the_home_directory_is_shown_in_full() {
    // `~` is only an abbreviation when the prefix is actually the home
    // directory, and the home directory itself has no shorter tail.
    assert_eq!(display_path(Path::new("/etc/hosts")), "/etc/hosts");
    let home = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned());
    if let Some(home) = home {
        assert_eq!(display_path(&home), home.to_string_lossy());
        assert_eq!(display_path(&home.join("src/x")), "~/src/x");
    }
}

pub(super) fn discard_key() -> ActiveWorktreeKey {
    ActiveWorktreeKey {
        project_id: ProjectId::default(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: PathBuf::from("/repo/main"),
    }
}

pub(super) fn discard_request(confirmed: bool) -> crate::repository::DiscardRequest {
    crate::repository::DiscardRequest {
        worktree: PathBuf::from("/repo/main"),
        tracked: Vec::new(),
        untracked: vec![RepoPath::parse("notes.txt").expect("a repository path")],
        confirmed,
    }
}

#[test]
fn only_the_discard_confirmations_own_answers_survive_the_modal_guard() {
    // `Message::Sidebar` is refused while any dialog is up, because the
    // pane keeps its focus behind the overlay and an accessibility Click
    // is not captured at all. The discard confirmation is mounted by
    // `dialog` rather than by `sidebar::view`, so its Cancel and Discard
    // arrive down this same route: refusing them would strand the dialog
    // with no way out.
    for action in [
        SidebarAction::ConfirmDiscard {
            key: discard_key(),
            request: discard_request(true),
        },
        SidebarAction::CancelDiscard,
    ] {
        assert!(
            answers_discard_confirmation(&action),
            "{action:?} is one of the dialog's own two answers"
        );
        assert!(
            !user_workspace_message_blocked_while_modal(&Message::Sidebar(action.clone())),
            "{action:?} must also survive the deferred-navigation queue, which drops every \
                 message it decides a modal blocks"
        );
    }

    for action in [
        SidebarAction::RequestDiscard {
            key: discard_key(),
            request: discard_request(false),
        },
        SidebarAction::Stage {
            key: discard_key(),
            selection: Vec::new(),
        },
        SidebarAction::Commit {
            key: discard_key(),
            message: "wip".into(),
            amend: false,
        },
        SidebarAction::Refresh,
    ] {
        assert!(
            !answers_discard_confirmation(&action),
            "{action:?} is a pane command and must stand down behind a dialog"
        );
        assert!(user_workspace_message_blocked_while_modal(
            &Message::Sidebar(action)
        ));
    }
}

#[test]
fn discard_confirmation_starts_on_cancel_and_enter_does_not_admit_a_mutation() {
    let mut app = context_inspector_test_app(false);
    let opening = app.update(Message::Sidebar(SidebarAction::RequestDiscard {
        key: discard_key(),
        request: discard_request(false),
    }));
    let cancel = sidebar::discard_cancel_id();
    assert_eq!(
        focused_element(app.dialog().unwrap(), opening),
        Some(cancel.clone())
    );
    let message = dialog_ui::tests::enter_control(app.dialog().unwrap(), cancel);
    assert!(matches!(
        message,
        Message::Sidebar(SidebarAction::CancelDiscard)
    ));
    drop(app.update(message));
    assert!(app.sidebar_state.pending_discard().is_none());
    assert!(app.source_control_mutations.is_empty());
}

#[test]
fn discard_reexposed_after_transient_close_still_focuses_cancel() {
    let mut app = context_inspector_test_app(false);
    drop(app.update(Message::Sidebar(SidebarAction::RequestDiscard {
        key: discard_key(),
        request: discard_request(false),
    })));
    // The transient close owner is drawn above discard. Its actual Cancel
    // reducer must restore the lower confirmation's safe focus target.
    app.transient_work_close = Some(window::Id::unique());
    let cancel_upper = dialog_ui::tests::enter_control(
        app.dialog().unwrap(),
        widget::Id::new("dialog.transient-close.cancel"),
    );
    assert!(matches!(cancel_upper, Message::CancelTransientWorkClose));
    let restoring = app.update(cancel_upper);
    assert!(app.transient_work_close.is_none());
    let cancel = sidebar::discard_cancel_id();
    assert_eq!(
        focused_element(app.dialog().unwrap(), restoring),
        Some(cancel.clone())
    );
    let enter = dialog_ui::tests::enter_control(app.dialog().unwrap(), cancel);
    assert!(matches!(
        enter,
        Message::Sidebar(SidebarAction::CancelDiscard)
    ));
    drop(app.update(enter));
    assert!(app.sidebar_state.pending_discard().is_none());
    assert!(app.source_control_mutations.is_empty());
}

#[test]
fn a_pending_discard_reads_as_a_modal_and_mounts_its_own_confirmation() {
    let mut app = context_inspector_test_app(false);
    assert!(!app.modal_is_open());
    assert!(app.dialog().is_none());

    app.sidebar_state.apply(&SidebarAction::RequestDiscard {
        key: discard_key(),
        request: discard_request(false),
    });
    assert!(
        app.modal_is_open(),
        "the one dialog that is on screen because it is about to destroy work must not be \
             the one dialog the pane keeps its commands live behind"
    );
    assert!(
        app.dialog().is_some(),
        "a pending discard is mounted by `dialog`; nothing else mounts it"
    );

    app.sidebar_state.apply(&SidebarAction::CancelDiscard);
    assert!(!app.modal_is_open());
    assert!(app.dialog().is_none());
}

/// Every accessible label the dialog the runtime mounts exposes.
///
/// libcosmic's `view_main` puts `Application::dialog` in a popover popup,
/// and a popup is an overlay, so walking the frame's widget tree never
/// reaches it. The dialog element itself is what the runtime asks for, so
/// that is what these tests read.
fn dialog_labels(app: &Cagents, renderer: &cosmic::Renderer) -> Vec<String> {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    let mut element = app.dialog().expect("a mounted dialog");
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
    );
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| node.node().label().map(str::to_owned))
        .collect()
}

/// A project with one deletable worktree beside its primary one, and a
/// session to record the deletion against, which is what the pre-flight
/// refusal asks for first.
fn deletable_worktree_app() -> (Cagents, tempfile::TempDir, ProjectId, WorktreeId) {
    let mut app = context_inspector_test_app(false);
    let directory = tempfile::tempdir().unwrap();
    let (session, _) = SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    app.session = Some(session);
    let project = ProjectId::new();
    let worktree = WorktreeId::new();
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        "/repo".into(),
        vec![
            sidebar::SidebarWorktree::from_durable(
                WorktreeId::new(),
                "/repo".into(),
                Some("main".into()),
            ),
            sidebar::SidebarWorktree::from_durable(
                worktree,
                "/repo/feature".into(),
                Some("feature".into()),
            ),
        ],
        Vec::new(),
    ));
    (app, directory, project, worktree)
}

/// "Delete worktree…" is a request for a confirmation, and a confirmation
/// nobody can answer is the same as no command at all: the dialog the
/// runtime mounts has to carry the question and its confirming button,
/// and answering it has to start the deletion.
#[test]
fn deleting_a_worktree_mounts_its_confirmation_and_confirming_starts_the_deletion() {
    let renderer = headless_renderer();
    let (mut app, _directory, project_id, worktree_id) = deletable_worktree_app();

    let _ = app.update(Message::Sidebar(SidebarAction::ProjectCommand {
        project_id,
        worktree_id: Some(worktree_id),
        command: sidebar::ProjectCommand::Delete,
    }));
    assert!(
        app.modal_is_open(),
        "the command opens a modal rather than deleting outright"
    );
    let labels = dialog_labels(&app, &renderer);
    assert!(
        labels.iter().any(|label| label == "Delete worktree?"),
        "the confirmation the runtime mounts asks the question: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| label == "Delete worktree"),
        "with nothing blocking it, the confirming button is offered: {labels:?}"
    );

    let _ = app.update(Message::ProjectUi(project_ui::Action::ConfirmDelete));
    assert!(
        app.project_ui
            .deleting_directory(Path::new("/repo/feature")),
        "confirming hands the worktree to the deletion path"
    );
    assert!(
        !app.modal_is_open() && app.dialog().is_none(),
        "and the dialog goes once it is confirmed: the row reports the rest (#264)"
    );
    let row = app.sidebar_model.projects[0]
        .worktrees
        .iter()
        .find(|worktree| worktree.id == worktree_id)
        .unwrap();
    assert_eq!(row.deletion, Some(sidebar::RowDeletion::Deleting));
}

/// Both rails speak for one worktree. The context rail's Changes and Pull
/// Request tabs read the indicators the navigation row draws, so a glance
/// at either rail reports the same thing.
#[test]
fn the_context_rail_indicates_what_the_navigation_row_indicates() {
    let mut app = context_inspector_test_app(false);
    let project = ProjectId::new();
    let worktree = WorktreeId::new();
    let path = PathBuf::from("/repo/feature");
    let mut row = sidebar::SidebarWorktree::new(
        worktree,
        path.clone(),
        Some("feature".into()),
        false,
        false,
        Some(crate::repository::DirtyState {
            staged: 1,
            unstaged: 2,
            untracked: 0,
            conflicted: 0,
            submodule: 0,
        }),
        None,
        None,
    );
    row.head_repository =
        Some(GitHubRepoKey::new("github.com", None, "example", "cagents").unwrap());
    let mut pull_request = SidebarPullRequest::new(
        project,
        42,
        "Polish the rail".into(),
        Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
        "feature".into(),
        None,
        false,
        sidebar::CheckStatus::Pending,
    );
    pull_request.head_repository = row.head_repository.clone();
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        "/repo".into(),
        vec![
            sidebar::SidebarWorktree::from_durable(
                WorktreeId::new(),
                "/repo".into(),
                Some("main".into()),
            ),
            row,
        ],
        vec![pull_request],
    ));
    app.sidebar_state.sync_active_worktree(
        &app.sidebar_model,
        &SidebarFocus::Repository {
            worktree: path.clone(),
        },
    );

    let navigation = sidebar::worktree_indicators(&app.sidebar_model.projects[0].worktrees[1]);
    assert_eq!(
        navigation.dirty,
        Some("3 changes"),
        "the navigation row counts the worktree's changes"
    );
    assert!(
        navigation.pending_checks,
        "and reports its pull request's checks as in flight"
    );
    assert_eq!(
        app.context_rail_indicators(),
        navigation,
        "the context rail indicates exactly what the navigation row indicates"
    );
}

#[test]
fn a_mutation_that_did_not_do_all_of_what_it_was_asked_says_so() {
    let path = RepoPath::parse("src/app.rs").expect("a repository path");
    assert_eq!(
        mutation_report(
            "Stage",
            Ok(MutationOutcome::Applied {
                paths: vec![path.clone()]
            })
        ),
        None,
        "a mutation that did everything asked reports nothing: the refreshed rows are the \
             report"
    );

    let partial = mutation_report(
        "Stage",
        Ok(MutationOutcome::PartiallyApplied {
            paths: vec![path.clone()],
            failures: vec![crate::repository::MutationFailure::Rejected {
                reason: "pathspec did not match".into(),
            }],
        }),
    )
    .expect("a partial application is a failure with a known good half");
    assert!(partial.contains("1 path"), "{partial}");
    assert!(partial.contains("pathspec did not match"), "{partial}");

    let unknown = mutation_report(
        "Stage",
        Ok(MutationOutcome::OutcomeUnknown {
            failure: crate::repository::MutationFailure::Rejected {
                reason: "git was killed".into(),
            },
        }),
    )
    .expect("an unknown outcome is not a success");
    assert!(unknown.contains("may or may not"), "{unknown}");
}

#[test]
fn only_a_created_commit_clears_the_message_the_user_typed() {
    let oid = crate::repository::Oid::parse("ab".repeat(20)).expect("a valid object id");
    assert_eq!(
        commit_report(Ok(CommitOutcome::Created {
            oid: oid.clone(),
            warnings: Vec::new()
        })),
        (None, true)
    );

    let (report, committed) = commit_report(Ok(CommitOutcome::Created {
        oid,
        warnings: vec![crate::repository::MutationFailure::Rejected {
            reason: "post-commit hook failed".into(),
        }],
    }));
    assert!(committed, "the commit exists; the hook ran after it");
    assert!(
        report.expect("a failing hook is reported").contains("hook"),
        "a warning that is not shown is a commit the user thinks was clean"
    );

    let (report, committed) = commit_report(Ok(CommitOutcome::NotCreated {
        failure: crate::repository::MutationFailure::Rejected {
            reason: "there are no staged changes to commit".into(),
        },
    }));
    assert!(
        !committed,
        "a commit that was not created must keep the message: the text is the user's work"
    );
    assert!(report.is_some());
}

/// An app with one project whose GitHub state lists the given open pull
/// requests, so a sidebar row can be resolved to a review target.
pub(super) fn pull_request_test_app(numbers: &[u64]) -> (Cagents, ProjectId) {
    let mut app = context_inspector_test_app(false);
    let project = ProjectId::new();
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "project".into(),
        "/repo".into(),
        vec![SidebarWorktree::from_durable(
            WorktreeId::new(),
            "/repo".into(),
            Some("main".into()),
        )],
        Vec::new(),
    ));
    app.github_availability.insert(
        project,
        GitHubAvailability::Ready {
            repository_key: GitHubRepoKey::new("github.com", None, "o", "r").unwrap(),
            pull_requests: numbers
                .iter()
                .map(|number| {
                    (
                        crate::pull_request_review::tests::pull_request(*number),
                        Vec::new(),
                    )
                })
                .collect(),
            refreshed_at: 0,
        },
    );
    (app, project)
}

fn pull_request_cards(app: &Cagents) -> Vec<(CardId, Option<u64>)> {
    app.durable
        .cards
        .iter()
        .filter_map(|card| match &card.kind {
            CardKind::PullRequests(view) => Some((card.id, view.selected_number)),
            _ => None,
        })
        .collect()
}

fn reviewed_number(app: &Cagents, card: CardId) -> Option<u64> {
    match app.cards.get(&card) {
        Some(CardRuntime::PullRequests(pane)) => pane
            .review
            .target()
            .map(|target| target.pull_request.number),
        _ => None,
    }
}

fn open_pull_request(project_id: ProjectId, number: u64) -> Message {
    Message::Sidebar(SidebarAction::OpenPullRequest {
        project_id,
        number,
        url: Url::parse(&format!("https://github.com/o/r/pull/{number}")).unwrap(),
    })
}

#[test]
fn a_visible_pull_request_browser_refreshes_beside_the_focused_chat() {
    let (mut app, _) = pull_request_test_app(&[]);
    app.active_workspace = Some("/repo".into());
    let (browser, _) = app.add_pull_requests_card("/repo".into());
    if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&browser) {
        crate::pull_request_review::tests::ready_due_browser(&mut pane.review);
    }
    drop(app.new_native_chat(crate::agent::Provider::Codex, Some("/repo".into())));
    assert_ne!(app.workspace.focused(), Some(browser));
    app.workspace.resize_viewport(2400.0);
    app.workspace.native_scroll(0.0, 2400.0);
    assert_eq!(
        app.workspace.visibility(browser),
        Some(VisibilityClass::Visible)
    );
    app.refresh_ui.focused = true;
    drop(app.update_refresh(refresh_ui::Action::Tick));
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&browser) else {
        panic!("PR browser")
    };
    // A started refresh cancels the prior generation's request: the new
    // page response must not be accepted by the prior browser surface.
    assert!(matches!(
        pane.review.list(),
        crate::pull_request_review::ListState::Ready(_)
    ));
    assert!(pane.review.browser_is_refreshing());
}

#[test]
fn a_sidebar_pull_request_opens_one_review_pane_per_repository() {
    let (mut app, project) = pull_request_test_app(&[7, 8]);

    let _ = app.update(open_pull_request(project, 7));
    let cards = pull_request_cards(&app);
    assert_eq!(cards.len(), 1, "one pull request card for the repository");
    let (card, selected) = cards[0];
    assert_eq!(selected, Some(7), "the card remembers its pull request");
    assert_eq!(reviewed_number(&app, card), Some(7));
    assert_eq!(app.workspace.focused(), Some(card));

    let _ = app.update(open_pull_request(project, 8));
    let cards = pull_request_cards(&app);
    assert_eq!(cards.len(), 1, "a second pull request reuses the card");
    assert_eq!(cards[0], (card, Some(8)));
    assert_eq!(reviewed_number(&app, card), Some(8));

    // A pull request the project's GitHub state does not list cannot be
    // reviewed here and is left to the browser.
    let _ = app.update(open_pull_request(project, 9));
    assert_eq!(pull_request_cards(&app)[0], (card, Some(8)));
    assert_eq!(reviewed_number(&app, card), Some(8));
}

#[test]
fn the_link_setting_keeps_pull_requests_in_the_browser() {
    let (mut app, project) = pull_request_test_app(&[7]);
    app.settings.pull_request_open_target = PullRequestOpenTarget::Link;

    let _ = app.update(open_pull_request(project, 7));

    assert!(
        pull_request_cards(&app).is_empty(),
        "a link setting opens no review pane"
    );
}

#[test]
fn pull_request_panes_stay_in_the_worktree_where_the_user_opened_them() {
    let (mut app, project) = pull_request_test_app(&[7]);
    // Restoring a session admits only a worktree whose directory still
    // exists; a missing one falls back as a removed worktree.
    let checkouts = tempfile::tempdir().unwrap();
    let feature_path = checkouts.path().join("feature");
    let other_path = checkouts.path().join("other");
    for path in [&feature_path, &other_path] {
        std::fs::create_dir(path).unwrap();
    }
    let terminal = |path: &Path| {
        let mut card = terminal_card_state();
        let CardKind::Terminal(session) = &mut card.kind else {
            unreachable!();
        };
        session.launch.working_directory = path.into();
        card
    };
    let feature = terminal(&feature_path);
    let other = terminal(&other_path);
    Arc::make_mut(&mut app.durable).cards = vec![feature.clone(), other.clone()].into();
    let rows = &mut app.sidebar_model.projects[0].worktrees;
    rows.extend([&feature_path, &other_path].map(|path| {
        SidebarWorktree::from_durable(WorktreeId::new(), path.clone(), Some("feature".into()))
    }));

    drop(app.select_worktree(feature_path.clone(), false));
    drop(app.update(open_pull_request(project, 7)));
    let review = pull_request_cards(&app)[0].0;
    assert_eq!(app.durable.card_scopes[&review], feature_path);

    drop(app.select_worktree(other_path.clone(), false));
    assert!(app.workspace.card(review).is_none());
    assert_eq!(app.workspace.focused(), Some(other.id));
    // Repository refreshes reselect the active worktree after discovery.
    drop(app.select_worktree(other_path.clone(), false));
    app.repository_generations.insert(project, 1);
    let availability = app.github_availability[&project].clone();
    drop(app.update(Message::GitUi(git_ui::Action::Remote {
        project,
        generation: 1,
        result: Ok((availability, Vec::new())),
    })));
    assert!(app.workspace.card(review).is_none());
    assert_eq!(app.workspace.focused(), Some(other.id));
    assert_eq!(pull_request_cards(&app).len(), 1);

    drop(app.update(open_pull_request(project, 7)));
    let other_review = app.workspace.focused().unwrap();
    assert_ne!(
        other_review, review,
        "explicit opening belongs to this worktree"
    );
    assert_eq!(app.durable.card_scopes[&other_review], other_path);
    drop(app.browse_pull_requests(project));
    assert_eq!(
        pull_request_cards(&app).len(),
        2,
        "browsing reuses the visible card"
    );

    // Persisted filing also controls a rebuilt session's strip.
    Arc::make_mut(&mut app.durable).active_worktree = Some(feature_path.clone());
    app.worktree_owners.clear();
    drop(app.restore_worktree_scope());
    assert!(app.workspace.card(review).is_some());
    assert!(app.workspace.card(other_review).is_none());
}

#[test]
fn dismissing_a_pending_restored_review_prevents_background_reopening() {
    let (mut app, project) = pull_request_test_app(&[7]);
    let (card, _) = app.add_pull_requests_card("/repo".into());
    if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
        pane.pending_number = Some(7);
    }
    let CardKind::PullRequests(view) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!();
    };
    view.selected_number = Some(7);

    drop(app.update(Message::PullRequestReview {
        card,
        action: pull_request_review::Action::Close,
    }));
    drop(app.open_pending_pull_requests(project));
    assert_eq!(reviewed_number(&app, card), None);
    assert_eq!(pull_request_cards(&app), vec![(card, None)]);
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&card) else {
        unreachable!();
    };
    assert_eq!(pane.pending_number, None);
}

#[test]
fn a_restored_pull_request_card_reopens_its_pull_request_when_github_loads() {
    let (mut app, project) = pull_request_test_app(&[7]);
    let (card, _) = app.add_pull_requests_card("/repo".into());
    if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
        pane.pending_number = Some(7);
    }
    assert_eq!(reviewed_number(&app, card), None);

    let _ = app.open_pending_pull_requests(project);

    assert_eq!(reviewed_number(&app, card), Some(7));
    assert_eq!(pull_request_cards(&app), vec![(card, Some(7))]);

    // A card restored with a pull request that has since been merged or
    // closed stops waiting: the listing is settled and will not gain it.
    let (mut app, project) = pull_request_test_app(&[7]);
    let (card, _) = app.add_pull_requests_card("/repo".into());
    if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
        pane.pending_number = Some(42);
    }
    let _ = app.open_pending_pull_requests(project);
    assert_eq!(reviewed_number(&app, card), None);
    match app.cards.get(&card) {
        Some(CardRuntime::PullRequests(pane)) => assert_eq!(pane.pending_number, None),
        _ => panic!("the pull request card is missing"),
    }
}

#[test]
fn a_restored_review_can_read_its_cache_when_the_sidebar_list_is_offline() {
    let (mut app, project) = pull_request_test_app(&[]);
    let key = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
    app.github_availability.insert(
        project,
        GitHubAvailability::Failed {
            detail: "offline".into(),
        },
    );
    app.review_repositories.insert(project, key.clone());
    let (card, _) = app.add_pull_requests_card("/repo".into());
    if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
        pane.pending_number = Some(42);
    }
    drop(app.open_pending_pull_requests(project));
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&card) else {
        panic!("missing review")
    };
    assert_eq!(
        pane.review.source().map(|source| &source.repository),
        Some(&key)
    );
    assert_eq!(
        pane.pending_number, None,
        "exact cache lookup was dispatched"
    );
    assert!(
        app.github_availability[&project].repository_key().is_none(),
        "local read identity never grants sidebar mutation authority"
    );
}

/// The pane's surfaces each add and remove named controls (the refresh
/// spinner, the viewed marks, the file toolbar), which is the condition
/// that strands a named node; see the repository surface test above.
#[test]
fn moving_through_the_pull_request_pane_repaints_the_window() {
    use crate::pull_request_review::{Action, tests as review};
    use crate::repository::ChangeKind;
    use cosmic::iced::advanced::widget::Tree;

    let (mut app, _) = pull_request_test_app(&[7, 8]);
    let (card, _) = app.add_pull_requests_card("/repo".into());
    let mut renderer = headless_renderer();
    let mut tree = Tree::empty();
    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);

    let review_action = |action| Message::PullRequestReview { card, action };
    let _ = app.update(review_action(Action::Select(7)));
    present(&app, &mut renderer, &mut tree);
    assert_frame_is_faithful(&app, &tree);

    if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
        review::finish_load(
            &mut pane.review,
            review::loaded(
                vec![
                    review::entry("src/lib.rs", ChangeKind::Modified),
                    review::entry("src/new.rs", ChangeKind::Added),
                    review::entry("docs/guide.md", ChangeKind::Deleted),
                ],
                vec![review::remote_file("src/lib.rs", true)],
            ),
        );
    }
    for action in [
        Action::SelectFile(RepoPath::parse("src/new.rs").unwrap()),
        Action::ToggleViewed(RepoPath::parse("src/new.rs").unwrap()),
        Action::NextFile,
        Action::PreviousFile,
        Action::Refresh,
        Action::Close,
        Action::Select(8),
    ] {
        let _ = app.update(review_action(action));
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);
    }
}

#[test]
fn thousand_file_review_mounts_a_bounded_tree_and_draws_repeated_frames() {
    use crate::pull_request_review::{Action, tests as review};
    use cosmic::iced::advanced::widget::Tree;
    let (mut app, project) = pull_request_test_app(&[1]);
    drop(app.update(open_pull_request(project, 1)));
    let card = pull_request_cards(&app)[0].0;
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
        panic!("review pane")
    };
    let mut snapshot = review::loaded(
        (0..1000)
            .map(|i| {
                review::entry(
                    &format!("src/file-{i:04}.rs"),
                    crate::repository::ChangeKind::Modified,
                )
            })
            .collect(),
        (0..100)
            .map(|i| review::remote_file(&format!("src/file-{i:04}.rs"), false))
            .collect(),
    );
    snapshot.next_file_cursor = Some("page-2".into());
    review::finish_load(&mut pane.review, snapshot);
    let mut renderer = headless_renderer();
    let mut tree = Tree::empty();
    fn nodes(tree: &Tree) -> usize {
        1 + tree.children.iter().map(nodes).sum::<usize>()
    }
    let mut timings = Vec::new();
    for frame in 0..12 {
        let start = Instant::now();
        present(&app, &mut renderer, &mut tree);
        timings.push(start.elapsed());
        assert!(
            nodes(&tree) < 5000,
            "offscreen file widgets must not be mounted: {}",
            nodes(&tree)
        );
        drop(app.update(Message::PullRequestReview {
            card,
            action: Action::FilePage(
                crate::pull_request_review::file_pages::PageAction::Viewport {
                    offset: frame as f32 * 70.0 * design::DENSE_ROW_HEIGHT,
                    height: 1000.0,
                },
            ),
        }));
    }
    timings.sort();
    eprintln!(
        "1000-file review, full layout/draw: median {:?}, max {:?}, mounted nodes {}",
        timings[6],
        timings[11],
        nodes(&tree)
    );
}

/// Restoring is the shell's doing, not a keystroke aimed at a dialog, so
/// a dialog on screen when the project loads must not swallow it.
#[test]
fn a_restored_pull_request_reopens_even_under_a_dialog() {
    let (mut app, project) = pull_request_test_app(&[7]);
    let (card, _) = app.add_pull_requests_card("/repo".into());
    if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
        pane.pending_number = Some(7);
    }
    app.settings_reset_confirm = true;
    assert!(app.modal_is_open());

    let _ = app.open_pending_pull_requests(project);

    assert_eq!(reviewed_number(&app, card), Some(7));
}

/// What a review pane shows can change without the durable workspace
/// changing, as when a restored card opens the pull request it already
/// names. The alert focus the daemon sees must follow the pane anyway.
#[test]
fn a_review_opened_without_a_durable_change_republishes_its_alert_focus() {
    let (mut app, project) = pull_request_test_app(&[7]);
    app.core.set_main_window_id(Some(window::Id::RESERVED));
    app.refresh_ui.focused = true;
    let (card, _) = app.add_pull_requests_card("/repo".into());
    app.complete_review_read_for_test(&[7]);
    app.sync_notification_presence();
    let before = app
        .notification_ui
        .take_pending_presence_for_test()
        .expect("the focused pane's presence");
    assert_eq!(before.focused_target, None, "no review is open yet");

    let durable = Arc::as_ptr(&app.durable);
    let target = app
        .pull_request_target(project, 7)
        .expect("a listed target");
    let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
        panic!("a pull request pane");
    };
    drop(
        pane.review
            .update(crate::pull_request_review::Action::Open(target)),
    );
    assert_eq!(reviewed_number(&app, card), Some(7));
    app.sync_notification_presence();
    assert_eq!(Arc::as_ptr(&app.durable), durable);
    let presence = app
        .notification_ui
        .take_pending_presence_for_test()
        .expect("opening a review changes what the window focuses");
    assert!(matches!(
        presence.focused_target,
        Some(crate::notifications::Target::PullRequest(request))
            if request.number == 7 && request.request == "request-7"
    ));
}

#[test]
fn choosing_a_listed_pull_request_in_the_pane_opens_it() {
    let (mut app, project) = pull_request_test_app(&[7]);
    let (card, _) = app.add_pull_requests_card("/repo".into());

    let _ = app.update(Message::PullRequestReview {
        card,
        action: crate::pull_request_review::Action::Select(7),
    });
    assert_eq!(reviewed_number(&app, card), Some(7));
    assert_eq!(pull_request_cards(&app), vec![(card, Some(7))]);

    let _ = app.update(Message::PullRequestReview {
        card,
        action: crate::pull_request_review::Action::Close,
    });
    assert_eq!(reviewed_number(&app, card), None);
    assert_eq!(
        pull_request_cards(&app),
        vec![(card, None)],
        "closing the review forgets the pull request"
    );
    let _ = project;
}
