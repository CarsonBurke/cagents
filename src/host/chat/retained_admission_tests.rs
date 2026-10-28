//! Admission regressions through the public host startup path. Every helper,
//! provider and discovery endpoint belongs to this test's isolated subprocess.
use super::*;
use crate::daemon::holder_registry::{self as holders, Kind};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn isolated(name: &str) -> Option<PathBuf> {
    const MODE: &str = "CAGENTS_RETAINED_ADMISSION_TEST";
    if std::env::var(MODE).as_deref() == Ok(name) {
        return Some(PathBuf::from(
            std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap(),
        ));
    }
    let named_resume = name.starts_with("named_claude_resume_");
    let directory = if named_resume {
        tempfile::Builder::new()
            .prefix("updater1513-")
            .tempdir_in("/tmp")
            .unwrap()
    } else {
        tempfile::tempdir_in("/var/tmp").unwrap()
    };
    let root = directory.path();
    for child in [
        "home/.claude",
        "run",
        "data",
        "config",
        "cache",
        "bin",
        "project",
    ] {
        fs::create_dir_all(root.join(child)).unwrap();
    }
    let claude_home = if named_resume {
        for child in ["account-alpha", "account-beta", "project"] {
            let path = root.join("fixture-inputs/workload").join(child);
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        root.join("fixture-inputs/workload/account-alpha")
    } else {
        root.join("home/.claude")
    };
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            &format!("host::chat::retained_admission_tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env(MODE, name)
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_HOLDER_ROOT", root)
        .env("HOME", root.join("home"))
        .env("CLAUDE_CONFIG_DIR", claude_home)
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        );
    if let Some(helper) = std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER") {
        command.env("CAGENTS_TEST_RETAINED_ACTOR_HELPER", helper);
    }
    let stdout = root.join("test.stdout");
    let stderr = root.join("test.stderr");
    command
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Failed assertions or a hung child must not retain these private helpers.
    cleanup_discoveries(&root.join("run"));
    assert!(
        status.is_some_and(|status| status.success()),
        "isolated admission fixture failed or exceeded 90 seconds:\n{}\n{}",
        fs::read_to_string(stdout).unwrap(),
        fs::read_to_string(stderr).unwrap()
    );
    None
}

fn cleanup_owner(owner: &holders::Record) {
    for process in [&owner.root, &owner.holder] {
        if process.pid != std::process::id() as i32 && process.is_current() {
            unsafe {
                libc::kill(process.pid, libc::SIGKILL);
            }
        }
    }
}

fn cleanup_discoveries(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            cleanup_discoveries(&entry.path());
        } else if kind.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            if let Ok(bytes) = fs::read(entry.path())
                && let Ok(owner) = serde_json::from_slice::<holders::Record>(&bytes)
            {
                cleanup_owner(&owner);
            }
        }
    }
}

fn setup(root: &Path, label: &str) -> (SharedHost, HostSessionProof) {
    let proof = HostSessionProof {
        host_session_id: label.into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: "current".into(),
    };
    let mut state = HostState::new(proof.clone());
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    (Arc::new(HostLock::new(state)), proof)
}

fn start(host: &SharedHost, session: Session) -> Result<ServerResponse, ProtocolError> {
    let context = Arc::new(
        chat::protocol::ExecutionContext::capture(&session.launch.recipe.working_directory)
            .unwrap(),
    );
    start_with_execution(
        host,
        0,
        session,
        None,
        false,
        None,
        Default::default(),
        Some(context),
        None,
    )
}

static FAILED_CONFIRMATION: std::sync::OnceLock<(
    std::sync::mpsc::SyncSender<TerminalId>,
    std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
)> = std::sync::OnceLock::new();

pub(super) fn pause_failed_confirmation(id: TerminalId) {
    if let Some((ready, resume)) = FAILED_CONFIRMATION.get() {
        ready.send(id).unwrap();
        resume
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(15))
            .unwrap();
    }
}

#[test]
fn named_claude_resume_failure_preserves_replacement_attachment() {
    named_resume_case(
        "named_claude_resume_failure_preserves_replacement_attachment",
        false,
        false,
    );
}

