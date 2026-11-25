use super::*;
use crate::model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity};

fn lock(path: &Path) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    file
}

fn reviewed(proof: &HostSessionProof, terminal: TerminalId) -> DaemonImpact {
    DaemonImpact {
        capability: crate::terminal_ipc::ShutdownCapability {
            proof: proof.clone(),
            token: "reviewed".into(),
            inventory_generation: 1,
            live_terminals: 1,
            background_subagents: 0,
            background_identity: String::new(),
            expires_at_unix_ms: 1,
        },
        policy: crate::update::impact::Policy::WhenIdle,
        sessions: vec![crate::update::impact::SessionImpact {
            id: terminal,
            kind: SessionKind::Terminal,
            title: String::new(),
            identity: "exact-owner".into(),
            disposition: crate::update::impact::Disposition::Resumes,
            reason: "".into(),
            interrupted_subagents: Vec::new(),
        }],
        blockers: Vec::new(),
    }
}

fn preparation(root: &Path) -> Preparation {
    let source = HostSessionProof {
        host_session_id: HostSessionId::new("acknowledged-source"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("exact-daemon"),
    };
    let terminal = TerminalSession {
        id: TerminalId::new(),
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        launch: LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "exit 0".into()],
            working_directory: root.to_owned(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::DetachedSameBoot {
            host_session_id: source.host_session_id.clone(),
            boot_id: source.boot_id.clone(),
            host_incarnation: source.host_incarnation.clone(),
        },
    };
    let worker_path = root.join("worker.lock");
    let fence_path = root.join("fence.lock");
    let legacy_path = root.join("legacy.lock");
    let legacy = lock(&legacy_path);
    assert_eq!(unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_SH) }, 0);
    let job = uuid::Uuid::new_v4().to_string();
    let request = b"the exact originating private update request";
    save(&root.join(format!("{job}.request")), request);
    let reviewed = reviewed(&source, terminal.id);
    let request_identity = digest(&serde_json::to_vec(&reviewed).unwrap());
    Preparation {
        worker: lock(&worker_path),
        fence: lock(&fence_path),
        legacy,
        authority: Authority {
            version: 1,
            origin: Origin::AcknowledgedPrepare,
            job,
            request_identity,
            job_request_hash: digest(request),
            reviewed: Some(reviewed),
            source,
            expected: vec![(SessionKind::Terminal, terminal.id)],
            windows: Vec::new(),
            census: Census {
                terminals: vec![terminal],
                chats: Vec::new(),
                snapshots: Vec::new(),
                ..Default::default()
            },
            checkpoint_path: root.join("daemon-upgrade.json"),
            checkpoint_hash: String::new(),
            worker_path,
            fence_path,
            legacy_path,
            daemon_path: PathBuf::new(),
            target: PathBuf::new(),
            target_hash: String::new(),
        },
    }
}

fn checkpoint(preparation: &Preparation, phase: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": 2, "phase": phase, "proof": preparation.authority.source,
        "terminals": [], "carried": [{"source": preparation.authority.census.terminals[0], "options": crate::terminal_ipc::TerminalSpawnOptions::default()}], "chats": [],
    })).unwrap()
}

fn save(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

fn handoff(root: &Path) -> LeaseHandoff {
    let path = root.join("cagentsd.lock");
    drop(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap(),
    );
    LeaseHandoff::wait(
        &crate::daemon::RuntimePaths {
            directory: root.to_owned(),
            socket: root.join("cagentsd.sock"),
            lock: path,
        },
        std::process::id(),
    )
    .unwrap()
}

#[test]
fn empty_or_partial_recovery_claim_is_present_without_authorizing_a_retry() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let bytes = checkpoint(&preparation, "Prepared");
    save(&preparation.authority.checkpoint_path, &bytes);
    let pending = PendingRecovery::inspect_at(&preparation.authority.checkpoint_path)
        .unwrap()
        .unwrap();
    assert!(!pending.has_recovery_claim().unwrap());
    let claim = pending
        .checkpoint_path
        .with_file_name(recovery_name(&pending.checkpoint_hash));
    for contents in [b"".as_slice(), b"{\"settled\":".as_slice()] {
        save(&claim, contents);
        assert!(pending.has_recovery_claim().unwrap());
        assert_eq!(fs::read(&claim).unwrap(), contents);
        assert_eq!(fs::read(&pending.checkpoint_path).unwrap(), bytes);
    }
    fs::set_permissions(&claim, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        pending.has_recovery_claim().is_err(),
        "unsafe evidence must not be mistaken for claim absence"
    );
}

#[test]
fn a_prepared_checkpoint_of_more_than_128_chats_stays_inspectable() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let path = &preparation.authority.checkpoint_path;
    let mut index: serde_json::Value =
        serde_json::from_slice(&checkpoint(&preparation, "Prepared")).unwrap();
    index["chats"] = (0..300)
        .map(|chat| {
            let (source, ..) = scale_chat(&preparation, chat, 0, 0);
            serde_json::json!({"source": source, "launch": source.launch})
        })
        .collect();
    save(path, &serde_json::to_vec(&index).unwrap());
    let pending = PendingRecovery::inspect_at(path).unwrap().unwrap();
    assert_eq!(pending.census.chats.len(), 300);
}

#[test]
fn pending_recovery_rejects_reduced_index_with_missing_host_options_without_mutation() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let path = &preparation.authority.checkpoint_path;
    let mut reduced: serde_json::Value =
        serde_json::from_slice(&checkpoint(&preparation, "Prepared")).unwrap();
    reduced["carried"][0]
        .as_object_mut()
        .unwrap()
        .remove("options");
    let bytes = serde_json::to_vec(&reduced).unwrap();
    save(path, &bytes);
    assert!(PendingRecovery::inspect_at(path).is_err());
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(
        !path.with_file_name(recovery_name(&digest(&bytes))).exists(),
        "malformed checkpoint must not consume a one-shot claim"
    );
    assert!(
        !capture_path(path, &preparation.authority.source)
            .unwrap()
            .exists()
    );
}

#[test]
fn explicit_recovery_request_binds_original_census_windows_and_typed_origin() {
    let root = crate::test_fixture::tempdir().unwrap();
    let mut authority = preparation(root.path()).authority;
    authority.origin = Origin::ExplicitRecovery {
        checkpoint: "a".repeat(64),
    };
    authority.reviewed = None;
    authority.checkpoint_hash = "a".repeat(64);
    authority.windows = vec!["primary".into()];
    authority.target = PathBuf::from("/candidate/bin/cagentsd");
    authority.target_hash = "b".repeat(64);
    let request = ManualRequest {
        version: 1,
        job: authority.job.clone(),
        checkpoint: authority.checkpoint_hash.clone(),
        source: authority.source.clone(),
        target: authority.target.clone(),
        target_hash: authority.target_hash.clone(),
        census_hash: digest(&serde_json::to_vec(&authority.census).unwrap()),
        windows: authority.windows.clone(),
    };
    let bytes = serde_json::to_vec(&request).unwrap();
    authority.request_identity = digest(&bytes);
    assert!(validate_request_origin(&authority, &bytes).is_ok());
    let original = authority.census.clone();
    authority.census.terminals.clear();
    assert!(
        validate_request_origin(&authority, &bytes).is_err(),
        "a changed or empty retry census cannot reuse explicit authorization"
    );
    authority.census = original;
    authority.windows.clear();
    assert!(validate_request_origin(&authority, &bytes).is_err());
    authority.windows = request.windows;
    authority.origin = Origin::ExplicitRecovery {
        checkpoint: "c".repeat(64),
    };
    assert!(validate_request_origin(&authority, &bytes).is_err());
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
)]
fn pending_prepared_keeps_original_census_and_exact_window_slots_for_retry() {
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    let path = &preparation.authority.checkpoint_path;
    let bytes = checkpoint(&preparation, "Prepared");
    save(path, &bytes);
    let partial = PendingRecovery::inspect_at(path).unwrap().unwrap();
    assert!(
        !partial.original_census_complete,
        "checkpoint subset alone is not the original full census"
    );
    let mut original = preparation.authority.census.clone();
    let mut stopped = original.terminals[0].clone();
    stopped.id = TerminalId::new();
    original.terminals.push(stopped.clone());
    persist_capture(
        path,
        &CapturedCensus {
            producer: None,
            source: preparation.authority.source.clone(),
            checkpoint_hash: Some(digest(&bytes)),
            census: original.clone(),
            windows: vec!["primary".into()],
        },
    )
    .unwrap();
    let unbound = PendingRecovery::inspect_at(path).unwrap().unwrap();
    assert!(
        !unbound.original_census_complete,
        "an abandoned same-source capture must never prove an arbitrary later checkpoint"
    );
    preparation.authority.census = original.clone();
    preparation.authority.windows = vec!["primary".into()];
    let captured = preparation.acknowledged().unwrap();
    let path = &captured.0.authority.checkpoint_path;
    let pending = PendingRecovery::inspect_at(path).unwrap().unwrap();
    assert!(pending.original_census_complete);
    assert_eq!(
        pending.census, original,
        "retry must retain stopped owners missing from the Prepared subset"
    );
    assert_eq!(pending.windows, ["primary"]);
    assert_eq!(pending.checkpoint_hash, digest(&bytes));
    assert_eq!(
        fs::read(path).unwrap(),
        bytes,
        "inspection must never promote or rewrite Prepared"
    );
    let command = pending.command(Path::new("/candidate with quote'/bin/cagents"), None);
    assert!(command.contains("--no-start daemon recover-update --checkpoint"));
    assert!(command.contains("--yes"));
    assert!(command.starts_with("'/candidate with quote'\\''/bin/cagents'"));
    let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    changed["version"] = serde_json::json!(3);
    save(path, &serde_json::to_vec(&changed).unwrap());
    assert!(
        !PendingRecovery::inspect_at(path)
            .unwrap()
            .unwrap()
            .original_census_complete,
        "same source and scope do not waive a bound checkpoint digest"
    );
}

#[test]
fn pending_original_census_rejects_foreign_source_and_preserves_prepared_bytes() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let path = &preparation.authority.checkpoint_path;
    let bytes = checkpoint(&preparation, "Prepared");
    save(path, &bytes);
    let pending = PendingRecovery::inspect_at(path).unwrap().unwrap();
    let census_path = root.path().join("original.json");
    let mut original = serde_json::json!({
        "version":1, "transaction":uuid::Uuid::new_v4().to_string(),
        "source":preparation.authority.source, "terminals":preparation.authority.census.terminals,
        "chats":[], "checkpoint_before":null,
    });
    save(&census_path, &serde_json::to_vec(&original).unwrap());
    let bound = pending.clone().with_original_census(&census_path).unwrap();
    assert!(bound.original_census_complete);
    original["source"]["host_incarnation"] = serde_json::json!("foreign-owner");
    save(&census_path, &serde_json::to_vec(&original).unwrap());
    assert!(pending.clone().with_original_census(&census_path).is_err());
    original["source"] = serde_json::to_value(&preparation.authority.source).unwrap();
    original["checkpoint_before"] = serde_json::json!(pending.checkpoint_hash);
    save(&census_path, &serde_json::to_vec(&original).unwrap());
    assert!(
        pending.with_original_census(&census_path).is_err(),
        "a census captured after this Prepared checkpoint cannot replace the original scope"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn exact_adopted_terminal_proof_ignores_output_but_not_allocation_changes() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let source = preparation.authority.source;
    let mut issued = crate::terminal_ipc::TerminalDescriptor::relaunchable(
        preparation.authority.census.terminals[0].id,
        preparation.authority.census.terminals[0].launch.clone(),
        None,
    );
    issued.session = preparation.authority.census.terminals[0].clone();
    let mut actual = issued.clone();
    actual.event_sequence += 100;
    actual.generation += 1;
    actual.title = Some("Output changed the title".into());
    assert!(same_adopted_terminal(&issued, &actual, &source));
    actual
        .session
        .launch
        .arguments
        .push("foreign-command".into());
    assert!(!same_adopted_terminal(&issued, &actual, &source));
    actual = issued.clone();
    actual.upgrade_sources.push(issued.session.clone());
    assert!(!same_adopted_terminal(&issued, &actual, &source));
}

#[test]
fn acknowledged_prepare_binds_source_and_every_reviewed_owner() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let bytes = checkpoint(&preparation, "Prepared");
    assert_eq!(
        validate_checkpoint(&preparation.authority, &bytes).unwrap(),
        "Prepared"
    );
    let mut foreign: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    foreign["proof"]["host_incarnation"] = "unrelated-daemon".into();
    assert!(
        validate_checkpoint(
            &preparation.authority,
            &serde_json::to_vec(&foreign).unwrap()
        )
        .is_err()
    );
    let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    changed["carried"][0]["source"]["launch"]["arguments"] =
        serde_json::json!(["-c", "different writer"]);
    assert!(
        validate_checkpoint(
            &preparation.authority,
            &serde_json::to_vec(&changed).unwrap()
        )
        .is_err()
    );
    changed["carried"] = serde_json::json!([]);
    assert!(
        validate_checkpoint(
            &preparation.authority,
            &serde_json::to_vec(&changed).unwrap()
        )
        .is_err()
    );
}

#[test]
fn a_reviewed_conversation_whose_provider_ended_need_not_be_checkpointed() {
    use crate::update::impact::{Disposition, PROVIDER_ENDED, SessionImpact};
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    let bytes = checkpoint(&preparation, "Prepared");
    let chat = |reason: &str| SessionImpact {
        id: TerminalId::new(),
        kind: SessionKind::NativeChat,
        title: String::new(),
        identity: "ended-owner".into(),
        disposition: Disposition::Resumes,
        reason: reason.into(),
        interrupted_subagents: Vec::new(),
    };
    let check = |preparation: &mut Preparation, row: SessionImpact| {
        let mut reviewed = preparation.authority.reviewed.clone().unwrap();
        reviewed.sessions.push(row);
        preparation.authority.expected = checkpointed_owners(&reviewed);
        validate_checkpoint(&preparation.authority, &bytes)
    };
    // The source skips a conversation with nothing left to carry; its
    // window restarts the saved conversation.
    assert_eq!(
        check(&mut preparation, chat(PROVIDER_ENDED)).unwrap(),
        "Prepared"
    );
    // Any other reviewed conversation must still be in the checkpoint.
    assert_eq!(
        check(
            &mut preparation,
            chat("The saved conversation will reopen with a new provider process.")
        )
        .unwrap_err(),
        "The acknowledged checkpoint omitted a reviewed source owner"
    );
    // Only a reopening conversation is exempt, not a terminal or an owner
    // that carries or waits, whatever its reason says.
    let mut terminal = chat(PROVIDER_ENDED);
    terminal.kind = SessionKind::Terminal;
    assert!(check(&mut preparation, terminal).is_err());
    let mut continues = chat(PROVIDER_ENDED);
    continues.disposition = Disposition::Continues;
    assert!(check(&mut preparation, continues).is_err());
}

