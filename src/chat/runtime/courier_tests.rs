//! Real local actor admission with a deterministic adapter barrier. This
//! exercises command dequeue, event reduction and actual `/bin/cat` pipe I/O;
//! it does not claim native Codex or Claude protocol qualification.
use super::*;
use crossbeam_channel::{Receiver, Sender};
use serde_json::{Value, json};

const REQUEST: &str = "courier-race-request";
const TIMEOUT: Duration = Duration::from_secs(5);
const NATIVE_PARENT: &str = "fixture native parent";
const BACKGROUND_CHILD: &str = "fixture-running-background-child";

#[derive(Default)]
struct Observed {
    prompts: Mutex<Vec<String>>,
    echoed: Mutex<Vec<String>>,
    runs: Mutex<Vec<crate::model::RunId>>,
}

struct GatedAdapter {
    entered: Sender<()>,
    release: Receiver<()>,
    observed: Arc<Observed>,
    managed_lifecycle: bool,
}

impl Adapter for GatedAdapter {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step {
            events: vec![Event::Ready(true)],
            ..Step::default()
        })
    }

    fn command(&mut self, command: Command) -> Result<Step, String> {
        let events = match command {
            Command::Prompt {
                text,
                attachments,
                mode: super::super::PromptMode::Send,
            } if self.managed_lifecycle && text == NATIVE_PARENT && attachments.is_empty() => {
                vec![
                    Event::TurnStarted,
                    Event::Status {
                        busy: true,
                        detail: String::new(),
                    },
                    Event::Subagent {
                        id: BACKGROUND_CHILD.into(),
                        kind: "fixture".into(),
                        description: "Still running after its native parent settles".into(),
                        status: Some(ToolStatus::Running),
                        background: Some(true),
                        model: None,
                        waiting: Some(false),
                        detail: None,
                    },
                ]
            }
            Command::Compact => {
                self.entered.send(()).map_err(|error| error.to_string())?;
                self.release
                    .recv_timeout(TIMEOUT)
                    .map_err(|error| format!("request barrier was not released: {error}"))?;
                vec![Event::Request(super::super::Request {
                    kind: super::super::RequestKind::Question,
                    id: REQUEST.into(),
                    title: "An explicit answer is required".into(),
                    description: String::new(),
                    fields: Vec::new(),
                    actions: ["resolve", "cancel"]
                        .into_iter()
                        .map(|action| super::super::Choice {
                            id: action.into(),
                            label: action.into(),
                            description: String::new(),
                        })
                        .collect(),
                })]
            }
            Command::Answer {
                request_id,
                action,
                answers,
            } if request_id == REQUEST
                && matches!(action.as_str(), "resolve" | "cancel")
                && answers.is_empty() =>
            {
                vec![Event::RequestResolved { id: request_id }]
            }
            Command::Interrupt => vec![
                Event::Status {
                    busy: false,
                    detail: String::new(),
                },
                Event::Waiting(false),
            ],
            Command::SetModel { model, .. } => match model.as_str() {
                "settle native parent" if self.managed_lifecycle => vec![
                    Event::Status {
                        busy: false,
                        detail: String::new(),
                    },
                    Event::TurnSettled,
                    Event::TurnFinished {
                        key: None,
                        outcome: crate::notifications::Kind::Succeeded,
                    },
                ],
                "busy" => vec![Event::Status {
                    busy: true,
                    detail: String::new(),
                }],
                "unready" => vec![Event::Ready(false)],
                "waiting" => vec![Event::Waiting(true)],
                _ => return Err("unknown fixture state".into()),
            },
            Command::Prompt {
                text,
                attachments,
                mode: super::super::PromptMode::Send,
            } if attachments.is_empty() => {
                // Count entry before returning any write. A rejected courier
                // cannot pass this assertion merely because I/O is delayed.
                self.observed.prompts.lock().unwrap().push(text.clone());
                return Ok(Step {
                    writes: vec![json!({"courier_fixture_prompt": text})],
                    ..Step::default()
                });
            }
            _ => return Err("unsupported fixture command".into()),
        };
        Ok(Step {
            events,
            ..Step::default()
        })
    }

    fn command_for_run(
        &mut self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<Step, String> {
        self.observed.runs.lock().unwrap().push(run);
        if matches!(&command, Command::Prompt { text, .. } if text == "fixture uncertain Step") {
            // The first frame can already be queued when a later frame fails
            // the real size bound. The caller cannot infer no provider effects.
            return Ok(Step {
                writes: vec![
                    json!({"courier_fixture_prompt": "uncertain"}),
                    Value::String("x".repeat(FRAME_LIMIT)),
                ],
                ..Step::default()
            });
        }
        if self.managed_lifecycle
            && let Command::Prompt {
                text,
                attachments,
                mode: super::super::PromptMode::Send,
            } = &command
            && attachments.is_empty()
        {
            self.observed.prompts.lock().unwrap().push(text.clone());
            return Ok(Step {
                writes: vec![json!({
                    "courier_fixture_prompt": text,
                    "managed_fixture_run": run,
                })],
                events: vec![
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Started {
                            key: format!("managed-fixture-{run}"),
                        },
                    },
                    Event::TurnStarted,
                    Event::Status {
                        busy: true,
                        detail: String::new(),
                    },
                ],
                ..Step::default()
            });
        }
        self.command(command)
    }

    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let text = frame["courier_fixture_prompt"]
            .as_str()
            .ok_or("unexpected fixture pipe frame")?;
        self.observed.echoed.lock().unwrap().push(text.into());
        if self.managed_lifecycle && !frame["managed_fixture_run"].is_null() {
            let run: crate::model::RunId =
                serde_json::from_value(frame["managed_fixture_run"].clone())
                    .map_err(|error| error.to_string())?;
            return Ok(Step {
                events: vec![
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Scope,
                    },
                    Event::Item(super::super::Item {
                        id: format!("managed-fixture-{run}"),
                        role: super::super::Role::Assistant,
                        started_at: None,
                        at: None,
                        content: ItemContent::Text {
                            text: format!("Managed result: {text}"),
                            thinking: false,
                            phase: Phase::Final,
                        },
                    }),
                    Event::Status {
                        busy: false,
                        detail: String::new(),
                    },
                    Event::TurnSettled,
                    Event::TurnFinished {
                        key: Some(format!("managed-fixture-{run}")),
                        outcome: crate::notifications::Kind::Succeeded,
                    },
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Settled,
                    },
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Finished {
                            outcome: crate::notifications::Kind::Succeeded,
                        },
                    },
                ],
                ..Step::default()
            });
        }
        Ok(Step::default())
    }
}

