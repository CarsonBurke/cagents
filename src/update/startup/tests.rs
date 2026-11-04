use super::*;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

fn installation() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let bin = root.path().join("versions/1.2.3/bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ["cagents", "cagents-ui", "cagentsd"] {
        fs::write(bin.join(name), b"fixture").unwrap();
    }
    fs::write(
        root.path().join(".cagents-install-root"),
        b"cagents install root\n",
    )
    .unwrap();
    let state = root.path().join(".update");
    fs::DirBuilder::new().mode(0o700).create(&state).unwrap();
    (root, bin.join("cagents-ui"), state)
}
fn job(phase: UpdatePhase) -> UpdateJob {
    UpdateJob {
        id: uuid::Uuid::new_v4().to_string(),
        version: "1.2.4".into(),
        phase,
        message: "PRIVATE_ERROR_SENTINEL /private/path CONFIRMATION_SENTINEL".into(),
        log_path: "/private/LOG_SENTINEL".into(),
        confirmation: None,
    }
}
fn write(state: &Path, job: &UpdateJob) {
    super::super::write_job(state, job).unwrap();
}
fn projected(phase: UpdatePhase, helper: LockState, fence: LockState) -> Snapshot {
    Snapshot {
        installation_lock: fence,
        helper_lock: helper,
        job: Some(Job {
            id: "one".into(),
            version: Some("1.2.4".into()),
            phase,
            waiting: 0,
            blocked: 0,
            daemon_timeout: false,
        }),
    }
}

#[test]
fn real_worker_lock_and_unchanged_status_distinguish_active_from_orphan_without_repair() {
    let (_root, executable, state) = installation();
    let job = job(UpdatePhase::WaitingForIdle);
    write(&state, &job);
    let original = fs::read(state.join("status.json")).unwrap();
    let meta = fs::metadata(state.join("status.json")).unwrap();
    let lock = super::super::acquire_lock(&state).unwrap();
    let active = observe_installation(&executable, LockState::Held);
    assert_eq!(active.helper_lock, LockState::Held);
    assert_eq!(active.heading(), "Waiting for conversations to finish");
    assert!(!active.can_open());
    assert!(
        matches!(
            super::super::acquire_lock(&state),
            Err(super::super::UpdateError::Busy)
        ),
        "observation did not release the worker's lock"
    );
    drop(lock);
    let orphan = observe_installation(&executable, LockState::Free);
    assert_eq!(orphan.heading(), "Update completion is unconfirmed");
    assert!(orphan.can_open() && !orphan.finished());
    assert_eq!(fs::read(state.join("status.json")).unwrap(), original);
    assert_eq!(
        fs::metadata(state.join("status.json")).unwrap().ino(),
        meta.ino()
    );
    assert_eq!(
        fs::metadata(state.join("status.json")).unwrap().mode(),
        meta.mode()
    );
}

#[test]
fn same_leased_job_completion_opens_once_but_stale_complete_and_changed_jobs_never_do() {
    let mut follow = Follow::default();
    let active = projected(UpdatePhase::Installing, LockState::Held, LockState::Held);
    let complete = projected(UpdatePhase::Complete, LockState::Free, LockState::Free);
    assert!(!follow.observe(&complete)); // Historical Complete alone is not a transition.
    assert!(!follow.observe(&active));
    let held_complete = projected(UpdatePhase::Complete, LockState::Held, LockState::Held);
    assert!(!follow.observe(&held_complete));
    assert!(follow.observe(&complete));
    assert!(!follow.observe(&complete));
    let mut follow = Follow::default();
    assert!(!follow.observe(&active));
    let mut other = complete.clone();
    other.job.as_mut().unwrap().id = "other".into();
    assert!(!follow.observe(&other));
    assert!(!follow.observe(&complete));
    let mut follow = Follow::default();
    assert!(!follow.observe(&active));
    let mut replacement_active = active.clone();
    replacement_active.job.as_mut().unwrap().id = "replacement".into();
    let mut replacement_complete = complete.clone();
    replacement_complete.job.as_mut().unwrap().id = "replacement".into();
    assert!(!follow.observe(&replacement_active));
    assert!(!follow.observe(&replacement_complete));
    assert!(
        !follow.observe(&complete),
        "retained original Complete cannot undo the changed-operation fence"
    );
    assert!(!follow.observe(&active));
    assert!(!follow.observe(&complete));
    let mut fresh = Follow::default();
    assert!(!fresh.observe(&replacement_active));
    assert!(
        fresh.observe(&replacement_complete),
        "a separately opened waiting window may follow its own first leased operation"
    );
    for phase in [
        UpdatePhase::Failed,
        UpdatePhase::Cancelled,
        UpdatePhase::WaitingForIdle,
    ] {
        let mut follow = Follow::default();
        assert!(!follow.observe(&active));
        assert!(!follow.observe(&projected(phase, LockState::Free, LockState::Free)));
    }
}

