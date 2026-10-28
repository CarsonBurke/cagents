//! Owning local-resume worker tests. No picker indexer or provider is started.
//!
//! The picker is opened by controlled fixture state: Action::Open starts a
//! real indexer synchronously. These tests drive the production resume entry,
//! original preparation receipt and actual retirement. App modal routing is
//! covered separately; core consumption here deliberately names its handler.
use super::tests::{Fixture, executor, fixture};
use super::*;
use crate::{agent::Provider, persistence::SavedConversation};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

const WAIT: Duration = Duration::from_secs(10);

fn message(task: Task<Message>, accept: impl Fn(&Message) -> bool) -> Message {
    use iced::futures::StreamExt;
    let mut stream = iced::runtime::task::into_stream(task).expect("actual owning task");
    executor().block_on(async {
        tokio::time::timeout(WAIT + WAIT, async {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    if accept(&message) {
                        return message;
                    }
                }
            }
            panic!("original task did not deliver its owning receipt")
        })
        .await
        .expect("bounded original worker receipt")
    })
}

fn prepared(task: Task<Message>) -> Message {
    message(task, |message| {
        matches!(message, Message::WorktreeNavigationPrepared { .. })
    })
}

fn consume(app: &mut Cagents, message: Message) -> Task<Message> {
    let Message::WorktreeNavigationPrepared {
        generation,
        receipt,
    } = message
    else {
        panic!("actual preparation receipt")
    };
    app.worktree_navigation_prepared(generation, receipt)
}

fn retire(app: &mut Cagents, task: Task<Message>) -> Task<Message> {
    let done = message(task, |message| {
        matches!(message, Message::WorktreeNavigationRetired(_))
    });
    let Message::WorktreeNavigationRetired(ticket) = done else {
        unreachable!()
    };
    app.worktree_navigation_retired(ticket)
}

fn settle(app: &mut Cagents, mut receipt: Message) {
    // An adoption/metadata change can require one fresh preparation behind
    // real retirement. Bound that actual protocol rather than dropping Task.
    for _ in 0..4 {
        let retirement = consume(app, receipt);
        let next = retire(app, retirement);
        if app.worktree_selection.active.is_none() {
            return;
        }
        receipt = prepared(next);
    }
    panic!("local resume did not settle within bounded metadata repreparation")
}

impl Cagents {
    /// Fresh headless fixture setup. The caller retains this private runtime
    /// directory until the app and all real selection receipts have retired.
    /// Install before restoring scope: existing guards cannot change runtime.
    pub(in crate::app) fn configure_local_resume_for_test(&mut self, runtime: PathBuf) {
        assert!(!self.worktree_selection.is_active());
        self.worktree_protections
            .use_fixture_runtime(runtime.clone());
        self.worktree_selection.local_window = true;
        self.worktree_selection.runtime = Some(runtime);
        self.worktree_selection.followup_effects = Some(Vec::new());
    }

    /// The isolated scripted-daemon history fixture owns its real effects and
    /// recursively drives App::update. Supply private navigation guards/ready
    /// storage without swallowing its required scripted attachment attempt.
    pub(in crate::app) fn configure_local_resume_without_effect_capture_for_test(
        &mut self,
        runtime: PathBuf,
    ) {
        self.configure_local_resume_for_test(runtime);
        self.worktree_selection.followup_effects = None;
    }

    /// Completes only the real local-selection protocol, including its actual
    /// retirement and bounded metadata retries. Configure a private fixture
    /// runtime first. Independent hydration/provider effects remain captured;
    /// this direct worker driver makes no App modal-routing or native claim.
    pub(in crate::app) fn finish_local_resume_for_test(&mut self, task: Task<Message>) {
        assert!(self.worktree_selection.runtime.is_some());
        assert!(self.worktree_selection.followup_effects.is_some());
        settle(self, prepared(task));
    }

