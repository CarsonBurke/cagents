//! Goal-specific runtime authority and projection regressions. Synthetic local
//! actors only; these do not establish installed SDK or native UI behavior.
use super::*;
use crate::chat::goal::{Goal, Operation, ReadState, Status, Support};
use std::os::unix::fs::MetadataExt;

fn observation(status: Status, revision: u64) -> crate::chat::goal::Snapshot {
    crate::chat::goal::Snapshot {
        thread_id: "original-thread".into(),
        revision,
        support: Support::Supported,
        read: ReadState::Known(Some(Goal {
            thread_id: "original-thread".into(),
            objective: "Keep the original objective".into(),
            status,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: 1,
            updated_at: 1,
        })),
        pending: false,
        uncertain: false,
        last_result: None,
    }
}

fn identity(home: &std::path::Path) -> super::super::protocol::ServerIdentity {
    super::super::protocol::ServerIdentity {
        user_agent: "cagents synthetic goal actor".into(),
        canonical_home: home.to_owned(),
        reported_home: home.to_string_lossy().into_owned(),
    }
}

fn scope(home: &std::path::Path) -> crate::account::scope::LaunchAccountScope {
    let metadata = std::fs::metadata(home).unwrap();
    crate::account::scope::LaunchAccountScope {
        provider: crate::agent::Provider::Codex,
        canonical_home: home.to_owned(),
        device: metadata.dev(),
        inode: metadata.ino(),
        registered_id: None,
        purge_ids: Vec::new(),
    }
}

#[test]
fn goal_work_requires_original_home_and_descriptor_even_after_restoration() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let home = root.path().join("codex");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&home).unwrap();
    let original = scope(&home);
    let server = identity(&home);
    let context = super::super::protocol::ExecutionContext::capture(&cwd).unwrap();
    let pin = context.pinned_launch_directory().unwrap();
    let restored = super::super::protocol::ExecutionContext::restored(pin.0);
    assert!(restored.directory_descriptor().is_none());
    assert!(
        validate_goal_work(
            Some(&original),
            Some(&server),
            Some(&restored),
            Some(&pin),
            &cwd
        )
        .is_ok()
    );
    assert!(
        validate_goal_work(Some(&original), Some(&server), Some(&restored), None, &cwd).is_err()
    );
    assert!(validate_goal_work(None, Some(&server), Some(&restored), Some(&pin), &cwd).is_err());
    assert!(validate_goal_work(Some(&original), None, Some(&restored), Some(&pin), &cwd).is_err());
    // The trusted tuple survives a holder serialization, without recapturing
    // the same pathname's replacement after restart.
    let retained: crate::account::scope::LaunchAccountScope =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    std::fs::rename(&home, root.path().join("old-codex")).unwrap();
    std::fs::create_dir(&home).unwrap();
    assert!(
        validate_goal_work(
            Some(&retained),
            Some(&server),
            Some(&restored),
            Some(&pin),
            &cwd
        )
        .is_err()
    );
    assert_ne!(retained, scope(&home));
    std::fs::remove_dir(&home).unwrap();
    std::fs::rename(root.path().join("old-codex"), &home).unwrap();
    std::fs::rename(&cwd, root.path().join("old-work")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    assert!(
        validate_goal_work(
            Some(&retained),
            Some(&server),
            Some(&restored),
            Some(&pin),
            &cwd
        )
        .is_err()
    );
    assert_eq!(context.pinned_launch_directory().unwrap().0, pin.0);
    let replacement = super::super::protocol::ExecutionContext::capture(&cwd).unwrap();
    assert!(
        validate_goal_work(
            Some(&retained),
            Some(&server),
            Some(&replacement),
            Some(&pin),
            &cwd
        )
        .is_err()
    );
}

struct GoalAdapter {
    identity: super::super::protocol::ServerIdentity,
}
impl Adapter for GoalAdapter {
    fn server_identity(&self) -> Option<&super::super::protocol::ServerIdentity> {
        Some(&self.identity)
    }
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        let mut goal = observation(Status::Active, 1);
        match command {
            Command::Goal {
                operation: Operation::Pause,
                ..
            } => {
                let ReadState::Known(Some(current)) = &mut goal.read else {
                    unreachable!()
                };
                current.status = Status::Paused;
            }
            Command::Goal {
                operation: Operation::Clear,
                ..
            } => goal.read = ReadState::Known(None),
            Command::Goal {
                operation: Operation::Get | Operation::Resume | Operation::SetObjective(_),
                ..
            } => {}
            Command::Prompt { .. } => return Ok(Step::default()),
            _ => return Err("Unexpected synthetic goal command".into()),
        }
        Ok(Step {
            events: vec![Event::Goal(goal)],
            ..Step::default()
        })
    }
    fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
        Ok(Step::default())
    }
}

