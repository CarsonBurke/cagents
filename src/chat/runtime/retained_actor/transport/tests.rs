use super::*;

#[test]
fn rejected_scope_request_still_starts_and_retires_a_real_retained_server() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const CHILD: &str = "CAGENTS_RETAINED_SCOPE_FALLBACK_TEST";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let test = thread::current().name().unwrap().to_owned();
        let output = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("HOME", root.path().join("home"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let cwd = root.path().join("cwd");
    let home = root.path().join("claude");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&home).unwrap();
    let marker = root.path().join("initialized");
    let mut source = fixture_source();
    source.launch.recipe.working_directory = cwd.clone();
    source.launch.recipe.environment = BTreeMap::from([
        ("HOME".into(), root.path().to_string_lossy().into_owned()),
        (
            "CLAUDE_CONFIG_DIR".into(),
            home.to_string_lossy().into_owned(),
        ),
    ]);
    source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
    let proof = source.proof.clone().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let (control, parent) = UnixStream::pair().unwrap();
    // The real caller/lease gate is independent of optional cgroup setup.
    authorize(&control, &proof).unwrap();
    let mut wrong = proof.clone();
    wrong.host_incarnation = crate::model::HostIncarnation::new("another-owner");
    assert!(authorize(&control, &wrong).is_err());
    let startup = |protocol| {
        let (spec, adapter) = chat::protocol::create(&source.launch).unwrap();
        let session_id = spec
            .arguments
            .iter()
            .find_map(|argument| argument.strip_prefix("--session-id="))
            .expect("fresh Claude launch must issue a session ID")
            .to_owned();
        let prepared = adapter.prepared_launch_identity().unwrap();
        Startup {
            protocol,
            source: source.clone(),
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                r#"IFS= read -r frame || exit 2
id=${frame#*\"request_id\":\"}
id=${id%%\"*}
printf initialized > "$2"
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"current_permission_mode":"default"}}}\n' "$id"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$1"
printf '{"type":"assistant","uuid":"scope-output","message":{"id":"scope-output","role":"assistant","content":[{"type":"text","text":"scope fallback initialized"}]}}\n'
printf '{"type":"result","subtype":"success","is_error":false,"session_id":"%s"}\n' "$1"
# Connected, the adapter asks where Claude Code compacts; take the request
# rather than leave it a closed pipe.
IFS= read -r frame || :
"#
                .into(),
                "scope-fixture".into(),
                session_id,
                marker.to_string_lossy().into_owned(),
            ],
            prepared_identity: prepared,
            environment: source.launch.recipe.environment.clone(),
            working_directory: cwd.clone(),
            directory_fd: None,
            directory_identity: None,
            mailbox_recognition: None,
            provider_directory_observed: false,
            recovery_environment: BTreeMap::new(),
        }
    };
    let reject = || {
        assert!(
            !marker.exists(),
            "enrollment must precede provider creation"
        );
        let error = crate::daemon::process_scope::tests::rejected_scope_request().unwrap_err();
        assert_eq!(
            error.to_string(),
            "user scope request failed: exit status: 1"
        );
        Err(error)
    };
    let error = Server::start_after_scope_enrollment(startup(0), &control, reject)
        .err()
        .unwrap();
    assert!(error.contains("Incompatible retained Claude actor launch"));
    assert!(!marker.exists(), "required startup failures remain fatal");
    let server = Server::start_after_scope_enrollment(startup(PROTOCOL), &control, reject).unwrap();
    let socket = server.socket.clone();
    let record = server.identity.record.clone();
    let record_path = registry::directory(Kind::Native)
        .unwrap()
        .join(format!("{}.json", record.terminal));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Completion is published after the exit is applied, so a snapshot
        // taken once completion is seen includes it; one taken before may not.
        if server.actor.is_completed() {
            let snapshot = server.actor.snapshot(None).unwrap();
            assert!(snapshot.exited);
            assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
            assert!(snapshot.items.iter().any(|item| matches!(
                &item.content,
                chat::ItemContent::Text { text, .. } if text == "scope fallback initialized"
            )));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "finite native fixture did not exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read(&marker).unwrap(), b"initialized");
    assert!(
        !record.root.is_current(),
        "original provider must be reaped"
    );
    assert!(socket.exists() && record_path.exists());
    struct Serving {
        parent: Option<UnixStream>,
        worker: Option<thread::JoinHandle<Result<(), String>>>,
    }
    impl Drop for Serving {
        fn drop(&mut self) {
            self.parent.take();
            if let Some(worker) = self.worker.take() {
                // The explicit success path checks the result below.
                // Still join on unwind without causing a double panic.
                let _ = worker.join();
            }
        }
    }
    let mut serving = Serving {
        parent: Some(parent),
        worker: Some(thread::spawn(move || {
            let mut server = server;
            server.serve(control, None)
        })),
    };
    let attached: Result<Attached, String> =
        receive(serving.parent.as_ref().unwrap(), TIMEOUT).unwrap();
    assert_eq!(attached.unwrap().identity.record, record);
    // No installation was committed: startup EOF performs the existing
    // normal actor cleanup and removes only its exact socket/record.
    serving.parent.take();
    serving.worker.take().unwrap().join().unwrap().unwrap();
    assert!(!socket.exists() && !record_path.exists());
}

#[test]
fn serving_actor_reaps_adopted_orphans_as_they_exit_and_sleeps_while_idle() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const CHILD: &str = "CAGENTS_RETAINED_ORPHAN_SWEEP_TEST";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let test = thread::current().name().unwrap().to_owned();
        let mut command = ChildCommand::new(std::env::current_exe().unwrap());
        // As in the helper, every thread blocks SIGCHLD before the sweep
        // reads it, including libtest's own.
        // SAFETY: the hook runs only async-signal-safe sigprocmask.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(
                &mut command,
                crate::daemon::child_events::block_sigchld_for_exec,
            );
        }
        let output = command
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("HOME", root.path().join("home"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // This isolated process stands in for the helper: a subreaper that
    // adopts every orphan its provider's tools leave behind.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let state = |pid: i32| {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 2..].chars().next()
    };
    let zombies = || {
        crate::daemon::owned_children::direct_children()
            .unwrap()
            .into_iter()
            .filter(|pid| state(*pid) == Some('Z'))
            .count()
    };
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let cwd = root.path().join("cwd");
    let home = root.path().join("claude");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&home).unwrap();
    let orphaned = root.path().join("orphaned");
    let mut source = fixture_source();
    source.launch.recipe.working_directory = cwd.clone();
    source.launch.recipe.environment = BTreeMap::from([
        ("HOME".into(), root.path().to_string_lossy().into_owned()),
        (
            "CLAUDE_CONFIG_DIR".into(),
            home.to_string_lossy().into_owned(),
        ),
    ]);
    source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
    let proof = source.proof.clone().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let (control, parent) = UnixStream::pair().unwrap();
    let (spec, adapter) = chat::protocol::create(&source.launch).unwrap();
    let session_id = spec
        .arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--session-id="))
        .unwrap()
        .to_owned();
    // After initializing, the provider's tools orphan 600 grandchildren in
    // bursts that exit together after their parents, so their SIGCHLDs
    // coalesce. One more orphaned daemon shares the provider's stdin, so
    // both stay alive until the actor, or this process, closes it.
    let startup = Startup {
        protocol: PROTOCOL,
        source: source.clone(),
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            r#"IFS= read -r frame || exit 2
id=${frame#*\"request_id\":\"}
id=${id%%\"*}
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"current_permission_mode":"default"}}}\n' "$id"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$1"
exec 3<&0
for burst in 1 2 3 4 5 6; do
  i=0
  while [ $i -lt 100 ]; do (/bin/sleep 0.2 &); i=$((i + 1)); done
done
(/bin/cat <&3 >/dev/null & echo $! > "$2")
while IFS= read -r frame; do :; done
"#
            .into(),
            "orphan-fixture".into(),
            session_id,
            orphaned.to_string_lossy().into_owned(),
        ],
        prepared_identity: adapter.prepared_launch_identity().unwrap(),
        environment: source.launch.recipe.environment.clone(),
        working_directory: cwd.clone(),
        directory_fd: None,
        directory_identity: None,
        mailbox_recognition: None,
        provider_directory_observed: false,
        recovery_environment: BTreeMap::new(),
    };
    let server = Server::start_after_scope_enrollment(startup, &control, || Ok(())).unwrap();
    let provider = server.identity.record.root.clone();
    let mut parent = Some(parent);
    let (serving, serving_thread) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        // SAFETY: gettid has no preconditions.
        serving.send(unsafe { libc::gettid() }).unwrap();
        // Its owner's thread creates and drops it, as the helper's does.
        let children = ChildEvents::new().expect("every fixture thread blocks SIGCHLD");
        let mut server = server;
        server.serve(control, Some(children))
    });
    let serving_thread = serving_thread.recv().unwrap();
    let attached: Result<Attached, String> = receive(parent.as_ref().unwrap(), TIMEOUT).unwrap();
    attached.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let live: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&orphaned)
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "provider tools did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    // Every exited orphan is reaped within a bound while both the actor
    // and its provider keep running.
    let bound = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = zombies();
        if remaining == 0 {
            break;
        }
        assert!(
            Instant::now() < bound,
            "{remaining} exited adopted orphans were not reaped while serving"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(provider.is_current(), "the provider must keep running");
    assert!(!worker.is_finished(), "the actor must keep serving");
    // Idle, the actor sleeps until a child exits or a client calls: its
    // serve loop and chat worker arm no timer.
    let switches = || {
        let mut total = 0;
        for task in std::fs::read_dir("/proc/self/task").unwrap() {
            let task = task.unwrap().path();
            let name = std::fs::read_to_string(task.join("comm")).unwrap_or_default();
            let tid: i32 = task.file_name().unwrap().to_str().unwrap().parse().unwrap();
            if tid != serving_thread && name.trim() != "cagents-chat" {
                continue;
            }
            let status = std::fs::read_to_string(task.join("status")).unwrap();
            total += status
                .lines()
                .filter_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
                .map(|count| count.trim().parse::<u64>().unwrap())
                .sum::<u64>();
        }
        total
    };
    // Only the provider and the live daemon remain to exit.
    let settle = Instant::now() + Duration::from_secs(5);
    while crate::daemon::owned_children::direct_children()
        .unwrap()
        .len()
        > 2
    {
        assert!(Instant::now() < settle, "short-lived tools did not finish");
        thread::sleep(Duration::from_millis(20));
    }
    let before = switches();
    thread::sleep(Duration::from_secs(2));
    let woken = switches() - before;
    // A 250 ms sweep timer alone woke the serve loop eight times here.
    assert!(woken <= 1, "an idle actor woke {woken} times in 2 s");
    // The live adopted daemon is still this actor's unsignalled child.
    assert_eq!(state(live), Some('S'));
    assert!(
        crate::daemon::owned_children::direct_children()
            .unwrap()
            .contains(&live)
    );
    // Startup EOF retires the actor through its existing cleanup.
    parent.take();
    worker.join().unwrap().unwrap();
    assert!(!provider.is_current(), "original provider must be reaped");
}

#[test]
fn an_ordinary_close_leaves_what_the_provider_leaves() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    use crate::daemon::owned_children::tests as children;
    // This isolated process stands in for the helper, which owns its
    // provider through a registration and adopts what the provider's
    // commands detach.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let (mut provider, registration) = crate::daemon::adopted_orphans::spawn_owned(
        &mut children::provider_with_detached_work(root.path()),
    )
    .unwrap();
    let work = children::detached_work(root.path());
    // As the actor's ChildGuard ends the provider, then the close's reap.
    unsafe { libc::kill(-(provider.id() as i32), libc::SIGTERM) };
    provider.wait().unwrap();
    drop(registration);
    reap_orphans(DetachedWork::Spare).unwrap();
    assert!(
        children::ends(work.running, || {
            reap_orphans(DetachedWork::Spare).unwrap();
        }),
        "the provider stopped its own command as it quit, and it is reaped"
    );
    for pid in [work.detached, work.left] {
        assert!(
            children::alive(pid) && children::parent(pid) == Some(std::process::id() as i32),
            "what the provider leaves running keeps running"
        );
    }
    // A close that stops everything, as for an unattended agent, reaps
    // them too.
    reap_orphans(DetachedWork::Stop).unwrap();
    assert!(!children::exists(work.detached) && !children::exists(work.left));
}