    /// Delivers the actual receipt without consuming it, for tests which must
    /// exercise App::update's modal/save routing rather than the core driver.
    pub(in crate::app) fn local_resume_prepared_for_test(task: Task<Message>) -> Message {
        prepared(task)
    }

    /// Waits for the original worker to report real retirement; the caller
    /// still delivers that message through its actual App::update route.
    pub(in crate::app) fn local_resume_retired_for_test(task: Task<Message>) -> Message {
        message(task, |message| {
            matches!(message, Message::WorktreeNavigationRetired(_))
        })
    }
}

fn entry(path: &Path) -> SavedConversation {
    // Codex avoids Claude's synchronous default-policy read in fixture setup.
    // This only constructs a saved recipe; no provider/listener task runs.
    let mut session = crate::chat::fresh_session(
        Provider::Codex,
        TerminalId::new(),
        path.to_owned(),
        &crate::account::Account::system(Provider::Codex),
    )
    .unwrap();
    session.launch.session_id = Some("local-resume-worker-fixture".into());
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
        title: "Recorded local conversation".into(),
        session,
        open: false,
        updated_at_unix_ms: 1,
    }
}

fn open_picker(app: &mut Cagents) {
    // Controlled opening only. Never call Action::Open/open_resume here.
    app.resume_ui.open = true;
    app.resume_ui.loading = false;
    app.resume_ui.error = None;
}

fn folder(app: &mut Cagents, path: &Path) -> ProjectId {
    let id = ProjectId::new();
    let mut project = crate::sidebar::SidebarProject::new(
        id,
        RepositoryId::new(),
        "resume fixture".into(),
        path.to_owned(),
        Vec::new(),
        Vec::new(),
    );
    project.kind = ProjectKind::Folder;
    app.sidebar_model.projects.push(project);
    id
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

fn resumed(app: &Cagents, expected: &SavedConversation) -> CardId {
    let matches = app
        .durable
        .cards
        .iter()
        .filter(|card| {
            card.existence.is_open()
                && matches!(&card.kind, CardKind::Chat(session)
                if session.launch == expected.session.launch)
        })
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "exact recorded launch appears once");
    let id = matches[0].id;
    // Successful saved-history insertion may record Phase::Starting. Its
    // provider handshake is a lazy independent effect captured by fixture();
    // these worker tests never poll it or claim native/provider hydration.
    id
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

fn during_preparation(
    f: &mut Fixture,
    entry: SavedConversation,
    held: impl FnOnce(&mut Fixture),
) -> Message {
    let (entered, observe) = crossbeam_channel::bounded(1);
    let (release, wait) = crossbeam_channel::bounded(1);
    let starts = Arc::new(AtomicUsize::new(0));
    let count = starts.clone();
    f.app.worktree_selection.before_prepare = Some(Arc::new(move |_| {
        if count.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            entered.send(()).unwrap();
            wait.recv_timeout(WAIT).unwrap();
        }
    }));
    let task = f.app.resume_saved_conversation(entry);
    std::thread::scope(|scope| {
        let original = scope.spawn(move || prepared(task));
        // Released on assertion failure before scope joins the original task.
        let mut release = Release(Some(release));
        observe.recv_timeout(WAIT).unwrap();
        held(f);
        assert_eq!(starts.load(AtomicOrdering::SeqCst), 1);
        release.release();
        original.join().unwrap()
    })
}

