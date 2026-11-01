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
    // SDK fixtures and private helper authority require a private root;
    // tempfile's directory mode can inherit a broader caller policy.
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
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
    let wait = |stage: &str, observed: &Handle, condition: &mut dyn FnMut() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            if Instant::now() >= deadline {
                let resident = observed.attachment_snapshot();
                let snapshot = observed.snapshot(None);
                let history_loading = snapshot.as_ref().map(|snapshot| snapshot.history_loading);
                let snapshot_error = snapshot.and_then(|snapshot| snapshot.error);
                panic!(
                    "named-resume fixture did not settle at {stage}: ready={} exited={} busy={} history_loading={history_loading:?} failure={:?} snapshot_error={snapshot_error:?} provider_done={} trace={}",
                    resident.ready,
                    resident.exited,
                    resident.busy,
                    observed.failure(),
                    root.join("provider-done").exists(),
                    fs::read_to_string(root.join("provider-trace.jsonl")).unwrap_or_default(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    };
    wait("initial SDK initialization", &handle, &mut || {
        handle.attachment_snapshot().ready
    });
    handle
        .command(chat::Command::Prompt {
            text: "LIVE_WORK_1513".into(),
            attachments: vec![],
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait("initial prompt result and idle", &handle, &mut || {
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
    let resumed_handle = host.lock().unwrap().chats[&requested.id].handle.clone();
    wait("resumed HOST history", &resumed_handle, &mut || {
        resumed_handle
            .snapshot(None)
            .is_some_and(|snapshot| !snapshot.history_loading)
    });
    client.set_visible(true).unwrap();
    wait("resumed Client transcript", &resumed_handle, &mut || {
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

/// The whole reboot route through the real helper, host and client. The kill
/// and removed discovery stand in for the reboot; the saved proof carries the
/// changed boot because one kernel cannot host both sides of it. The fixture
/// provider aborts on any second prompt.
#[test]
fn named_claude_resume_after_lost_holder_sends_nothing() {
    lost_holder_case("named_claude_resume_after_lost_holder_sends_nothing", false);
}

#[test]
fn named_claude_resume_of_lost_agent_rebinds_and_sends_nothing() {
    lost_holder_case(
        "named_claude_resume_of_lost_agent_rebinds_and_sends_nothing",
        true,
    );
}

fn lost_holder_case(name: &str, managed: bool) {
    let Some(root) = isolated(name) else { return };
    let provider = root.join("bin/claude");
    fs::write(
        &provider,
        include_str!("../../../tests/fixtures/claude-upgrade-continuity.py"),
    )
    .unwrap();
    fs::set_permissions(provider, fs::Permissions::from_mode(0o700)).unwrap();
    let project = root.join("fixture-inputs/workload/project");
    let wait = |stage: &str, condition: &mut dyn FnMut() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition() {
            assert!(Instant::now() < deadline, "did not settle at {stage}");
            thread::sleep(Duration::from_millis(10));
        }
    };
    let serve = |host: &SharedHost, id: ClientId, source: Session| {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let served = host.clone();
        let worker = thread::spawn(move || super::super::client::serve(server, id, served));
        (
            chat::client::ClientHandle::fixture_restore(client, source),
            worker,
        )
    };
    let users = |items: &[Arc<chat::Item>]| {
        items
            .iter()
            .filter(|item| item.role == chat::Role::User)
            .count()
    };
    let initial = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        project.clone(),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let initial_id = initial.id;
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let original = {
        let (host, proof) = setup(&root, "before-reboot");
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let notifications = super::super::notifications::Broker::start(
            &host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        host.lock().unwrap().notifications = Some(notifications.handle());
        let _wire = HostClientWire::start(&host, &proof);
        start(&host, initial).unwrap();
        let handle = host.lock().unwrap().chats[&initial_id].handle.clone();
        wait("initial SDK", &mut || handle.attachment_snapshot().ready);
        handle
            .command(chat::Command::Prompt {
                text: "LIVE_WORK_1513".into(),
                attachments: vec![],
                mode: chat::PromptMode::Send,
            })
            .unwrap();
        wait("initial prompt", &mut || {
            root.join("provider-done").exists() && handle.is_idle()
        });
        let ServerResponse::ChatListed { sessions } =
            process(&host, 0, ClientRequest::ChatList).unwrap()
        else {
            panic!("expected the hosted conversation");
        };
        let original = sessions
            .into_iter()
            .find(|session| session.id == initial_id)
            .unwrap();
        assert!(original.native_continuity.retains_provider());
        if managed {
            let registry = super::super::agent_execution::registry(&host).unwrap();
            let mut registry = registry.lock().unwrap();
            registry
                .apply(&crate::workspace_model::ModelEdit::OpenProject {
                    repository: project.clone(),
                    name: "Fixture".into(),
                    kind: crate::model::ProjectKind::Folder,
                })
                .unwrap();
            let accepted = registry
                .submit_agent(
                    &crate::agent_execution::Origin::User,
                    &crate::agent_execution::Submit {
                        name: None,
                        destination: crate::agent_execution::Destination::New(
                            crate::agent_execution::Spec {
                                provider: Provider::Claude,
                                account: None,
                                target: crate::agent_execution::Target {
                                    repository: project.clone(),
                                    directory: project.clone(),
                                    placement: None,
                                },
                                surface: crate::agent_execution::Surface::Chat,
                                model: None,
                                permission_mode: None,
                                reasoning_effort: None,
                            },
                        ),
                        prompt: "LIVE_WORK_1513".into(),
                        idempotency_key: "lost-holder-agent".into(),
                    },
                    1,
                )
                .unwrap();
            registry
                .bind_agent(
                    accepted.agent.id,
                    None,
                    crate::agent_execution::HostedSession::Chat(original.clone()),
                )
                .unwrap();
        }

        // A live holder still attaches as itself.
        let (attached, worker) = serve(&host, 80, original.clone());
        let attached = attached.unwrap();
        assert_eq!(attached.session(), original);
        assert!(attached.snapshot().read_only_reason.is_none());
        drop(attached);
        worker.join().unwrap();
        // And a live owner is not displaced by a descriptor claiming a changed
        // boot: the reader needs that allocation to be empty.
        let mut claimed = original.clone();
        claimed.proof.as_mut().unwrap().boot_id = "earlier-boot".into();
        let (refused, worker) = serve(&host, 81, claimed);
        assert!(refused.is_err());
        worker.join().unwrap();
        assert!(host.lock().unwrap().chats[&initial_id].handle.owns_work());

        let owner = handle.actor_identity().unwrap().record;
        cleanup_owner(&owner);
        wait("holder exit", &mut || {
            !owner.holder.is_current()
                && (owner.root.pid == std::process::id() as i32 || !owner.root.is_current())
        });
        original
    };
    fs::remove_dir_all(holders::directory(Kind::Native).unwrap()).unwrap();
    let _ = fs::remove_file(crate::terminal::ipc::daemon_socket_path().unwrap());

    let thread = original.launch.session_id.clone().unwrap();
    let project_key = project
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let account = root.join("fixture-inputs/workload/account-alpha");
    let history = account
        .join("projects")
        .join(project_key)
        .join(format!("{thread}.jsonl"));
    let history_before = fs::read(&history).unwrap();
    let prompts_before = fs::read_to_string(root.join("prompts")).unwrap();
    assert_eq!(prompts_before, "prompt\n");
    let provider_runs = || {
        fs::read_to_string(root.join("provider-trace.jsonl"))
            .unwrap()
            .matches("\"bootstrap\"")
            .count()
    };
    let runs_before = provider_runs();
    // The fixture otherwise fails its first resume on purpose.
    let refused_once = root.join("history-retry-start-failed-once.json");
    fs::write(
        &refused_once,
        serde_json::to_string(&serde_json::json!({
            "account": account.canonicalize().unwrap(),
            "thread": thread,
        }))
        .unwrap()
        .replace("\":\"", "\": \"")
        .replace("\",\"", "\", \""),
    )
    .unwrap();
    fs::set_permissions(&refused_once, fs::Permissions::from_mode(0o600)).unwrap();

    let (host, proof) = setup(&root, "after-reboot");
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let _wire = HostClientWire::start(&host, &proof);
    let mut saved = original.clone();
    saved.proof.as_mut().unwrap().boot_id = "earlier-boot".into();
    let reader = saved.lost_holder_history(&proof).unwrap();
    // What a real reboot leaves: every durable row still names the old boot,
    // including the ownership row's owner.
    let fixture = rusqlite::Connection::open(root.join("registry.sqlite3")).unwrap();
    let live_boot = proof.boot_id.to_string();
    let tables: &[(&str, &str)] = if managed {
        &[
            ("agent_authorities", "source"),
            ("native_actor_ownerships", "ownership"),
            ("agents", "record"),
        ]
    } else {
        &[
            ("agent_authorities", "source"),
            ("native_actor_ownerships", "ownership"),
        ]
    };
    for (table, column) in tables {
        let changed = fixture
            .execute(
                &format!(
                    "UPDATE {table} SET {column}=replace({column},?1,'earlier-boot') WHERE instr({column},?1)>0"
                ),
                [&live_boot],
            )
            .unwrap();
        assert!(changed >= 1, "{table} carried no boot to rewrite");
    }
    {
        let registry = super::super::agent_execution::registry(&host).unwrap();
        let owners = registry
            .lock()
            .unwrap()
            .native_actor_ownerships(None, 128)
            .unwrap();
        assert!(owners.iter().any(|owner| owner.source().id == saved.id
            && owner.owner().boot.to_string() == "earlier-boot"));
    }
    let authority_row = || -> (String, String) {
        fixture
            .query_row(
                "SELECT token,source FROM agent_authorities WHERE session_id=?1",
                [saved.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    let authority_before = authority_row();
    let agent_state = || {
        let registry = super::super::agent_execution::registry(&host).unwrap();
        let registry = registry.lock().unwrap();
        registry.agent_for_session(saved.id).unwrap().map(|agent| {
            let run = registry.latest_agent_run(agent.id).unwrap();
            (agent, run)
        })
    };
    if managed {
        // What daemon startup does to a run it cannot prove finished.
        super::super::agent_execution::registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .interrupt_unproven_agent_runs(2)
            .unwrap();
    }
    let lost_agent = agent_state();

    // A home remapped under the recorded launch is another account.
    let account_moved = root.join("fixture-inputs/workload/account-moved");
    let remap = || {
        fs::rename(&account, &account_moved).unwrap();
        fs::create_dir(&account).unwrap();
        fs::set_permissions(&account, fs::Permissions::from_mode(0o700)).unwrap();
    };
    let unmap = || {
        fs::remove_dir_all(&account).unwrap();
        fs::rename(&account_moved, &account).unwrap();
    };
    remap();
    let (remapped, worker) = serve(&host, 83, saved.clone());
    let failure = remapped.err().expect("a remapped home opened a reader");
    assert!(
        failure.message.contains("different physical account home"),
        "{}",
        failure.message
    );
    worker.join().unwrap();
    unmap();
    assert!(host.lock().unwrap().chats.is_empty());
    assert_eq!(authority_row(), authority_before);

    let (restored, worker) = serve(&host, 82, saved.clone());
    let restored = restored.unwrap();
    assert_eq!(restored.session(), reader);
    restored.set_visible(true).unwrap();
    wait("saved transcript", &mut || {
        let snapshot = restored.snapshot();
        !snapshot.history_loading && users(&snapshot.items) == 1
    });
    let snapshot = restored.snapshot();
    assert!(snapshot.read_only_reason.is_some() && snapshot.exited);
    assert!(snapshot.items.iter().any(
        |item| matches!(&item.content, chat::ItemContent::Text { text, .. } if text == "FRAME_000|")
    ));
    assert_eq!(
        provider_runs(),
        runs_before,
        "opening history ran no provider"
    );
    assert_eq!(fs::read(&history).unwrap(), history_before);

    assert_eq!(authority_row(), authority_before, "reading revokes nothing");
    assert_eq!(agent_state(), lost_agent, "reading changes no agent record");
    if let Some((agent, run)) = &lost_agent {
        assert_eq!(run.state, crate::agent_execution::State::Interrupted);
        assert!(
            snapshot
                .read_only_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no confirmed outcome")),
            "{:?}",
            snapshot.read_only_reason
        );
        // A reader is not this agent's provider to address.
        let refused = super::super::agent_execution::process(
            &host,
            Default::default(),
            crate::agent_execution::Operation::Requests(agent.id),
        )
        .unwrap_err();
        assert!(refused.contains("no longer live"), "{refused}");
    }
    assert!(
        !super::super::agent_execution::registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .retained_agent_sources(saved.id)
            .unwrap()
            .is_empty()
    );

    // The same remap between reading and resuming still refuses.
    remap();
    let refused = crate::chat::client::ClientHandle::connect(
        reader.clone(),
        chat::Intent::Start,
        None,
        false,
        None,
        |_| {},
    )
    .err()
    .expect("a remapped home resumed");
    assert!(
        refused.message.contains("different physical account home"),
        "{}",
        refused.message
    );
    unmap();
    assert_eq!(provider_runs(), runs_before);

    // The one explicit action: normal Start on the exact reader descriptor.
    let resumed = crate::chat::client::ClientHandle::connect(
        reader.clone(),
        chat::Intent::Start,
        None,
        false,
        None,
        |_| {},
    )
    .unwrap();
    assert!(
        provider_runs() > runs_before,
        "the action resumed a provider"
    );
    let session = resumed.session();
    assert_eq!(session.id, original.id);
    assert_eq!(session.launch, original.launch);
    assert!(session.native_continuity.retains_provider());
    resumed.set_visible(true).unwrap();
    wait("resumed transcript", &mut || {
        let snapshot = resumed.snapshot();
        snapshot.ready && !snapshot.history_loading && users(&snapshot.items) == 1
    });
    assert_eq!(
        fs::read_to_string(root.join("prompts")).unwrap(),
        prompts_before,
        "no prompt reached the provider"
    );
    assert_eq!(
        fs::read(&history).unwrap(),
        history_before,
        "the provider's user records are unchanged"
    );
    // Start's own fresh capability replaced the dead row.
    assert_ne!(authority_row().0, authority_before.0);
    if let Some((before, run)) = &lost_agent {
        let (agent, after) = agent_state().unwrap();
        assert_eq!(
            agent.session,
            Some(crate::agent_execution::HostedSession::Chat(session.clone())),
            "later prompts reach the provider now running"
        );
        assert_eq!(&after, run, "the interrupted run stays interrupted");
        assert_eq!(agent.id, before.id);
        // The provider is the agent's, and closes as the agent.
        super::super::agent_execution::process(
            &host,
            Default::default(),
            crate::agent_execution::Operation::Requests(agent.id),
        )
        .unwrap();
        let reply = super::super::agent_execution::process(
            &host,
            Default::default(),
            crate::agent_execution::Operation::Close(agent.id),
        )
        .unwrap();
        assert!(
            matches!(reply, crate::agent_execution::Reply::Closed(agent) if agent.closed),
            "the agent closes"
        );
        assert_eq!(
            fs::read_to_string(root.join("prompts")).unwrap(),
            prompts_before
        );
        drop(resumed);
        drop(restored);
        drop(worker);
        return;
    }

    // The kept allocation is not poisoned by the older owner's unreadable row.
    let closing = resumed.session();
    resumed.close().unwrap();
    drop(resumed);
    wait("resumed close", &mut || {
        !host.lock().unwrap().chats.contains_key(&closing.id)
    });
    let ServerResponse::ChatResumePrepared { session: next } = process(
        &host,
        0,
        ClientRequest::ChatPrepareResume {
            source: closing.clone(),
        },
    )
    .unwrap() else {
        panic!("expected exact resume preparation");
    };
    assert_ne!(next.id, closing.id);
    assert_eq!(next.launch, closing.launch);
    super::retained_actor::close_unattached(&host, &closing, None).unwrap();
    assert_eq!(
        fs::read(&history).unwrap(),
        history_before,
        "the provider's user records are still unchanged"
    );
    assert_eq!(
        fs::read_to_string(root.join("prompts")).unwrap(),
        prompts_before
    );
    drop(restored);
    drop(worker);
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
    host.lock().unwrap().native_chat_capacity = 1;
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
    {
        let state = host.lock().unwrap();
        assert!(state.chats.is_empty() && state.chat_spawning.is_empty());
        assert_eq!(capacity::usage(&state), 1);
        assert_eq!(capacity::status_from_state(&state).spawning, 1);
    }
    let mut other = requested.clone();
    other.id = TerminalId::new();
    other.launch.recipe.program = "/missing-provider-never-constructed".into();
    let rejected = start(&host, other).unwrap_err();
    assert_eq!(rejected.code, ProtocolErrorCode::ChatCapacity);

    // Exact recovery must reuse the occupied slot, reach the original holder,
    // and retain custody when the same publication fault still refuses it.
    let failure = start(&host, requested.clone()).unwrap_err();
    assert!(
        failure
            .message
            .contains("fixture initial ownership failure"),
        "{}",
        failure.message
    );
    assert_eq!(capacity::usage(&host.lock().unwrap()), 1);
    assert!(host.lock().unwrap().chat_spawning.is_empty());

    let mut changed = pending.source.clone();
    changed.launch.recipe.program = "/different-exact-source".into();
    let generation = {
        let mut state = host.lock().unwrap();
        state.native_chat_capacity = 2;
        state.inventory_generation
    };
    assert!(
        super::native_recovery::restore(&host, 0, &changed)
            .unwrap_err()
            .contains("retained startup belongs to another exact descriptor")
    );
    {
        let mut state = host.lock().unwrap();
        assert_eq!(state.inventory_generation, generation);
        assert!(state.chat_spawning.is_empty());
        assert!(Arc::ptr_eq(
            &state.native_actor_startups[&requested.id].context,
            &pending.context
        ));
        state.native_chat_capacity = 1;
    }
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
    assert_eq!(capacity::usage(&host.lock().unwrap()), 1);
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