#[test]
fn census_work_in_flight_is_optional_and_names_only_its_own_conversations() {
    use crate::update::in_flight::InFlight;
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let proof = &preparation.authority.source;
    let mut census = preparation.authority.census.clone();
    let chat = Session {
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: census.terminals[0].launch.clone(),
            session_id: Some("confirmed".into()),
            draft_model: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let mut snapshots = SnapshotCapture::default();
    snapshots.push(chat.clone(), Default::default()).unwrap();
    census.chats = vec![chat.clone()];
    census.snapshots = snapshots.finish();
    census.validate_evidence(proof).unwrap();

    // Without work in flight, a census is written exactly as before, and one
    // written before reads with none.
    let bytes = serde_json::to_value(&census).unwrap();
    assert!(bytes.get("in_flight").is_none(), "{bytes}");
    assert_eq!(serde_json::from_value::<Census>(bytes).unwrap(), census);

    let working = InFlight {
        id: chat.id,
        turn: true,
        ..InFlight::default()
    };
    census.in_flight = vec![working.clone()];
    census.validate_evidence(proof).unwrap();
    let bytes = serde_json::to_vec(&census).unwrap();
    assert_eq!(serde_json::from_slice::<Census>(&bytes).unwrap(), census);

    for invalid in [
        vec![InFlight {
            id: TerminalId::new(),
            ..working.clone()
        }],
        vec![working.clone(), working.clone()],
        vec![InFlight {
            id: chat.id,
            ..InFlight::default()
        }],
    ] {
        let mut changed = census.clone();
        changed.in_flight = invalid;
        assert!(changed.validate_evidence(proof).is_err());
    }
}

#[test]
fn paired_census_accepts_only_same_live_owner_attachment_and_hint_projections() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let proof = &preparation.authority.source;
    let mut census = preparation.authority.census.clone();
    let mut paired = census.terminals[0].clone();
    paired.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    paired.resume_hint = Some("original issued observation".into());
    census.terminals[0].resume_hint = Some("later worker observation".into());
    let chat = Session {
        id: paired.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: paired.launch.clone(),
            session_id: Some("confirmed-thread".into()),
            draft_model: None,
        },
        proof: Some(proof.clone()),
        terminal: Some(Box::new(paired)),
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let mut snapshots = SnapshotCapture::default();
    snapshots.push(chat.clone(), Default::default()).unwrap();
    census.chats = vec![chat];
    census.snapshots = snapshots.finish();
    let original = census.clone();
    census.validate_evidence(proof).unwrap();
    assert_eq!(
        census, original,
        "projection comparison must not rewrite captured descriptors"
    );
    let mut reversed = census.clone();
    let listed = reversed.terminals[0].continuity.clone();
    reversed.terminals[0].continuity = reversed.chats[0]
        .terminal
        .as_ref()
        .unwrap()
        .continuity
        .clone();
    reversed.chats[0].terminal.as_mut().unwrap().continuity = listed;
    reversed.snapshots[0].source = reversed.chats[0].clone();
    reversed.validate_evidence(proof).unwrap();
    for mutation in [
        "id",
        "program",
        "arguments",
        "directory",
        "environment",
        "unset",
        "listener",
        "origin",
        "predecessor",
        "transcript",
        "foreign-proof",
        "incomplete-proof",
        "nonlive",
    ] {
        let mut changed = census.clone();
        let paired = changed.chats[0].terminal.as_mut().unwrap();
        match mutation {
            "id" => paired.id = TerminalId::new(),
            "program" => paired.launch.program = "/different/provider".into(),
            "arguments" => paired.launch.arguments.push("different".into()),
            "directory" => paired.launch.working_directory = "/different".into(),
            "environment" => {
                paired
                    .launch
                    .environment
                    .insert("CODEX_HOME".into(), "/different".into());
            }
            "unset" => {
                paired.launch.unset_environment.insert("CODEX_HOME".into());
            }
            "listener" => {
                paired.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
                    socket: "/different/control.sock".into(),
                    allocator: proof.clone(),
                })
            }
            "origin" => paired.recovery_origin = Some(TerminalId::new()),
            "predecessor" => paired.predecessor = Some(TerminalId::new()),
            "transcript" => paired.transcript_path = Some("/different/transcript".into()),
            "foreign-proof" | "incomplete-proof" => {
                paired.continuity = TerminalContinuity::Live {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: HostIncarnation::new(if mutation == "foreign-proof" {
                        "different"
                    } else {
                        ""
                    }),
                }
            }
            "nonlive" => {
                paired.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: paired.launch.clone(),
                    last_exit: None,
                }
            }
            _ => unreachable!(),
        }
        changed.snapshots[0].source = changed.chats[0].clone();
        assert!(
            changed.validate_evidence(proof).is_err(),
            "accepted changed {mutation}"
        );
    }
    let mut nonlive = census.terminals[0].clone();
    nonlive.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: nonlive.launch.clone(),
        last_exit: None,
    };
    assert!(same_captured_terminal(&nonlive, &nonlive, proof));
    let mut different_hint = nonlive.clone();
    different_hint.resume_hint = Some("different nonlive observation".into());
    assert!(!same_captured_terminal(&nonlive, &different_hint, proof));
    assert!(!same_captured_terminal(
        &nonlive,
        &census.terminals[0],
        proof
    ));
}

#[test]
fn snapshot_capture_rejects_known_identity_change_without_mutating_prior_scope() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let source = Session {
        id: preparation.authority.census.terminals[0].id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: preparation.authority.census.terminals[0].launch.clone(),
            session_id: Some("original-thread".into()),
            draft_model: None,
        },
        proof: Some(preparation.authority.source.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let mut capture = SnapshotCapture::default();
    capture
        .push(
            source.clone(),
            crate::chat::Snapshot {
                session_id: source.launch.session_id.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        capture
            .push(
                source.clone(),
                crate::chat::Snapshot {
                    session_id: Some("other-thread".into()),
                    ..Default::default()
                }
            )
            .is_err()
    );
    let saved = capture.finish();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].source, source);
    let mut unnamed = source.clone();
    unnamed.launch.session_id = None;
    let mut captured = SnapshotCapture::default();
    captured
        .push(
            unnamed.clone(),
            crate::chat::Snapshot {
                session_id: source.launch.session_id.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    for invalid in [String::new(), "x".repeat(1025)] {
        assert!(
            captured
                .push(
                    unnamed.clone(),
                    crate::chat::Snapshot {
                        session_id: Some(invalid),
                        ..Default::default()
                    }
                )
                .is_err()
        );
    }
    let saved = captured.finish();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0].source, unnamed,
        "capture retains the immutable issued descriptor; only later read-only binding can use this exact snapshot"
    );
}

