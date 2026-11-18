use super::*;
use crate::{
    chat::{
        Command, Event,
        protocol::{Adapter, ProcessSpec, Step},
    },
    model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
};
use serde_json::{Value, json};

#[test]
fn upgrade_restore_queue_budget_is_shared_across_conversations() {
    let mut budget = UpgradeRestoreQueueBudget::default();
    budget.charge(Duration::from_secs(40));
    assert!(budget.permits(Duration::from_secs(19)));
    assert!(!budget.permits(Duration::from_secs(20)));
    budget.charge(Duration::from_secs(15));
    assert!(budget.permits(Duration::from_secs(4)));
    assert!(!budget.permits(Duration::from_secs(5)));
}

#[test]
fn upgrade_restore_queue_budget_exhaustion_still_permits_unqueued_startup() {
    let mut budget = UpgradeRestoreQueueBudget::default();
    // A sample can cross the remaining budget; charging it must saturate.
    budget.charge(Duration::from_secs(61));
    assert!(budget.permits(Duration::ZERO));
    assert!(!budget.permits(Duration::from_millis(1)));
}

// Each case owns the process-global daemon wake pipe in an exact child;
// unrelated tests cannot supply the wake this regression is waiting for.
fn isolated_completion_wake_case(marker: &str) -> bool {
    const AUTH_PRESENT: &str = "CAGENTS_COMPLETION_WAKE_AUTH_PRESENT";
    if std::env::var_os(marker).is_some() {
        assert_eq!(
            std::env::var(AUTH_PRESENT).unwrap(),
            std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                .is_some()
                .to_string()
        );
        return false;
    }
    let directory = tempfile::Builder::new().prefix("cw-").tempdir().unwrap();
    let stdout_path = directory.path().join("stdout.log");
    let stderr_path = directory.path().join("stderr.log");
    let name = thread::current().name().unwrap().to_owned();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(marker, "1")
        .env(
            AUTH_PRESENT,
            std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                .is_some()
                .to_string(),
        )
        .env("XDG_RUNTIME_DIR", directory.path())
        .env("XDG_DATA_HOME", directory.path().join("data"))
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .env("XDG_CACHE_HOME", directory.path().join("cache"))
        .env("HOME", directory.path().join("home"))
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = std::fs::read_to_string(&stdout_path).unwrap();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap();
    print!("{stdout}");
    eprint!("{stderr}");
    if !status.success() {
        eprintln!(
            "saved completion-wake child logs: {}",
            directory.keep().display()
        );
    }
    assert!(status.success(), "completion wake child failed");
    true
}

fn completion_wake_host() -> SharedHost {
    Arc::new(HostLock::new(HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new(uuid::Uuid::new_v4().to_string()),
        host_incarnation: HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
        boot_id: crate::daemon::read_boot_id().unwrap(),
    })))
}