fn start(
    spec: ProcessSpec,
    scope: Option<crate::account::scope::LaunchAccountScope>,
    adapter: Box<dyn Adapter>,
) -> LocalHandle {
    let mut initial = Step {
        events: vec![Event::Ready(true)],
        ..Step::default()
    };
    let store = preflight(&spec, &mut initial).unwrap();
    LocalHandle::start_with_scope(
        spec,
        adapter,
        initial,
        store,
        Some("original-thread".into()),
        None,
        scope,
        |_| {},
    )
    .unwrap()
}
fn spec(cwd: &std::path::Path) -> ProcessSpec {
    ProcessSpec {
        execution: Some(Arc::new(
            super::super::protocol::ExecutionContext::capture(cwd).unwrap(),
        )),
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: cwd.into(),
        environment: BTreeMap::new(),
    }
}
fn command(operation: Operation) -> Command {
    Command::Goal {
        thread_id: "original-thread".into(),
        revision: 1,
        operation,
    }
}

#[test]
fn active_goal_blocks_idle_between_turns_but_live_stop_controls_survive_path_loss() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let home = root.path().join("codex");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&home).unwrap();
    let handle = start(
        spec(&cwd),
        Some(scope(&home)),
        Box::new(GoalAdapter {
            identity: identity(&home),
        }),
    );
    handle.command(command(Operation::Get)).unwrap();
    handle
        .command(command(Operation::SetObjective(
            "Keep the original objective".into(),
        )))
        .unwrap();
    handle.command(command(Operation::Resume)).unwrap();
    let snapshot = handle.snapshot(None).unwrap();
    assert!(!snapshot.busy && !snapshot.waiting);
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Busy
    );
    std::fs::rename(&cwd, root.path().join("old-work")).unwrap();
    std::fs::rename(&home, root.path().join("old-codex")).unwrap();
    for operation in [
        Operation::Resume,
        Operation::SetObjective("Another objective".into()),
    ] {
        assert!(handle.command(command(operation)).is_err());
        assert_eq!(
            handle.snapshot(None).unwrap().goal,
            Some(observation(Status::Active, 1))
        );
    }
    handle.command(command(Operation::Pause)).unwrap();
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Idle
    );
    handle.command(command(Operation::Get)).unwrap();
    handle.command(command(Operation::Clear)).unwrap();
    assert_eq!(
        handle.snapshot(None).unwrap().goal.unwrap().read,
        ReadState::Known(None)
    );
    handle.close().unwrap();
}

#[test]
fn resume_revalidates_original_scope_after_adapter_preparation_before_any_frame() {
    struct SwapBeforeResume {
        home: std::path::PathBuf,
        old: std::path::PathBuf,
        frame: serde_json::Value,
        identity: super::super::protocol::ServerIdentity,
        admission: Option<super::super::protocol::GoalResumeAdmission>,
    }
    impl Adapter for SwapBeforeResume {
        fn set_goal_resume_admission(
            &mut self,
            admission: super::super::protocol::GoalResumeAdmission,
        ) {
            self.admission = Some(admission);
        }
        fn server_identity(&self) -> Option<&super::super::protocol::ServerIdentity> {
            Some(&self.identity)
        }
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            assert_eq!(
                self.admission,
                Some(super::super::protocol::GoalResumeAdmission::OriginalScope)
            );
            std::fs::rename(&self.home, &self.old).map_err(|e| e.to_string())?;
            std::fs::create_dir(&self.home).map_err(|e| e.to_string())?;
            Ok(Step {
                writes: vec![
                    serde_json::json!({"method":"unrelated/first"}),
                    self.frame.clone(),
                ],
                events: vec![Event::Goal(observation(Status::Active, 2))],
                ..Step::default()
            })
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            panic!("No frame from a refused step may reach the provider")
        }
    }
    for frame in [
        serde_json::json!({"method":"thread/resume"}),
        serde_json::json!({"method":"thread/goal/set","params":{"threadId":"original-thread","objective":"Original work"}}),
        serde_json::json!({"method":"thread/goal/set","params":{"threadId":"original-thread","status":"active"}}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let handle = start(
            spec(root.path()),
            Some(scope(&home)),
            Box::new(SwapBeforeResume {
                home: home.clone(),
                old: root.path().join("old-codex"),
                frame,
                identity: identity(&home),
                admission: None,
            }),
        );
        let error = handle.command(Command::Compact).unwrap_err();
        assert!(error.contains("home") || error.contains("scope"), "{error}");
        assert!(
            handle.snapshot(None).unwrap().goal.is_none(),
            "The refused step did not publish a confirmed goal"
        );
        handle.close().unwrap();
    }
}