thread_local! {
    static INSPECT_SOURCES_CAPTURED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn after_inspection_sources() {
    INSPECT_SOURCES_CAPTURED.with(|slot| {
        if let Some(cutpoint) = slot.borrow_mut().take() {
            cutpoint();
        }
    });
}
struct Fixture {
    server: Server,
    _root: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.actor.close();
    }
}
fn resident_fixture() -> Fixture {
    resident_fixture_with_body(None)
}
fn resident_fixture_with_body(body: Option<String>) -> Fixture {
    let (server, root) = resident_server(body);
    Fixture {
        server,
        _root: root,
    }
}
/// A resident fixture's parts, for a test that moves the server into
/// [`Server::serve`], which closes its actor on the way out.
pub(super) fn resident_server(body: Option<String>) -> (Server, tempfile::TempDir) {
    use crate::chat::protocol::{Adapter, Step};
    struct Idle;
    impl Adapter for Idle {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![chat::Event::Ready(true)],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let remote = RemoteActor::fixture();
    let mut identity = remote.identity();
    let proof = remote.0.proof.clone();
    let recipe = crate::model::LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.path().into(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    let source = chat::Session {
        id: identity.record.terminal,
        launch: chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: recipe.clone(),
            session_id: None,
            draft_model: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: chat::NativeContinuity::ReconnectOnly,
    };
    let spec = ProcessSpec::from(&recipe);
    let mut adapter: Box<dyn Adapter> = Box::new(Idle);
    let mut initial = adapter.initialize().unwrap();
    if let Some(text) = body {
        // Readiness is the fixture's startup barrier. Publish its body
        // first so a ready reader cannot race an empty Copy source list.
        initial.events.insert(
            0,
            chat::Event::Item(chat::Item {
                id: "retained-body".into(),
                role: chat::Role::Assistant,
                content: chat::ItemContent::Text {
                    text,
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: None,
                at: None,
            }),
        );
    }
    let store = super::super::super::preflight(&spec, &mut initial).unwrap();
    let actor = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !actor.0.state.lock().unwrap().ready {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    identity.instance = uuid::Uuid::new_v4();
    let journal_directory = root.path().join("journal");
    std::fs::create_dir(&journal_directory).unwrap();
    let socket = root.path().join("fixture.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let metadata = std::fs::symlink_metadata(&socket).unwrap();
    (
        Server {
            actor,
            identity,
            source,
            lineage: Vec::new(),
            execution_console: None,
            paired_holder: None,
            proof,
            epoch: uuid::Uuid::new_v4(),
            installed: true,
            journal: Arc::new(Mutex::new(Journal::open(&journal_directory).unwrap())),
            changed: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            watcher: Arc::new(push::Watcher::default()),
            state_reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            fail_state_reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            prompt_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending: Vec::new(),
            account_reports: Arc::new(Mutex::new(Vec::new())),
            environment: crate::terminal_ipc::ChatExecutionEnvironment(Default::default()),
            recovery_environment: BTreeMap::new(),
            listener,
            socket,
            socket_identity: (metadata.dev(), metadata.ino()),
            _directory: None,
            closed_snapshot: None,
            session_stopping: false,
            attachment_preserved: false,
            close_only: false,
            detached: DetachedWork::Stop,
        },
        root,
    )
}
#[test]
fn continuity_loss_refuses_unchanged_presentation_without_mutating_source_or_copy() {
    let body = "Retained answer λ\n  preserve source and copy";
    let mut fixture = resident_fixture_with_body(Some(body.into()));
    let server = &mut fixture.server;
    let source = server.source.clone();
    let proof = server.proof.clone();
    let epoch = server.epoch;
    let id = source.id;
    let request = |method: &str, args| Invocation {
        proof: proof.clone(),
        epoch: Some(epoch),
        method: method.into(),
        args,
    };
    let before: Snapshot =
        serde_json::from_value(server.invoke(request("snapshot", json!([null]))).unwrap()).unwrap();
    assert!(before.ready);
    let copy = before.text_copy_sources.conversation[0].clone().unwrap();
    let before_page: chat::TextCopyPage = serde_json::from_value(
        server
            .invoke(request("text_copy", json!([copy, 0])))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(before_page.text, body);
    let generation = before.generation;
    assert_eq!(
        server
            .invoke(request("presentation", json!([id, generation])))
            .unwrap(),
        Value::Null,
    );
    let loss = "Retained outcome journal commit failed; run outcome is unconfirmed";
    server.journal.lock().unwrap().record_loss(loss).unwrap();
    // FULL, matching, and lagged cursors all report the same loss before
    // the reducer can suppress reads or hydrate a transcript window.
    for after in [None, Some(generation), Some(generation.saturating_sub(1))] {
        assert_eq!(
            server
                .invoke(request("presentation", json!([id, after])))
                .unwrap_err(),
            loss,
        );
    }
    let after: Snapshot =
        serde_json::from_value(server.invoke(request("snapshot", json!([null]))).unwrap()).unwrap();
    assert_eq!(
        after.generation, generation,
        "loss invents no transcript revision"
    );
    assert_eq!(after.error.as_deref(), Some(loss));
    assert!(!after.ready);
    assert_eq!(after.items, before.items);
    assert_eq!(after.text_copy_sources, before.text_copy_sources);
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.exited, before.exited);
    assert_eq!(server.source, source);
    assert!(server.actor.owns_work());
    assert!(!server.actor.is_completed());
    assert!(!server.session_stopping);
    assert!(server.closed_snapshot.is_none());
    let copied: chat::TextCopyPage = serde_json::from_value(
        server
            .invoke(request("text_copy", json!([copy, 0])))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(copied, before_page);
}
#[test]
fn removed_or_replaced_home_can_only_adopt_the_exact_helper_for_close() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const CHILD: &str = "CAGENTS_RETAINED_CLOSE_ONLY_TEST";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let test = thread::current().name().unwrap().to_owned();
        let output = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("HOME", root.path().join("home"))
            .env("CAGENTS_LOCAL_DEBUG", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    struct Owner(RemoteActor);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.close();
            let _ = self.0.finish_close();
        }
    }
    fn bypass_proxy(actor: &RemoteActor, method: &str, args: Value) -> Result<Value, String> {
        let control = registry::connect(&actor.0.socket).map_err(|error| error.to_string())?;
        verify_peer(&control, &actor.identity().record)?;
        send(
            &control,
            &Invocation {
                proof: actor.0.proof.clone(),
                epoch: Some(actor.0.epoch),
                method: method.into(),
                args,
            },
            TIMEOUT,
        )?;
        receive::<Result<Value, String>>(&control, TIMEOUT)?
    }
    for replaced in [false, true] {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let home = root.path().join("claude");
        let previous_home = root.path().join("previous-claude");
        let cwd = root.path().join("cwd");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        let mut source = fixture_source();
        source.launch.recipe = crate::model::LaunchRecipe {
            program: "/bin/sleep".into(),
            arguments: vec!["600".into()],
            working_directory: cwd.clone(),
            environment: BTreeMap::from([
                ("HOME".into(), root.path().to_string_lossy().into_owned()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    home.to_string_lossy().into_owned(),
                ),
            ]),
            unset_environment: Default::default(),
        };
        source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
        let proof = source.proof.clone().unwrap();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        // This idle fake provider deliberately substitutes argv, while
        // obtaining its typed adapter identity from real preparation.
        // Fake provider argv is intentionally independent of Claude flags;
        // the adapter selection still comes from the real constructor.
        let prepared = chat::protocol::create(&source.launch)
            .unwrap()
            .1
            .prepared_launch_identity()
            .unwrap();
        let mut spec = ProcessSpec::from(&source.launch.recipe);
        spec.execution = Some(Arc::new(ExecutionContext::capture(&cwd).unwrap()));
        let original = Owner(
            RemoteActor::spawn(spec, source.clone(), prepared, BTreeMap::new(), |_| {}).unwrap(),
        );
        original.0.commit_installation().unwrap();
        let identity = original.0.identity();
        let captured_scope = original.0.launch_account_scope().unwrap().clone();
        if replaced {
            std::fs::rename(&home, &previous_home).unwrap();
            std::fs::create_dir(&home).unwrap();
        } else {
            std::fs::remove_dir(&home).unwrap();
        }
        std::fs::rename(&cwd, root.path().join("original-cwd")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        assert!(
            RemoteActor::adopt(&identity.record, source.clone(), proof.clone(), |_| {}).is_err()
        );
        let mut wrong_source = source.clone();
        wrong_source.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            root.path().join("foreign").to_string_lossy().into_owned(),
        );
        assert!(
            RemoteActor::adopt_for_close(&identity.record, wrong_source, proof.clone(), |_| {})
                .is_err()
        );
        let mut wrong_record = identity.record.clone();
        wrong_record.root.start = wrong_record.root.start.wrapping_add(1);
        assert!(
            RemoteActor::adopt_for_close(&wrong_record, source.clone(), proof.clone(), |_| {})
                .is_err()
        );
        let mut wrong_proof = proof.clone();
        wrong_proof.host_incarnation = crate::model::HostIncarnation::new("not-the-current-lease");
        assert!(
            RemoteActor::adopt_for_close(&identity.record, source.clone(), wrong_proof, |_| {})
                .is_err()
        );
        assert!(identity.record.root.is_current());
        let close = Owner(
            RemoteActor::adopt_for_close(&identity.record, source.clone(), proof.clone(), |_| {})
                .unwrap(),
        );
        assert!(close.0.is_close_only());
        assert_eq!(close.0.identity(), identity);
        assert_eq!(close.0.launch_account_scope(), Ok(&captured_scope));
        assert!(
            original.0.call::<()>("preserve", Value::Null).is_err(),
            "close attachment supersedes the old normal epoch"
        );
        assert!(matches!(
            close.0.submit(
                None,
                Command::Prompt {
                    text: "must never reach provider".into(),
                    attachments: Vec::new(),
                    mode: chat::PromptMode::Send
                }
            ),
            Err(Rejection::Refused(_))
        ));
        assert!(close.0.interrupt(None, true).is_err());
        assert!(close.0.preserve().is_err());
        assert!(close.0.commit_installation().is_err());
        assert!(
            close
                .0
                .call::<()>("file_query", json!([Command::Refresh]))
                .is_err()
        );
        for method in [
            "input",
            "file_query",
            "text_copy",
            "preserve",
            "commit",
            "restart",
        ] {
            assert!(
                bypass_proxy(&close.0, method, Value::Null)
                    .unwrap_err()
                    .contains("close-only"),
                "server must fence {method} independently of proxy"
            );
        }
        assert!(bypass_proxy(&close.0, "attach", json!([source, identity.record])).is_err());
        assert!(close.0.pending_inputs().is_empty());
        assert_eq!(close.0.state().next_input, 1);
        assert!(close.0.snapshot(None).is_some());
        close.0.refresh_state().unwrap();
        if replaced {
            std::fs::remove_dir(&home).unwrap();
            std::fs::rename(&previous_home, &home).unwrap();
            let normal = Owner(
                RemoteActor::adopt(&identity.record, source.clone(), proof.clone(), |_| {})
                    .unwrap(),
            );
            assert!(!normal.0.is_close_only());
            assert!(
                close.0.call::<()>("observe", Value::Null).is_err(),
                "normal adoption invalidates old close-only epoch"
            );
            assert_eq!(normal.0.identity(), identity);
            std::fs::rename(&home, &previous_home).unwrap();
            std::fs::create_dir(&home).unwrap();
            let final_close = Owner(
                RemoteActor::adopt_for_close(
                    &identity.record,
                    source.clone(),
                    proof.clone(),
                    |_| {},
                )
                .unwrap(),
            );
            assert!(normal.0.call::<()>("preserve", Value::Null).is_err());
            final_close.0.close().unwrap();
            final_close.0.finish_close().unwrap();
        } else {
            close.0.close().unwrap();
            close.0.finish_close().unwrap();
        }
        assert!(!identity.record.root.is_current());
        // The retirement proof sees the helper exit before its reaper
        // collects it.
        assert!(identity.record.holder.reaped());
        let retired = RemoteActor::retired_actor_receipt(&identity.record, &proof)
            .unwrap()
            .unwrap();
        assert_eq!(retired.identity, identity);
        assert_eq!(retired.launch_account_scope, Ok(captured_scope));
        assert!(retired.paired_holder.is_none());
    }
}
#[test]
fn authenticated_directory_pin_keeps_original_after_replacement_and_epoch_change() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const CHILD: &str = "CAGENTS_RETAINED_DIRECTORY_PIN_TEST";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let name = thread::current().name().unwrap().to_owned();
        let output = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("HOME", root.path().join("home"))
            .env("CAGENTS_LOCAL_DEBUG", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(root.path().join("claude")).unwrap();
    std::fs::write(cwd.join("marker"), "original").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let provider = cwd.join("fixture-claude");
    std::fs::write(
        &provider,
        r#"#!/bin/sh
printf '%s\n' "$@" > arguments
session=
for argument do
    case "$argument" in --session-id=*) session=${argument#--session-id=} ;; esac
done
[ -n "$session" ] || exit 2
printf '%s' "$session" > session-id
/bin/cat marker > observed
IFS= read -r frame || exit 3
id=${frame#*\"request_id\":\"}
id=${id%%\"*}
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"current_permission_mode":"default"}}}\n' "$id"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$session"
exec /bin/sleep 60
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let expected =
        crate::agent_execution::DirectoryIdentity::from_metadata(&std::fs::metadata(&cwd).unwrap());
    let mut source = fixture_source();
    source.launch.recipe = crate::model::LaunchRecipe {
        program: provider.to_str().unwrap().to_owned(),
        arguments: Vec::new(),
        working_directory: cwd.clone(),
        environment: BTreeMap::from([
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                root.path().join("claude").to_string_lossy().into_owned(),
            ),
        ]),
        unset_environment: Default::default(),
    };
    source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
    let proof = source.proof.clone().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let (spec, adapter) = chat::protocol::create(&source.launch).unwrap();
    let issued_session = spec
        .arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--session-id="))
        .expect("real fresh Claude preparation must issue its exact session ID")
        .to_owned();
    uuid::Uuid::parse_str(&issued_session).unwrap();
    let public_arguments = spec.arguments.clone();
    assert!(spec.execution.is_none()); // ordinary, no inherited startup directory FD
    let prepared = adapter.prepared_launch_identity().unwrap();
    struct Owner(RemoteActor);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.close();
            let _ = self.0.finish_close();
        }
    }
    let original =
        Owner(RemoteActor::spawn(spec, source.clone(), prepared, BTreeMap::new(), |_| {}).unwrap());
    original.0.commit_installation().unwrap();
    let record = original.0.identity().record;
    let deadline = Instant::now() + TIMEOUT;
    while std::fs::read_to_string(cwd.join("observed"))
        .ok()
        .as_deref()
        != Some("original")
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        std::fs::read_to_string(cwd.join("arguments")).unwrap(),
        public_arguments.join("\n") + "\n",
        "the private executable receives the real adapter's complete public argv"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("session-id")).unwrap(),
        issued_session
    );
    while original.0.state().session_id.as_deref() != Some(issued_session.as_str()) {
        assert!(
            Instant::now() < deadline,
            "prepared session identity was not observed"
        );
        original.0.refresh_state().unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    std::fs::rename(&cwd, root.path().join("moved")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("marker"), "replacement").unwrap();
    assert_eq!(original.0.pinned_launch_directory().unwrap().0, expected);
    let control = registry::connect(&original.0.0.socket).unwrap();
    let mut wrong = proof.clone();
    wrong.host_incarnation = crate::model::HostIncarnation::new("wrong-directory-query");
    send(
        &control,
        &Invocation {
            proof: wrong,
            epoch: Some(original.0.0.epoch),
            method: "pinned_directory".into(),
            args: Value::Null,
        },
        TIMEOUT,
    )
    .unwrap();
    let (refused, rights) = crate::descriptor_frame::receive_json_with_fds_until::<
        Result<crate::agent_execution::DirectoryIdentity, String>,
    >(
        &control,
        Instant::now() + TIMEOUT,
        MAX_FRAME,
        1,
        crate::descriptor_frame::FrameByteOrder::LittleEndian,
    )
    .unwrap();
    assert!(refused.is_err());
    assert!(rights.is_empty());
    let adopted =
        Owner(RemoteActor::adopt(&record, source.clone(), proof.clone(), |_| {}).unwrap());
    assert!(
        original.0.pinned_launch_directory().is_err(),
        "stale epoch cannot receive directory authority"
    );
    let (identity, pin) = adopted.0.pinned_launch_directory().unwrap();
    assert_eq!(identity, expected);
    let fd = unsafe {
        libc::openat(
            pin.as_raw_fd(),
            c"marker".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    assert!(fd >= 0);
    let mut text = String::new();
    use std::io::Read;
    unsafe { File::from_raw_fd(fd) }
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "original");
    assert_eq!(
        std::fs::read_to_string(cwd.join("marker")).unwrap(),
        "replacement"
    );
    let close = Owner(RemoteActor::adopt_for_close(&record, source, proof, |_| {}).unwrap());
    assert!(close.0.pinned_launch_directory().is_err());
    assert!(adopted.0.pinned_launch_directory().is_err());
    close.0.close().unwrap();
    close.0.finish_close().unwrap();
    assert!(!record.root.is_current());
}

#[test]
fn helper_control_rejects_regular_files_and_unconnected_listeners() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let file = File::create(root.path().join("ordinary-file")).unwrap();
    assert!(validate_control_descriptor(file.as_raw_fd()).is_err());
    let listener = UnixListener::bind(root.path().join("listener.sock")).unwrap();
    assert!(validate_control_descriptor(listener.as_raw_fd()).is_err());
    let (client, server) = UnixStream::pair().unwrap();
    assert!(validate_control_descriptor(client.as_raw_fd()).is_ok());
    assert!(validate_control_descriptor(server.as_raw_fd()).is_ok());
}
#[test]
fn helper_restores_descriptor_seal_before_any_provider_exec() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const CHILD: &str = "CAGENTS_ACTOR_DESCRIPTOR_SEAL_TEST";
    if std::env::var_os(CHILD).is_none() {
        let isolated = tempfile::tempdir_in("/var/tmp").unwrap();
        let test = thread::current().name().unwrap().to_owned();
        let output = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", isolated.path())
            .env("XDG_DATA_HOME", isolated.path().join("data"))
            .env("XDG_CONFIG_HOME", isolated.path().join("config"))
            .env("XDG_CACHE_HOME", isolated.path().join("cache"))
            .env("HOME", isolated.path().join("home"))
            .env("CAGENTS_LOCAL_DEBUG", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    std::fs::create_dir(root.path().join("claude")).unwrap();
    let mut source = fixture_source();
    source.launch.provider = crate::agent::Provider::Claude;
    source.launch.session_id = None;
    source.launch.recipe = crate::model::LaunchRecipe {
        program: "/bin/sleep".into(),
        arguments: vec!["600".into()],
        working_directory: root.path().into(),
        environment: BTreeMap::from([
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                root.path().join("claude").to_string_lossy().into_owned(),
            ),
        ]),
        unset_environment: Default::default(),
    };
    source.owned_codex_listener = None;
    source.terminal = None;
    source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
    let context = Arc::new(ExecutionContext::capture(root.path()).unwrap());
    let prepared = chat::protocol::create(&source.launch)
        .unwrap()
        .1
        .prepared_launch_identity()
        .unwrap();
    let mut spec = ProcessSpec::from(&source.launch.recipe);
    spec.execution = Some(context.clone());
    let limit = crate::host::descriptor_limit::tests::raise_with_fixture_limit();
    // This execs the real retained-helper entry and its real provider
    // spawning path. Omitting the entry seal leaks both inherited FDs.
    let actor = RemoteActor::spawn(spec, source, prepared, BTreeMap::new(), |_| {}).unwrap();
    struct Owner(RemoteActor);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.close();
            let _ = self.0.finish_close();
        }
    }
    let owner = Owner(actor);
    let actor = &owner.0;
    actor.commit_installation().unwrap();
    let provider = actor.state().provider_process.clone().unwrap();
    assert!(provider.is_current());
    crate::host::descriptor_limit::tests::assert_process_limit(provider.pid as u32, &limit);
    crate::host::descriptor_limit::tests::assert_process_limit(
        actor.native_holder_record().holder.pid as u32,
        &limit,
    );
    crate::host::descriptor_limit::tests::assert_daemon_limit_remains_raised(&limit);
    let inherited = std::fs::read_dir(format!("/proc/{}/fd", provider.pid))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            match std::fs::read_link(path) {
                Ok(target) => Some(target),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("Cannot inspect exact fixture provider FD: {error}"),
            }
        })
        .collect::<Vec<_>>();
    actor.close().unwrap();
    actor.finish_close().unwrap();
    assert!(!provider.is_current());
    assert!(
        !inherited
            .iter()
            .any(|target| target.to_string_lossy().starts_with("socket:[")),
        "provider exec must not inherit the private helper startup control socket"
    );
    assert!(
        !inherited.iter().any(|target| target == root.path()),
        "provider exec must not retain the helper's approved-directory FD"
    );
    assert!(
        context
            .directory_descriptor()
            .unwrap()
            .metadata()
            .unwrap()
            .is_dir(),
        "the caller's approved pin remains usable"
    );
}