fn completion_wake_record(
    host: &SharedHost,
    id: TerminalId,
    instance: uuid::Uuid,
    handle: Handle,
    launch: chat::Launch,
) -> Session {
    let mut state = host.lock().unwrap();
    let session = Session {
        id,
        launch,
        proof: Some(state.proof().clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    state.chats.insert(
        id,
        Record {
            execution_instance: instance,
            console_recipe: None,
            paired_terminal: None,
            session: session.clone(),
            handle,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    session
}

fn direct_completion_actor(
    script: &str,
    cwd: &std::path::Path,
    notify: impl Fn(Update) + Send + Sync + 'static,
) -> (Handle, chat::Launch) {
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), script.into()],
        working_directory: cwd.into(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(spec, Box::new(Echo(false)), initial, store, None, notify).unwrap();
    (
        handle,
        chat::Launch {
            provider: Provider::Claude,
            recipe,
            session_id: None,
        },
    )
}

fn completion_wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "completion fixture did not advance"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn hidden_completion_wakes_parked_daemon_after_rearm_is_consumed_and_fences_old_owners() {
    if isolated_completion_wake_case("CAGENTS_LOCAL_COMPLETION_WAKE_CASE") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let listener_path = directory.path().join("idle.sock");
    let listener = std::os::unix::net::UnixListener::bind(&listener_path).unwrap();
    let host = completion_wake_host();
    let id = TerminalId::new();
    let instance = uuid::Uuid::new_v4();
    let weak = Arc::downgrade(&host);
    let (updates, received) = std::sync::mpsc::channel();
    let (handle, launch) = direct_completion_actor(
        "printf '{}\\n'; IFS= read -r line; printf '{}\\n'",
        directory.path(),
        move |update| {
            let completed = weak.upgrade().is_some_and(|host| {
                host.lock()
                    .unwrap()
                    .chats
                    .get(&id)
                    .is_some_and(|record| record.handle.is_completed())
            });
            let observed = matches!(update, Update::Observed { .. });
            changed_owned(&weak, id, instance, update);
            if observed {
                let _ = updates.send(completed);
            }
        },
    );
    completion_wake_record(&host, id, instance, handle.clone(), launch);
    handle.set_visible(false);
    // A real provider frame consumed the initial one-shot observation.
    assert!(!received.recv_timeout(Duration::from_secs(5)).unwrap());
    assert!(!handle.is_completed() && handle.owns_work());
    assert!(host.lock().unwrap().chats[&id].subscribers.is_empty());
    crate::daemon::wake();
    crate::daemon::wait_for_activity(&listener).unwrap(); // drain setup only
    let parked = listener.try_clone().unwrap();
    let (done, returned) = std::sync::mpsc::channel();
    let waiter = thread::spawn(move || {
        done.send(crate::daemon::wait_for_activity(&parked))
            .unwrap();
    });
    assert!(matches!(
        returned.recv_timeout(Duration::from_millis(40)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    changed_owned(
        &Arc::downgrade(&host),
        id,
        instance,
        Update::Observed { generation: 0 },
    );
    assert!(
        matches!(
            returned.recv_timeout(Duration::from_millis(40)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "a still-owned actor is not completion"
    );
    // This wakes only the actor's own command pipe, not the daemon listener.
    handle.command(Command::Refresh).unwrap();
    let result = returned.recv_timeout(Duration::from_secs(5));
    if result.is_err() {
        // Bounded fixture cleanup must not disguise the missing product wake.
        let _ = std::os::unix::net::UnixStream::connect(&listener_path);
    }
    waiter.join().unwrap();
    result
        .expect("completion did not wake parked daemon")
        .unwrap();
    completion_wait_until(|| handle.is_completed());
    handle.close().unwrap();
    let post = received.try_iter().filter(|completed| *completed).count();
    assert_eq!(
        post, 1,
        "exactly one local post-completion observation is forced"
    );
    assert!(!handle.owns_work());

    // A genuinely different, completed actor reuses this pane ID. Without
    // the instance fence, the retired callback would wake on its state.
    let (replacement, launch) = direct_completion_actor("printf '{}\\n'", directory.path(), |_| {});
    completion_wait_until(|| replacement.is_completed());
    replacement.close().unwrap();
    let replacement_instance = uuid::Uuid::new_v4();
    completion_wake_record(&host, id, replacement_instance, replacement.clone(), launch);
    crate::daemon::wake();
    crate::daemon::wait_for_activity(&listener).unwrap();
    let parked = listener.try_clone().unwrap();
    let (done, returned) = std::sync::mpsc::channel();
    let waiter = thread::spawn(move || {
        done.send(crate::daemon::wait_for_activity(&parked))
            .unwrap();
    });
    changed_owned(
        &Arc::downgrade(&host),
        id,
        instance,
        Update::Observed {
            generation: u64::MAX,
        },
    );
    assert!(
        matches!(
            returned.recv_timeout(Duration::from_millis(40)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "retired instance woke for replacement"
    );
    let (failed, launch) =
        direct_completion_actor("printf 'not-json\\n'", directory.path(), |_| {});
    completion_wait_until(|| failed.is_completed());
    assert!(failed.failure().is_some() && failed.owns_work());
    let failed_instance = uuid::Uuid::new_v4();
    completion_wake_record(&host, id, failed_instance, failed.clone(), launch);
    changed_owned(
        &Arc::downgrade(&host),
        id,
        failed_instance,
        Update::Observed {
            generation: u64::MAX,
        },
    );
    assert!(
        matches!(
            returned.recv_timeout(Duration::from_millis(40)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "failed owner announced released work"
    );
    let _client = std::os::unix::net::UnixStream::connect(&listener_path).unwrap();
    returned
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    waiter.join().unwrap();
    failed.close().unwrap();
    host.lock().unwrap().chats.clear();
}

#[test]
fn retained_hidden_completion_forwards_final_metadata_and_wakes_parked_daemon() {
    if isolated_completion_wake_case("CAGENTS_RETAINED_COMPLETION_WAKE_CASE") {
        return;
    }
    use std::io::Write;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    };
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("claude");
    std::fs::create_dir(&home).unwrap();
    let gate = directory.path().join("finish");
    let gate_path = std::ffi::CString::new(gate.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(gate_path.as_ptr(), 0o600) }, 0);
    let provider = directory.path().join("claude-fixture");
    std::fs::write(&provider, r#"#!/bin/sh
for arg in "$@"; do
    case "$arg" in --session-id=*) session=${arg#--session-id=} ;; esac
done
IFS= read -r frame || exit 2
id=${frame#*\"request_id\":\"}
id=${id%%\"*}
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"current_permission_mode":"default"}}}\n' "$id"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$session"
IFS= read -r finish < finish
"#).unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let host = completion_wake_host();
    // Semantic close commits the genuine helper journal and retirement;
    // the wake-only bare Host fixture does not install that durable store.
    host.lock().unwrap().install_model(
        super::super::model::Registry::at(&directory.path().join("registry.sqlite3")).unwrap(),
    );
    let proof = host.lock().unwrap().proof().clone();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let id = TerminalId::new();
    let source = Session {
        id,
        proof: Some(proof),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: provider.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: directory.path().into(),
                environment: BTreeMap::from([
                    (
                        "HOME".into(),
                        directory.path().to_string_lossy().into_owned(),
                    ),
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        home.to_string_lossy().into_owned(),
                    ),
                ]),
                unset_environment: Default::default(),
            },
        },
    };
    let (spec, adapter) = chat::protocol::create(&source.launch).unwrap();
    let prepared = adapter.prepared_launch_identity().unwrap();
    let weak = Arc::downgrade(&host);
    let owner = Arc::new(std::sync::OnceLock::new());
    let callback_owner = owner.clone();
    let handle = Handle::start_remote_owned(
        spec,
        source.clone(),
        prepared,
        BTreeMap::new(),
        move |update| {
            if let Some(instance) = callback_owner.get() {
                changed_owned(&weak, id, *instance, update);
            }
        },
    )
    .unwrap();
    // Use the production authenticated publication boundary: it records
    // pending ownership before installation starts the provider, then
    // promotes the exact confirmed helper/source before exposing input.
    let retained = [crate::agent_execution::HostedSession::Chat(source.clone())];
    let source = retained_actor::publish_authenticated_source(
        &host,
        &handle,
        source,
        &retained,
        None,
        None,
        retained_actor::Publication::Install,
    )
    .unwrap();
    let identity = handle.actor_identity().unwrap();
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let registered = registry
        .lock()
        .unwrap()
        .native_actor_ownership(identity.instance)
        .unwrap()
        .expect("production publication must retain exact durable ownership");
    assert!(!registered.is_pending());
    assert_eq!(registered.owner(), &identity.record);
    assert_eq!(registered.source(), &source);
    assert_eq!(registered.scope(), handle.launch_account_scope());
    owner.set(identity.instance).unwrap();
    host.lock().unwrap().chats.insert(
        id,
        Record {
            execution_instance: identity.instance,
            console_recipe: None,
            paired_terminal: None,
            session: source.clone(),
            handle: handle.clone(),
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    handle.set_visible(false);
    completion_wait_until(|| handle.snapshot(None).is_some_and(|snapshot| snapshot.ready));
    // No subscriber acknowledges/rearms this initial observation. The real
    // helper proxy must still publish final completed/owns_work metadata.
    assert!(!handle.is_completed() && handle.owns_work());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut finish = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&gate)
        {
            Ok(file) => break file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let listener_path = directory.path().join("idle.sock");
    let listener = std::os::unix::net::UnixListener::bind(&listener_path).unwrap();
    crate::daemon::wake();
    crate::daemon::wait_for_activity(&listener).unwrap();
    let parked = listener.try_clone().unwrap();
    let (done, returned) = std::sync::mpsc::channel();
    let waiter = thread::spawn(move || {
        done.send(crate::daemon::wait_for_activity(&parked))
            .unwrap();
    });
    changed_owned(
        &Arc::downgrade(&host),
        id,
        identity.instance,
        Update::Observed { generation: 0 },
    );
    assert!(
        matches!(
            returned.recv_timeout(Duration::from_millis(60)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "live retained owner woke as completion"
    );
    finish.write_all(b"finish\n").unwrap();
    drop(finish);
    let result = returned.recv_timeout(Duration::from_secs(5));
    if result.is_err() {
        let _ = std::os::unix::net::UnixStream::connect(&listener_path);
    }
    waiter.join().unwrap();
    result
        .expect("real retained completion did not wake daemon")
        .unwrap();
    assert!(handle.is_completed() && !handle.owns_work());
    assert!(
        !identity.record.root.is_current(),
        "original provider cleanup must precede wake"
    );
    handle.close().unwrap();
    // Use the existing semantic commit-before-retirement path, never ACK
    // discarded outcomes merely to make this fixture exit.
    retained_actor::finish_close(&host, &handle, &source).unwrap();
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let retired = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .expect("exact helper semantic retirement must be durably committed");
    assert_eq!(retired.actor(), identity.instance);
    assert_eq!(retired.owner(), &identity.record);
    assert_eq!(retired.source().id, source.id);
    // The retirement receipt can precede the startup reaper consuming the
    // original helper's zombie. Observe that exact incarnation disappearing.
    assert!(
        identity.record.holder.reaped(),
        "real helper was not retired"
    );
    host.lock().unwrap().chats.clear();
}

#[test]
fn chat_read_reports_remote_endpoint_failure_without_retiring_owned_chat() {
    let (handle, _other_attachment) = Handle::fixture_remote_attachment_pair();
    let identity = handle.actor_identity().unwrap();
    let proof = HostSessionProof {
        host_session_id: "presentation-host".into(),
        boot_id: "presentation-boot".into(),
        host_incarnation: "presentation-incarnation".into(),
    };
    let source = Session {
        id: identity.record.terminal,
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe::shell("/".into()).unwrap(),
            session_id: Some(uuid::Uuid::new_v4().to_string()),
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
    };
    let mut state = HostState::new(proof);
    state.register_client(1);
    state.chats.insert(
        source.id,
        Record {
            execution_instance: identity.instance,
            console_recipe: None,
            paired_terminal: None,
            session: source.clone(),
            handle: handle.clone(),
            source_process: None,
            subscribers: BTreeMap::from([(1, true)]),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    let host = Arc::new(HostLock::new(state));
    for after_generation in [None, Some(1)] {
        let failure = process(
            &host,
            1,
            ClientRequest::ChatRead {
                chat_id: source.id,
                after_generation,
            },
        )
        .unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(!failure.message.is_empty());
        let state = host.lock().unwrap();
        let retained = &state.chats[&source.id];
        assert_eq!(retained.session, source);
        assert!(retained.handle.same_attachment(&handle));
        assert!(retained.handle.owns_work());
        assert!(!retained.handle.is_completed());
        assert!(state.native_closing.is_empty());
        assert!(state.chat_spawning.is_empty());
    }
}

#[test]
fn native_constructor_environment_is_private_once_and_preserves_precedence_and_removals() {
    let root = tempfile::tempdir().unwrap();
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        let mut launch = chat::Launch {
            provider,
            session_id: None,
            recipe: LaunchRecipe {
                program: "private-fixture".into(),
                arguments: vec![],
                working_directory: root.path().into(),
                environment: [
                    ("HOME".into(), root.path().to_str().unwrap().into()),
                    ("PATH".into(), "/recipe/bin".into()),
                ]
                .into(),
                unset_environment: ["HTTPS_PROXY".into()].into(),
            },
        };
        let issued = launch.clone();
        let daemon = [
            ("PATH".into(), "/daemon/bin".into()),
            ("HTTPS_PROXY".into(), "daemon-proxy".into()),
        ]
        .into();
        let selected = [("PATH".into(), "/selected/bin".into())].into();
        let prepared =
            prepare_native_process_launch(&launch, Some(daemon), &selected, None).unwrap();
        assert_eq!(prepared.recipe.environment["PATH"], "/recipe/bin");
        assert!(!prepared.recipe.environment.contains_key("HTTPS_PROXY"));
        assert_eq!(launch, issued);
        let source = [
            ("HOME".into(), root.path().to_str().unwrap().into()),
            ("PATH".into(), "/source/bin".into()),
        ]
        .into();
        let prepared =
            prepare_native_process_launch(&launch, None, &BTreeMap::new(), Some(&source)).unwrap();
        assert_eq!(prepared.recipe.environment, source);
        let spec = chat::protocol::ProcessSpec::from(&prepared.recipe);
        assert_eq!(spec.environment, prepared.recipe.environment);
        assert_eq!(spec.unset_environment, prepared.recipe.unset_environment);
        assert_eq!(launch, issued);
        launch.recipe.environment.remove("PATH");
        assert!(
            prepare_native_process_launch(
                &launch,
                Some(BTreeMap::new()),
                &[(
                    "UNRELATED_PRIVATE_FIXTURE_KEY".into(),
                    "do-not-admit".into()
                )]
                .into(),
                None
            )
            .is_err()
        );
        // An older client started inside another agent's session still
        // sends it; the conversation starts without it.
        let prepared = prepare_native_process_launch(
            &launch,
            Some(BTreeMap::new()),
            &[
                ("PATH".into(), "/selected/bin".into()),
                ("CLAUDECODE".into(), "1".into()),
                (
                    "CLAUDE_CODE_MESSAGING_SOCKET".into(),
                    "/run/parent.sock".into(),
                ),
            ]
            .into(),
            None,
        )
        .unwrap();
        assert_eq!(prepared.recipe.environment["PATH"], "/selected/bin");
        assert!(!prepared.recipe.environment.contains_key("CLAUDECODE"));
        assert!(
            !prepared
                .recipe
                .environment
                .contains_key("CLAUDE_CODE_MESSAGING_SOCKET")
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_constructor_history_uses_effective_source_selected_and_unset_home_without_reading_issued_home()
 {
    use std::{
        io::Read,
        os::fd::{AsRawFd, FromRawFd},
    };
    const CONVERSATION: &str = "12345678-1234-4234-8234-123456789abc";
    for mode in ["source", "selected", "unset"] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let issued_home = root.path().join("issued-home");
        let actual_home = root.path().join("actual-home");
        let issued_config = issued_home.join(".claude");
        let actual_config = actual_home.join(".claude");
        let mut issued_file = None;
        for (config, body) in [
            (&issued_config, "issued-A-body"),
            (&actual_config, "effective-B-body"),
        ] {
            let path = config
                .join("projects")
                .join(&project)
                .join(format!("{CONVERSATION}.jsonl"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("{}\n{}\n",
                json!({"type":"user","uuid":"private-prompt","parentUuid":null,"sessionId":CONVERSATION,"cwd":cwd,"message":{"role":"user","content":body}}),
                json!({"type":"custom-title","sessionId":CONVERSATION,"customTitle":format!("{body} title")}))).unwrap();
            if config == &issued_config {
                issued_file = Some(path);
            }
        }
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let mut watcher = unsafe { std::fs::File::from_raw_fd(fd) };
        let path =
            std::ffi::CString::new(issued_file.unwrap().as_os_str().as_encoded_bytes()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    watcher.as_raw_fd(),
                    path.as_ptr(),
                    libc::IN_OPEN | libc::IN_ACCESS,
                )
            } >= 0
        );
        let mut launch = chat::Launch {
            provider: Provider::Claude,
            session_id: Some(CONVERSATION.into()),
            recipe: LaunchRecipe {
                program: "private-claude-not-executed".into(),
                arguments: vec!["--resume".into(), CONVERSATION.into()],
                working_directory: cwd,
                environment: [
                    ("HOME".into(), issued_home.to_str().unwrap().into()),
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        issued_config.to_str().unwrap().into(),
                    ),
                ]
                .into(),
                unset_environment: Default::default(),
            },
        };
        let source = [
            ("HOME".into(), actual_home.to_str().unwrap().into()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                actual_config.to_str().unwrap().into(),
            ),
        ]
        .into();
        let selected = if mode == "selected" {
            launch.recipe.environment.remove("CLAUDE_CONFIG_DIR");
            [(
                "CLAUDE_CONFIG_DIR".into(),
                actual_config.to_str().unwrap().into(),
            )]
            .into()
        } else {
            BTreeMap::new()
        };
        if mode == "unset" {
            launch
                .recipe
                .environment
                .insert("HOME".into(), actual_home.to_str().unwrap().into());
            launch
                .recipe
                .unset_environment
                .insert("CLAUDE_CONFIG_DIR".into());
        }
        let issued = launch.clone();
        let prepared = prepare_native_process_launch(
            &launch,
            (mode != "source").then(BTreeMap::new),
            &selected,
            (mode == "source").then_some(&source),
        )
        .unwrap();
        assert_eq!(
            Provider::Claude.session_root(&prepared.recipe),
            Some(actual_config.clone())
        );
        let (spec, mut adapter) = chat::protocol::create(&prepared).unwrap();
        assert_eq!(spec.environment, prepared.recipe.environment);
        assert_eq!(spec.unset_environment, prepared.recipe.unset_environment);
        let initial = adapter.initialize().unwrap();
        let items: Vec<_> = initial
            .events
            .iter()
            .filter_map(|event| {
                if let chat::Event::History { items, .. } = event {
                    Some(items)
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        assert!(items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text {text, ..} if text == "effective-B-body")));
        assert!(!items.iter().any(|item| matches!(&item.content, chat::ItemContent::Text {text, ..} if text == "issued-A-body")));
        // The real adapter's confirmed result samples its title reader.
        // The same prepared origin must supply both history and metadata.
        let result = adapter
            .receive(json!({"type":"result","session_id":CONVERSATION}))
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(event,
            chat::Event::Session {id, title:Some(title)}
                if id == CONVERSATION && title == "effective-B-body title")));
        assert!(!result.events.iter().any(|event| matches!(event,
            chat::Event::Session {title:Some(title), ..} if title == "issued-A-body title")));
        let mut bytes = [0_u8; 256];
        assert_eq!(
            watcher.read(&mut bytes).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "issued transcript must never be opened/indexed"
        );
        assert_eq!(launch, issued);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn upgrade_history_validation_keeps_recorded_home_after_live_working_directory_disappears() {
    use std::{
        io::Read,
        os::fd::{AsRawFd, FromRawFd},
    };
    const ID: &str = "12345678-1234-4234-8234-123456789abc";
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("now-missing-project");
    let project: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let home_a = root.path().join("recorded-home");
    let home_b = root.path().join("daemon-home");
    let mut watches = Vec::new();
    for (home, body) in [(&home_a, "recorded-A"), (&home_b, "unrelated-B")] {
        let file = home
            .join(".claude/projects")
            .join(&project)
            .join(format!("{ID}.jsonl"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, json!({"type":"user","uuid":"private-prompt","parentUuid":null,"sessionId":ID,"message":{"role":"user","content":body}}).to_string()+"\n").unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        let watcher = unsafe { std::fs::File::from_raw_fd(fd) };
        let path = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    watcher.as_raw_fd(),
                    path.as_ptr(),
                    libc::IN_OPEN | libc::IN_ACCESS,
                )
            } >= 0
        );
        watches.push(watcher);
    }
    let launch = chat::Launch {
        provider: Provider::Claude,
        session_id: Some(ID.into()),
        recipe: LaunchRecipe {
            program: "private-provider-not-executed".into(),
            arguments: vec!["--resume".into(), ID.into()],
            working_directory: cwd,
            environment: [("HOME".into(), home_a.to_str().unwrap().into())].into(),
            unset_environment: Default::default(),
        },
    };
    std::fs::create_dir(&launch.recipe.working_directory).unwrap();
    assert!(!launch.needs_read_only_history());
    std::fs::remove_dir(&launch.recipe.working_directory).unwrap();
    assert!(launch.needs_read_only_history());
    let issued = launch.clone();
    let restored = upgrade_environment(
        &launch,
        false,
        [
            ("HOME".into(), home_b.to_str().unwrap().into()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                home_b.join(".claude").to_str().unwrap().into(),
            ),
        ]
        .into(),
    )
    .unwrap();
    assert_eq!(restored, launch.recipe.environment);
    let mut bytes = [0_u8; 256];
    assert!(
        watches[0].read(&mut bytes).unwrap() > 0,
        "actual upgrade factory must read recorded A"
    );
    assert_eq!(
        watches[1].read(&mut bytes).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "upgrade validation must not read daemon B"
    );
    assert_eq!(launch, issued);
}

#[test]
fn removed_claude_account_is_reported_before_missing_resume_history() {
    const CHILD: &str = "CAGENTS_REMOVED_ACCOUNT_HISTORY_TEST";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        crate::secure_file::prepare_private_directory(&directory.path().join("runtime")).unwrap();
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "host::chat::tests::removed_claude_account_is_reported_before_missing_resume_history", "--nocapture"])
            .env_clear().env(CHILD, "1").env("HOME", directory.path())
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
            .env("PATH", "/usr/bin:/bin")
            .status().unwrap().success());
        return;
    }
    let homes = crate::account::home::Homes::installation().unwrap();
    let store = crate::account::registry::Store::installation().unwrap();
    let (key, home) = store
        .create_pending(&homes, Provider::Claude, 1, || {
            homes.create(Provider::Claude, None)
        })
        .unwrap();
    let session = Session {
        id: TerminalId::new(),
        native_continuity: Default::default(),
        owned_codex_listener: None,
        proof: None,
        terminal: None,
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: "/bin/false".into(),
                arguments: Vec::new(),
                working_directory: std::env::current_dir().unwrap(),
                environment: BTreeMap::from([(
                    "CLAUDE_CONFIG_DIR".into(),
                    home.to_str().unwrap().into(),
                )]),
                unset_environment: Default::default(),
            },
            session_id: Some("3a888c9d-7e33-47ad-9c0f-d538f63b9099".into()),
        },
    };
    store
        .remove_home(&homes, Provider::Claude, key, false)
        .unwrap();
    assert!(!home.exists());
    // Without the account preflight this exact adapter preparation error
    // used to hide Recovery::Account and leave the UI without its chooser.
    assert!(
        chat::protocol::create(&session.launch)
            .err()
            .unwrap()
            .contains("history project directory is unavailable")
    );
    let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
        HostSessionProof {
            host_session_id: HostSessionId::new("removed-history"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        },
    )));
    let failure = start(&host, 1, session, None, false, None, Default::default()).unwrap_err();
    assert_eq!(failure.code, ProtocolErrorCode::AccountRemoved);
    assert!(
        !home.exists(),
        "preparation never recreates the removed home"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
}

/// A conversation opened on a stopped agent terminal starts with the
/// daemon's own provider configuration, so an upgrade restores it the same
/// way rather than refusing it for a variable only the daemon supplies.
#[test]
fn an_upgrade_restores_the_environment_a_chat_actually_started_with() {
    let launch = chat::Launch {
        provider: Provider::Omp,
        recipe: LaunchRecipe {
            unset_environment: Default::default(),
            program: "omp".into(),
            arguments: vec!["--model".into(), "model".into()],
            working_directory: "/project".into(),
            environment: BTreeMap::from([
                ("HOME".into(), "/home/user".into()),
                ("PATH".into(), "/recipe/bin".into()),
            ]),
        },
        session_id: Some("01a0ab66-21d6-7065-9fe5-95f302b669e1".into()),
    };
    let daemon = BTreeMap::from([
        ("BUN_INSTALL_GLOBAL_STORE".into(), "/store".into()),
        ("PATH".into(), "/daemon/bin".into()),
    ]);
    let started = sourceless_environment(
        daemon.clone(),
        BTreeMap::new(),
        chat::protocol::create(&launch).unwrap().0.environment,
    );
    assert_eq!(started["BUN_INSTALL_GLOBAL_STORE"], "/store");
    assert_eq!(started["PATH"], "/recipe/bin");
    assert_eq!(
        upgrade_environment(&launch, false, daemon.clone()).unwrap(),
        started
    );
    // A conversation whose terminal is still hosted restarts from that
    // terminal, not from the daemon.
    assert_eq!(
        upgrade_environment(&launch, true, daemon).unwrap(),
        launch.recipe.environment
    );
}

#[test]
fn selected_account_removals_survive_daemon_environment_and_upgrade() {
    let account = crate::account::Account::managed(
        Provider::Claude,
        uuid::Uuid::new_v4(),
        "/selected".into(),
    );
    let launch = chat::Launch {
        provider: Provider::Claude,
        recipe: LaunchRecipe {
            program: "claude".into(),
            arguments: Vec::new(),
            working_directory: "/project".into(),
            environment: BTreeMap::from([
                ("CLAUDE_CONFIG_DIR".into(), "/selected".into()),
                ("ANTHROPIC_API_KEY".into(), "recipe-fixture".into()),
            ]),
            unset_environment: account
                .credential_overrides()
                .iter()
                .map(|name| (*name).into())
                .collect(),
        },
        session_id: None,
    };
    let daemon = BTreeMap::from([
        ("ANTHROPIC_API_KEY".into(), "daemon-fixture".into()),
        ("HTTPS_PROXY".into(), "http://fixture".into()),
    ]);
    let selected = BTreeMap::from([("CLAUDE_CODE_OAUTH_TOKEN".into(), "gui-fixture".into())]);
    let (mut spec, _) = chat::protocol::create(&launch).unwrap();
    spec.environment = sourceless_environment(daemon.clone(), selected, spec.environment);
    spec.withhold_environment();
    assert_eq!(
        spec.environment
            .get("CLAUDE_CONFIG_DIR")
            .map(String::as_str),
        Some("/selected")
    );
    assert!(!spec.environment.contains_key("ANTHROPIC_API_KEY"));
    assert!(!spec.environment.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
    assert_eq!(
        spec.environment.get("HTTPS_PROXY").map(String::as_str),
        Some("http://fixture")
    );
    assert_eq!(
        upgrade_environment(&launch, false, daemon).unwrap(),
        spec.environment
    );
    assert!(
        !upgrade_environment(&launch, true, BTreeMap::new())
            .unwrap()
            .contains_key("ANTHROPIC_API_KEY")
    );
}

/// A conversation on the system Claude login resolves the CLI's default
/// home, as the window's probe does, even under a daemon started with
/// its own `CLAUDE_CONFIG_DIR`, and after an upgrade.
#[test]
fn a_system_claude_launch_does_not_take_the_daemon_home() {
    let mut recipe = LaunchRecipe {
        program: "claude".into(),
        arguments: Vec::new(),
        working_directory: "/project".into(),
        environment: BTreeMap::from([("HOME".into(), "/home/a".into())]),
        unset_environment: Default::default(),
    };
    crate::account::Account::system(Provider::Claude)
        .bind_home_in(&mut recipe, &|name| {
            (name == "HOME").then(|| "/home/a".to_owned())
        })
        .unwrap();
    let launch = chat::Launch {
        provider: Provider::Claude,
        recipe,
        session_id: None,
    };
    let daemon = BTreeMap::from([
        ("CLAUDE_CONFIG_DIR".into(), "/daemon/claude".into()),
        ("HTTPS_PROXY".into(), "http://fixture".into()),
    ]);
    let prepared =
        prepare_native_process_launch(&launch, Some(daemon.clone()), &BTreeMap::new(), None)
            .unwrap();
    assert!(
        !prepared
            .recipe
            .environment
            .contains_key("CLAUDE_CONFIG_DIR")
    );
    assert!(
        prepared
            .recipe
            .unset_environment
            .contains("CLAUDE_CONFIG_DIR")
    );
    assert_eq!(
        prepared
            .recipe
            .environment
            .get("HTTPS_PROXY")
            .map(String::as_str),
        Some("http://fixture")
    );
    assert!(
        !upgrade_environment(&launch, false, daemon)
            .unwrap()
            .contains_key("CLAUDE_CONFIG_DIR")
    );
}

#[test]
fn paired_codex_upgrade_preserves_confirmed_fresh_or_switched_thread() {
    use crate::model::agent_recovery::{AgentProcess, bind_session};
    const SELECTED: &str = "12345678-1234-4234-8234-123456789abc";
    const ORIGINAL: &str = "98765432-1234-4234-8234-123456789abc";
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "codex".into(),
        arguments: vec!["--profile".into(), "work".into()],
        working_directory: "/project".into(),
        environment: BTreeMap::from([("CODEX_HOME".into(), "/account".into())]),
    };
    let expected = bind_session(&recipe, SELECTED).unwrap();
    let launch = chat::Launch {
        provider: Provider::Codex,
        recipe: expected.clone(),
        session_id: Some(SELECTED.into()),
    };
    for source in [recipe.clone(), bind_session(&recipe, ORIGINAL).unwrap()] {
        let process = AgentProcess {
            pid: 42,
            start: 100,
            provider: Provider::Codex,
            launch: source.clone(),
            recovery: source,
        };
        assert_eq!(
            paired_upgrade_recipe(&process, &process, &launch, Some(SELECTED)).unwrap(),
            expected,
        );
        assert!(paired_upgrade_recipe(&process, &process, &launch, Some(ORIGINAL)).is_err());
        assert!(paired_upgrade_recipe(&process, &process, &launch, None).is_err());
        let mut replacement = process.clone();
        replacement.start += 1;
        assert!(paired_upgrade_recipe(&process, &replacement, &launch, Some(SELECTED)).is_err());
        let mut other_account = process.clone();
        other_account
            .recovery
            .environment
            .insert("CODEX_HOME".into(), "/other".into());
        assert!(paired_upgrade_recipe(&process, &other_account, &launch, Some(SELECTED)).is_err());
        // Matching process evidence alone cannot bless a stale native launch.
        assert!(
            paired_upgrade_recipe(&other_account, &other_account, &launch, Some(SELECTED)).is_err()
        );
        for mutation in 0..5 {
            let mut changed = process.clone();
            match mutation {
                0 => changed.recovery.program = "/other/codex".into(),
                1 => changed.recovery.working_directory = "/other".into(),
                2 => changed
                    .recovery
                    .arguments
                    .extend(["--model".into(), "other".into()]),
                3 => {
                    changed
                        .recovery
                        .unset_environment
                        .insert("CODEX_API_KEY".into());
                }
                _ => changed
                    .recovery
                    .arguments
                    .extend(["--remote".into(), "unix:///other".into()]),
            }
            assert!(paired_upgrade_recipe(&changed, &changed, &launch, Some(SELECTED)).is_err());
        }
    }
}

#[test]
fn paired_upgrade_preserves_issued_metadata_and_refuses_owner_changes() {
    let proof = |name| HostSessionProof {
        host_session_id: HostSessionId::new(name),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new(name),
    };
    let continuity = |proof: HostSessionProof| crate::model::TerminalContinuity::Live {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    let mut issued = crate::terminal_ipc::TerminalDescriptor::relaunchable(
        TerminalId::new(),
        LaunchRecipe::shell("/project".into()).unwrap(),
        None,
    )
    .session;
    issued.continuity = continuity(proof("old"));
    issued.resume_hint = Some("original issued observation".into());
    let mut current = issued.clone();
    current.continuity = continuity(proof("new"));
    current.resume_hint = Some("current worker observation".into());
    let mut restored = restored_terminal_source(&issued, &current).unwrap();
    assert_eq!(restored.continuity, current.continuity);
    restored.continuity = issued.continuity.clone();
    assert_eq!(restored, issued);
    for mutation in 0..9 {
        let mut changed = current.clone();
        match mutation {
            0 => changed.id = TerminalId::new(),
            1 => changed.launch.program = "/another".into(),
            2 => changed.launch.arguments.push("another".into()),
            3 => changed.launch.working_directory = "/another".into(),
            4 => {
                changed
                    .launch
                    .environment
                    .insert("CODEX_HOME".into(), "/another".into());
            }
            5 => {
                changed
                    .launch
                    .unset_environment
                    .insert("CODEX_API_KEY".into());
            }
            6 => changed.transcript_path = Some("/another".into()),
            7 => changed.recovery_origin = Some(TerminalId::new()),
            _ => changed.predecessor = Some(TerminalId::new()),
        }
        assert!(restored_terminal_source(&issued, &changed).is_err());
    }
    current.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
        recipe: current.launch.clone(),
        last_exit: None,
    };
    assert!(restored_terminal_source(&issued, &current).is_err());
}

#[test]
fn confirmed_selection_is_bound_to_process_account_identity_and_expiration() {
    let now = Instant::now();
    let id = "12345678-1234-4234-8234-123456789abc";
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "codex".into(),
        arguments: Vec::new(),
        working_directory: "/same".into(),
        environment: BTreeMap::from([("HOME".into(), "/account".into())]),
    };
    let process = crate::model::agent_recovery::AgentProcess {
        pid: 42,
        start: 100,
        provider: Provider::Codex,
        launch: recipe.clone(),
        recovery: recipe,
    };
    let binding = PendingBinding {
        preview: chat::BindingPreview {
            token: "single-use".into(),
            session_id: id.into(),
            title: "Chosen".into(),
            working_directory: "/same".into(),
        },
        process: process.clone(),
        expires: now + Duration::from_secs(1),
    };
    assert!(binding.authorizes(&process, id, now));
    assert!(!binding.authorizes(&process, "98765432-1234-4234-8234-123456789abc", now));
    assert!(!binding.authorizes(&process, id, binding.expires));
    let mut replacement = process.clone();
    replacement.start += 1;
    assert!(!binding.authorizes(&replacement, id, now));
    let mut other_account = process;
    other_account
        .recovery
        .environment
        .insert("HOME".into(), "/other-account".into());
    assert!(!binding.authorizes(&other_account, id, now));
}

#[test]
fn a_candidate_listing_names_a_terminal_where_a_preview_names_a_conversation() {
    let (_directory, host, _saved) = upgrade_fixture();
    let source = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let session = Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                unset_environment: Default::default(),
                program: "codex".into(),
                arguments: Vec::new(),
                working_directory: "/project".into(),
                environment: BTreeMap::new(),
            },
            session_id: None,
        },
        proof: Some(source.clone()),
        terminal: None,
    };
    // A listing names a terminal, never a conversation, so the missing ID
    // that a preview rejects outright still reaches the terminal lookup.
    let listed = process(
        &host,
        1,
        ClientRequest::ChatCandidates {
            session: session.clone(),
            source: source.clone(),
        },
    )
    .unwrap_err();
    assert_eq!(listed.code, ProtocolErrorCode::TerminalUnavailable);
    let previewed = process(
        &host,
        1,
        ClientRequest::ChatPreview {
            session: session.clone(),
            source: source.clone(),
        },
    )
    .unwrap_err();
    assert_eq!(previewed.code, ProtocolErrorCode::InvalidRequest);
    // An activation still has to consume a token issued by a real preview,
    // and no listing can produce one.
    let activated = process(
        &host,
        1,
        ClientRequest::ChatStartSelected {
            session,
            source,
            token: "forged".into(),
        },
    )
    .unwrap_err();
    assert_eq!(activated.code, ProtocolErrorCode::InvalidRequest);
}