#[test]
fn captured_pane_aliases_and_presentations_bind_the_exact_confirmed_allocation() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let known = Session {
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: preparation.authority.census.terminals[0].launch.clone(),
            session_id: Some("confirmed-thread".into()),
            draft_model: None,
        },
        proof: Some(preparation.authority.source.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let mut alias = known.clone();
    alias.launch.session_id = None;
    let agent = crate::model::AgentId::new();
    let presentation = super::super::floor::Presentation {
        agent,
        session: known.clone(),
        scope: crate::workspace_model::views::Scope::Channel(crate::model::ChannelId::new()),
        pane: crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(agent.as_uuid()),
            title: "Exact paused pane".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: crate::model::CardKind::Chat(alias.clone()),
            scope: None,
        },
        ordinal: 0,
        restored: false,
    };
    let census = Census {
        chats: vec![known],
        aliases: vec![alias],
        presentations: vec![presentation],
        ..Default::default()
    };
    census.validate_presentations().unwrap();
    for changed in ["proof", "thread", "recipe", "allocation", "owner", "pane"] {
        let mut invalid = census.clone();
        match changed {
            "proof" => {
                invalid.aliases[0].proof.as_mut().unwrap().host_incarnation =
                    HostIncarnation::new("foreign")
            }
            "thread" => invalid.aliases[0].launch.session_id = Some("unrelated-thread".into()),
            "recipe" => invalid.aliases[0]
                .launch
                .recipe
                .arguments
                .push("changed".into()),
            "allocation" => invalid.aliases[0].id = TerminalId::new(),
            "owner" => invalid.presentations[0].agent = crate::model::AgentId::new(),
            "pane" => {
                invalid.presentations[0].pane.kind = crate::model::CardKind::Chat({
                    let mut unrelated = invalid.aliases[0].clone();
                    unrelated.id = TerminalId::new();
                    unrelated
                })
            }
            _ => unreachable!(),
        }
        assert!(invalid.validate_presentations().is_err(), "{changed}");
    }
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
)]
fn frozen_source_checkpoint_binds_confirmed_thread_to_effective_launch_not_issued_alias() {
    for original_id in [None, Some(uuid::Uuid::new_v4().to_string())] {
        let root = crate::test_fixture::tempdir().unwrap();
        let mut preparation = preparation(root.path());
        let confirmed = uuid::Uuid::new_v4().to_string();
        // Frozen 991daa43e ChatList overlays the handle's confirmed ID,
        // while its checkpoint source retains the initially issued alias.
        let listed = Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: vec!["--remote".into(), "unix:///fixture/control.sock".into()],
                    working_directory: root.path().to_owned(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(confirmed.clone()),
                draft_model: None,
            },
            proof: Some(preparation.authority.source.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
        };
        let mut issued = listed.clone();
        issued.launch.session_id = original_id;
        preparation
            .authority
            .expected
            .push((SessionKind::NativeChat, listed.id));
        preparation.authority.census.chats.push(listed.clone());
        let mut captured_page = SnapshotCapture::default();
        captured_page
            .push(
                listed.clone(),
                crate::chat::Snapshot {
                    session_id: Some(confirmed.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        preparation.authority.census.snapshots = captured_page.finish();
        let mut checkpoint: serde_json::Value =
            serde_json::from_slice(&checkpoint(&preparation, "Prepared")).unwrap();
        checkpoint["chats"] = serde_json::json!([{ "source": issued, "launch": listed.launch }]);
        let bytes = serde_json::to_vec(&checkpoint).unwrap();
        save(&preparation.authority.checkpoint_path, &bytes);
        let captured = preparation.acknowledged().unwrap();
        assert_eq!(captured.0.authority.checkpoint_hash, digest(&bytes));
        // Ready fallback uses this same distinction and still requires the
        // exact current conversation, including after an actor reset.
        checkpoint["phase"] = "Ready".into();
        assert_eq!(
            validate_checkpoint(
                &captured.0.authority,
                &serde_json::to_vec(&checkpoint).unwrap()
            )
            .unwrap(),
            "Ready"
        );
        checkpoint["chats"][0]["launch"]["session_id"] = uuid::Uuid::new_v4().to_string().into();
        assert!(
            validate_checkpoint(
                &captured.0.authority,
                &serde_json::to_vec(&checkpoint).unwrap()
            )
            .is_err(),
            "a different effective conversation is never recovery authority"
        );
        checkpoint["chats"][0]["launch"]["session_id"] = confirmed.into();
        checkpoint["chats"][0]["launch"]["recipe"]["arguments"] =
            serde_json::json!(["--different-provider-target"]);
        assert!(
            validate_checkpoint(
                &captured.0.authority,
                &serde_json::to_vec(&checkpoint).unwrap()
            )
            .is_err(),
            "normalization cannot change any other launch option"
        );
    }
}

#[test]
fn preexisting_prepared_or_unknown_evidence_is_preserved_before_any_prepare() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let path = &preparation.authority.checkpoint_path;
    require_absent_checkpoint(path).unwrap();
    for bytes in [
        checkpoint(&preparation, "Prepared"),
        b"unknown evidence that must not be replaced".to_vec(),
    ] {
        save(path, &bytes);
        assert!(require_absent_checkpoint(path).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

/// A chat as a busy daemon holds it: a launch environment and arguments
/// of realistic size, a pane presenting it, the alias that pane saved
/// before its conversation ID was confirmed, and `entries` transcript
/// entries of `bytes` each. Every third chat is Codex, as in a mixed
/// daemon of 51 Claude and 30 Codex chats.
fn scale_chat(
    preparation: &Preparation,
    index: usize,
    entries: usize,
    bytes: usize,
) -> (
    Session,
    crate::chat::Snapshot,
    Session,
    super::super::floor::Presentation,
) {
    let terminal = &preparation.authority.census.terminals[0];
    let provider = if index.is_multiple_of(3) {
        crate::agent::Provider::Codex
    } else {
        crate::agent::Provider::Claude
    };
    let mut recipe = terminal.launch.clone();
    recipe.program = format!("/home/person/.local/bin/{}", provider.executable());
    // Eight arguments each provider accepts on an exact resume.
    recipe.arguments = match provider {
        crate::agent::Provider::Codex => [
            "--model",
            "gpt-5-codex",
            "--sandbox",
            "workspace-write",
            "--ask-for-approval",
            "on-request",
            "--config",
            "model_reasoning_effort=high",
        ],
        _ => [
            "--model",
            "claude-sonnet-4-5",
            "--permission-mode",
            "default",
            "--effort",
            "high",
            "--setting-sources",
            "user,project",
        ],
    }
    .map(Into::into)
    .to_vec();
    recipe.environment = [("PATH".to_owned(), "/usr/local/sbin:".repeat(26))]
        .into_iter()
        .chain((0..10).map(|variable| {
            (
                format!("PERSON_SETTING_{variable}"),
                format!("/home/person/.config/tool-{variable}/value"),
            )
        }))
        .collect();
    let session_id = uuid::Uuid::new_v4().to_string();
    let source = Session {
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider,
            recipe,
            session_id: Some(session_id.clone()),
            draft_model: None,
        },
        proof: Some(preparation.authority.source.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let mut alias = source.clone();
    alias.launch.session_id = None;
    let agent = crate::model::AgentId::new();
    let presentation = super::super::floor::Presentation {
        agent,
        session: source.clone(),
        scope: crate::workspace_model::views::Scope::Channel(crate::model::ChannelId::new()),
        pane: crate::workspace_model::views::Pane {
            id: crate::model::CardId::from_uuid(agent.as_uuid()),
            title: format!("Chat {index}"),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: crate::model::CardKind::Chat(alias.clone()),
            scope: None,
        },
        ordinal: index,
        restored: false,
    };
    let snapshot = crate::chat::Snapshot {
        session_id: Some(session_id),
        title: Some(format!("chat {index}")),
        items: (0..entries)
            .map(|entry| {
                std::sync::Arc::new(crate::chat::Item {
                    id: format!("{index}-{entry}"),
                    role: crate::chat::Role::Assistant,
                    content: crate::chat::ItemContent::Text {
                        text: "x".repeat(bytes),
                        thinking: false,
                        phase: Default::default(),
                    },
                    started_at: None,
                    at: None,
                })
            })
            .collect(),
        composer: Some("an unsent draft the paused page never shows".into()),
        commands: vec![
            crate::chat::SlashCommand {
                name: "review".into(),
                description: "A command the paused page never offers".into(),
            };
            40
        ],
        ..Default::default()
    };
    (source, snapshot, alias, presentation)
}

/// `chats` chats captured as the update captures them, each page shaped
/// by `page(index)` as (entries, bytes per entry). The reviewed impact
/// and expected owners of `preparation` include every chat.
fn scale_census(
    preparation: &mut Preparation,
    chats: usize,
    page: impl Fn(usize) -> (usize, usize),
) -> Census {
    let mut census = preparation.authority.census.clone();
    let mut captured = SnapshotCapture::default();
    let mut sessions = Vec::new();
    for index in 0..chats {
        let (entries, bytes) = page(index);
        let (source, snapshot, alias, presentation) =
            scale_chat(preparation, index, entries, bytes);
        sessions.push(crate::update::impact::SessionImpact {
            id: source.id,
            kind: SessionKind::NativeChat,
            title: format!("chat {index}"),
            identity: "0".repeat(64),
            disposition: crate::update::impact::Disposition::Resumes,
            reason: "".into(),
            interrupted_subagents: Vec::new(),
        });
        captured.push(source.clone(), snapshot).unwrap();
        census.chats.push(source);
        census.aliases.push(alias);
        census.presentations.push(presentation);
    }
    census.snapshots = captured.finish();
    let reviewed = preparation.authority.reviewed.as_mut().unwrap();
    reviewed.sessions.extend(sessions);
    preparation.authority.expected = reviewed
        .sessions
        .iter()
        .map(|item| (item.kind, item.id))
        .collect();
    census
}

/// Long chats hold 400 entries of 1.5 KiB (600 KiB of transcript each);
/// every tenth chat is short and must keep its whole page.
fn long_or_short(index: usize) -> (usize, usize) {
    if index.is_multiple_of(10) {
        (6, 200)
    } else {
        (400, 1536)
    }
}

/// Fit `chats` long transcripts and require every bound the census
/// meets before shutdown to accept them, each page to keep its newest
/// contiguous entries, and long pages to share the room evenly. Returns
/// the fewest entries a long page kept.
fn fit_long_transcripts(chats: usize, page: fn(usize) -> (usize, usize)) -> usize {
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    let mut census = scale_census(&mut preparation, chats, page);
    let source = preparation.authority.source.clone();
    let reviewed = preparation.authority.reviewed.clone().unwrap();
    census.fit_pages(&source, &reviewed, &[]).unwrap();
    census.validate_capture(&source).unwrap();
    preparation.authority.census = census.clone();
    preflight_preparation_size(&preparation.authority).unwrap();
    // Recovery saves these pages without shortening them any further.
    let mut record = census.record(&source);
    serde_json::from_slice::<super::super::floor::Record>(&record.encode_capture().unwrap())
        .unwrap()
        .snapshots
        .iter()
        .zip(&census.snapshots)
        .for_each(|(saved, fitted)| assert_eq!(saved, fitted));
    eprintln!(
        "{chats} chats: census {} of {SNAPSHOT_LIMIT}, preparation {} of {PREPARATION_LIMIT}; \
             with empty pages {:?}",
        encoded_size(&census, usize::MAX).unwrap(),
        preparation_size(&preparation.authority).unwrap(),
        census
            .identity_bounds(&source, &reviewed, &[])
            .unwrap()
            .map(|bound| bound.needed),
    );
    let mut kept = Vec::new();
    for (index, saved) in census.snapshots.iter().enumerate() {
        assert_eq!(saved.source, census.chats[index]);
        assert_eq!(saved.snapshot.composer, None);
        assert!(saved.snapshot.commands.is_empty());
        let (entries, _) = page(index);
        let ids: Vec<_> = saved.snapshot.items.iter().map(|item| &item.id).collect();
        let expected: Vec<_> = (entries - ids.len()..entries)
            .map(|entry| format!("{index}-{entry}"))
            .collect();
        assert_eq!(
            ids,
            expected.iter().collect::<Vec<_>>(),
            "chat {index} keeps its newest contiguous entries"
        );
        let limitation = saved.limitation.as_deref().unwrap();
        if ids.len() == entries {
            assert!(!saved.snapshot.has_earlier);
            assert!(!limitation.ends_with(super::super::floor::OLDER_HISTORY_OMITTED));
        } else {
            assert!(saved.snapshot.has_earlier);
            assert!(limitation.ends_with(super::super::floor::OLDER_HISTORY_OMITTED));
        }
        if entries < 400 {
            assert_eq!(ids.len(), entries, "short pages keep everything");
        } else {
            kept.push(ids.len());
        }
    }
    // Fitting is stable: a fitted census needs no further shortening.
    let fitted = census.clone();
    census.fit_pages(&source, &reviewed, &[]).unwrap();
    assert_eq!(census, fitted);
    let fewest = kept.iter().copied().min().unwrap_or(400);
    let most = kept.iter().copied().max().unwrap_or(400);
    eprintln!("{chats} chats: each long page keeps {fewest} to {most} of 400 entries");
    assert!(
        most - fewest <= 1,
        "long pages share evenly: {fewest}..{most}"
    );
    fewest
}

#[test]
fn no_chats_fit_without_pages() {
    assert_eq!(fit_long_transcripts(0, long_or_short), 400);
}

#[test]
fn one_long_chat_keeps_its_whole_page() {
    assert_eq!(fit_long_transcripts(1, |_| (400, 1536)), 400);
}

#[test]
fn long_transcripts_of_81_chats_keep_their_newest_entries() {
    // 72 long chats share about 5.5 MiB: some 50 entries (75 KiB) each.
    assert!(fit_long_transcripts(81, long_or_short) >= 40);
}

#[test]
fn long_transcripts_of_150_chats_keep_their_newest_entries() {
    assert!(fit_long_transcripts(150, long_or_short) >= 15);
}

#[test]
fn long_transcripts_of_300_chats_keep_their_newest_entries() {
    assert!(fit_long_transcripts(300, long_or_short) >= 2);
}

/// Runs the calling test again in a process whose data directories are
/// private, so it can retain stopped-pane evidence. Returns true in the
/// caller, which then has nothing more to do.
fn rerun_with_private_data() -> bool {
    const ROOT: &str = "CAGENTS_TEST_RECOVERY_PRIVATE_DATA";
    if std::env::var_os(ROOT).is_some() {
        return false;
    }
    let root = crate::test_fixture::tempdir().unwrap();
    for directory in ["home", "run", "data", "config", "cache"] {
        fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    let test = std::thread::current().name().unwrap().to_owned();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env(ROOT, root.path())
        .env("HOME", root.path().join("home"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

/// Ending chats before preparation retains their final pages for this
/// source. Recovery replaces those pages with the census's own, so they
/// must not also be charged against the census's budget.
#[test]
fn pages_retained_by_ending_chats_leave_the_census_fit() {
    if rerun_with_private_data() {
        return;
    }
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    let mut census = scale_census(&mut preparation, 81, long_or_short);
    let source = preparation.authority.source.clone();
    let reviewed = preparation.authority.reviewed.clone().unwrap();
    census.fit_pages(&source, &reviewed, &[]).unwrap();
    // Twenty long chats end through the floor with their final pages,
    // which fill the stopped-pane record by themselves.
    let mut ended = super::super::floor::Record::new(source.clone(), Vec::new(), Vec::new());
    for index in (1..81).filter(|index| index % 10 != 0).take(20) {
        let (chat, snapshot, _, _) = scale_chat(&preparation, index, 400, 1536);
        let mut chat = chat;
        chat.id = census.chats[index].id;
        ended.chats.push(chat.clone());
        ended.snapshots.push(super::super::floor::StoppedSnapshot {
            source: chat,
            snapshot,
            limitation: None,
        });
    }
    ended.save().unwrap();
    let retained = super::super::floor::record_for_source(&source)
        .unwrap()
        .unwrap()
        .encode_capture()
        .unwrap()
        .len();
    assert!(
        retained + encoded_size(&census, usize::MAX).unwrap() > SNAPSHOT_LIMIT,
        "the retained pages and the census exceed one budget together"
    );
    census.validate_capture(&source).unwrap();
    let fitted = census.clone();
    census.fit_pages(&source, &reviewed, &[]).unwrap();
    assert_eq!(
        census, fitted,
        "retained pages take nothing from the census"
    );
    // Saving the paused record beside them fits by shortening pages only.
    census.record(&source).save().unwrap();
    let saved = super::super::floor::record_for_source(&source)
        .unwrap()
        .unwrap();
    assert_eq!(saved.chats.len(), 81);
    assert_eq!(saved.snapshots.len(), 81);
}

#[test]
fn capture_shortens_pages_while_it_runs_without_losing_their_fair_share() {
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    // 60 chats of 600 KiB: 35 MiB unless capture compacts as it goes.
    let mut census = scale_census(&mut preparation, 60, |_| (400, 1536));
    let retained: usize = census
        .snapshots
        .iter()
        .map(|saved| encoded_size(&saved.snapshot.items, usize::MAX).unwrap())
        .sum();
    assert!(retained <= CAPTURE_COMPACTION + 600 * 1024, "{retained}");
    let source = preparation.authority.source.clone();
    let reviewed = preparation.authority.reviewed.clone().unwrap();
    census.fit_pages(&source, &reviewed, &[]).unwrap();
    let kept: Vec<_> = census
        .snapshots
        .iter()
        .map(|saved| saved.snapshot.items.len())
        .collect();
    let (fewest, most) = (kept.iter().min().unwrap(), kept.iter().max().unwrap());
    assert!(most - fewest <= 1, "{fewest}..{most}");
    assert!(*fewest >= 60, "{fewest}");
}

#[test]
fn one_huge_entry_does_not_starve_other_pages() {
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    let mut census = scale_census(&mut preparation, 40, |index| {
        if index == 0 {
            (1, 6 * 1024 * 1024)
        } else {
            (50, 1024)
        }
    });
    let source = preparation.authority.source.clone();
    let reviewed = preparation.authority.reviewed.clone().unwrap();
    census.fit_pages(&source, &reviewed, &[]).unwrap();
    census.validate_capture(&source).unwrap();
    assert!(census.snapshots[0].snapshot.items.is_empty());
    assert!(census.snapshots[0].snapshot.has_earlier);
    assert!(
        census.snapshots[1..]
            .iter()
            .all(|saved| saved.snapshot.items.len() == 50)
    );
}

#[test]
fn identity_evidence_that_cannot_fit_says_how_many_chats_to_close() {
    // At 16 KiB only the receipt overflows; at 48 KiB the census and the
    // floor records do too, each with a different share per chat.
    for (environment, failing) in [(16 * 1024, 1), (48 * 1024, 4)] {
        let root = crate::test_fixture::tempdir().unwrap();
        let mut preparation = preparation(root.path());
        let mut census = scale_census(&mut preparation, 120, |_| (2, 64));
        for chat in &mut census.chats {
            chat.launch
                .recipe
                .environment
                .insert("LARGE_CAPTURED_ENVIRONMENT".into(), "x".repeat(environment));
        }
        for (saved, chat) in census.snapshots.iter_mut().zip(&census.chats) {
            saved.source = chat.clone();
        }
        for (presentation, chat) in census.presentations.iter_mut().zip(&census.chats) {
            presentation.session = chat.clone();
        }
        let source = preparation.authority.source.clone();
        let bounds = census
            .identity_bounds(
                &source,
                preparation.authority.reviewed.as_ref().unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(
            bounds.iter().filter(|bound| !bound.fits()).count(),
            failing,
            "{bounds:?}"
        );
        let original = census.clone();
        let error = census
            .fit_pages(
                &source,
                preparation.authority.reviewed.as_ref().unwrap(),
                &[],
            )
            .unwrap_err();
        assert!(
            error.starts_with("This daemon has 120 open chats"),
            "{error}"
        );
        let close: usize = error
            .split("Close at least ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|count| count.parse().ok())
            .unwrap_or_else(|| panic!("{error}"));
        assert!(
            error.contains("Nothing was prepared and no chat was closed"),
            "{error}"
        );
        assert_eq!(census, original, "a refusal shortens nothing");
        // Closing that many leaves a census which fits every bound.
        let keep = 120 - close;
        census.chats.truncate(keep);
        census.snapshots.truncate(keep);
        census.aliases.truncate(keep);
        census.presentations.truncate(keep);
        let reviewed = preparation.authority.reviewed.as_mut().unwrap();
        let kept: BTreeSet<_> = census.chats.iter().map(|chat| chat.id).collect();
        reviewed.sessions.retain(|session| {
            session.kind != SessionKind::NativeChat || kept.contains(&session.id)
        });
        census.fit_pages(&source, reviewed, &[]).unwrap();
        assert!(!preparation.authority.checkpoint_path.exists());
    }
}

#[test]
fn losing_the_continuous_worker_lease_prevents_receipt_even_with_matching_bytes() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let bytes = checkpoint(&preparation, "Prepared");
    save(&preparation.authority.checkpoint_path, &bytes);
    assert_eq!(
        unsafe { libc::flock(preparation.worker.as_raw_fd(), libc::LOCK_UN) },
        0
    );
    assert!(preparation.acknowledged().is_err());
    assert_eq!(
        fs::read(root.path().join("daemon-upgrade.json")).unwrap(),
        bytes
    );
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
)]
fn a_changed_prepared_checkpoint_is_neither_reinterpreted_nor_modified() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let original = checkpoint(&preparation, "Prepared");
    save(&preparation.authority.checkpoint_path, &original);
    let captured = preparation.acknowledged().unwrap();
    let mut changed = original.clone();
    changed.push(b'\n'); // Same parsed inventory, different checkpoint identity.
    save(&captured.0.authority.checkpoint_path, &changed);
    assert!(
        captured
            .receipt(Path::new("/unused"), &handoff(root.path()))
            .is_err()
    );
    assert_eq!(
        fs::read(root.path().join("daemon-upgrade.json")).unwrap(),
        changed
    );
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
)]
fn source_finishing_ready_keeps_normal_restore_and_preserves_checkpoint_bytes() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    save(
        &preparation.authority.checkpoint_path,
        &checkpoint(&preparation, "Prepared"),
    );
    let captured = preparation.acknowledged().unwrap();
    let ready = checkpoint(&captured.0, "Ready");
    save(&captured.0.authority.checkpoint_path, &ready);
    assert!(
        captured
            .receipt(Path::new("/unused"), &handoff(root.path()))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read(root.path().join("daemon-upgrade.json")).unwrap(),
        ready
    );
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
)]
fn recovery_receipt_releases_its_daemon_reservation_before_owner_discovery() {
    const ISOLATED: &str = "CAGENTS_TEST_RECOVERY_RESERVATION_ISOLATED";
    if std::env::var_os(ISOLATED).is_none() {
        // Parallel libtests may fork while this test owns a flock. Even a
        // CLOEXEC duplicate extends that open-file description until exec.
        // Allocate every tested lock only after this single-test exec.
        let selector = std::thread::current().name().unwrap().to_owned();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .env(ISOLATED, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    save(
        &preparation.authority.checkpoint_path,
        &checkpoint(&preparation, "Prepared"),
    );
    let captured = preparation.acknowledged().unwrap();
    let handoff = handoff(root.path());
    let mut receipt = captured
        .receipt(&fs::canonicalize("/bin/sh").unwrap(), &handoff)
        .unwrap()
        .unwrap();
    let path = descriptor_path(receipt.daemon.as_ref().unwrap()).unwrap();
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    drop(handoff);
    let held = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    let held_error = io::Error::last_os_error();
    assert_eq!(held, -1, "the receipt must retain the daemon lease");
    assert_eq!(held_error.kind(), io::ErrorKind::WouldBlock, "{held_error}");
    receipt.release_daemon_reservation();
    let released = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    let released_error = io::Error::last_os_error();
    assert_eq!(
        released, 0,
        "the daemon lease was not released: {released_error}"
    );
    assert!(receipt.inherit(&mut Command::new("true")).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn recovery_receipt_is_immutable_after_its_causal_identity_is_sealed() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let mut receipt = sealed_receipt(&preparation.authority).unwrap();
    assert_eq!(
        unsafe { libc::fcntl(receipt.as_raw_fd(), libc::F_GET_SEALS) } & SEALS,
        SEALS
    );
    assert_eq!(
        receipt
            .write_all(b"replacement")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
}

#[test]
fn mutable_files_and_invalid_descriptors_cannot_pass_the_receipt_seal_check() {
    let file = tempfile::tempfile().unwrap();
    assert!(validate_sealed_descriptor(file.as_raw_fd()).is_err());
    assert!(validate_sealed_descriptor(-1).is_err());
}

#[test]
fn recovered_owner_verification_rejects_same_id_with_a_different_recipe_or_conversation() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let known = preparation.authority.census.terminals[0].clone();
    let mut serving = preparation.authority.source.clone();
    serving.host_incarnation = HostIncarnation::new("successor");
    let mut actual =
        crate::terminal_ipc::TerminalDescriptor::relaunchable(known.id, known.launch.clone(), None);
    actual.session = known.clone();
    actual.session.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: serving.host_session_id.clone(),
        boot_id: serving.boot_id.clone(),
        host_incarnation: serving.host_incarnation.clone(),
    };
    actual.upgrade_sources.push(known.clone());
    assert!(verified_terminal(
        &preparation.authority.census,
        &actual,
        &serving,
        Path::new("/candidate/bin/cagentsd")
    ));
    actual.session.launch.arguments.push("unrelated".into());
    assert!(!verified_terminal(
        &preparation.authority.census,
        &actual,
        &serving,
        Path::new("/candidate/bin/cagentsd")
    ));
    let before = Session {
        id: known.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: known.launch.clone(),
            session_id: Some("original-thread".into()),
            draft_model: None,
        },
        proof: Some(preparation.authority.source.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: crate::chat::NativeContinuity::Durable {
            provider_version: "fixture".into(),
        },
    };
    let mut after = before.clone();
    after.proof = Some(serving.clone());
    assert!(recovered_chat(&before, &after, &serving));
    after.launch.session_id = Some("another-thread".into());
    assert!(!recovered_chat(&before, &after, &serving));
    after.launch.session_id = before.launch.session_id.clone();
    after.launch.provider = crate::agent::Provider::Claude;
    assert!(!recovered_chat(&before, &after, &serving));
}

#[test]
fn prepared_recovery_verifies_the_exact_retained_console_replacement_lineage() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let mut known = preparation.authority.census.terminals[0].clone();
    let agent = crate::model::AgentId::new();
    known.launch.program = "/original/bin/cagents".into();
    known.launch.arguments = vec!["agent".into(), "console".into(), agent.to_string()];
    let source = Session {
        id: known.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: Vec::new(),
                ..known.launch.clone()
            },
            session_id: Some("retained-conversation".into()),
            draft_model: None,
        },
        proof: Some(preparation.authority.source.clone()),
        terminal: Some(Box::new(known.clone())),
        owned_codex_listener: None,
        native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
    };
    let census = Census {
        terminals: vec![known.clone()],
        chats: vec![source.clone()],
        snapshots: Vec::new(),
        ..Default::default()
    };
    let mut serving = preparation.authority.source.clone();
    serving.host_incarnation = HostIncarnation::new("successor");
    let target = Path::new("/candidate/bin/cagentsd");
    let mut actual =
        crate::terminal_ipc::TerminalDescriptor::relaunchable(known.id, known.launch.clone(), None);
    actual.session = known.clone();
    actual.session.launch.program = "/candidate/bin/cagents".into();
    actual.session.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: serving.host_session_id.clone(),
        boot_id: serving.boot_id.clone(),
        host_incarnation: serving.host_incarnation.clone(),
    };
    actual.upgrade_sources.push(known.clone());
    actual
        .upgrade_relaunches
        .push(crate::terminal_ipc::UpgradeRelaunch {
            source: known.clone(),
            recipe: actual.session.launch.clone(),
        });
    let mut chat = source.clone();
    chat.proof = Some(serving.clone());
    chat.terminal = Some(Box::new(actual.session.clone()));
    assert!(verified_terminal(&census, &actual, &serving, target));
    assert!(verified_chat(
        &census,
        &chat,
        &serving,
        &[actual.clone()],
        target
    ));
    assert!(!verified_chat(&census, &chat, &serving, &[], target));

    // Retirement validates the same exact issued console replacement,
    // including a later serving proof; raw old/new recipe equality would
    // reject this supported replacement after it had already settled.
    let mut record: RecoveryRecord = serde_json::from_value(serde_json::json!({
        "version": 2, "job": preparation.authority.job,
        "request_identity":"unused-in-lineage-test", "job_request_hash":"unused",
        "source": preparation.authority.source, "checkpoint_hash":"unused",
        "census": census, "adopted_terminals":[known.id], "adopted_chats":[known.id],
        "adopted_chat_descriptors":[chat], "adopted_terminal_descriptors":[actual],
        "paused_terminals":[], "paused_chats":[], "settled":true,
        "successor":serving, "target_hash":"unused", "target":target,
    }))
    .unwrap();
    record.validate_issued_chats().unwrap();
    let mut later = chat.clone();
    later.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("later-console-owner");
    let proof = later.proof.as_ref().unwrap();
    later.terminal.as_mut().unwrap().continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    record
        .validate_additional_issued_chats(&[later.clone()])
        .unwrap();
    let mut wrong_target = later.clone();
    wrong_target.terminal.as_mut().unwrap().launch.program = "/foreign/bin/cagents".into();
    assert!(
        record
            .validate_additional_issued_chats(&[wrong_target.clone()])
            .is_err()
    );
    let mut wrong_receipt_target: RecoveryRecord =
        serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
    wrong_receipt_target.target = Some("/foreign/bin/cagentsd".into());
    assert!(
        wrong_receipt_target.validate_issued_chats().is_err(),
        "descriptor normalization is limited to the receipt's exact target executable"
    );
    let immutable_copy: RecoveryRecord =
        serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
    let mut settled = recovered_data(
        immutable_copy,
        &root.path().join("pure-checkpoint.json"),
        target.to_path_buf(),
        vec![later.clone()],
        "pure-outcome".into(),
    );
    assert!(current_recovered_chat(
        &source,
        &settled.normalized_chat(&later),
        proof
    ));
    assert!(
        !current_recovered_chat(&source, &settled.normalized_chat(&wrong_target), proof),
        "current inventory cannot normalize another installed console binary"
    );
    let invalid: [fn(&mut Session); 11] = [
        |chat| chat.launch.session_id = Some("foreign-thread".into()),
        |chat| chat.proof = None,
        |chat| chat.proof.as_mut().unwrap().boot_id = BootId::new("foreign-boot"),
        |chat| chat.id = TerminalId::new(),
        |chat| chat.launch.recipe.arguments.push("foreign".into()),
        |chat| chat.terminal.as_mut().unwrap().launch.program = "/foreign/bin/cagents".into(),
        |chat| {
            chat.terminal.as_mut().unwrap().launch.arguments[2] =
                crate::model::AgentId::new().to_string()
        },
        |chat| {
            chat.terminal
                .as_mut()
                .unwrap()
                .launch
                .arguments
                .push("foreign".into())
        },
        |chat| {
            chat.terminal
                .as_mut()
                .unwrap()
                .launch
                .working_directory
                .push("foreign")
        },
        |chat| {
            chat.terminal
                .as_mut()
                .unwrap()
                .launch
                .environment
                .insert("OTHER".into(), "foreign".into());
        },
        |chat| {
            chat.terminal
                .as_mut()
                .unwrap()
                .launch
                .unset_environment
                .insert("OTHER".into());
        },
    ];
    for (index, mutate) in invalid.into_iter().enumerate() {
        let mut changed = later.clone();
        mutate(&mut changed);
        assert!(
            record
                .validate_additional_issued_chats(&[changed.clone()])
                .is_err()
        );
        if index != 0 {
            assert!(!current_recovered_chat(
                &source,
                &settled.normalized_chat(&changed),
                proof
            ));
        } else {
            // /clear may preserve a real current owner, but its different
            // thread cannot alias the original captured conversation.
            assert!(current_recovered_chat(
                &source,
                &settled.normalized_chat(&changed),
                proof
            ));
            assert!(!captured_chat_alias(
                &census,
                &source,
                &settled.normalized_chat(&changed),
                proof
            ));
        }
    }
    settled.witness.adopted_terminals[0]
        .upgrade_relaunches
        .clear();
    assert!(
        !current_recovered_chat(&source, &settled.normalized_chat(&later), proof),
        "current inventory retains the immutable source/relaunch proof"
    );
    let mut foreign = later.clone();
    foreign
        .terminal
        .as_mut()
        .unwrap()
        .launch
        .arguments
        .push("foreign".into());
    assert!(record.validate_additional_issued_chats(&[foreign]).is_err());
    record.adopted_terminal_descriptors[0]
        .upgrade_relaunches
        .clear();
    assert!(record.validate_issued_chats().is_err());

    let mut changed = actual.clone();
    changed.upgrade_relaunches.clear();
    assert!(!verified_terminal(&census, &changed, &serving, target));
    changed = actual.clone();
    changed.upgrade_sources.clear();
    assert!(!verified_terminal(&census, &changed, &serving, target));
    changed = actual.clone();
    changed.upgrade_relaunches[0].source.id = TerminalId::new();
    assert!(!verified_terminal(&census, &changed, &serving, target));
    changed = actual.clone();
    changed.upgrade_relaunches[0]
        .recipe
        .arguments
        .push("another-command".into());
    assert!(!verified_terminal(&census, &changed, &serving, target));
    assert!(!verified_terminal(
        &census,
        &actual,
        &serving,
        Path::new("/unrelated/bin/cagentsd")
    ));
    let mutations: [fn(&mut LaunchRecipe); 4] = [
        |recipe: &mut LaunchRecipe| recipe.arguments.push("another-command".into()),
        |recipe: &mut LaunchRecipe| recipe.working_directory.push("another-directory"),
        |recipe: &mut LaunchRecipe| {
            recipe.environment.insert("OTHER".into(), "value".into());
        },
        |recipe: &mut LaunchRecipe| {
            recipe.unset_environment.insert("OTHER".into());
        },
    ];
    for mutate in mutations {
        changed = actual.clone();
        mutate(&mut changed.session.launch);
        changed.upgrade_relaunches[0].recipe = changed.session.launch.clone();
        assert!(!verified_terminal(&census, &changed, &serving, target));
    }
    let mut unpaired = census.clone();
    unpaired.chats.clear();
    assert!(!verified_terminal(&unpaired, &actual, &serving, target));
    unpaired = census.clone();
    unpaired.chats[0].native_continuity = Default::default();
    assert!(!verified_terminal(&unpaired, &actual, &serving, target));
    chat.terminal
        .as_mut()
        .unwrap()
        .launch
        .environment
        .insert("OTHER".into(), "value".into());
    assert!(!verified_chat(
        &census,
        &chat,
        &serving,
        &[actual.clone()],
        target
    ));
    chat.terminal = Some(Box::new(actual.session.clone()));
    chat.launch.session_id = Some("different-conversation".into());
    assert!(!verified_chat(&census, &chat, &serving, &[actual], target));
}

#[test]
fn recovery_outcome_requires_disjoint_complete_inventory_including_paired_terminals() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let terminal = preparation.authority.census.terminals[0].clone();
    let mut record = RecoveryRecord {
        version: 2,
        origin: preparation.authority.origin.clone(),
        job: preparation.authority.job.clone(),
        request_identity: preparation.authority.request_identity.clone(),
        job_request_hash: preparation.authority.job_request_hash.clone(),
        reviewed: preparation.authority.reviewed.clone(),
        windows: preparation.authority.windows.clone(),
        source: preparation.authority.source.clone(),
        checkpoint_hash: String::new(),
        census: preparation.authority.census.clone(),
        adopted_terminals: Vec::new(),
        adopted_chats: Vec::new(),
        adopted_chat_descriptors: Vec::new(),
        adopted_terminal_descriptors: Vec::new(),
        paused_terminals: vec![terminal.id],
        paused_chats: Vec::new(),
        settled: false,
        successor: None,
        target_hash: String::new(),
        target: None,
        #[cfg(feature = "host")]
        authority: None,
    };
    assert!(record.validate_census().is_ok());
    record.paused_terminals.clear();
    assert!(
        record.validate_census().is_err(),
        "missing owners cannot disappear from the outcome"
    );
    record.adopted_terminals = vec![terminal.id, TerminalId::new()];
    assert!(
        record.validate_census().is_err(),
        "an unrelated holder cannot substitute for an original owner"
    );
    record.adopted_terminals = vec![terminal.id];
    record.paused_terminals = vec![terminal.id];
    assert!(
        record.validate_census().is_err(),
        "an original owner cannot be both live and paused"
    );
    record.paused_terminals.clear();
    // A retained native owner can adopt its paired terminal internally,
    // although that console was absent from the independent terminal list.
    record.census.terminals.clear();
    record.census.chats.push(Session {
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: terminal.launch.clone(),
            session_id: Some("thread".into()),
            draft_model: None,
        },
        proof: Some(record.source.clone()),
        terminal: Some(Box::new(terminal.clone())),
        owned_codex_listener: None,
        native_continuity: Default::default(),
    });
    record.paused_chats.push(terminal.id);
    assert!(
        record.validate_census().is_ok(),
        "paired console adoption is part of the original chat's census"
    );
}
#[test]
fn issued_chat_aliases_bind_the_original_allocation_and_each_serving_proof() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let terminal = &preparation.authority.census.terminals[0];
    let known = Session {
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: "/offline/claude".into(),
                arguments: Vec::new(),
                ..terminal.launch.clone()
            },
            session_id: Some("11111111-1111-4111-8111-111111111111".into()),
            draft_model: None,
        },
        proof: Some(preparation.authority.source.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
    };
    let mut issued = known.clone();
    issued.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("first-successor");
    let mut later = issued.clone();
    later.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("later-successor");
    let mut record = RecoveryRecord {
        version: 2,
        origin: Origin::AcknowledgedPrepare,
        windows: Vec::new(),
        adopted_terminal_descriptors: Vec::new(),
        job: preparation.authority.job.clone(),
        request_identity: String::new(),
        job_request_hash: String::new(),
        reviewed: preparation.authority.reviewed.clone(),
        target: None,
        source: preparation.authority.source.clone(),
        checkpoint_hash: String::new(),
        census: Census {
            chats: vec![known],
            ..Default::default()
        },
        adopted_terminals: Vec::new(),
        adopted_chats: vec![issued.id],
        adopted_chat_descriptors: vec![issued.clone()],
        paused_terminals: Vec::new(),
        paused_chats: Vec::new(),
        settled: true,
        successor: issued.proof.clone(),
        target_hash: String::new(),
        #[cfg(feature = "host")]
        authority: None,
    };
    record.adopted_chat_descriptors = vec![issued.clone()];
    record.validate_issued_chats().unwrap();
    record
        .validate_additional_issued_chats(&[later.clone()])
        .unwrap();
    record.adopted_chat_descriptors = vec![later.clone()];
    assert!(
        record.validate_issued_chats().is_err(),
        "original outcome descriptors must use its original successor proof"
    );
    record.adopted_chat_descriptors = vec![issued.clone()];
    for mutation in ["thread", "recipe", "boot", "proof", "duplicate"] {
        let mut additional = vec![later.clone()];
        match mutation {
            "thread" => additional[0].launch.session_id = Some("unrelated".into()),
            "recipe" => additional[0]
                .launch
                .recipe
                .arguments
                .push("different".into()),
            "boot" => additional[0].proof.as_mut().unwrap().boot_id = BootId::new("other-boot"),
            "proof" => additional[0].proof = None,
            "duplicate" => additional.push(issued.clone()),
            _ => unreachable!(),
        }
        assert!(
            record
                .validate_additional_issued_chats(&additional)
                .is_err(),
            "accepted changed {mutation}"
        );
    }
    record.adopted_chat_descriptors.clear();
    assert!(
        record.validate_issued_chats().is_err(),
        "adopted owners need their exact original descriptor"
    );
    record.adopted_chat_descriptors = vec![issued.clone()];
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let bin = root.path().join("install/versions/fixture/bin");
    fs::create_dir_all(&bin).unwrap();
    let prefix = root.path().join("install");
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    for executable in ["cagents", "cagents-ui", "cagentsd"] {
        fs::write(bin.join(executable), "private fake binary").unwrap();
    }
    let target = fs::canonicalize(bin.join("cagentsd")).unwrap();
    let state = prefix.join(".update");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let request = b"original immutable native update request";
    save(&state.join(format!("{}.request", record.job)), request);
    record.reviewed.as_mut().unwrap().sessions[0].kind = SessionKind::NativeChat;
    record.request_identity =
        digest(&serde_json::to_vec(record.reviewed.as_ref().unwrap()).unwrap());
    record.job_request_hash = digest(request);
    record.target = Some(target.clone());
    record.target_hash = executable_hash(&target).unwrap();
    let known = record.census.chats[0].clone();
    let mut captured = SnapshotCapture::default();
    captured
        .push(
            known.clone(),
            crate::chat::Snapshot {
                session_id: known.launch.session_id.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    record.census.snapshots = captured.finish();
    let checkpoint = serde_json::to_vec(&serde_json::json!({
        "version":5,"phase":"Prepared","proof":record.source,"terminals":[],
        "chats":[{"source":known,"launch":known.launch}],
    }))
    .unwrap();
    record.checkpoint_hash = digest(&checkpoint);
    let path = &preparation.authority.checkpoint_path;
    save(path, &checkpoint);
    let original = serde_json::to_vec(&record).unwrap();
    let outcome_path = path.with_file_name(recovery_name(&record.checkpoint_hash));
    save(&outcome_path, &original);
    // Retirement has its own namespace: ordinary issued chat lineage
    // must remain readable and byte-identical across every pointer advance.
    let journal = super::super::handoff::Store::open(path.parent().unwrap()).unwrap();
    journal
        .record_recovered(&[super::super::handoff::RecoveredChat {
            evidence: super::super::handoff::Chat {
                source: known.clone(),
                previous_sources: Vec::new(),
                launch: known.launch.clone(),
            },
            replacement: issued.clone(),
            aliases: Vec::new(),
        }])
        .unwrap();
    let journal_path = path.with_file_name("daemon-upgrade.recovered.json");
    let journal_bytes = fs::read(&journal_path).unwrap();
    assert!(journal.recovered_chats().unwrap().proves(
        &known,
        &issued,
        issued.proof.as_ref().unwrap()
    ));
    let classified = classify_retained_prepared(path, &target).unwrap().unwrap();
    classified
        .archive_completed(
            issued.proof.as_ref().unwrap(),
            &[],
            &[issued.clone()],
            &[],
            &[],
        )
        .unwrap();
    let first = archived_proof(path, &target).unwrap().unwrap();
    let first_bytes = fs::read(&first.retirement_path).unwrap();
    classified
        .archive_completed(
            later.proof.as_ref().unwrap(),
            &[],
            &[later.clone()],
            &[],
            &[],
        )
        .unwrap();
    let second = archived_proof(path, &target).unwrap().unwrap();
    assert_ne!(first.retirement_sha256, second.retirement_sha256);
    assert_eq!(fs::read(&first.retirement_path).unwrap(), first_bytes);
    assert_eq!(fs::read(&outcome_path).unwrap(), original);
    assert_eq!(fs::read(&second.archive_path).unwrap(), checkpoint);
    let retired: Retirement =
        serde_json::from_slice(&fs::read(&second.retirement_path).unwrap()).unwrap();
    assert_eq!(retired.previous.as_ref(), Some(&first.retirement_sha256));
    assert!(
        archived_discovery(path, &target)
            .unwrap()
            .unwrap()
            .adopted_chat_descriptors
            .contains(&later)
    );

    let mut rotated = later.clone();
    rotated.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("third-successor");
    rotated.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
    assert!(current_recovered_chat(
        &known,
        &rotated,
        rotated.proof.as_ref().unwrap()
    ));
    assert!(!recovered_chat(
        &known,
        &rotated,
        rotated.proof.as_ref().unwrap()
    ));
    classified
        .archive_completed(
            rotated.proof.as_ref().unwrap(),
            &[],
            &[rotated.clone()],
            &[],
            &[],
        )
        .unwrap();
    assert!(
        !archived_discovery(path, &target)
            .unwrap()
            .unwrap()
            .adopted_chat_descriptors
            .contains(&rotated)
    );
    for mutation in ["provider", "recipe", "proof"] {
        let mut invalid = rotated.clone();
        match mutation {
            "provider" => invalid.launch.provider = crate::agent::Provider::Codex,
            "recipe" => invalid
                .launch
                .recipe
                .arguments
                .push("changed static option".into()),
            "proof" => invalid.proof = issued.proof.clone(),
            _ => unreachable!(),
        }
        assert!(
            classified
                .archive_completed(rotated.proof.as_ref().unwrap(), &[], &[invalid], &[], &[])
                .is_err(),
            "accepted changed {mutation}"
        );
    }
    let mut unknown = known.clone();
    unknown.launch.session_id = None;
    let mut unknown_census = record.census.clone();
    unknown_census.chats[0] = unknown.clone();
    unknown_census.snapshots[0].source = unknown.clone();
    assert!(!captured_chat_alias(
        &unknown_census,
        &unknown,
        &rotated,
        rotated.proof.as_ref().unwrap()
    ));
    assert_eq!(fs::read(&outcome_path).unwrap(), original);
    assert_eq!(fs::read(&journal_path).unwrap(), journal_bytes);
    assert!(journal.recovered_chats().unwrap().proves(
        &known,
        &issued,
        issued.proof.as_ref().unwrap()
    ));
}

#[test]
fn reviewed_payload_budget_is_rejected_before_source_preparation() {
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    preflight_preparation_size(&preparation.authority).unwrap();
    preparation.authority.reviewed.as_mut().unwrap().sessions[0].reason =
        "x".repeat(LIMIT as usize).into();
    let error = preflight_preparation_size(&preparation.authority).unwrap_err();
    assert!(error.contains("byte limit"), "{error}");
    assert!(!preparation.authority.checkpoint_path.exists());
    assert!(preparation.authority.checkpoint_hash.is_empty());
}

#[test]
fn preparation_budget_includes_future_adopted_descriptor_copies_and_checkpoint_lineage() {
    let root = crate::test_fixture::tempdir().unwrap();
    let mut preparation = preparation(root.path());
    preparation.authority.census.terminals[0]
        .launch
        .environment
        .insert(
            "LARGE_CAPTURED_ENVIRONMENT".into(),
            "x".repeat(4 * 1024 * 1024),
        );
    // A receipt alone fits. Its later descriptor also carries the current
    // allocation and checkpoint-issued sources, so preflight must reserve
    // those copies before any source shutdown can be requested.
    encoded_size(&preparation.authority, LIMIT as usize).unwrap();
    assert!(preflight_preparation_size(&preparation.authority).is_err());
    assert!(!preparation.authority.checkpoint_path.exists());
}

#[test]
fn settled_prepared_is_read_only_history_without_active_update_authority() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let checkpoint_bytes = checkpoint(&preparation, "Prepared");
    let authority = preparation.authority.clone();
    drop(preparation); // Cold startup has no worker or window fence authority.
    save(&authority.checkpoint_path, &checkpoint_bytes);
    let prefix = root.path().join("install");
    let bin = prefix.join("versions/fixture/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    for name in ["cagents", "cagents-ui", "cagentsd"] {
        fs::write(bin.join(name), format!("private fake {name}")).unwrap();
    }
    let target = fs::canonicalize(bin.join("cagentsd")).unwrap();
    let state = prefix.join(".update");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let request = b"the immutable original job request";
    save(&state.join(format!("{}.request", authority.job)), request);
    let successor = HostSessionProof {
        host_session_id: HostSessionId::new("first-recovered-daemon"),
        boot_id: authority.source.boot_id.clone(),
        host_incarnation: HostIncarnation::new("recovered-incarnation"),
    };
    let record = RecoveryRecord {
        version: 2,
        origin: authority.origin,
        job: authority.job,
        request_identity: digest(
            &serde_json::to_vec(authority.reviewed.as_ref().unwrap()).unwrap(),
        ),
        job_request_hash: digest(request),
        reviewed: authority.reviewed,
        windows: authority.windows,
        source: authority.source,
        checkpoint_hash: digest(&checkpoint_bytes),
        census: authority.census,
        adopted_terminals: Vec::new(),
        adopted_chats: Vec::new(),
        adopted_chat_descriptors: Vec::new(),
        adopted_terminal_descriptors: Vec::new(),
        paused_terminals: vec![authority.expected[0].1],
        paused_chats: Vec::new(),
        settled: true,
        successor: Some(successor),
        target: Some(target.clone()),
        target_hash: executable_hash(&target).unwrap(),
        #[cfg(feature = "host")]
        authority: None,
    };
    let sidecar = authority
        .checkpoint_path
        .with_file_name(recovery_name(&record.checkpoint_hash));
    let original = serde_json::to_vec(&record).unwrap();
    save(&sidecar, &original);
    // No status.json or update lock exists; a later plain restart uses the
    // original immutable request and exact finished outcome only.
    for _ in 0..2 {
        let classified = classify_retained_prepared(&authority.checkpoint_path, &target)
            .unwrap()
            .unwrap();
        assert_eq!(classified.source, record.source);
        assert_eq!(classified.census, record.census);
        assert_eq!(classified.paused_terminals, record.paused_terminals);
        assert!(classified.paused_chats.is_empty());
        assert_eq!(
            fs::read(&authority.checkpoint_path).unwrap(),
            checkpoint_bytes
        );
        assert_eq!(fs::read(&sidecar).unwrap(), original);
    }
    let baseline: serde_json::Value = serde_json::from_slice(&original).unwrap();
    // Explicit manual recovery has its own immutable request, not an
    // invented source review or ACK. Cold classification validates both
    // origins exhaustively through the same owner.
    let manual = ManualRequest {
        version: 1,
        job: record.job.clone(),
        checkpoint: record.checkpoint_hash.clone(),
        source: record.source.clone(),
        target: target.clone(),
        target_hash: record.target_hash.clone(),
        census_hash: digest(&serde_json::to_vec(&record.census).unwrap()),
        windows: record.windows.clone(),
    };
    let manual_request = serde_json::to_vec(&manual).unwrap();
    let mut explicit = baseline.clone();
    explicit["origin"] =
        serde_json::json!({"kind":"explicit_recovery","checkpoint":record.checkpoint_hash});
    explicit["reviewed"] = serde_json::Value::Null;
    explicit["job_request_hash"] = digest(&manual_request).into();
    explicit["request_identity"] = digest(&manual_request).into();
    let request_path = state.join(format!("{}.request", record.job));
    save(&request_path, &manual_request);
    save(&sidecar, &serde_json::to_vec(&explicit).unwrap());
    assert!(
        classify_retained_prepared(&authority.checkpoint_path, &target)
            .unwrap()
            .is_some()
    );
    let mut mixed = explicit.clone();
    mixed["reviewed"] = baseline["reviewed"].clone();
    save(&sidecar, &serde_json::to_vec(&mixed).unwrap());
    assert!(
        classify_retained_prepared(&authority.checkpoint_path, &target).is_err(),
        "manual recovery must not invent an acknowledged review"
    );
    mixed = explicit;
    mixed["origin"] = serde_json::json!({"kind":"acknowledged_prepare"});
    save(&sidecar, &serde_json::to_vec(&mixed).unwrap());
    assert!(
        classify_retained_prepared(&authority.checkpoint_path, &target).is_err(),
        "automatic recovery requires its real reviewed source payload"
    );
    save(&request_path, request);
    save(&sidecar, &original);
    for (field, replacement) in [
        ("version", serde_json::json!(1)),
        ("settled", serde_json::json!(false)),
        ("job", serde_json::json!(uuid::Uuid::new_v4().to_string())),
        ("request_identity", serde_json::json!("0".repeat(64))),
        ("job_request_hash", serde_json::json!("0".repeat(64))),
        ("checkpoint_hash", serde_json::json!("0".repeat(64))),
        (
            "source",
            serde_json::to_value(record.successor.as_ref().unwrap()).unwrap(),
        ),
        ("successor", serde_json::Value::Null),
        ("successor", serde_json::to_value(&record.source).unwrap()),
        (
            "successor",
            serde_json::json!({
                "host_session_id": "recovered-session",
                "boot_id": "different-boot",
                "host_incarnation": "recovered-daemon",
            }),
        ),
        ("target", serde_json::to_value(bin.join("cagents")).unwrap()),
        ("target_hash", serde_json::json!("0".repeat(64))),
        ("paused_terminals", serde_json::json!([])),
        (
            "adopted_terminals",
            serde_json::json!(record.paused_terminals),
        ),
    ] {
        let mut changed = baseline.clone();
        changed[field] = replacement;
        save(&sidecar, &serde_json::to_vec(&changed).unwrap());
        assert!(
            classify_retained_prepared(&authority.checkpoint_path, &target).is_err(),
            "accepted changed {field}"
        );
        assert_eq!(
            fs::read(&authority.checkpoint_path).unwrap(),
            checkpoint_bytes
        );
    }
    let mut duplicate = baseline.clone();
    duplicate["census"]["terminals"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&record.census.terminals[0]).unwrap());
    save(&sidecar, &serde_json::to_vec(&duplicate).unwrap());
    assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
    save(&sidecar, &original);
    let original_target = fs::read(&target).unwrap();
    fs::write(&target, b"different daemon bytes at the same path").unwrap();
    assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
    fs::write(&target, original_target).unwrap();
    let request_path = state.join(format!("{}.request", record.job));
    save(&request_path, b"a different request for the same job");
    assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
    save(&request_path, request);
    save(&sidecar, &original);
    fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
    fs::remove_file(&sidecar).unwrap();
    assert!(classify_retained_prepared(&authority.checkpoint_path, &target).is_err());
    assert_eq!(
        fs::read(&authority.checkpoint_path).unwrap(),
        checkpoint_bytes
    );
    // Archival is a byte-for-byte no-clobber rename. A conflicting entry
    // cannot be replaced, including a symlink to these same bytes.
    save(&sidecar, &original);
    let classified = classify_retained_prepared(&authority.checkpoint_path, &target)
        .unwrap()
        .unwrap();
    assert!(
        classified
            .archive_completed(record.successor.as_ref().unwrap(), &[], &[], &[], &[])
            .is_err(),
        "missing current owners require durable paused evidence before archival"
    );
    assert_eq!(
        fs::read(&authority.checkpoint_path).unwrap(),
        checkpoint_bytes
    );
    let known = record.census.terminals[0].clone();
    let serving = record.successor.as_ref().unwrap();
    let mut unexpectedly_live =
        crate::terminal_ipc::TerminalDescriptor::relaunchable(known.id, known.launch.clone(), None);
    unexpectedly_live.session = known.clone();
    unexpectedly_live.session.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: serving.host_session_id.clone(),
        boot_id: serving.boot_id.clone(),
        host_incarnation: serving.host_incarnation.clone(),
    };
    unexpectedly_live.upgrade_sources.push(known);
    assert!(verified_terminal(
        &record.census,
        &unexpectedly_live,
        serving,
        &target
    ));
    assert!(
        classified
            .archive_completed(
                serving,
                &[unexpectedly_live],
                &[],
                &record.paused_terminals,
                &record.paused_chats
            )
            .is_err(),
        "an originally paused owner cannot silently become adopted"
    );
    let archive = archive_path(&authority.checkpoint_path, &record.checkpoint_hash);
    save(&archive, b"unrelated archive evidence");
    assert!(
        classified
            .archive_completed(
                record.successor.as_ref().unwrap(),
                &[],
                &[],
                &record.paused_terminals,
                &record.paused_chats
            )
            .is_err()
    );
    assert_eq!(fs::read(&archive).unwrap(), b"unrelated archive evidence");
    fs::remove_file(&archive).unwrap();
    std::os::unix::fs::symlink(&authority.checkpoint_path, &archive).unwrap();
    assert!(
        classified
            .archive_completed(
                record.successor.as_ref().unwrap(),
                &[],
                &[],
                &record.paused_terminals,
                &record.paused_chats
            )
            .is_err()
    );
    fs::remove_file(&archive).unwrap();
    assert_eq!(
        fs::read(&authority.checkpoint_path).unwrap(),
        checkpoint_bytes
    );

    // An older settled outcome which adopted this ID must not override
    // the active later outcome that explicitly paused it.
    let mut earlier: RecoveryRecord = serde_json::from_slice(&original).unwrap();
    earlier.checkpoint_hash = digest(b"earlier unrelated Prepared bytes");
    earlier.adopted_terminals = earlier.paused_terminals.clone();
    earlier.paused_terminals.clear();
    atomic_metadata(
        &authority
            .checkpoint_path
            .with_file_name(recovery_name(&earlier.checkpoint_hash)),
        &earlier,
    )
    .unwrap();
    let earlier_bytes = serde_json::to_vec(&earlier).unwrap();
    let earlier_retired = Retirement {
        version: 1,
        checkpoint_hash: earlier.checkpoint_hash.clone(),
        outcome_hash: digest(&earlier_bytes),
        source: earlier.source.clone(),
        job: earlier.job.clone(),
        request_identity: earlier.request_identity.clone(),
        archive_path: archive_path(&authority.checkpoint_path, &earlier.checkpoint_hash),
        adopted_chat_descriptors: Vec::new(),
        previous: None,
    };
    let earlier_retirement_bytes = serde_json::to_vec(&earlier_retired).unwrap();
    let earlier_retirement_hash = digest(&earlier_retirement_bytes);
    save(
        &retirement_path(&authority.checkpoint_path, &earlier_retirement_hash),
        &earlier_retirement_bytes,
    );
    atomic_metadata(
        &authority.checkpoint_path.with_file_name(ACTIVE_RECOVERY),
        &ActiveRecovery {
            version: 2,
            checkpoint_hash: earlier.checkpoint_hash,
            retirement_hash: earlier_retirement_hash,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        archived_discovery(&authority.checkpoint_path, &target)
            .unwrap()
            .is_none()
    );

    // Simulate a crash after the durable plan but before rename. Ordinary
    // classification remains retryable and nothing reads archive recipes.
    let planned = Retirement {
        version: 1,
        checkpoint_hash: record.checkpoint_hash.clone(),
        outcome_hash: digest(&original),
        source: record.source.clone(),
        job: record.job.clone(),
        request_identity: record.request_identity.clone(),
        archive_path: archive.clone(),
        adopted_chat_descriptors: Vec::new(),
        previous: None,
    };
    let planned_bytes = serde_json::to_vec(&planned).unwrap();
    let planned_hash = digest(&planned_bytes);
    let planned_path = retirement_path(&authority.checkpoint_path, &planned_hash);
    save(&planned_path, &planned_bytes);
    atomic_metadata(
        &authority.checkpoint_path.with_file_name(ACTIVE_RECOVERY),
        &ActiveRecovery {
            version: 2,
            checkpoint_hash: record.checkpoint_hash.clone(),
            retirement_hash: planned_hash.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    let inode = fs::metadata(&authority.checkpoint_path).unwrap().ino();
    classified
        .archive_completed(
            record.successor.as_ref().unwrap(),
            &[],
            &[],
            &record.paused_terminals,
            &record.paused_chats,
        )
        .unwrap();
    assert_eq!(
        fs::read(&sidecar).unwrap(),
        original,
        "archival must never rewrite the original settled outcome"
    );
    assert_eq!(fs::read(&planned_path).unwrap(), planned_bytes);
    let proof = archived_proof(&authority.checkpoint_path, &target)
        .unwrap()
        .unwrap();
    assert_eq!(proof.outcome_sha256, digest(&original));
    assert_eq!(proof.checkpoint_sha256, digest(&checkpoint_bytes));
    assert_eq!(proof.retirement_sha256, planned_hash);
    assert!(!authority.checkpoint_path.exists());
    assert_eq!(fs::metadata(&archive).unwrap().ino(), inode);
    assert_eq!(fs::read(&archive).unwrap(), checkpoint_bytes);
    let discovery = archived_discovery(&authority.checkpoint_path, &target)
        .unwrap()
        .unwrap();
    assert!(
        discovery.adopted_terminals.is_empty(),
        "earlier adopted IDs must not be unioned into a later paused outcome"
    );
    assert_eq!(discovery.paused_terminals, record.paused_terminals);
    // A completed rename can be retried without altering its bytes.
    classified
        .archive_completed(
            record.successor.as_ref().unwrap(),
            &[],
            &[],
            &record.paused_terminals,
            &record.paused_chats,
        )
        .unwrap();
    assert_eq!(fs::read(&archive).unwrap(), checkpoint_bytes);
    // Discovery is deliberately metadata-only, even if archive storage is
    // unavailable. It conveys only a candidate for ordinary holder auth.
    let hidden = archive.with_extension("hidden");
    fs::rename(&archive, &hidden).unwrap();
    assert!(
        archived_discovery(&authority.checkpoint_path, &target)
            .unwrap()
            .is_some()
    );
    fs::rename(&hidden, &archive).unwrap();
    save(&authority.checkpoint_path, br#"{"phase":"Ready"}"#);
    assert!(
        archived_discovery(&authority.checkpoint_path, &target)
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(&archive).unwrap(), checkpoint_bytes);
}

struct SettledFiles {
    root: tempfile::TempDir,
    checkpoint: PathBuf,
    target: PathBuf,
    outcome: PathBuf,
    record: RecoveryRecord,
    checkpoint_bytes: Vec<u8>,
    outcome_bytes: Vec<u8>,
}

fn settled_files() -> SettledFiles {
    let root = crate::test_fixture::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let preparation = preparation(root.path());
    let checkpoint_bytes = checkpoint(&preparation, "Prepared");
    let authority = preparation.authority.clone();
    drop(preparation);
    let prefix = root.path().join("install");
    let bin = prefix.join("versions/fixture/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    for name in ["cagents", "cagents-ui", "cagentsd"] {
        fs::write(bin.join(name), format!("private {name}")).unwrap();
    }
    let target = fs::canonicalize(bin.join("cagentsd")).unwrap();
    let state = prefix.join(".update");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let request = b"original immutable request";
    save(&state.join(format!("{}.request", authority.job)), request);
    let mut successor = authority.source.clone();
    successor.host_incarnation = HostIncarnation::new("settled-successor");
    let record = RecoveryRecord {
        version: 2,
        origin: Origin::AcknowledgedPrepare,
        job: authority.job,
        request_identity: digest(
            &serde_json::to_vec(authority.reviewed.as_ref().unwrap()).unwrap(),
        ),
        job_request_hash: digest(request),
        reviewed: authority.reviewed,
        windows: authority.windows,
        source: authority.source,
        checkpoint_hash: digest(&checkpoint_bytes),
        census: authority.census,
        adopted_terminals: Vec::new(),
        adopted_chats: Vec::new(),
        adopted_chat_descriptors: Vec::new(),
        adopted_terminal_descriptors: Vec::new(),
        paused_terminals: vec![authority.expected[0].1],
        paused_chats: Vec::new(),
        settled: true,
        successor: Some(successor),
        target_hash: executable_hash(&target).unwrap(),
        target: Some(target.clone()),
        #[cfg(feature = "host")]
        authority: None,
    };
    let checkpoint = authority.checkpoint_path;
    save(&checkpoint, &checkpoint_bytes);
    let outcome = checkpoint.with_file_name(recovery_name(&record.checkpoint_hash));
    let outcome_bytes = serde_json::to_vec(&record).unwrap();
    save(&outcome, &outcome_bytes);
    SettledFiles {
        root,
        checkpoint,
        target,
        outcome,
        record,
        checkpoint_bytes,
        outcome_bytes,
    }
}

fn complete_files(files: &SettledFiles) {
    let recovered = pending_retirement(&files.checkpoint, &files.target)
        .unwrap()
        .unwrap();
    recovered
        .archive_completed(
            files.record.successor.as_ref().unwrap(),
            &[],
            &[],
            &files.record.paused_terminals,
            &[],
        )
        .unwrap();
}

#[test]
fn daemon_upgrade_retention_keeps_the_active_bundle_and_expires_an_older_one() {
    use std::time::{Duration, SystemTime};

    let old = settled_files();
    let active = settled_files();
    complete_files(&old);
    complete_files(&active);
    let current = archived_proof(&active.checkpoint, &active.target)
        .unwrap()
        .unwrap();
    let old_proof = archived_proof(&old.checkpoint, &old.target)
        .unwrap()
        .unwrap();
    let archive = archive_path(&active.checkpoint, &old.record.checkpoint_hash);
    let outcome = active
        .checkpoint
        .with_file_name(recovery_name(&old.record.checkpoint_hash));
    let cold = active.checkpoint.with_file_name(format!(
        "daemon-upgrade.cold-outcome.{}.json",
        old.record.checkpoint_hash
    ));
    save(&archive, &old.checkpoint_bytes);
    save(&outcome, &old.outcome_bytes);
    save(
        &cold,
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "witness": { "checkpoint_hash": old.record.checkpoint_hash },
        }))
        .unwrap(),
    );
    let mut retired: Retirement =
        serde_json::from_slice(&fs::read(old_proof.retirement_path).unwrap()).unwrap();
    retired.archive_path = archive.clone();
    let retired = serde_json::to_vec(&retired).unwrap();
    let retirement = retirement_path(&active.checkpoint, &digest(&retired));
    save(&retirement, &retired);

    let now = SystemTime::now();
    let serving = active.record.successor.as_ref().unwrap();
    assert_eq!(
        retention::collect_at(&active.checkpoint, serving, now).unwrap(),
        0
    );
    let expired = now + retention::GRACE + Duration::from_secs(1);
    assert_eq!(
        retention::collect_at(&active.checkpoint, serving, expired).unwrap(),
        4
    );
    for path in [archive, outcome, cold, retirement] {
        assert!(!path.exists(), "{} survived", path.display());
    }
    let remaining = archived_proof(&active.checkpoint, &active.target)
        .unwrap()
        .unwrap();
    assert_eq!(remaining.checkpoint_sha256, current.checkpoint_sha256);
    assert_eq!(
        retention::collect_at(&active.checkpoint, serving, expired).unwrap(),
        0
    );
}