#[test]
fn stopped_provider_keeps_its_semantic_journal_until_outcome_and_loss_ack() {
    for installed in [false, true] {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        server.installed = installed;
        server.actor.close().unwrap();
        server.closed_snapshot = Some(Arc::new(server.actor.snapshot(None).unwrap()));
        server
            .journal
            .lock()
            .unwrap()
            .append(
                &Update::Attention(crate::notifications::Evidence {
                    key: "final-turn".into(),
                    kind: crate::notifications::Kind::Succeeded,
                }),
                None,
            )
            .unwrap();
        assert!(server.check_retirement().is_err());
        assert_eq!(
            server.journal.lock().unwrap().page(0).unwrap().len(),
            1,
            "provider teardown must not discard the uncommitted final outcome"
        );
        server.journal.lock().unwrap().ack(1).unwrap();
        server
            .journal
            .lock()
            .unwrap()
            .record_loss("fixture loss")
            .unwrap();
        assert!(server.check_retirement().is_err());
        let id = server
            .journal
            .lock()
            .unwrap()
            .continuity_loss()
            .unwrap()
            .unwrap()
            .id;
        server.journal.lock().unwrap().ack_loss(id).unwrap();
        assert!(server.check_retirement().is_ok());
    }
}
#[test]
fn courier_input_completion_persists_uncertainty_and_retains_affected_runs() {
    for uncertain in [false, true] {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: [8; 32],
        };
        let run = crate::model::RunId::new();
        server
            .journal
            .lock()
            .unwrap()
            .admit_with_run(&identity, Some(run))
            .unwrap();
        let (reply, response) = crossbeam_channel::bounded(1);
        let error = "fixture input failure".to_owned();
        reply
            .send(Err(if uncertain {
                Rejection::Unacknowledged(error.clone())
            } else {
                Rejection::Refused(error.clone())
            }))
            .unwrap();
        server.pending.push((
            identity.clone(),
            response,
            Some((
                run,
                Arc::new(std::sync::atomic::AtomicU8::new(if uncertain {
                    2
                } else {
                    1
                })),
            )),
        ));
        server.finish_inputs();
        assert!(server.pending.is_empty());
        let journal = server.journal.lock().unwrap();
        let receipt = journal.input(&identity).unwrap().unwrap();
        assert_eq!(
            receipt.state,
            if uncertain {
                InputState::Unknown(error)
            } else {
                InputState::Acknowledged(Err(error))
            }
        );
        assert_eq!(
            journal.retained_runs().unwrap(),
            if uncertain { vec![run] } else { vec![] }
        );
        // Helpers keep their Journal for one lifetime. An independent
        // read-only database connection checks the committed receipt;
        // reopening the actor Journal itself is intentionally refused.
        let persisted = rusqlite::Connection::open_with_flags(
            fixture._root.path().join("journal/actor-journal.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let bytes: Vec<u8> = persisted
            .query_row(
                "SELECT state FROM inputs WHERE sequence=?1",
                [identity.sequence],
                |row| row.get(0),
            )
            .unwrap();
        let state: InputState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(state, receipt.state);
    }
}

#[test]
fn input_refusal_and_storage_failure_publish_new_resident_revisions() {
    for storage_failure in [false, true] {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        let run = crate::model::RunId::new();
        let identity = InputIdentity {
            sequence: 1,
            id: uuid::Uuid::new_v4(),
            hash: [9; 32],
        };
        {
            let mut journal = server.journal.lock().unwrap();
            journal.admit_with_run(&identity, Some(run)).unwrap();
            if storage_failure {
                journal.fail_input_storage();
            }
        }
        let before = server.resident().unwrap();
        assert_eq!(before.retained_runs, vec![run]);
        let (reply, response) = crossbeam_channel::bounded(1);
        reply
            .send(Err(Rejection::Refused("fixture refusal".to_owned())))
            .unwrap();
        server.pending.push((
            identity,
            response,
            Some((
                run,
                Arc::new(std::sync::atomic::AtomicU8::new(if storage_failure {
                    2
                } else {
                    1
                })),
            )),
        ));
        server.finish_inputs();
        let response = server
            .invoke(Invocation {
                proof: server.proof.clone(),
                epoch: Some(server.epoch),
                method: "state".into(),
                args: json!([before.changed]),
            })
            .unwrap();
        let next: Option<Resident> = serde_json::from_value(response).unwrap();
        let next = next.expect("consumed completions must invalidate an earlier resident token");
        if storage_failure {
            assert!(next.continuity_loss.is_some());
            assert_eq!(next.continuity_loss.unwrap().affected_runs, vec![run]);
        } else {
            assert!(next.retained_runs.is_empty());
            assert!(next.continuity_loss.is_none());
        }
    }
}
#[test]
fn retained_parent_accepts_prompts_while_background_children_keep_working() {
    let fixture = resident_fixture();
    {
        let mut state = fixture.server.actor.0.state.lock().unwrap();
        state.busy = false;
        state.subagents.push(chat::Subagent {
            id: "background-child".into(),
            kind: "fixture".into(),
            description: "Still running".into(),
            status: chat::ToolStatus::Running,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        });
    }
    let resident = fixture.server.resident().unwrap();
    assert!(resident.working);
    assert!(!resident.busy);
    assert!(
        resident.idle,
        "parent admission is independent of aggregate child activity"
    );
    {
        let mut state = fixture.server.actor.0.state.lock().unwrap();
        state.busy = true;
    }
    assert!(!fixture.server.resident().unwrap().idle);
    fixture.server.actor.0.state.lock().unwrap().busy = false;
    let notify = fixture.server.notifier();
    notify(Update::PromptAvailable);
    let next = fixture.server.resident().unwrap();
    assert!(next.idle);
    assert_ne!(
        resident.prompt_revision, next.prompt_revision,
        "a coalesced parent busy/ready edge must remain observable across proxy polls"
    );
    let before = next.prompt_revision;
    notify(Update::Presentation);
    assert_eq!(fixture.server.resident().unwrap().prompt_revision, before);
}
#[test]
fn a_helper_that_predates_named_holds_still_reports_its_background_work() {
    let mut resident = serde_json::to_value(RemoteActor::fixture().state().as_ref()).unwrap();
    let fields = resident.as_object_mut().unwrap();
    assert!(fields.remove("work_hold").is_some());
    fields.insert("background_subagents".into(), json!(["toolu_old"]));
    let older: Resident = serde_json::from_value(resident).unwrap();
    let hold = older.work_hold();
    assert!(!hold.turn);
    assert_eq!(
        hold.subagents,
        [chat::HeldSubagent {
            name: String::new(),
            waiting: false
        }]
    );
    assert_eq!(
        hold.subagents().detail(),
        "1 background subagent is still running."
    );
}
#[test]
fn protocol_one_attachment_is_refused_without_starting_or_retiring_its_owner() {
    let original = RemoteActor::fixture();
    let identity = original.identity();
    let proof = original.proof();
    let result = RemoteActor::from_attached(
        Attached {
            protocol: 1,
            close_only: false,
            identity: identity.clone(),
            epoch: uuid::Uuid::new_v4(),
            resident: original.state().as_ref().clone(),
            source: original.0.source.clone(),
            launch_account_scope: Err("legacy fixture".into()),
            execution_environment: Default::default(),
            recovery_environment: BTreeMap::new(),
        },
        proof,
        None,
        |_| panic!("an incompatible attachment cannot publish updates"),
    );
    assert!(result.err().unwrap().contains("incompatible"));
    assert_eq!(original.identity(), identity);
    assert!(original.state().owns_work);
    assert!(!original.0.stop.load(Ordering::Acquire));
    assert!(identity.record.root.is_current());
    assert!(original.pending_inputs().is_empty());
}
/// #2849 follow-up: each journaled outcome names the conversation the
/// provider had confirmed, and the daemon publishes that conversation
/// before committing it. After a reset replaces the conversation, the
/// helper must still recognize that projection while the outcome is
/// unacknowledged. A recognized projection stays a published alias, since the
/// census keeps naming it; an unrecognized one is not issued after the ack.
#[test]
fn an_unacknowledged_outcome_keeps_its_conversation_recognized_after_a_reset() {
    let mut fixture = resident_fixture();
    let followed = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let current = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let unrelated = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(followed.into());
    let notify = fixture.server.notifier();
    notify(Update::Execution {
        run: crate::model::RunId::new(),
        update: crate::agent_execution::Update::Finished {
            state: crate::agent_execution::State::Succeeded,
            result: Default::default(),
        },
    });
    *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(current.into());
    let page = fixture.server.journal.lock().unwrap().page(0).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].confirmed_session.as_deref(), Some(followed));
    let continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
    let call = |server: &mut Server, method: &str, args: Value| {
        let (proof, epoch) = (server.proof.clone(), server.epoch);
        server.invoke(Invocation {
            proof,
            epoch: Some(epoch),
            method: method.into(),
            args,
        })
    };
    let recognize = |server: &mut Server, session| {
        let projected = published_source(&server.source, Some(session), &continuity);
        call(server, "recognize_sources", json!([[projected]]))
    };
    recognize(&mut fixture.server, followed).unwrap();
    assert!(recognize(&mut fixture.server, unrelated).is_err());
    call(&mut fixture.server, "ack_outbox", json!([page[0].sequence])).unwrap();
    // The daemon's census now names the followed projection. Republishing
    // the current conversation authenticates that row first.
    recognize(&mut fixture.server, followed).unwrap();
    // An outcome acknowledged without its projection ever being recognized
    // leaves nothing issued.
    *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(unrelated.into());
    notify(Update::Execution {
        run: crate::model::RunId::new(),
        update: crate::agent_execution::Update::Finished {
            state: crate::agent_execution::State::Succeeded,
            result: Default::default(),
        },
    });
    *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(current.into());
    let sequence = fixture.server.journal.lock().unwrap().latest().unwrap();
    call(&mut fixture.server, "ack_outbox", json!([sequence])).unwrap();
    assert!(
        recognize(&mut fixture.server, unrelated).is_err(),
        "an acknowledged outcome issues no further conversation"
    );
}
#[test]
fn inspection_commits_the_exact_captured_alias_before_a_concurrent_reset() {
    let mut fixture = resident_fixture();
    let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    {
        let mut state = fixture.server.actor.0.state.lock().unwrap();
        state.session_id = Some(a.into());
        confirm_installed_snapshot(&mut state);
    }
    *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(a.into());
    let expected_a = published_source(
        &fixture.server.source,
        Some(a),
        &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
    );
    let actor = fixture.server.actor.clone();
    INSPECT_SOURCES_CAPTURED.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            actor.0.state.lock().unwrap().session_id = Some(b.into());
            *actor.0.confirmed_session.lock().unwrap() = Some(b.into());
        }))
    });
    let response = fixture
        .server
        .invoke(Invocation {
            proof: fixture.server.proof.clone(),
            epoch: None,
            method: "inspect".into(),
            args: json!([]),
        })
        .unwrap();
    let inspection: ActorInspection = serde_json::from_value(response).unwrap();
    assert!(inspection.previous_sources.contains(&expected_a));
    assert_eq!(inspection.state.session_id.as_deref(), Some(b));
    let db = rusqlite::Connection::open_with_flags(
        fixture._root.path().join("journal/actor-journal.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT value FROM metadata WHERE key='published_sources'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let durable: Vec<chat::Session> = serde_json::from_slice(&bytes).unwrap();
    assert!(
        durable.contains(&expected_a),
        "the exact outgoing alias A must commit before resident B is sampled"
    );
}

#[test]
fn public_identity_responses_commit_aliases_before_returning_and_skip_unchanged_writes() {
    for method in [
        "inspect",
        "state",
        "snapshot",
        "presentation",
        "resolve_source",
    ] {
        let mut fixture = resident_fixture();
        let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        {
            let mut state = fixture.server.actor.0.state.lock().unwrap();
            state.session_id = Some(a.into());
            confirm_installed_snapshot(&mut state);
        }
        *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(a.into());
        let source = fixture.server.source.clone();
        let expected_a = published_source(
            &source,
            Some(a),
            &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
        );
        let path = fixture._root.path().join("journal/actor-journal.sqlite");
        let fault = rusqlite::Connection::open(&path).unwrap();
        fault.execute_batch("CREATE TRIGGER refuse_publication BEFORE INSERT ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'fixture publication refusal'); END;").unwrap();
        let invoke = |server: &mut Server| {
            let args = match method {
                "inspect" => json!([]),
                "state" => json!([0]),
                "snapshot" => json!([null]),
                "presentation" => json!([source.id, null]),
                "resolve_source" => json!([source]),
                _ => unreachable!(),
            };
            server.invoke(Invocation {
                proof: server.proof.clone(),
                epoch: Some(server.epoch),
                method: method.into(),
                args,
            })
        };
        assert!(
            invoke(&mut fixture.server)
                .unwrap_err()
                .contains("fixture publication refusal"),
            "no identity-bearing {method} response may precede the journal commit"
        );
        assert!(
            fixture
                .server
                .journal
                .lock()
                .unwrap()
                .published_sources()
                .is_empty()
        );
        fault
            .execute_batch("DROP TRIGGER refuse_publication;")
            .unwrap();
        invoke(&mut fixture.server).unwrap();
        let bytes: Vec<u8> = fault
            .query_row(
                "SELECT value FROM metadata WHERE key='published_sources'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let durable: Vec<chat::Session> = serde_json::from_slice(&bytes).unwrap();
        assert!(
            durable.contains(&expected_a),
            "the returned {method} identity must already be durable"
        );
        fault.execute_batch("CREATE TRIGGER refuse_publication BEFORE INSERT ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'unchanged publication wrote'); END; CREATE TRIGGER refuse_publication_update BEFORE UPDATE ON metadata WHEN NEW.key='published_sources' BEGIN SELECT RAISE(ABORT,'unchanged publication updated'); END;").unwrap();
        invoke(&mut fixture.server).expect("unchanged public identities must not write again");
        fault
            .execute_batch(
                "DROP TRIGGER refuse_publication; DROP TRIGGER refuse_publication_update;",
            )
            .unwrap();
        {
            let mut state = fixture.server.actor.0.state.lock().unwrap();
            state.session_id = Some(b.into());
            state.generation += 1;
        }
        *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(b.into());
        let resolved = fixture
            .server
            .invoke(Invocation {
                proof: fixture.server.proof.clone(),
                epoch: Some(fixture.server.epoch),
                method: "resolve_source".into(),
                args: json!([expected_a]),
            })
            .unwrap();
        let resolved: chat::Session = serde_json::from_value(resolved).unwrap();
        assert_eq!(resolved.launch.session_id.as_deref(), Some(b));
        assert!(fixture.server.published_sources().contains(&expected_a));
        assert!(fixture.server.published_sources().contains(&resolved));
    }
}

#[test]
fn source_resolution_checks_issued_membership_before_returning_current_thread() {
    let mut fixture = resident_fixture();
    let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    fixture.server.source.launch.session_id = Some(a.into());
    let issued = fixture.server.source.clone();
    *fixture.server.actor.0.confirmed_session.lock().unwrap() = Some(b.into());
    let invoke = |server: &mut Server, source: &chat::Session| {
        server.invoke(Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "resolve_source".into(),
            args: json!([source]),
        })
    };
    let resolved: chat::Session =
        serde_json::from_value(invoke(&mut fixture.server, &issued).unwrap()).unwrap();
    assert_eq!(resolved.launch.session_id.as_deref(), Some(b));
    assert_eq!(resolved.launch.recipe, issued.launch.recipe);
    assert_eq!(resolved.proof, issued.proof);
    assert!(matches!(
        resolved.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
    ));
    // A current projection is accepted too, without changing the issued
    // source or turning ordinary Ready/idle completion into process death.
    assert!(invoke(&mut fixture.server, &resolved).is_ok());
    for change in ["account", "cwd", "uuid", "erased", "proof", "listener"] {
        let mut foreign = issued.clone();
        match change {
            "account" => {
                foreign
                    .launch
                    .recipe
                    .environment
                    .insert("CLAUDE_CONFIG_DIR".into(), "/foreign/account".into());
            }
            "cwd" => foreign.launch.recipe.working_directory = "/foreign/project".into(),
            "uuid" => {
                foreign.launch.session_id = Some("cccccccc-cccc-4ccc-8ccc-cccccccccccc".into())
            }
            "erased" => foreign.launch.session_id = None,
            "proof" => {
                foreign.proof.as_mut().unwrap().host_incarnation =
                    crate::model::HostIncarnation::new("foreign")
            }
            "listener" => {
                foreign.owned_codex_listener = Some(chat::OwnedCodexListener {
                    socket: "/foreign/codex.sock".into(),
                    allocator: foreign.proof.clone().unwrap(),
                })
            }
            _ => unreachable!(),
        }
        assert!(
            invoke(&mut fixture.server, &foreign)
                .unwrap_err()
                .contains("exact issued descriptor"),
            "{change}"
        );
    }
    assert_eq!(fixture.server.source, issued);
    assert!(fixture.server.lineage.is_empty());
    assert!(fixture.server.pending.is_empty());
    assert_eq!(fixture.server.journal.lock().unwrap().latest().unwrap(), 0);
}
#[test]
fn source_resolution_refuses_stale_cleanup_pending_and_process_completion() {
    for refusal in [
        "epoch",
        "proof",
        "close_only",
        "pending",
        "stopping",
        "closed",
        "completed",
    ] {
        let mut fixture = resident_fixture();
        let source = fixture.server.source.clone();
        let mut request = Invocation {
            proof: fixture.server.proof.clone(),
            epoch: Some(fixture.server.epoch),
            method: "resolve_source".into(),
            args: json!([source]),
        };
        match refusal {
            "epoch" => request.epoch = Some(uuid::Uuid::new_v4()),
            "proof" => request.proof.host_incarnation = crate::model::HostIncarnation::new("old"),
            "close_only" => fixture.server.close_only = true,
            "pending" => fixture.server.installed = false,
            "stopping" => fixture.server.session_stopping = true,
            "closed" => fixture.server.closed_snapshot = Some(Arc::new(Snapshot::default())),
            "completed" => fixture.server.actor.close().unwrap(),
            _ => unreachable!(),
        }
        let epoch = fixture.server.epoch;
        let error = fixture.server.invoke(request).unwrap_err();
        assert!(
            if matches!(refusal, "epoch" | "proof") {
                error.contains("superseded")
            } else if refusal == "close_only" {
                error.contains("close-only")
            } else {
                error.contains("live installed")
            },
            "{refusal}: {error}"
        );
        assert_eq!(fixture.server.epoch, epoch);
        assert_eq!(fixture.server.source, source);
        assert!(fixture.server.pending.is_empty());
    }
}
#[test]
fn source_resolution_uses_one_deadline_and_never_returns_a_cached_final_snapshot() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("resolve.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut actor = RemoteActor::fixture();
    Arc::get_mut(&mut actor.0).unwrap().socket = socket;
    let source = actor.0.source.clone();
    let epoch = actor.0.epoch;
    *actor.0.final_snapshot.lock().unwrap() = Some(Arc::new(Snapshot {
        exited: true,
        ..Default::default()
    }));
    let (release, wait) = crossbeam_channel::bounded(1);
    let peer = thread::spawn(move || {
        let (control, _) = listener.accept().unwrap();
        let request: Invocation = receive(&control, TIMEOUT).unwrap();
        assert_eq!(request.method, "resolve_source");
        assert_eq!(request.epoch, Some(epoch));
        // Stall until the client has returned: a client that ignored its
        // deadline would never return.
        let _ = wait.recv();
    });
    let started = Instant::now();
    let (actor, error) = crate::test_timing::returns("source resolution", move || {
        let error = actor
            .resolve_source_until(&source, Instant::now() + Duration::from_millis(40))
            .unwrap_err();
        (actor, error)
    });
    let _ = release.send(());
    peer.join().unwrap();
    assert!(error.contains("deadline"), "{error}");
    // Sooner than the socket timeout an ignored deadline would fall back to.
    assert!(started.elapsed() < TIMEOUT);
    assert_eq!(actor.0.epoch, epoch);
    assert!(actor.pending_inputs().is_empty());
}
#[test]
fn close_only_discards_only_an_unowned_historical_terminal_view() {
    for actual_owner in ["none", "pair", "console"] {
        let mut fixture = resident_fixture();
        fixture.server.close_only = true;
        fixture.server.source.terminal = Some(Box::new(
            crate::terminal_ipc::TerminalDescriptor::relaunchable(
                fixture.server.source.id,
                fixture.server.source.launch.recipe.clone(),
                None,
            )
            .session,
        ));
        if actual_owner == "pair" {
            let mut pair = fixture.server.identity.record.clone();
            pair.kind = Kind::Terminal;
            fixture.server.paired_holder = Some(pair);
        } else if actual_owner == "console" {
            fixture.server.execution_console = Some(fixture.server.source.launch.recipe.clone());
        }
        let issued = fixture.server.source.clone();
        let mut cleared = issued.clone();
        cleared.terminal = None;
        let result = fixture.server.invoke(Invocation {
            proof: fixture.server.proof.clone(),
            epoch: Some(fixture.server.epoch),
            method: "update_source".into(),
            args: json!([
                cleared,
                fixture.server.execution_console,
                fixture.server.paired_holder
            ]),
        });
        if actual_owner == "none" {
            result.unwrap();
            assert!(fixture.server.source.terminal.is_none());
            assert!(fixture.server.lineage.contains(&issued));
        } else {
            assert!(
                result
                    .unwrap_err()
                    .contains("cannot change paired ownership")
            );
            assert_eq!(fixture.server.source, issued);
            assert!(fixture.server.lineage.is_empty());
        }
        assert!(!fixture.server.actor.is_completed());
    }
}
#[test]
fn resident_naming_and_notification_projections_do_not_need_a_helper_socket() {
    let actor = RemoteActor::fixture();
    assert_eq!(actor.conversation_title().unwrap(), (None, None));
    assert_eq!(
        actor.notification_content(crate::notifications::Kind::Waiting),
        Default::default()
    );
    assert!(!actor.0.socket.exists());
}
#[test]
fn concurrent_ready_publication_cannot_be_hidden_by_a_new_revision_on_old_metadata() {
    let mut fixture = resident_fixture();
    fixture.server.actor.0.state.lock().unwrap().ready = false;
    let first = fixture
        .server
        .resident_after_copy(|| {
            let mut state = fixture.server.actor.0.state.lock().unwrap();
            state.ready = true;
            state.generation += 1;
            drop(state);
            fixture.server.changed.fetch_add(1, Ordering::Release);
        })
        .unwrap();
    assert!(!first.ready);
    assert_ne!(first.changed, fixture.server.revision());
    let response = fixture
        .server
        .invoke(Invocation {
            proof: fixture.server.proof.clone(),
            epoch: Some(fixture.server.epoch),
            method: "state".into(),
            args: json!([first.changed]),
        })
        .unwrap();
    let next: Option<Resident> = serde_json::from_value(response).unwrap();
    assert!(
        next.unwrap().ready,
        "the next real state RPC must fetch the raced Ready transition"
    );
}
#[test]
fn read_only_inspection_of_a_stalled_exact_peer_obeys_one_budget_without_actor_changes() {
    let fixture = resident_fixture();
    let mut record = fixture.server.identity.record.clone();
    record.boot = crate::daemon::read_boot_id().unwrap();
    record
        .build
        .as_mut()
        .unwrap()
        .capabilities
        .push(CAPABILITY.into());
    let socket = fixture._root.path().join("deadline-inspect.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (release, wait) = crossbeam_channel::bounded(1);
    let peer = thread::spawn(move || {
        let (control, _) = listener.accept().unwrap();
        let request: Invocation = receive(&control, TIMEOUT).unwrap();
        assert_eq!(request.method, "inspect");
        assert!(request.epoch.is_none());
        // SO_PEERCRED has identified this exact Record.holder process, but
        // its inspection response stalls until the client has returned.
        let _ = wait.recv();
    });
    let epoch = fixture.server.epoch;
    let source = fixture.server.source.clone();
    let proof = fixture.server.proof.clone();
    let started = Instant::now();
    let error = crate::test_timing::returns("the stalled inspection", move || {
        RemoteActor::inspect_at_until(
            &record,
            proof,
            &socket,
            Instant::now() + Duration::from_millis(40),
        )
        .err()
        .expect("stalled inspection must time out")
    });
    let _ = release.send(());
    peer.join().unwrap();
    assert!(error.contains("deadline"), "{error}");
    assert!(started.elapsed() < TIMEOUT);
    assert_eq!(fixture.server.epoch, epoch);
    assert_eq!(fixture.server.source, source);
    assert!(!fixture.server.actor.is_completed());
    assert!(fixture.server.pending.is_empty());
    assert_eq!(fixture.server.journal.lock().unwrap().latest().unwrap(), 0);
}

#[test]
fn proved_close_wins_over_delayed_live_metadata_and_disconnection_publications() {
    let actor = RemoteActor::fixture();
    let delayed = actor.state().as_ref().clone();
    let snapshot = Snapshot {
        generation: 42,
        exited: true,
        ..Default::default()
    };
    actor.publish_closed(snapshot.clone());
    assert!(
        !actor.publish_resident(delayed),
        "the actual worker publication rechecks the stop fence under the state lock"
    );
    actor.publish_unavailable("late socket failure");
    let state = actor.state();
    assert!(state.completed);
    assert!(!state.owns_work);
    assert!(state.failure.is_none());
    assert_eq!(actor.snapshot(None), Some(snapshot));
}
#[test]
fn proved_close_keeps_exact_final_publication_without_a_live_socket() {
    let actor = RemoteActor::fixture();
    let snapshot = Snapshot {
        generation: 42,
        session_id: Some("final-session".into()),
        title: Some("final-title".into()),
        exited: true,
        ..Default::default()
    };
    *actor.0.final_snapshot.lock().unwrap() = Some(Arc::new(snapshot.clone()));
    assert_eq!(actor.snapshot(None), Some(snapshot));
    assert!(actor.snapshot(Some(42)).is_none());
    assert!(actor.owns_work());
    actor.0.preserved.store(true, Ordering::Release);
    assert!(!actor.owns_work());
    assert!(
        actor.state().owns_work,
        "detaching this proxy does not claim provider teardown"
    );
}
#[test]
fn commit_seals_exact_pre_id_capability_before_confirmation_race() {
    let mut fixture = resident_fixture();
    let server = &mut fixture.server;
    server.installed = false;
    let published = published_source(
        &server.source,
        None,
        &chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL },
    );
    *server.actor.0.confirmed_session.lock().unwrap() = Some(uuid::Uuid::new_v4().to_string());
    let commit = || Invocation {
        proof: server.proof.clone(),
        epoch: Some(server.epoch),
        method: "commit".into(),
        args: Value::Null,
    };
    let request = commit();
    server.invoke(request).unwrap();
    assert!(server.installed);
    assert!(server.lineage.contains(&published));
    assert!(published.launch.session_id.is_none());
    let before = serde_json::to_vec(&server.lineage).unwrap();
    let request = Invocation {
        proof: server.proof.clone(),
        epoch: Some(server.epoch),
        method: "commit".into(),
        args: Value::Null,
    };
    server.invoke(request).unwrap();
    assert_eq!(
        serde_json::to_vec(&server.lineage).unwrap(),
        before,
        "lost commit ACK must permit idempotent repeat"
    );
    let stale = Invocation {
        proof: server.proof.clone(),
        epoch: Some(uuid::Uuid::new_v4()),
        method: "commit".into(),
        args: Value::Null,
    };
    assert!(server.invoke(stale).unwrap_err().contains("superseded"));
    assert_eq!(serde_json::to_vec(&server.lineage).unwrap(), before);
    let request = Invocation {
        proof: server.proof.clone(),
        epoch: Some(server.epoch),
        method: "update_source".into(),
        args: json!([published, null, null]),
    };
    server.invoke(request).unwrap();
    assert_eq!(server.source, published);
    let mut forged = published;
    forged.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    let request = Invocation {
        proof: server.proof.clone(),
        epoch: Some(server.epoch),
        method: "update_source".into(),
        args: json!([forged, null, null]),
    };
    assert!(
        server.invoke(request).is_err(),
        "commit must not admit invented thread IDs"
    );
}
#[test]
fn conditional_idle_close_refuses_stale_or_preserved_attachment_without_stopping_provider() {
    for refusal in ["epoch", "proof", "preserved"] {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        let generation = server.actor.snapshot(None).unwrap().generation;
        let mut request = Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "try_close_if_idle".into(),
            args: json!([generation]),
        };
        match refusal {
            "epoch" => request.epoch = Some(uuid::Uuid::new_v4()),
            "proof" => request.proof.host_incarnation = crate::model::HostIncarnation::new("old"),
            "preserved" => server.attachment_preserved = true,
            _ => unreachable!(),
        }
        assert!(server.invoke(request).is_err(), "{refusal}");
        assert!(!server.actor.is_completed(), "{refusal}");
        assert!(server.closed_snapshot.is_none(), "{refusal}");
        assert_eq!(server.actor.snapshot(None).unwrap().generation, generation);
    }
}

