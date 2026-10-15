// Real retained Claude + real holder-backed /bin/cat console. These are host
// contracts; compositor/native UI and installed-provider acceptance are separate.
struct TerminalPairCloseFixture {
    native: RetainedCloseRaceFixture,
    descriptor: crate::terminal_ipc::TerminalDescriptor,
    worker: crate::terminal::TerminalHandle,
}
impl TerminalPairCloseFixture {
    fn start() -> Self {
        let native = RetainedCloseRaceFixture::start();
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(), arguments: vec![],
            working_directory: native.path.parent().unwrap().join("project"),
            environment: Default::default(), unset_environment: Default::default(),
        };
        create_console_from_recipe(&native.host, &registry(&native.host).unwrap(),
            native.agent.id, &native.source, &native.handle, None, recipe)
            .unwrap_or_else(|error| panic!("real console publication: {}", match error {
                ConsoleError::Known(error) | ConsoleError::Uncertain(error) => error,
            }));
        let (descriptor, worker) = {
            let state = native.host.lock().unwrap();
            let descriptor = state.descriptor(EXECUTION_CLIENT, native.source.id).unwrap();
            let worker = state.terminals[&native.source.id].handle.clone();
            assert!(state.chats[&native.source.id].paired_terminal.as_ref().unwrap().same_worker(&worker));
            (descriptor, worker)
        };
        assert!(worker.holder_identity().is_some(), "must use a real private holder");
        Self { native, descriptor, worker }
    }
    fn request(&self, route: u8) -> ClientRequest {
        match route {
            0 => ClientRequest::CloseTerminalSourceChecked {
                source: self.descriptor.session.clone(), execution_instance: self.descriptor.execution_instance,
                allow_active: true,
            },
            1 => ClientRequest::CloseTerminalChecked {terminal_id:self.native.source.id, allow_active:true},
            2 => {
                self.worker.input(vec![4]).unwrap(); // actual canonical EOF, not injected exit metadata
                let deadline = Instant::now() + Duration::from_secs(3);
                let session = loop {
                    let descriptor = self.native.host.lock().unwrap().descriptor(EXECUTION_CLIENT, self.native.source.id).unwrap();
                    if matches!(descriptor.session.continuity, crate::model::TerminalContinuity::RelaunchAvailable {last_exit:Some(_), ..} | crate::model::TerminalContinuity::Exited {..}) {
                        break descriptor.session;
                    }
                    assert!(Instant::now() < deadline, "actual worker exit callback must publish its ended descriptor");
                    std::thread::sleep(Duration::from_millis(5));
                };
                ClientRequest::ReleaseCompletedTerminal {session, allow_active:true}
            },
            _ => unreachable!(),
        }
    }
}
impl Drop for TerminalPairCloseFixture {
    fn drop(&mut self) { let _ = self.worker.shutdown(); }
}
struct TerminalPairCut(Option<std::sync::mpsc::Sender<()>>);
impl Drop for TerminalPairCut {
    fn drop(&mut self) { if let Some(release) = self.0.take() { let _ = release.send(()); } }
}
fn hold_terminal_pair_cut(id: TerminalId) -> (TerminalPairCut, std::sync::mpsc::Receiver<()>) {
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    crate::host::client::before_terminal_pair_cleanup(id, move || {
        let _ = entered.send(());
        let _ = released.recv_timeout(Duration::from_secs(10));
    });
    (TerminalPairCut(Some(release)), entering)
}

