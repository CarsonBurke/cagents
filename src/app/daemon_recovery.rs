use std::{path::Path, time::Duration};

use crate::{
    control::ControlClient,
    model::{HostSessionProof, TerminalSession},
    terminal::ensure_daemon_ready,
    upgrade::{ensure_no_surviving_process, exact_conversation},
};

pub(super) struct RecoveredTerminal {
    pub(super) session: TerminalSession,
    pub(super) expected_host: Option<HostSessionProof>,
    pub(super) recovery_lease: Option<crate::daemon::RecoveryLease>,
}

/// Match the work represented by two terminal records within one proven
/// recovery lineage. An agent needs an exact conversation; a plain terminal
/// needs the same launch recipe. Conflicting direct and hinted agent IDs are
/// never evidence for attachment.
pub(super) fn same_recovery_content(saved: &TerminalSession, hosted: &TerminalSession) -> bool {
    let identity = |session: &TerminalSession| {
        let direct = exact_conversation(&session.launch);
        let resume = session
            .agent_resume_recipe()
            .as_ref()
            .and_then(exact_conversation);
        if direct.is_some() && resume.is_some() && direct != resume {
            return Err(());
        }
        Ok(direct.or(resume))
    };
    match (identity(saved), identity(hosted)) {
        (Ok(Some(saved)), Ok(Some(hosted))) => saved == hosted,
        (Ok(None), Ok(None)) => {
            saved.resume_hint.is_none()
                && hosted.resume_hint.is_none()
                && crate::agent::Provider::from_program(&saved.launch.program).is_none()
                && crate::agent::Provider::from_program(&hosted.launch.program).is_none()
                && saved.launch == hosted.launch
        }
        _ => false,
    }
}

/// Discovery is separate from attachment: an existing handle never changes its
/// proof or replays buffered input into the replacement terminal.
pub(super) fn reconcile(
    session: TerminalSession,
    explicit_retry: bool,
) -> Result<RecoveredTerminal, String> {
    if session.continuity.live_proof().is_none() {
        return Ok(RecoveredTerminal {
            session,
            expected_host: None,
            recovery_lease: None,
        });
    }
    let socket = ensure_daemon_ready().map_err(|error| error.to_string())?;
    let id = session.id;
    reconcile_at(session, explicit_retry, &socket, Path::new("/proc")).inspect_err(|error| {
        tracing::warn!(terminal = %id, explicit_retry, %error, "terminal recovery refused");
    })
}