#[test]
fn commit_alias_capacity_refuses_before_installation_or_lineage_mutation() {
    for byte_capacity in [false, true] {
        let mut fixture = resident_fixture();
        let server = &mut fixture.server;
        server.installed = false;
        let mut alias = server.source.clone();
        if byte_capacity {
            alias
                .launch
                .recipe
                .environment
                .insert("FIXTURE_METADATA".into(), "x".repeat(4 * 1024 * 1024));
            server.lineage.push(alias);
        } else {
            for _ in 0..1024 {
                alias.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
                server.lineage.push(alias.clone());
            }
        }
        let before = serde_json::to_vec(&server.lineage).unwrap();
        let continuity = server
            .actor
            .0
            .state
            .lock()
            .unwrap()
            .native_continuity
            .clone();
        let request = Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "commit".into(),
            args: Value::Null,
        };
        assert!(
            server
                .invoke(request)
                .unwrap_err()
                .contains("lineage capacity")
        );
        assert!(!server.installed);
        assert_eq!(
            server.actor.0.state.lock().unwrap().native_continuity,
            continuity
        );
        assert_eq!(serde_json::to_vec(&server.lineage).unwrap(), before);
    }
}

#[test]
fn published_source_retains_issued_recipe_and_exact_account_identity() {
    let source = chat::Session {
        id: crate::model::TerminalId::new(),
        launch: chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: "claude".into(),
                arguments: vec!["--session-id".into(), "issued".into()],
                working_directory: "/var/tmp".into(),
                environment: BTreeMap::from([("CLAUDE_CONFIG_DIR".into(), "/account-a".into())]),
                unset_environment: Default::default(),
            },
            session_id: None,
            draft_model: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: chat::NativeContinuity::ReconnectOnly,
    };
    let continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL };
    let published = published_source(&source, Some("confirmed"), &continuity);
    assert_eq!(published.launch.recipe, source.launch.recipe);
    assert_eq!(published.launch.session_id.as_deref(), Some("confirmed"));
    let mut invented = published.clone();
    invented.launch.session_id = Some("invented".into());
    assert_ne!(published, invented);
    invented = published.clone();
    invented
        .launch
        .recipe
        .environment
        .insert("CLAUDE_CONFIG_DIR".into(), "/account-b".into());
    assert_ne!(published, invented);
    assert!(validate_pair(&source, None, None, None).is_ok());
    assert!(validate_pair(&source, Some(&source.launch.recipe), None, None).is_err());
}
#[test]
fn attachment_metadata_and_observation_are_local_and_body_free() {
    let actor = RemoteActor::fixture();
    actor.set_visible(false);
    actor.set_visible(true);
    assert_eq!(actor.observe_next_update(), 1);
    assert_eq!(*actor.0.desired_visible.lock().unwrap(), Some(true));
    assert!(actor.0.desired_observe.load(Ordering::Acquire));
    let snapshot = actor.attachment_snapshot();
    assert_eq!(snapshot.generation, 1);
    assert!(snapshot.ready);
    assert!(snapshot.items.is_empty());
}
#[test]
fn confirmed_install_updates_full_publication_once_and_is_idempotent() {
    let mut snapshot = Snapshot::default();
    assert_eq!(
        snapshot.native_continuity,
        chat::NativeContinuity::ReconnectOnly
    );
    confirm_installed_snapshot(&mut snapshot);
    assert!(matches!(
        snapshot.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
    ));
    let generation = snapshot.generation;
    confirm_installed_snapshot(&mut snapshot);
    assert_eq!(snapshot.generation, generation);
}
#[test]
fn installed_actor_publications_preserve_valid_continuity_before_provider_id() {
    let fixture = resident_fixture();
    let server = &fixture.server;
    let resident = server.resident().unwrap();
    assert_eq!(
        resident.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
    );
    let mut snapshot = server.actor.snapshot(None).unwrap();
    confirm_installed_snapshot(&mut snapshot);
    assert_eq!(snapshot.native_continuity, resident.native_continuity);
    let source = published_source(&server.source, None, &resident.native_continuity);
    assert!(source.launch.session_id.is_none());
    assert!(source.has_valid_native_continuity());
    assert!(
        !source.is_unstarted(),
        "installed pre-ID helper must never become an editable unstarted draft"
    );
    let mut invalid = source.clone();
    invalid.launch.provider = crate::agent::Provider::Codex;
    assert!(!invalid.has_valid_native_continuity());
    invalid = source;
    invalid.native_continuity = chat::NativeContinuity::RetainedClaudeActor {
        protocol: PROTOCOL + 1,
    };
    assert!(!invalid.has_valid_native_continuity());
}

