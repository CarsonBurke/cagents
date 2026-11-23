//! Drive the owning poll/scan/configure/drain path with one injected quota
//! refusal. All other registrations and the recovered ref event are native.
use super::*;
use notify::{Event, RecursiveMode, Watcher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct QuotaWatcher {
    native: notify::RecommendedWatcher,
    refs: PathBuf,
    refuse: Arc<AtomicBool>,
    attempts: Arc<AtomicUsize>,
}

impl Watcher for QuotaWatcher {
    fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
        unreachable!("the fixture constructs its native backend with its exact callback")
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        if path == self.refs {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.refuse.load(Ordering::SeqCst) {
                return Err(
                    notify::Error::new(notify::ErrorKind::MaxFilesWatch).add_path(path.to_owned())
                );
            }
        }
        self.native.watch(path, mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.native.unwatch(path)
    }

    fn kind() -> notify::WatcherKind {
        notify::RecommendedWatcher::kind()
    }
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "commit.gpgsign=false"])
        .args(arguments)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(output.status.success(), "{arguments:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn fixture_handle() -> Handle {
    Handle(Arc::new(Inner {
        host: Weak::new(),
        shared: Mutex::new(Shared {
            projects: BTreeMap::new(),
            listings: forge::Listings::default(),
            pins: BTreeMap::new(),
            session_activity: HashMap::new(),
            in_use: Arc::default(),
            tickets: BTreeMap::new(),
            linear_unavailable: None,
            stale_after_days: crate::work::DEFAULT_STALE_AFTER_DAYS,
            board_read_at: None,
            watching: false,
            restored: BTreeMap::new(),
        }),
        updated: Condvar::new(),
        scanner: Signal::default(),
        forge: Signal::default(),
        publisher: Signal::default(),
        publication: Mutex::new(Publication::default()),
        jobs: Jobs::default(),
        store: Mutex::new(None),
        conversations: conversations::Conversations::default(),
        cancellation: CancellationToken::new(),
    }))
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "native watch transition did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn complete_scan(scanner: &mut Scanner, handle: &Handle) {
    complete_scan_at(scanner, handle, now_unix_ms());
}

/// Completes a scan as if at `now_unix_ms`, so a fixture made just now can
/// stand for worktrees nobody has touched for a while.
fn complete_scan_at(scanner: &mut Scanner, handle: &Handle, now_unix_ms: u64) {
    scanner.dispatch(handle);
    assert_eq!(handle.0.jobs.state.lock().unwrap().queue.len(), 1);
    let job = handle.0.jobs.next().unwrap();
    let result = scan::scan(scan::Input {
        id: job.id,
        repository: &job.repository,
        previous: job.previous.as_deref(),
        change: &job.change,
        full: job.full,
        in_use: &job.in_use,
        hints: &job.hints,
        now_unix_ms,
        cancellation: &handle.0.cancellation,
    });
    assert!(result.is_ok(), "real Git scan: {result:?}");
    handle.publish(job, result);
}

fn coverage(scanner: &mut Scanner, handle: &Handle, id: ProjectId, expected: bool) {
    wait_until(|| {
        scanner
            .monitor
            .as_ref()
            .unwrap()
            .pending_coverage_for_test(id)
            == Some(expected)
    });
    scanner.drain(handle);
    assert_eq!(
        handle.shared().projects.values().next().unwrap().covered,
        expected
    );
}