fn upgrade_fixture() -> (tempfile::TempDir, SharedHost, crate::upgrade::handoff::Chat) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("omp");
    std::fs::write(&executable, r#"#!/usr/bin/python3
import json, os, sys
identity = sys.argv[sys.argv.index("--resume") + 1]
print(json.dumps({"type": "ready"}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    command = request["type"]
    if command not in ("negotiate_protocol", "get_state", "get_available_models", "get_available_commands", "get_session_stats", "get_messages_page", "get_subagents", "set_subagent_subscription"):
        sys.exit(7)
    data = {"sessionId": os.environ.get("REPORTED_SESSION", identity), "messages": []}
    print(json.dumps({"type": "response", "id": request["id"], "command": command, "success": True, "data": data}), flush=True)
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let old = HostSessionProof {
        host_session_id: HostSessionId::new("old-session"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("old-incarnation"),
    };
    let current = HostSessionProof {
        host_session_id: HostSessionId::new("new-session"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("new-incarnation"),
    };
    let launch = chat::Launch {
        provider: Provider::Omp,
        recipe: LaunchRecipe {
            unset_environment: Default::default(),
            program: executable.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            working_directory: directory.path().to_owned(),
            environment: BTreeMap::new(),
        },
        session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
    };
    let source = Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: launch.clone(),
        proof: Some(old),
        terminal: None,
    };
    (
        directory,
        Arc::new(crate::host::state::HostLock::new(HostState::new(current))),
        crate::upgrade::handoff::Chat {
            source,
            launch,
            previous_sources: Vec::new(),
        },
    )
}

#[test]
fn missing_claude_directory_restores_a_hosted_history_reader_without_authority() {
    let (root, host, mut saved) = upgrade_fixture();
    let cwd = root.path().join("deleted-project");
    let config = root.path().join("claude-config");
    let project: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcript = config
        .join("projects")
        .join(project)
        .join("12345678-1234-4234-8234-123456789abc.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, serde_json::json!({"type":"user","uuid":"saved-prompt","parentUuid":null,"sessionId":"12345678-1234-4234-8234-123456789abc","message":{"role":"user","content":"A prompt saved before the worktree was removed"}}).to_string() + "\n").unwrap();
    saved.launch.provider = Provider::Claude;
    saved.launch.recipe.program = "claude".into();
    saved.launch.recipe.working_directory = cwd;
    saved.launch.recipe.environment = BTreeMap::from([(
        "CLAUDE_CONFIG_DIR".into(),
        config.to_string_lossy().into_owned(),
    )]);
    saved.source.launch = saved.launch.clone();
    saved.source.proof = None;
    let response = start(
        &host,
        1,
        saved.source.clone(),
        None,
        false,
        None,
        Default::default(),
    )
    .unwrap();
    assert!(matches!(response, ServerResponse::ChatAttached { .. }));
    let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
        .handle
        .clone();
    handle.set_visible(true);
    // The reader's exit ends the saved frame it applies after attach.
    let snapshot = crate::test_timing::wait_for("the saved reader to apply its frame", || {
        handle.snapshot(None).filter(|snapshot| snapshot.exited)
    });
    assert_eq!(snapshot.session_id, saved.launch.session_id);
    assert_eq!(snapshot.items.len(), 1);
    assert!(!handle.owns_work());
    assert!(
        handle.confirmed_session_id().is_none(),
        "disk identity is not provider confirmation"
    );
    assert!(
        !handle
            .execution_environment()
            .0
            .contains_key(crate::agent_execution::AUTHORITY_ENV)
    );
    assert!(
        handle
            .command(chat::Command::Prompt {
                text: "do not run".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send
            })
            .unwrap_err()
            .contains("read-only")
    );
    handle.command(chat::Command::Refresh).unwrap();
    let failure = restore_upgrade(&host, &saved).unwrap_err();
    assert!(failure.contains("only its conversation history can be opened"));
    assert!(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .upgrade_chats
            .is_empty()
    );
    handle.close().unwrap();
}

#[test]
fn distinct_recovery_candidates_share_one_conversation_spawn_claim() {
    let (_directory, host, saved) = upgrade_fixture();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut candidates = Vec::new();
    for client in [1, 2] {
        let mut candidate = saved.source.clone();
        candidate.id = TerminalId::new();
        candidate.proof = None;
        let host = Arc::clone(&host);
        let barrier = Arc::clone(&barrier);
        candidates.push(thread::spawn(move || {
            barrier.wait();
            (
                client,
                start(
                    &host,
                    client,
                    candidate,
                    None,
                    false,
                    None,
                    Default::default(),
                ),
            )
        }));
    }
    barrier.wait();
    let outcomes = candidates
        .into_iter()
        .map(|task| task.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    let (client, id) = outcomes
        .into_iter()
        .find_map(|(client, result)| match result {
            Ok(ServerResponse::ChatAttached { session, .. }) => Some((client, session.id)),
            _ => None,
        })
        .unwrap();
    let mut retry = saved.source;
    retry.id = TerminalId::new();
    retry.proof = None;
    assert!(start(&host, 3, retry, None, false, None, Default::default()).is_err());
    assert_eq!(host.lock().unwrap().chats.len(), 1);
    process(&host, client, ClientRequest::ChatClose { chat_id: id }).unwrap();
}

#[test]
fn durable_terminal_commit_retires_evidence_after_terminal_retirement() {
    let (_directory, host, saved) = upgrade_fixture();
    let source_id = saved.source.id;
    let recipe = saved.launch.terminal_recipe().unwrap();
    let terminal_id = TerminalId::new();
    let terminal = crate::model::TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: terminal_id,
        launch: recipe.clone(),
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
    };
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .chat_handoffs
        .insert(
            source_id,
            TerminalHandoff::new(
                saved.source,
                terminal,
                1,
                BTreeMap::new(),
                Instant::now() + ACKNOWLEDGEMENT_GRACE,
            ),
        );
    // No terminal record remains, but the proof-bound terminal ID still
    // names the durable replacement. EOF/terminal close did not consume it.
    for _ in 0..2 {
        assert!(matches!(
            process(&host, 1, ClientRequest::ChatCommitTerminal { terminal_id }).unwrap(),
            ServerResponse::Accepted
        ));
    }
    assert!(
        !host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chat_handoffs
            .contains_key(&source_id)
    );
}

#[test]
pub(in crate::host) fn standalone_credentials_survive_failed_and_retried_terminal_handoff_without_persistence()
 {
    if !super::super::state::tests::upgrade_fixture_in_fresh_process() {
        return;
    }
    let (directory, host, saved) = upgrade_fixture();
    std::fs::write(
        directory.path().join("omp"),
        r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
assert os.environ["ANTHROPIC_API_KEY"] == "gui-only-key"
assert os.environ["HTTPS_PROXY"] == "http://user:password@proxy.invalid:8080"
assert os.environ["PI_CACHE_RETENTION"] == "long"
assert os.environ["GIT_CONFIG_KEY_0"] == "http.extraHeader"
assert os.environ["GIT_CONFIG_VALUE_0"] == "Authorization: Bearer git-private-token"
assert "PI_TOOL_BRIDGE_TOKEN" not in os.environ
identity = "12345678-1234-4234-8234-123456789abc"
if "--mode" not in sys.argv:
    assert sys.argv[sys.argv.index("--resume") + 1] == identity
    with pathlib.Path("terminal-starts").open("a") as starts:
        starts.write("ok\n")
    for line in sys.stdin:
        pass
    sys.exit(0)
pathlib.Path("environment-received").write_text("ok")
print(json.dumps({"type": "ready"}), flush=True)
for line in sys.stdin:
    while not pathlib.Path("publish-identity").exists():
        time.sleep(0.01)
    request = json.loads(line)
    data = {"sessionId": identity, "messages": []}
    print(json.dumps({"type": "response", "id": request["id"], "command": request["type"], "success": True, "data": data}), flush=True)
"#,
    )
    .unwrap();
    let mut session = saved.source;
    session.proof = None;
    session.launch.session_id = None;
    let execution_environment = crate::terminal_ipc::ChatExecutionEnvironment(BTreeMap::from([
        ("ANTHROPIC_API_KEY".into(), "gui-only-key".into()),
        (
            "HTTPS_PROXY".into(),
            "http://user:password@proxy.invalid:8080".into(),
        ),
        ("PI_CACHE_RETENTION".into(), "long".into()),
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
        (
            "GIT_CONFIG_VALUE_0".into(),
            "Authorization: Bearer git-private-token".into(),
        ),
    ]));
    let request = ClientRequest::ChatStart {
        session: session.clone(),
        source: None,
        terminate_source: false,
        execution_environment,
    };
    assert!(!format!("{request:?}").contains("gui-only-key"));
    assert!(!format!("{request:?}").contains("user:password"));
    let response = process(&host, 1, request).unwrap();
    let ServerResponse::ChatAttached {
        session: attached, ..
    } = response
    else {
        panic!("standalone chat did not attach");
    };
    assert!(
        !attached
            .launch
            .recipe
            .environment
            .contains_key("ANTHROPIC_API_KEY")
    );
    assert!(
        !attached
            .launch
            .recipe
            .environment
            .contains_key("HTTPS_PROXY")
    );
    let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&session.id]
        .handle
        .clone();
    let resources_alive = handle.resource_lifetime();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !directory.path().join("environment-received").exists() {
        assert!(
            Instant::now() < deadline,
            "provider did not receive GUI execution environment"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // A successful process spawn is not a durable fresh conversion boundary.
    let mut launch = attached.launch.clone();
    assert!(confirm_fresh_session(&handle, &mut launch, Instant::now(), || false).is_err());
    assert_eq!(launch.session_id, None);
    std::fs::write(directory.path().join("publish-identity"), "").unwrap();
    confirm_fresh_session(
        &handle,
        &mut launch,
        Instant::now() + Duration::from_secs(60),
        || false,
    )
    .unwrap();
    assert_eq!(
        launch.session_id.as_deref(),
        Some("12345678-1234-4234-8234-123456789abc")
    );
    assert!(launch.terminal_recipe().is_some());
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
    assert!(failure.message.contains("execution environment"));
    assert!(!handle.is_completed());
    assert!(!host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down);

    // Losing the executable after native startup must retain the selected
    // account for retry, even though the native actor has already closed.
    std::fs::rename(
        directory.path().join("omp"),
        directory.path().join("held-omp"),
    )
    .unwrap();
    assert!(
        process(
            &host,
            1,
            ClientRequest::ChatReturnTerminal {
                chat_id: session.id
            }
        )
        .is_err()
    );
    assert!(handle.is_completed());
    assert!(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .contains_key(&session.id)
    );
    assert!(
        !host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chat_handoffs
            .contains_key(&session.id)
    );
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
    assert!(failure.message.contains("replacement is still pending"));
    std::fs::rename(
        directory.path().join("held-omp"),
        directory.path().join("omp"),
    )
    .unwrap();
    let ServerResponse::ChatTerminal {
        session: terminal, ..
    } = process(
        &host,
        1,
        ClientRequest::ChatReturnTerminal {
            chat_id: session.id,
        },
    )
    .unwrap()
    else {
        panic!("native chat did not return its terminal");
    };
    assert!(terminal.continuity.live_proof().is_some());
    drop(handle);
    assert!(
        !resources_alive(),
        "successful handoff retained closed actor resources"
    );
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        assert!(!state.chats.contains_key(&session.id));
        state.unregister_client(1);
        state.register_client(2);
    }
    assert!(
        process(
            &host,
            2,
            ClientRequest::ChatReturnTerminal {
                chat_id: session.id
            }
        )
        .is_err()
    );
    let ServerResponse::ChatAttached { snapshot, .. } = process(
        &host,
        2,
        ClientRequest::ChatAttach {
            session: attached.clone(),
        },
    )
    .unwrap() else {
        panic!("lost response did not permit exact reattachment");
    };
    assert!(snapshot.exited);
    assert_eq!(snapshot.session_id, launch.session_id);
    assert!(
        process(
            &host,
            2,
            ClientRequest::ChatClose {
                chat_id: session.id
            }
        )
        .is_err()
    );
    assert!(
        process(
            &host,
            2,
            ClientRequest::ChatStart {
                session: attached.clone(),
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            }
        )
        .is_err()
    );
    process(
        &host,
        2,
        ClientRequest::ChatVisibility {
            chat_id: session.id,
            visible: true,
        },
    )
    .unwrap();
    let ServerResponse::ChatSnapshot {
        snapshot: Some(polled),
        ..
    } = process(
        &host,
        2,
        ClientRequest::ChatRead {
            chat_id: session.id,
            after_generation: None,
        },
    )
    .unwrap()
    else {
        panic!("reattached receipt did not provide closed metadata");
    };
    assert!(polled.exited);
    let ServerResponse::ChatListed { sessions } =
        process(&host, 2, ClientRequest::ChatList).unwrap()
    else {
        panic!("inventory failed");
    };
    assert!(sessions.is_empty());
    let ServerResponse::ChatTerminal {
        session: repeated, ..
    } = process(
        &host,
        2,
        ClientRequest::ChatReturnTerminal {
            chat_id: session.id,
        },
    )
    .unwrap()
    else {
        panic!("lost handoff response was not replayed");
    };
    assert_eq!(repeated, terminal);
    let deadline = Instant::now() + Duration::from_secs(60);
    // The fixture creates this file before its buffered write reaches it.
    let terminal_starts = loop {
        let starts =
            std::fs::read_to_string(directory.path().join("terminal-starts")).unwrap_or_default();
        if !starts.is_empty() {
            break starts;
        }
        assert!(
            Instant::now() < deadline,
            "CLI did not receive the native execution environment"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(terminal_starts, "ok\n");
    for durable in [
        serde_json::to_string(&attached).unwrap(),
        serde_json::to_string(&terminal).unwrap(),
    ] {
        for secret in ["gui-only-key", "user:password", "git-private-token"] {
            assert!(!durable.contains(secret));
        }
    }
    let terminal_handle = host.lock().unwrap_or_else(|p| p.into_inner()).terminals[&terminal.id]
        .handle
        .clone();
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
    assert!(failure.message.contains("not durably acknowledged"));
    process(
        &host,
        2,
        ClientRequest::ChatCommitTerminal {
            terminal_id: TerminalId::new(),
        },
    )
    .unwrap();
    assert!(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .chat_handoffs
            .contains_key(&session.id)
    );
    // Receipt remains replayable until the caller has durably saved this
    // exact replacement. Losing the commit response permits the same ACK.
    for _ in 0..2 {
        assert!(matches!(
            process(
                &host,
                2,
                ClientRequest::ChatCommitTerminal {
                    terminal_id: terminal.id,
                }
            )
            .unwrap(),
            ServerResponse::Accepted
        ));
    }
    assert!(
        !host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chat_handoffs
            .contains_key(&session.id)
    );
    assert!(
        process(
            &host,
            2,
            ClientRequest::ChatAttach {
                session: attached.clone()
            }
        )
        .is_err()
    );
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    let failure = super::super::state::prepare_upgrade(&host, capability, false).unwrap_err();
    assert!(failure.message.contains("execution environment"));
    assert!(!terminal_handle.is_completed());
    assert!(!host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down);
    terminal_handle.shutdown().unwrap();
}

#[test]
fn native_upgrade_restores_exact_provider_session_and_only_resolves_proven_source() {
    let (_directory, host, mut saved) = upgrade_fixture();
    saved.launch.recipe = saved.launch.terminal_recipe().unwrap();
    assert_ne!(saved.launch.recipe, saved.source.launch.recipe);
    restore_upgrade(&host, &saved).unwrap();
    let restored = resolve_upgrade(&host, &saved.source).unwrap();
    assert_eq!(restored.id, saved.source.id);
    assert_eq!(restored.launch.session_id, saved.launch.session_id);
    assert_eq!(restored.launch.recipe, saved.source.launch.recipe);
    assert_eq!(
        restored.proof.as_ref(),
        Some(host.lock().unwrap_or_else(|p| p.into_inner()).proof())
    );
    assert_ne!(restored.proof, saved.source.proof);
    let mut unissued = saved.source.clone();
    unissued.launch.session_id = Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into());
    assert!(
        resolve_upgrade(&host, &unissued).is_err(),
        "a matching old proof cannot substitute an unissued conversation"
    );
    assert!(saved.source.launch.session_id.is_some());
    unissued.launch.session_id = None;
    assert!(
        resolve_upgrade(&host, &unissued).is_err(),
        "known identity cannot be erased into manufactured pre-ID authority"
    );
    let mut wrong_source = saved.source.clone();
    wrong_source.proof = restored.proof;
    assert!(resolve_upgrade(&host, &wrong_source).is_err());
    let mut unrelated_thread = saved.source.clone();
    unrelated_thread.launch.session_id = Some("98765432-1234-4234-8234-123456789abc".into());
    assert!(resolve_upgrade(&host, &unrelated_thread).is_err());
    assert!(restore_upgrade(&host, &saved).is_err());
    let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
        .handle
        .clone();
    handle.close().unwrap();
}

#[test]
fn native_upgrade_rejects_provider_substituting_another_conversation() {
    let (_directory, host, mut saved) = upgrade_fixture();
    saved.launch.recipe.environment.insert(
        "REPORTED_SESSION".into(),
        "98765432-1234-4234-8234-123456789abc".into(),
    );
    saved.source.launch.recipe.environment = saved.launch.recipe.environment.clone();
    assert!(restore_upgrade(&host, &saved).is_err());
    assert!(resolve_upgrade(&host, &saved.source).is_err());
    let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
        .handle
        .clone();
    handle
        .close()
        .expect("failed provider teardown permits explicit retirement");
    assert!(
        handle.failure().is_some(),
        "the rejected session remains diagnosed"
    );
}

#[test]
fn terminal_close_retires_only_the_exact_closed_actors_recovered_pane_aliases() {
    let (directory, host, saved) = upgrade_fixture();
    restore_upgrade(&host, &saved).unwrap();
    let replacement = resolve_upgrade(&host, &saved.source).unwrap();
    let evidence = Arc::new(crate::upgrade::handoff::Store::open(directory.path()).unwrap());
    evidence
        .record_recovered(&[crate::upgrade::handoff::RecoveredChat {
            evidence: saved.clone(),
            replacement: replacement.clone(),
            aliases: Vec::new(),
        }])
        .unwrap();
    let handle = {
        let mut state = host.lock().unwrap();
        state.upgrade_evidence = Some(evidence.clone());
        state.chats[&saved.source.id].handle.clone()
    };
    assert!(
        evidence
            .recovered_sources(&saved.source, &replacement)
            .unwrap()
            .is_some()
    );
    close_terminal_actor(&host, saved.source.id, &handle).unwrap();
    assert!(!host.lock().unwrap().chats.contains_key(&saved.source.id));
    assert!(
        evidence
            .recovered_sources(&saved.source, &replacement)
            .unwrap()
            .is_none()
    );
    // An old close receipt must not clear evidence for another live actor.
    restore_upgrade(&host, &saved).unwrap();
    let replacement = resolve_upgrade(&host, &saved.source).unwrap();
    evidence
        .record_recovered(&[crate::upgrade::handoff::RecoveredChat {
            evidence: saved.clone(),
            replacement: replacement.clone(),
            aliases: Vec::new(),
        }])
        .unwrap();
    close_terminal_actor(&host, saved.source.id, &handle).unwrap();
    assert!(
        evidence
            .recovered_sources(&saved.source, &replacement)
            .unwrap()
            .is_some()
    );
    let current = host.lock().unwrap().chats[&saved.source.id].handle.clone();
    close_terminal_actor(&host, saved.source.id, &current).unwrap();
}

#[test]
fn upgrade_resolution_checks_durable_older_aliases_when_ram_has_only_newer_sources() {
    let (directory, host, mut saved) = upgrade_fixture();
    saved.launch.recipe = saved.launch.terminal_recipe().unwrap();
    restore_upgrade(&host, &saved).unwrap();
    let restored = resolve_upgrade(&host, &saved.source).unwrap();
    let evidence = Arc::new(crate::upgrade::handoff::Store::open(directory.path()).unwrap());
    evidence
        .record_recovered(&[crate::upgrade::handoff::RecoveredChat {
            evidence: saved.clone(),
            replacement: restored.clone(),
            aliases: Vec::new(),
        }])
        .unwrap();
    let mut newer = saved.clone();
    newer.source.proof.as_mut().unwrap().host_incarnation =
        HostIncarnation::new("intermediate-daemon");
    {
        let mut state = host.lock().unwrap();
        state.upgrade_evidence = Some(evidence);
        state.upgrade_chats.insert(
            saved.source.id,
            vec![crate::upgrade::handoff::ChatSource {
                source: newer.source,
                launch: newer.launch,
            }],
        );
    }
    assert_eq!(resolve_upgrade(&host, &saved.source).unwrap(), restored);
    for mutation in 0..4 {
        let mut changed = saved.source.clone();
        match mutation {
            0 => {
                changed.proof.as_mut().unwrap().host_incarnation =
                    HostIncarnation::new("unrelated-daemon")
            }
            1 => {
                changed
                    .launch
                    .recipe
                    .environment
                    .insert("HOME".into(), "/another-account".into());
            }
            2 => changed.launch.session_id = Some("98765432-1234-4234-8234-123456789abc".into()),
            _ => changed
                .launch
                .recipe
                .arguments
                .push("--changed-listener".into()),
        }
        assert!(resolve_upgrade(&host, &changed).is_err());
    }
    let handle = host.lock().unwrap().chats[&saved.source.id].handle.clone();
    handle.close().unwrap();
}

#[test]
fn legacy_codex_upgrade_publishes_issued_arguments_and_hydrates_the_exact_thread() {
    const CHILD: &str = "CAGENTS_LEGACY_CODEX_UPGRADE_CONTRACT";
    let exact = thread::current()
        .name()
        .expect("named owning contract")
        .to_owned();
    if std::env::var(CHILD).as_deref() != Ok(exact.as_str()) {
        use crate::repository::{
            CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor,
        };
        // Keep private evidence on success AND failure; a child result does
        // not establish that every descendant has positively retired.
        let root = tempfile::Builder::new()
            .prefix("cagents-legacy-upgrade-")
            .tempdir_in("/var/tmp")
            .unwrap()
            .keep();
        for name in ["home", "run", "data", "config", "cache"] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir(root.join(name)).unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        eprintln!(
            "legacy upgrade owning evidence retained at {}",
            root.display()
        );
        let command = CommandSpec::new(std::env::current_exe().unwrap())
            .args(["--exact", exact.as_str(), "--nocapture", "--test-threads=1"])
            .env(CHILD, &exact)
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .timeout(Duration::from_secs(35))
            .output_limits(128 * 1024, 128 * 1024);
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        std::fs::write(root.join("stdout.log"), &output.stdout).unwrap();
        std::fs::write(root.join("stderr.log"), &output.stderr).unwrap();
        let output = output.require_success(&command.program).unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.lines().any(|line| line == "running 1 test")
                && stdout
                    .lines()
                    .any(|line| line == format!("test {exact} ... ok"))
                && stdout
                    .lines()
                    .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
            "exact owning contract did not execute; retained evidence {}",
            root.display()
        );
        return;
    }
    use std::os::unix::net::UnixListener;
    let (directory, host, mut saved) = upgrade_fixture();
    let socket = directory.path().join("codex.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let home = directory.path().join("codex-home");
    std::fs::create_dir(&home).unwrap();
    let identity = saved.launch.session_id.clone().unwrap();
    let expected = identity.clone();
    let reported_home = home.clone();
    let (finished, completion) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "exact Codex peer never connected"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        loop {
            assert!(Instant::now() < deadline, "exact Codex peer did not close");
            let message = match socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => break,
            };
            let tungstenite::Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame.get("id").is_none() {
                continue;
            }
            let result = match frame["method"].as_str().unwrap() {
                "initialize" => {
                    serde_json::json!({"userAgent":"cagents/0.1.0", "codexHome":reported_home})
                }
                "thread/resume" => {
                    assert_eq!(frame["params"]["threadId"], expected);
                    serde_json::json!({"thread":{"id":expected,"canAcceptDirectInput":true,"status":{"type":"idle"}},
                        "model":"fixture-luna", "initialTurnsPage":{"data":[{"id":"saved-turn","status":"completed","items":[
                            {"type":"userMessage","id":"saved-prompt","content":[{"type":"text","text":"existing prompt"}]},
                            {"type":"agentMessage","id":"saved-answer","text":"existing exact-thread answer","phase":"final_answer"}
                        ]}],"nextCursor":null}})
                }
                "model/list" | "skills/list" | "thread/list" | "thread/queue/list" => {
                    serde_json::json!({"data":[],"nextCursor":null})
                }
                method => panic!("unexpected fixture method {method}"),
            };
            if socket
                .send(tungstenite::Message::Text(
                    serde_json::json!({"id":frame["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .is_err()
            {
                break;
            }
        }
        let _ = finished.send(());
    });
    saved.source.launch.provider = Provider::Codex;
    saved.source.launch.recipe.program = "codex".into();
    saved.source.launch.recipe.arguments =
        vec!["--remote".into(), format!("unix://{}", socket.display())];
    saved
        .source
        .launch
        .recipe
        .environment
        .insert("CODEX_HOME".into(), home.display().to_string());
    saved.launch = saved.source.launch.clone();
    saved.launch.recipe = saved.launch.terminal_recipe().unwrap();
    assert_ne!(saved.launch.recipe, saved.source.launch.recipe);
    restore_upgrade(&host, &saved).unwrap();
    let (replacement, handle) = {
        let state = host.lock().unwrap();
        let record = &state.chats[&saved.source.id];
        (record.session.clone(), record.handle.clone())
    };
    assert_eq!(replacement.launch.recipe, saved.source.launch.recipe);
    assert_eq!(replacement.launch.session_id, Some(identity));
    assert!(chat::is_upgrade_recovery(&saved.source, &replacement));
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let snapshot = handle.snapshot(None).unwrap();
        if snapshot.ready && !snapshot.history_loading {
            break snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "exact-thread fixture did not become ready: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(snapshot.ready);
    assert!(snapshot.items.iter().any(|item| matches!(&item.content,
        chat::ItemContent::Text { text, .. } if text == "existing exact-thread answer")));
    handle.close().unwrap();
    completion
        .recv_timeout(Duration::from_secs(3))
        .expect("bounded Codex fixture completion");
    server.join().unwrap();
}

#[test]
fn native_restart_cannot_change_the_issued_account_environment() {
    let (_directory, host, mut saved) = upgrade_fixture();
    saved
        .launch
        .recipe
        .environment
        .insert("HOME".into(), "/another/account".into());
    assert_eq!(
        restore_upgrade(&host, &saved).unwrap_err(),
        "Upgrade native source recipe is inconsistent"
    );
    assert!(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .is_empty()
    );
}

struct Echo(bool);
#[test]
fn automatic_handoff_fences_idle_state_and_confirms_the_same_conversation() {
    use std::os::unix::fs::PermissionsExt;
    struct Idle;
    impl Adapter for Idle {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("No work is issued by this fixture".into())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![
                    Event::Ready(true),
                    Event::Session {
                        id: "12345678-1234-4234-8234-123456789abc".into(),
                        title: None,
                    },
                ],
            })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let provider = directory.path().join("claude");
    let conversation = "12345678-1234-4234-8234-123456789abc";
    std::fs::write(&provider, format!(r#"#!/bin/sh
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"{conversation}"}}'
while IFS= read -r frame; do
    request=$(printf '%s' "$frame" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
    if [ -n "$request" ]; then
        printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"%s","response":{{"commands":[],"models":[]}}}}}}\n' "$request"
    fi
done
"#)).unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let original_home = directory.path().join("original");
    let alternate_home = directory.path().join("alternate");
    std::fs::create_dir_all(&alternate_home).unwrap();
    let project: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let transcript = original_home
        .join("projects")
        .join(&project)
        .join(format!("{conversation}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    let proof = HostSessionProof {
        host_session_id: "account-handoff".into(),
        boot_id: "boot".into(),
        host_incarnation: "incarnation".into(),
    };
    let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
        proof.clone(),
    )));
    let source = Session {
        id: TerminalId::new(),
        owned_codex_listener: None,
        native_continuity: Default::default(),
        proof: Some(proof),
        terminal: None,
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: Some(conversation.into()),
            recipe: LaunchRecipe {
                program: provider.to_str().unwrap().into(),
                arguments: Vec::new(),
                working_directory: cwd,
                environment: BTreeMap::from([
                    ("HOME".into(), directory.path().to_str().unwrap().into()),
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        original_home.to_str().unwrap().into(),
                    ),
                    (crate::account::routing::AUTOMATIC_MARKER.into(), "1".into()),
                ]),
                unset_environment: Default::default(),
            },
        },
    };
    let mut process_recipe = source.launch.recipe.clone();
    process_recipe.program = "/bin/cat".into();
    let spec = ProcessSpec::from(&process_recipe);
    let mut initial = Step {
        suppress_current_notification: false,
        notification_preview: None,
        writes: vec![json!({"ready": true})],
        events: vec![
            Event::Ready(true),
            Event::Session {
                id: conversation.into(),
                title: None,
            },
        ],
    };
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(
        spec,
        Box::new(Idle),
        initial,
        store,
        Some(conversation.into()),
        |_| {},
    )
    .unwrap();
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state.chats.insert(
            source.id,
            Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
    }
    let mut replacement = source.clone();
    replacement.id = TerminalId::new();
    replacement.proof = None;
    replacement.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        alternate_home.to_str().unwrap().into(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.confirmed_session_id().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let generation = handle.snapshot(None).unwrap().generation;
    let mut pinned = source.clone();
    pinned
        .launch
        .recipe
        .environment
        .remove(crate::account::routing::AUTOMATIC_MARKER);
    let mut pinned_replacement = replacement.clone();
    pinned_replacement
        .launch
        .recipe
        .environment
        .remove(crate::account::routing::AUTOMATIC_MARKER);
    // Refusals before the close changed nothing, and say so, with the
    // daemon's own sentence.
    let pinned = recover_account(
        &host,
        1,
        pinned,
        pinned_replacement,
        Some(generation),
        Default::default(),
    )
    .unwrap_err();
    assert_eq!(pinned.code, ProtocolErrorCode::NotAdmitted);
    assert_eq!(
        pinned.message,
        "This conversation has an explicitly pinned account"
    );
    assert!(!handle.is_completed());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    // Another start of the same conversation is under way.
    host.lock()
        .unwrap()
        .chat_spawning
        .insert(source.id, source.clone());
    let owned = recover_account(
        &host,
        1,
        source.clone(),
        replacement.clone(),
        Some(generation),
        Default::default(),
    )
    .unwrap_err();
    assert_eq!(owned.code, ProtocolErrorCode::NotAdmitted);
    assert!(owned.message.contains("another live owner"), "{owned:?}");
    assert!(!handle.is_completed());
    host.lock().unwrap().chat_spawning.remove(&source.id);
    // The other account already holds a different copy of this
    // conversation, so its history can't be prepared there.
    let divergent = alternate_home
        .join("projects")
        .join(&project)
        .join(format!("{conversation}.jsonl"));
    std::fs::create_dir_all(divergent.parent().unwrap()).unwrap();
    std::fs::write(&divergent, "{\"other\":true}\n").unwrap();
    let diverged = recover_account(
        &host,
        1,
        source.clone(),
        replacement.clone(),
        Some(generation),
        Default::default(),
    )
    .unwrap_err();
    assert_eq!(diverged.code, ProtocolErrorCode::NotAdmitted);
    assert!(
        diverged.message.contains("could not be prepared"),
        "{diverged:?}"
    );
    assert!(!handle.is_completed());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    std::fs::remove_file(&divergent).unwrap();
    // The close's own refusal is never reported as refused before any
    // effect: once the actor has taken it, the original may be closing.
    let stale = recover_account(
        &host,
        1,
        source.clone(),
        replacement.clone(),
        Some(generation + 1),
        Default::default(),
    )
    .unwrap_err();
    assert_ne!(stale.code, ProtocolErrorCode::NotAdmitted, "{stale:?}");
    assert!(!handle.is_completed());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    let response = recover_account(
        &host,
        1,
        source.clone(),
        replacement.clone(),
        Some(generation),
        Default::default(),
    )
    .unwrap();
    let ServerResponse::ChatAttached { session, snapshot } = response else {
        panic!("Expected confirmed account attachment")
    };
    assert_eq!(session.launch.session_id.as_deref(), Some(conversation));
    assert_eq!(snapshot.session_id.as_deref(), Some(conversation));
    assert_eq!(
        std::fs::read(
            alternate_home
                .join("projects")
                .join(project)
                .join(format!("{conversation}.jsonl"))
        )
        .unwrap(),
        b"{}\n"
    );
    assert!(handle.is_completed());
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    process(
        &host,
        1,
        ClientRequest::ChatClose {
            chat_id: session.id,
        },
    )
    .unwrap();
}

#[test]
fn account_recovery_requires_provider_confirmation_not_the_resume_hint() {
    struct Confirm;
    impl Adapter for Confirm {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({"id":"saved-conversation"})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, value: Value) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![Event::Session {
                    id: value["id"].as_str().unwrap().to_owned(),
                    title: None,
                }],
            })
        }
    }
    let recipe = LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(
        spec,
        Box::new(Confirm),
        initial,
        store,
        Some("saved-conversation".into()),
        |_| {},
    )
    .unwrap();
    assert_eq!(handle.session_id().as_deref(), Some("saved-conversation"));
    assert!(
        confirm_saved_conversation(&handle, "saved-conversation", false, Instant::now(), || {
            false
        })
        .is_err()
    );
    handle.command(Command::Interrupt).unwrap();
    confirm_saved_conversation(
        &handle,
        "saved-conversation",
        false,
        Instant::now() + Duration::from_secs(5),
        || false,
    )
    .unwrap();
    // A confirmed identity alone is not a provider that answered initialize.
    assert!(
        confirm_saved_conversation(
            &handle,
            "saved-conversation",
            true,
            Instant::now() + Duration::from_millis(100),
            || false
        )
        .is_err()
    );
    assert!(
        confirm_saved_conversation(
            &handle,
            "another-conversation",
            false,
            Instant::now(),
            || false
        )
        .is_err()
    );
    assert!(
        confirm_saved_conversation(&handle, "saved-conversation", false, Instant::now(), || {
            true
        })
        .is_err()
    );
    handle.close().unwrap();
}