#[test]
fn failed_or_terminal_status_does_not_override_real_held_or_unknown_ownership() {
    for phase in [
        UpdatePhase::Complete,
        UpdatePhase::Failed,
        UpdatePhase::Cancelled,
    ] {
        let locked = projected(phase.clone(), LockState::Free, LockState::Held);
        assert!(!locked.can_open() && !locked.finished());
        assert_eq!(locked.heading(), "Waiting for the update owner");
        let unknown = projected(phase, LockState::Unknown, LockState::Unknown);
        assert!(!unknown.can_open() && !unknown.finished());
    }
    let mut failed = projected(UpdatePhase::Failed, LockState::Free, LockState::Free);
    failed.job.as_mut().unwrap().daemon_timeout = true;
    assert!(failed.heading().contains("did not complete"));
    assert!(
        failed
            .explanation()
            .contains("daemon did not finish starting")
    );
}

#[test]
fn private_status_projection_never_exposes_capability_error_or_path_content() {
    let (_root, executable, state) = installation();
    for phase in [
        UpdatePhase::Installing,
        UpdatePhase::AwaitingConfirmation,
        UpdatePhase::Failed,
        UpdatePhase::Complete,
    ] {
        let mut job = job(phase);
        job.confirmation = Some(super::super::confirmation::Confirmation {
            nonce: "CONFIRMATION_NONCE_SENTINEL".into(),
            preview: super::super::impact::Preview {
                candidate: super::super::impact::Candidate {
                    version: "1.2.4".into(),
                    protocol: crate::terminal_ipc::PROTOCOL_VERSION,
                    digest: "PRIVATE_DIGEST_SENTINEL".into(),
                },
                replaces_daemon: true,
                daemon: Some(super::super::impact::DaemonImpact {
                    capability: crate::terminal_ipc::ShutdownCapability {
                        proof: crate::model::HostSessionProof {
                            host_session_id: crate::model::HostSessionId::new("private-host"),
                            boot_id: crate::model::BootId::new("private-boot"),
                            host_incarnation: crate::model::HostIncarnation::new("private-daemon"),
                        },
                        token: "REAL_CAPABILITY_FIELD_SENTINEL".into(),
                        inventory_generation: 1,
                        live_terminals: 0,
                        background_subagents: 0,
                        background_identity: String::new(),
                        expires_at_unix_ms: 1,
                    },
                    policy: super::super::impact::Policy::WhenIdle,
                    sessions: Vec::new(),
                    blockers: vec!["PRIVATE_BLOCKER_PATH /private/prompt".into()],
                }),
            },
            expires_at_unix_ms: u64::MAX,
        });
        job.version = "PRIVATE_INVALID_VERSION /path".into();
        write(&state, &job);
        let snapshot = observe_installation(&executable, LockState::Held);
        let visible = format!(
            "{} {} {} {:?}",
            snapshot.heading(),
            snapshot.explanation(),
            snapshot.technical_summary(),
            snapshot.target()
        );
        for sentinel in [
            "PRIVATE_ERROR_SENTINEL",
            "CONFIRMATION_SENTINEL",
            "LOG_SENTINEL",
            "PRIVATE_INVALID_VERSION",
            "CONFIRMATION_NONCE_SENTINEL",
            "REAL_CAPABILITY_FIELD_SENTINEL",
            "PRIVATE_DIGEST_SENTINEL",
            "PRIVATE_BLOCKER_PATH",
            "/private",
        ] {
            assert!(!visible.contains(sentinel));
        }
    }
}

#[test]
fn symlink_fifo_hardlink_oversize_and_unsafe_permissions_never_become_completion() {
    let (_root, executable, state) = installation();
    let p = state.join("status.json");
    let good = state.join("good.json");
    write(&state, &job(UpdatePhase::Complete));
    fs::rename(&p, &good).unwrap();
    std::os::unix::fs::symlink(&good, &p).unwrap();
    assert!(
        observe_installation(&executable, LockState::Free)
            .job
            .is_none()
    );
    fs::remove_file(&p).unwrap();
    fs::hard_link(&good, &p).unwrap();
    assert!(
        observe_installation(&executable, LockState::Free)
            .job
            .is_none()
    );
    fs::remove_file(&p).unwrap();
    let cpath = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    assert!(read_job(&p).is_err());
    fs::remove_file(&p).unwrap();
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&p)
        .unwrap();
    file.set_len(4 * 1024 * 1024 + 1).unwrap();
    assert!(read_job(&p).is_err());
    drop(file);
    fs::remove_file(&p).unwrap();
    fs::copy(&good, &p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_job(&p).is_err());
}

#[test]
fn awaiting_approval_and_idle_blockers_use_counts_without_session_names_or_capabilities() {
    let mut snapshot = projected(
        UpdatePhase::WaitingForIdle,
        LockState::Held,
        LockState::Held,
    );
    snapshot.job.as_mut().unwrap().waiting = 2;
    assert!(snapshot.explanation().contains("2 conversations"));
    snapshot.job.as_mut().unwrap().blocked = 1;
    assert!(
        snapshot
            .explanation()
            .contains("existing window needs attention")
    );
    snapshot.job.as_mut().unwrap().blocked = 0;
    snapshot.job.as_mut().unwrap().waiting = 0;
    snapshot.job.as_mut().unwrap().phase = UpdatePhase::AwaitingConfirmation;
    assert!(snapshot.explanation().contains("cannot approve"));
}