/// Reusable by the HOST mailbox fixture. The fixture owns only its own
/// Compact/idle-command workers; callers must retain their mailbox worker.
pub(crate) struct RequestRaceFixture {
    pub(crate) handle: Handle,
    entered: Receiver<()>,
    release: Sender<()>,
    request: Option<JoinHandle<Result<(), String>>>,
    attempts: Vec<JoinHandle<()>>,
    observed: Arc<Observed>,
    recipe: crate::model::LaunchRecipe,
    _directory: tempfile::TempDir,
}

impl RequestRaceFixture {
    pub(crate) fn new() -> Self {
        Self::build(|_| {}, false)
    }

    /// HOST-only construction mode: real actor events drive the supplied
    /// observer. The ordinary courier constructor keeps its old semantics.
    pub(crate) fn with_execution_notify(notify: impl Fn(Update) + Send + Sync + 'static) -> Self {
        Self::build(notify, true)
    }

    fn build(notify: impl Fn(Update) + Send + Sync + 'static, managed_lifecycle: bool) -> Self {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let home = directory.path().join("home");
        let cache = directory.path().join("cache");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&cache).unwrap();
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::from([
                ("HOME".into(), home.to_string_lossy().into_owned()),
                (
                    "XDG_CACHE_HOME".into(),
                    cache.to_string_lossy().into_owned(),
                ),
            ]),
        };
        let recipe = crate::model::LaunchRecipe {
            program: spec.program.clone(),
            arguments: spec.arguments.clone(),
            working_directory: spec.working_directory.clone(),
            environment: spec.environment.clone(),
            unset_environment: spec.unset_environment.clone(),
        };
        let (entered_tx, entered) = crossbeam_channel::bounded(1);
        let (release, release_rx) = crossbeam_channel::bounded(1);
        let observed = Arc::new(Observed::default());
        let mut adapter = GatedAdapter {
            entered: entered_tx,
            release: release_rx,
            observed: observed.clone(),
            managed_lifecycle,
        };
        let mut initial = adapter.initialize().unwrap();
        // Follow real preflight. Its cfg(test) Store owns a unique private
        // temporary cache; ProcessSpec does not override that cache's root.
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(adapter), initial, store, None, notify).unwrap();
        let fixture = Self {
            handle,
            entered,
            release,
            request: None,
            attempts: Vec::new(),
            observed,
            recipe,
            _directory: directory,
        };
        assert_eq!(
            fixture.handle.check_update_readiness().unwrap(),
            UpdateReadiness::Idle,
            "the real actor must reduce its initial Ready event"
        );
        assert!(fixture.handle.is_idle());
        fixture
    }

    pub(crate) fn recipe(&self) -> &crate::model::LaunchRecipe {
        &self.recipe
    }

    pub(crate) fn start_native_parent(&self) {
        self.handle
            .command(Command::Prompt {
                text: NATIVE_PARENT.into(),
                attachments: Vec::new(),
                mode: super::super::PromptMode::Send,
            })
            .unwrap();
    }

    pub(crate) fn settle_native_parent(&self) {
        self.handle
            .command(Command::SetModel {
                model: "settle native parent".into(),
                reasoning: None,
            })
            .unwrap();
    }

    /// Release fixture barriers before HOST teardown, including unwind. The
    /// existing Drop remains the owner of actor close and exact worker joins.
    pub(crate) fn release_host_barriers(&mut self) -> Result<(), String> {
        let _ = self.release.try_send(());
        if let Some(request) = self.request.take() {
            request
                .join()
                .map_err(|_| "fixture Compact worker panicked".to_owned())??;
        }
        if self.handle.is_completed() {
            return Ok(());
        }
        self.handle.command(Command::SetModel {
            model: "settle native parent".into(),
            reasoning: None,
        })?;
        if self
            .handle
            .requests()
            .iter()
            .any(|request| request.id == REQUEST)
        {
            self.handle.command(Command::Answer {
                request_id: REQUEST.into(),
                action: "resolve".into(),
                answers: Vec::new(),
            })?;
        }
        Ok(())
    }

    /// Return only once Compact is dequeued and blocked inside the adapter.
    /// The Request event is emitted by its Step after release, not by a test
    /// mutating a resident snapshot.
    pub(crate) fn begin_request(&mut self) {
        assert!(self.request.is_none());
        let handle = self.handle.clone();
        self.request = Some(thread::spawn(move || handle.command(Command::Compact)));
        self.entered.recv_timeout(TIMEOUT).unwrap();
        assert!(self.handle.is_idle());
        assert!(self.handle.requests().is_empty());
    }

    /// Under the held Compact gate, the actor cannot consume another command.
    /// This observes the actual queue after the courier caller submitted work.
    pub(crate) fn wait_for_queued_prompt(&self) {
        assert!(self.request.is_some(), "request barrier must be held");
        let deadline = Instant::now() + TIMEOUT;
        while self.handle.local_command_queue_is_empty() {
            assert!(Instant::now() < deadline, "courier command was not queued");
            thread::yield_now();
        }
    }

    pub(crate) fn release_request(&mut self) {
        self.release.try_send(()).unwrap();
        self.request
            .take()
            .expect("request barrier must be held")
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(self.handle.requests().len(), 1);
        assert!(!self.handle.is_idle());
    }

    pub(crate) fn clear_request(&self) {
        self.resolve_request("resolve");
    }

    fn resolve_request(&self, action: &str) {
        self.handle
            .command(Command::Answer {
                request_id: REQUEST.into(),
                action: action.into(),
                answers: Vec::new(),
            })
            .unwrap();
        assert!(self.handle.requests().is_empty());
    }

    pub(crate) fn prompt_count(&self) -> usize {
        self.observed.prompts.lock().unwrap().len()
    }

    pub(crate) fn prompt_texts(&self) -> Vec<String> {
        self.observed.prompts.lock().unwrap().clone()
    }

    pub(crate) fn provider_write_count(&self) -> usize {
        self.observed.echoed.lock().unwrap().len()
    }
    pub(crate) fn correlated_runs(&self) -> Vec<crate::model::RunId> {
        self.observed.runs.lock().unwrap().clone()
    }

    pub(crate) fn close(&mut self) {
        assert!(self.request.is_none(), "release the request barrier first");
        self.handle.close().unwrap();
        for attempt in self.attempts.drain(..) {
            attempt.join().unwrap();
        }
        assert!(self.handle.is_completed());
    }

    fn wait_for_provider_writes(&self, expected: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while self.provider_write_count() < expected {
            assert!(
                Instant::now() < deadline,
                "provider did not echo the prompt"
            );
            thread::yield_now();
        }
        assert_eq!(self.provider_write_count(), expected);
    }

    fn queue_idle_prompt(&mut self, text: &str) -> Receiver<Result<(), Rejection>> {
        let handle = self.handle.clone();
        let text = text.to_owned();
        let (reply, result) = crossbeam_channel::bounded(1);
        self.attempts.push(thread::spawn(move || {
            let _ = reply.send(handle.try_idle_prompt(text));
        }));
        result
    }

    fn queue_managed_prompt(
        &mut self,
        run: crate::model::RunId,
        text: &str,
    ) -> Receiver<Result<(), Rejection>> {
        let handle = self.handle.clone();
        let text = text.to_owned();
        let (reply, result) = crossbeam_channel::bounded(1);
        self.attempts.push(thread::spawn(move || {
            let _ = reply.send(handle.try_idle_prompt_for_run(run, text));
        }));
        result
    }
}

