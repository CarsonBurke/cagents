//! Actual App completion routing over the original local selection worker.
//! Picker opening is controlled fixture state, never the live indexer action.
use super::*;
use crate::{agent::Provider, persistence::SavedConversation};
use std::sync::atomic::{AtomicUsize, Ordering};

const WAIT: Duration = Duration::from_secs(10);

struct Fixture {
    app: Cagents,
    _directory: tempfile::TempDir,
    runtime: PathBuf,
    old: PathBuf,
    next: PathBuf,
    next_project: ProjectId,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().canonicalize().unwrap();
    let runtime = base.join("runtime");
    let old = base.join("old");
    let next = base.join("next");
    let mut app = tests::context_inspector_test_app(false);
    let mut next_project = None;
    for path in [&runtime, &old, &next] {
        std::fs::create_dir(path).unwrap();
        if path == &runtime {
            continue;
        }
        let id = ProjectId::new();
        let mut project = crate::sidebar::SidebarProject::new(
            id,
            RepositoryId::new(),
            "App resume fixture".into(),
            path.clone(),
            Vec::new(),
            Vec::new(),
        );
        project.kind = ProjectKind::Folder;
        app.sidebar_model.projects.push(project);
        if path == &next {
            next_project = Some(id);
        }
    }
    app.configure_local_resume_for_test(runtime.clone());
    drop(app.select_worktree(old.clone(), false));
    Fixture {
        app,
        _directory: directory,
        runtime,
        old,
        next,
        next_project: next_project.unwrap(),
    }
}

fn saved(path: &Path, name: &str) -> SavedConversation {
    let mut session = crate::chat::fresh_session(
        Provider::Codex,
        TerminalId::new(),
        path.to_owned(),
        &crate::account::Account::system(Provider::Codex),
    )
    .unwrap();
    session.launch.session_id = Some(name.into());
    session.launch.recipe.environment.insert(
        "CODEX_HOME".into(),
        path.parent()
            .unwrap()
            .join("recorded-account")
            .to_string_lossy()
            .into_owned(),
    );
    session.launch.recipe.unset_environment.remove("CODEX_HOME");
    SavedConversation {
        card: CardId::new(),
        title: name.into(),
        session,
        open: false,
        updated_at_unix_ms: 1,
    }
}

fn picker(app: &mut Cagents) {
    app.resume_ui.open = true;
    app.resume_ui.generation = app.resume_ui.generation.wrapping_add(1);
    app.resume_ui.loading = false;
    app.resume_ui.error = None;
    *app.resume_ui.cancel_for_test() = Arc::default();
}

fn prepared(app: &mut Cagents, saved: SavedConversation) -> Message {
    Cagents::local_resume_prepared_for_test(app.resume_saved_conversation(saved))
}

fn settle(app: &mut Cagents, mut receipt: Message) {
    for _ in 0..4 {
        // Deliberately exercise App::update, never the direct core handler.
        let retirement = app.update(receipt);
        let retired = Cagents::local_resume_retired_for_test(retirement);
        let next = app.update(retired);
        if !app.worktree_selection.is_active() {
            drop(next);
            return;
        }
        receipt = Cagents::local_resume_prepared_for_test(next);
    }
    panic!("App resume did not settle after bounded metadata retries");
}

fn resumed(app: &Cagents, saved: &SavedConversation) -> usize {
    app.durable.cards.iter().filter(|card| {
        card.existence.is_open()
            && matches!(&card.kind, CardKind::Chat(session) if session.launch == saved.session.launch)
    }).count()
}

fn protected(runtime: &Path, path: &Path) -> bool {
    match crate::worktree_cleanup::fixture_protection(runtime, path, true) {
        Ok(guard) => {
            drop(guard);
            false
        }
        Err(error) => {
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            true
        }
    }
}

struct Release(Option<crossbeam_channel::Sender<()>>);

impl Release {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        self.release();
    }
}

fn retirement_barrier(
    app: &mut Cagents,
) -> (crossbeam_channel::Receiver<std::thread::ThreadId>, Release) {
    let (at, entered) = crossbeam_channel::bounded(1);
    let (release, wait) = crossbeam_channel::bounded(1);
    let visits = Arc::new(AtomicUsize::new(0));
    app.worktree_selection
        .before_retire_for_test(Arc::new(move || {
            if visits.fetch_add(1, Ordering::SeqCst) == 0 {
                at.send(std::thread::current().id()).unwrap();
                wait.recv_timeout(WAIT).unwrap();
            }
        }));
    (entered, Release(Some(release)))
}