struct ActiveFixture;
impl Adapter for ActiveFixture {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        Ok(Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: Vec::new(),
            events: if matches!(command, Command::Interrupt) {
                vec![Event::Status {
                    busy: false,
                    detail: String::new(),
                }]
            } else {
                Vec::new()
            },
        })
    }
    fn receive(&mut self, _: Value) -> Result<Step, String> {
        Ok(Step::default())
    }
}
#[test]
fn client_listener_evidence_cannot_claim_an_unrelated_root() {
    let allocator = HostSessionProof {
        host_session_id: "allocator".into(),
        boot_id: "boot".into(),
        host_incarnation: "previous".into(),
    };
    let current = HostSessionProof {
        host_incarnation: "current".into(),
        ..allocator.clone()
    };
    let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
        current.clone(),
    )));
    let mut session = Session {
        id: TerminalId::new(),
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                unset_environment: Default::default(),
                program: "codex".into(),
                arguments: vec!["--remote".into(), "unix://".into()],
                working_directory: "/".into(),
                environment: BTreeMap::new(),
            },
            session_id: None,
        },
        proof: Some(current.clone()),
        terminal: None,
    };
    let root = std::path::Path::new("/daemon-root");
    assert!(session.claim_owned_codex_listener(root, &allocator));
    session.launch.session_id = Some("12345678-1234-4234-8234-123456789abc".into());
    // Inventory must retain allocation evidence while the actor starts.
    host.lock()
        .unwrap()
        .chat_spawning
        .insert(session.id, session.clone());
    let ServerResponse::ChatListed { sessions } =
        process(&host, 2, ClientRequest::ChatList).unwrap()
    else {
        panic!("chat inventory")
    };
    assert_eq!(sessions, vec![session.clone()]);
    host.lock().unwrap().codex_listeners = CodexListenerRoots::at(root.into());
    let validated = client_session(&host, session.clone());
    assert_eq!(validated.owned_codex_listener, session.owned_codex_listener);
    assert_eq!(
        validated.owned_codex_listener.as_ref().unwrap().allocator,
        allocator
    );

    host.lock().unwrap().codex_listeners = CodexListenerRoots::at("/another-daemon-root".into());
    let untrusted = client_session(&host, session.clone());
    assert!(untrusted.owned_codex_listener.is_none());
    assert_eq!(untrusted.launch.recipe, session.launch.recipe);
    assert!(!untrusted.owns_codex_listener());

    // A daemon-owned checkpoint carries the original allocation across
    // both proof rotation and a changed daemon temporary root.
    let mut checkpointed: Session =
        serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
    assert!(
        checkpointed
            .claim_owned_codex_listener(std::path::Path::new("/replacement-root"), &current)
    );
    assert_eq!(
        checkpointed.owned_codex_listener,
        session.owned_codex_listener
    );
    assert_eq!(checkpointed.launch.recipe, session.launch.recipe);
}