#[test]
fn resumed_scope_classification_is_scheduled_without_ui_filesystem_reads() {
    let mut f = fixture();
    let saved = entry(&f.next);
    open_picker(&mut f.app);
    let ui = std::thread::current().id();
    let reads = Arc::new(Mutex::new(Vec::new()));
    let capture = reads.clone();
    f.app.worktree_selection.owner_read = Some(Arc::new(move || {
        capture.lock().unwrap().push(std::thread::current().id());
    }));
    let capture = reads.clone();
    f.app
        .worktree_protections
        .observe_navigation_io(Arc::new(move || {
            capture.lock().unwrap().push(std::thread::current().id());
        }));
    let retired_on = Arc::new(Mutex::new(Vec::new()));
    let capture = retired_on.clone();
    f.app.worktree_selection.after_retire = Some(Arc::new(move || {
        capture.lock().unwrap().push(std::thread::current().id());
    }));
    let (at_retirement, observe) = crossbeam_channel::bounded(1);
    let (release, wait) = crossbeam_channel::bounded(1);
    f.app.worktree_selection.before_retire = Some(Arc::new(move || {
        at_retirement.send(std::thread::current().id()).unwrap();
        wait.recv_timeout(WAIT).unwrap();
    }));
    let mut release = Release(Some(release));
    let cards = f.app.durable.cards.len();
    let receipt = during_preparation(&mut f, saved.clone(), |f| {
        assert!(
            reads.lock().unwrap().is_empty(),
            "no owner/protection IO at entry"
        );
        assert!(f.app.resume_ui.open && f.app.resume_ui.loading);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_eq!(f.app.durable.cards.len(), cards);
        let search = f.app.settings_search.clone();
        drop(
            f.app
                .update(Message::SettingsSearchChanged("input during resume".into())),
        );
        // Picker modality deliberately refuses unrelated workspace input. A
        // blocked worker must not change that policy or cause UI owner IO.
        assert_eq!(f.app.settings_search, search);
        drop(f.app.update(Message::Frame(Instant::now())));
        assert!(reads.lock().unwrap().is_empty());
    });
    assert_eq!(
        f.app.durable.cards.len(),
        cards,
        "receipt delivery is not commit"
    );
    assert!(!reads.lock().unwrap().is_empty());
    assert!(reads.lock().unwrap().iter().all(|thread| *thread != ui));
    let retirement = consume(&mut f.app, receipt);
    assert_ne!(observe.recv_timeout(WAIT).unwrap(), ui);
    assert!(
        f.app.worktree_selection.is_active(),
        "receipt delivery did not free its worker slot"
    );
    assert!(
        protected(&f.runtime, &f.old),
        "old guard remains until actual retirement"
    );
    assert!(
        protected(&f.runtime, &f.next),
        "new scope is already protected"
    );
    assert!(retired_on.lock().unwrap().is_empty());
    release.release();
    let next = retire(&mut f.app, retirement);
    assert!(!f.app.worktree_selection.is_active());
    drop(next);
    resumed(&f.app, &saved);
    assert!(!f.app.resume_ui.open);
    assert!(!f.app.worktree_selection.is_active());
    assert!(protected(&f.runtime, &f.next));
    assert!(!protected(&f.runtime, &f.old));
    assert!(reads.lock().unwrap().iter().all(|thread| *thread != ui));
    assert!(!retired_on.lock().unwrap().is_empty());
    assert!(
        retired_on
            .lock()
            .unwrap()
            .iter()
            .all(|thread| *thread != ui)
    );
}

fn remembered_channel(f: &mut Fixture, saved: &SavedConversation) -> (ChannelId, PathBuf) {
    let path = f.old.parent().unwrap().join("remembered-filing");
    std::fs::create_dir(&path).unwrap();
    let project = folder(&mut f.app, &path);
    let channel = ChannelId::new();
    Arc::make_mut(&mut f.app.durable)
        .channels
        .push(crate::model::ProjectChannel {
            id: channel,
            project_id: project,
            title: "Renamed remembered channel".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: false,
            cards: vec![saved.card],
            focused: Some(saved.card),
        });
    let mut closed = crate::app::tests::terminal_card_state();
    closed.id = saved.card;
    closed.kind = CardKind::Chat(saved.session.clone());
    // Use the real account-aware mapping writer, not a duplicated key formula.
    f.app.remember_closed_conversation_channel(&closed);
    let state = Arc::make_mut(&mut f.app.durable);
    let row = state
        .channels
        .iter_mut()
        .find(|row| row.id == channel)
        .unwrap();
    row.cards.clear();
    row.focused = None;
    assert_eq!(state.conversation_channels.len(), 1);
    (channel, path)
}