#[test]
fn daemon_upgrade_retention_expires_only_unreferenced_captures() {
    use std::time::{Duration, SystemTime};

    let files = settled_files();
    let source = &files.record.source;
    let mut capture = CapturedCensus {
        producer: None,
        source: source.clone(),
        checkpoint_hash: None,
        census: Census::default(),
        windows: Vec::new(),
    };
    let proposed = capture_path(&files.checkpoint, source).unwrap();
    save(&proposed, &serde_json::to_vec(&capture).unwrap());
    let hash = &files.record.checkpoint_hash;
    let bound = bound_capture_path(&files.checkpoint, hash);
    capture.checkpoint_hash = Some(hash.clone());
    save(&bound, &serde_json::to_vec(&capture).unwrap());
    let expired = SystemTime::now() + retention::GRACE + Duration::from_secs(1);
    let serving = files.record.successor.as_ref().unwrap();
    assert_eq!(
        retention::collect_at(&files.checkpoint, serving, expired).unwrap(),
        0
    );

    // A deliberately interrupted archive remains the only copy of some panes.
    let interrupted = files.checkpoint.with_file_name(format!(
        "daemon-upgrade.interrupted-setup.{}.json",
        uuid::Uuid::new_v4()
    ));
    fs::rename(&files.checkpoint, &interrupted).unwrap();
    assert_eq!(
        retention::collect_at(&files.checkpoint, serving, expired).unwrap(),
        0
    );

    // A Ready publication continues to require the exact original capture.
    let ready = files
        .checkpoint
        .with_file_name("daemon-upgrade.ready-outcome.json");
    save(
        &ready,
        &serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "serving": serving,
            "target": files.target,
            "target_hash": files.record.target_hash,
            "partitions": [{
                "source": source,
                "checkpoint_hash": hash,
                "capture_hash": digest(&fs::read(&bound).unwrap()),
                "chats": [],
                "aliases": [],
                "terminals": [],
            }],
        }))
        .unwrap(),
    );
    fs::remove_file(interrupted).unwrap();
    assert_eq!(
        retention::collect_at(&files.checkpoint, serving, expired).unwrap(),
        0
    );
    fs::remove_file(ready).unwrap();
    assert_eq!(
        retention::collect_at(&files.checkpoint, source, expired).unwrap(),
        0
    );

    let unrelated = files.checkpoint.with_file_name("my-review.json");
    save(&unrelated, b"retain unknown data");
    let symlink = bound_capture_path(&files.checkpoint, &"a".repeat(64));
    std::os::unix::fs::symlink(&unrelated, &symlink).unwrap();
    assert_eq!(
        retention::collect_at(&files.checkpoint, serving, expired).unwrap(),
        0
    );
    assert!(
        files.outcome.exists(),
        "an outcome without an archive is still evidence"
    );
    fs::remove_file(files.outcome).unwrap();
    let quarantine = files
        .checkpoint
        .with_file_name(format!(".retirement-pointer.{}.json", uuid::Uuid::new_v4()));
    save(&quarantine, b"unrecognized retirement routing evidence");
    assert_eq!(
        retention::collect_at(&files.checkpoint, serving, expired).unwrap(),
        0
    );
    assert!(proposed.exists() && bound.exists() && quarantine.exists());
    fs::remove_file(quarantine).unwrap();
    assert_eq!(
        retention::collect_at(&files.checkpoint, serving, expired).unwrap(),
        2
    );
    assert!(!proposed.exists());
    assert!(!bound.exists());
    assert_eq!(fs::read(symlink).unwrap(), b"retain unknown data");
}