fn terminal_route_excludes_restart(name: &str, route: u8) {
    if owning_close_isolated(name).is_some() { return; }
        let fixture = TerminalPairCloseFixture::start();
        let request = fixture.request(route);
        let (cut, entering) = hold_terminal_pair_cut(fixture.native.source.id);
        let (completed, completion) = std::sync::mpsc::channel();
        let host = fixture.native.host.clone();
        std::thread::spawn(move || { let _ = completed.send(crate::host::client::process_request(&host, EXECUTION_CLIENT, request)); });
        entering.recv_timeout(Duration::from_secs(5)).expect("actual route must reach the pre-native cleanup cut");
        assert!(fixture.worker.is_completed(), "route {route} must retain the completed original W1");
        let current = fixture.native.host.lock().unwrap().chats[&fixture.native.source.id].session.clone();
        let denied = crate::host::client::process_request(&fixture.native.host, EXECUTION_CLIENT,
            ClientRequest::ChatAttach {session:current}).unwrap_err();
        assert!(denied.message.contains("lifecycle transition"), "route {route}: {denied:?}");
        assert_eq!(fixture.native.spawns(), 1, "no backend B may start at the old race window");
        assert!(fixture.native.identity.record.holder.is_current());
        assert_eq!(fixture.native.handle.actor_identity().unwrap(), fixture.native.identity);
        drop(cut);
        let (response, _, _) = completion.recv_timeout(Duration::from_secs(15)).expect("bounded paired cleanup").unwrap();
        assert!(matches!(response, ServerResponse::TerminalCloseChecked {outcome:crate::terminal_ipc::TerminalCloseResult::Closed, ..}));
        assert!(!fixture.native.identity.record.holder.is_current(), "exact original helper must retire");
        assert!(fixture.worker.is_completed());
        assert_eq!(fixture.native.spawns(), 1);
        assert_eq!(registry(&fixture.native.host).unwrap().lock().unwrap().agent_run(fixture.native.run.id).unwrap(), fixture.native.run);
}

#[test]
fn combined_close_retained_lifecycle_source_close_excludes_restart_after_worker_close() {
    terminal_route_excludes_restart("combined_close_retained_lifecycle_source_close_excludes_restart_after_worker_close", 0);
}
#[test]
fn combined_close_retained_lifecycle_legacy_close_excludes_restart_after_worker_close() {
    terminal_route_excludes_restart("combined_close_retained_lifecycle_legacy_close_excludes_restart_after_worker_close", 1);
}
#[test]
fn combined_close_retained_lifecycle_completed_release_excludes_restart_after_worker_close() {
    terminal_route_excludes_restart("combined_close_retained_lifecycle_completed_release_excludes_restart_after_worker_close", 2);
}

#[test]
fn combined_close_retained_lifecycle_initial_console_publication_defers_to_terminal_close() {
    if owning_close_isolated("combined_close_retained_lifecycle_initial_console_publication_defers_to_terminal_close").is_some() { return; }
    let fixture = TerminalPairCloseFixture::start();
    let request = fixture.request(0);
    let (cut, entering) = hold_terminal_pair_cut(fixture.native.source.id);
    let (completed, completion) = std::sync::mpsc::channel();
    let host = fixture.native.host.clone();
    std::thread::spawn(move || {let _ = completed.send(crate::host::client::process_request(&host, EXECUTION_CLIENT, request));});
    entering.recv_timeout(Duration::from_secs(5)).unwrap();
    let (source, recipe) = {
        let state = fixture.native.host.lock().unwrap();
        let record = &state.chats[&fixture.native.source.id];
        (record.session.clone(), record.console_recipe.clone())
    };
    let mut replacement = recipe.clone().unwrap();
    replacement.environment.insert("MUST_NOT_PUBLISH".into(), "replacement".into());
    let result = create_console_from_recipe(&fixture.native.host, &registry(&fixture.native.host).unwrap(),
        fixture.native.agent.id, &source, &fixture.native.handle, None, replacement);
    let error = match result {Err(ConsoleError::Known(error) | ConsoleError::Uncertain(error)) => error, Ok(_) => panic!("console publication bypassed close")};
    assert!(error.contains("lifecycle transition"), "{error}");
    {
        let state = fixture.native.host.lock().unwrap();
        let record = &state.chats[&source.id];
        assert_eq!(record.console_recipe, recipe, "refusal must precede the first recipe mutation");
        assert!(record.paired_terminal.as_ref().unwrap().same_worker(&fixture.worker));
        assert!(state.terminals[&source.id].handle.same_worker(&fixture.worker));
    }
    assert_eq!(fixture.native.spawns(), 1);
    drop(cut);
    completion.recv_timeout(Duration::from_secs(15)).unwrap().unwrap();
    assert!(!fixture.native.identity.record.holder.is_current());
}

