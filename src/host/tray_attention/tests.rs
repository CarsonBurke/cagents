//! Real HostState/source identity and navigation queue tests, without a PTY,
//! provider, desktop socket or child launch. Activity-table inputs exercise the
//! production sampled reducer; they do not establish provider observation.
use super::*;
use crate::{
    agent::Activity,
    model::{
        BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalContinuity,
    },
    notifications::{Presence, Target},
    terminal::RenderStreamFixture,
};
use std::{cell::Cell, path::PathBuf};

fn empty_host() -> SharedHost {
    Arc::new(super::super::state::HostLock::new(HostState::new(
        HostSessionProof {
            host_session_id: HostSessionId::new("tray-source-test"),
            boot_id: BootId::new("tray-test-boot"),
            host_incarnation: HostIncarnation::new("tray-test-incarnation"),
        },
    )))
}

fn recipe() -> LaunchRecipe {
    LaunchRecipe {
        program: "/bin/true".into(),
        arguments: Vec::new(),
        working_directory: PathBuf::from("/private/parent/tray-project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    }
}

fn terminal(id: TerminalId) -> TerminalSession {
    let launch = recipe();
    TerminalSession {
        id,
        launch: launch.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
    }
}

fn install_chat(state: &mut HostState, id: TerminalId, handle: Handle) {
    let identity = handle
        .actor_identity()
        .expect("existing retained fixture identity");
    let session = crate::chat::Session {
        id,
        launch: crate::chat::Launch {
            provider: Provider::Claude,
            recipe: recipe(),
            session_id: None,
        },
        proof: Some(state.proof().clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
    };
    state.chats.insert(
        id,
        super::super::chat::Record {
            execution_instance: identity.instance,
            console_recipe: None,
            paired_terminal: None,
            session,
            handle,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
}

fn destination(window: &str) -> Destination {
    Destination {
        application: Application::Cagents,
        window: window.into(),
    }
}

fn presence(destination: &Destination, session: Option<TerminalId>, focused: bool) -> Presence {
    Presence {
        window: destination.window.clone(),
        application: destination.application,
        focused_target: None,
        sessions: session.into_iter().collect(),
        focused: session.filter(|_| focused),
    }
}

fn drain(host: &SharedHost, destination: &Destination) -> Option<Target> {
    let mut state = host.lock().unwrap();
    assert_eq!(
        state.watch_notifications(100, presence(destination, None, false)),
        None,
        "a generic watch cannot strip provenance from private tray intent"
    );
    let intent = state.pending_tray_session(destination)?.clone();
    state.acknowledge_tray_session(destination, &intent);
    Some(Target::Session(intent.session))
}

fn request(session: TerminalId, window: &str) -> crate::instance::TrayActivation {
    crate::instance::TrayActivation {
        request: uuid::Uuid::new_v4(),
        registry: uuid::Uuid::new_v4(),
        host: empty_host().lock().unwrap().proof().clone(),
        window: window.into(),
        session,
        sources: vec![[1; 32]],
    }
}

#[test]
fn captured_native_source_rejects_actor_attachment_and_descriptor_replacement() {
    let host = empty_host();
    let (original, attachment) = Handle::fixture_remote_attachment_pair();
    assert!(original.same_actor(&attachment));
    assert!(!original.same_attachment(&attachment));
    let id = original.actor_identity().unwrap().record.terminal;
    let mut state = host.lock().unwrap();
    install_chat(&mut state, id, original.clone());
    let captured = Source::capture(&state, id).unwrap();
    assert!(captured.matches(&state, id));
    drop(state);
    assert!(
        !captured.waiting(),
        "the actual fixture is resident ready Idle"
    );
    let mut state = host.lock().unwrap();
    state.chats.get_mut(&id).unwrap().handle = attachment;
    assert!(
        !captured.matches(&state, id),
        "same actor is not the same proxy attachment"
    );
    state.chats.get_mut(&id).unwrap().handle = original.clone();
    assert!(captured.matches(&state, id));
    let (replacement, _) = Handle::fixture_remote_attachment_pair();
    assert!(!original.same_actor(&replacement));
    state.chats.get_mut(&id).unwrap().handle = replacement;
    assert!(!captured.matches(&state, id));
    state.chats.get_mut(&id).unwrap().handle = original;
    state
        .chats
        .get_mut(&id)
        .unwrap()
        .session
        .launch
        .recipe
        .working_directory = "/changed".into();
    assert!(!captured.matches(&state, id));
    state.chats.remove(&id);
    assert!(!captured.matches(&state, id));
}

#[test]
fn actual_worker_pairing_is_required_and_replacement_retires_captured_source() {
    let host = empty_host();
    let first = RenderStreamFixture::new(|_| {});
    let next = RenderStreamFixture::new(|_| {});
    let (native, _) = Handle::fixture_remote_attachment_pair();
    let id = native.actor_identity().unwrap().record.terminal;
    let mut state = host.lock().unwrap();
    state.register_client(1);
    state
        .finish_spawn(1, terminal(id), first.handle.clone(), false)
        .unwrap();
    let terminal_only = Source::capture(&state, id).unwrap();
    assert!(terminal_only.matches(&state, id));
    state
        .finish_spawn(1, terminal(id), next.handle.clone(), false)
        .unwrap();
    assert!(
        !terminal_only.matches(&state, id),
        "same ID and descriptor do not replace a worker"
    );
    state
        .finish_spawn(1, terminal(id), first.handle.clone(), false)
        .unwrap();
    install_chat(&mut state, id, native);
    assert!(
        Source::capture(&state, id).is_err(),
        "raw duplicate ID must not guess a pair"
    );
    let captured = capture(&state);
    assert!(captured[&id].ambiguous);
    let source_terminal = state.terminals[&id].session.clone();
    let chat = state.chats.get_mut(&id).unwrap();
    chat.session.terminal = Some(source_terminal);
    chat.paired_terminal = Some(first.handle.clone());
    assert!(paired(&state, id));
    let source = Source::capture(&state, id).unwrap();
    assert!(source.matches(&state, id));
    state
        .finish_spawn(1, terminal(id), next.handle.clone(), false)
        .unwrap();
    assert!(!paired(&state, id));
    assert!(!source.matches(&state, id));
    state
        .finish_spawn(1, terminal(id), first.handle.clone(), false)
        .unwrap();
    assert!(paired(&state, id));
    state
        .chats
        .get_mut(&id)
        .unwrap()
        .session
        .terminal
        .as_mut()
        .unwrap()
        .launch
        .arguments
        .push("changed".into());
    assert!(
        !paired(&state, id),
        "matching worker cannot hide descriptor replacement"
    );
    assert!(!source.matches(&state, id));
}

#[test]
fn raw_relaunch_terminal_fingerprints_issued_live_projection_without_losing_worker_custody() {
    let host = empty_host();
    let first = RenderStreamFixture::new(|_| {});
    let replacement = RenderStreamFixture::new(|_| {});
    let id = TerminalId::new();
    let original = terminal(id);
    let mut state = host.lock().unwrap();
    state.register_client(1);
    state
        .finish_spawn(1, original.clone(), first.handle.clone(), false)
        .unwrap();
    let source = Source::capture(&state, id).unwrap();
    assert!(
        source
            .terminal
            .as_ref()
            .unwrap()
            .0
            .continuity
            .live_proof()
            .is_none()
    );
    let presentation = source.presentation_sources(&state, id).unwrap();
    assert_eq!(presentation.len(), 1);
    assert_eq!(
        presentation[0],
        crate::agent_execution::HostedSession::Terminal(
            state.native_terminal_projection(id).unwrap()
        )
    );
    assert!(presentation[0].has_complete_proof());
    assert!(crate::instance::tray_source_fingerprint(&presentation[0]).is_ok());
    assert!(source.matches(&state, id));
    assert!(
        state.terminals[&id]
            .session
            .continuity
            .live_proof()
            .is_none(),
        "projection cannot rewrite raw source custody"
    );
    state
        .finish_spawn(1, original.clone(), replacement.handle.clone(), false)
        .unwrap();
    assert!(
        source.presentation_sources(&state, id).is_err(),
        "same raw record cannot substitute another worker"
    );
    state
        .finish_spawn(1, original, first.handle.clone(), false)
        .unwrap();
    state
        .terminals
        .get_mut(&id)
        .unwrap()
        .session
        .launch
        .arguments
        .push("different-process".into());
    assert!(
        source.presentation_sources(&state, id).is_err(),
        "changed raw process cannot borrow an old projection"
    );
    drop(state);
    let mut incomplete = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new(""),
        boot_id: BootId::new(""),
        host_incarnation: HostIncarnation::new(""),
    });
    incomplete.register_client(1);
    incomplete
        .finish_spawn(1, terminal(id), first.handle.clone(), false)
        .unwrap();
    let source = Source::capture(&incomplete, id).unwrap();
    assert!(
        source.presentation_sources(&incomplete, id).is_err(),
        "existing authenticated projection refuses an incomplete host proof"
    );
}

#[test]
fn raw_terminal_focus_preparation_queues_actual_issued_fingerprint_and_refuses_replacement() {
    for paired_native in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let registry =
            super::super::model::Registry::at(&directory.path().join("tray-focus.sqlite3"))
                .unwrap();
        let identity = registry.identity().unwrap();
        let host = empty_host();
        let first = RenderStreamFixture::new(|_| {});
        let replacement = RenderStreamFixture::new(|_| {});
        let (native, _) = Handle::fixture_remote_attachment_pair();
        let id = native.actor_identity().unwrap().record.terminal;
        let raw = terminal(id);
        {
            let mut state = host.lock().unwrap();
            state.install_model(registry);
            state.register_client(1);
            state
                .finish_spawn(1, raw.clone(), first.handle.clone(), false)
                .unwrap();
            if paired_native {
                install_chat(&mut state, id, native);
                let issued_pair_source = state.terminals[&id].session.clone();
                let chat = state.chats.get_mut(&id).unwrap();
                chat.session.terminal = Some(issued_pair_source);
                chat.paired_terminal = Some(first.handle.clone());
                assert!(paired(&state, id));
            }
        }
        let delivered = std::cell::RefCell::new(None);
        // The fixture supplies only Waiting observation and suppresses delivery;
        // actual focus production preparation owns projection/hash/payload/queue.
        focus_with(
            &host,
            id,
            |_| true,
            |host, destination, intent, source, _| {
                let state = host.lock().unwrap();
                assert!(source.matches(&state, id));
                assert_eq!(state.pending_tray_session(destination), Some(intent));
                assert_eq!(intent.registry, identity);
                assert_eq!(intent.sources.len(), if paired_native { 2 } else { 1 });
                if paired_native {
                    assert!(
                        intent.source_matches(&crate::agent_execution::HostedSession::Chat(
                            state.chats[&id].session.clone()
                        ))
                    );
                }
                assert_eq!(intent.host, *state.proof());
                assert!(
                    intent.source_matches(&crate::agent_execution::HostedSession::Terminal(
                        state.native_terminal_projection(id).unwrap()
                    ))
                );
                assert!(
                    state.terminals[&id]
                        .session
                        .continuity
                        .live_proof()
                        .is_none()
                );
                *delivered.borrow_mut() = Some((destination.clone(), intent.clone()));
                Ok("prepared exact intent".into())
            },
        )
        .unwrap();
        let (destination, old) = delivered.into_inner().unwrap();
        host.lock()
            .unwrap()
            .acknowledge_tray_session(&destination, &old);
        let outcome = focus_with(
            &host,
            id,
            |_| {
                host.lock()
                    .unwrap()
                    .finish_spawn(1, raw, replacement.handle.clone(), false)
                    .unwrap();
                true
            },
            |_, _, _, _, _| panic!("changed worker must not queue or dispatch"),
        );
        assert!(outcome.unwrap_err().contains("changed"));
        assert!(
            host.lock()
                .unwrap()
                .pending_tray_session(&destination)
                .is_none()
        );
    }
}

#[test]
fn sampled_policy_preserves_pair_fallback_counts_all_waiting_and_omits_ambiguous_rows() {
    let host = empty_host();
    let stream = RenderStreamFixture::new(|_| {});
    let (native, _) = Handle::fixture_remote_attachment_pair();
    let id = native.actor_identity().unwrap().record.terminal;
    let mut state = host.lock().unwrap();
    state.register_client(1);
    state
        .finish_spawn(1, terminal(id), stream.handle.clone(), false)
        .unwrap();
    install_chat(&mut state, id, native);
    let source_terminal = state.terminals[&id].session.clone();
    let chat = state.chats.get_mut(&id).unwrap();
    chat.session.terminal = Some(source_terminal);
    chat.paired_terminal = Some(stream.handle.clone());
    let candidates = capture(&state);
    let candidate = &candidates[&id];
    assert!(candidate.paired && !candidate.ambiguous);
    let waiting = Activity {
        provider: Provider::Codex,
        state: ActivityState::Waiting,
    };
    for (native, expected) in [
        (
            None,
            AgentTally {
                working: 0,
                waiting: 1,
            },
        ),
        (
            Some((false, ActivityState::Waiting)),
            AgentTally {
                working: 0,
                waiting: 1,
            },
        ),
        (
            Some((true, ActivityState::Unknown)),
            AgentTally {
                working: 0,
                waiting: 1,
            },
        ),
        (Some((true, ActivityState::Idle)), AgentTally::default()),
        (
            Some((true, ActivityState::Waiting)),
            AgentTally {
                working: 0,
                waiting: 1,
            },
        ),
        (
            Some((true, ActivityState::Working)),
            AgentTally {
                working: 1,
                waiting: 0,
            },
        ),
    ] {
        let mut snapshot = TraySnapshot::default();
        candidate.observe_sampled(
            id,
            Some(waiting),
            native.map(|(ready, activity)| ActivityObservation {
                ready,
                activity,
                title: Some("Native name".into()),
            }),
            &mut snapshot,
        );
        assert_eq!(snapshot.tally, expected);
        assert_eq!(snapshot.waiting_agents.len(), expected.waiting);
        if expected.waiting == 1 {
            assert_eq!(snapshot.waiting_agents[0].session, id);
        }
    }
    state.chats.get_mut(&id).unwrap().paired_terminal = None;
    let ambiguous = capture(&state);
    let mut snapshot = TraySnapshot::default();
    ambiguous[&id].observe_sampled(
        id,
        Some(waiting),
        Some(ActivityObservation {
            ready: true,
            activity: ActivityState::Waiting,
            title: None,
        }),
        &mut snapshot,
    );
    assert_eq!(
        snapshot.tally.waiting, 2,
        "unpaired owners are not silently collapsed"
    );
    assert!(snapshot.waiting_agents.is_empty());
    drop(state);

    let many = empty_host();
    let mut state = many.lock().unwrap();
    for _ in 0..crate::tray::MAX_WAITING_AGENTS + 5 {
        let (handle, _) = Handle::fixture_remote_attachment_pair();
        let id = handle.actor_identity().unwrap().record.terminal;
        install_chat(&mut state, id, handle);
    }
    let candidates = capture(&state);
    drop(state);
    let mut snapshot = TraySnapshot::default();
    for (id, candidate) in &candidates {
        candidate.observe_sampled(
            *id,
            None,
            Some(ActivityObservation {
                ready: true,
                activity: ActivityState::Waiting,
                title: Some("界".repeat(1000)),
            }),
            &mut snapshot,
        );
    }
    assert_eq!(snapshot.tally.waiting, crate::tray::MAX_WAITING_AGENTS + 5);
    assert_eq!(
        snapshot.waiting_agents.len(),
        crate::tray::MAX_WAITING_AGENTS
    );
    assert_eq!(
        snapshot
            .waiting_agents
            .iter()
            .map(|row| row.session)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        crate::tray::MAX_WAITING_AGENTS
    );
    assert!(
        snapshot
            .waiting_agents
            .iter()
            .all(|row| row.label.chars().count() <= 100)
    );
}

#[test]
fn native_resident_getter_preserves_long_title_distinctions_through_actual_snapshot_and_menu() {
    let host = empty_host();
    let mut ids = Vec::new();
    for ending in ["alpha", "beta"] {
        let handle = Handle::fixture_waiting_title(format!(
            "{} {ending}",
            "Shared long conversation title ".repeat(30)
        ));
        let id = handle.actor_identity().unwrap().record.terminal;
        let observed = handle.attention_activity().unwrap();
        assert!(observed.ready);
        assert_eq!(observed.activity, ActivityState::Waiting);
        assert!(observed.title.as_deref().unwrap().ends_with(ending));
        assert!(observed.title.as_ref().unwrap().chars().count() <= 100);
        install_chat(&mut host.lock().unwrap(), id, handle);
        ids.push(id);
    }
    // No managed Agent.name/user_title exists, and no synthetic sampled
    // observation bypasses the production native resident getter.
    let observed = snapshot(&host);
    assert_eq!(
        observed.tally,
        AgentTally {
            working: 0,
            waiting: 2
        }
    );
    let menu = crate::tray::waiting_menu_for_test(observed)
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    for (id, ending) in ids.into_iter().zip(["alpha", "beta"]) {
        assert!(menu[&id].contains(ending));
        assert!(menu[&id].ends_with(" — tray-project"));
        assert!(
            !menu[&id].contains(" · Chat "),
            "real title endings must survive rather than arbitrary ordinal fallback"
        );
    }
}

#[test]
fn captured_native_and_console_scope_reaches_distinguishable_full_menu_without_recipe_details() {
    let host = empty_host();
    let (left, _) = Handle::fixture_remote_attachment_pair();
    let (right, _) = Handle::fixture_remote_attachment_pair();
    let left_id = left.actor_identity().unwrap().record.terminal;
    let right_id = right.actor_identity().unwrap().record.terminal;
    let console_id = TerminalId::new();
    let console = RenderStreamFixture::new(|_| {});
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        install_chat(&mut state, left_id, left);
        install_chat(&mut state, right_id, right);
        for (id, path) in [
            (left_id, "/private/client/project"),
            (right_id, "/private/service/project"),
        ] {
            let recipe = &mut state.chats.get_mut(&id).unwrap().session.launch.recipe;
            recipe.working_directory = path.into();
            recipe.environment.insert(
                "TRAY_DISPLAY_SECRET".into(),
                "never-render-recipe-value".into(),
            );
        }
        let mut source = terminal(console_id);
        source.launch.working_directory = "/private/client/project".into();
        source.launch.environment.insert(
            "TRAY_DISPLAY_SECRET".into(),
            "never-render-recipe-value".into(),
        );
        state
            .finish_spawn(1, source, console.handle.clone(), false)
            .unwrap();
    }
    // Use the actual Host capture and sampled activity reducer. Synthetic
    // observations establish display preparation, not real provider readiness.
    let snapshot = snapshot_with(&host, |id, candidate, snapshot| {
        candidate.observe_sampled(
            id,
            candidate.terminal.as_ref().map(|_| Activity {
                provider: Provider::Claude,
                state: ActivityState::Waiting,
            }),
            candidate.chat.as_ref().map(|_| ActivityObservation {
                ready: true,
                activity: ActivityState::Waiting,
                title: None,
            }),
            snapshot,
        );
    });
    assert_eq!(
        snapshot.tally,
        AgentTally {
            working: 0,
            waiting: 3
        }
    );
    let menu = crate::tray::waiting_menu_for_test(snapshot)
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(menu[&left_id], "Claude Code · client/project · Chat");
    assert_eq!(menu[&right_id], "Claude Code · service/project");
    assert_eq!(menu[&console_id], "Claude Code · client/project · Console");
    for (session, label) in menu {
        assert!(!label.contains(&session.to_string()));
        assert!(!label.contains("TRAY_DISPLAY_SECRET"));
        assert!(!label.contains("never-render-recipe-value"));
        assert!(!label.contains("private"));
    }
}

#[test]
fn proven_pair_menu_scope_keeps_both_surfaces_across_native_readiness_fallback() {
    let host = empty_host();
    let (paired_native, _) = Handle::fixture_remote_attachment_pair();
    let (standalone, _) = Handle::fixture_remote_attachment_pair();
    let pair_id = paired_native.actor_identity().unwrap().record.terminal;
    let standalone_id = standalone.actor_identity().unwrap().record.terminal;
    let console = RenderStreamFixture::new(|_| {});
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state
            .finish_spawn(1, terminal(pair_id), console.handle.clone(), false)
            .unwrap();
        install_chat(&mut state, pair_id, paired_native);
        install_chat(&mut state, standalone_id, standalone);
        let source = state.terminals[&pair_id].session.clone();
        let chat = state.chats.get_mut(&pair_id).unwrap();
        chat.session.terminal = Some(source);
        chat.paired_terminal = Some(console.handle.clone());
        assert!(paired(&state, pair_id));
    }
    for ready in [false, true] {
        let snapshot = snapshot_with(&host, |id, candidate, snapshot| {
            candidate.observe_sampled(
                id,
                candidate.terminal.as_ref().map(|_| Activity {
                    provider: Provider::Claude,
                    state: ActivityState::Waiting,
                }),
                Some(ActivityObservation {
                    ready: id != pair_id || ready,
                    activity: ActivityState::Waiting,
                    title: None,
                }),
                snapshot,
            );
        });
        assert_eq!(
            snapshot.tally,
            AgentTally {
                working: 0,
                waiting: 2
            }
        );
        let menu = crate::tray::waiting_menu_for_test(snapshot)
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            menu[&pair_id],
            "Claude Code · tray-project · Chat + console"
        );
        assert_eq!(menu[&standalone_id], "Claude Code · tray-project · Chat");
    }
}