#[test]
fn retirement_guard_contention_is_bounded_and_leaves_no_waiter() {
    const ISOLATED: &str = "CAGENTS_TEST_RETIREMENT_CONTENTION_ISOLATED";
    if std::env::var_os(ISOLATED).is_none() {
        // Sibling libtests may fork and transiently inherit a locked OFD.
        // Test release and immediate reacquisition after a private exec.
        let selector = std::thread::current().name().unwrap().to_owned();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .env(ISOLATED, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    let files = settled_files();
    let RetirementAcquisition::Acquired(held) =
        RetirementGuard::acquire(&files.checkpoint).unwrap()
    else {
        panic!("private retirement fixture unexpectedly busy");
    };
    let checkpoint = files.checkpoint.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let contender = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let busy = matches!(
            RetirementGuard::acquire_for(&checkpoint, std::time::Duration::from_millis(40))
                .unwrap(),
            RetirementAcquisition::Busy
        );
        let _ = sender.send((busy, started.elapsed()));
    });
    let result = receiver.recv_timeout(std::time::Duration::from_secs(2));
    // No blocked flock or background waiter remains to steal custody
    // after the busy caller has returned. Release before asserting so a
    // regression to blocking flock can still be joined and cleaned up.
    drop(held);
    contender.join().unwrap();
    let (busy, elapsed) = result.expect("retirement contention exceeded its bounded deadline");
    assert!(busy);
    assert!(elapsed >= std::time::Duration::from_millis(40));
    assert!(matches!(
        RetirementGuard::acquire_for(&files.checkpoint, std::time::Duration::ZERO).unwrap(),
        RetirementAcquisition::Acquired(_)
    ));
    assert_eq!(fs::read(&files.checkpoint).unwrap(), files.checkpoint_bytes);
    assert!(!files.checkpoint.with_file_name(ACTIVE_RECOVERY).exists());
}