impl Drop for RequestRaceFixture {
    fn drop(&mut self) {
        // Release before either joining or closing, including assertion panic.
        let _ = self.release.try_send(());
        if let Some(request) = self.request.take() {
            let _ = request.join();
        }
        let _ = self.handle.close();
        for attempt in self.attempts.drain(..) {
            let _ = attempt.join();
        }
    }
}

fn refused(result: Result<(), Rejection>) {
    assert!(
        matches!(result, Err(Rejection::Refused(_))),
        "a safe refusal is required, got {result:?}"
    );
}

#[test]
fn courier_prompt_rechecks_request_published_after_idle_observation() {
    let mut fixture = RequestRaceFixture::new();
    fixture.begin_request();
    assert!(fixture.handle.is_idle(), "the initial observation is stale");
    let delivery = fixture.queue_idle_prompt("courier delivery");
    fixture.wait_for_queued_prompt();
    fixture.release_request();
    refused(delivery.recv_timeout(TIMEOUT).unwrap());
    assert_eq!(fixture.prompt_count(), 0);
    assert!(fixture.prompt_texts().is_empty());
    assert_eq!(fixture.provider_write_count(), 0);
    assert_eq!(fixture.handle.requests()[0].id, REQUEST);
    fixture.close();
}

#[test]
fn managed_prompt_rechecks_actor_contention_then_admits_the_same_run_once() {
    let mut fixture = RequestRaceFixture::new();
    let run = crate::model::RunId::new();
    fixture.begin_request();
    let delivery = fixture.queue_managed_prompt(run, "managed delivery");
    fixture.wait_for_queued_prompt();
    fixture.release_request();
    assert!(matches!(
        delivery.recv_timeout(TIMEOUT).unwrap(),
        Err(Rejection::NotIdle(_))
    ));
    assert_eq!(fixture.prompt_count(), 0);
    assert_eq!(fixture.provider_write_count(), 0);
    assert!(
        fixture.correlated_runs().is_empty(),
        "refusal precedes adapter entry"
    );
    fixture.clear_request();
    fixture
        .handle
        .try_idle_prompt_for_run(run, "managed delivery".into())
        .unwrap();
    fixture.wait_for_provider_writes(1);
    assert_eq!(fixture.correlated_runs(), vec![run]);
    assert_eq!(fixture.prompt_texts(), vec!["managed delivery"]);
    fixture.close();
}