fn preparation_count(app: &mut Cagents) -> Arc<AtomicUsize> {
    let starts = Arc::new(AtomicUsize::new(0));
    let observe = starts.clone();
    app.worktree_selection
        .before_prepare_for_test(Arc::new(move |_| {
            observe.fetch_add(1, Ordering::SeqCst);
        }));
    starts
}

fn ordinary(path: &Path, number: u64) -> Message {
    Message::PullRequestCheckoutReady {
        card: CardId::new(),
        project: ProjectId::new(),
        generation: number,
        target: pull_request_review::CheckoutTarget::read_only(
            path.to_owned(),
            number,
            crate::repository::Oid::parse(&"a".repeat(40)).unwrap(),
        ),
        result: Err("queued ordinary completion".into()),
    }
}

#[test]
fn current_resume_completion_commits_through_app_while_only_its_picker_is_modal() {
    let mut f = fixture();
    let saved = saved(&f.next, "App current picker");
    picker(&mut f.app);
    let receipt = prepared(&mut f.app, saved.clone());
    assert!(f.app.modal_is_open() && f.app.resume_ui.open && f.app.resume_ui.loading);
    assert!(!f.app.navigation_completion_is_blocked(&receipt));
    assert_eq!(resumed(&f.app, &saved), 0);
    settle(&mut f.app, receipt);
    assert_eq!(resumed(&f.app, &saved), 1);
    assert!(!f.app.resume_ui.open && !f.app.worktree_selection.is_active());
    assert_eq!(f.app.active_workspace.as_ref(), Some(&f.next));
    assert!(protected(&f.runtime, &f.next));
    assert!(!protected(&f.runtime, &f.old));
}

#[derive(Clone, Copy)]
enum Blocker {
    Device,
    Worktree,
    PullRequest,
    Update,
    TopModal,
    Save,
}

fn set_modal(app: &mut Cagents, blocker: Blocker, enabled: bool) {
    match blocker {
        Blocker::Device => app.device_ui.open = enabled,
        Blocker::Worktree => {
            app.worktree_dialog = enabled.then(|| {
                WorktreeDialog::new(
                    ProjectId::new(),
                    PathBuf::from("/fixture"),
                    WorktreeDraft::default(),
                )
            })
        }
        Blocker::PullRequest => {
            app.pull_request_dialog = enabled.then(|| PullRequestDialog {
                project_id: ProjectId::new(),
                repository: GitHubRepoKey::new("github.com", None, "fixture", "repository")
                    .unwrap(),
                worktree: None,
                base: "main".into(),
                head: "topic".into(),
                title: "fixture".into(),
                body: String::new().into(),
                draft: true,
                retry_confirmation: None,
            })
        }
        Blocker::Update => {
            app.update_ui.confirmation = enabled.then(|| crate::update::UpdateConfirmation {
                nonce: "App fixture confirmation".into(),
                preview: crate::update::impact::Preview {
                    candidate: crate::update::impact::Candidate {
                        version: "fixture".into(),
                        protocol: 1,
                        digest: "fixture".into(),
                    },
                    replaces_daemon: false,
                    daemon: None,
                },
                expires_at_unix_ms: u64::MAX,
            })
        }
        Blocker::TopModal => app.settings_reset_confirm = enabled,
        Blocker::Save => unreachable!("save uses the actual editor's active ticket"),
    }
}

#[test]
fn shared_form_predicate_preserves_every_original_form_and_excludes_only_resume() {
    for flags in 0..16 {
        let mut app = tests::context_inspector_test_app(false);
        set_modal(&mut app, Blocker::Device, flags & 1 != 0);
        set_modal(&mut app, Blocker::Worktree, flags & 2 != 0);
        set_modal(&mut app, Blocker::PullRequest, flags & 4 != 0);
        app.resume_ui.open = flags & 8 != 0;
        assert_eq!(app.form_dialog_is_open(), flags != 0);
        assert_eq!(app.form_dialog_is_open_including_resume(true), flags != 0);
        assert_eq!(
            app.form_dialog_is_open_including_resume(false),
            flags & 7 != 0
        );
    }
}