#[test]
fn failed_archive_claim_leaves_routing_intent_without_completion_proof() {
    let files = settled_files();
    let recovered = pending_retirement(&files.checkpoint, &files.target)
        .unwrap()
        .unwrap();
    let mut ready: serde_json::Value = serde_json::from_slice(&files.checkpoint_bytes).unwrap();
    ready["phase"] = "Ready".into();
    let ready = serde_json::to_vec(&ready).unwrap();
    let canonical = files.checkpoint.clone();
    let replacement = ready.clone();
    ARCHIVE_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |stage| {
            if stage == "before-claim" {
                let temporary = canonical.with_file_name("new-ready.json");
                save(&temporary, &replacement);
                fs::rename(temporary, &canonical).unwrap();
            }
        }));
    });
    let result = recovered.archive_completed(
        files.record.successor.as_ref().unwrap(),
        &[],
        &[],
        &files.record.paused_terminals,
        &[],
    );
    ARCHIVE_TEST_HOOK.with(|hook| hook.borrow_mut().take());
    assert!(result.unwrap_err().contains("safely restored"));
    assert!(files.checkpoint.with_file_name(ACTIVE_RECOVERY).exists());
    assert_eq!(fs::read(&files.checkpoint).unwrap(), ready);
    assert!(!archive_path(&files.checkpoint, &files.record.checkpoint_hash).exists());
    assert!(archived_proof(&files.checkpoint, &files.target).is_err());
    assert_eq!(fs::read(&files.outcome).unwrap(), files.outcome_bytes);
}