#[test]
fn uncovered_periodic_scans_retry_native_watches_without_a_topology_event() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("repository");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Watch fixture"]);
    git(&root, &["config", "user.email", "watch@example.invalid"]);
    std::fs::write(root.join("README"), "tracked\n").unwrap();
    git(&root, &["add", "README"]);
    git(&root, &["commit", "-qm", "fixture"]);
    let oid = git(&root, &["rev-parse", "HEAD"]);
    let root = root.canonicalize().unwrap();
    let repository = root.join(".git");
    let refs = repository.join("refs");
    let handle = fixture_handle();
    let id = handle.shared().track(&repository, "fixture").id;
    let refuse = Arc::new(AtomicBool::new(true));
    let attempts = Arc::new(AtomicUsize::new(0));
    let monitor = {
        let signal = Arc::downgrade(&handle.0);
        let refuse = refuse.clone();
        let attempts = attempts.clone();
        git_watch::Monitor::start_for_test(
            move || {
                if let Some(inner) = signal.upgrade() {
                    inner.scanner.notify();
                }
            },
            move |handler: Box<dyn FnMut(notify::Result<Event>) + Send>| {
                Ok(QuotaWatcher {
                    // The production backend's configuration, kernel mask
                    // included.
                    native: notify::RecommendedWatcher::new(
                        handler,
                        notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
                    )?,
                    refs: refs.clone(),
                    refuse: refuse.clone(),
                    attempts: attempts.clone(),
                })
            },
        )
        .unwrap()
    };
    let mut scanner = Scanner {
        monitor: Some(monitor),
        configured: BTreeMap::new(),
        stamped: HashMap::new(),
        sessions: Vec::new(),
        sampled: None,
        refreshed: None,
    };
    complete_scan(&mut scanner, &handle);
    let original = handle.shared().projects[&repository]
        .local
        .as_ref()
        .unwrap()
        .plan
        .clone()
        .unwrap();
    scanner.configure(&handle);
    coverage(&mut scanner, &handle, id, false);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(
        handle.shared().projects[&repository].watch_recoverable,
        "a native quota refusal may recover on a later registration"
    );
    // configure only signals the monitor thread, so judge a wrongly issued
    // reconfiguration by the scanner's own record, not by watch attempts that
    // may not have happened yet.
    let configured = |scanner: &Scanner| {
        let configured = &scanner.configured[&id];
        (configured.scan, configured.attempted)
    };

    // Initial registration can request one reconciliation scan. Its unchanged
    // plan and all ordinary coordinator wakes stay inside the retry backoff.
    handle
        .shared()
        .projects
        .get_mut(&repository)
        .unwrap()
        .last_scan = Some(Instant::now() - SCAN_SPACING);
    complete_scan(&mut scanner, &handle);
    let before = configured(&scanner);
    for _ in 0..100 {
        scanner.configure(&handle);
    }
    assert_eq!(configured(&scanner), before);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    // Advance only the existing scheduler's timestamps, avoiding a real
    // two-minute sleep. dispatch generates the real fallback poll, the worker
    // runs Git, and publish keeps an unchanged watch plan.
    let poll = |scanner: &mut Scanner| {
        {
            let mut shared = handle.shared();
            let tracked = shared.projects.get_mut(&repository).unwrap();
            tracked.last_scan = Some(Instant::now() - IDLE_POLL);
            let mut local = (**tracked.local.as_ref().unwrap()).clone();
            local.listed_at = Some(Instant::now() - IDLE_POLL);
            tracked.local = Some(Arc::new(local));
        }
        scanner.configured.get_mut(&id).unwrap().attempted = Instant::now() - ACTIVE_POLL;
        complete_scan(scanner, &handle);
        assert_eq!(
            &original,
            handle.shared().projects[&repository]
                .local
                .as_ref()
                .unwrap()
                .plan
                .as_ref()
                .unwrap()
        );
        scanner.configure(&handle);
    };
    poll(&mut scanner);
    coverage(&mut scanner, &handle, id, false);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    // Capacity can return in an unrelated process with no Git event here.
    // A completed fallback poll alone must retry and acknowledge real coverage.
    refuse.store(false, Ordering::SeqCst);
    let before = configured(&scanner);
    for _ in 0..100 {
        scanner.configure(&handle);
    }
    assert_eq!(configured(&scanner), before);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    poll(&mut scanner);
    coverage(&mut scanner, &handle, id, true);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    // Native metadata events now reach Work, and the next real scan makes the
    // new branch current. The fixture never manually reconfigures the monitor.
    std::fs::write(root.join(".git/refs/heads/recovered"), format!("{oid}\n")).unwrap();
    wait_until(|| {
        scanner.drain(&handle);
        handle.shared().projects[&repository]
            .pending
            .as_ref()
            .is_some_and(|change| change.references)
    });
    handle
        .shared()
        .projects
        .get_mut(&repository)
        .unwrap()
        .last_scan = Some(Instant::now() - SCAN_SPACING);
    complete_scan(&mut scanner, &handle);
    assert!(
        handle.shared().projects[&repository]
            .local
            .as_ref()
            .unwrap()
            .branch_names
            .contains("recovered")
    );
    let before = configured(&scanner);
    scanner.configure(&handle);
    assert_eq!(
        configured(&scanner).0,
        before.0,
        "a covered project's new scan does not reconfigure the monitor"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "healthy plans retain their registrations"
    );
}