#[test]
fn navigation_errors_retain_queue_and_callbacks_can_acquire_host() {
    for case in 0..4 {
        let host = empty_host();
        let destination = destination("tray-owner");
        let id = TerminalId::new();
        let intent = request(id, &destination.window);
        host.lock()
            .unwrap()
            .queue_tray_session(destination.clone(), intent.clone())
            .unwrap();
        let activates = Cell::new(0);
        let launches = Cell::new(0);
        let result = navigate_with(
            &host,
            &destination,
            &intent,
            || {
                host.lock().unwrap().register_client(10);
                activates.set(activates.get() + 1);
                match case {
                    0 => Err("session socket failed".into()),
                    3 if activates.get() == 4 => Ok(true),
                    _ => Ok(false),
                }
            },
            || {
                host.lock().unwrap().register_client(11);
                launches.set(launches.get() + 1);
                if case == 1 {
                    Err("matched launcher failed".into())
                } else {
                    Ok(())
                }
            },
            || {
                host.lock().unwrap().register_client(12);
            },
            std::time::Instant::now,
        );
        match case {
            0 => {
                assert_eq!(result.unwrap_err(), "session socket failed");
                assert_eq!(launches.get(), 0);
            }
            1 => {
                assert_eq!(result.unwrap_err(), "matched launcher failed");
                assert_eq!(activates.get(), 1);
            }
            2 => {
                assert!(result.unwrap_err().contains("did not accept"));
                assert_eq!(activates.get(), 151, "the cold delivery bound is finite");
            }
            3 => {
                assert!(result.is_ok());
                assert_eq!(activates.get(), 4);
                assert_eq!(launches.get(), 1);
            }
            _ => unreachable!(),
        }
        assert_eq!(
            drain(&host, &destination),
            if case == 3 {
                None
            } else {
                Some(Target::Session(id))
            }
        );
    }
}