#[test]
fn managed_cli_keeps_the_active_actor_and_close_retires_both_views() {
    let (_directory, host, _) = upgrade_fixture();
    let chat_id = TerminalId::new();
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step {
        suppress_current_notification: false,
        notification_preview: None,
        writes: Vec::new(),
        events: vec![Event::Status {
            busy: true,
            detail: "working".into(),
        }],
    };
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let identity = "12345678-1234-4234-8234-123456789abc";
    let handle = Handle::start(
        spec,
        Box::new(ActiveFixture),
        initial,
        store,
        Some(identity.into()),
        |_| {},
    )
    .unwrap();
    let mut session = Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: chat_id,
        proof: None,
        terminal: None,
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: recipe.clone(),
            session_id: Some(identity.into()),
        },
    };
    session.launch.recipe.arguments = vec![
        "--remote".into(),
        format!(
            "unix://{}/cagents-codex-{chat_id}/control.sock",
            std::env::temp_dir().display()
        ),
    ];
    assert!(
        session.claim_owned_codex_listener(&std::env::temp_dir(), host.lock().unwrap().proof())
    );
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state.chats.insert(
            chat_id,
            Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: session.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
    }
    host.lock().unwrap().codex_listeners = CodexListenerRoots::at("/another-root".into());
    assert_eq!(
        client_session(&host, session.clone()).owned_codex_listener,
        session.owned_codex_listener
    );
    let view = |client, session, recipe| {
        let (owners, closing) = {
            let mut state = host.lock().unwrap();
            let owners = paired_close::Owners::capture(&state, &state.chats[&chat_id])?;
            let closing = capacity::Reservation::closing(&host, &mut state, &owners.target.source)?;
            (owners, closing)
        };
        codex_terminal_view(&host, client, chat_id, owners, &closing, session, recipe)
    };
    let first = view(1, session.clone(), recipe.clone()).unwrap();
    let ServerResponse::ChatTerminal {
        session: terminal,
        snapshot,
        ..
    } = first
    else {
        panic!("CLI view missing")
    };
    assert!(!snapshot.exited);
    assert!(!handle.is_completed());
    let worker = host.lock().unwrap().terminals[&terminal.id].handle.clone();
    let second = view(1, session.clone(), recipe.clone()).unwrap();
    assert!(matches!(second, ServerResponse::ChatTerminal { snapshot, .. } if !snapshot.exited));
    assert!(
        host.lock().unwrap().terminals[&terminal.id]
            .handle
            .same_worker(&worker)
    );
    worker.shutdown().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(
        host.lock()
            .unwrap()
            .descriptor(1, terminal.id)
            .unwrap()
            .session
            .continuity,
        crate::model::TerminalContinuity::RelaunchAvailable { .. }
    ) {
        assert!(Instant::now() < deadline, "terminal exit was not published");
        thread::sleep(Duration::from_millis(10));
    }
    let mut retained = {
        let mut state = host.lock().unwrap();
        state.register_client(2);
        let mut retained = state.chats[&chat_id].session.clone();
        retained.proof = Some(state.proof().clone());
        retained
    };
    let valid_proof = retained.proof.take();
    assert!(
        start(
            &host,
            2,
            retained.clone(),
            None,
            false,
            None,
            Default::default()
        )
        .is_err()
    );
    retained.proof = valid_proof;
    assert!(matches!(
        start(&host, 2, retained, None, false, None, Default::default()).unwrap(),
        ServerResponse::ChatAttached { .. }
    ));
    assert!(
        host.lock().unwrap().chats[&chat_id]
            .handle
            .same_actor(&handle)
    );
    view(2, session, recipe).unwrap();
    let replacement = host.lock().unwrap().terminals[&terminal.id].handle.clone();
    assert!(!replacement.same_worker(&worker));
    // A recreated terminal can have the same public ID, launch recipe and
    // daemon proof. A stale private pairing must not authorize its close.
    host.lock()
        .unwrap()
        .chats
        .get_mut(&chat_id)
        .unwrap()
        .paired_terminal = Some(worker.clone());
    assert!(process(&host, 2, ClientRequest::ChatClose { chat_id }).is_err());
    assert!(!replacement.is_completed());
    assert!(host.lock().unwrap().chats.contains_key(&chat_id));
    host.lock()
        .unwrap()
        .chats
        .get_mut(&chat_id)
        .unwrap()
        .paired_terminal = Some(replacement.clone());
    {
        let mut state = host.lock().unwrap();
        let terminal = state.terminals.get_mut(&terminal.id).unwrap();
        let mut observed = terminal.session.launch.clone();
        observed.program = "codex".into();
        observed.arguments = vec!["resume".into(), identity.into()];
        terminal.session.resume_hint = crate::model::agent_recovery::hint_from_launch(&observed);
        assert!(terminal.session.resume_hint.is_some());
    }
    process(&host, 2, ClientRequest::ChatClose { chat_id }).unwrap();
    assert!(handle.is_completed());
    assert!(worker.is_completed());
    assert!(replacement.is_completed());
    assert!(!host.lock().unwrap().terminals.contains_key(&terminal.id));
    assert!(!host.lock().unwrap().chats.contains_key(&chat_id));
}
impl Adapter for Echo {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, _: Command) -> Result<Step, String> {
        Ok(Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: vec![json!({"tick": true})],
            events: Vec::new(),
        })
    }
    fn receive(&mut self, _: Value) -> Result<Step, String> {
        self.0 = !self.0;
        Ok(Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: Vec::new(),
            events: vec![Event::Waiting(self.0)],
        })
    }
}