#[test]
fn goal_observations_fence_thread_reset_and_never_make_unknown_or_limited_work_idle() {
    let mut state = Snapshot {
        session_id: Some("original-thread".into()),
        ..Snapshot::default()
    };
    let mut indices = BTreeMap::new();
    let mut apply = |state: &mut Snapshot, event| apply_event(state, &mut indices, event);
    apply(&mut state, Event::Goal(observation(Status::Active, 1)));
    assert_eq!(
        goal_readiness(state.goal.as_ref()),
        Some(UpdateReadiness::Busy)
    );
    let mut foreign = observation(Status::Paused, 2);
    foreign.thread_id = "another-thread".into();
    apply(&mut state, Event::Goal(foreign));
    assert_eq!(state.goal, Some(observation(Status::Active, 1)));
    for status in [Status::Blocked, Status::UsageLimited, Status::BudgetLimited] {
        let goal = observation(status, 3);
        assert_eq!(goal_readiness(Some(&goal)), Some(UpdateReadiness::Busy));
    }
    let mut unknown = observation(Status::Paused, 4);
    unknown.read = ReadState::Unknown;
    assert_eq!(
        goal_readiness(Some(&unknown)),
        Some(UpdateReadiness::Unready)
    );
    unknown.support = Support::Unsupported;
    assert_eq!(
        goal_readiness(Some(&unknown)),
        Some(UpdateReadiness::Unready),
        "Unsupported is not proof that a goal-capable actor is inactive"
    );
    unknown.pending = true;
    assert_eq!(
        goal_readiness(Some(&unknown)),
        Some(UpdateReadiness::Unready)
    );
    unknown.pending = false;
    unknown.uncertain = true;
    assert_eq!(
        goal_readiness(Some(&unknown)),
        Some(UpdateReadiness::Unready)
    );
    apply(
        &mut state,
        Event::Session {
            id: "another-thread".into(),
            title: None,
        },
    );
    assert!(state.goal.is_none());
    state.session_id = Some("original-thread".into());
    apply(&mut state, Event::Goal(observation(Status::Paused, 5)));
    apply(
        &mut state,
        Event::Reset {
            reason: String::new(),
        },
    );
    assert!(state.goal.is_none());
}

#[test]
fn goal_work_binds_healthy_original_scope_to_the_initialized_server_across_alias_capture() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let home_a = root.path().join("home-a");
    let home_b = root.path().join("home-b");
    let alias = root.path().join("selected-home");
    std::fs::create_dir(&home_a).unwrap();
    std::fs::create_dir(&home_b).unwrap();
    symlink(&home_b, &alias).unwrap();
    // Protocol create captured B before runtime scope capture. The original
    // A tuple is physically valid, but is not the initialized server's home.
    let server = identity(&alias.canonicalize().unwrap());
    std::fs::remove_file(&alias).unwrap();
    symlink(&home_a, &alias).unwrap();
    let captured = scope(&alias.canonicalize().unwrap());
    std::fs::remove_file(&alias).unwrap();
    symlink(&home_b, &alias).unwrap();
    captured.revalidate().unwrap();
    assert_ne!(captured.canonical_home, server.canonical_home);
    let handle = start(
        spec(root.path()),
        Some(captured),
        Box::new(GoalAdapter { identity: server }),
    );
    handle.command(command(Operation::Get)).unwrap();
    for operation in [
        Operation::Resume,
        Operation::SetObjective("Unrelated home must not authorize".into()),
    ] {
        let error = handle.command(command(operation)).unwrap_err();
        assert!(
            error.contains("does not match this initialized server"),
            "{error}"
        );
        assert_eq!(
            handle.snapshot(None).unwrap().goal,
            Some(observation(Status::Active, 1))
        );
    }
    // Denying new work must not prevent stopping the actual original actor B.
    handle.command(command(Operation::Pause)).unwrap();
    handle.command(command(Operation::Clear)).unwrap();
    assert_eq!(
        handle.snapshot(None).unwrap().goal.unwrap().read,
        ReadState::Known(None)
    );
    handle.close().unwrap();
    // A genuinely matched B tuple is still allowed, with no fallback to A.
    let handle = start(
        spec(root.path()),
        Some(scope(&home_b)),
        Box::new(GoalAdapter {
            identity: identity(&home_b),
        }),
    );
    handle.command(command(Operation::Resume)).unwrap();
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Busy
    );
    handle.close().unwrap();
}