#[test]
fn planned_archive_retry_requires_material_and_then_completes_idempotently() {
    let files = settled_files();
    complete_files(&files);
    let proof = archived_proof(&files.checkpoint, &files.target)
        .unwrap()
        .unwrap();
    let pointer = fs::read(files.checkpoint.with_file_name(ACTIVE_RECOVERY)).unwrap();
    let retirement = fs::read(&proof.retirement_path).unwrap();
    // Reproduce a crash after routing intent was synced but before the
    // canonical checkpoint was moved. Intent can route recovery reads,
    // but must never count as an archive or completion.
    fs::rename(&proof.archive_path, &files.checkpoint).unwrap();
    assert!(archived_proof(&files.checkpoint, &files.target).is_err());
    assert!(
        pending_retirement(&files.checkpoint, &files.target)
            .unwrap()
            .is_some()
    );
    complete_files(&files);
    assert!(
        archived_proof(&files.checkpoint, &files.target)
            .unwrap()
            .is_some()
    );
    // Crash after the move but before the caller sees completion: retry
    // validates existing exact bytes without rewriting any generation.
    complete_files(&files);
    assert!(!files.checkpoint.exists());
    assert_eq!(
        fs::read(&proof.archive_path).unwrap(),
        files.checkpoint_bytes
    );
    assert_eq!(fs::read(&files.outcome).unwrap(), files.outcome_bytes);
    assert_eq!(fs::read(&proof.retirement_path).unwrap(), retirement);
    assert_eq!(
        fs::read(files.checkpoint.with_file_name(ACTIVE_RECOVERY)).unwrap(),
        pointer
    );
}

#[test]
fn retained_job_routing_binds_the_producers_exact_job_and_capture() {
    let files = settled_files();
    let mut pending = PendingRecovery::inspect_at(&files.checkpoint)
        .unwrap()
        .unwrap();
    assert!(pending.matches_retained_job("manual", b"request", b"census"));
    pending.original_census_complete = true;
    assert!(!pending.matches_retained_job("manual", b"request", b"census"));
    for producer in [
        CaptureProducer::Proposed {
            job: "job".into(),
            request_hash: digest(b"request"),
        },
        CaptureProducer::Acknowledged {
            job: "job".into(),
            request_hash: digest(b"request"),
        },
    ] {
        pending.producer = Some(producer);
        assert!(pending.matches_retained_job("job", b"request", b"census"));
        assert!(!pending.matches_retained_job("foreign", b"request", b"census"));
        assert!(!pending.matches_retained_job("job", b"changed", b"census"));
    }
    pending.producer = Some(CaptureProducer::ExplicitCensus {
        transaction: "job".into(),
        census_hash: digest(b"census"),
    });
    assert!(pending.matches_retained_job("job", b"request", b"census"));
    assert!(!pending.matches_retained_job("job", b"request", b"changed"));
    assert!(!pending.matches_retained_job("foreign", b"request", b"census"));
}

#[test]
fn historical_routing_ignores_other_target_and_source_before_strict_payloads() {
    let files = settled_files();
    complete_files(&files);
    let pointer = files.checkpoint.with_file_name(ACTIVE_RECOVERY);
    let bytes = read_checkpoint(&pointer).unwrap().unwrap();
    let active: ActiveRecovery = serde_json::from_slice(&bytes).unwrap();
    fs::remove_file(retirement_path(&files.checkpoint, &active.retirement_hash)).unwrap();
    let other = files.target.with_file_name("other-cagentsd");
    fs::write(&other, b"unrelated installed daemon").unwrap();
    assert!(
        archived_discovery(&files.checkpoint, &other)
            .unwrap()
            .is_none()
    );
    assert!(archived_proof(&files.checkpoint, &other).unwrap().is_none());
    let mut source = files.record.source.clone();
    source.host_incarnation = HostIncarnation::new("unrelated-source");
    assert!(
        pending_retirement_for_source(&files.checkpoint, &files.target, &source)
            .unwrap()
            .is_none()
    );
    assert!(
        pending_retirement_for_source(&files.checkpoint, &files.target, &files.record.source)
            .is_err()
    );
    assert!(archived_proof(&files.checkpoint, &files.target).is_err());
    assert!(
        archived_discovery(&files.checkpoint, &files.target)
            .unwrap()
            .is_none()
    );
    save(&pointer, b"unsupported, incomplete unrelated history");
    assert!(
        archived_discovery(&files.checkpoint, &files.target)
            .unwrap()
            .is_none()
    );
    assert!(
        pending_retirement_for_source(&files.checkpoint, &files.target, &source)
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(&files.outcome).unwrap(), files.outcome_bytes);
}

#[test]
fn canonical_unrelated_source_never_decodes_stale_retirement() {
    let files = settled_files();
    save(
        &files.checkpoint.with_file_name(ACTIVE_RECOVERY),
        b"old malformed pointer",
    );
    let mut other = files.record.source.clone();
    other.host_incarnation = HostIncarnation::new("new-source");
    assert!(
        pending_retirement_for_source(&files.checkpoint, &files.target, &other)
            .unwrap()
            .is_none()
    );
    assert!(
        classify_retained_prepared(&files.checkpoint, &files.target)
            .unwrap()
            .is_some()
    );
    fs::set_permissions(
        files.checkpoint.with_file_name(ACTIVE_RECOVERY),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(
        classify_retained_prepared(&files.checkpoint, &files.target)
            .unwrap()
            .is_some()
    );
    fs::remove_file(files.checkpoint.with_file_name(ACTIVE_RECOVERY)).unwrap();
    std::os::unix::fs::symlink(
        &files.outcome,
        files.checkpoint.with_file_name(ACTIVE_RECOVERY),
    )
    .unwrap();
    assert!(
        classify_retained_prepared(&files.checkpoint, &files.target)
            .unwrap()
            .is_some()
    );
    assert_eq!(fs::read(&files.checkpoint).unwrap(), files.checkpoint_bytes);
}

#[test]
fn archive_claim_validates_the_moved_inode_and_restores_changed_checkpoint() {
    let files = settled_files();
    let mut ready: serde_json::Value = serde_json::from_slice(&files.checkpoint_bytes).unwrap();
    ready["phase"] = "Ready".into();
    let ready = serde_json::to_vec(&ready).unwrap();
    let canonical = files.checkpoint.clone();
    let replacement = ready.clone();
    ARCHIVE_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |stage| {
            if stage == "before-claim" {
                let temp = canonical.with_file_name("new-ready.json");
                save(&temp, &replacement);
                fs::rename(temp, &canonical).unwrap();
            }
        }))
    });
    let archive = archive_path(&files.checkpoint, &files.record.checkpoint_hash);
    let error = archive_claimed_checkpoint(&files.checkpoint, &archive, &files.checkpoint_bytes)
        .unwrap_err();
    ARCHIVE_TEST_HOOK.with(|hook| hook.borrow_mut().take());
    assert!(error.contains("safely restored"), "{error}");
    assert_eq!(fs::read(&files.checkpoint).unwrap(), ready);
    assert!(!archive.exists());
    assert_eq!(fs::read(&files.outcome).unwrap(), files.outcome_bytes);
}

#[test]
fn archive_mismatch_retains_typed_custody_when_new_canonical_already_exists() {
    let files = settled_files();
    let mut ready: serde_json::Value = serde_json::from_slice(&files.checkpoint_bytes).unwrap();
    ready["phase"] = "Ready".into();
    let ready = serde_json::to_vec(&ready).unwrap();
    let mut newer = ready.clone();
    newer.push(b'\n');
    let canonical = files.checkpoint.clone();
    let moved = ready.clone();
    let published = newer.clone();
    ARCHIVE_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |stage| {
            let bytes = if stage == "before-claim" {
                &moved
            } else {
                &published
            };
            let temp = canonical.with_file_name("new-ready.json");
            save(&temp, bytes);
            fs::rename(temp, &canonical).unwrap();
        }))
    });
    let archive = archive_path(&files.checkpoint, &files.record.checkpoint_hash);
    let error = archive_claimed_checkpoint(&files.checkpoint, &archive, &files.checkpoint_bytes)
        .unwrap_err();
    ARCHIVE_TEST_HOOK.with(|hook| hook.borrow_mut().take());
    let custody = fs::read_dir(files.root.path())
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".checkpoint-retirement.")
        })
        .unwrap()
        .path();
    assert!(error.contains(custody.to_str().unwrap()), "{error}");
    assert_eq!(fs::read(&custody).unwrap(), ready);
    assert_eq!(fs::read(&files.checkpoint).unwrap(), newer);
    assert!(!archive.exists());
    drop(RetirementGuard::acquire(&files.checkpoint).unwrap());
    assert_eq!(
        fs::read(&custody).unwrap(),
        ready,
        "custody must never be swept as scratch metadata"
    );
    fs::remove_file(&files.checkpoint).unwrap();
    assert!(
        require_absent_checkpoint(&files.checkpoint)
            .unwrap_err()
            .contains(custody.to_str().unwrap())
    );
}

