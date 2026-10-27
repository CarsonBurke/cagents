//! Actual registered-client Resolve -> Attach over an execed retained helper.
//! Source-only until admitted with a matched CAGENTS_TEST_RETAINED_ACTOR_HELPER.
use super::*;
use crate::daemon::holder_registry::Record as HolderRecord;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn wait(mut predicate: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(Instant::now() < deadline, "{reason}");
        thread::sleep(Duration::from_millis(10));
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
        } else if kind.is_file() && entry.path().extension().is_some_and(|v| v == "json") {
            if let Ok(bytes) = fs::read(entry.path())
                && let Ok(owner) = serde_json::from_slice::<HolderRecord>(&bytes)
            {
                for process in [&owner.root, &owner.holder] {
                    if process.pid != std::process::id() as i32 && process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
}

/// Same bounded isolated-subprocess pattern as retained_admission_tests; the
/// parent retains its directory until the exact child exits and cleans only
/// discovery records under that child's private runtime on failure.
fn isolated() -> Option<PathBuf> {
    const MODE: &str = "CAGENTS_RETAINED_RESOLUTION_TEST";
    let test = thread::current().name().unwrap().to_owned();
    if std::env::var(MODE).as_deref() == Ok(test.as_str()) {
        return Some(PathBuf::from(
            std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap(),
        ));
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    for child in [
        "home/.claude",
        "run",
        "data",
        "config",
        "cache",
        "bin",
        "project",
    ] {
        fs::create_dir_all(root.path().join(child)).unwrap();
    }
    let stdout = root.path().join("test.stdout");
    let stderr = root.path().join("test.stderr");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture"])
        .env_clear()
        .env(MODE, &test)
        .env(
            "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
            retained_actor_tests::retained_helper_executable(),
        )
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_HOLDER_ROOT", root.path())
        .env("HOME", root.path().join("home"))
        .env("CLAUDE_CONFIG_DIR", root.path().join("home/.claude"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
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
    cleanup_discoveries(&root.path().join("run"));
    assert!(
        status.is_some_and(|s| s.success()),
        "resolution fixture failed/deadline:\n{}\n{}",
        fs::read_to_string(stdout).unwrap(),
        fs::read_to_string(stderr).unwrap()
    );
    None
}

struct Fixture {
    root: PathBuf,
    host: SharedHost,
    lease: Option<crate::daemon::HostLease>,
    broker: Option<super::super::notifications::Broker>,
    original: Session,
    identity: chat::runtime::ActorIdentity,
}

impl Fixture {
    fn new(root: PathBuf) -> Self {
        Self::with_reset(root, false)
    }

    fn with_reset(root: PathBuf, reset: bool) -> Self {
        let provider = root.join("bin/claude");
        retained_actor_tests::install_provider(&provider);
        if reset {
            // Extend the same offline provider's actual wire protocol. A fresh
            // provider UUID is learned only after its explicit reset event;
            // no HOST source, alias map or helper journal is fabricated.
            let script = fs::read_to_string(&provider).unwrap();
            let anchor = "        (root/'provider-done').write_text('done')";
            assert_eq!(script.matches(anchor).count(), 1);
            let replacement = r#"        (root/'provider-done').write_text('done')
        deadline=time.monotonic()+30
        while not (root/'reset-now').exists():
            if time.monotonic()>=deadline: raise RuntimeError('reset gate timed out')
            time.sleep(0.01)
        send({'type':'conversation_reset'})
        import uuid
        session=str(uuid.uuid4())
        send({'type':'system','subtype':'init','session_id':session})
        (root/'reset-id.tmp').write_text(session)
        (root/'reset-id.tmp').replace(root/'reset-id')"#;
            fs::write(&provider, script.replace(anchor, replacement)).unwrap();
        }
        let proof = HostSessionProof {
            host_session_id: "resolution-fixture".into(),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: "first".into(),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let mut state = HostState::new(proof);
        state.register_client(0);
        state.install_model(
            super::super::model::Registry::at(&root.join("registry.sqlite3")).unwrap(),
        );
        let host = Arc::new(HostLock::new(state));
        let broker = super::super::notifications::Broker::start(
            &host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        host.lock().unwrap().notifications = Some(broker.handle());
        let source = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.join("project"),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        let id = source.id;
        start_execution(
            &host,
            0,
            source,
            Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
        )
        .unwrap();
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        let identity = handle
            .actor_identity()
            .expect("actual remote helper required");
        wait(
            || handle.attachment_snapshot().ready,
            "helper did not become ready",
        );
        let original = host.lock().unwrap().chats[&id].session.clone();
        assert!(original.launch.session_id.is_none());
        assert!(matches!(
            original.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        ));
        Self {
            root,
            host,
            lease,
            broker: Some(broker),
            original,
            identity,
        }
    }

    fn handle(&self) -> Handle {
        self.host.lock().unwrap().chats[&self.original.id]
            .handle
            .clone()
    }

    fn rotate_host_without_discovery(&mut self) {
        let mut proof = self.host.lock().unwrap().proof().clone();
        proof.host_incarnation = "successor".into();
        let mut state = HostState::new(proof.clone());
        state.install_model(
            super::super::model::Registry::at(&self.root.join("registry.sqlite3")).unwrap(),
        );
        let successor = Arc::new(HostLock::new(state));
        drop(self.broker.take());
        drop(std::mem::replace(&mut self.host, successor));
        drop(self.lease.take());
        assert!(self.identity.record.holder.is_current());
        assert!(self.identity.record.root.is_current());
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        self.lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let broker = super::super::notifications::Broker::start(
            &self.host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        self.host.lock().unwrap().notifications = Some(broker.handle());
        self.broker = Some(broker);
        assert_ne!(self.original.proof.as_ref(), Some(&proof));
        for client in [11, 12] {
            self.host.lock().unwrap().register_client(client);
        }
    }

    fn rotate_host(&mut self) {
        self.rotate_host_without_discovery();
        restore_orphaned_native(&self.host, &[]).unwrap();
        assert_eq!(self.handle().actor_identity(), Some(self.identity.clone()));
    }

    fn resolve(&self, client: ClientId, source: &Session) -> Result<Session, ProtocolError> {
        let (response, _, _) = super::super::client::process_request(
            &self.host,
            client,
            ClientRequest::ResolveUpgradeChat {
                source: source.clone(),
            },
        )?;
        let ServerResponse::UpgradeChatResolved { session } = response else {
            panic!("wrong public resolution response")
        };
        Ok(session)
    }

    fn attach(&self, client: ClientId, session: &Session) -> Result<ServerResponse, ProtocolError> {
        process(
            &self.host,
            client,
            ClientRequest::ChatAttach {
                session: session.clone(),
            },
        )
    }

    fn publicly_attached_identity(&self, expected: Option<&str>, reason: &str) -> Session {
        let deadline = Instant::now() + Duration::from_secs(15);
        let listed = loop {
            assert!(
                Instant::now() < deadline,
                "{reason}: metadata deadline exhausted"
            );
            // ChatList reads cached resident metadata; do not issue a blocking
            // helper RPC inside the absolute identity-publication deadline.
            let (response, _, _) =
                super::super::client::process_request(&self.host, 0, ClientRequest::ChatList)
                    .expect("public ChatList must remain available");
            let ServerResponse::ChatListed { sessions } = response else {
                panic!("wrong public ChatList response")
            };
            let current = sessions
                .into_iter()
                .find(|session| session.id == self.original.id)
                .expect("the public roster must retain the original actor");
            let confirmed = self.handle().confirmed_session_id();
            let matched = current.launch.session_id.is_some()
                && current.launch.session_id == confirmed
                && expected
                    .is_none_or(|expected| current.launch.session_id.as_deref() == Some(expected));
            assert!(
                Instant::now() < deadline,
                "{reason}: public_id={:?} confirmed_id={confirmed:?} issued_id={:?} provider_done={}",
                current.launch.session_id,
                self.original.launch.session_id,
                self.root.join("provider-done").exists()
            );
            if matched {
                break current;
            }
            thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        };
        // Exercise the actual attachment/snapshot projection separately from
        // the bounded metadata wait. Do not mutate Record.session or manually
        // update the helper source to manufacture historical authority.
        let (response, _, _) = super::super::client::process_request(
            &self.host,
            0,
            ClientRequest::ChatAttach {
                session: listed.clone(),
            },
        )
        .expect("public attachment must preserve the published identity");
        let ServerResponse::ChatAttached { session, snapshot } = response else {
            panic!("wrong public ChatAttach response")
        };
        assert_eq!(session.launch.session_id, listed.launch.session_id);
        assert_eq!(snapshot.session_id, listed.launch.session_id);
        assert_eq!(snapshot.native_continuity, session.native_continuity);
        assert_eq!(
            self.handle().confirmed_session_id(),
            session.launch.session_id
        );
        session
    }

    fn spent(&self, client: ClientId) -> bool {
        matches!(
            self.host.lock().unwrap().chat_resolutions.get(&client),
            Some(upgrade_resolution::Attempt::Spent(_))
        )
    }

    fn no_prompt(&self) {
        assert!(
            !self.root.join("prompts").exists(),
            "recovery must not synthesize provider input"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let handle = self
            .host
            .lock()
            .unwrap()
            .chats
            .get(&self.original.id)
            .map(|r| r.handle.clone());
        if let Some(handle) = handle {
            if handle.close().is_ok() && handle.finish_retained_close().is_ok() {
                return;
            }
        }
        // Failure containment only, not a successful retirement oracle.
        for process in [&self.identity.record.root, &self.identity.record.holder] {
            if process.is_current() {
                unsafe {
                    libc::kill(process.pid, libc::SIGKILL);
                }
            }
        }
    }
}

#[test]
fn real_helper_resolution_adopts_an_unregistered_retained_owner_without_launching() {
    let Some(root) = isolated() else { return };
    let mut fixture = Fixture::new(root);
    fixture.rotate_host_without_discovery();
    assert!(fixture.host.lock().unwrap().chats.is_empty());
    let mut changed = fixture.original.clone();
    changed
        .launch
        .recipe
        .arguments
        .push("--different-account-or-options".into());
    assert!(fixture.resolve(11, &changed).is_err());
    assert!(fixture.host.lock().unwrap().chats.is_empty());
    assert!(fixture.spent(11));
    assert!(fixture.identity.record.root.is_current());
    assert!(fixture.identity.record.holder.is_current());
    let resolved = fixture.resolve(11, &fixture.original).unwrap();
    assert_eq!(
        fixture.handle().actor_identity(),
        Some(fixture.identity.clone())
    );
    assert_eq!(resolved.id, fixture.original.id);
    assert_eq!(resolved.launch.recipe, fixture.original.launch.recipe);
    assert_ne!(resolved.proof, fixture.original.proof);
    {
        let state = fixture.host.lock().unwrap();
        let record = &state.chats[&fixture.original.id];
        assert!(
            record.subscribers.is_empty(),
            "Resolve authenticates but must not subscribe before Attach"
        );
        assert!(record.observations.is_empty());
    }
    let response = fixture.attach(11, &resolved).unwrap();
    let ServerResponse::ChatAttached { session, snapshot } = response else {
        panic!("expected actual hydrated attachment")
    };
    assert_eq!(session, resolved);
    assert!(snapshot.ready);
    assert_eq!(snapshot.session_id, session.launch.session_id);
    assert!(
        fixture.attach(11, &resolved).is_err(),
        "resolution grant is single-use"
    );
    fixture.no_prompt();
    assert!(fixture.identity.record.root.is_current());
    assert!(fixture.identity.record.holder.is_current());
}

#[test]
fn real_retained_claude_update_preview_preserves_pre_id_and_active_turns() {
    let Some(root) = isolated() else { return };
    let mut fixture = Fixture::new(root);
    let inspect = |host: &SharedHost| {
        super::super::state::update::inspect(host, crate::update::impact::Policy::WhenIdle, true)
            .unwrap()
    };
    // The actual installed helper is ready before its first provider UUID.
    // Keeping its reducer and pipes requires no launch/resume recipe.
    assert!(fixture.original.launch.session_id.is_none());
    // Simulate a lagging presentation cache only. The actual helper, its
    // acknowledged capability, proof and source receipts are untouched.
    fixture
        .host
        .lock()
        .unwrap()
        .chats
        .get_mut(&fixture.original.id)
        .unwrap()
        .session
        .native_continuity = chat::NativeContinuity::ReconnectOnly;
    let unnamed = inspect(&fixture.host);
    assert!(unnamed.ready(), "pre-ID retained owner: {unnamed:?}");
    assert!(
        unnamed
            .sessions
            .iter()
            .any(|session| session.id == fixture.original.id
                && session.disposition == crate::update::impact::Disposition::Continues)
    );
    fixture.no_prompt();
    fs::write(
        fixture.root.join("lease-third-frame-gate"),
        b"hold actual turn",
    )
    .unwrap();
    fixture
        .handle()
        .command(chat::Command::Prompt {
            text: "One explicit turn survives the application reconnecting".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait(
        || {
            fs::read_to_string(fixture.root.join("produced"))
                .is_ok_and(|text| text.lines().count() >= 3)
                && fixture.handle().attachment_snapshot().busy
        },
        "actual provider did not reach its busy gate",
    );
    assert_eq!(
        fixture.handle().check_update_readiness().unwrap(),
        chat::runtime::UpdateReadiness::Busy
    );
    let active = inspect(&fixture.host);
    assert!(
        active.ready(),
        "retained active turn must continue: {active:?}"
    );
    assert!(active.sessions.iter().all(|session| session.disposition
        == crate::update::impact::Disposition::Continues
        && session.interrupted_subagents.is_empty()));
    assert!(fixture.identity.record.root.is_current());
    fixture.rotate_host();
    let reconnected = inspect(&fixture.host);
    assert!(reconnected.ready());
    assert_eq!(
        fixture.handle().actor_identity(),
        Some(fixture.identity.clone())
    );
    assert!(fixture.handle().attachment_snapshot().busy);
    assert_eq!(
        fs::read_to_string(fixture.root.join("prompts")).unwrap(),
        "prompt\n"
    );
    // Let the genuine fixture provider finish normally; inspection/recovery
    // sent no interruption, replay or replacement launch.
    fs::write(fixture.root.join("lease-third-frame-release"), b"finish").unwrap();
    wait(
        || fixture.root.join("provider-done").exists(),
        "retained turn did not finish after reconnecting",
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("prompts")).unwrap(),
        "prompt\n"
    );
}

#[test]
fn real_helper_resolution_grant_is_single_use_and_connection_scoped() {
    let Some(root) = isolated() else { return };
    let mut fixture = Fixture::new(root);
    fixture.rotate_host();
    fixture.no_prompt();
    let resolved = fixture.resolve(11, &fixture.original).unwrap();
    assert!(resolved.launch.session_id.is_none());
    // Another registered connection's ordinary Attach cannot steal this grant.
    assert!(matches!(
        fixture.attach(12, &resolved),
        Ok(ServerResponse::ChatAttached { .. })
    ));
    assert!(matches!(
        fixture.host.lock().unwrap().chat_resolutions.get(&11),
        Some(upgrade_resolution::Attempt::Ready(_))
    ));
    assert!(matches!(
        fixture.attach(11, &resolved),
        Ok(ServerResponse::ChatAttached { .. })
    ));
    assert!(fixture.spent(11));
    assert!(fixture.attach(11, &resolved).is_err());
    assert!(fixture.spent(11));
    // Every attempted Attach consumes the grant, including a wrong descriptor.
    let resolved = fixture.resolve(11, &fixture.original).unwrap();
    let mut wrong = resolved.clone();
    wrong
        .launch
        .recipe
        .arguments
        .push("--foreign-recipe".into());
    assert!(fixture.attach(11, &wrong).is_err());
    assert!(fixture.spent(11));
    assert!(
        fixture.host.lock().unwrap().chats[&resolved.id]
            .subscribers
            .contains_key(&11),
        "failed grant must preserve a preexisting ordinary/successful subscription"
    );
    assert!(fixture.attach(11, &resolved).is_err());
    let retry = fixture.resolve(11, &fixture.original).unwrap();
    assert_eq!(retry, resolved);
    assert!(matches!(
        fixture.attach(11, &retry),
        Ok(ServerResponse::ChatAttached { .. })
    ));

    // Expiration is a deterministic counterfactual on a real issued grant;
    // no manufactured grant and no clock sleeps authorize the attachment.
    let expired = fixture.resolve(11, &fixture.original).unwrap();
    upgrade_resolution::expire_for_test(&fixture.host, 11);
    assert!(fixture.attach(11, &expired).is_err());
    assert!(fixture.spent(11));
    assert!(fixture.attach(11, &expired).is_err());
    let renewed = fixture.resolve(11, &fixture.original).unwrap();
    assert!(matches!(
        fixture.attach(11, &renewed),
        Ok(ServerResponse::ChatAttached { .. })
    ));
    // A failed fresh Resolve invalidates a prior ready grant as well.
    fixture.resolve(11, &fixture.original).unwrap();
    let mut wrong_source = fixture.original.clone();
    wrong_source.launch.recipe.program = "/foreign/claude".into();
    assert!(fixture.resolve(11, &wrong_source).is_err());
    assert!(fixture.spent(11));
    fixture.resolve(11, &fixture.original).unwrap();
    drop(fixture.host.lock().unwrap().unregister_client(11));
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .chat_resolutions
            .contains_key(&11)
    );
    assert!(fixture.resolve(11, &fixture.original).is_err());
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .chat_resolutions
            .contains_key(&11)
    );
    fixture.host.lock().unwrap().register_client(11);
    fixture.resolve(11, &fixture.original).unwrap();
    fixture.host.lock().unwrap().forget_client_delivery(11);
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .chat_resolutions
            .contains_key(&11)
    );
    assert!(fixture.resolve(11, &fixture.original).is_err());
    assert!(
        !fixture
            .host
            .lock()
            .unwrap()
            .chat_resolutions
            .contains_key(&11)
    );
    fixture.no_prompt();
    assert_eq!(
        fixture.handle().actor_identity(),
        Some(fixture.identity.clone())
    );
}

#[test]
fn real_helper_epoch_rotation_between_resolve_and_attach_rejects_old_proxy() {
    let Some(root) = isolated() else { return };
    let mut fixture = Fixture::new(root);
    fixture.rotate_host();
    let resolved = fixture.resolve(11, &fixture.original).unwrap();
    let old = fixture.handle();
    let proof = fixture.host.lock().unwrap().proof().clone();
    // Actual authenticated attach rotates the helper epoch without changing
    // the HOST record, actor UUID, provider process, source or execution ID.
    let replacement =
        Handle::adopt_remote(&fixture.identity.record, resolved.clone(), proof, |_| {}).unwrap();
    assert!(replacement.same_actor(&old));
    assert!(!replacement.same_attachment(&old));
    assert!(
        fixture.attach(11, &resolved).is_err(),
        "same actor UUID must not authorize a stale helper epoch"
    );
    assert!(fixture.spent(11));
    assert!(fixture.attach(11, &resolved).is_err());
    assert!(
        !fixture.host.lock().unwrap().chats[&resolved.id]
            .subscribers
            .contains_key(&11),
        "failed hydration must roll back its provisional subscription"
    );
    fixture
        .host
        .lock()
        .unwrap()
        .chats
        .get_mut(&resolved.id)
        .unwrap()
        .handle = replacement;
    let retry = fixture.resolve(11, &fixture.original).unwrap();
    assert!(matches!(
        fixture.attach(11, &retry),
        Ok(ServerResponse::ChatAttached { .. })
    ));
    fixture.no_prompt();
    assert_eq!(
        fixture.handle().actor_identity(),
        Some(fixture.identity.clone())
    );
}

#[test]
fn real_helper_resolution_learns_only_provider_confirmed_pre_id_identity() {
    let Some(root) = isolated() else { return };
    let mut fixture = Fixture::new(root);
    fixture.rotate_host();
    let before = fixture.resolve(11, &fixture.original).unwrap();
    assert!(before.launch.session_id.is_none());
    fixture.attach(11, &before).unwrap();
    fixture.no_prompt();
    let scope = fixture.handle().launch_account_scope().cloned().unwrap();

    fixture
        .handle()
        .command(chat::Command::Prompt {
            text: "One exact explicit identity confirmation".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait(
        || fixture.root.join("provider-done").exists(),
        "offline turn did not finish",
    );
    let published = fixture.publicly_attached_identity(
        None,
        "public descriptor never learned the provider-confirmed identity",
    );
    assert_eq!(published.launch.provider, fixture.original.launch.provider);
    assert_eq!(published.launch.recipe, fixture.original.launch.recipe);
    assert_eq!(
        published.proof.as_ref(),
        Some(fixture.host.lock().unwrap().proof())
    );
    assert_eq!(
        published.native_continuity,
        fixture.original.native_continuity
    );
    assert_eq!(
        fixture.handle().actor_identity(),
        Some(fixture.identity.clone())
    );
    assert_eq!(fixture.handle().launch_account_scope(), Ok(&scope));
    let resolved = fixture.resolve(11, &fixture.original).unwrap();
    let confirmed = fixture
        .handle()
        .confirmed_session_id()
        .expect("actual provider confirmation required");
    assert_eq!(
        published.launch.session_id.as_deref(),
        Some(confirmed.as_str())
    );
    assert_eq!(
        resolved.launch.session_id.as_deref(),
        Some(confirmed.as_str())
    );
    assert_eq!(resolved.launch.recipe, fixture.original.launch.recipe);
    assert!(chat::is_upgrade_identity_learning(
        &fixture.original,
        &resolved
    ));
    let ServerResponse::ChatAttached { session, snapshot } = fixture.attach(11, &resolved).unwrap()
    else {
        panic!("expected actual attachment snapshot")
    };
    assert_eq!(session, resolved);
    assert_eq!(snapshot.session_id.as_deref(), Some(confirmed.as_str()));
    assert_eq!(
        fs::read_to_string(fixture.root.join("prompts")).unwrap(),
        "prompt\n",
        "resolve/attach must not repeat the explicit prompt"
    );
    assert_eq!(
        fixture.handle().actor_identity(),
        Some(fixture.identity.clone())
    );
}

#[test]
fn real_helper_epoch_rotation_after_snapshot_rejects_cached_hydration() {
    let Some(root) = isolated() else { return };
    let mut fixture = Fixture::new(root);
    fixture.rotate_host();
    let resolved = fixture.resolve(11, &fixture.original).unwrap();
    let original_proxy = fixture.handle();
    let owner = fixture.identity.record.clone();
    let proof = fixture.host.lock().unwrap().proof().clone();
    let requested = resolved.clone();
    let captured = Arc::new(std::sync::Mutex::new(None));
    let hook_result = Arc::clone(&captured);
    // The seam runs after the actual snapshot returns, outside HostLock. Only
    // a real helper adoption rotates the epoch; HOST retains the original Arc.
    let _hook = upgrade_resolution::after_snapshot_for_test(move || {
        let replacement = Handle::adopt_remote(&owner, requested, proof, |_| {}).unwrap();
        *hook_result.lock().unwrap() = Some(replacement);
    });
    assert!(
        fixture.attach(11, &resolved).is_err(),
        "cached hydration must not certify a superseded epoch"
    );
    let replacement = captured
        .lock()
        .unwrap()
        .take()
        .expect("actual after-snapshot hook must run");
    assert!(replacement.same_actor(&original_proxy));
    assert!(!replacement.same_attachment(&original_proxy));
    assert!(
        fixture.handle().same_attachment(&original_proxy),
        "the HOST Arc was not replaced; the final authenticated helper check must reject"
    );
    assert!(fixture.spent(11));
    assert!(
        !fixture.host.lock().unwrap().chats[&resolved.id]
            .subscribers
            .contains_key(&11)
    );
    assert!(fixture.attach(11, &resolved).is_err());
    fixture
        .host
        .lock()
        .unwrap()
        .chats
        .get_mut(&resolved.id)
        .unwrap()
        .handle = replacement;
    let retry = fixture.resolve(11, &fixture.original).unwrap();
    assert!(matches!(
        fixture.attach(11, &retry),
        Ok(ServerResponse::ChatAttached { .. })
    ));
    fixture.no_prompt();
}

#[test]
fn real_helper_explicit_reset_resolves_known_a_to_b_from_authenticated_lineage() {
    let Some(root) = isolated() else { return };
    let mut fixture = Fixture::with_reset(root, true);
    fixture
        .handle()
        .command(chat::Command::Prompt {
            text: "One exact turn before provider reset".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait(
        || fixture.root.join("provider-done").exists(),
        "initial provider turn did not finish",
    );
    let issued_a =
        fixture.publicly_attached_identity(None, "HOST never published actual identity A");
    assert!(
        fixture.host.lock().unwrap().chats[&fixture.original.id]
            .session
            .launch
            .session_id
            .is_none(),
        "issued startup descriptor remains immutable"
    );
    let a = issued_a.launch.session_id.clone().unwrap();
    assert_eq!(
        fixture.handle().confirmed_session_id().as_deref(),
        Some(a.as_str())
    );
    fs::write(fixture.root.join("reset-now"), b"provider reset").unwrap();
    wait(
        || fixture.root.join("reset-id").exists(),
        "provider did not emit its explicit reset/new identity",
    );
    let b = fs::read_to_string(fixture.root.join("reset-id")).unwrap();
    assert_ne!(a, b);
    let issued_b =
        fixture.publicly_attached_identity(Some(&b), "HOST never published actual identity B");
    assert_eq!(issued_b.launch.session_id.as_deref(), Some(b.as_str()));
    fixture.rotate_host();
    let resolved = fixture
        .resolve(11, &issued_a)
        .expect("RETAINED_PUBLIC_ISSUED_A_MUST_RESOLVE_AFTER_RESET_AND_HOST_LOSS");
    assert_eq!(resolved.launch.session_id.as_deref(), Some(b.as_str()));
    assert_eq!(resolved.launch.recipe, issued_a.launch.recipe);
    assert!(
        !chat::is_upgrade_recovery(&issued_a, &resolved),
        "ordinary persistence predicate must stay strict for known A to B"
    );
    assert!(
        !chat::is_upgrade_identity_learning(&issued_a, &resolved),
        "this is not unnamed identity learning"
    );
    let ServerResponse::ChatAttached { session, snapshot } = fixture.attach(11, &resolved).unwrap()
    else {
        panic!("expected actual resolved attachment")
    };
    assert_eq!(session, resolved);
    assert_eq!(snapshot.session_id.as_deref(), Some(b.as_str()));
    assert_eq!(
        fixture.handle().actor_identity(),
        Some(fixture.identity.clone())
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("prompts")).unwrap(),
        "prompt\n",
        "recovery/reset must not resend or synthesize a prompt"
    );
}

#[test]
fn real_helper_snapshot_publication_rechecks_exact_host_binding() {
    let Some(root) = isolated() else { return };
    for replace_proxy in [false, true] {
        let mut fixture = Fixture::new(root.clone());
        fixture.rotate_host();
        let resolved = fixture.resolve(11, &fixture.original).unwrap();
        let original = fixture.handle();
        let host = Arc::clone(&fixture.host);
        let id = resolved.id;
        let owner = fixture.identity.record.clone();
        let proof = fixture.host.lock().unwrap().proof().clone();
        let requested = resolved.clone();
        let _hook = upgrade_resolution::after_snapshot_for_test(move || {
            // Deliberate published-binding counterfactual at the production
            // hydration boundary, not a claimed native interleaving. The proxy
            // replacement uses a real helper adoption/epoch; the other case
            // changes only HOST's execution instance to isolate that guard.
            let replacement = replace_proxy
                .then(|| Handle::adopt_remote(&owner, requested, proof, |_| {}).unwrap());
            let mut state = host.lock().unwrap();
            let record = state.chats.get_mut(&id).unwrap();
            if let Some(handle) = replacement {
                record.handle = handle;
            } else {
                record.execution_instance = uuid::Uuid::new_v4();
            }
        });
        assert!(
            fixture.attach(11, &resolved).is_err(),
            "changed HOST binding must not publish hydrated attachment"
        );
        assert!(fixture.spent(11));
        if replace_proxy {
            assert!(fixture.handle().same_actor(&original));
            assert!(!fixture.handle().same_attachment(&original));
        } else {
            assert!(fixture.handle().same_attachment(&original));
        }
        fixture.no_prompt();
    }
}