#[test]
fn missing_install_commit_cannot_advertise_durable_continuity() {
    let actor = RemoteActor::fixture();
    actor.0.installed.store(false, Ordering::Release);
    {
        let mut state = actor.0.state.lock().unwrap();
        let state = Arc::make_mut(&mut state);
        state.installed = false;
        state.native_continuity = chat::NativeContinuity::ReconnectOnly;
    }
    assert!(actor.commit_installation().is_err());
    assert!(!actor.state().installed);
    assert_eq!(
        actor.state().native_continuity,
        chat::NativeContinuity::ReconnectOnly
    );
}
#[test]
fn acknowledged_install_cannot_be_reversed_by_an_inflight_precommit_observation() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("install.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut actor = RemoteActor::fixture();
    Arc::get_mut(&mut actor.0).unwrap().socket = socket;
    actor.0.installed.store(false, Ordering::Release);
    let before = {
        let mut state = actor.0.state.lock().unwrap();
        let state = Arc::make_mut(&mut state);
        state.installed = false;
        state.native_continuity = chat::NativeContinuity::ReconnectOnly;
        state.clone()
    };
    let proof = actor.0.proof.clone();
    let epoch = actor.0.epoch;
    let peer = thread::spawn(move || {
        let (control, _) = listener.accept().unwrap();
        let request: Invocation = receive(&control, TIMEOUT).unwrap();
        assert_eq!(request.method, "commit");
        assert_eq!(request.proof, proof);
        assert_eq!(request.epoch, Some(epoch));
        send(&control, &Ok::<Value, String>(Value::Null), TIMEOUT).unwrap();
    });
    actor.commit_installation().unwrap();
    peer.join().unwrap();
    // This is the exact precommit response an already-issued observer
    // RPC can return. Equal revisions cannot revoke a successful ACK.
    assert!(!actor.publish_resident(before.clone()));
    let installed = actor.state();
    assert!(installed.installed);
    assert_eq!(installed.changed, before.changed);
    assert_eq!(installed.generation, before.generation);
    assert_eq!(installed.confirmed_session, before.confirmed_session);
    assert_eq!(
        installed.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { protocol: PROTOCOL }
    );
    let mut contradictory = installed.as_ref().clone();
    contradictory.changed += 1;
    contradictory.generation += 1;
    contradictory.native_continuity = chat::NativeContinuity::ReconnectOnly;
    assert!(!actor.publish_resident(contradictory));
    let mut next = installed.as_ref().clone();
    next.changed += 1;
    next.generation += 1;
    assert!(actor.publish_resident(next));
    assert_eq!(actor.state().generation, before.generation + 1);
    let final_snapshot = Snapshot {
        exited: true,
        ..Default::default()
    };
    actor.publish_closed_inner(final_snapshot.clone(), Some(before));
    assert!(actor.state().completed);
    assert!(actor.state().installed);
    assert_eq!(actor.state().native_continuity, installed.native_continuity);
    assert_eq!(actor.snapshot(None), Some(final_snapshot));
}
#[test]
fn retained_active_model_status_rejects_equal_clock_stale_revisions() {
    let mut actor = RemoteActor::fixture();
    let (published, publications) = crossbeam_channel::unbounded();
    Arc::get_mut(&mut actor.0).unwrap().notify = Arc::new(move |update| {
        let _ = published.send(update);
    });
    let model = |name: &str| chat::ActiveModel {
        id: name.into(),
        label: name.into(),
        reasoning: None,
        default_reasoning: None,
        aliases: Vec::new(),
    };
    let mut older = actor.state().as_ref().clone();
    older.active_model_revision = 5;
    older.active_model = Some(model("older"));
    let mut newer = older.clone();
    newer.active_model_revision = 6;
    newer.active_model = Some(model("newer"));
    assert_eq!(older.changed, newer.changed);
    assert_eq!(older.generation, newer.generation);
    assert!(actor.publish_resident(older.clone()));
    assert!(actor.publish_resident(newer.clone()));
    for revision in [5, 6] {
        assert!(matches!(publications.recv_timeout(TIMEOUT).unwrap(),
            Update::ActiveModel { revision: actual, .. } if actual == revision));
    }
    assert!(!actor.publish_resident(older.clone()));
    assert!(publications.try_recv().is_err());
    let attached = actor.state();
    assert_eq!(attached.active_model_revision, 6);
    assert_eq!(attached.active_model, newer.active_model);
    actor.publish_closed_inner(
        Snapshot {
            exited: true,
            ..Snapshot::default()
        },
        Some(older),
    );
    let closed = actor.state();
    assert!(closed.completed);
    assert_eq!(closed.active_model_revision, 6);
    assert_eq!(closed.active_model, newer.active_model);
}

#[test]
fn confirmation_wakes_durable_broker_without_semantic_outcomes() {
    let mut actor = RemoteActor::fixture();
    let (sent, received) = crossbeam_channel::unbounded();
    Arc::get_mut(&mut actor.0).unwrap().notify = Arc::new(move |update| {
        sent.send(update).unwrap();
    });
    let mut confirmed = actor.state().as_ref().clone();
    confirmed.confirmed_session = Some(uuid::Uuid::new_v4().to_string());
    assert_eq!(confirmed.outbox_latest, confirmed.outbox_ack);
    assert!(actor.publish_resident(confirmed.clone()));
    assert!(
        received
            .try_iter()
            .any(|update| matches!(update, Update::RetainedOutcomes))
    );
    assert!(actor.publish_resident(confirmed));
    assert!(
        received.try_iter().next().is_none(),
        "unchanged metadata must not churn the broker"
    );
}