#[test]
fn named_claude_resume_failure_retires_before_exact_retry() {
    named_resume_case(
        "named_claude_resume_failure_retires_before_exact_retry",
        false,
        false,
    );
}

#[test]
fn named_claude_resume_retry_finishes_failed_retirement_write() {
    named_resume_case(
        "named_claude_resume_retry_finishes_failed_retirement_write",
        true,
        false,
    );
}

#[test]
fn named_claude_resume_normal_close_finishes_failed_retirement_write() {
    named_resume_case(
        "named_claude_resume_normal_close_finishes_failed_retirement_write",
        true,
        true,
    );
}

fn named_resume_case(name: &str, fail_retirement: bool, finish_with_close: bool) {
    let Some(root) = isolated(name) else { return };
    let provider = root.join("bin/claude");
    fs::write(
        &provider,
        include_str!("../../../tests/fixtures/claude-upgrade-continuity.py"),
    )
    .unwrap();
    fs::set_permissions(provider, fs::Permissions::from_mode(0o700)).unwrap();
    let project = root.join("fixture-inputs/workload/project");
    let (host, proof) = setup(&root, name);
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let _wire = HostClientWire::start(&host, &proof);
    let initial = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        project.clone(),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let initial_id = initial.id;
    start(&host, initial).unwrap();
    let handle = host.lock().unwrap().chats[&initial_id].handle.clone();
    let wait = |stage: &str, condition: &mut dyn FnMut() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            if Instant::now() >= deadline {
                let resident = handle.attachment_snapshot();
                let snapshot_error = handle.snapshot(None).and_then(|snapshot| snapshot.error);
                panic!(
                    "named-resume fixture did not settle at {stage}: ready={} exited={} busy={} history_loading={} failure={:?} snapshot_error={snapshot_error:?} provider_done={} trace={}",
                    resident.ready,
                    resident.exited,
                    resident.busy,
                    resident.history_loading,
                    handle.failure(),
                    root.join("provider-done").exists(),
                    fs::read_to_string(root.join("provider-trace.jsonl")).unwrap_or_default(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    };
    wait("initial SDK initialization", &mut || {
        handle.attachment_snapshot().ready
    });
    handle
        .command(chat::Command::Prompt {
            text: "LIVE_WORK_1513".into(),
            attachments: vec![],
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait("initial prompt result and idle", &mut || {
        root.join("provider-done").exists() && handle.is_idle()
    });
    let ServerResponse::ChatListed { sessions } =
        process(&host, 0, ClientRequest::ChatList).unwrap()
    else {
        panic!("expected the actual hosted conversation");
    };
    let original = sessions
        .into_iter()
        .find(|session| session.id == initial_id)
        .unwrap();
    let thread = original.launch.session_id.as_ref().unwrap();
    let project_key = project
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let history = root
        .join("fixture-inputs/workload/account-alpha/projects")
        .join(project_key)
        .join(format!("{thread}.jsonl"));
    let history_before = fs::read(&history).unwrap();
    process(
        &host,
        0,
        ClientRequest::ChatClose {
            chat_id: initial_id,
        },
    )
    .unwrap();
    let ServerResponse::ChatResumePrepared { session: requested } = process(
        &host,
        0,
        ClientRequest::ChatPrepareResume {
            source: original.clone(),
        },
    )
    .unwrap() else {
        panic!("expected authenticated resume preparation")
    };
    assert_eq!(requested.launch, original.launch);
    assert_ne!(requested.id, original.id);
    assert_eq!(requested.proof, None);

    let fault = rusqlite::Connection::open(root.join("registry.sqlite3")).unwrap();
    if fail_retirement {
        fault.execute_batch("CREATE TRIGGER fail_resume_retirement BEFORE DELETE ON agent_authorities BEGIN SELECT RAISE(ABORT, 'fixture resume retirement refusal'); END;").unwrap();
    }
    let connect = |intent| {
        crate::chat::client::ClientHandle::connect(
            requested.clone(),
            intent,
            None,
            false,
            None,
            |_| {},
        )
    };
    let replaced = name == "named_claude_resume_failure_preserves_replacement_attachment";
    let first = if replaced {
        let (ready, paused) = std::sync::mpsc::sync_channel(1);
        let (resume, released) = std::sync::mpsc::sync_channel(1);
        FAILED_CONFIRMATION
            .set((ready, std::sync::Mutex::new(released)))
            .unwrap();
        thread::scope(|scope| {
            let starting = scope.spawn(|| connect(chat::Intent::Start));
            assert_eq!(
                paused.recv_timeout(Duration::from_secs(15)).unwrap(),
                requested.id
            );
            let (source, old, instance) = {
                let state = host.lock().unwrap();
                let record = &state.chats[&requested.id];
                (
                    record.session.clone(),
                    record.handle.clone(),
                    record.execution_instance,
                )
            };
            let owner = old.actor_identity().unwrap().record;
            let weak = Arc::downgrade(&host);
            let id = requested.id;
            let replacement = Handle::adopt_remote(&owner, source, proof.clone(), move |update| {
                changed_owned(&weak, id, instance, update);
            })
            .unwrap();
            assert!(old.same_actor(&replacement));
            assert!(!old.same_attachment(&replacement));
            {
                let mut state = host.lock().unwrap();
                let record = state.chats.get_mut(&requested.id).unwrap();
                assert!(record.handle.same_attachment(&old));
                record.handle = replacement.clone();
            }
            resume.send(()).unwrap();
            let result = starting.join().unwrap();
            assert!(
                host.lock().unwrap().chats[&requested.id]
                    .handle
                    .same_attachment(&replacement)
            );
            assert!(host.lock().unwrap().native_actor_startups.is_empty());
            assert!(
                super::super::agent_execution::registry(&host)
                    .unwrap()
                    .lock()
                    .unwrap()
                    .retained_agent_authority_source(requested.id)
                    .unwrap()
                    .is_some()
            );
            assert!(
                owner.holder.is_current(),
                "replacement helper must survive stale cleanup"
            );
            // The failed request cannot retire this new authenticated proxy.
            // Its ordinary Close remains available and finishes real teardown.
            process(
                &host,
                0,
                ClientRequest::ChatClose {
                    chat_id: requested.id,
                },
            )
            .unwrap();
            result
        })
    } else {
        connect(chat::Intent::Start)
    };
    let failure = match first {
        Err(failure) => failure,
        Ok(handle) => {
            let _ = handle.close();
            panic!("unconfirmed initialization must not become a successful attachment");
        }
    };
    if replaced {
        assert!(
            failure.message.contains("changed its owner"),
            "{}",
            failure.message
        );
        assert!(
            fs::read_to_string(root.join("provider-trace.jsonl"))
                .unwrap()
                .contains("owned-fixture-resume-refused-once")
        );
    } else {
        assert!(failure.message.contains("75"), "{}", failure.message);
    }
    let registry = super::super::agent_execution::registry(&host).unwrap();
    if fail_retirement {
        assert!(
            failure
                .message
                .contains("fixture resume retirement refusal"),
            "{}",
            failure.message
        );
        assert!(
            host.lock().unwrap().native_actor_startups[&requested.id]
                .failed_close
                .is_some()
        );
        assert!(host.lock().unwrap().chats.contains_key(&requested.id));
        fault
            .execute_batch("DROP TRIGGER fail_resume_retirement")
            .unwrap();
    } else {
        assert!(!host.lock().unwrap().chats.contains_key(&requested.id));
        assert!(host.lock().unwrap().native_actor_startups.is_empty());
        assert!(
            registry
                .lock()
                .unwrap()
                .retained_agent_authority_source(requested.id)
                .unwrap()
                .is_none()
        );
    }
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    assert_eq!(fs::read(&history).unwrap(), history_before);
    if finish_with_close {
        process(
            &host,
            0,
            ClientRequest::ChatClose {
                chat_id: requested.id,
            },
        )
        .unwrap();
        assert!(!host.lock().unwrap().chats.contains_key(&requested.id));
        assert!(host.lock().unwrap().native_actor_startups.is_empty());
        assert!(
            registry
                .lock()
                .unwrap()
                .retained_agent_authority_source(requested.id)
                .unwrap()
                .is_none()
        );
    }

    if !fail_retirement {
        // The real never-confirmed actor has a valid named issued descriptor
        // and completed retirement. It is obsolete metadata, not authority to
        // resume this history. Exercise the both-None confirmation case before
        // any successful successor exists.
        let failed = registry
            .lock()
            .unwrap()
            .native_actor_ownerships(None, 128)
            .unwrap()
            .into_iter()
            .filter(|owner| owner.source().id == requested.id)
            .collect::<Vec<_>>();
        assert_eq!(failed.len(), 1);
        let retired = registry
            .lock()
            .unwrap()
            .native_actor_retirement(failed[0].actor())
            .unwrap()
            .unwrap();
        assert_eq!(retired.confirmed_session(), None);
        let source = retired.source().clone();
        assert!(source.has_valid_native_continuity());
        assert_eq!(source.launch.session_id, requested.launch.session_id);
        let trace_before = fs::read(root.join("provider-trace.jsonl")).unwrap();
        assert!(process(&host, 0, ClientRequest::ChatPrepareResume { source }).is_err());
        assert_eq!(
            fs::read(root.join("provider-trace.jsonl")).unwrap(),
            trace_before
        );
        assert_eq!(fs::read(&history).unwrap(), history_before);
        assert!(host.lock().unwrap().chats.is_empty());
        assert!(host.lock().unwrap().native_actor_startups.is_empty());
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .native_actor_ownerships(None, 128)
                .unwrap()
                .len(),
            2
        );
    }

    // The exact original request is retried only after the real failed owner
    // finishes normal Close, including a previously failed retirement write.
    let client = connect(chat::Intent::RetryStart).unwrap();
    let resumed = client.session();
    // The adapter does not broadcast daemon change notifications. Wait for
    // normal history loading, then exercise the real Client visibility/read
    // path rather than asserting a potentially still-loading initial body.
    wait("resumed HOST history", &mut || {
        !host.lock().unwrap().chats[&requested.id]
            .handle
            .attachment_snapshot()
            .history_loading
    });
    client.set_visible(true).unwrap();
    wait("resumed Client transcript", &mut || {
        client
            .snapshot()
            .items
            .iter()
            .filter(|item| item.role == chat::Role::User)
            .count()
            == 1
            && !client.snapshot().history_loading
    });
    let snapshot = client.snapshot();
    assert_eq!(
        resumed.id, requested.id,
        "keep the daemon-issued allocation"
    );
    assert_eq!(resumed.launch, requested.launch);
    assert!(snapshot.ready && !snapshot.exited);
    assert_eq!(snapshot.session_id, requested.launch.session_id);
    assert_eq!(
        snapshot
            .items
            .iter()
            .filter(|item| item.role == chat::Role::User)
            .count(),
        1
    );
    for index in 0..40 {
        assert_eq!(snapshot.items.iter().filter(|item| {
            matches!(&item.content, chat::ItemContent::Text { text, .. } if text == &format!("FRAME_{index:03}|"))
        }).count(), 1);
    }
    assert_eq!(
        fs::read_to_string(root.join("prompts")).unwrap(),
        "prompt\n"
    );
    assert_eq!(fs::read(&history).unwrap(), history_before);
    assert!(host.lock().unwrap().native_actor_startups.is_empty());
    let failed = registry
        .lock()
        .unwrap()
        .native_actor_ownerships(None, 128)
        .unwrap()
        .into_iter()
        .filter(|owner| owner.source().id == requested.id)
        .collect::<Vec<_>>();
    assert_eq!(
        failed.len(),
        2,
        "retain failed and successful actor metadata"
    );
    for owner in &failed {
        if registry
            .lock()
            .unwrap()
            .native_actor_retirement(owner.actor())
            .unwrap()
            .is_some()
        {
            assert!(!owner.owner().root.is_current() && !owner.owner().holder.is_current());
        }
    }
    client.close().unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .retained_agent_authority_source(resumed.id)
            .unwrap()
            .is_none()
    );
    // The obsolete unconfirmed actor is not history authority and does not
    // veto the genuinely confirmed, normally retired successor's next resume.
    let ServerResponse::ChatResumePrepared { session: next } = process(
        &host,
        0,
        ClientRequest::ChatPrepareResume {
            source: resumed.clone(),
        },
    )
    .unwrap() else {
        panic!("expected exact resume after the completed retry")
    };
    assert_ne!(next.id, resumed.id);
    assert_eq!(next.launch, resumed.launch);
    assert_eq!(next.proof, None);
    assert_eq!(fs::read(&history).unwrap(), history_before);
    assert!(host.lock().unwrap().chats.is_empty());
}

/// Framed native Client calls use the actual HOST handlers, lease and private
/// execed helper. This test adapter supplies only the Connect envelope; it does
/// not fabricate start/attach, provider exit, semantic ACK or retirement data.
struct HostClientWire {
    stop: Arc<std::sync::atomic::AtomicBool>,
    streams: Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HostClientWire {
    fn start(host: &SharedHost, proof: &HostSessionProof) -> Self {
        use crate::terminal_ipc::{
            ClientEnvelope, ServerEnvelope, ServerMessage, read_frame, write_frame,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let streams = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stopped = stop.clone();
        let connections = streams.clone();
        let host = host.clone();
        let proof = proof.clone();
        let worker = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("native client fixture accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                connections
                    .lock()
                    .unwrap()
                    .push(stream.try_clone().unwrap());
                let stopped = stopped.clone();
                let host = host.clone();
                let proof = proof.clone();
                workers.push(thread::spawn(move || {
                    while !stopped.load(Ordering::Acquire) {
                        let envelope: ClientEnvelope = match read_frame(&mut stream) {
                            Ok(Some(envelope)) => envelope,
                            Ok(None) => break,
                            Err(_) if stopped.load(Ordering::Acquire) => break,
                            Err(crate::terminal_ipc::FrameError::Io(error))
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(error) => panic!("native client fixture request: {error}"),
                        };
                        assert_eq!(envelope.version, PROTOCOL_VERSION);
                        let result = match envelope.request {
                            ClientRequest::Connect {
                                expected_host,
                                resume,
                            } => {
                                assert!(
                                    expected_host.is_none()
                                        || expected_host.as_ref() == Some(&proof)
                                );
                                assert!(resume.is_empty());
                                Ok(ServerResponse::Connected {
                                    proof: proof.clone(),
                                    resumed: vec![],
                                })
                            }
                            request => {
                                assert_eq!(envelope.proof.as_ref(), Some(&proof));
                                process(&host, 0, request)
                            }
                        };
                        let message = match result {
                            Ok(response) => ServerMessage::Response { response },
                            Err(error) => ServerMessage::Error(error),
                        };
                        if write_frame(
                            &mut stream,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(envelope.request_id),
                                proof: proof.clone(),
                                message,
                            },
                        )
                        .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            stop,
            streams,
            worker: Some(worker),
        }
    }
}

impl Drop for HostClientWire {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        for stream in self.streams.lock().unwrap().iter() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn unsupported_live_owner_fences_reconnect_only_original_alias_before_rpc() {
    let Some(root) =
        isolated("unsupported_live_owner_fences_reconnect_only_original_alias_before_rpc")
    else {
        return;
    };
    let provider = root.join("bin/claude");
    fs::write(&provider, "#!/bin/sh\nprintf spawned > \"$0.spawned\"\n").unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let (host, proof) = setup(&root, "unsupported-admission");
    let mut requested = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    requested.proof = Some(proof.clone());
    requested.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let directory = holders::directory(Kind::Native).unwrap();
    crate::secure_file::prepare_private_directory(&directory).unwrap();
    let socket = holders::socket(&directory, requested.id);
    let listener = holders::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let process = holders::Process::read(std::process::id() as i32).unwrap();
    let mut build = holders::BuildIdentity::current(Kind::Native);
    build.capabilities.push("claude-actor-v1".into());
    let owner = holders::Record {
        version: 4,
        build: Some(build),
        kind: Kind::Native,
        application: crate::instance::APPLICATION_ID.into(),
        boot: proof.boot_id,
        terminal: requested.id,
        holder: process.clone(),
        root: process,
    };
    owner.create(&directory).unwrap();
    assert!(!Handle::is_remote_record(&owner));
    let failure = start(&host, requested.clone()).unwrap_err();
    assert!(
        failure.message.contains("unsupported private protocol"),
        "{}",
        failure.message
    );
    assert!(
        !provider.with_extension("spawned").exists(),
        "discovery must fence recipe execution"
    );
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "unsupported protocol must refuse before RPC"
    );
    assert_eq!(
        holders::recovery_candidate(&socket, Kind::Native).unwrap(),
        Some(owner.clone())
    );
    assert!(owner.holder.is_current() && owner.root.is_current());
    let state = host.lock().unwrap();
    assert!(
        state.chat_spawning.is_empty()
            && state.chats.is_empty()
            && state.native_actor_startups.is_empty()
    );
}

#[test]
fn installed_startup_with_uncommitted_attention_adopts_before_cleanup_retry() {
    let Some(root) =
        isolated("installed_startup_with_uncommitted_attention_adopts_before_cleanup_retry")
    else {
        return;
    };
    let _helper = super::retained_actor_tests::retained_helper_executable();
    let provider = root.join("bin/claude");
    super::retained_actor_tests::install_provider(&provider);
    let original = fs::read_to_string(&provider).unwrap();
    let initialize = "with (root/'spawns').open('a') as log: log.write('spawn\\n')\nsend({'type':'result','uuid':'startup-attention','subtype':'success','is_error':False,'session_id':session})\nfor line in sys.stdin:";
    assert!(original.contains("for line in sys.stdin:"));
    fs::write(
        &provider,
        original.replacen("for line in sys.stdin:", initialize, 1),
    )
    .unwrap();
    let (host, proof) = setup(&root, "post-install-admission");
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let mut requested = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    requested.proof = Some(proof.clone());
    let fault = rusqlite::Connection::open(root.join("registry.sqlite3")).unwrap();
    fault.execute_batch("CREATE TRIGGER fail_initial_actor_ownership BEFORE INSERT ON native_actor_ownerships BEGIN SELECT RAISE(ABORT, 'fixture initial ownership failure'); END;").unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            cleanup_discoveries(&self.0);
        }
    }
    let _cleanup = Cleanup(root.join("run"));
    let failure = start(&host, requested.clone()).unwrap_err();
    assert!(
        failure
            .message
            .contains("fixture initial ownership failure"),
        "{}",
        failure.message
    );
    let pending = host.lock().unwrap().native_actor_startups[&requested.id].clone();
    let directory = holders::directory(Kind::Native).unwrap();
    let owner =
        holders::recovery_candidate(&holders::socket(&directory, requested.id), Kind::Native)
            .unwrap()
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let before = loop {
        let inspection = Handle::inspect_remote(&owner, proof.clone()).unwrap();
        if inspection.state.outbox_latest > inspection.state.outbox_ack {
            break inspection;
        }
        assert!(
            Instant::now() < deadline,
            "installed helper never retained startup attention"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(before.state.provider_process.as_ref().unwrap().is_current());
    assert!(host.lock().unwrap().chats.is_empty());
    fault
        .execute_batch("DROP TRIGGER fail_initial_actor_ownership;")
        .unwrap();
    let response = start(&host, requested.clone()).unwrap();
    let ServerResponse::ChatAttached { session, .. } = response else {
        panic!("retry did not attach");
    };
    assert_eq!(session.id, requested.id);
    let handle = host.lock().unwrap().chats[&requested.id].handle.clone();
    assert_eq!(handle.actor_identity().unwrap(), before.identity);
    assert_eq!(handle.provider_process(), before.state.provider_process);
    assert_eq!(
        fs::read_to_string(root.join("spawns"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(
        !root.join("prompts").exists(),
        "recovery must not invent or replay a prompt"
    );
    assert!(host.lock().unwrap().native_actor_startups.is_empty());
    assert_eq!(pending.requested, requested);
    assert_eq!(pending.source.id, requested.id);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspection = Handle::inspect_remote(&owner, proof.clone()).unwrap();
        if inspection.state.outbox_ack >= before.state.outbox_latest {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "adopted helper attention was not committed and acknowledged"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let registry = super::super::agent_execution::registry(&host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_cursor_for_test(before.identity.instance)
            .unwrap()
            .is_some()
    );
    super::native_cleanup::close_retained(&host, &session).unwrap();
    assert!(!owner.root.is_current() && !owner.holder.is_current());
}