/// Closing a live chat reports its teardown - the provider's exit is an
/// activity change - to every attached client, the closer included, before
/// the close is answered. The control client attaches only to close, and
/// must still see the close succeed and its connection stay usable.
#[test]
fn a_control_client_closes_a_live_chat_it_attached_to() {
    control_client_closes_chat(false, false);
}

#[test]
fn a_control_client_waits_for_slow_provider_interruption_before_closing() {
    control_client_closes_chat(true, false);
}

#[test]
fn returning_to_terminal_waits_for_provider_interruption() {
    control_client_closes_chat(true, true);
}

fn control_client_closes_chat(slow: bool, return_terminal: bool) {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("claude");
    std::fs::write(&cli, "#!/bin/sh\nexec cat\n").unwrap();
    std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    struct Interruptible;
    impl Adapter for Interruptible {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            assert_eq!(command, Command::Interrupt);
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({"interrupt": true})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, value: Value) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![if value.get("ready").is_some() {
                    Event::Session {
                        id: "12345678-1234-4234-8234-123456789abc".into(),
                        title: None,
                    }
                } else {
                    Event::Status {
                        busy: false,
                        detail: "Interrupted".into(),
                    }
                }],
            })
        }
    }
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("control-close"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
        proof.clone(),
    )));
    let chat_id = TerminalId::new();
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: if slow { "/bin/sh" } else { "/bin/cat" }.into(),
        arguments: if slow {
            vec![
                "-c".into(),
                "printf '{\"ready\":true}\\n'; read -r interrupt; sleep 6; printf '{\"done\":true}\\n'; exec cat".into(),
            ]
        } else {
            Vec::new()
        },
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    if slow {
        initial.events.push(Event::Status {
            busy: true,
            detail: "Working".into(),
        });
    }
    if return_terminal {
        initial.events.push(Event::Session {
            id: "12345678-1234-4234-8234-123456789abc".into(),
            title: None,
        });
        initial.events.push(Event::Request(chat::Request {
            kind: crate::chat::RequestKind::Approval,
            id: "pending-approval".into(),
            title: "Edit".into(),
            description: "Waiting for approval".into(),
            fields: Vec::new(),
            actions: Vec::new(),
        }));
    }
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let weak = Arc::downgrade(&host);
    let handle = Handle::start(
        spec,
        if slow {
            Box::new(Interruptible) as Box<dyn Adapter>
        } else {
            Box::new(Echo(false))
        },
        initial,
        store,
        return_terminal.then(|| "12345678-1234-4234-8234-123456789abc".into()),
        move |update| changed(&weak, chat_id, update),
    )
    .unwrap();
    if return_terminal {
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.confirmed_session_id().is_none() {
            assert!(
                Instant::now() < deadline,
                "provider did not confirm identity"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    let session = Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: chat_id,
        launch: chat::Launch {
            provider: if return_terminal {
                Provider::Claude
            } else {
                Provider::Codex
            },
            recipe: if return_terminal {
                LaunchRecipe {
                    program: cli.to_string_lossy().into_owned(),
                    arguments: Vec::new(),
                    ..recipe
                }
            } else {
                recipe
            },
            session_id: None,
        },
        proof: Some(proof),
        terminal: None,
    };
    host.lock().unwrap().chats.insert(
        chat_id,
        Record {
            execution_instance: uuid::Uuid::nil(),
            console_recipe: None,
            paired_terminal: None,
            session: session.clone(),
            handle: handle.clone(),
            source_process: None,
            subscribers: if return_terminal {
                BTreeMap::from([(1, true)])
            } else {
                BTreeMap::new()
            },
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    let socket = directory.path().join("cagentsd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server_host = Arc::clone(&host);
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        super::super::client::serve(stream, 1, server_host);
    });

    let mut client = crate::control::ControlClient::connect(&socket).unwrap();
    let started = Instant::now();
    if return_terminal {
        host.lock().unwrap().install_model(
            super::super::model::Registry::at(&directory.path().join("registry.sqlite3")).unwrap(),
        );
        host.lock().unwrap().register_client(1);
        let ServerResponse::ChatTerminal {
            session: terminal, ..
        } = process(&host, 1, ClientRequest::ChatReturnTerminal { chat_id }).unwrap()
        else {
            panic!("chat did not return to terminal")
        };
        // The terminal takes over the chat's lineage and its agents.
        assert_eq!(
            super::super::agent_execution::registry(&host)
                .unwrap()
                .lock()
                .unwrap()
                .session_lineage(terminal.id)
                .unwrap(),
            Some(crate::agent_execution::Lineage(chat_id))
        );
        assert!(
            terminal
                .launch
                .arguments
                .windows(2)
                .any(|args| args == ["--resume", "12345678-1234-4234-8234-123456789abc"])
        );
        let replacement = host.lock().unwrap().terminals.remove(&terminal.id).unwrap();
        let _ = replacement.handle.shutdown();
    } else {
        client.close_chat(session).unwrap();
    }
    if slow {
        assert!(started.elapsed() >= Duration::from_secs(6));
    }
    assert!(handle.is_completed());
    assert!(client.list_chats().unwrap().is_empty());
    drop(client);
    server.join().unwrap();
}

/// Every surface showing a conversation hears how the provider addresses
/// its subagents as each is learned, and one attaching later is told
/// those already known.
#[test]
fn agent_addresses_reach_every_subscriber_and_each_new_one() {
    struct Addresser;
    impl Adapter for Addresser {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({"tick": true})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![Event::AgentAddress(chat::AgentAddress {
                    address: "a1892b93e88999a7d".into(),
                    subagent: "toolu_relocate".into(),
                })],
            })
        }
    }
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("agent-addresses"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
        proof.clone(),
    )));
    let chat_id = TerminalId::new();
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let weak = Arc::downgrade(&host);
    let handle = Handle::start(
        spec,
        Box::new(Addresser),
        initial,
        store,
        None,
        move |update| changed(&weak, chat_id, update),
    )
    .unwrap();
    let queue = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let queue = state.register_client(1);
        state.chats.insert(
            chat_id,
            Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: Session {
                    native_continuity: Default::default(),
                    owned_codex_listener: None,
                    id: chat_id,
                    launch: chat::Launch {
                        provider: Provider::Claude,
                        recipe,
                        session_id: None,
                    },
                    proof: Some(proof),
                    terminal: None,
                },
                handle: handle.clone(),
                source_process: None,
                // Hidden, as most surfaces showing it are.
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        queue
    };
    assert!(
        agent_addresses(&host, chat_id).is_none(),
        "a conversation that knows none tells a new subscriber nothing"
    );
    let (heard, hearing) = crossbeam_channel::unbounded();
    let reader = thread::spawn({
        let queue = queue.clone();
        move || {
            while let Some(outbound) = queue.pop() {
                if let ServerMessage::ChatAgentAddresses { chat_id, addresses } = outbound.message {
                    let _ = heard.send((chat_id, addresses));
                }
            }
        }
    });
    handle.command(Command::Interrupt).unwrap();
    let expected = vec![chat::AgentAddress {
        address: "a1892b93e88999a7d".into(),
        subagent: "toolu_relocate".into(),
    }];
    assert_eq!(
        hearing.recv_timeout(Duration::from_secs(5)).unwrap(),
        (chat_id, expected.clone())
    );
    let Some(ServerMessage::ChatAgentAddresses { addresses, .. }) = agent_addresses(&host, chat_id)
    else {
        panic!("a new subscriber is told the addresses known");
    };
    assert_eq!(addresses, expected);
    handle.close().unwrap();
    queue.close_after_drain();
    reader.join().unwrap();
}