fn succeeded(message: &Message) -> bool {
    let Message::WorktreeNavigationPrepared { receipt, .. } = message else {
        panic!("actual owning receipt")
    };
    receipt
        .client
        .shared
        .slot
        .lock()
        .unwrap()
        .result
        .as_ref()
        .unwrap()
        .is_ok()
}

#[cfg(unix)]
#[test]
fn resume_revalidates_alias_after_watch_registration_before_guarding() {
    let mut f = fixture();
    let first = f.next.clone();
    let second = f.old.parent().unwrap().join("new-alias-owner");
    let alias = f.old.parent().unwrap().join("resume-alias");
    std::fs::create_dir(&second).unwrap();
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    folder(&mut f.app, &second);
    folder(&mut f.app, &alias);
    let saved = entry(&alias);
    open_picker(&mut f.app);
    let swapped = Arc::new(AtomicUsize::new(0));
    let count = swapped.clone();
    let target = second.clone();
    let link = alias.clone();
    f.app.worktree_selection.before_resume_resolve = Some(Arc::new(move |path| {
        assert_eq!(path, link);
        if count.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
        }
    }));
    let original = f.app.durable.clone();
    let receipt = prepared(f.app.resume_saved_conversation(saved.clone()));
    assert!(swapped.load(AtomicOrdering::SeqCst) > 0);
    assert!(
        Arc::ptr_eq(&original, &f.app.durable),
        "no exposure before commit"
    );
    assert!(
        !protected(&f.runtime, &first),
        "old physical owner is never admitted"
    );
    let accepted = succeeded(&receipt);
    if accepted {
        assert!(
            protected(&f.runtime, &second),
            "fresh owner guarded before exposure"
        );
    }
    // A watch epoch change may conservatively refuse rather than commit the
    // new owner. Both outcomes must retire the original real worker.
    settle(&mut f.app, receipt);
    assert!(!f.app.worktree_selection.is_active());
    assert!(!protected(&f.runtime, &first));
    if !f.app.durable.cards.is_empty() {
        resumed(&f.app, &saved);
        assert!(protected(&f.runtime, &second));
    } else {
        // The watch notification may arrive after successful preparation but
        // before its UI consumption. That late revocation must also refuse.
        assert!(Arc::ptr_eq(&original, &f.app.durable));
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert!(!protected(&f.runtime, &second));
        assert!(f.app.resume_ui.open && !f.app.resume_ui.loading);
        assert!(f.app.resume_ui.error.is_some());
        f.app.worktree_selection.before_resume_resolve = None;
        let retry = prepared(f.app.resume_saved_conversation(saved.clone()));
        assert!(succeeded(&retry), "stable retargeted owner can be retried");
        settle(&mut f.app, retry);
        resumed(&f.app, &saved);
        assert!(protected(&f.runtime, &second));
    }
}

#[cfg(unix)]
#[test]
fn resume_retarget_during_guard_acquisition_rejects_and_releases_guard() {
    let mut f = fixture();
    let first = f.next.clone();
    let second = f.old.parent().unwrap().join("retargeted-during-acquire");
    let alias = f.old.parent().unwrap().join("acquire-alias");
    std::fs::create_dir(&second).unwrap();
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    folder(&mut f.app, &second);
    folder(&mut f.app, &alias);
    let saved = entry(&alias);
    open_picker(&mut f.app);
    let observed = Arc::new(AtomicUsize::new(0));
    let capture = observed.clone();
    let runtime = f.runtime.clone();
    let old_owner = first.clone();
    let link = alias.clone();
    let target = second.clone();
    f.app.worktree_selection.after_acquire = Some(Arc::new(move |_| {
        assert!(
            protected(&runtime, &old_owner),
            "original worker really acquired guard"
        );
        if capture.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
        }
    }));
    let original = f.app.durable.clone();
    let focused = f.app.workspace.focused();
    let receipt = prepared(f.app.resume_saved_conversation(saved));
    assert!(observed.load(AtomicOrdering::SeqCst) > 0);
    assert!(
        !succeeded(&receipt),
        "post-acquisition retarget invalidates proof"
    );
    settle(&mut f.app, receipt);
    assert!(Arc::ptr_eq(&original, &f.app.durable));
    assert_eq!(f.app.workspace.focused(), focused);
    assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
    assert!(f.app.resume_ui.open && !f.app.resume_ui.loading);
    assert!(f.app.resume_ui.error.is_some());
    assert!(!f.app.worktree_selection.is_active());
    assert!(!protected(&f.runtime, &first));
    assert!(!protected(&f.runtime, &second));
    assert!(protected(&f.runtime, &f.old));
}