fn reconcile_at(
    session: TerminalSession,
    explicit_retry: bool,
    socket: &Path,
    proc_root: &Path,
) -> Result<RecoveredTerminal, String> {
    let expected = session
        .continuity
        .live_proof()
        .ok_or("terminal has no live proof")?;
    if !expected.is_complete() {
        return Err("terminal ownership proof is incomplete; automatic recovery is unsafe".into());
    }
    // Hold one runtime-wide lease from fresh discovery through successful attachment.
    // A second card or UI must observe the first recovery's newly hosted conversation.
    let recovery_lease = Some(
        crate::daemon::RecoveryLease::acquire(socket, Duration::from_secs(5))
            .map_err(|error| format!("cannot serialize terminal recovery: {error}"))?,
    );
    let mut client = ControlClient::connect(socket).map_err(|error| error.to_string())?;
    let proof = client.proof().clone();
    let terminals = client.list().map_err(|error| error.to_string())?;
    if proof == expected {
        let terminal = terminals
            .iter()
            .find(|terminal| {
                terminal.session.id == session.id && terminal.session.launch == session.launch
            })
            .ok_or(
                "the original daemon is live but no matching terminal exists; relaunch is unsafe",
            )?;
        if terminal.session.continuity.can_resume_live(&expected) {
            return Ok(RecoveredTerminal {
                session: terminal.session.clone(),
                expected_host: Some(expected),
                recovery_lease,
            });
        }
        if !matches!(
            terminal.session.continuity,
            crate::model::TerminalContinuity::RelaunchAvailable { .. }
        ) {
            return Err(
                "the original terminal's exit is not established; preserving its identity".into(),
            );
        }
        if !explicit_retry {
            return Err(
                "the terminal exited while disconnected; use Retry to launch it again".into(),
            );
        }
    }
    if proof != expected
        && let Some(restored) = terminals.iter().find(|terminal| {
            terminal.upgrade_sources.iter().any(|source| {
                source.id == session.id
                    && source.launch == session.launch
                    && source.continuity.live_proof().as_ref() == Some(&expected)
            }) && terminal.session.continuity.can_resume_live(&proof)
        })
    {
        crate::daemon::replacement_owner(socket, &proof)
            .map_err(|error| format!("cannot establish upgraded daemon ownership: {error}"))?;
        // Only the replacement daemon's committed checkpoint can authorize
        // adopting a new process. Ordinary ID coincidence remains forbidden.
        return Ok(RecoveredTerminal {
            session: restored.session.clone(),
            expected_host: Some(proof),
            recovery_lease,
        });
    }
    if proof != expected
        && terminals
            .iter()
            .any(|terminal| terminal.session.id == session.id)
    {
        return Err("a different daemon owns this terminal ID; preserving that session instead of retargeting it".into());
    }
    // Recovery can span more than one daemon replacement. Upgrade receipts
    // describe only the latest handoff, while the stable recovery origin still
    // identifies the saved card's process lineage. Adopt a live descendant
    // only when its exact conversation or plain terminal recipe agrees too.
    let origin = session.recovery_origin.unwrap_or(session.id);
    let mut descendants = terminals.iter().filter(|terminal| {
        proof != expected
            && terminal.session.recovery_origin == Some(origin)
            && terminal.session.continuity.can_resume_live(&proof)
            && same_recovery_content(&session, &terminal.session)
    });
    if let Some(restored) = descendants.next() {
        if descendants.next().is_some() {
            return Err("multiple live terminals claim this saved conversation".into());
        }
        crate::daemon::replacement_owner(socket, &proof)
            .map_err(|error| format!("cannot establish upgraded daemon ownership: {error}"))?;
        return Ok(RecoveredTerminal {
            session: restored.session.clone(),
            expected_host: Some(proof),
            recovery_lease,
        });
    }
    if proof != expected
        && terminals.iter().any(|terminal| {
            terminal.session.recovery_origin == Some(origin)
                && terminal.session.continuity.can_resume_live(&proof)
        })
    {
        return Err("a live terminal shares this recovery lineage but its command or conversation differs; preserving it".into());
    }
    let owner = crate::daemon::replacement_owner(socket, &proof)
        .map_err(|error| format!("cannot establish replacement daemon ownership: {error}"))?;
    // Explicit replay changes only the recipe policy, never the ownership evidence.
    let plan = crate::upgrade::recovery_plan(&session);
    let planned_recipe = plan
        .as_ref()
        .map_or_else(|_| session.launch.clone(), |(recipe, _)| recipe.clone());
    // A lost Spawn reply is not evidence that Spawn failed. Use one stable
    // candidate per source/daemon and discover it before authorizing a replay.
    let mut replacement_id = recovery_terminal_id(session.id, &proof);
    while let Some(candidate) = terminals
        .iter()
        .find(|terminal| terminal.session.id == replacement_id)
    {
        if candidate.session.launch != planned_recipe {
            return Err(
                "the pending recovery identity belongs to a different command; preserving it"
                    .into(),
            );
        }
        if candidate.session.continuity.can_resume_live(&proof) {
            return Ok(RecoveredTerminal {
                session: candidate.session.clone(),
                expected_host: Some(proof),
                recovery_lease,
            });
        }
        if !explicit_retry
            || !matches!(
                candidate.session.continuity,
                crate::model::TerminalContinuity::RelaunchAvailable { .. }
            )
        {
            return Err("the recovered command already ran or its state is uncertain; use Retry only after checking its result".into());
        }
        replacement_id = recovery_terminal_id(replacement_id, &proof);
    }
    if session
        .agent_resume_recipe()
        .as_ref()
        .and_then(exact_conversation)
        .is_some_and(|identity| {
            terminals.iter().any(|terminal| {
                terminal.session.continuity.can_resume_live(&proof)
                    && terminal
                        .session
                        .agent_resume_recipe()
                        .as_ref()
                        .and_then(exact_conversation)
                        == Some(identity)
            })
        })
    {
        return Err("the saved agent conversation is already hosted by another terminal; preserving the live session".into());
    }
    ensure_no_surviving_process(&session, owner, proc_root)?;
    // Recheck the lease after process inspection. If discovery raced replacement,
    // do not authorize a launch against either incarnation.
    if crate::daemon::replacement_owner(socket, &proof)
        .map_err(|error| format!("daemon ownership changed during recovery: {error}"))?
        != owner
    {
        return Err("daemon ownership changed during recovery".into());
    }
    let (recipe, kind) = match plan {
        Ok((recipe, kind)) => (recipe, kind.label()),
        Err(_) if explicit_retry => (session.launch.clone(), "explicit replay"),
        Err(error) => return Err(error),
    };
    tracing::info!(
        terminal = %session.id,
        replacement = %replacement_id,
        program = %recipe.program,
        cwd = %recipe.working_directory.display(),
        recovery = kind,
        "relaunching a terminal whose daemon is gone"
    );
    let mut replacement = super::fresh_terminal_relaunch(&session);
    replacement.id = replacement_id;
    replacement.recovery_origin = Some(session.recovery_origin.unwrap_or(session.id));
    replacement.launch = recipe.clone();
    replacement.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
        recipe,
        last_exit: None,
    };
    Ok(RecoveredTerminal {
        session: replacement,
        expected_host: Some(proof),
        recovery_lease,
    })
}