#[test]
fn slow_successful_launcher_receives_full_post_launch_delivery_budget() {
    let host = empty_host();
    let destination = destination("tray-owner");
    let intent = request(TerminalId::new(), &destination.window);
    host.lock()
        .unwrap()
        .queue_tray_session(destination.clone(), intent.clone())
        .unwrap();
    let origin = std::time::Instant::now();
    let elapsed = Cell::new(Duration::ZERO);
    let sends = Cell::new(0);
    let launches = Cell::new(0);
    let result = navigate_with(
        &host,
        &destination,
        &intent,
        || {
            sends.set(sends.get() + 1);
            Ok(sends.get() == 2)
        },
        || {
            launches.set(launches.get() + 1);
            // The real launcher can successfully finish its existing recovery
            // handshake after more than the separate delivery budget.
            elapsed.set(Duration::from_secs(20));
            Ok(())
        },
        || panic!("the newly ready window must receive its immediate typed send"),
        || origin + elapsed.get(),
    );
    assert!(result.is_ok());
    assert_eq!(launches.get(), 1);
    assert_eq!(
        sends.get(),
        2,
        "successful slow launch still delivers the exact intent"
    );
    assert_eq!(elapsed.get(), Duration::from_secs(20));
    assert_eq!(
        drain(&host, &destination),
        None,
        "only the admitted exact request is acknowledged"
    );
}