#[test]
fn an_unwatched_change_is_read_while_a_watched_worktree_keeps_changing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("repository");
    let linked = directory.path().join("linked");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Watch fixture"]);
    git(&root, &["config", "user.email", "watch@example.invalid"]);
    std::fs::write(root.join("README"), "tracked\n").unwrap();
    git(&root, &["add", "README"]);
    git(&root, &["commit", "-qm", "fixture"]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    let repository = root.canonicalize().unwrap().join(".git");
    let handle = fixture_handle();
    handle.shared().track(&repository, "fixture");
    let mut scanner = Scanner {
        monitor: None,
        configured: BTreeMap::new(),
        stamped: HashMap::new(),
        sessions: Vec::new(),
        sampled: None,
        refreshed: None,
    };
    complete_scan(&mut scanner, &handle);
    let (primary, unwatched) = {
        let shared = handle.shared();
        let tracked = &shared.projects[&repository];
        assert!(!tracked.covered);
        let local = tracked.local.as_ref().unwrap();
        assert_eq!(local.worktrees.len(), 2);
        (
            local
                .worktrees
                .iter()
                .find(|worktree| worktree.primary)
                .unwrap()
                .id,
            local
                .worktrees
                .iter()
                .find(|worktree| !worktree.primary)
                .unwrap()
                .clone(),
        )
    };
    assert_eq!(unwatched.dirty.as_ref().unwrap().paths, 0);
    std::fs::write(linked.join("unwatched-change"), "new file\n").unwrap();

    for pass in 0..3 {
        std::fs::write(root.join("watched-change"), format!("edit {pass}\n")).unwrap();
        {
            let mut shared = handle.shared();
            let tracked = shared.projects.get_mut(&repository).unwrap();
            tracked.pending = Some(git_watch::Change {
                worktrees: BTreeSet::from([primary]),
                ..git_watch::Change::default()
            });
            tracked.last_scan = Some(Instant::now() - SCAN_SPACING);
        }
        complete_scan(&mut scanner, &handle);
        let shared = handle.shared();
        let linked = shared.projects[&repository]
            .local
            .as_ref()
            .unwrap()
            .worktrees
            .iter()
            .find(|worktree| worktree.id == unwatched.id)
            .unwrap();
        assert_eq!(linked.dirty, unwatched.dirty);
        assert_eq!(linked.status_read_at, unwatched.status_read_at);
    }

    {
        let mut shared = handle.shared();
        let tracked = shared.projects.get_mut(&repository).unwrap();
        assert!(tracked.pending.is_none());
        let mut local = (**tracked.local.as_ref().unwrap()).clone();
        local
            .worktrees
            .iter_mut()
            .find(|worktree| worktree.id == unwatched.id)
            .unwrap()
            .status_read_at = Some(Instant::now() - IDLE_POLL);
        tracked.local = Some(Arc::new(local));
        tracked.last_scan = Some(Instant::now() - SCAN_SPACING);
    }
    complete_scan(&mut scanner, &handle);
    let shared = handle.shared();
    let linked = shared.projects[&repository]
        .local
        .as_ref()
        .unwrap()
        .worktrees
        .iter()
        .find(|worktree| worktree.id == unwatched.id)
        .unwrap();
    assert_eq!(linked.dirty.as_ref().unwrap().paths, 1);
}