#[test]
fn managed_prompt_waiting_unready_or_busy_never_enters_the_adapter() {
    for model in ["busy", "unready", "waiting"] {
        let mut fixture = RequestRaceFixture::new();
        fixture
            .handle
            .command(Command::SetModel {
                model: model.into(),
                reasoning: None,
            })
            .unwrap();
        assert!(matches!(
            fixture
                .handle
                .try_idle_prompt_for_run(crate::model::RunId::new(), "deferred".into()),
            Err(Rejection::NotIdle(_))
        ));
        assert!(fixture.correlated_runs().is_empty());
        assert_eq!(fixture.prompt_count(), 0);
        assert_eq!(fixture.provider_write_count(), 0);
        fixture.close();
    }
}

#[test]
fn managed_prompt_step_failure_is_uncertain_after_possible_writes() {
    let fixture = RequestRaceFixture::new();
    let run = crate::model::RunId::new();
    let result = fixture
        .handle
        .try_idle_prompt_for_run(run, "fixture uncertain Step".into());
    assert!(
        matches!(result, Err(Rejection::Unacknowledged(_))),
        "{result:?}"
    );
    assert_eq!(fixture.correlated_runs(), vec![run]);
    // The fixture's existing owner closes and waits its exact child. There is
    // no automatic second prompt after this uncertain first attempt.
}