#[test]
fn resumed_filing_protects_independent_launch_and_busy_channel_falls_back() {
    for busy_channel in [false, true] {
        let mut f = fixture();
        let saved = entry(&f.next);
        let (channel, filing) = remembered_channel(&mut f, &saved);
        let mapping = f.app.durable.conversation_channels.clone();
        let exclusive = busy_channel.then(|| {
            crate::worktree_cleanup::fixture_protection(&f.runtime, &filing, true).unwrap()
        });
        open_picker(&mut f.app);
        let receipt = prepared(f.app.resume_saved_conversation(saved.clone()));
        assert!(succeeded(&receipt));
        assert!(
            protected(&f.runtime, &f.next),
            "immutable launch protected before exposure"
        );
        if !busy_channel {
            assert!(
                protected(&f.runtime, &filing),
                "independent filing protected before exposure"
            );
        }
        assert!(f.app.durable.cards.is_empty());
        settle(&mut f.app, receipt);
        let card = resumed(&f.app, &saved);
        assert_eq!(f.app.durable.conversation_channels, mapping);
        assert!(
            protected(&f.runtime, &f.next),
            "launch survives worker retirement"
        );
        assert!(!f.app.worktree_selection.is_active());
        if busy_channel {
            assert_eq!(f.app.active_workspace.as_ref(), Some(&f.next));
            assert_eq!(f.app.durable.active_channel, None);
            assert!(
                !f.app
                    .durable
                    .channels
                    .iter()
                    .find(|row| row.id == channel)
                    .unwrap()
                    .cards
                    .contains(&card)
            );
            drop(exclusive);
            assert!(
                !protected(&f.runtime, &filing),
                "failed candidate leaked no filing guard"
            );
        } else {
            assert_eq!(f.app.active_workspace.as_ref(), Some(&filing));
            assert_eq!(f.app.durable.active_channel, Some(channel));
            assert!(
                f.app
                    .durable
                    .channels
                    .iter()
                    .find(|row| row.id == channel)
                    .unwrap()
                    .cards
                    .contains(&card)
            );
            assert!(
                protected(&f.runtime, &filing),
                "filing survives worker retirement"
            );
        }
    }
}