#[test]
fn synchronous_refresh_publishes_all_transitions_and_rejects_a_delayed_observer_response() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("refresh.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let fixture = RemoteActor::fixture();
    let mut old = fixture.state().as_ref().clone();
    old.ready = false;
    old.busy = true;
    old.idle = false;
    old.activity = crate::agent::ActivityState::Working;
    let mut next = old.clone();
    next.changed += 1;
    next.generation += 1;
    next.prompt_revision = 1;
    next.ready = true;
    next.busy = false;
    next.idle = true;
    next.activity = crate::agent::ActivityState::Idle;
    next.confirmed_session = Some("confirmed-after-refresh".into());
    next.outbox_latest = 1;
    let usage = Update::AccountUsage {
        report: crate::account::LiveReport::Claude(crate::account::claude::LiveUpdate {
            windows: Vec::new(),
            status: None,
        }),
        observed_at: 123,
    };
    next.account_reports.push(usage.clone());
    let addresses = vec![chat::AgentAddress {
        address: "a1892b93e88999a7d".into(),
        subagent: "toolu_relocate".into(),
    }];
    next.agent_addresses = addresses.clone();
    // A helper that predates them reports none.
    let mut predating = serde_json::to_value(&old).unwrap();
    predating.as_object_mut().unwrap().remove("agent_addresses");
    assert!(
        serde_json::from_value::<Resident>(predating)
            .unwrap()
            .agent_addresses
            .is_empty()
    );
    let delayed = old.clone();
    let expected = vec![
        Update::Observed {
            generation: next.generation,
        },
        Update::Presentation,
        Update::Activity {
            state: next.activity,
            generation: next.generation,
        },
        Update::Ready,
        Update::PromptAvailable,
        Update::RetainedOutcomes,
        usage,
        Update::AgentAddresses(addresses),
    ];
    let (observed_poll, poll_started) = crossbeam_channel::bounded(1);
    let (release, wait_release) = crossbeam_channel::bounded(1);
    let (processed, poll_processed) = crossbeam_channel::bounded(1);
    let (stop, stopped) = crossbeam_channel::bounded(1);
    let peer = thread::spawn(move || {
        // This ordering fixture models an older helper without readiness.
        let (watch, _) = listener.accept().unwrap();
        let request: Invocation = receive(&watch, TIMEOUT).unwrap();
        assert_eq!(request.method, "watch");
        send(&watch, &Err::<Value, _>(UNKNOWN_METHOD.to_owned()), TIMEOUT).unwrap();
        // Hold the observer's response to force refresh_state to publish
        // the new resident before that observer returns an unchanged poll.
        let (observer, _) = listener.accept().unwrap();
        let request: Invocation = receive(&observer, TIMEOUT).unwrap();
        assert_eq!(request.method, "state");
        observed_poll.send(()).unwrap();
        let (refresh, _) = listener.accept().unwrap();
        let request: Invocation = receive(&refresh, TIMEOUT).unwrap();
        assert_eq!(request.method, "state");
        send(
            &refresh,
            &Ok::<Value, String>(serde_json::to_value(Some(next)).unwrap()),
            TIMEOUT,
        )
        .unwrap();
        wait_release.recv_timeout(TIMEOUT).unwrap();
        send(
            &observer,
            &Ok::<Value, String>(serde_json::to_value(Some(delayed)).unwrap()),
            TIMEOUT,
        )
        .unwrap();
        let (observer, _) = listener.accept().unwrap();
        let request: Invocation = receive(&observer, TIMEOUT).unwrap();
        assert_eq!(request.method, "state");
        send(&observer, &Ok::<Value, String>(Value::Null), TIMEOUT).unwrap();
        processed.send(()).unwrap();
        let _ = stopped.recv_timeout(TIMEOUT);
    });
    let (published, publications) = crossbeam_channel::unbounded();
    let actor = RemoteActor::from_attached_at(
        Attached {
            protocol: PROTOCOL,
            close_only: false,
            identity: fixture.identity(),
            epoch: uuid::Uuid::new_v4(),
            resident: old,
            source: fixture.0.source.clone(),
            launch_account_scope: Err("fixture".into()),
            execution_environment: Default::default(),
            recovery_environment: BTreeMap::new(),
        },
        fixture.0.proof.clone(),
        None,
        socket,
        move |update| {
            let _ = published.send(update);
        },
    )
    .unwrap();
    poll_started.recv_timeout(TIMEOUT).unwrap();
    actor.refresh_state().unwrap();
    for expected in expected {
        assert_eq!(publications.recv_timeout(TIMEOUT).unwrap(), expected);
    }
    release.send(()).unwrap();
    poll_processed.recv_timeout(TIMEOUT).unwrap();
    assert!(
        publications.try_recv().is_err(),
        "a delayed stale observer response must publish no transitions"
    );
    assert!(actor.state().ready);
    assert_eq!(
        actor.state().confirmed_session.as_deref(),
        Some("confirmed-after-refresh")
    );
    assert_eq!(actor.state().activity, crate::agent::ActivityState::Idle);
    actor.0.stop.store(true, Ordering::Release);
    stop.send(()).unwrap();
    peer.join().unwrap();
}
/// #2849: the helper confirms a conversation and then journals the run
/// outcome that follows it. An outcome read must leave the proxy holding
/// an identity at least as new as its page, so the daemon cannot settle
/// that run before publishing the conversation it requires.
#[test]
fn an_outcome_page_is_never_newer_than_the_identity_read_with_it() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("outcome-read.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let actor = RemoteActor::fixture_at(socket);
    let unconfirmed = actor.state().as_ref().clone();
    assert!(unconfirmed.confirmed_session.is_none());
    let mut confirmed = unconfirmed.clone();
    confirmed.changed += 1;
    confirmed.confirmed_session = Some("confirmed-before-outcome".into());
    confirmed.outbox_latest += 1;
    let finished = OutboxEntry {
        sequence: confirmed.outbox_latest,
        update: Update::Execution {
            run: crate::model::RunId::new(),
            update: crate::agent_execution::Update::Finished {
                state: crate::agent_execution::State::Succeeded,
                result: Default::default(),
            },
        },
        confirmed_session: None,
    };
    // The helper confirms and journals the outcome right after answering
    // the first request of the read, whichever request that is.
    let peer = thread::spawn(move || {
        let mut published = false;
        for _ in 0..4 {
            let (stream, _) = listener.accept().unwrap();
            let request: Invocation = receive(&stream, TIMEOUT).unwrap();
            let reply = match request.method.as_str() {
                "state" => {
                    serde_json::to_value(Some(if published { &confirmed } else { &unconfirmed }))
                }
                "outbox_unacked" => serde_json::to_value(if published {
                    vec![finished.clone()]
                } else {
                    Vec::new()
                }),
                method => panic!("unexpected outcome-read request {method}"),
            };
            send(&stream, &Ok::<Value, String>(reply.unwrap()), TIMEOUT).unwrap();
            published = true;
        }
    });
    for _ in 0..2 {
        let page = actor.uncommitted_outcomes().unwrap();
        assert!(
            page.is_empty() || actor.state().confirmed_session.is_some(),
            "the page holds an outcome that follows a confirmation the proxy has not read"
        );
    }
    assert_eq!(
        actor.state().confirmed_session.as_deref(),
        Some("confirmed-before-outcome")
    );
    peer.join().unwrap();
}
/// #2339: while a close drains the outbox, the outcome broker refreshes
/// the same closed actor. When its newer reply is published first, the
/// close's older reply is stale, not a failed close.
#[test]
fn a_closed_refresh_overtaken_by_a_newer_one_is_stale_not_a_failure() {
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = root.path().join("closed-refresh.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let actor = RemoteActor::fixture_at(socket);
    let mut older = actor.state().as_ref().clone();
    older.completed = true;
    older.close.completed = true;
    let mut newer = older.clone();
    newer.changed += 1;
    newer.outbox_ack = 1;
    newer.outbox_latest = 1;
    actor.publish_closed_inner(
        Snapshot {
            exited: true,
            ..Default::default()
        },
        Some(newer.clone()),
    );
    // Current, yet it contradicts the acknowledged installation.
    let mut contradictory = newer.clone();
    contradictory.installed = false;
    let replies = [older, contradictory];
    let peer = thread::spawn(move || {
        for reply in replies {
            let (stream, _) = listener.accept().unwrap();
            let request: Invocation = receive(&stream, TIMEOUT).unwrap();
            assert_eq!(request.method, "state");
            send(
                &stream,
                &Ok::<Value, String>(serde_json::to_value(Some(reply)).unwrap()),
                TIMEOUT,
            )
            .unwrap();
        }
    });
    actor.refresh_state().unwrap();
    assert_eq!(actor.state().changed, newer.changed);
    assert_eq!(actor.state().outbox_ack, 1);
    assert_eq!(
        actor.refresh_state().unwrap_err(),
        "Closed actor metadata was superseded during refresh"
    );
    peer.join().unwrap();
}
#[test]
fn oversized_input_refusal_keeps_the_next_normal_input_admissible() {
    let actor = RemoteActor::fixture();
    // More than a complete wire frame, while still below the aggregate
    // pending-byte budget: this must fail before publishing any receipt.
    let result = actor.submit(
        None,
        Command::Prompt {
            text: "x".repeat(35 * 1024 * 1024),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        },
    );
    assert!(matches!(result, Err(Rejection::Refused(_))));
    assert!(actor.pending_inputs().is_empty());
    assert!(actor.0.uncertain_input.lock().unwrap().is_none());
    assert_eq!(*actor.0.input_sequence.lock().unwrap(), 1);
    // Missing fixture socket makes this normal input uncertain, but it is
    // admitted as the first exact envelope, proving the oversized refusal
    // did not permanently fence this actor.
    let reply = actor.enqueue(None, Command::Refresh).unwrap();
    assert_eq!(actor.pending_inputs()[0].sequence, 1);
    assert!(matches!(
        reply.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    ));
}
#[test]
fn failed_input_call_is_retransmitted_then_settled_by_the_helper() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = directory.path().join("actor.sock");
    let actor = RemoteActor::fixture_at(socket.clone());
    // The helper is unreachable, so the original `input` call fails and
    // its delivery is unknown: it may never have reached the helper.
    let response = actor.enqueue(None, Command::Refresh).unwrap();
    let identity = actor.pending_inputs().pop().unwrap();
    assert_eq!(
        actor.enqueue_now(Command::Refresh).unwrap_err(),
        UNSETTLED_INPUT
    );
    actor.resolve_uncertain_input();
    let retransmit = |actor: &RemoteActor| {
        let uncertain = actor.0.uncertain_input.lock().unwrap();
        let retransmit = uncertain.as_ref().unwrap().retransmit.as_ref();
        retransmit.map(|retransmit| (retransmit.next, retransmit.delay))
    };
    let (next, delay) = retransmit(&actor).expect("a failed call schedules retransmission");
    assert!(next > Instant::now() && delay > RETRANSMIT_DELAY);
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    actor.resolve_uncertain_input();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "nothing is retransmitted before its backoff elapses"
    );
    listener.set_nonblocking(false).unwrap();
    let expected = identity.clone();
    let helper = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let request: Invocation = receive(&stream, TIMEOUT).unwrap();
        assert_eq!(request.method, "input");
        let (identity, payload): (InputIdentity, Value) =
            serde_json::from_value(request.args).unwrap();
        assert_eq!(identity, expected, "the exact original identity");
        assert_eq!(
            payload_digest(&serde_json::to_vec(&payload).unwrap()),
            identity.hash,
            "the exact original envelope"
        );
        let receipt = InputReceipt {
            identity,
            state: InputState::Acknowledged(Ok(())),
        };
        send(
            &stream,
            &Ok::<Value, String>(serde_json::to_value(receipt).unwrap()),
            TIMEOUT,
        )
        .unwrap();
    });
    let backdate = |actor: &RemoteActor| {
        let mut uncertain = actor.0.uncertain_input.lock().unwrap();
        uncertain
            .as_mut()
            .unwrap()
            .retransmit
            .as_mut()
            .unwrap()
            .next = Instant::now();
    };
    backdate(&actor);
    actor.resolve_uncertain_input();
    helper.join().unwrap();
    assert!(actor.0.uncertain_input.lock().unwrap().is_none());
    assert_eq!(response.try_recv(), Ok(Ok(())));
    assert!(actor.pending_inputs().is_empty());

    // Once its caller stops waiting, the input is never sent again: the
    // helper settles it instead, so it cannot start after the caller was
    // told its outcome is unknown, and the fence lifts.
    std::fs::remove_file(&socket).unwrap();
    let unknown = actor.enqueue(None, Command::Refresh).unwrap();
    let expected = actor.pending_inputs().pop().unwrap();
    backdate(&actor);
    actor
        .0
        .uncertain_input
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .acknowledgement_deadline = Instant::now();
    let listener = UnixListener::bind(&socket).unwrap();
    let settle =
        |listener: &UnixListener, expected: &InputIdentity, answer: Result<Value, String>| {
            let (stream, _) = listener.accept().unwrap();
            let request: Invocation = receive(&stream, TIMEOUT).unwrap();
            assert_eq!(request.method, "abandon_input", "never the input itself");
            let (identity, reason): (InputIdentity, String) =
                serde_json::from_value(request.args).unwrap();
            assert_eq!(&identity, expected);
            assert_eq!(reason, UNCONFIRMED_INPUT);
            send(&stream, &answer, TIMEOUT).unwrap();
        };
    // A helper that cannot settle the input yet, such as on a busy
    // journal, leaves the fence in place, and the proxy asks again after
    // its backoff.
    let busy = thread::scope(|scope| {
        let helper = scope.spawn(|| settle(&listener, &expected, Err("database is locked".into())));
        actor.resolve_uncertain_input();
        helper.join().unwrap();
        retransmit(&actor)
    });
    assert!(busy.is_some(), "the input stays fenced");
    backdate(&actor);
    let settled = InputReceipt {
        identity: expected.clone(),
        state: InputState::Acknowledged(Err(UNCONFIRMED_INPUT.into())),
    };
    thread::scope(|scope| {
        let helper = scope.spawn(|| {
            settle(
                &listener,
                &expected,
                Ok(serde_json::to_value(&settled).unwrap()),
            )
        });
        actor.resolve_uncertain_input();
        helper.join().unwrap();
    });
    assert!(actor.0.uncertain_input.lock().unwrap().is_none());
    assert_eq!(
        unknown.try_recv(),
        Ok(Err(Rejection::Refused(UNCONFIRMED_INPUT.into())))
    );
    assert!(actor.pending_inputs().is_empty());

    // An older helper cannot settle inputs at all. The proxy stops asking
    // and refuses new input at once, saying how to recover, instead of
    // making each one wait for a fence that will not lift.
    std::fs::remove_file(&socket).unwrap();
    let stuck = actor.enqueue(None, Command::Refresh).unwrap();
    let expected = actor.pending_inputs().pop().unwrap();
    assert_eq!(expected.sequence, 3);
    backdate(&actor);
    actor
        .0
        .uncertain_input
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .acknowledgement_deadline = Instant::now();
    let listener = UnixListener::bind(&socket).unwrap();
    thread::scope(|scope| {
        let helper = scope.spawn(|| settle(&listener, &expected, Err(UNKNOWN_METHOD.into())));
        actor.resolve_uncertain_input();
        helper.join().unwrap();
    });
    assert!(
        retransmit(&actor).is_none(),
        "an older helper is not asked again"
    );
    // Refused at once, for the conversation: waiting on the fence would end
    // in UNSETTLED_INPUT, and trying later changes nothing.
    assert_eq!(
        actor.enqueue(None, Command::Refresh).unwrap_err(),
        UNRESOLVABLE_INPUT
    );
    // A receipt the helper reports on its own still lifts the fence.
    actor.apply_receipts(&[InputReceipt {
        identity: expected,
        state: InputState::Acknowledged(Ok(())),
    }]);
    assert_eq!(stuck.try_recv(), Ok(Ok(())));
    drop(listener);
    actor.enqueue_now(Command::Refresh).unwrap();
    let expected = actor.pending_inputs().pop().unwrap();
    assert_eq!(expected.sequence, 4);

    // A helper that refuses to settle an input for now, as a full or busy
    // journal does, stops new input waiting on it after `SETTLE_ATTEMPTS`
    // refusals, but is still asked: one that recovers settles it. A refusal
    // its journal gives for good, as for an input it retired, ends the
    // asking at once, and new input is refused for the conversation.
    std::fs::remove_file(&socket).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let refuse = |actor: &RemoteActor, refusal: &str| {
        backdate(actor);
        {
            let mut uncertain = actor.0.uncertain_input.lock().unwrap();
            let uncertain = uncertain.as_mut().unwrap();
            uncertain.acknowledgement_deadline = Instant::now();
            uncertain.retransmit.as_mut().unwrap().next = Instant::now();
        }
        thread::scope(|scope| {
            let helper = scope.spawn(|| settle(&listener, &expected, Err(refusal.into())));
            actor.resolve_uncertain_input();
            helper.join().unwrap();
        });
    };
    let unsettleable = |actor: &RemoteActor| {
        actor
            .0
            .uncertain_input
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .unsettleable
    };
    let busy =
        "Actor input receipt capacity reached; commit completed receipts before sending more";
    for attempt in 1..=SETTLE_ATTEMPTS {
        assert!(!unsettleable(&actor), "new input still waits at {attempt}");
        refuse(&actor, busy);
        assert!(retransmit(&actor).is_some(), "still asking at {attempt}");
    }
    assert!(unsettleable(&actor), "new input no longer waits");
    assert_eq!(
        actor.enqueue(None, Command::Refresh).unwrap_err(),
        UNSETTLEABLE_INPUT
    );
    refuse(&actor, super::super::journal::RETIRED_RECEIPT);
    assert!(
        retransmit(&actor).is_none(),
        "a final refusal ends the asking"
    );
    assert_eq!(
        actor.enqueue(None, Command::Refresh).unwrap_err(),
        UNRESOLVABLE_INPUT
    );
    drop(listener);
    // A receipt the helper reports on its own still lifts the fence.
    actor.apply_receipts(&[InputReceipt {
        identity: expected,
        state: InputState::Acknowledged(Err(UNCONFIRMED_INPUT.into())),
    }]);
    assert!(actor.0.uncertain_input.lock().unwrap().is_none());
    actor.enqueue_now(Command::Refresh).unwrap();
}
/// An admitted input the actor's queue will not take is refused as its
/// cause: a closing actor's for good, a full queue's as crowded.
#[test]
fn an_input_the_actor_queue_refuses_is_refused_as_its_cause() {
    assert_eq!(unrun_input_outcome(QueueRefusal::Closed), CLOSING_INPUT);
    assert_eq!(unrun_input_outcome(QueueRefusal::Full), CROWDED_INPUT);
}
/// A helper that does not answer requests to settle an input, whether
/// unreachable or wedged so each request times out, is not counted as
/// refusing them. After `SETTLE_LIMIT` new input stops waiting on it, but
/// it is still asked, so a helper that returns can still settle it.
#[test]
fn new_input_stops_waiting_on_a_helper_that_never_answers() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let socket = directory.path().join("actor.sock");
    let actor = RemoteActor::fixture_at(socket.clone());
    let unknown = actor.enqueue(None, Command::Refresh).unwrap();
    let expected = actor.pending_inputs().pop().unwrap();
    let settle_now = |actor: &RemoteActor| {
        let mut uncertain = actor.0.uncertain_input.lock().unwrap();
        let uncertain = uncertain.as_mut().unwrap();
        uncertain.acknowledgement_deadline = Instant::now();
        uncertain.retransmit.as_mut().unwrap().next = Instant::now();
    };
    let state = |actor: &RemoteActor| {
        let uncertain = actor.0.uncertain_input.lock().unwrap();
        let uncertain = uncertain.as_ref().unwrap();
        (
            uncertain.settling_since.is_some(),
            uncertain.settle_failures,
            uncertain.unsettleable,
            uncertain.retransmit.is_some(),
        )
    };
    // An unreachable helper.
    for attempt in 1..=SETTLE_ATTEMPTS * 2 {
        settle_now(&actor);
        actor.resolve_uncertain_input();
        assert_eq!(
            state(&actor),
            (true, 0, false, true),
            "asked to settle, uncounted, at {attempt}"
        );
    }
    // A wedged helper takes the request and never answers it.
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let accept = |listener: &UnixListener| {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "the proxy never asked");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    };
    let wedged = |listener: &UnixListener| {
        let stream = accept(listener);
        let request: Invocation = receive(&stream, TIMEOUT).unwrap();
        assert_eq!(request.method, "abandon_input");
        // Hold the connection until the proxy gives up on the answer.
        let _ = stream.set_read_timeout(Some(TIMEOUT * 2));
        let _ = std::io::Read::read(&mut &stream, &mut [0; 1]);
    };
    settle_now(&actor);
    thread::scope(|scope| {
        let helper = scope.spawn(|| wedged(&listener));
        actor.resolve_uncertain_input();
        helper.join().unwrap();
    });
    assert_eq!(
        state(&actor),
        (true, 0, false, true),
        "a timeout is uncounted"
    );
    let delay = |actor: &RemoteActor| {
        let uncertain = actor.0.uncertain_input.lock().unwrap();
        uncertain
            .as_ref()
            .unwrap()
            .retransmit
            .as_ref()
            .unwrap()
            .delay
    };
    assert_eq!(delay(&actor), TIMEOUT, "asked as often until overdue");
    // Past the limit, new input is refused at once.
    *actor
        .0
        .uncertain_input
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .settling_since
        .as_mut()
        .unwrap() -= SETTLE_LIMIT;
    settle_now(&actor);
    thread::scope(|scope| {
        let helper = scope.spawn(|| wedged(&listener));
        actor.resolve_uncertain_input();
        helper.join().unwrap();
    });
    assert_eq!(state(&actor), (true, 0, true, true), "refused, still asked");
    assert!(delay(&actor) > TIMEOUT, "asked less often once overdue");
    // Refused at once: waiting on the fence would end in UNSETTLED_INPUT.
    assert_eq!(
        actor.enqueue(None, Command::Refresh).unwrap_err(),
        UNSETTLEABLE_INPUT
    );
    // A helper that returns still settles it, which lifts the fence.
    settle_now(&actor);
    let settled = InputReceipt {
        identity: expected.clone(),
        state: InputState::Acknowledged(Err(UNCONFIRMED_INPUT.into())),
    };
    thread::scope(|scope| {
        let helper = scope.spawn(|| {
            let stream = accept(&listener);
            let request: Invocation = receive(&stream, TIMEOUT).unwrap();
            assert_eq!(request.method, "abandon_input");
            send(
                &stream,
                &Ok::<Value, String>(serde_json::to_value(&settled).unwrap()),
                TIMEOUT,
            )
            .unwrap();
        });
        actor.resolve_uncertain_input();
        helper.join().unwrap();
    });
    assert!(actor.0.uncertain_input.lock().unwrap().is_none());
    assert_eq!(
        unknown.try_recv(),
        Ok(Err(Rejection::Refused(UNCONFIRMED_INPUT.into())))
    );
}
/// A proxy attached to the real helper request handling, served one
/// request at a time as the helper's serve loop runs them.
struct ServedHelper {
    actor: RemoteActor,
    fixture: Arc<Mutex<Fixture>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    stop: Arc<AtomicBool>,
    helper: Option<thread::JoinHandle<()>>,
}
impl ServedHelper {
    fn new(prepare: impl FnOnce(&mut Server)) -> Self {
        let mut fixture = resident_fixture();
        prepare(&mut fixture.server);
        fixture.server.listener.set_nonblocking(true).unwrap();
        let attached = fixture.server.attached().unwrap();
        let proof = fixture.server.proof.clone();
        let socket = fixture.server.socket.clone();
        let fixture = Arc::new(Mutex::new(fixture));
        let requests = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let helper = {
            let (fixture, requests, stop) = (fixture.clone(), requests.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let mut fixture = fixture.lock().unwrap();
                    fixture.server.finish_inputs();
                    let control = match fixture.server.listener.accept() {
                        Ok((control, _)) => control,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            drop(fixture);
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    let request: Invocation = receive(&control, TIMEOUT).unwrap();
                    requests
                        .lock()
                        .unwrap()
                        .push((request.method.clone(), request.args.clone()));
                    // This deliberately models a pre-readiness protocol-2 helper.
                    let response = if request.method == "watch" {
                        Err(UNKNOWN_METHOD.into())
                    } else {
                        fixture.server.invoke(request)
                    };
                    let _ = send(&control, &response, TIMEOUT);
                }
            })
        };
        let actor = RemoteActor::from_attached_at(attached, proof, None, socket, |_| {}).unwrap();
        Self {
            actor,
            fixture,
            requests,
            stop,
            helper: Some(helper),
        }
    }
    /// The arguments of the first `input` request for `sequence`.
    fn input_request(&self, sequence: u64) -> Value {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|(method, args)| method == "input" && args[0]["sequence"] == sequence)
            .map(|(_, args)| args.clone())
            .unwrap()
    }
    /// Deliver `args` to the helper again, as a late copy of its frame.
    fn invoke_input(&self, args: Value) -> Result<Value, String> {
        let mut fixture = self.fixture.lock().unwrap();
        let server = &mut fixture.server;
        server.invoke(Invocation {
            proof: server.proof.clone(),
            epoch: Some(server.epoch),
            method: "input".into(),
            args,
        })
    }
}
impl Drop for ServedHelper {
    fn drop(&mut self) {
        self.actor.0.stop.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        self.actor.0.wake.notify();
        if let Some(helper) = self.helper.take() {
            let _ = helper.join();
        }
    }
}
#[test]
fn replacement_proxy_reclaims_an_orphaned_unknown_receipt() {
    let identity = InputIdentity {
        sequence: 1,
        id: uuid::Uuid::new_v4(),
        hash: [7; 32],
    };
    let served = ServedHelper::new(|server| {
        let mut journal = server.journal.lock().unwrap();
        journal.admit(&identity).unwrap();
        journal
            .finish(&identity, InputState::Unknown("no provider receipt".into()))
            .unwrap();
    });
    let actor = &served.actor;
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.0.recovering_inputs.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "unknown receipt was never retired"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        actor
            .call::<Option<InputReceipt>>("input_receipt", json!([identity]))
            .unwrap_err(),
        journal::RETIRED_RECEIPT
    );
    assert!(
        served
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _)| method != "input")
    );
    let next = actor.enqueue(None, Command::Refresh).unwrap();
    assert_eq!(next.recv_timeout(ACKNOWLEDGEMENT_WAIT).unwrap(), Ok(()));
    assert_eq!(served.input_request(2)[0]["sequence"], 2);
}
/// #2242: an input the helper refused without a receipt fenced every
/// later input of the conversation for good.
#[test]
fn refused_input_is_settled_and_the_next_input_is_admitted() {
    // The helper refuses input before installation, without a receipt.
    let served = ServedHelper::new(|server| server.installed = false);
    let (actor, fixture) = (&served.actor, &served.fixture);
    let installed = |installed| fixture.lock().unwrap().server.installed = installed;
    // A refusal that ends while its caller waits is still delivered.
    let model = Command::SetModel {
        model: "haiku".into(),
        reasoning: None,
    };
    let delivered = actor.enqueue(None, model).unwrap();
    thread::sleep(RETRANSMIT_DELAY * 3);
    installed(true);
    assert_eq!(
        delivered.recv_timeout(ACKNOWLEDGEMENT_WAIT).unwrap(),
        Ok(())
    );
    // A refusal that lasts until its caller stops waiting is settled: the
    // caller hears it, rather than an unknown outcome, and the
    // conversation takes the next input.
    installed(false);
    let refused = actor.enqueue(None, Command::Refresh).unwrap();
    actor
        .0
        .uncertain_input
        .lock()
        .unwrap()
        .as_mut()
        .expect("the refused input is fenced")
        .acknowledgement_deadline = Instant::now();
    match refused.recv_timeout(Duration::from_secs(3)) {
        Ok(Err(Rejection::Refused(reason))) => assert_eq!(
            reason, UNCONFIRMED_INPUT,
            "the helper's own reason stays in the logs"
        ),
        outcome => panic!("the refused input was never settled: {outcome:?}"),
    }
    assert!(actor.0.uncertain_input.lock().unwrap().is_none());
    installed(true);
    let next = actor.enqueue(None, Command::Refresh).unwrap();
    assert_eq!(next.recv_timeout(ACKNOWLEDGEMENT_WAIT).unwrap(), Ok(()));
    // A late copy of the refused frame never runs: it is answered with
    // the refusal or, once the proxy retired that, refused as retired.
    match served
        .invoke_input(served.input_request(2))
        .map(serde_json::from_value::<InputReceipt>)
    {
        Ok(Ok(InputReceipt {
            state: InputState::Acknowledged(Err(_)),
            ..
        })) => {}
        Err(refusal) if refusal.contains("never replay") => {}
        late => panic!("a late copy of a refused input was admitted: {late:?}"),
    }
    assert!(fixture.lock().unwrap().server.pending.is_empty());
}
/// A refusal that waiting cannot change is reported at once with its own
/// next step, rather than retransmitted until the caller gives up and
/// reported as a delay.
#[test]
fn a_refusal_that_cannot_clear_is_reported_at_once() {
    type Toggle = fn(&mut Server, bool);
    let stopping: Toggle = |server, on| server.session_stopping = on;
    let preserved: Toggle = |server, on| server.attachment_preserved = on;
    for (toggle, expected) in [(stopping, CLOSING_INPUT), (preserved, HANDOVER_INPUT)] {
        let served = ServedHelper::new(|server| toggle(server, true));
        let actor = &served.actor;
        let started = Instant::now();
        let refused = actor.enqueue(None, Command::Refresh).unwrap();
        assert_eq!(
            refused.recv_timeout(ACKNOWLEDGEMENT_WAIT),
            Ok(Err(Rejection::Refused(expected.into())))
        );
        // Well before the caller's wait would end with retransmission.
        assert!(
            started.elapsed() < ACKNOWLEDGEMENT_WAIT / 2,
            "reported at once"
        );
        assert!(
            actor.0.uncertain_input.lock().unwrap().is_none(),
            "the refusal lifts the fence"
        );
        // The refusal is the input's outcome: a late copy of its frame
        // returns it even after the refusal's cause has cleared. Once the
        // caller has heard it, the proxy retires that receipt on its own
        // worker, and from then on a late copy is refused as retired.
        toggle(&mut served.fixture.lock().unwrap().server, false);
        match served.invoke_input(served.input_request(1)) {
            Ok(late) => assert_eq!(
                serde_json::from_value::<InputReceipt>(late).unwrap().state,
                InputState::Acknowledged(Err(expected.into()))
            ),
            Err(refusal) => assert!(refusal.contains("never replay"), "{refusal}"),
        }
        // The next input is admitted.
        let next = actor.enqueue(None, Command::Refresh).unwrap();
        assert_eq!(next.recv_timeout(ACKNOWLEDGEMENT_WAIT).unwrap(), Ok(()));
        assert!(served.fixture.lock().unwrap().server.pending.is_empty());
    }
}
/// A damaged frame, a request this helper's protocol does not describe,
/// and input after a continuity loss are each refused for good, without
/// running, with their own outcome.
#[test]
fn inputs_that_can_never_run_are_refused_as_their_outcome() {
    let served = ServedHelper::new(|_| {});
    let actor = &served.actor;
    let delivered = actor.enqueue(None, Command::Refresh).unwrap();
    assert_eq!(
        delivered.recv_timeout(ACKNOWLEDGEMENT_WAIT).unwrap(),
        Ok(())
    );
    let refused = |sequence: u64, payload: Value, hash: Option<[u8; 32]>| {
        let mut args = served.input_request(1);
        let hash = hash.unwrap_or_else(|| payload_digest(&serde_json::to_vec(&payload).unwrap()));
        args[0]["sequence"] = json!(sequence);
        args[0]["id"] = json!(uuid::Uuid::new_v4());
        args[0]["hash"] = json!(hash);
        args[1] = payload;
        serde_json::from_value::<InputReceipt>(served.invoke_input(args).unwrap())
            .unwrap()
            .state
    };
    let original: InputIdentity =
        serde_json::from_value(served.input_request(1)[0].clone()).unwrap();
    assert_eq!(
        refused(
            2,
            json!({"IdlePrompt": {"text": "altered"}}),
            Some(original.hash)
        ),
        InputState::Acknowledged(Err(DAMAGED_INPUT.into()))
    );
    assert_eq!(
        refused(3, json!({"FromANewerProtocol": {}}), None),
        InputState::Acknowledged(Err(UNSUPPORTED_INPUT.into()))
    );
    assert!(served.fixture.lock().unwrap().server.pending.is_empty());
    // After a continuity loss the proxy's caller hears it at once.
    served
        .fixture
        .lock()
        .unwrap()
        .server
        .journal
        .lock()
        .unwrap()
        .record_loss("fixture loss")
        .unwrap();
    *actor.0.input_sequence.lock().unwrap() = 4;
    let started = Instant::now();
    let lost = actor.enqueue(None, Command::Refresh).unwrap();
    assert_eq!(
        lost.recv_timeout(ACKNOWLEDGEMENT_WAIT),
        Ok(Err(Rejection::Refused(DISCONTINUOUS_INPUT.into())))
    );
    assert!(
        started.elapsed() < ACKNOWLEDGEMENT_WAIT / 2,
        "reported at once"
    );
    assert!(served.fixture.lock().unwrap().server.pending.is_empty());
}
#[test]
fn a_new_input_waits_for_an_unconfirmed_one_to_settle() {
    let actor = RemoteActor::fixture();
    let _first = actor.enqueue(None, Command::Refresh).unwrap();
    let first = actor.pending_inputs().pop().unwrap();
    thread::scope(|scope| {
        let next = scope.spawn(|| actor.enqueue(None, Command::Interrupt));
        // The second caller waits rather than being refused.
        thread::sleep(RETRANSMIT_DELAY * 2);
        assert!(!next.is_finished());
        assert_eq!(actor.pending_inputs(), vec![first.clone()]);
        actor.apply_receipts(&[InputReceipt {
            identity: first.clone(),
            state: InputState::Accepted,
        }]);
        next.join().unwrap().unwrap();
    });
    let pending = actor.pending_inputs();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[1].sequence, 2);
}
#[test]
fn lost_input_reply_keeps_the_original_envelope_and_fences_new_input() {
    let actor = RemoteActor::fixture();
    let response = actor.enqueue(None, Command::Interrupt).unwrap();
    let identity = actor.pending_inputs().pop().unwrap();
    assert_eq!(identity.sequence, 1);
    assert!(actor.enqueue_now(Command::Interrupt).is_err());
    assert_eq!(actor.pending_inputs(), vec![identity.clone()]);
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let mut journal = Journal::open(directory.path()).unwrap();
    let receipt = journal.admit(&identity).unwrap();
    assert_eq!(journal.admit(&identity).unwrap().identity, identity);
    actor.apply_receipts(&[receipt]);
    assert!(actor.0.uncertain_input.lock().unwrap().is_none());
    journal
        .finish(&identity, InputState::Acknowledged(Ok(())))
        .unwrap();
    actor.apply_receipts(&[journal.input(&identity).unwrap().unwrap()]);
    assert_eq!(response.try_recv(), Ok(Ok(())));
    assert!(actor.pending_inputs().is_empty());
    assert_eq!(*actor.0.input_sequence.lock().unwrap(), 2);
}
#[test]
fn adopted_real_helper_reclaims_prior_acknowledgements_before_more_than_capacity_commands() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    const CHILD: &str = "CAGENTS_ACTOR_RECEIPT_RETIREMENT_TEST";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let test = thread::current()
            .name()
            .expect("test harness name")
            .to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("HOME", root.path().join("home"))
            .env("CAGENTS_LOCAL_DEBUG", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // Each wait below is for a real event; under load the helper's RPCs
    // and journal writes can take seconds, so the bound only turns a
    // wedged actor into a failure instead of a hung test.
    const HANG_GUARD: Duration = Duration::from_secs(120);
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    std::fs::create_dir(root.path().join("claude")).unwrap();
    let mut source = fixture_source();
    source.launch.provider = crate::agent::Provider::Claude;
    source.launch.session_id = None;
    source.launch.recipe = crate::model::LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: Vec::new(),
        working_directory: root.path().into(),
        environment: BTreeMap::from([
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                root.path().join("claude").to_string_lossy().into_owned(),
            ),
        ]),
        unset_environment: Default::default(),
    };
    source.owned_codex_listener = None;
    source.terminal = None;
    source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    source.proof.as_mut().unwrap().boot_id = crate::daemon::read_boot_id().unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, source.proof.as_ref().unwrap()).unwrap();
    let script = r#"while IFS= read -r frame; do
  case "$frame" in
    *'"type":"control_request"'*)
      id=${frame#*'"request_id":"'}; id=${id%%'"'*}
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      ;;
    *'"type":"user"'*) printf 'unexpected prompt\n' >> prompts ;;
  esac