#[test]
fn delayed_producer_callback_after_rearm_is_fenced_without_reads() {
    acknowledgement_race(false);
}

#[test]
fn delayed_producer_callback_before_rearm_cannot_consume_new_acknowledgement() {
    acknowledgement_race(true);
}

fn acknowledgement_race(before_rearm: bool) {
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("chat-observation"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(HostState::new(
        proof.clone(),
    )));
    let chat_id = TerminalId::new();
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let (observed, pending) = crossbeam_channel::bounded(1);
    let (release, released) = crossbeam_channel::bounded(1);
    let (delivered, completed) = crossbeam_channel::bounded(1);
    let weak = Arc::downgrade(&host);
    let handle = Handle::start(
        spec,
        Box::new(Echo(false)),
        initial,
        store,
        None,
        move |update| {
            let generation = match update {
                Update::Activity { generation, .. } if before_rearm => Some(generation),
                Update::Observed { generation } if !before_rearm => Some(generation),
                _ => None,
            };
            if let Some(generation) = generation {
                // Pause on either side of consuming the producer rearm, always
                // after the semantic event and before its unread host callback.
                if observed.send(generation).is_ok() {
                    let _ = released.recv_timeout(Duration::from_secs(5));
                }
            }
            let observed = matches!(update, Update::Observed { .. });
            changed(&weak, chat_id, update);
            if observed {
                let _ = delivered.try_send(());
            }
        },
    )
    .unwrap();
    let (first, second) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let first = state.register_client(1);
        let second = state.register_client(2);
        state.chats.insert(
            chat_id,
            Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: Session {
                    native_continuity: Default::default(),
                    owned_codex_listener: None,
                    id: chat_id,
                    launch: chat::Launch {
                        provider: Provider::Claude,
                        recipe,
                        session_id: None,
                    },
                    proof: Some(proof),
                    terminal: None,
                },
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false), (2, false)]),
                observations: BTreeMap::from([
                    (1, Observation::default()),
                    (2, Observation::default()),
                ]),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        (first, second)
    };
    handle.command(Command::Interrupt).unwrap();
    let old_generation = pending.recv_timeout(Duration::from_secs(2)).unwrap();
    // This must not deadlock on the producer's snapshot lock or hydrate any
    // transcript. Only subscriber 1 acknowledges the already-produced event.
    process(
        &host,
        1,
        ClientRequest::ChatAcknowledge { chat_id, epoch: 1 },
    )
    .unwrap();
    release.send(()).unwrap();
    completed.recv_timeout(Duration::from_secs(2)).unwrap();
    handle.command(Command::Interrupt).unwrap();
    let fresh_generation = pending.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(fresh_generation > old_generation);
    // Reordered and duplicate ACKs cannot advance the source cutoff to this
    // fresh event, nor clear a subscriber's already-delivered observation.
    for epoch in [1, 0] {
        process(&host, 1, ClientRequest::ChatAcknowledge { chat_id, epoch }).unwrap();
    }
    release.send(()).unwrap();
    completed.recv_timeout(Duration::from_secs(2)).unwrap();
    for epoch in [1, 0] {
        process(&host, 1, ClientRequest::ChatAcknowledge { chat_id, epoch }).unwrap();
    }
    changed(
        &Arc::downgrade(&host),
        chat_id,
        Update::Observed {
            generation: fresh_generation + 1,
        },
    );
    // Closing the queues makes missing or surplus observations deterministic.
    first.close_after_drain();
    second.close_after_drain();
    let drain = |queue: &super::super::delivery::DeliveryQueue| {
        let mut epochs = Vec::new();
        while let Some(envelope) = queue.pop() {
            match envelope.message {
                ServerMessage::ChatObserved { epoch, .. } => epochs.push(epoch),
                ServerMessage::ChatActivity { .. } => {}
                other => panic!("hidden acknowledgement requested presentation: {other:?}"),
            }
        }
        epochs
    };
    assert_eq!(
        drain(&first),
        vec![1],
        "old callback or duplicate ACK resurrected unread"
    );
    assert_eq!(
        drain(&second),
        vec![0],
        "one subscriber's ACK must not rearm another"
    );
    assert!(
        process(
            &host,
            1,
            ClientRequest::ChatRead {
                chat_id,
                after_generation: None
            }
        )
        .is_err()
    );
    process(&host, 1, ClientRequest::ChatDetach { chat_id }).unwrap();
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.unregister_client(2);
        let record = state.chats.get(&chat_id).unwrap();
        assert!(record.subscribers.is_empty());
        assert!(record.observations.is_empty());
    }
    drop(pending);
    drop(release);
    handle.close().unwrap();
}
