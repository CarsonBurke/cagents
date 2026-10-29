//! Exact live-source routing; no recipe/account DTO can select a replacement.
use super::state::SharedHost;
use crate::{
    account::reset::{
        controller,
        request::{Report, Request},
    },
    agent::Provider,
};
use std::time::{Duration, Instant};

pub(super) fn execute(host: &SharedHost, request: Request) -> Result<Report, String> {
    execute_with_storage(host, request, || {
        crate::instance::project_dirs().map(|dirs| dirs.data_dir().join("account-reset-attempts"))
    })
}

fn execute_with_storage(
    host: &SharedHost,
    request: Request,
    storage: impl FnOnce() -> Option<std::path::PathBuf>,
) -> Result<Report, String> {
    let expires = Instant::now() + Duration::from_secs(45);
    request.validate()?;
    let session = request.session;
    let (handle, instance) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err("Daemon is shutting down; no reset operation was admitted".into());
        }
        let record = state.chats.get(&session).ok_or("Target is not an existing live native conversation. No replacement process or usage command was started")?;
        if record.session.launch.provider != Provider::Codex {
            return Err(match record.session.launch.provider {
                Provider::Claude => "Claude's documented saved resets are redeemed manually on web/Desktop; this live terminal/IDE transport has no supported programmatic redemption path",
                Provider::Omp => "Omp has upstream saved-reset commands, but this live transport exposes no authenticated credential-ID reset path. No potentially auto-redeeming usage command was run",
                Provider::Codex => unreachable!(),
            }.into());
        }
        if record.returning_terminal || record.handle.is_completed() {
            return Err("The exact target actor is completed or changing surface; no restart or reset was attempted".into());
        }
        (record.handle.clone(), record.execution_instance)
    };
    let current = || {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || !state.chats.get(&session).is_some_and(|record| {
                record.execution_instance == instance
                    && record.handle.same_actor(&handle)
                    && !record.returning_terminal
                    && !record.handle.is_completed()
            })
        {
            return Err("The exact target owner changed; preserve and reconcile any existing reset attempt instead of selecting a replacement".into());
        }
        Ok(())
    };
    let root = storage();
    controller::execute(&handle, request, root.as_deref(), expires, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::reset::{ProviderAccount, request::Action},
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
    };
    use std::sync::{Arc, Mutex};
    #[test]
    fn missing_target_unauthorized_reset_and_shutdown_never_create_a_replacement_source() {
        let host = Arc::new(super::super::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("private-allowance-test"),
                boot_id: BootId::new("private-boot"),
                host_incarnation: HostIncarnation::new("private-incarnation"),
            }),
        ));
        let session = TerminalId::new();
        let error = execute(
            &host,
            Request {
                session,
                action: Action::Inspect,
            },
        )
        .unwrap_err();
        assert!(error.contains("not an existing live native conversation"));
        let error = execute(
            &host,
            Request {
                session,
                action: Action::Redeem {
                    expected: ProviderAccount {
                        account_id: "private-account".into(),
                        backend_origin: "https://chatgpt.com".into(),
                    },
                    credit_id: "private-credit".into(),
                    authorized: false,
                    condition: None,
                },
            },
        )
        .unwrap_err();
        assert!(error.contains("explicit --authorize"));
        host.lock().unwrap().shutting_down = true;
        let error = execute(
            &host,
            Request {
                session,
                action: Action::Inspect,
            },
        )
        .unwrap_err();
        assert!(error.contains("shutting down"));
        let state = host.lock().unwrap();
        assert!(
            state.chats.is_empty() && state.chat_spawning.is_empty() && state.terminals.is_empty()
        );
        assert!(state.chat_handoffs.is_empty());
    }

    #[test]
    fn bound_agent_device_requests_refuse_before_allowance_target_lookup() {
        use crate::{
            agent_execution::Lineage,
            device_service::Operation,
            terminal_ipc::{ClientRequest, ProtocolErrorCode},
        };
        let host = Arc::new(super::super::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("private-allowance-caller-policy"),
                boot_id: BootId::new("private-boot"),
                host_incarnation: HostIncarnation::new("private-incarnation"),
            }),
        ));
        const USER: u64 = 1;
        const AGENT: u64 = 2;
        {
            let mut state = host.lock().unwrap();
            state.register_client(USER);
            state.register_client(AGENT);
            // Explicit unit-test precondition: this is the bound lineage read
            // by actual admission, not proof of token mint/authentication.
            // The separate framed CLI fixture covers Authenticate ordering.
            state
                .agent_clients
                .insert(AGENT, Lineage(TerminalId::new()));
        }
        for action in [
            Action::Inspect,
            Action::Redeem {
                expected: account(),
                credit_id: "private-credit".into(),
                authorized: true,
                condition: None,
            },
        ] {
            let request = ClientRequest::DeviceOperation {
                operation: Operation::AccountAllowance {
                    request: Request {
                        session: TerminalId::new(),
                        action,
                    },
                },
            };
            let denied =
                super::super::client::process_request(&host, AGENT, request.clone()).unwrap_err();
            assert_eq!(denied.code, ProtocolErrorCode::PermissionDenied);
            let target = super::super::client::process_request(&host, USER, request).unwrap_err();
            assert_eq!(target.code, ProtocolErrorCode::InvalidRequest);
            assert!(
                target
                    .message
                    .contains("not an existing live native conversation")
            );
        }
        assert!(host.lock().unwrap().chats.is_empty());
    }

    #[test]
    fn conditional_redemption_refuses_before_source_lookup_or_durable_storage() {
        let host = Arc::new(super::super::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("private-automatic-refusal"),
                boot_id: BootId::new("private-boot"),
                host_incarnation: HostIncarnation::new("private-incarnation"),
            }),
        ));
        let mut request = redeem(TerminalId::new());
        let Action::Redeem { condition, .. } = &mut request.action else {
            panic!()
        };
        *condition = Some(crate::account::reset::request::Threshold {
            used_percent: 90,
            max_age_ms: 30_000,
        });
        let error = execute_with_storage(&host, request, || {
            panic!("unsupported automatic admission cannot open or mutate storage")
        })
        .unwrap_err();
        assert_eq!(
            error,
            crate::account::reset::request::AUTOMATIC_ADMISSION_UNAVAILABLE
        );
        assert!(host.lock().unwrap().chats.is_empty());
    }

    /// The real actor owns/reaps /bin/cat through the production runtime. Only
    /// allowance responses are modeled; no provider binary or credit is used.
    struct Model {
        allowance: crate::account::reset::Allowance,
        keys: std::collections::BTreeSet<String>,
        consumes: usize,
        prompts: usize,
        before_next_read: Option<Box<dyn FnOnce() + Send>>,
        intent_root: Option<std::path::PathBuf>,
    }
    struct Adapter {
        model: Arc<Mutex<Model>>,
        reservation: Option<crate::account::reset::transport::Reservation>,
    }
    impl crate::chat::protocol::Adapter for Adapter {
        fn initialize(&mut self) -> Result<crate::chat::protocol::Step, String> {
            Ok(Default::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<crate::chat::protocol::Step, String> {
            Err("Private fixture must not receive provider frames".into())
        }
        fn command(
            &mut self,
            command: crate::chat::Command,
        ) -> Result<crate::chat::protocol::Step, String> {
            if matches!(command, crate::chat::Command::Interrupt) {
                return Ok(crate::chat::protocol::Step {
                    writes: vec![],
                    events: vec![crate::chat::Event::Status {
                        busy: false,
                        detail: "private fixture cleanup".into(),
                    }],
                });
            }
            self.model.lock().unwrap().prompts += 1;
            Err("No ordinary provider input is permitted in this fixture".into())
        }
        fn read_allowance(
            &mut self,
            request: crate::account::reset::transport::Read,
        ) -> Result<crate::chat::protocol::Step, String> {
            request.check()?;
            let callback = self.model.lock().unwrap().before_next_read.take();
            if let Some(callback) = callback {
                callback();
            }
            let mut model = self.model.lock().unwrap();
            model.allowance.observed_at_ms = crate::chat::now_ms().unwrap();
            let _ = request.reply.send(Ok(model.allowance.clone()));
            Ok(Default::default())
        }
        fn reserve_reset_credit(
            &mut self,
            request: crate::account::reset::transport::Reservation,
        ) -> Result<(), String> {
            request.check()?;
            if self.reservation.as_ref().is_some_and(|reserved| {
                reserved.check().is_ok()
                    && (reserved.token != request.token || reserved.attempt != request.attempt)
            }) {
                return Err(
                    "Private fixture actor already has a different reset reservation".into(),
                );
            }
            self.reservation = Some(request);
            Ok(())
        }
        fn release_reset_credit(&mut self, token: uuid::Uuid) {
            if self
                .reservation
                .as_ref()
                .is_some_and(|reserved| reserved.token == token)
            {
                self.reservation = None;
            }
        }
        fn consume_reset_credit(
            &mut self,
            request: crate::account::reset::transport::Consume,
        ) -> Result<crate::chat::protocol::Step, String> {
            request.check()?;
            let reserved = self
                .reservation
                .as_ref()
                .filter(|reserved| {
                    reserved.token == request.reservation && reserved.attempt == request.attempt()
                })
                .ok_or("Private fixture consume requires its exact actor reservation")?;
            reserved.check()?;
            let mut model = self.model.lock().unwrap();
            assert_eq!(model.allowance.account.as_ref(), Some(&request.expected));
            assert_eq!(request.credit_id, "private-credit");
            let root = model
                .intent_root
                .as_ref()
                .expect("Private fixture must pin durable intent storage");
            let receipts: Vec<_> = std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "json")
                })
                .collect();
            assert_eq!(receipts.len(), 1);
            let receipt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&receipts[0]).unwrap()).unwrap();
            assert_eq!(
                receipt["account_id"].as_str(),
                Some(request.expected.account_id.as_str())
            );
            assert_eq!(
                receipt["backend_origin"].as_str(),
                Some(request.expected.backend_origin.as_str())
            );
            assert_eq!(
                receipt["attempts"][0]["credit_id"].as_str(),
                Some(request.credit_id.as_str())
            );
            assert_eq!(
                receipt["attempts"][0]["idempotency_key"].as_str(),
                Some(request.key.as_str())
            );
            assert!(
                receipt["attempts"][0]["outcome"].is_null(),
                "Intent must precede the modeled effect"
            );
            let first = model.keys.insert(request.key.clone());
            model.consumes += usize::from(first);
            model.allowance.observed_at_ms = crate::chat::now_ms().unwrap();
            model.allowance.windows[0].used_percent = 0;
            let inventory = model.allowance.reset_credits.as_mut().unwrap();
            inventory.available_count = 0;
            inventory.details.as_mut().unwrap()[0].available = false;
            let _ = request
                .reply
                .send(Ok(crate::account::reset::transport::Consumed {
                    outcome: if first {
                        crate::account::reset::Outcome::Reset
                    } else {
                        crate::account::reset::Outcome::AlreadyRedeemed
                    },
                    allowance: model.allowance.clone(),
                }));
            Ok(Default::default())
        }
    }
    fn model() -> Arc<Mutex<Model>> {
        use crate::account::reset::{Allowance, Period, ResetCredit, ResetCredits, Window};
        Arc::new(Mutex::new(Model {
            allowance: Allowance {
                observed_at_ms: crate::chat::now_ms().unwrap(),
                account: Some(account()),
                account_binding_detail: None,
                windows: vec![Window {
                    bucket: "shared-private-plan".into(),
                    period: Period::Primary,
                    used_percent: 95,
                    resets_at_ms: None,
                }],
                reset_credits: Some(ResetCredits {
                    available_count: 1,
                    details: Some(vec![ResetCredit {
                        id: "private-credit".into(),
                        supported_type: true,
                        available: true,
                        expires_at_ms: None,
                    }]),
                }),
            },
            keys: Default::default(),
            consumes: 0,
            prompts: 0,
            before_next_read: None,
            intent_root: None,
        }))
    }
    fn account() -> ProviderAccount {
        ProviderAccount {
            account_id: "private-account".into(),
            backend_origin: "https://chatgpt.com".into(),
        }
    }
    fn redeem(session: TerminalId) -> Request {
        Request {
            session,
            action: Action::Redeem {
                expected: account(),
                credit_id: "private-credit".into(),
                authorized: true,
                condition: None,
            },
        }
    }
    struct Fixture {
        root: std::path::PathBuf,
        host: SharedHost,
        handles: Vec<crate::chat::runtime::Handle>,
    }
    impl Fixture {
        fn new() -> Self {
            // Keep on success AND panic. No directory deletion is inferred
            // from a process timeout; outer strict family settlement remains required.
            let root = tempfile::tempdir().unwrap().keep();
            crate::secure_file::prepare_private_directory(&root).unwrap();
            Self {
                root,
                host: Arc::new(super::super::state::HostLock::new(
                    super::super::state::HostState::new(HostSessionProof {
                        host_session_id: HostSessionId::new("private-allowance-owner"),
                        boot_id: BootId::new("private-boot"),
                        host_incarnation: HostIncarnation::new("private-incarnation"),
                    }),
                )),
                handles: vec![],
            }
        }
        fn start(
            &mut self,
            model: Arc<Mutex<Model>>,
        ) -> (TerminalId, crate::chat::runtime::Handle) {
            use crate::chat::{
                Launch, Session,
                protocol::{ProcessSpec, Step, TransportSpec},
                runtime,
            };
            let session = Session {
                id: TerminalId::new(),
                launch: Launch {
                    provider: Provider::Codex,
                    recipe: crate::model::LaunchRecipe {
                        program: "/bin/cat".into(),
                        arguments: vec![],
                        working_directory: self.root.clone(),
                        environment: [("HOME".into(), self.root.to_str().unwrap().into())].into(),
                        unset_environment: Default::default(),
                    },
                    session_id: None,
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            };
            let spec = ProcessSpec {
                transport: TransportSpec::JsonLines,
                ..ProcessSpec::from(&session.launch.recipe)
            };
            let mut initial = Step {
                writes: vec![],
                events: vec![
                    crate::chat::Event::Ready(true),
                    crate::chat::Event::Status {
                        busy: true,
                        detail: "private modeled running work".into(),
                    },
                ],
            };
            let store = runtime::preflight(&spec, &mut initial).unwrap();
            let handle = runtime::Handle::start(
                spec,
                Box::new(Adapter {
                    model,
                    reservation: None,
                }),
                initial,
                store,
                None,
                |_| {},
            )
            .unwrap();
            self.handles.push(handle.clone());
            // Query completion is an actor-loop barrier after initial events,
            // not evidence that a real provider supports allowance operations.
            handle
                .read_allowance(Instant::now() + Duration::from_secs(2))
                .unwrap();
            self.host.lock().unwrap().chats.insert(
                session.id,
                super::super::chat::Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: session.clone(),
                    handle: handle.clone(),
                    source_process: None,
                    subscribers: Default::default(),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            (session.id, handle)
        }
        fn finish(&mut self) {
            self.host.lock().unwrap().chats.clear();
            for handle in &self.handles {
                handle.close().unwrap();
                assert!(handle.is_completed());
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .chats
                .clear();
            for handle in &self.handles {
                if let Err(error) = handle.close() {
                    eprintln!(
                        "Private actor cleanup uncertain; retained {}: {error}",
                        self.root.display()
                    );
                }
            }
        }
    }
    #[test]
    fn actual_host_actors_share_one_durable_credit_without_prompt_or_source_replacement() {
        let mut fixture = Fixture::new();
        let model = model();
        let (first, first_handle) = fixture.start(model.clone());
        let (second, second_handle) = fixture.start(model.clone());
        let original: Vec<_> = fixture
            .host
            .lock()
            .unwrap()
            .chats
            .values()
            .map(|record| record.session.clone())
            .collect();
        let root = fixture.root.join("attempts");
        model.lock().unwrap().intent_root = Some(root.clone());
        let result =
            execute_with_storage(&fixture.host, redeem(first), || Some(root.clone())).unwrap();
        assert_eq!(result.outcome, Some(crate::account::reset::Outcome::Reset));
        assert_eq!(result.allowance.windows[0].used_percent, 0);
        assert_eq!(result.allowance.reset_credits.unwrap().available_count, 0);
        let result = execute_with_storage(&fixture.host, redeem(second), || Some(root)).unwrap();
        assert!(result.reconciled);
        assert_eq!(result.outcome, Some(crate::account::reset::Outcome::Reset));
        let state = model.lock().unwrap();
        assert_eq!(state.consumes, 1);
        assert_eq!(state.keys.len(), 1);
        assert_eq!(state.prompts, 0);
        drop(state);
        assert!(!first_handle.is_completed() && !second_handle.is_completed());
        assert!(
            first_handle.snapshot(None).unwrap().busy && second_handle.snapshot(None).unwrap().busy
        );
        assert_eq!(
            fixture
                .host
                .lock()
                .unwrap()
                .chats
                .values()
                .map(|record| record.session.clone())
                .collect::<Vec<_>>(),
            original
        );
        fixture.finish();
    }
    #[test]
    fn actual_host_refuses_same_handle_instance_change_and_replacement_handle_before_effect() {
        for replace_handle in [false, true] {
            let mut fixture = Fixture::new();
            let model = model();
            let (session, _) = fixture.start(model.clone());
            let (_, other) = fixture.start(model.clone());
            // Install the callback on the next read via an actor-owned adapter
            // flag rather than mutating runtime/Host state under a HostLock.
            let weak = Arc::downgrade(&fixture.host);
            let callback: Box<dyn FnOnce() + Send> = Box::new(move || {
                let host = weak.upgrade().unwrap();
                let mut host = host.lock().unwrap();
                let record = host.chats.get_mut(&session).unwrap();
                if replace_handle {
                    record.handle = other;
                } else {
                    record.execution_instance = uuid::Uuid::new_v4();
                }
            });
            // The adapter's one-shot hook is separate from shared provider
            // observations and executes at the actual first read boundary.
            model.lock().unwrap().before_next_read = Some(callback);
            let root = fixture.root.join("attempts");
            let error = execute_with_storage(&fixture.host, redeem(session), || Some(root.clone()))
                .unwrap_err();
            assert!(error.contains("exact target owner changed"), "{error}");
            assert!(!root.exists());
            let state = model.lock().unwrap();
            assert_eq!(state.consumes, 0);
            assert_eq!(state.prompts, 0);
            drop(state);
            fixture.finish();
        }
    }
}