#[test]
fn cold_delivery_elapsed_deadline_preserves_exact_intent_without_more_writes() {
    let host = empty_host();
    let destination = destination("tray-owner");
    let intent = request(TerminalId::new(), &destination.window);
    host.lock()
        .unwrap()
        .queue_tray_session(destination.clone(), intent.clone())
        .unwrap();
    let origin = std::time::Instant::now();
    let elapsed = Cell::new(Duration::ZERO);
    let sends = Cell::new(0);
    let waits = Cell::new(0);
    let result = navigate_with(
        &host,
        &destination,
        &intent,
        || {
            sends.set(sends.get() + 1);
            Ok(false)
        },
        || Ok(()),
        || {
            waits.set(waits.get() + 1);
            elapsed.set(Duration::from_secs(15));
        },
        || origin + elapsed.get(),
    );
    assert!(result.unwrap_err().contains("did not accept"));
    assert_eq!(
        sends.get(),
        2,
        "deadline prevents the next post-launch write"
    );
    assert_eq!(waits.get(), 1);
    assert_eq!(
        host.lock().unwrap().pending_tray_session(&destination),
        Some(&intent)
    );
}

#[test]
fn superseded_cold_request_cannot_send_again_after_backoff() {
    let host = empty_host();
    let destination = destination("tray-owner");
    let intent = request(TerminalId::new(), &destination.window);
    let replacement = request(intent.session, &destination.window);
    host.lock()
        .unwrap()
        .queue_tray_session(destination.clone(), intent.clone())
        .unwrap();
    let sends = Cell::new(0);
    let launches = Cell::new(0);
    let waits = Cell::new(0);
    let result = navigate_with(
        &host,
        &destination,
        &intent,
        || {
            sends.set(sends.get() + 1);
            Ok(false)
        },
        || {
            launches.set(launches.get() + 1);
            Ok(())
        },
        || {
            waits.set(waits.get() + 1);
            host.lock()
                .unwrap()
                .queue_tray_session(destination.clone(), replacement.clone())
                .unwrap();
        },
        std::time::Instant::now,
    );
    assert!(result.unwrap_err().contains("replaced"));
    assert_eq!(
        sends.get(),
        2,
        "the superseded request cannot make its next socket write"
    );
    assert_eq!(launches.get(), 1);
    assert_eq!(waits.get(), 1);
    assert_eq!(
        host.lock().unwrap().pending_tray_session(&destination),
        Some(&replacement)
    );
}