#[test]
fn concurrent_retirement_helpers_all_complete_idempotently() {
    let files = settled_files();
    let recovered = pending_retirement(&files.checkpoint, &files.target)
        .unwrap()
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let handles = (0..4)
        .map(|_| {
            let recovered = recovered.clone();
            let barrier = barrier.clone();
            let serving = files.record.successor.clone().unwrap();
            let paused = files.record.paused_terminals.clone();
            std::thread::spawn(move || {
                barrier.wait();
                recovered.archive_completed(&serving, &[], &[], &paused, &[])
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert!(!files.checkpoint.exists());
    let proof = archived_proof(&files.checkpoint, &files.target)
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read(proof.archive_path).unwrap(),
        files.checkpoint_bytes
    );
    assert_eq!(fs::read(files.outcome).unwrap(), files.outcome_bytes);
    assert_eq!(
        fs::read_dir(files.root.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("daemon-upgrade.retirement."))
            .count(),
        1
    );
}

#[test]
fn identical_existing_archive_is_a_successful_no_clobber_retry() {
    let files = settled_files();
    let archive = archive_path(&files.checkpoint, &files.record.checkpoint_hash);
    save(&archive, &files.checkpoint_bytes);
    complete_files(&files);
    assert!(!files.checkpoint.exists());
    assert_eq!(fs::read(&archive).unwrap(), files.checkpoint_bytes);
    assert_eq!(fs::read(&files.outcome).unwrap(), files.outcome_bytes);
}

#[test]
fn unknown_process_uid_or_uninspectable_same_user_retains_metadata() {
    let files = settled_files();
    let orphan = files.root.path().join(".recovery-Ab12Cd");
    save(&orphan, b"unproven orphan");
    let proc_root = files.root.path().join("proc-model");
    let process = proc_root.join("123456");
    fs::create_dir_all(&process).unwrap();
    save(&process.join("status"), b"Uid: unknown\n");
    sweep_archive_temporaries_at(files.root.path(), &proc_root);
    assert!(
        orphan.exists(),
        "unknown status must not fall back to proc-directory ownership"
    );
    let uid = crate::daemon::current_uid();
    save(
        &process.join("status"),
        format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n").as_bytes(),
    );
    save(&process.join("fd"), b"uninspectable descriptor directory");
    sweep_archive_temporaries_at(files.root.path(), &proc_root);
    assert!(
        orphan.exists(),
        "same-UID process with unavailable descriptors may still hold the file"
    );
    assert_eq!(
        status_uids(format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n").as_bytes()),
        Some([uid; 4])
    );
}

#[test]
fn verified_new_canonical_quarantines_unsafe_history_without_losing_evidence() {
    for symlink in [false, true] {
        let files = settled_files();
        let pointer = files.checkpoint.with_file_name(ACTIVE_RECOVERY);
        let old_target = files.root.path().join("old-unrelated-pointer");
        save(&old_target, b"unparseable unrelated historical metadata");
        if symlink {
            std::os::unix::fs::symlink(&old_target, &pointer).unwrap();
        } else {
            save(&pointer, b"unparseable unrelated historical metadata");
            fs::set_permissions(&pointer, fs::Permissions::from_mode(0o644)).unwrap();
        }
        complete_files(&files);
        let quarantine = fs::read_dir(files.root.path())
            .unwrap()
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".retirement-pointer.")
            })
            .unwrap()
            .path();
        if symlink {
            assert_eq!(fs::read_link(&quarantine).unwrap(), old_target);
        } else {
            assert_eq!(
                fs::read(&quarantine).unwrap(),
                b"unparseable unrelated historical metadata"
            );
        }
        assert_eq!(
            fs::read(&old_target).unwrap(),
            b"unparseable unrelated historical metadata"
        );
        let proof = archived_proof(&files.checkpoint, &files.target)
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(proof.archive_path).unwrap(),
            files.checkpoint_bytes
        );
        assert_eq!(fs::read(&files.outcome).unwrap(), files.outcome_bytes);
    }
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
)]
fn retirement_sweeps_only_closed_private_metadata_temporaries() {
    let files = settled_files();
    let active = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(files.root.path().join(".recovery-Ab12Cd"))
        .unwrap();
    let inspection = File::open(files.root.path().join(".recovery-Ab12Cd")).unwrap();
    assert!(!unopened_temporary(
        &inspection.metadata().unwrap(),
        inspection.as_raw_fd()
    ));
    drop(inspection);
    let proc_root = files.root.path().join("proc-model");
    let process = proc_root.join(std::process::id().to_string());
    fs::create_dir_all(process.join("fd")).unwrap();
    let uid = crate::daemon::current_uid();
    save(
        &process.join("status"),
        format!("Name:\ttest\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n").as_bytes(),
    );
    std::os::unix::fs::symlink(
        format!("/proc/self/fd/{}", active.as_raw_fd()),
        process.join("fd").join(active.as_raw_fd().to_string()),
    )
    .unwrap();
    let orphan = files.root.path().join(".retirement-Ef34Gh");
    save(&orphan, b"orphan metadata");
    let capture = files.root.path().join(".recovery-capture-private");
    save(&capture, b"active capture");
    let custody = files
        .root
        .path()
        .join(".checkpoint-retirement.private.json");
    save(&custody, b"sole checkpoint custody");
    let symlink = files.root.path().join(".recovery-Ij56Kl");
    std::os::unix::fs::symlink(&capture, &symlink).unwrap();
    let linked = files.root.path().join(".retirement-Mn78Op");
    fs::hard_link(&capture, &linked).unwrap();
    sweep_archive_temporaries_at(files.root.path(), &proc_root);
    assert!(!orphan.exists());
    assert!(files.root.path().join(".recovery-Ab12Cd").exists());
    assert_eq!(fs::read(&capture).unwrap(), b"active capture");
    assert_eq!(fs::read(&custody).unwrap(), b"sole checkpoint custody");
    assert!(
        fs::symlink_metadata(&symlink)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(linked.exists());
    drop(active);
    sweep_archive_temporaries_at(files.root.path(), &proc_root);
    assert!(!files.root.path().join(".recovery-Ab12Cd").exists());
}

#[test]
fn archived_discovery_ignores_a_fresh_install_without_recovery_storage() {
    let root = crate::test_fixture::tempdir().unwrap();
    assert!(
        archived_discovery(
            &root.path().join("not-created/daemon-upgrade.json"),
            Path::new("/no/daemon")
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn retained_checkpoint_without_prepared_phase_has_no_settled_classification() {
    let root = crate::test_fixture::tempdir().unwrap();
    let preparation = preparation(root.path());
    let path = &preparation.authority.checkpoint_path;
    assert!(
        classify_retained_prepared(path, Path::new("/no/daemon"))
            .unwrap()
            .is_none()
    );
    save(path, &checkpoint(&preparation, "Ready"));
    let before = fs::read(path).unwrap();
    assert!(
        classify_retained_prepared(path, Path::new("/no/daemon"))
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(path).unwrap(), before);
    // A legacy Ready checkpoint may use recipe DTOs outside this
    // classifier's current census contract. Ordinary restore owns it.
    save(
        path,
        br#"{"phase":"Ready","chats":[{"legacy_recipe":"opaque"}]}"#,
    );
    assert!(
        classify_retained_prepared(path, Path::new("/no/daemon"))
            .unwrap()
            .is_none()
    );
}

#[cfg(target_os = "linux")]
mod forwarding_tests {
    use super::*;
    use std::process::Stdio;

    const TEST: &str = "upgrade::recovery::tests::forwarding_tests::bash_cli_forwards_recovery_only_to_the_verified_daemon";
    const MODE: &str = "CAGENTS_TEST_RECOVERY_FORWARD_MODE";
    const ROOT: &str = "CAGENTS_TEST_RECOVERY_FORWARD_ROOT";

    #[test]
    fn bash_cli_forwards_recovery_only_to_the_verified_daemon() {
        if crate::retained_fixture::run_isolated() {
            return;
        }
        if let Some(mode) = std::env::var_os(MODE) {
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            crate::daemon::lease_handoff::initialize_inherited_lease().unwrap();
            assert!(has_inherited_receipt().unwrap());
            assert!(crate::daemon::lease_handoff::has_inherited_lease().unwrap());
            for fd in [LEGACY_FD, RECEIPT_FD, WORKER_FD, FENCE_FD, DAEMON_FD] {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert_ne!(flags, -1, "Bash lost recovery descriptor {fd}");
                assert_ne!(flags & libc::FD_CLOEXEC, 0);
            }

            // Model the store/schema maintenance launch: sanitize, seal,
            // then forward the ordinary reservation, never the receipt.
            let maintenance_marker = root.join("maintenance-executed");
            // Maintenance uses the very same daemon executable, so a
            // path/hash match alone must not turn the normal lease hook
            // into recovery forwarding.
            let mut maintenance = Command::new(root.join("bin/cagentsd"));
            maintenance
                .args([
                    "-c",
                    r#"
                    test "$CAGENTS_DAEMON_LEASE_FD" = 199 || exit 10
                    test -e /proc/self/fd/199 || exit 11
                    test -z "${CAGENTS_UPDATE_RECOVERY_FD+x}" || exit 12
                    for fd in 189 193 194 195 196; do
                        test ! -e "/proc/self/fd/$fd" || exit 13
                    done
                    printf maintenance > "$1"
                "#,
                    "maintenance-fixture",
                ])
                .arg(&maintenance_marker);
            crate::install_guard::sanitize_child_environment(&mut maintenance);
            unsafe { maintenance.pre_exec(crate::daemon::seal_inherited_descriptors) };
            crate::daemon::lease_handoff::inherit_daemon_lease(&mut maintenance).unwrap();
            assert!(maintenance.status().unwrap().success());
            assert_eq!(fs::read(&maintenance_marker).unwrap(), b"maintenance");

            let wrong_marker = root.join("wrong-target-executed");
            let mut wrong = Command::new(root.join("other/cagentsd"));
            wrong
                .args(["-c", "printf wrong > \"$1\"", "wrong-target-fixture"])
                .arg(&wrong_marker);
            crate::install_guard::sanitize_child_environment(&mut wrong);
            unsafe { wrong.pre_exec(crate::daemon::seal_inherited_descriptors) };
            let rejected = crate::daemon::lease_handoff::inherit_target_daemon_lease(&mut wrong)
                .and_then(|()| wrong.status());
            assert!(
                rejected.is_err(),
                "identical bytes and basename are not the selected path"
            );
            assert!(!wrong_marker.exists());

            let target_marker = root.join("target-executed");
            let mut target = Command::new(root.join("bin/cagentsd"));
            target
                .args([
                    "-c",
                    r#"
                    test "$CAGENTS_UPDATE_RECOVERY_FD" = 193 || exit 20
                    test "$CAGENTS_DAEMON_LEASE_FD" = 199 || exit 21
                    test -f /proc/self/fd/193 || exit 22
                    test /proc/self/fd/194 -ef "$1/worker.lock" || exit 23
                    test /proc/self/fd/195 -ef "$1/fence.lock" || exit 24
                    test /proc/self/fd/189 -ef "$1/legacy.lock" || exit 25
                    test /proc/self/fd/196 -ef "$2" || exit 26
                    test /proc/self/fd/199 -ef "$2" || exit 27
                    printf daemon > "$3"
                "#,
                    "target-daemon-fixture",
                ])
                .arg(&root)
                .arg(crate::daemon::RuntimePaths::discover().unwrap().lock)
                .arg(&target_marker);
            crate::install_guard::sanitize_child_environment(&mut target);
            unsafe { target.pre_exec(crate::daemon::seal_inherited_descriptors) };
            let result = crate::daemon::lease_handoff::inherit_target_daemon_lease(&mut target)
                .and_then(|()| target.status());
            if mode == "bad-hash" {
                assert!(
                    result.is_err(),
                    "changed verified bytes must fail before exec"
                );
                assert!(!target_marker.exists());
            } else {
                assert_eq!(mode, "valid");
                assert!(result.unwrap().success());
                assert_eq!(fs::read(&target_marker).unwrap(), b"daemon");
            }
            return;
        }

        let root = crate::test_fixture::tempdir().unwrap();
        for mode in ["valid", "bad-hash"] {
            let case = root.path().join(mode);
            fs::create_dir(&case).unwrap();
            fs::set_permissions(&case, fs::Permissions::from_mode(0o700)).unwrap();
            for child in ["bin", "other", "runtime"] {
                fs::create_dir(case.join(child)).unwrap();
                fs::set_permissions(case.join(child), fs::Permissions::from_mode(0o700)).unwrap();
            }
            // Small ELF fixtures expose inherited descriptors without ever
            // starting a provider or the installed daemon. The negative
            // target deliberately has identical bytes and basename.
            for executable in ["bin/cagentsd", "other/cagentsd"] {
                fs::copy("/bin/sh", case.join(executable)).unwrap();
                fs::set_permissions(case.join(executable), fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            let runtime = case.join("runtime");
            let daemon_directory = runtime.join(crate::instance::runtime_subdirectory());
            fs::create_dir(&daemon_directory).unwrap();
            fs::set_permissions(&daemon_directory, fs::Permissions::from_mode(0o700)).unwrap();
            let preparation = preparation(&case);
            save(
                &preparation.authority.checkpoint_path,
                &checkpoint(&preparation, "Prepared"),
            );
            let lease = handoff(&daemon_directory);
            let mut receipt = preparation
                .acknowledged()
                .unwrap()
                .receipt(&case.join("bin/cagentsd"), &lease)
                .unwrap()
                .unwrap();
            if mode == "bad-hash" {
                receipt.authority.target_hash = "0".repeat(64);
                receipt.receipt = sealed_receipt(&receipt.authority).unwrap();
            }
            let mut command = Command::new("/bin/bash");
            command
                .args([
                    "--noprofile",
                    "--norc",
                    "-p",
                    "-c",
                    "exec \"$@\"",
                    "receipt-bash-fixture",
                ])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env(MODE, mode)
                .env(ROOT, &case)
                .env("XDG_RUNTIME_DIR", &runtime)
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .stdin(Stdio::null());
            crate::install_guard::sanitize_child_environment(&mut command);
            unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
            // The updater explicitly grants the verified shell/selected CLI
            // its bundle. Later generic lease forwarding must not repeat it.
            lease.inherit(&mut command).unwrap();
            receipt.inherit(&mut command).unwrap();
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{mode}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        }
    }
}