fn saving_fixture(app: &mut Cagents) -> (CardId, RepositoryAction) {
    // Existing editor metadata fixture owns a real active SaveTicket. It does
    // not execute a writer or claim a disk-save result. Restore the content
    // to that submitted snapshot so its typed durable completion clears both
    // the save and ordinary dirty-buffer navigation fences.
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::post_persist_failure_fixture(card);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: repository_widget::EditorAction::SelectAll,
    }));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: repository_widget::EditorAction::Edit(repository_widget::EditorEdit::Paste(
            Arc::new("B\n".into()),
        )),
    }));
    let completion = runtime.durable_active_save_completion();
    assert!(runtime.is_saving());
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    app.workspace
        .add_card(
            WorkspaceCard::new(card, "Saving fixture", CardWidth::Fraction(0.5)).unwrap(),
            app.workspace.cards().len(),
            true,
        )
        .unwrap();
    assert!(app.focused_repository_is_saving());
    (card, completion)
}

#[test]
fn current_resume_deferred_by_other_dialog_or_save_replays_with_picker_still_open() {
    for blocker in [
        Blocker::Device,
        Blocker::Worktree,
        Blocker::PullRequest,
        Blocker::Update,
        Blocker::TopModal,
        Blocker::Save,
    ] {
        let mut f = fixture();
        let saved = saved(&f.next, "App deferred current picker");
        picker(&mut f.app);
        let receipt = prepared(&mut f.app, saved.clone());
        let save = if matches!(blocker, Blocker::Save) {
            Some(saving_fixture(&mut f.app))
        } else {
            set_modal(&mut f.app, blocker, true);
            None
        };
        assert!(f.app.form_dialog_is_open());
        assert!(f.app.navigation_completion_is_blocked(&receipt));
        drop(f.app.update(ordinary(&f.old, 1)));
        drop(f.app.update(receipt));
        drop(f.app.update(ordinary(&f.old, 2)));
        assert_eq!(f.app.deferred_navigation.len(), 3);
        assert_eq!(resumed(&f.app, &saved), 0);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        if let Some((card, completion)) = save {
            let Some(CardRuntime::Repository(runtime)) = f.app.cards.get_mut(&card) else {
                unreachable!()
            };
            drop(runtime.update(completion));
            assert!(!runtime.is_saving() && !runtime.blocks_navigation());
            assert!(!f.app.focused_repository_is_saving());
            // This nondurable editor fixture only supplies focused-save
            // metadata; remove it before resuming the real folder fixture.
            f.app.workspace.request_close(card).unwrap();
            f.app.workspace.commit_close(card).unwrap();
            f.app.cards.remove(&card);
        } else {
            set_modal(&mut f.app, blocker, false);
        }
        assert!(f.app.resume_ui.open && f.app.resume_ui.loading);
        let release = f.app.update(Message::Frame(Instant::now()));
        assert_eq!(f.app.deferred_navigation.len(), 2);
        let numbers = f
            .app
            .deferred_navigation
            .iter()
            .map(|message| match message {
                Message::PullRequestCheckoutReady { generation, .. } => *generation,
                _ => panic!("ordinary queued neighbor changed"),
            })
            .collect::<Vec<_>>();
        assert_eq!(numbers, vec![1, 2], "ordinary neighbor FIFO is retained");
        assert!(
            f.app
                .deferred_navigation
                .iter()
                .all(|message| f.app.navigation_completion_is_blocked(message))
        );
        let receipt = Cagents::local_resume_prepared_for_test(release);
        assert!(f.app.resume_ui.open);
        settle(&mut f.app, receipt);
        assert_eq!(resumed(&f.app, &saved), 1);
        assert!(!f.app.resume_ui.open && !f.app.worktree_selection.is_active());
    }
}

#[test]
fn pickerless_resume_keeps_the_original_modal_completion_fence() {
    let mut f = fixture();
    let saved = saved(&f.next, "App pickerless resume");
    assert!(!f.app.resume_ui.open);
    let receipt = prepared(&mut f.app, saved.clone());
    f.app.settings_reset_confirm = true;
    assert!(f.app.navigation_completion_is_blocked(&receipt));
    drop(f.app.update(receipt));
    assert_eq!(f.app.deferred_navigation.len(), 1);
    assert_eq!(resumed(&f.app, &saved), 0);
    f.app.settings_reset_confirm = false;
    let release = f.app.update(Message::Frame(Instant::now()));
    let receipt = Cagents::local_resume_prepared_for_test(release);
    settle(&mut f.app, receipt);
    assert_eq!(resumed(&f.app, &saved), 1);
}