#[test]
fn legacy_inactive_admission_requires_original_directory_and_exact_retained_server() {
    use super::super::protocol::GoalResumeAdmission as Admission;
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let home = root.path().join("codex");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&home).unwrap();
    let original = scope(&home);
    let server = identity(&home);
    let context = super::super::protocol::ExecutionContext::capture(&cwd).unwrap();
    let pin = context.pinned_launch_directory().unwrap();
    let restored = super::super::protocol::ExecutionContext::restored(pin.0);
    let classify = |scope, current, legacy, directory| {
        goal_resume_admission(scope, current, legacy, Some(&restored), directory, &cwd)
    };
    assert_eq!(
        classify(Some(&original), Some(&server), None, Some(&pin)),
        Admission::OriginalScope
    );
    assert_eq!(
        classify(None, Some(&server), Some(&server), Some(&pin)),
        Admission::InactiveOnly
    );
    assert_eq!(
        classify(None, Some(&server), None, Some(&pin)),
        Admission::Unavailable,
        "A fresh missing tuple is never a legacy fallback"
    );
    assert_eq!(
        classify(None, None, Some(&server), Some(&pin)),
        Admission::Unavailable
    );
    assert_eq!(
        classify(None, Some(&server), Some(&server), None),
        Admission::Unavailable
    );
    let mut changed = server.clone();
    changed.reported_home = "/another-original-report".into();
    assert_eq!(
        classify(None, Some(&changed), Some(&server), Some(&pin)),
        Admission::Unavailable
    );
    std::fs::rename(&home, root.path().join("old-codex")).unwrap();
    std::fs::create_dir(&home).unwrap();
    assert_eq!(
        classify(Some(&original), Some(&server), Some(&server), Some(&pin)),
        Admission::Unavailable,
        "A failed retained tuple cannot downgrade to inactive-only"
    );
    std::fs::rename(&cwd, root.path().join("old-work")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    assert_eq!(
        classify(None, Some(&server), Some(&server), Some(&pin)),
        Admission::Unavailable
    );
}

#[test]
fn legacy_resume_requires_latest_exact_positive_inactive_observation_not_unknown_or_old_active() {
    use super::super::protocol::GoalResumeAdmission as Admission;
    let mut state = Snapshot {
        session_id: Some("original-thread".into()),
        goal: Some(observation(Status::Active, 1)),
        ..Snapshot::default()
    };
    let resume =
        serde_json::json!({"method":"thread/resume","params":{"threadId":"original-thread"}});
    let mut step = Step {
        writes: vec![resume],
        events: vec![Event::Goal(observation(Status::Paused, 2))],
        ..Step::default()
    };
    assert!(
        validate_goal_resume_step(Admission::InactiveOnly, &state, &step).is_ok(),
        "The latest paused probe must override old Active"
    );
    for status in [Status::Paused, Status::Complete] {
        step.events = vec![Event::Goal(observation(status, 3))];
        assert!(validate_goal_resume_step(Admission::InactiveOnly, &state, &step).is_ok());
    }
    let mut absent = observation(Status::Paused, 4);
    absent.read = ReadState::Known(None);
    step.events = vec![Event::Goal(absent.clone())];
    assert!(validate_goal_resume_step(Admission::InactiveOnly, &state, &step).is_ok());
    for status in [
        Status::Active,
        Status::Blocked,
        Status::UsageLimited,
        Status::BudgetLimited,
    ] {
        step.events = vec![Event::Goal(observation(status, 5))];
        assert!(validate_goal_resume_step(Admission::InactiveOnly, &state, &step).is_err());
    }
    state.goal = Some(observation(Status::Paused, 6));
    let mut unknown = observation(Status::Paused, 7);
    unknown.read = ReadState::Unknown;
    for support in [Support::Unknown, Support::Unsupported, Support::Supported] {
        unknown.support = support;
        step.events = vec![Event::Goal(unknown.clone())];
        assert!(
            validate_goal_resume_step(Admission::InactiveOnly, &state, &step).is_err(),
            "New unknown/unsupported cannot fall back to cached paused"
        );
        assert_eq!(
            goal_readiness(Some(&unknown)),
            Some(UpdateReadiness::Unready)
        );
        assert!(
            validate_goal_resume_step(Admission::OriginalScope, &state, &step).is_ok(),
            "The original physical proof authorizes restore independently of optional Goal API support"
        );
    }
    for (pending, uncertain) in [(true, false), (false, true)] {
        let mut unanswered = absent.clone();
        unanswered.pending = pending;
        unanswered.uncertain = uncertain;
        step.events = vec![Event::Goal(unanswered)];
        assert!(validate_goal_resume_step(Admission::InactiveOnly, &state, &step).is_err());
    }
    step.events = vec![Event::Goal(absent)];
    assert!(validate_goal_resume_step(Admission::Unavailable, &state, &step).is_err());
    assert!(validate_goal_resume_step(Admission::OriginalScope, &state, &step).is_ok());
    step.writes[0]["params"]["threadId"] = serde_json::json!("another-thread");
    assert!(validate_goal_resume_step(Admission::InactiveOnly, &state, &step).is_err());
    assert!(
        goal_readiness(None).is_none(),
        "Ordinary no-Goal snapshots keep existing policy"
    );
}