#[test]
fn unavailable_launch_lease_refuses_all_dependent_changes_and_retry_succeeds() {
    let mut f = fixture();
    let saved = entry(&f.next);
    let (channel, filing) = remembered_channel(&mut f, &saved);
    let exclusive = crate::worktree_cleanup::fixture_protection(&f.runtime, &f.next, true).unwrap();
    open_picker(&mut f.app);
    let original = f.app.durable.clone();
    let order = f.app.workspace.order().collect::<Vec<_>>();
    let focus = f.app.workspace.focused();
    let receipt = prepared(f.app.resume_saved_conversation(saved.clone()));
    assert!(
        !succeeded(&receipt),
        "required immutable launch lease cannot be waived"
    );
    settle(&mut f.app, receipt);
    assert!(
        Arc::ptr_eq(&original, &f.app.durable),
        "cards, filing, membership and recents unchanged"
    );
    assert_eq!(f.app.workspace.order().collect::<Vec<_>>(), order);
    assert_eq!(f.app.workspace.focused(), focus);
    assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
    assert_eq!(f.app.durable.active_channel, None);
    assert!(f.app.resume_ui.open && !f.app.resume_ui.loading);
    assert!(f.app.resume_ui.error.is_some());
    assert!(!f.app.worktree_selection.is_active());
    assert!(
        !protected(&f.runtime, &filing),
        "partial candidate guard retired"
    );
    assert!(protected(&f.runtime, &f.old), "old browsing guard retained");
    drop(exclusive);
    let receipt = prepared(f.app.resume_saved_conversation(saved.clone()));
    assert!(succeeded(&receipt));
    settle(&mut f.app, receipt);
    resumed(&f.app, &saved);
    assert_eq!(f.app.durable.active_channel, Some(channel));
    assert_eq!(f.app.active_workspace.as_ref(), Some(&filing));
    assert!(protected(&f.runtime, &f.next));
    assert!(protected(&f.runtime, &filing));
}

#[test]
fn missing_history_fallback_revalidates_a_launch_directory_created_before_its_proof() {
    for appears in [false, true] {
        let mut f = fixture();
        let missing = f.old.parent().unwrap().join("missing-recorded-launch");
        let mut saved = entry(&missing);
        // Exact saved history metadata, not fresh Claude policy discovery or
        // an authenticated/runtime capability. No provider is contacted.
        saved.session.launch.provider = Provider::Claude;
        saved.session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
        saved.session.launch.recipe.program = "claude".into();
        saved.session.launch.recipe.arguments.clear();
        saved.session.launch.recipe.environment.remove("CODEX_HOME");
        saved.session.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            f.old
                .parent()
                .unwrap()
                .join("recorded-claude-account")
                .to_string_lossy()
                .into_owned(),
        );
        saved
            .session
            .launch
            .recipe
            .unset_environment
            .remove("CLAUDE_CONFIG_DIR");
        assert!(saved.session.launch.needs_read_only_history());
        open_picker(&mut f.app);
        let original = f.app.durable.clone();
        let focus = f.app.workspace.focused();
        let (at_fallback, observe) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        let path = missing.clone();
        let ui = std::thread::current().id();
        f.app.worktree_selection.before_missing_resume = Some(Arc::new(move |launch| {
            assert_eq!(launch, path);
            assert!(
                !launch.exists(),
                "actual failed launch choices precede this barrier"
            );
            at_fallback.send(std::thread::current().id()).unwrap();
            wait.recv_timeout(WAIT).unwrap();
            if appears {
                std::fs::create_dir(launch).unwrap();
            }
        }));
        let task = f.app.resume_saved_conversation(saved.clone());
        let receipt = std::thread::scope(|scope| {
            let worker = scope.spawn(move || prepared(task));
            let mut release = Release(Some(release));
            assert_ne!(observe.recv_timeout(WAIT).unwrap(), ui);
            assert!(Arc::ptr_eq(&original, &f.app.durable));
            assert!(f.app.resume_ui.open && f.app.resume_ui.loading);
            assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
            assert_eq!(f.app.workspace.focused(), focus);
            assert!(!missing.exists());
            assert!(protected(&f.runtime, &f.old));
            release.release();
            worker.join().unwrap()
        });
        if appears {
            assert!(
                missing.is_dir(),
                "real task-private path appeared inside the worker gap"
            );
            assert!(
                !succeeded(&receipt),
                "newly available launch invalidates KeepScope classification"
            );
            settle(&mut f.app, receipt);
            assert!(Arc::ptr_eq(&original, &f.app.durable));
            assert!(
                f.app.cards.is_empty(),
                "no conversation/start runtime installed on refusal"
            );
            assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
            assert_eq!(f.app.workspace.focused(), focus);
            assert!(f.app.resume_ui.open && !f.app.resume_ui.loading);
            assert!(f.app.resume_ui.error.is_some());
            assert!(!f.app.worktree_selection.is_active());
            assert!(protected(&f.runtime, &f.old));
            assert!(
                !protected(&f.runtime, &missing),
                "refused proof leaves no launch guard behind"
            );
            f.app.worktree_selection.before_missing_resume = None;
            let retry = prepared(f.app.resume_saved_conversation(saved.clone()));
            assert!(succeeded(&retry));
            assert!(
                protected(&f.runtime, &missing),
                "fresh available launch guarded before exposure"
            );
            settle(&mut f.app, retry);
            resumed(&f.app, &saved);
            assert_eq!(
                f.app.active_workspace.as_ref(),
                Some(&missing),
                "retry selects P rather than stale KeepScope"
            );
            assert!(protected(&f.runtime, &missing));
        } else {
            assert!(
                succeeded(&receipt),
                "genuinely missing exact history remains readable"
            );
            settle(&mut f.app, receipt);
            resumed(&f.app, &saved);
            assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
            assert!(protected(&f.runtime, &f.old));
            assert!(
                !missing.exists(),
                "history fallback does not manufacture launch authority"
            );
        }
        assert!(!f.app.worktree_selection.is_active());
    }
}