#[test]
fn old_navigation_ack_cannot_remove_newer_destination_intent() {
    let host = empty_host();
    let destination = destination("tray-owner");
    let old = TerminalId::new();
    let current = old;
    let old_request = request(old, &destination.window);
    let current_request = request(current, &destination.window);
    host.lock()
        .unwrap()
        .queue_tray_session(destination.clone(), old_request.clone())
        .unwrap();
    let result = navigate_with(
        &host,
        &destination,
        &old_request,
        || {
            host.lock()
                .unwrap()
                .queue_tray_session(destination.clone(), current_request.clone())
                .unwrap();
            Ok(true)
        },
        || panic!("an acknowledged existing window must not launch a child"),
        || panic!("an admitted existing window must not back off"),
        std::time::Instant::now,
    );
    assert!(result.is_ok());
    assert_eq!(
        host.lock().unwrap().pending_tray_session(&destination),
        Some(&current_request),
        "an old ACK cannot consume a newer nonce even for the identical session"
    );
    assert_eq!(drain(&host, &destination), Some(Target::Session(current)));
    assert_eq!(drain(&host, &destination), None);
}

#[test]
fn newer_tray_choice_retires_old_generic_watch_and_new_desktop_choice_retires_tray() {
    let host = empty_host();
    let destination = destination(crate::instance::PRIMARY_WINDOW);
    let old = TerminalId::new();
    let intent = request(TerminalId::new(), &destination.window);
    {
        let mut state = host.lock().unwrap();
        state.activate_notification(Target::Session(old), Some(old), None);
        state
            .queue_tray_session(destination.clone(), intent.clone())
            .unwrap();
        assert_eq!(
            state.watch_notifications(100, presence(&destination, None, false)),
            None
        );
        assert_eq!(state.pending_tray_session(&destination), Some(&intent));
        state.activate_notification(Target::Session(old), Some(old), None);
        assert!(state.pending_tray_session(&destination).is_none());
    }
    assert!(
        navigate_with(
            &host,
            &destination,
            &intent,
            || panic!("superseded request cannot dispatch"),
            || panic!("superseded request cannot launch"),
            || panic!("superseded request cannot retry"),
            std::time::Instant::now,
        )
        .unwrap_err()
        .contains("replaced")
    );
}

