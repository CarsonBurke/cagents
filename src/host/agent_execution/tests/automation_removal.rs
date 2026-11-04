//! Removing a finished schedule must not require a process from a prior boot.
use super::*;
use crate::agent_execution::ResultSummary;
use uuid::Uuid;

#[test]
fn removal_retires_only_completed_missing_providers_from_a_proven_previous_boot() {
    if isolated("host::agent_execution::tests::automation_removal::removal_retires_only_completed_missing_providers_from_a_proven_previous_boot").is_some() {
        return;
    }
    for case in 0..9 {
        let (host, request, path) = fixture();
        let automation = automation_fixture(&host, &request);
        let accepted = claim_fixture(&host, &automation);
        let Destination::New(spec) = &request.destination else {
            unreachable!()
        };
        let mut proof = host.lock().unwrap().proof().clone();
        if case != 1 {
            proof.boot_id = "previous-kernel-boot".into();
        }
        if case == 2 {
            proof.host_incarnation = "".into();
        }
        let source = chat::Session {
            id: crate::model::TerminalId::new(),
            launch: chat::Launch {
                provider: spec.provider,
                recipe: crate::model::LaunchRecipe {
                    program: "fixture-provider-never-started".into(),
                    arguments: vec![],
                    working_directory: spec.target.directory.clone(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some(Uuid::new_v4().to_string()),
            },
            native_continuity: Default::default(),
            proof: (case != 3).then_some(proof),
            terminal: None,
            owned_codex_listener: None,
        };
        let store = registry(&host).unwrap();
        let (run, firing) = {
            let mut store = store.lock().unwrap();
            let agent = store
                .bind_agent(accepted.agent.id, None, HostedSession::Chat(source.clone()))
                .unwrap();
            store
                .fresh_agent_authority(&HostedSession::Chat(source.clone()))
                .unwrap();
            let run = store
                .update_agent_run(
                    accepted.run.id,
                    accepted.run.revision,
                    Update::Started {
                        provider_key: "saved-turn".into(),
                    },
                    100,
                )
                .unwrap();
            if case == 4 {
                (run, None)
            } else {
                let run = store
                    .update_agent_run(
                        run.id,
                        run.revision,
                        Update::Finished {
                            state: if case == 5 {
                                State::Interrupted
                            } else {
                                State::Succeeded
                            },
                            result: ResultSummary {
                                text: "Saved result".into(),
                                ..Default::default()
                            },
                        },
                        200,
                    )
                    .unwrap();
                let firing = store
                    .reconcile_automation_run(run.id, 200)
                    .unwrap()
                    .unwrap();
                if case != 5 {
                    store
                        .finish_automation_release(firing.id, &agent, 200)
                        .unwrap();
                }
                (run, Some(firing.id))
            }
        };
        if case == 6 {
            host.lock()
                .unwrap()
                .chat_spawning
                .insert(source.id, source.clone());
        }
        if case == 7 {
            host.lock().unwrap().reserve_spawn(source.id).unwrap();
        }
        if case == 8 {
            let agent = store.lock().unwrap().agent(accepted.agent.id).unwrap();
            let mut scheduler = store.lock().unwrap().execution_connection().unwrap();
            let next = scheduler
                .claim_automation(
                    automation.id,
                    automation.execution_revision,
                    None,
                    crate::automations::Trigger::Manual {
                        request: Uuid::new_v4(),
                    },
                    None,
                    true,
                    300,
                )
                .unwrap()
                .accepted
                .unwrap();
            assert_eq!(next.agent.id, agent.id);
            // The cleanup snapshot preceded this acceptance on another WAL
            // connection. A local mutex cannot authorize its cancellation.
            assert!(
                store
                    .lock()
                    .unwrap()
                    .retire_completed_automation_process(&agent, &run, 400)
                    .is_err()
            );
            assert_eq!(scheduler.agent_run(next.run.id).unwrap(), next.run);
            assert!(!scheduler.agent(agent.id).unwrap().closed);
        }
        let remove = crate::automations::Request::Remove {
            id: automation.id,
            revision: automation.revision,
        };
        let result = if case == 0 {
            // Keep the retirement transaction waiting while a concurrent client
            // lists inventory. This is an actual WAL writer, not a test hook.
            let mut writer = rusqlite::Connection::open(&path).unwrap();
            let transaction = writer
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let removing = host.clone();
            let worker = std::thread::spawn(move || {
                crate::host::automations::service::process_under_control(
                    &removing,
                    Authority::default(),
                    remove,
                )
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            let reserved = loop {
                if host.lock().unwrap().chat_spawning.contains_key(&source.id) {
                    break true;
                }
                if worker.is_finished() || Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            // Genuine pending recovery can retain an old proof too. It must
            // remain visible; only the exact cleanup reservation is private.
            let mut pending = source.clone();
            pending.id = crate::model::TerminalId::new();
            pending.launch.session_id = Some(Uuid::new_v4().to_string());
            let current_proof = {
                let mut state = host.lock().unwrap();
                state.chat_spawning.insert(pending.id, pending.clone());
                state.proof().clone()
            };
            let inventory =
                crate::host::chat::process(&host, 2, crate::terminal_ipc::ClientRequest::ChatList);
            let capacity = crate::host::chat::capacity::status(&host);
            let duplicate_start_refused = host.lock().unwrap().reserve_spawn(source.id).is_err();
            host.lock().unwrap().chat_spawning.remove(&pending.id);
            // Always release the writer and join before assertions, including
            // the expected pre-fix failure, so no blocked worker is abandoned.
            transaction.rollback().unwrap();
            let result = worker.join().unwrap();
            assert!(reserved, "removal never reached its in-flight reservation");
            assert!(
                duplicate_start_refused,
                "cleanup must retain admission custody"
            );
            let crate::terminal_ipc::ServerResponse::ChatListed { sessions } = inventory.unwrap()
            else {
                panic!("expected chat inventory");
            };
            assert!(
                !sessions.iter().any(|session| session.id == source.id),
                "historical cleanup must not publish a current attachment: {sessions:?}"
            );
            assert_eq!(sessions.len(), 1, "ordinary pending recovery stays visible");
            assert_eq!(sessions[0].id, pending.id);
            assert_eq!(sessions[0].proof.as_ref(), Some(&current_proof));
            assert_eq!(capacity.resident, 0);
            assert!(capacity.residents.is_empty());
            assert_eq!(capacity.omitted_residents, 0);
            assert_eq!(capacity.spawning, 2, "admission counts remain conservative");
            result
        } else {
            crate::host::automations::service::process_under_control(
                &host,
                Authority::default(),
                remove,
            )
        };
        let store = store.lock().unwrap();
        if case == 0 {
            assert!(
                matches!(result, Ok(crate::automations::Response::Removed)),
                "{result:?}"
            );
            assert!(store.automation(automation.id).is_err());
            assert!(store.agent(accepted.agent.id).unwrap().closed);
            assert!(
                store
                    .retained_agent_authority_source(source.id)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                store
                    .automation_history(Some(automation.id), None, 50)
                    .unwrap()
                    .firings[0]
                    .id,
                firing.unwrap()
            );
        } else {
            assert!(
                result.is_err(),
                "case {case}: uncertain or current work must remain owned"
            );
            assert!(store.automation(automation.id).is_ok());
            assert!(!store.agent(accepted.agent.id).unwrap().closed);
            assert!(
                store
                    .retained_agent_authority_source(source.id)
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(
            store.agent_run(run.id).unwrap(),
            run,
            "saved result is immutable"
        );
        let state = host.lock().unwrap();
        assert!(state.chats.is_empty());
        assert_eq!(
            state.chat_spawning.get(&source.id),
            (case == 6).then_some(&source),
            "cleanup releases only its own reservation"
        );
        drop(state);
        assert!(
            !path.parent().unwrap().join("spawns").exists(),
            "removal never launches or replays a provider"
        );
    }
}
