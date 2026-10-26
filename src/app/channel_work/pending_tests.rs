mod pending_adapter {
    use super::*;

    fn record(home: Home) -> Record {
        Record::new(
            crate::channel_work::Mutation {
                home,
                expected_revision: 0,
                operation: uuid::Uuid::new_v4(),
                change: crate::channel_work::Change::Claim {
                    target: crate::channel_work::Target::Local(uuid::Uuid::new_v4()),
                    role: crate::channel_work::Role::Contribute,
                },
            },
            Some(uuid::Uuid::new_v4()),
            Destination {
                profile: recovery::capture_profile(),
                socket: "/private/original.sock".into(),
                proof: crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new("original"),
                    boot_id: crate::model::BootId::new("original"),
                    host_incarnation: crate::model::HostIncarnation::new("original"),
                },
            },
            "Original channel".into(),
            1,
        )
        .unwrap()
    }

    fn receipt(record: &Record) -> crate::channel_work::Receipt {
        crate::channel_work::Receipt {
            home: record.mutation().home,
            operation: record.operation(),
            revision: 1,
            sequence: 1,
            changed: true,
        }
    }

    fn reply(record: &Record) -> crate::channel_work::Reply {
        crate::channel_work::Reply {
            receipt: receipt(record),
            snapshot: crate::channel_work::Snapshot {
                home: record.mutation().home,
                lifecycle: crate::channel_work::HomeLifecycle::Active,
                revision: 1,
                entries: vec![],
                history: vec![],
                next_current: None,
                next_history: None,
            },
        }
    }

    fn hydrate(app: &mut Cagents, records: Vec<Record>) {
        // The real startup/load effect is lazy: the test supplies its matching
        // disk-only completion without starting a daemon or transport task.
        drop(app.load_saved_channel_work());
        let desired = app.channel_work.desired_saved.unwrap();
        if let Some(actual) = app
            .channel_work
            .loading_saved
            .filter(|actual| *actual != desired)
        {
            drop(app.complete_channel_work(Completion::Loaded {
                generation: actual,
                result: Err("Earlier fixture read completed".into()),
            }));
        }
        let generation = app.channel_work.loading_saved.unwrap();
        assert_eq!(generation, desired);
        let occupied = records.len();
        drop(app.complete_channel_work(Completion::Loaded {
            generation,
            result: Ok(journal::Loaded {
                records,
                problems: vec![],
                occupied,
                saturated: false,
            }),
        }));
    }

    #[test]
    fn mutation_sender_requires_exact_durable_reservation_and_preserves_positive_cleanup_failure() {
        let parent = tempfile::tempdir().unwrap();
        let journal_path = parent.path().join("pending");
        let journal = journal::Journal::at(journal_path.clone()).unwrap();
        let record = record(captured());
        let mut sends = 0;
        let (result, settled) = reserve_then_send(&journal, &record, |request| {
            sends += 1;
            assert_eq!(request, record.wire_request().unwrap());
            // The sender sees the exact record through a fresh locked load,
            // not merely a candidate that has not been published yet.
            assert_eq!(journal.load().unwrap().records, [record.clone()]);
            Ok(api::Response::Applied(reply(&record)))
        })
        .unwrap();
        assert_eq!(sends, 1);
        assert_eq!(result.unwrap().receipt, receipt(&record));
        assert!(settled);
        assert!(journal.load().unwrap().records.is_empty());

        journal.reserve(&record).unwrap();
        let mut altered = serde_json::to_value(&record).unwrap();
        altered["title"] = serde_json::json!("Conflicting original");
        let altered: Record = serde_json::from_value(altered).unwrap();
        assert!(matches!(
            reserve_then_send(&journal, &altered, |_| {
                sends += 1;
                Ok(api::Response::Applied(reply(&altered)))
            }),
            Err(journal::Error::Conflict)
        ));
        assert_eq!(
            sends, 1,
            "a failed exact reservation invokes no mutation sender"
        );
        assert_eq!(journal.load().unwrap().records, [record.clone()]);

        let (result, settled) = reserve_then_send(&journal, &record, |_| {
            let mut foreign = reply(&record);
            foreign.receipt.operation = uuid::Uuid::new_v4();
            Ok(api::Response::Applied(foreign))
        })
        .unwrap();
        assert!(result.is_err());
        assert!(!settled);
        assert_eq!(
            journal.load().unwrap().records,
            [record.clone()],
            "a foreign receipt cannot remove the original record"
        );

        let (result, settled) = reserve_then_send(&journal, &record, |_| {
            // A changed local record makes cleanup refuse after the genuine
            // positive response. This does not unconfirm the server outcome.
            std::fs::write(
                journal_path.join(format!("{}.json", record.operation())),
                b"inconsistent fixture record",
            )
            .unwrap();
            Ok(api::Response::Applied(reply(&record)))
        })
        .unwrap();
        assert_eq!(result.unwrap().receipt, receipt(&record));
        assert!(!settled);
        assert!(!journal.load().unwrap().problems.is_empty());

        let mut view = ui::State::default();
        let Some(Effect::Read { ticket, .. }) =
            view.open(record.mutation().home, "Original".into())
        else {
            panic!()
        };
        let completion = dispatch_saved(
            ticket,
            record.mutation().clone(),
            record.selection(),
            Some(record.clone()),
            Some(Route::saved(&record)),
            "Unused".into(),
            false,
        );
        assert!(
            matches!(
                completion,
                Completion::BeforeSendFailed {
                    retained: true,
                    record: Some(_),
                    ..
                }
            ),
            "an unverified saved route cannot connect or resend"
        );
        let completion = dispatch_saved(
            ticket,
            record.mutation().clone(),
            record.selection(),
            None,
            None,
            "Unused".into(),
            false,
        );
        assert!(
            matches!(
                completion,
                Completion::BeforeSendFailed {
                    retained: false,
                    record: None,
                    ..
                }
            ),
            "no route, journal attempt or RPC is a known pre-send failure"
        );
    }

    #[test]
    fn saved_hydration_rejects_stale_conflicting_and_overflow_unions_before_route_adoption() {
        let (mut app, _) = main_fixture();
        let original = record(captured());
        drop(app.load_saved_channel_work());
        let generation = app.channel_work.loading_saved.unwrap();
        drop(app.complete_channel_work(Completion::Loaded {
            generation: uuid::Uuid::new_v4(),
            result: Ok(journal::Loaded {
                records: vec![original.clone()],
                problems: vec![],
                occupied: 1,
                saturated: false,
            }),
        }));
        assert!(app.channel_work.saved.is_empty());
        drop(app.complete_channel_work(Completion::Loaded {
            generation,
            result: Ok(journal::Loaded {
                records: vec![original.clone()],
                problems: vec![],
                occupied: 1,
                saturated: false,
            }),
        }));
        assert_eq!(
            app.channel_work.saved.get(&original.operation()),
            Some(&original)
        );
        drop(app.load_saved_channel_work());
        let invalidated = app.channel_work.loading_saved.unwrap();
        let late = record(captured());
        app.channel_work.view.invalidate_saved_load();
        drop(app.complete_channel_work(Completion::Loaded {
            generation: invalidated,
            result: Ok(journal::Loaded {
                records: vec![late.clone()],
                problems: vec![],
                occupied: 1,
                saturated: false,
            }),
        }));
        assert!(
            !app.channel_work.saved.contains_key(&late.operation()),
            "the leaf's retired generation also fences adapter adoption"
        );
        assert!(
            !app.channel_work
                .operation_routes
                .contains_key(&late.operation())
        );

        let mut conflicting = serde_json::to_value(&original).unwrap();
        conflicting["destination"]["socket"] = serde_json::json!("/private/foreign.sock");
        hydrate(&mut app, vec![serde_json::from_value(conflicting).unwrap()]);
        assert_eq!(
            app.channel_work.saved.get(&original.operation()),
            Some(&original)
        );
        assert_eq!(
            app.channel_work.operation_routes[&original.operation()].socket,
            original.destination().socket
        );
        assert!(!app.channel_work.saved_ready);

        for _ in 0..15 {
            hydrate(&mut app, vec![record(captured())]);
        }
        assert_eq!(app.channel_work.saved.len(), 16);
        let old_routes = app
            .channel_work
            .operation_routes
            .keys()
            .copied()
            .collect::<HashSet<_>>();
        let overflow = record(captured());
        hydrate(&mut app, vec![overflow.clone()]);
        assert_eq!(app.channel_work.saved.len(), 16);
        assert!(!app.channel_work.saved.contains_key(&overflow.operation()));
        assert_eq!(
            app.channel_work
                .operation_routes
                .keys()
                .copied()
                .collect::<HashSet<_>>(),
            old_routes
        );
        assert_eq!(app.channel_work.view.pending_homes().count(), 16);
        assert!(!app.channel_work.view.saved_discovery_ready());
    }

    #[test]
    fn confirmed_unsaved_operation_cannot_retry_or_be_resurrected_by_old_load_and_dispatch() {
        let (mut app, _) = main_fixture();
        let record = record(captured());
        let operation = record.operation();
        hydrate(&mut app, vec![record.clone()]);
        drop(app.open_saved_channel_work(operation));
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update_channel_work(ui::Action::Check(owner, operation)));
        let ticket = app.channel_work.lookups[&operation];
        drop(app.load_saved_channel_work());
        let old_load = app.channel_work.loading_saved.unwrap();
        let mut checked = Route::saved(&record);
        checked.recovery_verified = true;
        checked.socket = "/private/checked-original.sock".into();
        drop(app.complete_channel_work(Completion::LookedUp {
            ticket,
            home: record.mutation().home,
            operation,
            route: Some(checked.clone()),
            result: Ok(Some(receipt(&record))),
            settled: false,
        }));
        assert_eq!(app.channel_work.saved.get(&operation), Some(&record));
        assert_eq!(
            app.channel_work.operation_routes[&operation].socket,
            checked.socket
        );
        assert!(
            app.channel_work
                .view
                .update(ui::Action::Retry(owner, operation))
                .is_none(),
            "positive receipt disables resend while cleanup is unconfirmed"
        );
        assert_ne!(app.channel_work.desired_saved, Some(old_load));
        assert_eq!(
            app.channel_work.loading_saved,
            Some(old_load),
            "old physical read survives until its actual completion"
        );
        drop(app.complete_channel_work(Completion::Loaded {
            generation: old_load,
            result: Ok(journal::Loaded {
                records: vec![record.clone()],
                problems: vec![],
                occupied: 1,
                saturated: false,
            }),
        }));
        assert_eq!(app.channel_work.saved.get(&operation), Some(&record));
        assert_eq!(
            app.channel_work.operation_routes[&operation].socket,
            checked.socket
        );

        drop(app.update_channel_work(ui::Action::Check(owner, operation)));
        let next = app.channel_work.lookups[&operation];
        drop(app.complete_channel_work(Completion::LookedUp {
            ticket: next,
            home: record.mutation().home,
            operation,
            route: Some(checked),
            result: Ok(Some(receipt(&record))),
            settled: true,
        }));
        assert!(!app.channel_work.saved.contains_key(&operation));
        assert!(!app.channel_work.operation_routes.contains_key(&operation));
        drop(app.complete_channel_work(Completion::Applied {
            ticket,
            record: record.clone(),
            route: Route::saved(&record),
            result: Err(Failure::Unavailable),
            settled: false,
        }));
        assert!(!app.channel_work.saved.contains_key(&operation));
        assert!(!app.channel_work.operation_routes.contains_key(&operation));
        // Check updates only its per-operation route, not the view transport.
        assert_eq!(
            app.channel_work.routes[&record.mutation().home].socket,
            record.destination().socket
        );
    }

    #[test]
    fn saved_list_and_absent_home_use_main_read_only_without_recreating_membership_or_navigation() {
        let (mut app, ordinary) = main_fixture();
        let saved = record(captured());
        let before = app.workspace_model.clone();
        let active = app.durable.active_channel;
        hydrate(&mut app, vec![saved.clone()]);
        drop(app.update_channel_work(ui::Action::ShowSaved));
        assert!(app.channel_work_is_main());
        assert!(app.channel_work.view.saved_is_shown());
        assert!(app.channel_home().is_none());
        assert_eq!(app.durable.active_channel, active);
        drop(app.open_saved_channel_work(saved.operation()));
        assert!(app.channel_work_is_main());
        assert_eq!(
            app.channel_home(),
            Some((saved.mutation().home, saved.title().to_owned()))
        );
        assert_eq!(app.workspace_model, before);
        assert_eq!(app.durable.active_channel, active);
        let owner = app.channel_work.view.owner().unwrap();
        assert_ne!(owner.home, ordinary);
        assert!(
            app.channel_work
                .view
                .update(ui::Action::Attach(owner))
                .is_none()
        );
        assert!(!app.channel_work.routes[&owner.home].recovery_verified);
        assert!(!app.channel_work.operation_routes[&saved.operation()].recovery_verified);
        let original = app.channel_work.operation_routes[&saved.operation()]
            .socket
            .clone();
        app.install_normal_channel_route_with(owner.home, recovery::capture_profile(), || {
            Ok("/private/current.sock".into())
        });
        assert_eq!(
            app.channel_work.operation_routes[&saved.operation()].socket,
            original
        );
        assert_eq!(
            app.channel_work.routes[&owner.home].socket,
            PathBuf::from("/private/current.sock")
        );
        app.install_normal_channel_route_with(owner.home, recovery::capture_profile(), || {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert!(
            !app.channel_work.routes.contains_key(&owner.home),
            "normal route failure cannot fall back to saved transport"
        );
    }

    #[test]
    fn empty_home_retirement_waits_for_discovery_and_pending_settlement_then_emits_once() {
        let mut app = super::super::super::tests::context_inspector_test_app(false);
        let home = captured();
        app.workspace_model = model(home);
        let (send, received) = async_channel::unbounded();
        app.workspace_model_tx = send;
        app.channel_work.retire_candidates.insert(home);
        app.retire_settled_empty_channels();
        assert!(received.try_recv().is_err());
        assert!(app.channel_work.retire_candidates.contains(&home));
        drop(app.load_saved_channel_work());
        let generation = app.channel_work.loading_saved.unwrap();
        drop(app.complete_channel_work(Completion::Loaded {
            generation,
            result: Err("Fixture unreadable journal".into()),
        }));
        app.retire_settled_empty_channels();
        assert!(received.try_recv().is_err());
        let record = record(home);
        hydrate(&mut app, vec![record.clone()]);
        app.retire_settled_empty_channels();
        assert!(
            received.try_recv().is_err(),
            "hydration preserves the exact unsent Home candidate"
        );
        assert!(app.channel_work.retire_candidates.contains(&home));
        drop(app.open_saved_channel_work(record.operation()));
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update_channel_work(ui::Action::Check(owner, record.operation())));
        let ticket = app.channel_work.lookups[&record.operation()];
        drop(app.complete_channel_work(Completion::LookedUp {
            ticket,
            home,
            operation: record.operation(),
            route: Some(Route::saved(&record)),
            result: Ok(Some(receipt(&record))),
            settled: true,
        }));
        assert!(
            received.try_recv().is_err(),
            "a new disk-only hydration still owns the retirement fence"
        );
        hydrate(&mut app, vec![]);
        assert_eq!(
            received.try_recv().unwrap(),
            ModelEdit::RetireEmptyChannel {
                home,
                expected_view_revision: 7
            }
        );
        app.retire_settled_empty_channels();
        assert!(
            received.try_recv().is_err(),
            "the candidate emits only once"
        );
        assert!(!app.channel_work.retire_candidates.contains(&home));

        // Capacity stops fresh intent, not unrelated retirement when all16
        // original Homes are known. A17th overflow is incomplete discovery.
        for overflow in [false, true] {
            let mut app = super::super::super::tests::context_inspector_test_app(false);
            let unrelated = captured();
            app.workspace_model = model(unrelated);
            let (send, received) = async_channel::unbounded();
            app.workspace_model_tx = send;
            let records = (0..journal::MAX_RECORDS)
                .map(|_| {
                    self::record(Home {
                        registry: unrelated.registry,
                        channel: ChannelId::new(),
                        creation_ordinal: 4,
                    })
                })
                .collect::<Vec<_>>();
            let protected = records[0].mutation().home;
            app.channel_work
                .retire_candidates
                .extend([unrelated, protected]);
            drop(app.load_saved_channel_work());
            let generation = app.channel_work.loading_saved.unwrap();
            drop(app.complete_channel_work(Completion::Loaded {
                generation,
                result: Ok(journal::Loaded {
                    records,
                    problems: if overflow {
                        vec![journal::Problem {
                            operation: None,
                            kind: journal::ProblemKind::Overflow,
                        }]
                    } else {
                        vec![]
                    },
                    occupied: journal::MAX_RECORDS + usize::from(overflow),
                    saturated: true,
                }),
            }));
            if overflow {
                assert!(
                    received.try_recv().is_err(),
                    "an unknown overflow Home retains every candidate"
                );
                assert!(app.channel_work.retire_candidates.contains(&unrelated));
                assert!(!app.channel_work.view.saved_discovery_ready());
            } else {
                assert_eq!(
                    received.try_recv().unwrap(),
                    ModelEdit::RetireEmptyChannel {
                        home: unrelated,
                        expected_view_revision: 7
                    }
                );
                assert!(received.try_recv().is_err());
                assert!(!app.channel_work.retire_candidates.contains(&unrelated));
                assert!(app.channel_work.view.saved_discovery_ready());
            }
            assert!(app.channel_work.retire_candidates.contains(&protected));
        }
    }
    // Exercise actual rendered field/button callbacks. The resulting Tasks are
    // kept lazy; these tests do not start a daemon or connect to a provider.
    fn prepared_local(app: &mut Cagents) -> (ui::FormOwner, api::LocalSelection) {
        let owner = app.channel_work.view.owner().unwrap();
        let Some(Effect::Focus { id, .. }) =
            app.channel_work.view.update(ui::Action::AttachLocal(owner))
        else {
            panic!()
        };
        let mut renderer = super::super::super::tests::headless_renderer();
        let mut frame = iced::runtime::UserInterface::build(
            app.channel_work
                .view
                .view(900.0, 1800.0)
                .map(Message::ChannelWork),
            iced::Size::new(900.0, 1800.0),
            Cache::new(),
            &mut renderer,
        );
        frame.operate(
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        let actions = events(
            &mut frame,
            &mut renderer,
            &[key(
                keyboard::Key::Character("x".into()),
                keyboard::key::Code::KeyX,
                Some("x"),
            )],
            iced::mouse::Cursor::Unavailable,
        );
        let [Message::ChannelWork(ui::Action::LocalPath(form, _))] = actions.as_slice() else {
            panic!("actual field callback: {actions:?}")
        };
        let form = *form;
        drop(frame);
        drop(app.update_channel_work(ui::Action::LocalPath(form, "/fixture/checkout".into())));
        let mut frame = iced::runtime::UserInterface::build(
            app.channel_work
                .view
                .view(900.0, 1800.0)
                .map(Message::ChannelWork),
            iced::Size::new(900.0, 1800.0),
            Cache::new(),
            &mut renderer,
        );
        frame.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
        let point = bounds(&frame, "Inspect checkout").center();
        let mut actions = click(&mut frame, &mut renderer, point);
        drop(frame);
        let Message::ChannelWork(action) = actions.remove(0) else {
            panic!()
        };
        let Some(Effect::PrepareLocal { ticket, home, .. }) = app.channel_work.view.update(action)
        else {
            panic!()
        };
        let selection = api::LocalSelection {
            selection: uuid::Uuid::new_v4(),
            home,
            expected_revision: 1,
            context: crate::channel_work::CheckoutContext {
                id: uuid::Uuid::new_v4(),
                registry: home.registry,
                repository: "/fixture".into(),
                path: "/fixture/checkout".into(),
                branch: Some("topic".into()),
                head: Some("a".repeat(40)),
                generation: 1,
                dirty: None,
            },
        };
        drop(app.update_channel_work(ui::Action::PreparedLocal {
            ticket,
            result: Ok(selection.clone()),
        }));
        (form, selection)
    }

    fn local_intent(
        app: &mut Cagents,
    ) -> (
        ui::FormOwner,
        ui::Ticket,
        crate::channel_work::Mutation,
        uuid::Uuid,
    ) {
        let (form, selection) = prepared_local(app);
        let mut renderer = super::super::super::tests::headless_renderer();
        let mut frame = iced::runtime::UserInterface::build(
            app.channel_work
                .view
                .view(900.0, 1800.0)
                .map(Message::ChannelWork),
            iced::Size::new(900.0, 1800.0),
            Cache::new(),
            &mut renderer,
        );
        frame.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            iced::mouse::Cursor::Unavailable,
        );
        let point = bounds(&frame, "Track selected local checkout").center();
        let mut actions = click(&mut frame, &mut renderer, point);
        drop(frame);
        let Message::ChannelWork(action) = actions.remove(0) else {
            panic!()
        };
        let effect = app.channel_work.view.update(action).unwrap();
        let Effect::Mutate {
            ticket,
            mutation,
            selection: Some(nonce),
        } = &effect
        else {
            panic!()
        };
        let result = (form, *ticket, mutation.clone(), *nonce);
        assert_eq!(*nonce, selection.selection);
        drop(app.channel_work_effect(effect));
        result
    }

    fn pr_intent(app: &mut Cagents) -> (ui::Ticket, crate::channel_work::Mutation) {
        let owner = app.channel_work.view.owner().unwrap();
        let Some(Effect::Focus { id, .. }) =
            app.channel_work.view.update(ui::Action::Attach(owner))
        else {
            panic!()
        };
        let mut renderer = super::super::super::tests::headless_renderer();
        let mut frame = iced::runtime::UserInterface::build(
            app.channel_work
                .view
                .view(900.0, 1800.0)
                .map(Message::ChannelWork),
            iced::Size::new(900.0, 1800.0),
            Cache::new(),
            &mut renderer,
        );
        frame.operate(
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        let actions = events(
            &mut frame,
            &mut renderer,
            &[key(
                keyboard::Key::Character("x".into()),
                keyboard::key::Code::KeyX,
                Some("x"),
            )],
            iced::mouse::Cursor::Unavailable,
        );
        let [Message::ChannelWork(ui::Action::Field(form, _, _))] = actions.as_slice() else {
            panic!("real PR field: {actions:?}")
        };
        let form = *form;
        drop(frame);
        drop(app.update_channel_work(ui::Action::Field(
            form,
            ui::Field::Repository,
            "https://github.com/fixture/work/pull/17".into(),
        )));
        let target = crate::channel_work::Target::PullRequest(
            crate::github_identity::PullRequestKey::from_url(
                "https://github.com/fixture/work/pull/17",
            )
            .unwrap(),
        );
        let effect = app
            .channel_work
            .view
            .update(ui::Action::Submit {
                owner: form,
                target,
                role: crate::channel_work::Role::Contribute,
                revision: 0,
            })
            .unwrap();
        let Effect::Mutate {
            ticket,
            mutation,
            selection: None,
        } = &effect
        else {
            panic!()
        };
        let result = (*ticket, mutation.clone());
        let task = app.channel_work_effect(effect);
        assert!(task.units() > 0);
        drop(task);
        result
    }
    #[test]
    fn selected_publication_failure_keeps_draft_but_ordinary_pr_does_not_enter_the_journal() {
        let (mut app, home) = main_fixture();
        let (form, ticket, mutation, nonce) = local_intent(&mut app);
        let private = tempfile::tempdir().unwrap();
        let journal_path = private.path().join("pending");
        let journal = journal::Journal::at(journal_path.clone()).unwrap();
        journal.load().unwrap();
        let held = crate::instance::owned_file_lock(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(journal_path.join(".journal-lock"))
                .unwrap(),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        let durable = Record::new(
            mutation.clone(),
            Some(nonce),
            record(home).destination().clone(),
            "Original".into(),
            1,
        )
        .unwrap();
        let mut sends = 0;
        let error = reserve_then_send(&journal, &durable, |_| {
            sends += 1;
            Ok(api::Response::Applied(reply(&durable)))
        })
        .unwrap_err();
        assert_eq!(error, journal::Error::Busy);
        assert_eq!(sends, 0);
        drop(app.complete_channel_work(failed_reservation(
            ticket,
            mutation.operation,
            None,
            false,
            error,
        )));
        assert!(app.channel_work.view.pending_homes().next().is_none());
        assert!(
            !app.channel_work
                .operation_routes
                .contains_key(&mutation.operation)
        );
        // The actual form remains editable after a proven prepublication failure.
        assert!(
            app.channel_work
                .view
                .update(ui::Action::LocalPath(form, "/fixture/other".into()))
                .is_none()
        );
        drop(held);
        assert!(journal.load().unwrap().records.is_empty());
        // Even unsafe/full discovery never redirects ordinary memory-only intent
        // into reserve_then_send or a durable-operation map.
        app.channel_work.saved_ready = false;
        let mut plain = mutation.clone();
        plain.operation = uuid::Uuid::new_v4();
        plain.change = crate::channel_work::Change::Claim {
            target: crate::channel_work::Target::PullRequest(
                crate::github_identity::PullRequestKey::from_url(
                    "https://github.com/fixture/work/pull/17",
                )
                .unwrap(),
            ),
            role: crate::channel_work::Role::Contribute,
        };
        let task = app.channel_work_effect(Effect::Mutate {
            ticket,
            mutation: plain.clone(),
            selection: None,
        });
        assert!(task.units() > 0);
        drop(task);
        assert!(!app.channel_work.dispatches.contains_key(&plain.operation));
        assert!(
            !app.channel_work
                .operation_routes
                .contains_key(&plain.operation)
        );
        assert!(journal.load().unwrap().records.is_empty());
    }

    #[test]
    fn interrupted_selected_request_cannot_recreate_its_record_or_open_a_blank_saved_surface() {
        let (mut app, home) = main_fixture();
        let (_, ticket, mutation, nonce) = local_intent(&mut app);
        let original = app.channel_work.operation_routes[&mutation.operation].clone();
        drop(app.complete_channel_work(Completion::Interrupted {
            ticket,
            operation: mutation.operation,
        }));
        assert_eq!(
            app.channel_work.operation_routes[&mutation.operation].socket,
            original.socket
        );
        assert!(!app.channel_work.saved.contains_key(&mutation.operation));
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update_channel_work(ui::Action::Close(owner)));
        drop(app.update_channel_work(ui::Action::ShowSaved));
        let generation = app.channel_work.desired_saved.unwrap();
        drop(app.update_channel_work(ui::Action::OpenSaved {
            generation,
            operation: mutation.operation,
        }));
        assert!(app.channel_work.view.saved_is_shown());
        assert!(app.channel_work_is_main());
        assert!(app.channel_work.view.owner().is_none());
        assert!(!app.channel_work.saved.contains_key(&mutation.operation));
        let durable = Record::new(
            mutation.clone(),
            Some(nonce),
            record(home).destination().clone(),
            "Original".into(),
            1,
        )
        .unwrap();
        // A verified Check cannot mint an absent immutable original for Retry.
        let mut checked = original;
        checked.recovery_verified = true;
        let completion = dispatch_saved(
            ticket,
            mutation.clone(),
            Some(nonce),
            None,
            Some(checked),
            "New title".into(),
            true,
        );
        assert!(matches!(
            completion,
            Completion::BeforeSendFailed {
                retained: true,
                record: None,
                ..
            }
        ));
        // Once the exact disk record arrives, it keeps the original nonce and
        // presentation can open its captured Home without creating membership.
        if let Some(actual) = app.channel_work.loading_saved {
            drop(app.complete_channel_work(Completion::Loaded {
                generation: actual,
                result: Ok(journal::Loaded {
                    records: vec![],
                    problems: vec![],
                    occupied: 0,
                    saturated: false,
                }),
            }));
        }
        hydrate(&mut app, vec![durable.clone()]);
        assert_eq!(app.channel_work.saved[&mutation.operation], durable);
        assert_eq!(
            app.channel_work.saved[&mutation.operation].selection(),
            Some(nonce)
        );
    }

    #[test]
    fn saturated_recovery_cohort_does_not_consume_resident_none_budget_and_global_saved_can_leave_without_home()
     {
        let (mut app, ordinary) = main_fixture();
        let records = (0..journal::MAX_RECORDS)
            .map(|_| {
                record(Home {
                    registry: ordinary.registry,
                    channel: ChannelId::new(),
                    creation_ordinal: 4,
                })
            })
            .collect::<Vec<_>>();
        hydrate(&mut app, records.clone());
        assert_eq!(app.channel_work.recovery_only.len(), journal::MAX_RECORDS);
        let (form, selected) = prepared_local(&mut app);
        assert!(
            app.channel_work
                .view
                .update(ui::Action::SubmitLocal {
                    owner: form,
                    selection: selected.selection,
                    target: selected.context.id,
                    role: crate::channel_work::Role::Contribute,
                    revision: selected.expected_revision
                })
                .is_none()
        );
        assert!(app.channel_work.dispatches.is_empty());
        assert!(
            app.channel_work
                .operation_routes
                .keys()
                .all(|id| app.channel_work.saved.contains_key(id))
        );
        let (mut app, _) = main_fixture();
        hydrate(&mut app, records);
        let (_, plain) = pr_intent(&mut app);
        assert!(!app.channel_work.saved.contains_key(&plain.operation));
        assert!(!app.channel_work.dispatches.contains_key(&plain.operation));
        assert_eq!(app.channel_work.recovery_only.len(), journal::MAX_RECORDS);
        // Reopen a fresh Saved-only state with no canonical channel or Home.
        let mut empty = super::super::super::tests::context_inspector_test_app(false);
        drop(empty.update_channel_work(ui::Action::ShowSaved));
        assert!(empty.channel_work.view.saved_is_shown());
        assert!(empty.channel_work.view.owner().is_none());
        let task = empty
            .intercept_channel_work_navigation(&Message::Sidebar(SidebarAction::OpenSettings))
            .unwrap();
        drop(task);
        assert!(!empty.channel_work.view.saved_is_shown());
        assert!(!empty.channel_work_is_main());
        assert!(empty.channel_work.deferred.is_none());
        assert!(
            empty
                .cards
                .values()
                .any(|runtime| matches!(runtime, CardRuntime::Settings))
        );
        assert!(empty.channel_home().is_none());
    }

    #[test]
    fn one_actual_disk_scan_survives_saved_churn_and_stale_completions_only_release_their_own_slot()
    {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (mut app, _) = main_fixture();
        let count = Arc::new(AtomicUsize::new(0));
        app.channel_work.saved_loader = Some({
            let count = count.clone();
            Arc::new(move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(journal::Loaded {
                    records: vec![],
                    problems: vec![],
                    occupied: 0,
                    saturated: false,
                })
            })
        });
        let first_task = app.load_saved_channel_work();
        let original = app.channel_work.loading_saved.unwrap();
        for _ in 0..64 {
            let effect = app.channel_work.view.update(ui::Action::ShowSaved).unwrap();
            let task = app.channel_work_effect(effect);
            assert_eq!(task.units(), 0);
            let close = app.channel_work.view.escape().unwrap();
            drop(app.channel_work_effect(close));
        }
        let final_effect = app.channel_work.view.update(ui::Action::ShowSaved).unwrap();
        assert_eq!(app.channel_work_effect(final_effect).units(), 0);
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "first lazy task has not been polled"
        );
        let wanted = app.channel_work.desired_saved.unwrap();
        assert_ne!(wanted, original);
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let read = |task| {
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            let action = executor.block_on(stream.next()).unwrap();
            let iced::runtime::Action::Output(cosmic::Action::App(Message::ChannelWorkCompleted(
                completion,
            ))) = action
            else {
                panic!("actual disk completion")
            };
            *completion
        };
        let completion = read(first_task);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let next = app.complete_channel_work(completion.clone());
        assert_eq!(app.channel_work.loading_saved, Some(wanted));
        assert_eq!(
            app.complete_channel_work(completion).units(),
            0,
            "duplicate cannot release the successor"
        );
        assert_eq!(app.channel_work.loading_saved, Some(wanted));
        let completion = read(next);
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "exactly one newest scan follows the stale actual read"
        );
        drop(app.complete_channel_work(completion));
        assert!(app.channel_work.loading_saved.is_none());
        assert!(app.channel_work.view.saved_discovery_ready());
        assert!(app.channel_work.saved.is_empty());
    }
    #[test]
    fn memory_only_unknown_is_visible_in_saved_list_without_a_dead_open_or_fabricated_record() {
        let (mut app, home) = main_fixture();
        let (ticket, original) = pr_intent(&mut app);
        drop(app.update_channel_work(ui::Action::Applied {
            ticket,
            result: Err(Failure::Unavailable),
        }));
        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update_channel_work(ui::Action::Close(owner)));
        assert!(app.channel_work.view.owner().is_none());
        drop(app.update_channel_work(ui::Action::ShowSaved));
        let generation = app.channel_work.desired_saved.unwrap();
        drop(app.update_channel_work(ui::Action::OpenSaved {
            generation,
            operation: original.operation,
        }));
        assert!(app.channel_work.view.saved_is_shown());
        assert!(app.channel_work.view.owner().is_none());
        assert_eq!(
            app.channel_work
                .view
                .original_mutation(home, original.operation),
            Some(&original)
        );
        assert!(!app.channel_work.saved.contains_key(&original.operation));
        assert!(
            !app.channel_work
                .operation_routes
                .contains_key(&original.operation)
        );
        assert!(app.channel_work_is_main());
    }

    #[test]
    fn recordless_positive_receipt_releases_only_after_locked_absence_and_exact_original_validation()
     {
        let (mut app, home) = main_fixture();
        let (_, ticket, original, _) = local_intent(&mut app);
        let captured = app.channel_work.operation_routes[&original.operation].clone();
        drop(app.complete_channel_work(Completion::Interrupted {
            ticket,
            operation: original.operation,
        }));
        let owner = app.channel_work.view.owner().unwrap();
        let private = tempfile::tempdir().unwrap();
        let storage = journal::Journal::at(private.path().join("pending")).unwrap();
        assert!(storage.load().unwrap().records.is_empty());
        drop(app.update_channel_work(ui::Action::Check(owner, original.operation)));
        let check = app.channel_work.lookups[&original.operation];
        let positive = crate::channel_work::Receipt {
            home,
            operation: original.operation,
            revision: original.expected_revision + 1,
            sequence: 1,
            changed: true,
        };
        let mut wrong = positive.clone();
        wrong.revision += 1;
        assert!(storage.settle_absent(&original, &wrong).is_err());
        drop(app.complete_channel_work(Completion::LookedUp {
            ticket: check,
            home,
            operation: original.operation,
            route: Some(captured.clone()),
            result: Ok(Some(wrong)),
            settled: true,
        }));
        assert!(
            app.channel_work
                .view
                .original_mutation(home, original.operation)
                .is_some()
        );
        drop(app.update_channel_work(ui::Action::Check(owner, original.operation)));
        let check = app.channel_work.lookups[&original.operation];
        storage.settle_absent(&original, &positive).unwrap();
        drop(app.complete_channel_work(Completion::LookedUp {
            ticket: check,
            home,
            operation: original.operation,
            route: Some(captured),
            result: Ok(Some(positive)),
            settled: true,
        }));
        assert!(
            app.channel_work
                .view
                .original_mutation(home, original.operation)
                .is_none()
        );
        assert!(
            !app.channel_work
                .operation_routes
                .contains_key(&original.operation)
        );
        assert!(app.channel_work.saved.is_empty());
        assert!(storage.load().unwrap().records.is_empty());
    }
}