#[test]
fn bounded_queue_validates_destination_and_drains_exact_current_intent() {
    let host = empty_host();
    let original = TerminalId::new();
    let replacement = TerminalId::new();
    let mut state = host.lock().unwrap();
    let editor = Destination {
        application: Application::Ceditor,
        window: "editor".into(),
    };
    assert!(
        state
            .queue_tray_session(editor, request(original, "editor"))
            .is_err()
    );
    assert!(
        state
            .queue_tray_session(destination("../invalid"), request(original, "../invalid"))
            .is_err()
    );
    for index in 0..32 {
        state
            .queue_tray_session(
                destination(&format!("tray-{index}")),
                request(original, &format!("tray-{index}")),
            )
            .unwrap();
    }
    assert!(
        state
            .queue_tray_session(destination("overflow"), request(original, "overflow"))
            .is_err()
    );
    state
        .queue_tray_session(destination("tray-0"), request(replacement, "tray-0"))
        .unwrap();
    drop(state);
    for index in 0..32 {
        assert_eq!(
            drain(&host, &destination(&format!("tray-{index}"))),
            Some(Target::Session(if index == 0 {
                replacement
            } else {
                original
            }))
        );
    }
    assert_eq!(drain(&host, &destination("overflow")), None);
}

#[test]
fn destination_selection_requires_exact_session_and_cagents_audience() {
    let host = empty_host();
    let id = TerminalId::new();
    let unrelated = TerminalId::new();
    let mut state = host.lock().unwrap();
    let editor = Destination {
        application: Application::Ceditor,
        window: "editor".into(),
    };
    state.watch_notifications(1, presence(&editor, Some(id), true));
    state.watch_notifications(
        2,
        presence(&destination("unrelated"), Some(unrelated), true),
    );
    assert_eq!(
        state.tray_destination(id),
        destination(crate::instance::PRIMARY_WINDOW)
    );
    state.watch_notifications(3, presence(&destination("working"), Some(id), false));
    state.watch_notifications(4, presence(&destination("focused"), Some(id), true));
    assert_eq!(state.tray_destination(id), destination("focused"));
}