#[test]
fn unanswered_request_rejects_retries_until_resolve_or_cancel_then_admits_once() {
    for action in ["resolve", "cancel"] {
        let mut fixture = RequestRaceFixture::new();
        fixture.begin_request();
        fixture.release_request();
        for _ in 0..3 {
            refused(fixture.handle.try_idle_prompt("deferred courier".into()));
        }
        assert_eq!(fixture.prompt_count(), 0);
        assert_eq!(fixture.provider_write_count(), 0);
        fixture.resolve_request(action);
        assert!(fixture.handle.is_idle());
        fixture
            .handle
            .try_idle_prompt("deferred courier".into())
            .unwrap();
        fixture.wait_for_provider_writes(1);
        assert_eq!(fixture.prompt_texts(), ["deferred courier"]);
        refused(
            fixture
                .handle
                .try_idle_prompt("duplicate while busy".into()),
        );
        assert_eq!(fixture.prompt_count(), 1);
        fixture.close();
    }
}

#[test]
fn ordinary_human_prompt_keeps_adapter_admission_while_request_is_pending() {
    let mut fixture = RequestRaceFixture::new();
    fixture.begin_request();
    fixture.release_request();
    fixture
        .handle
        .command(Command::Prompt {
            text: "human follow-up".into(),
            attachments: Vec::new(),
            mode: super::super::PromptMode::Send,
        })
        .unwrap();
    fixture.wait_for_provider_writes(1);
    assert_eq!(fixture.prompt_texts(), ["human follow-up"]);
    assert_eq!(fixture.handle.requests()[0].id, REQUEST);
    fixture.close();
}

#[test]
fn courier_prompt_refuses_busy_unready_waiting_and_exited_actors() {
    for state in ["busy", "unready", "waiting"] {
        let mut fixture = RequestRaceFixture::new();
        fixture
            .handle
            .command(Command::SetModel {
                model: state.into(),
                reasoning: None,
            })
            .unwrap();
        assert!(!fixture.handle.is_idle());
        refused(fixture.handle.try_idle_prompt("courier delivery".into()));
        assert_eq!(fixture.prompt_count(), 0);
        assert_eq!(fixture.provider_write_count(), 0);
        fixture.close();
    }
    let mut fixture = RequestRaceFixture::new();
    fixture.close();
    assert!(fixture.handle.is_completed());
    refused(fixture.handle.try_idle_prompt("courier delivery".into()));
    assert_eq!(fixture.prompt_count(), 0);
}