/// The regression test for an idle daemon: with Git's bookkeeping watched,
/// nothing is due however long it waits, and a Git event in one idle
/// worktree reads that worktree's status alone, without listing the
/// worktrees or reading the refs again.
#[test]
fn an_idle_board_runs_no_git_and_an_event_reads_only_its_worktree() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("repository");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Watch fixture"]);
    git(&root, &["config", "user.email", "watch@example.invalid"]);
    std::fs::write(root.join("README"), "tracked\n").unwrap();
    git(&root, &["add", "README"]);
    git(&root, &["commit", "-qm", "fixture"]);
    let linked = (0..12)
        .map(|index| {
            let path = directory.path().join(format!("w{index}"));
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("w{index}"),
                    path.to_str().unwrap(),
                ],
            );
            path.canonicalize().unwrap()
        })
        .collect::<Vec<_>>();
    let repository = root.canonicalize().unwrap().join(".git");
    let handle = fixture_handle();
    let id = handle.shared().track(&repository, "fixture").id;
    let monitor = {
        let signal = Arc::downgrade(&handle.0);
        git_watch::Monitor::start(move || {
            if let Some(inner) = signal.upgrade() {
                inner.scanner.notify();
            }
        })
        .unwrap()
    };
    let mut scanner = Scanner {
        monitor: Some(monitor),
        configured: BTreeMap::new(),
        stamped: HashMap::new(),
        sessions: Vec::new(),
        sampled: None,
        refreshed: None,
    };
    let local = |handle: &Handle| handle.shared().projects[&repository].local.clone().unwrap();
    let worktree = |local: &scan::Local, path: &Path| {
        local
            .worktrees
            .iter()
            .find(|worktree| worktree.path == path)
            .unwrap()
            .clone()
    };
    // The worktrees were made just now; scans as of a day later find them
    // idle, so only Git's bookkeeping is watched in them.
    let later = now_unix_ms() + 24 * 60 * 60 * 1000;
    let rescan = |scanner: &mut Scanner| {
        handle
            .shared()
            .projects
            .get_mut(&repository)
            .unwrap()
            .last_scan = Some(Instant::now() - SCAN_SPACING);
        complete_scan_at(scanner, &handle, later);
        scanner.configure(&handle);
    };
    // Waits for the watcher to report something for the project, and takes
    // it.
    let drained = |scanner: &mut Scanner, until: &dyn Fn(&Tracked) -> bool| {
        wait_until(|| {
            scanner.drain(&handle);
            until(&handle.shared().projects[&repository])
        });
    };

    complete_scan_at(&mut scanner, &handle, later);
    scanner.configure(&handle);
    coverage(&mut scanner, &handle, id, true);
    // The registration asks for one read of what it now watches.
    if handle.shared().projects[&repository].pending.is_some() {
        rescan(&mut scanner);
    }
    {
        let shared = handle.shared();
        let tracked = &shared.projects[&repository];
        let unwatched = tracked.unwatched.as_ref().expect("bookkeeping is watched");
        let local = tracked.local.as_ref().unwrap();
        for path in &linked {
            assert!(
                unwatched.contains(&worktree(local, path).id),
                "an idle linked worktree's files are not watched"
            );
        }
        assert!(
            !unwatched.contains(&local.worktrees[0].id),
            "the primary's are"
        );
    }

    // Idle: the daemon's own status reads take no optional locks, so they
    // wake nothing, and no poll comes due however long it waits.
    std::thread::sleep(Duration::from_millis(300));
    scanner.drain(&handle);
    let mut shared = handle.shared();
    assert!(shared.projects[&repository].pending.is_none());
    assert_eq!(shared.until_scan(Instant::now()), None);
    shared.queue_polls(Instant::now() + Duration::from_secs(7 * 24 * 60 * 60));
    assert!(shared.projects[&repository].pending.is_none());
    drop(shared);

    // Staging in an idle worktree is a Git event for it alone. Its scan reads
    // that worktree, and not the worktree list or the refs.
    let before = local(&handle);
    std::fs::write(linked[7].join("README"), "staged\n").unwrap();
    git(&linked[7], &["add", "README"]);
    drained(&mut scanner, &|tracked| {
        tracked
            .pending
            .as_ref()
            .is_some_and(|change| !change.worktrees.is_empty())
    });
    {
        let shared = handle.shared();
        let change = shared.projects[&repository].pending.clone().unwrap();
        assert_eq!(
            change.worktrees,
            BTreeSet::from([worktree(&before, &linked[7]).id])
        );
        assert!(!change.topology && !change.references);
    }
    rescan(&mut scanner);
    let after = local(&handle);
    assert_eq!(after.listed_at, before.listed_at, "the list was not read");
    for path in &linked {
        let (was, now) = (worktree(&before, path), worktree(&after, path));
        if *path == linked[7] {
            assert_eq!(now.dirty.as_ref().unwrap().paths, 1);
            assert!(now.status_read_at > was.status_read_at);
        } else {
            assert_eq!(now.status_read_at, was.status_read_at, "{path:?} was read");
        }
    }

    // A session in a worktree has its files watched, so an edit there with
    // nothing staged arrives as an event.
    {
        let mut shared = handle.shared();
        shared.in_use = Arc::new(BTreeSet::from([linked[3].clone()]));
        shared
            .projects
            .get_mut(&repository)
            .unwrap()
            .pending
            .get_or_insert_with(Default::default);
    }
    rescan(&mut scanner);
    let in_use = worktree(&local(&handle), &linked[3]).id;
    // The worktree just edited has its files watched too, within the
    // budget: an agent elsewhere may be editing it by path.
    let changed = worktree(&local(&handle), &linked[7]).id;
    drained(&mut scanner, &|tracked| {
        tracked
            .unwatched
            .as_ref()
            .is_some_and(|unwatched| !unwatched.contains(&in_use) && !unwatched.contains(&changed))
    });
    let idle = worktree(&local(&handle), &linked[5]).id;
    assert!(
        handle.shared().projects[&repository]
            .unwatched
            .as_ref()
            .unwrap()
            .contains(&idle),
        "an idle one's still are not"
    );
    // Gaining file watches asks for one read.
    if handle.shared().projects[&repository].pending.is_some() {
        rescan(&mut scanner);
    }
    std::fs::write(linked[3].join("README"), "edited\n").unwrap();
    drained(&mut scanner, &|tracked| {
        tracked
            .pending
            .as_ref()
            .is_some_and(|change| change.worktrees.contains(&in_use))
    });
    rescan(&mut scanner);
    assert_eq!(
        worktree(&local(&handle), &linked[3])
            .dirty
            .as_ref()
            .unwrap()
            .paths,
        1
    );
}
