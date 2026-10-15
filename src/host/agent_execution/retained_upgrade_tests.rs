// The existing owning_close_isolated parent retains its original Child/pidfd,
// bounds the subprocess, checks the exact named test and preserves the ORIGINAL
// private runtime on both outcomes. Whole-family settlement remains external.
struct UpgradeProviderPins(Vec<crate::daemon::owned_children::FixtureProcessPin>);
impl Drop for UpgradeProviderPins {
    fn drop(&mut self) {
        // Only original, private fixture process incarnations. The actual
        // runtime/holder remains responsible for wait/reap and cleanup proof.
        for pin in &self.0 {
            let _ = pin.signal(libc::SIGKILL);
        }
    }
}
struct PausedUpgradeHelper(Option<crate::daemon::owned_children::FixtureProcessPin>);
impl PausedUpgradeHelper {
    fn resume(&mut self) {
        if let Some(pin) = self.0.take() {
            pin.signal(libc::SIGCONT).unwrap();
        }
    }
}
impl Drop for PausedUpgradeHelper {
    fn drop(&mut self) {
        if let Some(pin) = &self.0 {
            let _ = pin.signal(libc::SIGCONT);
        }
    }
}

fn retained_upgrade_checkpoint_case(pause_claude: bool) {
    use crate::{
        daemon::{HostLease, RuntimePaths, holder_registry},
        upgrade::handoff::{Chat, Checkpoint, Phase, Store},
    };
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap();
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let marker = std::fs::OpenOptions::new()
            .write(true).create_new(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("combined-close-owner.json")).unwrap();
        serde_json::to_writer(marker, &serde_json::json!({
            "root":root.canonicalize().unwrap(),
            "uid":std::fs::metadata(root).unwrap().uid()
        })).unwrap();
    }
    let program = root.join("bin/codex");
    std::fs::write(&program, include_str!("../../../tests/fixtures/combined-close-codex.py")).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let paths = RuntimePaths::discover().unwrap();
    let old_proof = host.lock().unwrap().proof().clone();
    let lease = HostLease::acquire(&paths, &old_proof).unwrap();
    let notifications = crate::host::notifications::Broker::start(
        &host, crate::notifications::Application::Cagents).unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let mut pins = UpgradeProviderPins(Vec::new());
    let mut owners = Vec::new();
    let mut handles = Vec::new();
    let mut saved = Vec::new();
    for provider in [Provider::Claude, Provider::Codex] {
        request.idempotency_key = format!("retained-upgrade-{provider:?}");
        let Destination::New(spec) = &mut request.destination else { unreachable!() };
        spec.provider = provider;
        let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
        let run = settled(&host, accepted.run.id);
        assert_eq!(run.state, State::Succeeded, "{run:?}");
        let id = registry(&host).unwrap().lock().unwrap()
            .agent(accepted.agent.id).unwrap().session.unwrap().id();
        let (handle, mut source) = {
            let state = host.lock().unwrap();
            let record = &state.chats[&id];
            (record.handle.clone(), record.session.clone())
        };
        let owner = handle.native_holder_record().expect("actual execed native owner").clone();
        pins.0.push(crate::daemon::owned_children::FixtureProcessPin::capture(&owner.root).unwrap());
        pins.0.push(crate::daemon::owned_children::FixtureProcessPin::capture(&owner.holder).unwrap());
        source.launch.session_id = handle.confirmed_session_id();
        source.native_continuity = handle.native_continuity();
        assert!(source.native_continuity.retains_provider());
        assert_eq!(source.proof.as_ref(), Some(&old_proof));
        assert!(source.terminal.is_none(), "fixture has no synthetic paired console");
        if provider == Provider::Claude {
            assert!(handle.actor_identity().is_some(), "genuine retained Claude reducer");
            assert!(!crate::host::chat::owns_codex_listener(&source));
            let inspected = chat::runtime::Handle::inspect_remote(&owner, old_proof.clone()).unwrap();
            assert!(inspected.source == source || inspected.previous_sources.contains(&source));
        } else {
            assert!(handle.actor_identity().is_none(), "Codex is the Local native-holder facade");
            let (recovery, exited) = chat::native_holder::inspect(&owner, &old_proof).unwrap();
            assert!(!exited);
            assert!(recovery.source == source || recovery.previous_sources.contains(&source));
        }
        saved.push(Chat { launch:source.launch.clone(), source, previous_sources:Vec::new() });
        handles.push(handle);
        owners.push(owner);
    }
    let mut checkpoint = Checkpoint::new(old_proof.clone(), Vec::new(), Vec::new(), saved.clone());
    checkpoint.phase = Phase::Ready;
    checkpoint.validate().unwrap();
    let store = Store::discover().unwrap();
    store.save(&checkpoint).unwrap();
    for handle in &handles { handle.preserve_for_restart().unwrap(); }
    // Positive preservation acknowledgements and actual lease rotation; no
    // actor/Child/completed state is injected or relaunch recipe executed.
    drop(notifications);
    drop(host);
    drop(handles);
    drop(lease);
    let mut proof = old_proof.clone();
    proof.host_incarnation = HostIncarnation::new("retained-checkpoint-successor");
    let _lease = HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof.clone());
    state.install_model(Registry::at(&path).unwrap());
    let successor = Arc::new(crate::host::state::HostLock::new(state));
    let _notifications = crate::host::notifications::Broker::start(
        &successor, crate::notifications::Application::Cagents).unwrap();
    successor.lock().unwrap().notifications = Some(_notifications.handle());
    let mut paused = PausedUpgradeHelper(None);
    if pause_claude {
        let pin = crate::daemon::owned_children::FixtureProcessPin::capture(&owners[0].holder).unwrap();
        pin.signal(libc::SIGSTOP).unwrap();
        paused.0 = Some(pin);
    }
    let refused = crate::host::state::restore_upgrade(&successor).unwrap();
    assert!(store.load().unwrap().is_none(), "only the successfully processed checkpoint is consumed");
    if pause_claude {
        assert_eq!(refused, vec![saved[0].source.id]);
        let unresolved = store.unattached().unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].id, saved[0].source.id);
        assert!(!successor.lock().unwrap().chats.contains_key(&saved[0].source.id));
        assert!(owners[0].root.is_current() && owners[0].holder.is_current());
        assert!(successor.lock().unwrap().chats.contains_key(&saved[1].source.id),
            "isolated Claude refusal must not undo healthy Codex restoration");
        paused.resume();
        // A later EXPLICIT attach can retry the same genuine holder. The boot
        // loop neither silently retried it nor relaunched its provider recipe.
        successor.lock().unwrap().register_client(17);
        crate::host::chat::process(&successor, 17, ClientRequest::ChatAttach {
            session:saved[0].source.clone()
        }).unwrap();
        assert!(store.unattached().unwrap().is_empty());
    } else {
        assert!(refused.is_empty());
        assert!(store.unattached().unwrap().is_empty());
    }
    for (saved, owner) in saved.iter().zip(&owners) {
        let handle = successor.lock().unwrap().chats[&saved.source.id].handle.clone();
        assert_eq!(handle.native_holder_record(), Some(owner));
        assert!(owner.root.is_current() && owner.holder.is_current());
        assert_eq!(handle.confirmed_session_id(), saved.source.launch.session_id);
        assert_eq!(successor.lock().unwrap().chats[&saved.source.id].session.proof.as_ref(), Some(&proof));
    }
    assert_eq!(std::fs::read_to_string(root.join("spawns")).unwrap().lines().count(), 1);
    assert_eq!(std::fs::read_to_string(root.join("prompts")).unwrap().lines().count(), 1);
    assert_eq!(std::fs::read_to_string(root.join("codex-processes.jsonl")).unwrap().lines().count(), 1);
    let turns = std::fs::read_to_string(root.join("codex-rpc.jsonl")).unwrap().lines()
        .filter(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["method"] == "turn/start").count();
    assert_eq!(turns, 1, "restoration cannot replay the accepted prompt");
    for saved in &saved {
        let handle = successor.lock().unwrap().chats[&saved.source.id].handle.clone();
        handle.close_until(Instant::now() + Duration::from_secs(5)).unwrap();
        crate::host::chat::retained_actor::finish_close(&successor, &handle, &saved.source).unwrap();
        // This is exact-provider cleanup, not external whole-family proof.
        let socket = holder_registry::socket(&holder_registry::directory(holder_registry::Kind::Native).unwrap(), saved.source.id);
        assert!(!holder_registry::evidence(&socket, holder_registry::Kind::Native).unwrap()
            .is_some_and(|record| record.root.is_current() && record.holder.is_current()));
    }
}

#[test]
fn combined_close_retained_lifecycle_upgrade_restores_claude_and_codex_exactly_once() {
    if owning_close_isolated("combined_close_retained_lifecycle_upgrade_restores_claude_and_codex_exactly_once").is_some() { return; }
    retained_upgrade_checkpoint_case(false);
}
#[test]
fn combined_close_retained_lifecycle_upgrade_keeps_a_real_refusal_unattached_and_healthy_owner_live() {
    if owning_close_isolated("combined_close_retained_lifecycle_upgrade_keeps_a_real_refusal_unattached_and_healthy_owner_live").is_some() { return; }
    retained_upgrade_checkpoint_case(true);
}
