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
    let context = super::super::protocol::ExecutionContext::capture(&cwd).unwrap();
    let pin = context.pinned_launch_directory().unwrap();
    let restored = super::super::protocol::ExecutionContext::restored(pin.0);
    assert!(restored.directory_descriptor().is_none());
    assert!(validate_goal_work(Some(&original), Some(&restored), Some(&pin), &cwd).is_ok());
    assert!(validate_goal_work(Some(&original), Some(&restored), None, &cwd).is_err());
    assert!(validate_goal_work(None, Some(&restored), Some(&pin), &cwd).is_err());
    // The trusted tuple survives a holder serialization, without recapturing
    // the same pathname's replacement after restart.
    let retained: crate::account::scope::LaunchAccountScope =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    std::fs::rename(&home, root.path().join("old-codex")).unwrap();
    std::fs::create_dir(&home).unwrap();
    assert!(validate_goal_work(Some(&retained), Some(&restored), Some(&pin), &cwd).is_err());
    assert_ne!(retained, scope(&home));
    std::fs::remove_dir(&home).unwrap();
    std::fs::rename(root.path().join("old-codex"), &home).unwrap();
    std::fs::rename(&cwd, root.path().join("old-work")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    assert!(validate_goal_work(Some(&retained), Some(&restored), Some(&pin), &cwd).is_err());
    assert_eq!(context.pinned_launch_directory().unwrap().0, pin.0);
    let replacement = super::super::protocol::ExecutionContext::capture(&cwd).unwrap();
    assert!(validate_goal_work(Some(&retained), Some(&replacement), Some(&pin), &cwd).is_err());
}

struct GoalAdapter;
impl Adapter for GoalAdapter {
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
    let handle = start(spec(&cwd), Some(scope(&home)), Box::new(GoalAdapter));
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
    }
    impl Adapter for SwapBeforeResume {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
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
        None,
        "Unsupported optional goals leave ordinary chat usable"
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
