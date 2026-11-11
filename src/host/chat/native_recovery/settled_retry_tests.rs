//! Retrying an authenticated original actor after a saved unavailable snapshot.
use super::*;
use crate::{
    chat::runtime::ActorIdentity,
    model::{HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalSession},
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MODE: &str = "CAGENTS_SETTLED_NATIVE_RETRY_TEST";

fn isolated(matched_executable: bool) -> Option<PathBuf> {
    if std::env::var_os(MODE).is_some() {
        return Some(PathBuf::from(
            std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap(),
        ));
    }
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = directory.path();
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
    fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
    let exact = thread::current().name().unwrap().to_owned();
    let stdout = root.join("test.stdout");
    let stderr = root.join("test.stderr");
    let executable = if matched_executable {
        // The real private Unix peer below must identify the actual serving
        // executable, so execute this same test ELF from a managed matched root.
        // The siblings are inert descriptors and are never run.
        let prefix = root.join("install");
        let bin = prefix.join("versions/fixture-cold/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            prefix.join(".cagents-install-root"),
            b"cagents install root\n",
        )
        .unwrap();
        let executable = bin.join("cagentsd");
        let current = std::env::current_exe().unwrap();
        // This must be an independent file: stripping a hard link would edit
        // the built test executable. Do this only before any exec/hash/record.
        fs::copy(&current, &executable).unwrap();
        match std::process::Command::new("/usr/bin/strip")
            .args(["--strip-debug", "--"])
            .arg(&executable)
            .status()
        {
            Ok(status) => assert!(status.success(), "could not strip private fixture ELF"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not run strip on private fixture ELF: {error}"),
        }
        for sibling in ["cagents", "cagents-ui"] {
            fs::write(bin.join(sibling), b"inert matched fixture sibling").unwrap();
        }
        executable
    } else {
        std::env::current_exe().unwrap()
    };
    // run_isolated's separate subreaper owns this complete process family.
    // If the bounded child is killed below, unwinding the worker makes that
    // supervisor kill and reap the cold host and setsid provider descendants.
    assert!(crate::retained_fixture::owner_env().is_some());
    let mut child = std::process::Command::new(executable)
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env_clear()
        .envs(crate::retained_fixture::owner_env())
        .envs(
            std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                .map(|value| (crate::agent_execution::AUTHORITY_ENV, value)),
        )
        .env(MODE, "1")
        .env(
            "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
            super::super::retained_actor_tests::retained_helper_executable(),
        )
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_HOLDER_ROOT", root)
        .env("HOME", root.join("home"))
        .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    // The measured first archive-qualified hop spends about 195 seconds on
    // production SHA/census checks over the stripped 245 MB test ELF in this
    // unoptimized test lane. Bound all three physical hops, setup and teardown
    // together; the separate subreaper has another 120 seconds of margin.
    let deadline = Instant::now() + Duration::from_secs(if matched_executable { 900 } else { 90 });
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.is_some_and(|status| status.success()),
        "isolated native retry fixture failed:\n{}\n{}",
        fs::read_to_string(&stdout).unwrap(),
        fs::read_to_string(&stderr).unwrap()
    );
    if matched_executable {
        eprintln!("{}", fs::read_to_string(&stderr).unwrap());
    }
    None
}

fn wait(mut condition: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "{reason}");
        thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture {
    root: PathBuf,
    proof: HostSessionProof,
    lease: Option<crate::daemon::HostLease>,
    host: Option<SharedHost>,
    broker: Option<crate::host::notifications::Broker>,
    owners: Vec<ActorIdentity>,
}

impl Fixture {
    fn new(root: PathBuf) -> Self {
        let provider = root.join("bin/claude");
        super::super::retained_actor_tests::install_provider(&provider);
        // This private provider receives only this fixture's explicit seed
        // commands. Record metadata, not message bodies, to distinguish those
        // original turns from any replay during later host adoption.
        let script = fs::read_to_string(&provider).unwrap();
        let marker = "        with (root/'prompts').open('a') as log: log.write('prompt\\n')";
        assert_eq!(script.matches(marker).count(), 1);
        let receipt = r#"        with (root/'prompt-receipts').open('a') as log: log.write(json.dumps({'pid':os.getpid(),'session':session,'uuid':frame['uuid'],'type':frame.get('type'),'seed':frame.get('message')=={'role':'user','content':[{'type':'text','text':"Confirm this private recovery fixture's conversation"}]}})+'\n')"#;
        fs::write(
            &provider,
            script.replace(marker, &format!("{marker}\n{receipt}")),
        )
        .unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("settled-native-retry"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("original"),
        };
        let mut fixture = Self {
            root,
            proof,
            lease: None,
            host: None,
            broker: None,
            owners: Vec::new(),
        };
        fixture.install_host();
        fixture
    }

    fn install_host(&mut self) {
        self.lease = Some(
            crate::daemon::HostLease::acquire(
                &crate::daemon::RuntimePaths::discover().unwrap(),
                &self.proof,
            )
            .unwrap(),
        );
        let mut state = HostState::new(self.proof.clone());
        state.enable_holder_recovery();
        state.register_client(0);
        state.install_model(crate::host::model::Registry::open().unwrap());
        let host = Arc::new(HostLock::new(state));
        let broker = crate::host::notifications::Broker::start(
            &host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        host.lock().unwrap().notifications = Some(broker.handle());
        self.host = Some(host);
        self.broker = Some(broker);
    }

    fn host(&self) -> &SharedHost {
        self.host.as_ref().unwrap()
    }

    fn start(&mut self) -> Session {
        let source = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            self.root.join("project"),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        let id = source.id;
        start_execution(
            self.host(),
            0,
            source,
            Arc::new(
                chat::protocol::ExecutionContext::capture(&self.root.join("project")).unwrap(),
            ),
        )
        .unwrap();
        let handle = self.host().lock().unwrap().chats[&id].handle.clone();
        self.owners.push(handle.actor_identity().unwrap());
        wait(
            || handle.attachment_snapshot().ready,
            "fake retained provider did not initialize",
        );
        // Claude confirms its prepared UUID on its first real turn. Seed that
        // ordinary wire exchange before taking the no-replay baseline; a ready
        // pre-ID helper alone cannot establish resumable checkpoint aliases.
        handle
            .command(chat::Command::Prompt {
                text: "Confirm this private recovery fixture's conversation".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            })
            .unwrap();
        wait(
            || {
                handle.confirmed_session_id().is_some()
                    && handle.is_idle()
                    && handle
                        .uncommitted_outcomes()
                        .is_ok_and(|outcomes| outcomes.is_empty())
            },
            "fake provider turn and semantic acknowledgement did not settle",
        );
        let confirmed = handle.confirmed_session_id().unwrap();
        uuid::Uuid::parse_str(&confirmed).expect("fixture must report a real provider UUID");
        let ServerResponse::ChatListed { sessions } =
            super::super::process(self.host(), 0, ClientRequest::ChatList).unwrap()
        else {
            panic!("wrong public chat list response")
        };
        let issued = sessions
            .into_iter()
            .find(|session| session.id == id)
            .unwrap();
        let ServerResponse::ChatAttached { session, snapshot } = super::super::process(
            self.host(),
            0,
            ClientRequest::ChatAttach { session: issued },
        )
        .unwrap() else {
            panic!("wrong public chat attach response")
        };
        assert_eq!(
            session.launch.session_id.as_deref(),
            Some(confirmed.as_str())
        );
        assert_eq!(snapshot.session_id.as_deref(), Some(confirmed.as_str()));
        assert!(matches!(
            session.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        ));
        session
    }

    fn leave_host(&mut self) {
        let terminals = self
            .host()
            .lock()
            .unwrap()
            .terminals
            .values()
            .map(|record| record.handle.clone())
            .collect::<Vec<_>>();
        for terminal in &terminals {
            terminal.preserve_for_restart().unwrap();
        }
        drop(terminals);
        let handles: Vec<_> = self
            .host()
            .lock()
            .unwrap()
            .chats
            .values()
            .map(|record| record.handle.clone())
            .collect();
        for handle in &handles {
            if handle.owns_work() {
                handle.preserve_for_restart().unwrap();
            }
        }
        drop(handles);
        drop(self.broker.take());
        drop(self.host.take());
        drop(self.lease.take());
    }

    fn rotate(&mut self, incarnation: &str) {
        self.leave_host();
        self.proof.host_incarnation = HostIncarnation::new(incarnation);
        self.install_host();
        assert!(
            self.owners.iter().all(|owner| {
                owner.record.root.is_current() && owner.record.holder.is_current()
            })
        );
    }

    fn save_paused(&self, source: Session) {
        let mut record = crate::upgrade::floor::Record::new(
            source.proof.clone().unwrap(),
            vec![source],
            Vec::new(),
        );
        record.protocol = PROTOCOL_VERSION;
        record.service_stopped = true;
        record.save().unwrap();
    }

    fn prepare_ready_checkpoint(
        &self,
    ) -> (
        crate::upgrade::handoff::Checkpoint,
        crate::upgrade::handoff::Checkpoint,
    ) {
        let control_gate = self.host().lock().unwrap().control_gate.clone();
        let control = control_gate.write().unwrap();
        let capability = self
            .host()
            .lock()
            .unwrap()
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        crate::host::state::prepare_upgrade(self.host(), capability, false).unwrap();
        drop(control);
        let store = crate::upgrade::handoff::Store::discover().unwrap();
        let prepared = store.load().unwrap().unwrap();
        assert_eq!(prepared.phase, crate::upgrade::handoff::Phase::Prepared);
        crate::host::finish_chat_shutdown(self.host()).unwrap();
        let ready = store.load().unwrap().unwrap();
        ready.require_ready().unwrap();
        (prepared, ready)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(host) = &self.host {
            let terminals = host
                .lock()
                .unwrap()
                .terminals
                .values()
                .map(|record| record.handle.clone())
                .collect::<Vec<_>>();
            for terminal in terminals {
                let _ = terminal.close_checked(true);
            }
        }
        for owner in &self.owners {
            let closed = Handle::inspect_remote(&owner.record, self.proof.clone())
                .and_then(|inspection| {
                    Handle::adopt_remote(
                        &owner.record,
                        inspection.source,
                        self.proof.clone(),
                        |_| {},
                    )
                })
                .and_then(|handle| {
                    handle.close()?;
                    handle.finish_retained_close()
                });
            if closed.is_ok() {
                continue;
            }
            // Failure containment is scoped to these exact private process
            // incarnations; it is not evidence that normal teardown succeeded.
            for process in [&owner.record.root, &owner.record.holder] {
                if process.is_current() {
                    unsafe {
                        libc::kill(process.pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

#[test]
fn preserved_retry_authenticates_original_native_owner_without_reopening_paused_chat() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    let Some(root) = isolated(false) else { return };
    let mut fixture = Fixture::new(root);
    let original_retry = fixture.start();
    let originally_paused = fixture.start();
    // A public descriptor projects the confirmed UUID; an untouched helper
    // still owns the exact pre-ID descriptor issued at its first installation.
    // Retain that real source separately instead of fabricating equality.
    let paused_original =
        Handle::inspect_remote(&fixture.owners[1].record, fixture.proof.clone()).unwrap();
    assert_eq!(
        paused_original.state.confirmed_session,
        originally_paused.launch.session_id
    );
    fixture.save_paused(originally_paused.clone());
    let launches = fs::read(fixture.root.join("launches")).unwrap();
    assert_eq!(String::from_utf8_lossy(&launches).lines().count(), 2);
    let prompts = fs::read(fixture.root.join("prompts")).unwrap();
    assert_eq!(String::from_utf8_lossy(&prompts).lines().count(), 2);

    // The first cold owner genuinely adopts this helper. The other ID was
    // explicitly paused for the update and never belongs to the retry set.
    fixture.rotate("historical-adopter");
    restore_orphaned_native(fixture.host(), &[]).unwrap();
    let retry = {
        let state = fixture.host().lock().unwrap();
        assert_eq!(state.chats.len(), 1);
        assert!(!state.chats.contains_key(&originally_paused.id));
        let record = &state.chats[&original_retry.id];
        assert_eq!(
            record.handle.actor_identity(),
            Some(fixture.owners[0].clone())
        );
        assert_eq!(record.session.proof.as_ref(), Some(&fixture.proof));
        record.session.clone()
    };
    // An unavailable snapshot from a failed later restart adds the adopted
    // ID to the same floor that ordinarily excludes explicitly paused work.
    fixture.save_paused(retry.clone());
    fixture.rotate("cold-retry");
    let stopped = crate::upgrade::floor::load().unwrap();
    assert!(stopped.blocks_id(retry.id) && stopped.blocks_id(originally_paused.id));
    restore_orphaned_native(fixture.host(), &[]).unwrap();
    assert!(fixture.host().lock().unwrap().chats.is_empty());
    for source in [&retry, &originally_paused] {
        let refusal = restore(fixture.host(), 0, source).unwrap_err();
        assert!(refusal.contains("stopped for the update"), "{refusal}");
    }
    let preserved = BTreeSet::from([retry.id]);
    restore_orphaned_native_preserving(fixture.host(), &[retry.id], &preserved).unwrap();
    assert!(
        fixture.host().lock().unwrap().chats.is_empty(),
        "an already-refused owner must not be retried in the same pass"
    );
    restore_orphaned_native_preserving(fixture.host(), &[], &preserved).unwrap();
    let adopted = {
        let state = fixture.host().lock().unwrap();
        assert_eq!(state.chats.len(), 1);
        assert!(!state.chats.contains_key(&originally_paused.id));
        let record = &state.chats[&retry.id];
        assert_eq!(
            record.handle.actor_identity(),
            Some(fixture.owners[0].clone())
        );
        assert_eq!(record.session.proof.as_ref(), Some(&fixture.proof));
        record.session.clone()
    };
    let authenticated =
        Handle::inspect_remote(&fixture.owners[0].record, fixture.proof.clone()).unwrap();
    assert_eq!(
        authenticated.source, adopted,
        "the original helper must acknowledge the new lease's issued source"
    );
    let paused = Handle::inspect_remote(&fixture.owners[1].record, fixture.proof.clone()).unwrap();
    assert_eq!(paused.identity, paused_original.identity);
    assert_eq!(paused.source, paused_original.source);
    assert_eq!(
        paused.state.confirmed_session,
        originally_paused.launch.session_id
    );
    assert_ne!(paused.source.proof.as_ref(), Some(&fixture.proof));
    assert_eq!(fs::read(fixture.root.join("launches")).unwrap(), launches);
    assert_eq!(
        fs::read(fixture.root.join("prompts")).unwrap(),
        prompts,
        "native retry must neither launch a replacement nor replay provider input"
    );

    // A later ordinary update now has a real Ready checkpoint from the daemon
    // that owns the retried actor. Its older unavailable snapshot still exists,
    // but cannot prevent the next daemon from authenticating this exact owner.
    let (prepared, ready) = fixture.prepare_ready_checkpoint();
    assert_eq!(ready.chats.len(), 1);
    assert_eq!(ready.chats[0].source, adopted);
    fixture.rotate("ready-successor");
    assert!(crate::upgrade::floor::load().unwrap().blocks_id(adopted.id));
    assert!(restore(fixture.host(), 0, &adopted).is_err());
    assert!(restore_ready_checkpoint(fixture.host(), &adopted, &prepared).is_err());
    let mut restoring = ready.clone();
    restoring.phase = crate::upgrade::handoff::Phase::Restoring;
    assert!(restore_ready_checkpoint(fixture.host(), &adopted, &restoring).is_err());
    let mut absent = ready.clone();
    absent.chats.clear();
    absent.require_ready().unwrap();
    assert!(restore_ready_checkpoint(fixture.host(), &adopted, &absent).is_err());
    let mut wrong_proof = adopted.clone();
    wrong_proof.proof = Some(fixture.proof.clone());
    assert!(restore_ready_checkpoint(fixture.host(), &wrong_proof, &ready).is_err());
    assert!(restore_ready_checkpoint(fixture.host(), &originally_paused, &ready).is_err());

    // Even an internally consistent Ready payload cannot forge a descriptor
    // the live helper never issued: the floor exception grants no attachment
    // authority and must still execute the ordinary exact-source handshake.
    let mut forged = ready.clone();
    let unissued_directory = fixture.root.join("unissued-project");
    fs::create_dir(&unissued_directory).unwrap();
    let forged_chat = &mut forged.chats[0];
    forged_chat.source.launch.recipe.working_directory = unissued_directory.clone();
    forged_chat.launch.recipe.working_directory = unissued_directory;
    forged_chat.previous_sources.clear();
    forged.require_ready().unwrap();
    assert!(restore_ready_checkpoint(fixture.host(), &forged.chats[0].source, &forged).is_err());
    assert!(fixture.host().lock().unwrap().chats.is_empty());
    let unchanged =
        Handle::inspect_remote(&fixture.owners[0].record, fixture.proof.clone()).unwrap();
    assert_eq!(unchanged.source, adopted);

    // Use the production Ready-to-Restoring startup transition, rather than
    // admitting directly from this test's still-Ready in-memory checkpoint.
    // The ordinary path must complete its owner restore and Ready publication.
    crate::host::update_recovery::restore_ordinary(fixture.host()).unwrap();
    let current = {
        let state = fixture.host().lock().unwrap();
        assert_eq!(state.chats.len(), 1);
        assert!(!state.chats.contains_key(&originally_paused.id));
        let record = &state.chats[&adopted.id];
        assert_eq!(
            record.handle.actor_identity(),
            Some(fixture.owners[0].clone())
        );
        assert_eq!(record.session.proof.as_ref(), Some(&fixture.proof));
        record.session.clone()
    };
    let store = crate::upgrade::handoff::Store::discover().unwrap();
    assert!(store.load().unwrap().is_none());
    let listed = current_roster(&fixture)
        .1
        .into_iter()
        .find(|session| session.id == adopted.id)
        .unwrap();
    assert!(
        store
            .recovered_chats()
            .unwrap()
            .proves(&adopted, &listed, &fixture.proof)
    );
    let published: serde_json::Value = serde_json::from_slice(
        &fs::read(
            store
                .path
                .with_file_name("daemon-upgrade.ready-outcome.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        published["serving"],
        serde_json::to_value(&fixture.proof).unwrap()
    );
    let authenticated =
        Handle::inspect_remote(&fixture.owners[0].record, fixture.proof.clone()).unwrap();
    assert_eq!(authenticated.source, current);
    let paused = Handle::inspect_remote(&fixture.owners[1].record, fixture.proof.clone()).unwrap();
    assert_eq!(paused.identity, paused_original.identity);
    assert_eq!(paused.source, paused_original.source);
    assert_eq!(
        paused.state.confirmed_session,
        originally_paused.launch.session_id
    );
    assert!(
        fixture
            .owners
            .iter()
            .all(|owner| { owner.record.root.is_current() && owner.record.holder.is_current() })
    );
    assert_eq!(fs::read(fixture.root.join("launches")).unwrap(), launches);
    assert_eq!(fs::read(fixture.root.join("prompts")).unwrap(), prompts);
}

fn current_roster(
    fixture: &Fixture,
) -> (Vec<crate::terminal_ipc::TerminalDescriptor>, Vec<Session>) {
    let state = fixture.host().lock().unwrap();
    let chats = state
        .chats
        .values()
        .filter(|record| !record.handle.is_read_only_history())
        .map(|record| {
            let mut session = record.session.clone();
            session.launch.session_id = record.handle.session_id();
            session.native_continuity = record.handle.native_continuity();
            session
        })
        .collect();
    (state.list(0), chats)
}

fn start_ordinary_terminal(
    fixture: &Fixture,
) -> (TerminalSession, crate::terminal::holder::Process) {
    let pid_file = fixture.root.join("ordinary-terminal.pid");
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            format!(
                "printf '%s\\n' \"$$\" > '{}'; exec /bin/sleep 1200",
                pid_file.display()
            ),
        ],
        working_directory: fixture.root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        transcript_path: None,
        resume_hint: None,
    };
    let descriptor = crate::host::state::spawn_terminal(
        fixture.host(),
        0,
        session,
        crate::terminal_ipc::TerminalSpawnOptions::default(),
    )
    .unwrap();
    wait(
        || {
            fs::read_to_string(&pid_file)
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
                .is_some()
        },
        "ordinary terminal did not publish its real child PID",
    );
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let process = crate::terminal::holder::Process::read(pid).unwrap();
    assert!(
        fixture.host().lock().unwrap().terminals[&descriptor.session.id]
            .handle
            .holder_identity()
            .is_some()
    );
    (descriptor.session, process)
}

fn with_private_socket<T>(fixture: &Fixture, operation: impl FnOnce() -> T) -> T {
    let socket = crate::daemon::RuntimePaths::discover().unwrap().socket;
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let host = fixture.host().clone();
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_stopped = stopped.clone();
    let server = thread::spawn(move || {
        let mut readers = Vec::new();
        for client in 17.. {
            let (stream, _) = listener.accept().unwrap();
            if server_stopped.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            let host = host.clone();
            readers.push(thread::spawn(move || {
                crate::host::client::serve(stream, client, host)
            }));
        }
        for reader in readers {
            reader.join().unwrap();
        }
    });
    // Production first discovers the wire version, then authenticates a fresh
    // connection. Serve both real clients and wake accept explicitly to stop.
    let result = operation();
    stopped.store(true, std::sync::atomic::Ordering::Release);
    let wake = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    server.join().unwrap();
    drop(wake);
    fs::remove_file(socket).unwrap();
    result
}

fn verify_over_private_socket(
    fixture: &Fixture,
    original: &crate::update::recovery::Census,
) -> crate::update::recovery::Receipt {
    let verified = with_private_socket(fixture, || original.verify()).unwrap();
    eprintln!(
        "private serving proof {:?}: {:?}",
        fixture.proof, verified.outcome
    );
    verified
}

#[test]
fn settled_cold_host_keeps_real_actors_and_terminal_then_verifies_original_census() {
    if crate::retained_fixture::run_isolated_with_timeout(Duration::from_secs(1020)) {
        return;
    }
    let Some(root) = isolated(true) else { return };
    if let Ok(incarnation) = std::env::var("CAGENTS_SETTLED_COLD_HOP") {
        return cold_host_hop(root, &incarnation);
    }
    let mut fixture = Fixture::new(root);
    let accepted = fixture.start();
    let originally_paused = fixture.start();
    let (terminal, original_child) = start_ordinary_terminal(&fixture);
    let source = fixture.proof.clone();
    let actor = fixture.owners[0].clone();
    let paused_actor = fixture.owners[1].clone();
    let mut capture = crate::upgrade::recovery::SnapshotCapture::default();
    for session in [&accepted, &originally_paused] {
        let handle = fixture.host().lock().unwrap().chats[&session.id]
            .handle
            .clone();
        capture
            .push(session.clone(), handle.snapshot(None).unwrap())
            .unwrap();
    }
    let census = crate::upgrade::recovery::Census {
        terminals: vec![terminal.clone()],
        chats: vec![accepted.clone(), originally_paused.clone()],
        snapshots: capture.finish(),
        ..Default::default()
    };
    let original: crate::update::recovery::Census = serde_json::from_value(serde_json::json!({
        "version":1, "transaction":uuid::Uuid::new_v4().to_string(), "source":source,
        "terminals":census.terminals, "chats":census.chats, "checkpoint_before":null,
    }))
    .unwrap();
    let store = crate::upgrade::handoff::Store::discover().unwrap();
    let mut checkpoint = crate::upgrade::handoff::Checkpoint::new(
        source.clone(),
        Vec::new(),
        vec![crate::upgrade::handoff::Carried {
            source: terminal.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: Default::default(),
        }],
        [&accepted, &originally_paused]
            .into_iter()
            .map(|session| crate::upgrade::handoff::Chat {
                source: session.clone(),
                previous_sources: Vec::new(),
                launch: session.launch.clone(),
            })
            .collect(),
    );
    checkpoint.preserve_carried = true;
    store.save(&checkpoint).unwrap();
    let checkpoint_bytes = fs::read(&store.path).unwrap();
    fixture.save_paused(originally_paused.clone());
    // A genuinely retained paused pane keeps its actual saved page referenced.
    // Cold startup must not manufacture this reference from archive metadata.
    let mut registry = crate::host::model::Registry::open().unwrap();
    registry
        .apply(&crate::workspace_model::ModelEdit::OpenProject {
            repository: fixture.root.join("project"),
            name: "Private recovery fixture".into(),
            kind: crate::model::ProjectKind::Folder,
        })
        .unwrap();
    let scope = crate::workspace_model::views::Scope::Worktree {
        repository: fixture.root.join("project"),
        path: fixture.root.join("project"),
    };
    let seeded = registry
        .apply(&crate::workspace_model::ModelEdit::SeedView {
            scope,
            panes: vec![crate::workspace_model::views::Pane {
                id: crate::model::CardId::new(),
                title: "Retained paused fixture".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 1.0,
                kind: crate::model::CardKind::Chat(originally_paused.clone()),
                scope: None,
            }],
            closed: vec![],
        })
        .unwrap();
    assert!(
        seeded.changed,
        "the paused-pane reference must be published in an open project"
    );
    drop(registry);

    // This owner really ended during the incomplete preparation. A live
    // unadopted helper is quarantined work, not a paused history success.
    crate::host::chat::process(
        fixture.host(),
        0,
        crate::terminal_ipc::ClientRequest::ChatClose {
            chat_id: originally_paused.id,
        },
    )
    .unwrap();
    wait(
        || !paused_actor.record.root.is_current() && !paused_actor.record.holder.is_current(),
        "the originally paused actor did not actually close",
    );
    fixture.owners.retain(|owner| owner != &paused_actor);
    let launches = fs::read(fixture.root.join("launches")).unwrap();
    assert_eq!(String::from_utf8_lossy(&launches).lines().count(), 2);
    let prompts = fs::read(fixture.root.join("prompts")).unwrap();
    assert_eq!(prompts, b"prompt\nprompt\n");
    let prompt_receipts = fs::read(fixture.root.join("prompt-receipts")).unwrap();
    let receipts = String::from_utf8(prompt_receipts.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 2);
    for (receipt, (session, owner)) in receipts.iter().zip([
        (&accepted, &fixture.owners[0]),
        (&originally_paused, &paused_actor),
    ]) {
        assert_eq!(
            receipt["pid"].as_i64(),
            Some(i64::from(owner.record.root.pid))
        );
        assert_eq!(
            receipt["session"].as_str(),
            session.launch.session_id.as_deref()
        );
        assert_eq!(receipt["type"], "user");
        assert_eq!(receipt["seed"], true);
        uuid::Uuid::parse_str(receipt["uuid"].as_str().unwrap()).unwrap();
    }
    eprintln!(
        "source A: exactly two explicit fixture seed user frames completed, from provider PIDs {} and {}; retaining immutable no-replay baseline before B",
        receipts[0]["pid"], receipts[1]["pid"]
    );

    fixture.rotate("settled-B");
    crate::host::state::restore_orphaned_holders(fixture.host()).unwrap();
    restore_orphaned_native(fixture.host(), &[]).unwrap();
    let (terminals, chats) = current_roster(&fixture);
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].id, accepted.id);
    assert_eq!(
        fixture.host().lock().unwrap().chats[&accepted.id]
            .handle
            .actor_identity(),
        Some(actor.clone())
    );
    // The original receipt-bearing B recovery wrote this paused page before
    // it settled. A later cold daemon may observe it, never reconstruct it
    // from the immutable outcome if the person has since closed the pane.
    let mut paused =
        crate::upgrade::floor::Record::new(source.clone(), vec![originally_paused.clone()], vec![]);
    paused.snapshots = census
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.source == originally_paused)
        .cloned()
        .collect();
    paused.save().unwrap();
    let retained = crate::upgrade::floor::record_for_source(&source)
        .unwrap()
        .unwrap();
    assert!(retained.chats.contains(&originally_paused));
    assert!(
        retained
            .snapshots
            .iter()
            .any(|snapshot| snapshot.source == originally_paused)
    );
    let model = crate::host::model::Registry::open()
        .unwrap()
        .model()
        .unwrap();
    let reference_retained = model.views.iter().any(|view| {
        view.panes.iter().any(|pane| {
            matches!(&pane.kind, crate::model::CardKind::Chat(session)
                if session.id == originally_paused.id)
        })
    });
    assert!(
        reference_retained,
        "B first recovery must retain a real saved paused-pane reference before settling"
    );
    let target = std::env::current_exe().unwrap();
    let outcome = crate::upgrade::recovery::fixture_settled_outcome(
        &store.path,
        &target,
        &source,
        census,
        &fixture.proof,
        &terminals,
        &chats,
    )
    .unwrap();
    let outcome_bytes = fs::read(&outcome).unwrap();
    original
        .save(&fixture.root.join("original-census.json"))
        .unwrap();
    let data = ColdFixtureData {
        source,
        accepted,
        originally_paused,
        terminal,
        actor,
        paused_actor,
        original_child,
        checkpoint: store.path.clone(),
        outcome,
        checkpoint_bytes,
        outcome_bytes,
        launches,
        prompts,
        prompt_receipts,
    };
    let mut metadata = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(fixture.root.join("cold-fixture.json"))
        .unwrap();
    use std::io::Write;
    metadata
        .write_all(&serde_json::to_vec(&data).unwrap())
        .unwrap();
    metadata.sync_all().unwrap();
    // A fresh executable owns each cold daemon. Reusing a HostState inside the
    // source process would retain that source's original-child cleanup service,
    // whose real shutdown admission correctly forbids preflight until exit.
    fixture.leave_host();
    let selector = thread::current().name().unwrap().to_owned();
    for incarnation in ["cold-C", "cold-D", "cold-after-explicit-close"] {
        let started = Instant::now();
        eprintln!("starting {incarnation}");
        let output = std::process::Command::new(&target)
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .env("CAGENTS_SETTLED_COLD_HOP", incarnation)
            .stderr(std::process::Stdio::inherit())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{incarnation}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!(
            "{incarnation} completed in {:?}:\n{}",
            started.elapsed(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert_retired_fixture_evidence(&data, &target);
        assert_eq!(fs::read(&data.outcome).unwrap(), data.outcome_bytes);
        assert_eq!(
            fs::read(fixture.root.join("launches")).unwrap(),
            data.launches
        );
        assert_eq!(
            fs::read(fixture.root.join("prompts")).unwrap(),
            data.prompts
        );
        assert_eq!(
            fs::read(fixture.root.join("prompt-receipts")).unwrap(),
            data.prompt_receipts
        );
    }
    fixture.owners.retain(|owner| owner != &data.actor);
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ColdFixtureData {
    source: HostSessionProof,
    accepted: Session,
    originally_paused: Session,
    terminal: TerminalSession,
    actor: ActorIdentity,
    paused_actor: ActorIdentity,
    original_child: crate::terminal::holder::Process,
    checkpoint: PathBuf,
    outcome: PathBuf,
    checkpoint_bytes: Vec<u8>,
    outcome_bytes: Vec<u8>,
    launches: Vec<u8>,
    prompts: Vec<u8>,
    prompt_receipts: Vec<u8>,
}

fn assert_retired_fixture_evidence(data: &ColdFixtureData, target: &Path) {
    assert!(
        !data.checkpoint.exists(),
        "completed recovery must release the canonical checkpoint for the next update"
    );
    let archive = crate::upgrade::recovery::archived_proof(&data.checkpoint, target)
        .unwrap()
        .expect("the actual archived checkpoint must remain verifiable");
    assert_eq!(
        fs::read(&archive.archive_path).unwrap(),
        data.checkpoint_bytes
    );
    assert_eq!(fs::read(&archive.outcome_path).unwrap(), data.outcome_bytes);
    assert_eq!(archive.outcome_path, data.outcome);
}

fn cold_host_hop(root: PathBuf, incarnation: &str) {
    let data: ColdFixtureData =
        serde_json::from_slice(&fs::read(root.join("cold-fixture.json")).unwrap()).unwrap();
    let original =
        crate::update::recovery::Census::load(&root.join("original-census.json")).unwrap();
    let mut proof = data.source.clone();
    proof.host_incarnation = HostIncarnation::new(incarnation);
    let mut fixture = Fixture {
        root,
        proof,
        lease: None,
        host: None,
        broker: None,
        owners: Vec::new(),
    };
    fixture.install_host();
    eprintln!(
        "{incarnation}: host PID {}, original terminal {:?}, actor {} root {:?} holder {:?}",
        std::process::id(),
        data.original_child,
        data.actor.instance,
        data.actor.record.root,
        data.actor.record.holder
    );
    let target = std::env::current_exe().unwrap();
    let canonical = crate::host::update_recovery::restore_settled_from(
        fixture.host(),
        &data.checkpoint,
        &target,
    )
    .unwrap();
    if incarnation == "cold-C" {
        assert!(
            canonical,
            "the first cold recovery still owns the canonical Prepared checkpoint"
        );
    } else {
        assert!(
            !canonical,
            "completed recovery must use ordinary archived holder discovery"
        );
        crate::host::update_recovery::restore_ordinary(fixture.host()).unwrap();
        let ready = data
            .checkpoint
            .with_file_name("daemon-upgrade.ready-outcome.json");
        let publication: serde_json::Value =
            serde_json::from_slice(&fs::read(ready).unwrap()).unwrap();
        assert_eq!(
            publication["serving"],
            serde_json::to_value(&fixture.proof).unwrap()
        );
    }
    {
        let state = fixture.host().lock().unwrap();
        assert_eq!(
            state.startup_state(),
            crate::terminal_ipc::StartupState::Ready
        );
        assert!(!state.chats.contains_key(&data.originally_paused.id));
        assert!(state.terminals.contains_key(&data.terminal.id));
        if incarnation != "cold-after-explicit-close" {
            assert_eq!(state.chats.len(), 1);
            assert_eq!(
                state.chats[&data.accepted.id].handle.actor_identity(),
                Some(data.actor.clone())
            );
            let aliases = state
                .upgrade_chats
                .get(&data.accepted.id)
                .expect("fresh authentication must publish issued aliases");
            assert!(aliases.iter().any(|alias| alias.source == data.accepted));
            assert!(aliases.iter().any(|alias| {
                alias.source.proof.as_ref().is_some_and(|proof| {
                    proof.host_incarnation == HostIncarnation::new("settled-B")
                })
            }));
        } else {
            assert!(
                state.chats.is_empty(),
                "closed and originally paused actors must not reopen"
            );
        }
    }
    eprintln!("{incarnation}: cold host restoration settled; verifying original census");
    let outcome: serde_json::Value = serde_json::from_slice(&data.outcome_bytes).unwrap();
    let cold = data.checkpoint.with_file_name(format!(
        "daemon-upgrade.cold-outcome.{}.json",
        outcome["checkpoint_hash"].as_str().unwrap()
    ));
    let cold: serde_json::Value = serde_json::from_slice(&fs::read(cold).unwrap()).unwrap();
    eprintln!(
        "{incarnation}: exact cold-proof completeness: {}",
        cold["completeness"]
    );
    assert!(
        data.original_child.is_current(),
        "the exact original terminal child must survive every host process"
    );
    let receipt = verify_over_private_socket(&fixture, &original);
    if incarnation == "cold-after-explicit-close" {
        assert!(
            matches!(&receipt.outcome, crate::update::recovery::Outcome::Unverified {
            serving: Some(serving), readiness: Some(crate::terminal_ipc::StartupState::Ready), surviving_terminals: 1, ..
        } if *serving == fixture.proof),
            "{:?}",
            receipt.outcome
        );
        assert!(!data.actor.record.root.is_current() && !data.actor.record.holder.is_current());
    } else {
        assert!(
            matches!(&receipt.outcome, crate::update::recovery::Outcome::Usable {
            serving, terminals: 1, chats: 1, paused_terminals, paused_chats,
        } if *serving == fixture.proof && paused_terminals.is_empty() && paused_chats == &[data.originally_paused.id]),
            "{:?}",
            receipt.outcome
        );
    }
    if incarnation != "cold-after-explicit-close" {
        eprintln!(
            "{incarnation}: original census is Usable; starting production retirement/refresh verification"
        );
        if incarnation == "cold-C" {
            let retired = with_private_socket(&fixture, || {
                // The cold-start helper and the final update verifier can
                // discover the same pending retirement. Serialize the whole
                // root proof. The bounded loser reports Busy rather than
                // blocking indefinitely or claiming completion; retry only
                // after the winning helper has joined.
                let results = thread::scope(|scope| {
                    let first = scope.spawn(crate::update::recovery::retire_current_service);
                    let second = scope.spawn(crate::update::recovery::retire_current_service);
                    [first.join().unwrap(), second.join().unwrap()]
                });
                assert_eq!(
                    results
                        .iter()
                        .filter(|result| matches!(
                            result,
                            Ok(crate::update::recovery::RetirementStatus::Completed)
                        ))
                        .count(),
                    1,
                    "{results:?}"
                );
                assert_eq!(
                    results
                        .iter()
                        .filter(|result| matches!(
                            result,
                            Ok(crate::update::recovery::RetirementStatus::Busy)
                        ))
                        .count(),
                    1,
                    "{results:?}"
                );
                assert!(matches!(
                    crate::update::recovery::retire_current_service().unwrap(),
                    crate::update::recovery::RetirementStatus::AlreadyCompleted
                ));
                Ok::<_, crate::update::UpdateError>(
                    crate::update::recovery::RetirementStatus::Completed,
                )
            })
            .unwrap();
            assert!(matches!(
                retired,
                crate::update::recovery::RetirementStatus::Completed
            ));
        } else {
            assert!(
                !crate::daemon::RuntimePaths::discover()
                    .unwrap()
                    .socket
                    .exists()
            );
            let before = crate::upgrade::recovery::archived_proof(&data.checkpoint, &target)
                .unwrap()
                .unwrap();
            let bytes = fs::read(&before.retirement_path).unwrap();
            assert_eq!(
                crate::update::recovery::refresh_completed_service().unwrap(),
                crate::update::recovery::RetirementStatus::AlreadyCompleted
            );
            assert_eq!(
                fs::read(&before.retirement_path).unwrap(),
                bytes,
                "cold start must not rewrite or extend issued history"
            );
            eprintln!(
                "{incarnation}: completed archive skipped cold census/socket verification without rewriting history"
            );
        }
        assert_retired_fixture_evidence(&data, &target);
        let after = verify_over_private_socket(&fixture, &original);
        assert!(
            after.usable(),
            "archiving must retain exact current lineage: {:?}",
            after.outcome
        );
        let issued = crate::upgrade::recovery::archived_discovery(&data.checkpoint, &target)
            .unwrap()
            .unwrap();
        if incarnation == "cold-C" {
            assert!(
                issued
                    .adopted_chat_descriptors
                    .iter()
                    .any(|chat| chat.id == data.accepted.id
                        && chat.proof.as_ref() == Some(&fixture.proof))
            );
        }
        eprintln!(
            "{incarnation}: production RootCensus completed retirement; canonical checkpoint absent, exact archive/outcome unchanged, original census still Usable"
        );
    }
    let candidate_cli = target.with_file_name("cagents");
    if incarnation != "cold-after-explicit-close" {
        let (admitted, captured) = with_private_socket(&fixture, || {
            (
                crate::update::inspect_pending_recovery(&candidate_cli, None),
                crate::update::recovery::Census::capture(uuid::Uuid::new_v4().to_string()),
            )
        });
        admitted.unwrap();
        let captured = captured.unwrap();
        assert_eq!(
            captured.source.as_ref(),
            Some(&fixture.proof),
            "fresh admission must retain the exact current owner"
        );
        let captured = serde_json::to_value(&captured).unwrap();
        let terminals: Vec<TerminalSession> =
            serde_json::from_value(captured["terminals"].clone()).unwrap();
        let chats: Vec<Session> = serde_json::from_value(captured["chats"].clone()).unwrap();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].id, data.terminal.id);
        assert_eq!(
            terminals[0].continuity.live_proof().as_ref(),
            Some(&fixture.proof)
        );
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].id, data.accepted.id);
        assert_eq!(chats[0].proof.as_ref(), Some(&fixture.proof));
        assert!(
            chats
                .iter()
                .all(|chat| chat.id != data.originally_paused.id)
        );
        eprintln!(
            "{incarnation}: production pending admission accepted exact current owner; fresh census contains 1 current chat and 1 current terminal"
        );
    } else {
        assert_retired_fixture_evidence(&data, &target);
        assert!(
            !crate::daemon::RuntimePaths::discover()
                .unwrap()
                .socket
                .exists()
        );
        assert_eq!(
            crate::update::recovery::refresh_completed_service().unwrap(),
            crate::update::recovery::RetirementStatus::AlreadyCompleted
        );
        eprintln!(
            "{incarnation}: closed historical owner did not trigger a cold-start socket query or census recheck"
        );
        let (retired, admitted, captured) = with_private_socket(&fixture, || {
            (
                crate::update::recovery::retire_current_service(),
                crate::update::inspect_pending_recovery(&candidate_cli, None),
                crate::update::recovery::Census::capture(uuid::Uuid::new_v4().to_string()),
            )
        });
        assert!(matches!(
            retired.unwrap(),
            crate::update::recovery::RetirementStatus::AlreadyCompleted
        ));
        admitted.unwrap();
        let captured = captured.unwrap();
        assert_eq!(captured.source.as_ref(), Some(&fixture.proof));
        let captured = serde_json::to_value(&captured).unwrap();
        let terminals: Vec<TerminalSession> =
            serde_json::from_value(captured["terminals"].clone()).unwrap();
        let chats: Vec<Session> = serde_json::from_value(captured["chats"].clone()).unwrap();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].id, data.terminal.id);
        assert_eq!(
            terminals[0].continuity.live_proof().as_ref(),
            Some(&fixture.proof)
        );
        assert!(
            chats.is_empty(),
            "explicitly closed and originally paused chats must stay absent"
        );
        eprintln!(
            "{incarnation}: completed history did not gate a new job; fresh census contains the exact Ready current owner, 1 surviving terminal and no live chats"
        );
    }
    assert_retired_fixture_evidence(&data, &target);
    assert_eq!(fs::read(&data.outcome).unwrap(), data.outcome_bytes);
    assert_eq!(
        fs::read(fixture.root.join("launches")).unwrap(),
        data.launches
    );
    assert_eq!(
        fs::read(fixture.root.join("prompts")).unwrap(),
        data.prompts
    );
    assert_eq!(
        fs::read(fixture.root.join("prompt-receipts")).unwrap(),
        data.prompt_receipts
    );
    assert!(
        !data.paused_actor.record.root.is_current()
            && !data.paused_actor.record.holder.is_current()
    );
    if incarnation == "cold-D" {
        let current = current_roster(&fixture)
            .1
            .into_iter()
            .find(|session| session.id == data.accepted.id)
            .unwrap();
        let attached = crate::host::chat::process(
            fixture.host(),
            0,
            crate::terminal_ipc::ClientRequest::ChatAttach { session: current },
        )
        .unwrap();
        assert!(matches!(
            attached,
            crate::terminal_ipc::ServerResponse::ChatAttached { .. }
        ));
        crate::host::chat::process(
            fixture.host(),
            0,
            crate::terminal_ipc::ClientRequest::ChatClose {
                chat_id: data.accepted.id,
            },
        )
        .unwrap();
        wait(
            || !data.actor.record.root.is_current() && !data.actor.record.holder.is_current(),
            "explicit close did not retire original actor",
        );
    }
    if incarnation == "cold-after-explicit-close" {
        let handle = fixture.host().lock().unwrap().terminals[&data.terminal.id]
            .handle
            .clone();
        assert_eq!(
            handle.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
        wait(
            || !data.original_child.is_current(),
            "private terminal child did not settle during teardown",
        );
    }
    fixture.leave_host();
}
