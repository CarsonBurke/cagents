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
        .args(arguments)
        .output()
        .unwrap();
    assert!(output.status.success(), "{arguments:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
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
    scanner.dispatch(handle);
    assert_eq!(handle.0.jobs.state.lock().unwrap().queue.len(), 1);
    let job = handle.0.jobs.next().unwrap();
    let result = scan::scan(scan::Input {
        id: job.id,
        repository: &job.repository,
        previous: job.previous.as_deref(),
        change: &job.change,
        hints: &job.hints,
        now_unix_ms: now_unix_ms(),
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
    let refs = root.join(".git/refs");
    let handle = Handle(Arc::new(Inner {
        host: Weak::new(),
        shared: Mutex::new(Shared {
            projects: BTreeMap::new(),
            pins: BTreeMap::new(),
            session_activity: HashMap::new(),
            tickets: BTreeMap::new(),
            linear_unavailable: None,
            stale_after_days: crate::work::DEFAULT_STALE_AFTER_DAYS,
            read_at: None,
            restored: BTreeMap::new(),
        }),
        updated: Condvar::new(),
        scanner: Signal::default(),
        forge: Signal::default(),
        publisher: Signal::default(),
        publication: Mutex::new(Publication::default()),
        jobs: Jobs::default(),
        store: Mutex::new(None),
        cancellation: CancellationToken::new(),
    }));
    let id = handle.shared().track(&root, "fixture").id;
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
                    native: notify::recommended_watcher(handler)?,
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
        sampled: None,
        refreshed: None,
    };
    complete_scan(&mut scanner, &handle);
    let original = handle.shared().projects[&root]
        .local
        .as_ref()
        .unwrap()
        .plan
        .clone()
        .unwrap();
    scanner.configure(&handle);
    coverage(&mut scanner, &handle, id, false);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    // Initial registration can request one reconciliation scan. Its unchanged
    // plan and all ordinary coordinator wakes stay inside the retry backoff.
    handle.shared().projects.get_mut(&root).unwrap().last_scan =
        Some(Instant::now() - SCAN_SPACING);
    complete_scan(&mut scanner, &handle);
    for _ in 0..100 {
        scanner.configure(&handle);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    // Advance only the existing scheduler's timestamps, avoiding a real
    // two-minute sleep. dispatch generates the real fallback poll, the worker
    // runs Git, and publish retains the exact unchanged watch-plan Arc.
    let poll = |scanner: &mut Scanner| {
        handle.shared().projects.get_mut(&root).unwrap().last_scan =
            Some(Instant::now() - IDLE_POLL);
        scanner.configured.get_mut(&id).unwrap().attempted = Instant::now() - ACTIVE_POLL;
        complete_scan(scanner, &handle);
        assert!(Arc::ptr_eq(
            &original,
            handle.shared().projects[&root]
                .local
                .as_ref()
                .unwrap()
                .plan
                .as_ref()
                .unwrap()
        ));
        scanner.configure(&handle);
    };
    poll(&mut scanner);
    coverage(&mut scanner, &handle, id, false);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    // Capacity can return in an unrelated process with no Git event here.
    // A completed fallback poll alone must retry and acknowledge real coverage.
    refuse.store(false, Ordering::SeqCst);
    for _ in 0..100 {
        scanner.configure(&handle);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    poll(&mut scanner);
    coverage(&mut scanner, &handle, id, true);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    // Native metadata events now reach Work, and the next real scan makes the
    // new branch current. The fixture never manually reconfigures the monitor.
    std::fs::write(root.join(".git/refs/heads/recovered"), format!("{oid}\n")).unwrap();
    wait_until(|| {
        scanner.drain(&handle);
        handle.shared().projects[&root]
            .pending
            .as_ref()
            .is_some_and(|change| change.references)
    });
    handle.shared().projects.get_mut(&root).unwrap().last_scan =
        Some(Instant::now() - SCAN_SPACING);
    complete_scan(&mut scanner, &handle);
    assert!(
        handle.shared().projects[&root]
            .local
            .as_ref()
            .unwrap()
            .branch_names
            .contains("recovered")
    );
    scanner.configure(&handle);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "healthy plans retain their registrations"
    );
}