#[test]
fn app_retires_closed_picker_receipts_before_starting_the_reopened_picker_request() {
    for queued in [false, true] {
        let mut f = fixture();
        let starts = preparation_count(&mut f.app);
        let (retiring, mut release) = retirement_barrier(&mut f.app);
        let old = saved(&f.next, "obsolete App picker");
        let fresh = saved(&f.next, "fresh App picker");
        picker(&mut f.app);
        let old_receipt = prepared(&mut f.app, old.clone());
        let mut late = Some(old_receipt);
        if queued {
            f.app.settings_reset_confirm = true;
            drop(f.app.update(late.take().unwrap()));
            assert_eq!(f.app.deferred_navigation.len(), 1);
        }
        let closed = f
            .app
            .update(Message::ResumeConversation(resume_ui::Action::Close));
        f.app.settings_reset_confirm = false;
        picker(&mut f.app);
        f.app.resume_ui.query = "new picker query".into();
        drop(f.app.resume_saved_conversation(fresh.clone()));
        f.app.resume_ui.error = Some("new picker's own error".into());
        let generation = f.app.resume_ui.generation;
        let cancel = f.app.resume_ui.cancel_for_test().clone();
        let retirement = if queued {
            closed
        } else {
            drop(closed);
            f.app.update(late.take().unwrap())
        };
        assert_ne!(
            retiring.recv_timeout(WAIT).unwrap(),
            std::thread::current().id()
        );
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "fresh request waits for real retirement"
        );
        assert!(f.app.deferred_navigation.is_empty());
        assert!(f.app.resume_ui.open && f.app.resume_ui.loading);
        assert_eq!(f.app.resume_ui.query, "new picker query");
        assert_eq!(f.app.resume_ui.generation, generation);
        assert_eq!(
            f.app.resume_ui.error.as_deref(),
            Some("new picker's own error")
        );
        assert!(Arc::ptr_eq(&cancel, f.app.resume_ui.cancel_for_test()));
        assert!(!cancel.load(Ordering::Relaxed));
        assert_eq!(resumed(&f.app, &old), 0);
        assert_eq!(resumed(&f.app, &fresh), 0);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        release.release();
        let retired = Cagents::local_resume_retired_for_test(retirement);
        let fresh_task = f.app.update(retired);
        let receipt = Cagents::local_resume_prepared_for_test(fresh_task);
        assert!(starts.load(Ordering::SeqCst) >= 2);
        settle(&mut f.app, receipt);
        assert_eq!(resumed(&f.app, &old), 0);
        assert_eq!(resumed(&f.app, &fresh), 1);
    }
}

#[test]
fn ordinary_predecessor_is_cleanup_only_while_a_resume_waits_behind_it() {
    for queued in [false, true] {
        let mut f = fixture();
        let starts = preparation_count(&mut f.app);
        let (retiring, mut release) = retirement_barrier(&mut f.app);
        let ordinary_task = f
            .app
            .update(Message::Sidebar(SidebarAction::OpenRepository {
                project_id: f.next_project,
                repository: f.next.clone(),
            }));
        let mut ordinary_receipt = Some(Cagents::local_resume_prepared_for_test(ordinary_task));
        if queued {
            f.app.settings_reset_confirm = true;
            drop(f.app.update(ordinary_receipt.take().unwrap()));
            assert_eq!(f.app.deferred_navigation.len(), 1);
        }
        picker(&mut f.app);
        let saved = saved(&f.old, "resume supersedes ordinary navigation");
        let request = f.app.resume_saved_conversation(saved.clone());
        let retirement = if queued {
            // The new request drains an already-deferred predecessor. Its
            // returned Task owns the real retirement and cannot be dropped.
            request
        } else {
            drop(request);
            let receipt = ordinary_receipt.take().unwrap();
            assert!(!f.app.navigation_completion_is_blocked(&receipt));
            f.app.update(receipt)
        };
        assert_ne!(
            retiring.recv_timeout(WAIT).unwrap(),
            std::thread::current().id()
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(f.app.deferred_navigation.is_empty());
        assert_eq!(
            f.app.active_workspace.as_ref(),
            Some(&f.old),
            "ordinary target never commits"
        );
        assert!(f.app.resume_ui.open && f.app.resume_ui.loading);
        assert_eq!(resumed(&f.app, &saved), 0);
        f.app.settings_reset_confirm = false;
        release.release();
        let retired = Cagents::local_resume_retired_for_test(retirement);
        let next = f.app.update(retired);
        let receipt = Cagents::local_resume_prepared_for_test(next);
        settle(&mut f.app, receipt);
        assert_eq!(resumed(&f.app, &saved), 1);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert!(!f.app.worktree_selection.is_active());
    }
}