done"#;
    let prepared = chat::protocol::create(&source.launch)
        .unwrap()
        .1
        .prepared_launch_identity()
        .unwrap();
    let mut spec = ProcessSpec::from(&source.launch.recipe);
    spec.arguments = vec!["-c".into(), script.into()];
    let actor =
        RemoteActor::spawn(spec, source.clone(), prepared, BTreeMap::new(), |_| {}).unwrap();
    struct Owner(RemoteActor);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.close();
            let _ = self.0.finish_close();
        }
    }
    let owner = Owner(actor);
    let actor = &owner.0;
    actor.commit_installation().unwrap();
    let deadline = Instant::now() + HANG_GUARD;
    while !actor.state().ready {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    // Suspend only this test's metadata pump before admitting a command.
    // Preserve itself fences new helper input, so manufacturing a command
    // after preserve would test a forbidden ordering rather than a lost ACK.
    actor.0.stop.store(true, Ordering::Release);
    // An idle pump waits for readiness without a timer. Wake this exact
    // observer before joining it, as production preservation does.
    actor.0.wake.notify();
    let pump = actor.0.worker.lock().unwrap().take();
    pump.expect("metadata pump").join().unwrap();
    let identity = InputIdentity {
        sequence: 1,
        id: uuid::Uuid::new_v4(),
        hash: payload_digest(
            &serde_json::to_vec(
                &serde_json::to_value(InputPayload::Command {
                    run: None,
                    command: Command::Refresh,
                    decline_only: false,
                })
                .unwrap(),
            )
            .unwrap(),
        ),
    };
    let payload = serde_json::to_value(InputPayload::Command {
        run: None,
        command: Command::Refresh,
        decline_only: false,
    })
    .unwrap();
    let _: InputReceipt = actor.call("input", json!([identity, payload])).unwrap();
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let receipt: Option<InputReceipt> = actor.call("input_receipt", json!([identity])).unwrap();
        if receipt.is_some_and(|receipt| matches!(receipt.state, InputState::Acknowledged(_))) {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let orphaned: Vec<InputReceipt> = actor.call("acknowledged_inputs", Value::Null).unwrap();
    assert_eq!(orphaned.len(), 1);
    assert_eq!(orphaned[0].identity, identity);
    fixture_preserve_and_join(actor);
    let record = actor.identity().record;
    drop(_lease);
    let mut proof = source.proof.clone().unwrap();
    proof.host_incarnation = crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string());
    let _replacement_lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    // The private v1 helper contract is incompatible with this actor
    // generation. Refusal cannot adopt, restart, or dispose the owner.
    let mut legacy_record = record.clone();
    legacy_record
        .build
        .as_mut()
        .unwrap()
        .capabilities
        .retain(|capability| capability != CAPABILITY);
    legacy_record
        .build
        .as_mut()
        .unwrap()
        .capabilities
        .push("claude-actor-v1".into());
    assert!(!RemoteActor::is_record(&legacy_record));
    assert!(RemoteActor::adopt(&legacy_record, source.clone(), proof.clone(), |_| {}).is_err());
    assert!(record.root.is_current());
    let mut legacy_source = source.clone();
    legacy_source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 1 };
    assert!(RemoteActor::adopt(&record, legacy_source, proof.clone(), |_| {}).is_err());
    assert!(
        record.root.is_current(),
        "mixed-version refusal preserves the exact provider"
    );
    // A new daemon cannot silently rebind the retained provider's account
    // after the same pathname resolves to a different physical directory.
    let original_home = root.path().join("claude");
    let retained_home = root.path().join("retained-claude");
    std::fs::rename(&original_home, &retained_home).unwrap();
    std::fs::create_dir(&original_home).unwrap();
    let rejection = RemoteActor::adopt(&record, source.clone(), proof.clone(), |_| {})
        .err()
        .expect("replaced physical account home must refuse adoption");
    assert!(rejection.contains("replaced"));
    assert!(
        record.root.is_current(),
        "refusal preserves the original provider"
    );
    std::fs::remove_dir(&original_home).unwrap();
    std::fs::rename(&retained_home, &original_home).unwrap();
    let replacement = RemoteActor::adopt(&record, source.clone(), proof.clone(), |_| {}).unwrap();
    let replacement_owner = Owner(replacement);
    let actor = &replacement_owner.0;
    let deadline = Instant::now() + HANG_GUARD;
    while actor.0.recovering_inputs.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let reclaimed: Vec<InputReceipt> = actor.call("acknowledged_inputs", Value::Null).unwrap();
    assert!(reclaimed.is_empty());
    assert!(
        actor
            .call::<InputReceipt>("input", json!([identity, payload]))
            .is_err(),
        "a retired old-proxy command cannot execute again"
    );
    source.proof = Some(proof);
    source.native_continuity = actor.state().native_continuity.clone();
    actor
        .call::<()>(
            "update_source",
            json!([
                source,
                Option::<crate::model::LaunchRecipe>::None,
                Option::<crate::terminal::holder::Record>::None
            ]),
        )
        .unwrap();
    let mut total = 0usize;
    while total < journal::MAX_INPUTS + 33 {
        let count = 32.min(journal::MAX_INPUTS + 33 - total);
        let mut replies: Vec<crossbeam_channel::Receiver<_>> = Vec::new();
        while replies.len() < count {
            match actor.enqueue(None, Command::Refresh) {
                Ok(reply) => replies.push(reply),
                // A helper slower than this caller's whole wait leaves
                // the newest input unconfirmed. The metadata pump
                // settles it; then this command is admitted again.
                Err(error) if error == UNSETTLED_INPUT => {
                    let deadline = Instant::now() + HANG_GUARD;
                    while actor.0.uncertain_input.lock().unwrap().is_some() {
                        assert!(Instant::now() < deadline, "{error}");
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(error) => panic!("{error}"),
            }
        }
        for reply in replies {
            match reply.recv_timeout(HANG_GUARD).unwrap() {
                // Settled as never sent after its caller stopped waiting.
                Err(Rejection::Refused(reason)) if reason == UNCONFIRMED_INPUT => {}
                outcome => outcome.unwrap(),
            }
        }
        total += count;
    }
    let deadline = Instant::now() + HANG_GUARD;
    while !actor.0.retiring_inputs.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(actor.pending_inputs().is_empty());
    assert_eq!(*actor.0.input_sequence.lock().unwrap(), total as u64 + 2);
    assert!(
        !root.path().join("prompts").exists(),
        "receipt retirement must never enqueue a provider prompt"
    );
    actor.close().unwrap();
    actor.finish_close().unwrap();
}

#[test]
fn submitted_input_preserves_refusal_and_uncertain_completion_types() {
    use super::super::super::handle::RetainedInputOutcome;
    for outcome in [
        RetainedInputOutcome::Acknowledged(Err("explicit refusal".into())),
        RetainedInputOutcome::Unknown("provider effect is unknown".into()),
        RetainedInputOutcome::Unavailable,
        RetainedInputOutcome::Disconnected,
    ] {
        let refused = matches!(&outcome, RetainedInputOutcome::Acknowledged(Err(_)));
        let actor = RemoteActor::fixture();
        let submitter = actor.clone();
        let delivery = thread::spawn(move || submitter.submit(None, Command::Refresh));
        assert_eq!(actor.settle_input_fixture(outcome), 1);
        let result = delivery.join().unwrap();
        assert!(if refused {
            matches!(result, Err(Rejection::Refused(_)))
        } else {
            matches!(result, Err(Rejection::Unacknowledged(_)))
        });
        assert_eq!(*actor.0.input_sequence.lock().unwrap(), 2);
    }
}

#[test]
fn lost_retirement_reply_keeps_bounded_ack_metadata_and_never_resends_input() {
    let actor = RemoteActor::fixture();
    let response = actor.enqueue(None, Command::Refresh).unwrap();
    let identity = actor.pending_inputs().pop().unwrap();
    actor.apply_receipts(&[InputReceipt {
        identity: identity.clone(),
        state: InputState::Acknowledged(Ok(())),
    }]);
    assert_eq!(response.try_recv(), Ok(Ok(())));
    assert!(actor.pending_inputs().is_empty());
    assert_eq!(
        *actor.0.retiring_inputs.lock().unwrap(),
        vec![identity.clone()]
    );
    assert!(actor.flush_input_retirements().is_err());
    assert_eq!(
        *actor.0.retiring_inputs.lock().unwrap(),
        vec![identity.clone()]
    );
    assert!(
        actor.pending_inputs().is_empty(),
        "a failed metadata ACK never restores a provider envelope"
    );
    // Accepted input keeps its waiter until a terminal outcome arrives.
    let response = actor.enqueue(None, Command::Refresh).unwrap();
    let other = actor.pending_inputs().pop().unwrap();
    actor.apply_receipts(&[InputReceipt {
        identity: other.clone(),
        state: InputState::Accepted,
    }]);
    assert!(response.try_recv().is_err());
    assert_eq!(actor.pending_inputs(), vec![other.clone()]);
    actor.apply_receipts(&[InputReceipt {
        identity: other.clone(),
        state: InputState::Unknown("uncertain".into()),
    }]);
    assert_eq!(
        response.try_recv(),
        Ok(Err(Rejection::Unacknowledged("uncertain".into())))
    );
    assert_eq!(
        *actor.0.retiring_inputs.lock().unwrap(),
        vec![identity, other]
    );
    assert!(actor.pending_inputs().is_empty());
}

#[test]
fn unavailable_proxy_settles_pending_input_and_stops_reporting_healthy_metadata() {
    let actor = RemoteActor::fixture();
    let response = actor.enqueue(None, Command::Interrupt).unwrap();
    actor.publish_unavailable("Retained actor attachment was superseded");
    let state = actor.state();
    assert!(!state.ready);
    assert!(!state.idle);
    assert!(state.failure.is_some());
    assert_eq!(state.activity, crate::agent::ActivityState::Unknown);
    assert!(
        state.owns_work,
        "disconnection is not provider cleanup proof"
    );
    assert!(matches!(
        response.try_recv().unwrap(),
        Err(Rejection::Unacknowledged(error)) if error == STOPPED_INPUT
    ));
}
#[test]
fn connection_loss_requires_death_or_positive_authority_loss_evidence() {
    assert!(terminal_endpoint_error("connection reset", false));
    assert!(terminal_endpoint_error(
        "Retained actor attachment was superseded; stale input and ACKs are refused",
        true
    ));
    for detail in [
        "daemon lease does not match its discovery proof",
        "daemon ownership is not held",
    ] {
        let error = authority_result::<(), _>(Err(std::io::Error::other(detail))).unwrap_err();
        assert!(error.contains(detail));
        assert!(terminal_endpoint_error(&error, true));
    }
    assert!(!terminal_endpoint_error(
        "Resource temporarily unavailable (os error 11)",
        true
    ));
    assert!(!terminal_endpoint_error("timed out", true));
}