fn recovery_terminal_id(
    source: crate::model::TerminalId,
    proof: &HostSessionProof,
) -> crate::model::TerminalId {
    let mut hash = blake3::Hasher::new_derive_key("cagents terminal recovery identity v1");
    hash.update(source.as_uuid().as_bytes());
    hash.update(&serde_json::to_vec(proof).expect("host proof contains only strings"));
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    crate::model::TerminalId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalId},
        terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, TerminalDescriptor, read_frame, write_frame,
        },
    };
    use std::{
        fs,
        fs::{File, OpenOptions},
        io,
        os::{
            fd::AsRawFd,
            unix::{fs::OpenOptionsExt, net::UnixListener},
        },
        thread,
    };

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(incarnation),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn session(path: &Path, proof: &HostSessionProof) -> TerminalSession {
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            id: TerminalId::new(),
            launch: LaunchRecipe::shell(path.to_owned()).unwrap(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        }
    }

    fn lease(directory: &Path, proof: &HostSessionProof) -> File {
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("cagentsd.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        serde_json::to_writer(
            &file,
            &serde_json::json!({
                "pid": std::process::id(), "proof": proof,
            }),
        )
        .unwrap();
        file
    }

    fn discovery(
        socket: &Path,
        proof: HostSessionProof,
        terminals: Vec<TerminalDescriptor>,
        probes: usize,
    ) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(
                matches!(connect.request, ClientRequest::Connect { expected_host: None, ref resume } if resume.is_empty())
            );
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(connect.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: Vec::new(),
                        },
                    },
                },
            )
            .unwrap();
            let list: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(list.proof, Some(proof.clone()));
            assert_eq!(list.request, ClientRequest::List);
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(list.request_id),
                    proof,
                    message: ServerMessage::Response {
                        response: ServerResponse::Listed { terminals },
                    },
                },
            )
            .unwrap();
            // Recovery is read-only: no Spawn, Attach, Shutdown, or Input reaches
            // this connection. A newly returned plan must use a fresh handle.
            assert_eq!(read_frame::<ClientEnvelope>(&mut stream).unwrap(), None);
            for _ in 0..probes {
                listener.accept().unwrap();
            }
        })
    }

    #[test]
    fn replaced_daemon_recovers_a_shell_with_fresh_identity_and_pinned_owner() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof.clone(), Vec::new(), 2);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_ne!(recovered.session.id, original.id);
        assert_eq!(recovered.session.recovery_origin, Some(original.id));
        assert_eq!(recovered.session.launch, original.launch);
        assert_eq!(recovered.expected_host, Some(replacement_proof));
        assert!(matches!(
            recovered.session.continuity,
            TerminalContinuity::RelaunchAvailable { .. }
        ));
        assert!(matches!(
            crate::daemon::RecoveryLease::acquire(&socket, Duration::ZERO),
            Err(crate::daemon::PlatformError::Io(error)) if error.kind() == io::ErrorKind::TimedOut
        ));
        drop(recovered);
        let _next_recovery =
            crate::daemon::RecoveryLease::acquire(&socket, Duration::ZERO).unwrap();
    }

    #[test]
    fn exact_live_terminal_keeps_identity_without_relaunching() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("live");
        let original = session(directory.path(), &owner);
        let mut descriptor =
            TerminalDescriptor::relaunchable(original.id, original.launch.clone(), None);
        descriptor.session = original.clone();
        let server = discovery(&socket, owner.clone(), vec![descriptor], 0);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, original);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn committed_upgrade_lineage_adopts_restored_process_without_replaying() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("upgraded");
        let original = session(directory.path(), &proof("old"));
        let _lease = lease(directory.path(), &owner);
        let mut restored = session(directory.path(), &owner);
        restored.id = original.id;
        restored.launch.working_directory = directory.path().join("current-directory");
        let mut descriptor =
            TerminalDescriptor::relaunchable(restored.id, restored.launch.clone(), None);
        descriptor.session = restored.clone();
        let mut intermediate = original.clone();
        intermediate.continuity = session(directory.path(), &proof("intermediate")).continuity;
        descriptor.upgrade_sources = vec![original.clone(), intermediate];
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, restored);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn saved_agent_adopts_a_live_descendant_after_multiple_handoffs() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("latest");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "omp".into();
        original.launch.arguments = vec![
            "--resume".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let mut descendant = session(directory.path(), &owner);
        descendant.recovery_origin = Some(original.id);
        descendant.launch = original.launch.clone();
        let mut descriptor =
            TerminalDescriptor::relaunchable(descendant.id, descendant.launch.clone(), None);
        descriptor.session = descendant.clone();
        // The latest upgrade receipt knows only its immediate predecessor.
        descriptor.upgrade_sources = vec![session(directory.path(), &proof("intermediate"))];
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, descendant);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn saved_shell_adopts_a_live_descendant_after_multiple_handoffs() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("latest");
        let original = session(directory.path(), &proof("old"));
        let mut descendant = session(directory.path(), &owner);
        descendant.recovery_origin = Some(original.id);
        let mut descriptor =
            TerminalDescriptor::relaunchable(descendant.id, descendant.launch.clone(), None);
        descriptor.session = descendant.clone();
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, descendant);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn generic_agent_and_ambiguous_shell_hint_do_not_identify_a_descendant() {
        let directory = tempfile::tempdir().unwrap();
        let mut saved = session(directory.path(), &proof("old"));
        let mut hosted = session(directory.path(), &proof("latest"));
        hosted.recovery_origin = Some(saved.id);
        saved.launch.program = "claude".into();
        hosted.launch = saved.launch.clone();
        assert!(!same_recovery_content(&saved, &hosted));

        saved.launch.program = "/bin/sh".into();
        hosted.launch = saved.launch.clone();
        saved.resume_hint = Some("unconfirmed agent identity".into());
        assert!(!same_recovery_content(&saved, &hosted));
    }

    #[test]
    fn recovery_origin_without_the_exact_conversation_cannot_retarget_an_agent() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("latest");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "omp".into();
        original.launch.arguments = vec![
            "--resume".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let mut descendant = session(directory.path(), &owner);
        descendant.recovery_origin = Some(original.id);
        descendant.launch.program = "omp".into();
        descendant.launch.arguments = vec![
            "--resume".into(),
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into(),
        ];
        let mut descriptor =
            TerminalDescriptor::relaunchable(descendant.id, descendant.launch.clone(), None);
        descriptor.session = descendant;
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner, vec![descriptor], 0);
        let result = reconcile_at(original, false, &socket, directory.path());
        server.join().unwrap();
        assert!(result.err().unwrap().contains("conversation differs"));
    }

    #[test]
    fn upgrade_id_without_matching_source_proof_cannot_retarget_card() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("upgraded");
        let original = session(directory.path(), &proof("old"));
        let mut restored = session(directory.path(), &owner);
        restored.id = original.id;
        let mut wrong_source = session(directory.path(), &proof("unrelated"));
        wrong_source.id = original.id;
        let mut descriptor =
            TerminalDescriptor::relaunchable(restored.id, restored.launch.clone(), None);
        descriptor.session = restored;
        descriptor.upgrade_sources = vec![wrong_source];
        let server = discovery(&socket, owner, vec![descriptor], 0);
        assert!(reconcile_at(original, false, &socket, directory.path()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn lost_spawn_reply_rediscovers_the_same_recovery_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("replacement");
        let original = session(directory.path(), &proof("old"));
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), Vec::new(), 2);
        let planned = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        let mut running = session(directory.path(), &owner);
        running.id = planned.session.id;
        drop(planned);
        fs::remove_file(&socket).unwrap();
        let mut descriptor =
            TerminalDescriptor::relaunchable(running.id, running.launch.clone(), None);
        descriptor.session = running.clone();
        let server = discovery(&socket, owner.clone(), vec![descriptor], 1);
        let recovered = reconcile_at(original, false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session, running);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn agent_without_exact_identity_reopens_its_resume_picker_not_its_shell() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("replacement");
        let mut original = session(directory.path(), &proof("old"));
        // The shell's argv names nothing; the observation saw Claude inside it
        // without establishing which conversation it held.
        let agent = LaunchRecipe {
            unset_environment: Default::default(),
            program: "claude".into(),
            arguments: vec![
                "--model".into(),
                "sonnet".into(),
                "--resume".into(),
                "12345678-1234-4234-8234-123456789abc".into(),
            ],
            working_directory: directory.path().to_owned(),
            environment: Default::default(),
        };
        original.resume_hint =
            Some(serde_json::json!({ "version": 2, "recipe": agent }).to_string());
        assert!(original.agent_resume_recipe().is_none());
        let _lease = lease(directory.path(), &owner);
        let server = discovery(&socket, owner.clone(), Vec::new(), 2);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session.launch.program, "claude");
        assert_eq!(
            recovered.session.launch.arguments,
            ["--model", "sonnet", "--resume"]
        );
        assert_eq!(recovered.session.resume_hint, None);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn authenticated_exit_allows_only_explicit_relaunch() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let owner = proof("same-owner");
        let original = session(directory.path(), &owner);
        let _lease = lease(directory.path(), &owner);
        let descriptor =
            TerminalDescriptor::relaunchable(original.id, original.launch.clone(), None);
        let server = discovery(&socket, owner.clone(), vec![descriptor.clone()], 0);
        assert!(reconcile_at(original.clone(), false, &socket, directory.path()).is_err());
        server.join().unwrap();
        fs::remove_file(&socket).unwrap();
        let server = discovery(&socket, owner.clone(), vec![descriptor], 2);
        let recovered = reconcile_at(original.clone(), true, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_ne!(recovered.session.id, original.id);
        assert_eq!(recovered.session.launch, original.launch);
        assert_eq!(recovered.expected_host, Some(owner));
    }

    #[test]
    fn replacement_cannot_adopt_a_matching_id_from_another_owner() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let mut descriptor =
            TerminalDescriptor::relaunchable(original.id, original.launch.clone(), None);
        descriptor.session = session(directory.path(), &proof("replacement"));
        descriptor.session.id = original.id;
        let server = discovery(&socket, proof("replacement"), vec![descriptor], 0);
        assert!(reconcile_at(original, false, &socket, directory.path()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn socket_response_without_exclusive_ownership_cannot_authorize_relaunch() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let original = session(directory.path(), &proof("old"));
        let server = discovery(&socket, proof("replacement"), Vec::new(), 0);
        assert!(reconcile_at(original, false, &socket, directory.path()).is_err());
        server.join().unwrap();
    }

    #[test]
    fn side_effecting_commands_require_explicit_retry() {
        let directory = tempfile::tempdir().unwrap();
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        assert!(crate::upgrade::recovery_plan(&original).is_err());
    }

    #[test]
    fn explicit_retry_replays_command_only_after_replacement_is_authenticated() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof.clone(), Vec::new(), 2);
        let recovered = reconcile_at(original.clone(), true, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(recovered.session.launch, original.launch);
        assert_ne!(recovered.session.id, original.id);
        assert_eq!(recovered.expected_host, Some(replacement_proof));
    }

    #[test]
    fn explicit_retry_does_not_override_missing_daemon_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        let server = discovery(&socket, proof("replacement"), Vec::new(), 0);
        let result = reconcile_at(original, true, &socket, directory.path());
        server.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .contains("cannot establish replacement daemon ownership")
        );
    }

    #[test]
    fn explicit_retry_does_not_override_uncertain_process_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let mut original = session(directory.path(), &proof("old"));
        original.launch.arguments = vec!["-c".into(), "touch important-file".into()];
        let replacement_proof = proof("replacement");
        let _lease = lease(directory.path(), &replacement_proof);
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"unknown process identity").unwrap();
        let server = discovery(&socket, replacement_proof, Vec::new(), 1);
        let result = reconcile_at(original, true, &socket, directory.path());
        server.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .contains("cannot inspect process ancestry")
        );
    }

    #[test]
    fn orphaned_terminal_process_blocks_recovery_without_being_killed() {
        use std::io::Read;
        use std::process::Stdio;

        let directory = tempfile::tempdir().unwrap();
        let original = session(directory.path(), &proof("old"));
        for marker in [
            None,
            Some(recovery_terminal_id(original.id, &proof("intermediate")).to_string()),
        ] {
            let mut command = std::process::Command::new("/bin/sh");
            command
                // Publish readiness from the executed child before scanning its
                // proc files. Keep it alive on an open input pipe: neither a
                // scheduling delay nor the suite's duration ends the fixture.
                .args(["-c", "printf ready; read -r release"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .current_dir(directory.path())
                .env("CAGENTS_RECOVERY_ORIGIN", original.id.to_string())
                .env("TERM_PROGRAM", "cagents");
            if let Some(marker) = marker {
                command.env("CAGENTS_TERMINAL_ID", marker);
            } else {
                command.env_remove("CAGENTS_TERMINAL_ID");
            }
            let mut child = command.spawn().unwrap();
            let mut ready = [0; 5];
            child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
            std::os::unix::fs::symlink(
                format!("/proc/{}", child.id()),
                directory.path().join(child.id().to_string()),
            )
            .unwrap();
            let result = ensure_no_surviving_process(&original, u32::MAX, directory.path());
            let alive = child.try_wait().unwrap().is_none();
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(result.is_err());
            assert!(alive, "discovery must never signal an existing process");
            fs::remove_file(directory.path().join(child.id().to_string())).unwrap();
        }
    }

    #[test]
    fn incomplete_process_evidence_does_not_authorize_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let pid_directory = directory.path().join("42");
        fs::create_dir(&pid_directory).unwrap();
        fs::write(pid_directory.join("stat"), b"unreadable process identity").unwrap();
        let launch = LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            id: crate::model::TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        assert!(ensure_no_surviving_process(&session, u32::MAX, directory.path()).is_err());
    }

    #[test]
    fn exact_terminal_marker_protects_a_survivor_after_directory_change() {
        let directory = tempfile::tempdir().unwrap();
        let launch = LaunchRecipe::shell(directory.path().join("old-directory")).unwrap();
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            id: crate::model::TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"42 (sleep) S 1 42 42").unwrap();
        fs::write(process.join("cmdline"), b"/bin/sleep\0").unwrap();
        fs::write(process.join("environ"), b"PATH=/bin\0").unwrap();
        std::os::unix::fs::symlink(directory.path(), process.join("cwd")).unwrap();
        assert!(ensure_no_surviving_process(&session, u32::MAX, directory.path()).is_ok());
        fs::write(
            process.join("environ"),
            format!("CAGENTS_TERMINAL_ID={}\0", session.id),
        )
        .unwrap();
        assert!(ensure_no_surviving_process(&session, u32::MAX, directory.path()).is_err());
    }

    #[test]
    fn distinct_cards_cannot_resume_the_same_hosted_conversation_with_different_flags() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "codex".into();
        original.launch.arguments = vec![
            "resume".into(),
            id.into(),
            "--model".into(),
            "old-model".into(),
        ];
        let mut hosted = session(directory.path(), &proof("replacement"));
        hosted.launch.program = "/usr/bin/codex".into();
        hosted.launch.arguments = vec![
            "resume".into(),
            id.to_uppercase(),
            "--model".into(),
            "new-model".into(),
        ];
        let mut descriptor =
            TerminalDescriptor::relaunchable(hosted.id, hosted.launch.clone(), None);
        descriptor.session = hosted;
        let _lease = lease(directory.path(), &proof("replacement"));
        let server = discovery(&socket, proof("replacement"), vec![descriptor], 1);
        let result = reconcile_at(original, true, &socket, directory.path());
        server.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .contains("conversation is already hosted")
        );
    }

    #[test]
    fn unrelated_uuid_shaped_flags_do_not_identify_a_hosted_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let other_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "codex".into();
        original.launch.arguments = vec!["resume".into(), id.into()];
        let replacement_proof = proof("replacement");
        let mut hosted = session(directory.path(), &replacement_proof);
        hosted.launch.program = "codex".into();
        hosted.launch.arguments = vec![
            "resume".into(),
            other_id.into(),
            "--model".into(),
            id.into(),
        ];
        let mut descriptor =
            TerminalDescriptor::relaunchable(hosted.id, hosted.launch.clone(), None);
        descriptor.session = hosted;
        let _lease = lease(directory.path(), &replacement_proof);
        let server = discovery(&socket, replacement_proof, vec![descriptor], 2);
        let recovered = reconcile_at(original.clone(), false, &socket, directory.path()).unwrap();
        server.join().unwrap();
        assert_eq!(
            recovered.session.agent_resume_recipe(),
            original.agent_resume_recipe()
        );
        assert_ne!(recovered.session.id, original.id);
    }

    #[test]
    fn exact_omp_session_filepath_protects_an_orphan_writer() {
        let directory = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let mut original = session(directory.path(), &proof("old"));
        original.launch.program = "omp".into();
        original.launch.arguments = vec!["--resume".into(), id.into()];
        let process = directory.path().join("42");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stat"), b"42 (omp) S 1").unwrap();
        fs::write(
            process.join("cmdline"),
            format!("omp\0--session\0/tmp/2026-09-09_{id}.jsonl\0"),
        )
        .unwrap();
        let result = ensure_no_surviving_process(&original, u32::MAX, directory.path());
        assert!(
            result
                .unwrap_err()
                .contains("agent session still has a live process (42)")
        );
    }
}