#[test]
fn attention_registration_requires_cagents_and_unscoped_authority() {
    use crate::agent_execution::Authority;
    assert!(attention_enabled(
        Application::Cagents,
        &Authority::default()
    ));
    assert!(!attention_enabled(
        Application::Ceditor,
        &Authority::default()
    ));
    for token in [String::new(), "synthetic-test-scope".to_owned()] {
        let authority = Authority::session(token);
        assert!(!attention_enabled(Application::Cagents, &authority));
        assert!(!attention_enabled(Application::Ceditor, &authority));
    }
}

#[test]
fn snapshot_releases_host_before_actual_candidate_observation() {
    let host = empty_host();
    let stream = RenderStreamFixture::new(|_| {});
    let id = TerminalId::new();
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state
            .finish_spawn(1, terminal(id), stream.handle.clone(), false)
            .unwrap();
    }
    let observed = Cell::new(0);
    let result = snapshot_with(&host, |current, candidate, snapshot| {
        assert_eq!(current, id);
        host.lock().unwrap().register_client(2);
        observed.set(observed.get() + 1);
        candidate.observe(current, snapshot);
    });
    assert_eq!(observed.get(), 1);
    assert_eq!(result.tally, AgentTally::default());
    assert!(result.waiting_agents.is_empty());
}