#[test]
fn adoption_while_resume_prepares_reuses_exact_identity_in_its_current_channel() {
    let mut f = fixture();
    let saved = entry(&f.next);
    let (_remembered, remembered_path) = remembered_channel(&mut f, &saved);
    let mapping = f.app.durable.conversation_channels.clone();
    let adopted_path = f.old.parent().unwrap().join("adopted-current-filing");
    std::fs::create_dir(&adopted_path).unwrap();
    let project = folder(&mut f.app, &adopted_path);
    let current_channel = ChannelId::new();
    Arc::make_mut(&mut f.app.durable)
        .channels
        .push(crate::model::ProjectChannel {
            id: current_channel,
            project_id: project,
            title: "Concurrent actual filing".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: false,
            cards: Vec::new(),
            focused: None,
        });
    open_picker(&mut f.app);
    let mut adopted = crate::app::tests::terminal_card_state();
    adopted.kind = CardKind::Chat(saved.session.clone());
    adopted.title = "Concurrent adopted conversation".into();
    let adopted_id = adopted.id;
    assert_ne!(adopted_id, saved.card);
    let mut other_account = adopted.clone();
    other_account.id = CardId::new();
    let CardKind::Chat(other_session) = &mut other_account.kind else {
        unreachable!()
    };
    other_session.launch.recipe.environment.insert(
        "CODEX_HOME".into(),
        f.old
            .parent()
            .unwrap()
            .join("different-recorded-account")
            .to_string_lossy()
            .into_owned(),
    );
    let other_id = other_account.id;
    let receipt = during_preparation(&mut f, saved.clone(), |f| {
        // Controlled arrival of the same durable descriptor, as opposed to
        // invoking insert_chat_card and discarding a provider-start Task.
        // The other channel is not mounted in the currently browsed strip.
        let state = Arc::make_mut(&mut f.app.durable);
        // A same-provider/session/cwd descriptor in a different recorded
        // account deliberately comes first. It must never win adoption.
        state.cards.push(other_account);
        state.cards.push(adopted);
        let channel = state
            .channels
            .iter_mut()
            .find(|row| row.id == current_channel)
            .unwrap();
        channel.cards.push(other_id);
        channel.cards.push(adopted_id);
        channel.focused = Some(adopted_id);
        f.app.cards.insert(other_id, CardRuntime::Loading);
        f.app.cards.insert(adopted_id, CardRuntime::Loading);
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_ne!(f.app.workspace.focused(), Some(adopted_id));
    });
    // First receipt captured the old remembered channel. Consuming it must
    // retire and reprepare current adoption metadata, never focus early.
    let retirement = consume(&mut f.app, receipt);
    assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
    assert_ne!(f.app.workspace.focused(), Some(adopted_id));
    assert_eq!(f.app.durable.cards.len(), 2);
    let next = retire(&mut f.app, retirement);
    assert!(
        f.app.worktree_selection.is_active(),
        "fresh scope waits for actual old retirement"
    );
    let current = prepared(next);
    assert!(succeeded(&current));
    assert!(
        protected(&f.runtime, &adopted_path),
        "current filing guarded before focus"
    );
    settle(&mut f.app, current);
    assert_eq!(resumed(&f.app, &saved), adopted_id);
    assert_eq!(
        f.app.durable.cards.len(),
        2,
        "no third duplicate conversation"
    );
    assert_eq!(f.app.durable.active_channel, Some(current_channel));
    assert_eq!(f.app.active_workspace.as_ref(), Some(&adopted_path));
    assert_eq!(f.app.workspace.focused(), Some(adopted_id));
    assert_eq!(
        f.app.durable.conversation_channels, mapping,
        "adoption does not rewrite saved filing"
    );
    assert!(protected(&f.runtime, &f.next));
    assert!(protected(&f.runtime, &adopted_path));
    assert!(
        !protected(&f.runtime, &remembered_path),
        "obsolete filing retires"
    );
    assert!(
        !f.app.chat_start_pending(adopted_id),
        "reuse does not start a duplicate actor"
    );
    assert!(!f.app.worktree_selection.is_active());

    // An already-adopted channel can change project association while keeping
    // the exact same channel/card/path. That metadata is part of its receipt,
    // even though generic sidebar/card/path snapshots do not change here.
    let alternate_project = folder(&mut f.app, &adopted_path);
    assert_ne!(alternate_project, project);
    let back = prepared(f.app.navigate_worktree(f.old.clone(), false, None));
    settle(&mut f.app, back);
    assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
    assert_ne!(f.app.workspace.focused(), Some(adopted_id));
    open_picker(&mut f.app);
    let sidebar = f.app.sidebar_model.projects.clone();
    let receipt = during_preparation(&mut f, saved.clone(), |f| {
        let state = Arc::make_mut(&mut f.app.durable);
        let row = state
            .channels
            .iter_mut()
            .find(|row| row.id == current_channel)
            .unwrap();
        assert_eq!(row.project_id, project);
        row.project_id = alternate_project;
        assert_eq!(
            f.app.channel_directory(current_channel).as_ref(),
            Some(&adopted_path)
        );
        assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
        assert_ne!(f.app.workspace.focused(), Some(adopted_id));
    });
    assert_eq!(
        f.app.sidebar_model.projects, sidebar,
        "only channel association changed during preparation"
    );
    let retirement = consume(&mut f.app, receipt);
    assert!(
        f.app.resume_ui.open && f.app.resume_ui.loading,
        "stale project proof cannot close picker"
    );
    assert_eq!(f.app.active_workspace.as_ref(), Some(&f.old));
    assert_ne!(f.app.workspace.focused(), Some(adopted_id));
    let next = retire(&mut f.app, retirement);
    assert!(
        f.app.worktree_selection.is_active(),
        "project reparent requires a fresh original worker"
    );
    let current = prepared(next);
    assert!(succeeded(&current));
    settle(&mut f.app, current);
    assert_eq!(resumed(&f.app, &saved), adopted_id);
    assert_eq!(f.app.durable.cards.len(), 2);
    assert_eq!(f.app.durable.active_channel, Some(current_channel));
    assert_eq!(f.app.active_workspace.as_ref(), Some(&adopted_path));
    assert_eq!(f.app.workspace.focused(), Some(adopted_id));
    assert_eq!(
        f.app
            .durable
            .channels
            .iter()
            .find(|row| row.id == current_channel)
            .unwrap()
            .project_id,
        alternate_project
    );
    assert_eq!(f.app.durable.conversation_channels, mapping);
    assert!(!f.app.chat_start_pending(adopted_id));
    assert!(!f.app.worktree_selection.is_active());
}