fn degraded_registry_refuses_terminal_route(name: &str, route: u8) {
    if owning_close_isolated(name).is_some() { return; }
        let fixture = TerminalPairCloseFixture::start();
        let request = fixture.request(route);
        let completed_before = fixture.worker.is_completed();
        fixture.native.host.lock().unwrap().model_open_failed();
        let error = crate::host::client::process_request(&fixture.native.host, EXECUTION_CLIENT, request).unwrap_err();
        assert!(error.message.contains("registry") || error.message.contains("Registry"), "{error:?}");
        assert_eq!(fixture.worker.is_completed(), completed_before, "Registry refusal must precede worker teardown");
        assert!(fixture.native.identity.record.holder.is_current());
        assert_eq!(fixture.native.spawns(), 1);
        let state = fixture.native.host.lock().unwrap();
        assert!(state.terminals[&fixture.native.source.id].handle.same_worker(&fixture.worker));
        assert!(state.chats[&fixture.native.source.id].handle.same_actor(&fixture.native.handle));
}

#[test]
fn combined_close_retained_lifecycle_degraded_registry_source_close_has_no_effects() {
    degraded_registry_refuses_terminal_route("combined_close_retained_lifecycle_degraded_registry_source_close_has_no_effects", 0);
}
#[test]
fn combined_close_retained_lifecycle_degraded_registry_legacy_close_has_no_effects() {
    degraded_registry_refuses_terminal_route("combined_close_retained_lifecycle_degraded_registry_legacy_close_has_no_effects", 1);
}
#[test]
fn combined_close_retained_lifecycle_degraded_registry_completed_release_has_no_effects() {
    degraded_registry_refuses_terminal_route("combined_close_retained_lifecycle_degraded_registry_completed_release_has_no_effects", 2);
}

#[test]
fn combined_close_retained_lifecycle_degraded_registry_keeps_standalone_terminal_close_available() {
    if owning_close_isolated("combined_close_retained_lifecycle_degraded_registry_keeps_standalone_terminal_close_available").is_some() { return; }
    let (host, _, path) = fixture();
    let id = TerminalId::new();
    let recipe = crate::model::LaunchRecipe {
        program:"/bin/cat".into(), arguments:vec![], working_directory:path.parent().unwrap().join("project"),
        environment:Default::default(), unset_environment:Default::default(),
    };
    let (descriptor, worker) = super::super::state::spawn_terminal_view_owned(&host, EXECUTION_CLIENT,
        crate::terminal_ipc::TerminalDescriptor::relaunchable(id, recipe, None).session,
        Default::default(), None, true).unwrap();
    struct Cleanup(crate::terminal::TerminalHandle);
    impl Drop for Cleanup {fn drop(&mut self) {let _ = self.0.shutdown();}}
    let _cleanup = Cleanup(worker.clone());
    host.lock().unwrap().model_open_failed();
    let (response, _, _) = crate::host::client::process_request(&host, EXECUTION_CLIENT,
        ClientRequest::CloseTerminalSourceChecked {source:descriptor.session,
            execution_instance:descriptor.execution_instance, allow_active:true}).unwrap();
    assert!(matches!(response, ServerResponse::TerminalCloseChecked {outcome:crate::terminal_ipc::TerminalCloseResult::Closed, ..}));
    assert!(worker.is_completed());
    assert!(!host.lock().unwrap().terminals.contains_key(&id));
}