#[test]
fn goal_resume_advisory_refreshes_on_real_actor_command_and_receive() {
    use super::super::protocol::GoalResumeAdmission as Admission;
    struct Advisory {
        identity: super::super::protocol::ServerIdentity,
        admission: Option<Admission>,
        received: crossbeam_channel::Sender<Admission>,
    }
    impl Adapter for Advisory {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn server_identity(&self) -> Option<&super::super::protocol::ServerIdentity> {
            Some(&self.identity)
        }
        fn set_goal_resume_admission(&mut self, admission: Admission) {
            self.admission = Some(admission);
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            assert!(self.admission.is_some(), "Refresh must precede the command");
            Ok(Step {
                writes: vec![serde_json::json!({"synthetic":"echo"})],
                ..Step::default()
            })
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            self.received
                .send(self.admission.expect("Refresh before receive"))
                .unwrap();
            Ok(Step::default())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("codex");
    std::fs::create_dir(&home).unwrap();
    let (send, receive) = crossbeam_channel::bounded(2);
    let handle = start(
        spec(root.path()),
        Some(scope(&home)),
        Box::new(Advisory {
            identity: identity(&home),
            admission: None,
            received: send,
        }),
    );
    handle.command(Command::Compact).unwrap();
    assert_eq!(
        receive.recv_timeout(Duration::from_secs(5)).unwrap(),
        Admission::OriginalScope
    );
    std::fs::rename(&home, root.path().join("old-home")).unwrap();
    std::fs::create_dir(&home).unwrap();
    handle.command(Command::Interrupt).unwrap();
    assert_eq!(
        receive.recv_timeout(Duration::from_secs(5)).unwrap(),
        Admission::Unavailable,
        "The initialized actor must refresh authority after a physical home replacement"
    );
    handle.close().unwrap();
}

#[test]
fn admitted_fresh_goal_requires_the_exact_spawn_fd_and_preserves_unknown_refusal() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    let home = root.path().join("codex");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&home).unwrap();
    let source = crate::chat::Session {
        id: crate::model::TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: crate::model::LaunchRecipe {
                program: "codex".into(),
                arguments: Vec::new(),
                working_directory: cwd.clone(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let context = crate::chat::protocol::ExecutionContext::new(None);
    let original = scope(&home);
    let server = identity(&home);
    assert!(
        validate_goal_work(Some(&original), Some(&server), Some(&context), None, &cwd).is_err()
    );
    // Synthetic boundary control: runtime production obtains this FD from the
    // authenticated holder getter, never by reopening the current pathname.
    let directory = Arc::new(std::fs::File::open(&cwd).unwrap());
    context.admit_fresh_native_directory(&source).unwrap();
    assert!(context.claim_fresh_native_spawn(&source).unwrap());
    context
        .retain_fresh_spawn_directory(&source, directory.clone())
        .unwrap();
    let pin = context.pinned_launch_directory().unwrap();
    assert!(Arc::ptr_eq(&pin.1, &directory));
    assert!(
        validate_goal_work(
            Some(&original),
            Some(&server),
            Some(&context),
            Some(&pin),
            &cwd
        )
        .is_ok()
    );
    let unknown = crate::chat::protocol::ExecutionContext::new(None);
    assert!(
        validate_goal_work(
            Some(&original),
            Some(&server),
            Some(&unknown),
            Some(&pin),
            &cwd
        )
        .is_err()
    );
    std::fs::rename(&cwd, root.path().join("moved-work")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    assert_eq!(context.pinned_launch_directory().unwrap().0, pin.0);
    assert!(
        validate_goal_work(
            Some(&original),
            Some(&server),
            Some(&context),
            Some(&pin),
            &cwd
        )
        .is_err()
    );
    assert!(context.claim_fresh_native_spawn(&source).is_err());
}
