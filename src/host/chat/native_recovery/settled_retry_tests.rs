//! Retrying an authenticated original actor after a saved unavailable snapshot.
use super::*;
use crate::{
    chat::runtime::ActorIdentity,
    model::{HostIncarnation, HostSessionId},
};
use std::{collections::BTreeSet, fs, os::unix::fs::PermissionsExt, path::PathBuf};

const MODE: &str = "CAGENTS_SETTLED_NATIVE_RETRY_TEST";

fn isolated() -> Option<PathBuf> {
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
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env_clear()
        .envs(crate::retained_fixture::owner_env())
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
    let deadline = Instant::now() + Duration::from_secs(90);
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
        fs::read_to_string(stdout).unwrap(),
        fs::read_to_string(stderr).unwrap()
    );
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
        super::super::retained_actor_tests::install_provider(&root.join("bin/claude"));
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
        state.install_model(
            crate::host::model::Registry::at(&self.root.join("registry.sqlite3")).unwrap(),
        );
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

    fn rotate(&mut self, incarnation: &str) {
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
    let Some(root) = isolated() else { return };
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

    // Exercise the real startup boundary: it writes Restoring before adopting
    // retained chats, but must preserve the authenticated Ready qualification.
    crate::host::state::restore_upgrade(fixture.host()).unwrap();
    let store = crate::upgrade::handoff::Store::discover().unwrap();
    let completed = store.path.with_file_name("daemon-upgrade.recovered.json");
    let completed_bytes = fs::read(&completed).unwrap();
    assert!(!crate::host::update_recovery::restore_archived(fixture.host(), &[]).unwrap());
    assert_eq!(fs::read(&completed).unwrap(), completed_bytes);
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
