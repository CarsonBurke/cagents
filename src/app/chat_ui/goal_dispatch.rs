//! Native goal intent custody. Nothing in this path is a prompt or follow-up.
use super::*;
use crate::chat::goal::{Operation, Outcome, ReadState, Status, Support};
use std::sync::Weak;

#[derive(Clone)]
struct Binding {
    target: Target,
    session: chat::Session,
    handle: Weak<ClientHandle>,
}
impl Binding {
    fn capture(runtime: &Runtime) -> Option<Self> {
        let handle = runtime.handle()?;
        (handle.is_connected() && handle.session() == runtime.session).then(|| Self {
            target: runtime.target,
            session: runtime.session.clone(),
            handle: Arc::downgrade(handle),
        })
    }
    fn matches(&self, runtime: &Runtime) -> bool {
        self.target == runtime.target
            && self.session == runtime.session
            && self
                .handle
                .upgrade()
                .zip(runtime.handle())
                .is_some_and(|(original, current)| {
                    Arc::ptr_eq(&original, current)
                        && current.is_connected()
                        && current.session() == self.session
                })
    }
}

/// Includes the native actor witness: a Target and reused epoch alone are not
/// authority to redirect an already queued button callback to another actor.
#[derive(Clone)]
pub struct Control {
    binding: Binding,
    thread: String,
    intent: goal::Intent,
}

#[derive(Clone)]
struct Input {
    revision: u64,
    text: String,
}
impl Input {
    fn matches(&self, runtime: &Runtime) -> bool {
        runtime.revision == self.revision
            && runtime.composer.text() == self.text
            && runtime.attachments.is_empty()
            && runtime.pastes_pending == 0
            && runtime.attachment_chooser_pending.is_none()
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Waiting,
    Model(u64),
    Native {
        revision: u64,
        command: u64,
        accepted: bool,
    },
}
#[derive(Clone)]
pub(super) struct Pending {
    operation: Operation,
    input: Option<Input>,
    start: Option<chat::Session>,
    binding: Option<Binding>,
    model: Option<(String, Option<String>)>,
    model_sent: bool,
    wait_for_ready: bool,
    stage: Stage,
}

pub(super) fn blocks_automatic(snapshot: &Snapshot) -> bool {
    snapshot.goal.as_ref().is_some_and(|goal| {
        goal.pending
            || goal.uncertain
            || match (&goal.support, &goal.read) {
                (Support::Unsupported, _) => true,
                (Support::Supported, ReadState::Known(None)) => false,
                (Support::Supported, ReadState::Known(Some(goal))) => {
                    !matches!(goal.status, Status::Paused | Status::Complete)
                }
                _ => true,
            }
    })
}

/// Read the current handle publication at the dispatch boundary, not the last
/// rendered panel. All command paths additionally retain their original source.
pub(super) fn current_goal(
    runtime: &Runtime,
    handle: &Arc<ClientHandle>,
) -> Option<chat::goal::Snapshot> {
    if runtime.session.launch.provider != Provider::Codex
        || runtime.selected_subagent.is_some()
        || runtime.disconnected
        || !handle.is_connected()
        || runtime
            .handle()
            .is_none_or(|current| !Arc::ptr_eq(current, handle))
        || handle.session() != runtime.session
    {
        return None;
    }
    let latest = handle.snapshot();
    let goal = latest.goal.as_ref()?;
    (!latest.exited
        && latest.session_id.as_deref() == Some(goal.thread_id.as_str())
        && runtime.session.launch.session_id.as_deref() == Some(goal.thread_id.as_str()))
    .then(|| goal.clone())
}

impl Runtime {
    /// One view hook; the parent composer and its text/caret/attachments are
    /// never replaced by opening, closing or pressing a goal control.
    pub(in crate::app) fn goal_panel(&self) -> Option<Element<'_, Message>> {
        if !self.goal_open {
            return None;
        }
        let binding = Binding::capture(self);
        let live = self.handle().and_then(|handle| current_goal(self, handle));
        let observed = self.snapshot.goal.as_ref();
        let current = live
            .as_ref()
            .zip(observed)
            .is_some_and(|(live, observed)| live == observed)
            && self.pending.is_none()
            && self.goal_pending.is_none();
        let support = observed.map_or(
            if self.session.launch.provider == Provider::Codex {
                Support::Unknown
            } else {
                Support::Unsupported
            },
            |goal| goal.support,
        );
        let presentation = goal::Presentation {
            support,
            read: observed.map_or(&ReadState::Unknown, |goal| &goal.read),
            pending: observed.is_some_and(|goal| goal.pending) || self.goal_pending.is_some(),
            uncertain: observed.is_some_and(|goal| goal.uncertain),
            current,
            work_ready: self.handle().is_some_and(|handle| {
                let latest = handle.snapshot();
                latest.ready && latest.read_only_reason.is_none()
            }),
            observation_revision: observed.map_or(0, |goal| goal.revision),
        };
        let target = self.target;
        let thread = observed.map_or_else(String::new, |goal| goal.thread_id.clone());
        let panel = goal::panel(presentation, move |intent| {
            // Disabled controls never call the reducer. Keep the callback pure
            // even for an unavailable observation; there is no I/O here.
            let binding = binding.clone().unwrap_or_else(|| Binding {
                target,
                session: self.session.clone(),
                handle: Weak::new(),
            });
            Message::ChatUi(Action::GoalIntent(
                target,
                Control {
                    binding,
                    thread: thread.clone(),
                    intent,
                },
            ))
        });
        Some(
            crate::ui_controls::owned(
                (
                    target,
                    self.handle().map(|handle| Arc::as_ptr(handle) as usize),
                ),
                widget::column::with_children(vec![
                    crate::ui_controls::button::quiet("Close goal")
                        .on_press(Message::ChatUi(Action::GoalPanel(target, false)))
                        .into(),
                    panel,
                ])
                .spacing(design::space::S)
                .width(Length::Fill),
            )
            .into(),
        )
    }

    pub(super) fn cancel_unsent_goal(&mut self) {
        if self
            .goal_pending
            .as_ref()
            .is_some_and(|pending| matches!(pending.stage, Stage::Waiting))
        {
            self.goal_pending = None;
            let _ = self.present();
        }
    }
    pub(super) fn cancel_goal_for_target(&mut self, target: Target) {
        if self.goal_pending.as_ref().is_some_and(|pending| {
            matches!(pending.stage, Stage::Waiting)
                && pending
                    .binding
                    .as_ref()
                    .is_none_or(|binding| binding.target != target)
        }) {
            self.goal_pending = None;
        }
    }

    pub(super) fn fail_goal_start(&mut self, session: &chat::Session) {
        if self
            .goal_pending
            .as_ref()
            .is_some_and(|pending| pending.start.as_ref() == Some(session))
        {
            self.goal_pending = None;
        }
    }
    pub(super) fn bind_started_goal(
        &mut self,
        original: &chat::Session,
        target: Target,
        session: &chat::Session,
        handle: &Arc<ClientHandle>,
    ) {
        let Some(pending) = &mut self.goal_pending else {
            return;
        };
        if pending.start.as_ref() != Some(original) {
            return;
        }
        // Ready has already admitted the exact start result and canonical card.
        // Do not transfer a deferred intent into account recovery/another start.
        if self.session != *original
            || session.id != original.id
            || session.launch.provider != original.launch.provider
        {
            self.goal_pending = None;
            self.error = Some(
                "The goal's original conversation changed; submit it explicitly again.".into(),
            );
            return;
        }
        pending.start = None;
        pending.binding = Some(Binding {
            target,
            session: session.clone(),
            handle: Arc::downgrade(handle),
        });
    }
    pub(super) fn goal_command_matches(&self, command: u64) -> bool {
        self.goal_pending
            .as_ref()
            .is_some_and(|pending| match pending.stage {
                Stage::Model(id) | Stage::Native { command: id, .. } => command == id,
                Stage::Waiting => false,
            })
    }
    pub(super) fn observe_goal_completion(&mut self, snapshot: &Snapshot) {
        let Some(pending) = self.goal_pending.as_ref() else {
            return;
        };
        let Stage::Native {
            revision,
            accepted: true,
            ..
        } = pending.stage
        else {
            return;
        };
        if pending
            .binding
            .as_ref()
            .is_none_or(|binding| !binding.matches(self))
        {
            self.goal_pending = None;
            return;
        }
        let Some(goal) = &snapshot.goal else { return };
        if snapshot.session_id.as_deref() != Some(goal.thread_id.as_str())
            || self.session.launch.session_id.as_deref() != Some(goal.thread_id.as_str())
            || goal.pending
        {
            return;
        }
        let result = goal
            .last_result
            .filter(|result| result.revision == revision);
        let Some(result) = result else {
            if goal.revision > revision {
                self.error = Some("The native goal changed before its result was confirmed. Check the goal; your input is kept.".into());
                self.goal_pending = None;
            }
            return;
        };
        let input = pending.input.clone();
        let objective_matches = match &pending.operation {
            Operation::SetObjective(expected) => matches!(&goal.read,
                ReadState::Known(Some(observed)) if &observed.objective == expected),
            _ => true,
        };
        self.goal_pending = None;
        match result.outcome {
            Outcome::Confirmed => {
                if !objective_matches {
                    self.error = Some("The provider confirmed a different current goal objective. Its actual observation is shown; your input is kept.".into());
                    return;
                }
                if input.as_ref().is_some_and(|input| input.matches(self)) {
                    self.replace_draft(text_editor::Content::new());
                }
                self.error = None;
            }
            Outcome::Uncertain => self.error = Some("The native goal result is unconfirmed. Check the goal before trying a change again; your input is kept.".into()),
            Outcome::Unsupported => self.error = Some("Native goals are not supported by this provider connection; your input is kept.".into()),
        }
        let _ = self.present();
    }
}

impl Cagents {
    fn goal_source_current(&self, target: Target) -> bool {
        matches!((self.card_state(target.card), self.cards.get(&target.card)),
            (Some(card), Some(CardRuntime::Chat(runtime))) if runtime.target == target
                && matches!(&card.kind, CardKind::Chat(session) if session == &runtime.session))
    }

    pub(super) fn submit_goal(
        &mut self,
        target: Target,
        mode: PromptMode,
        parsed: Result<Operation, chat::goal::Error>,
    ) -> Task<Message> {
        if !self.goal_source_current(target) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.goal_open = true;
        let refusal = if mode != PromptMode::Send {
            Some(
                "Native goals cannot be queued or steered. Use Send to request the goal change."
                    .to_owned(),
            )
        } else if runtime.session.launch.provider != Provider::Codex {
            Some("Native goals are supported only by Codex connections; this input will not be sent as a prompt.".into())
        } else if runtime.selected_subagent.is_some() {
            Some("Return to the parent conversation before using its native goal controls.".into())
        } else if runtime.pastes_pending > 0
            || runtime.attachment_chooser_pending.is_some()
            || !runtime.attachments.is_empty()
        {
            Some("Finish the paste and remove attachments before requesting a native goal; all input is kept.".into())
        } else if runtime.goal_pending.is_some()
            || runtime.pending.is_some()
            || runtime.queued.is_some()
            || !runtime.followups.is_empty()
        {
            Some("Finish the pending chat or goal request before submitting a native goal.".into())
        } else {
            None
        };
        if let Some(refusal) = refusal {
            runtime.error = Some(refusal);
            return Task::none();
        }
        let operation = match parsed {
            Ok(operation) => operation,
            Err(error) => {
                runtime.error = Some(error.to_string());
                return Task::none();
            }
        };
        let draft = runtime.is_draft();
        let mut session = runtime.session.clone();
        if draft {
            session.id = TerminalId::new();
        }
        let binding = if draft {
            None
        } else {
            Binding::capture(runtime)
        };
        if !draft && binding.is_none() {
            runtime.error = Some(
                "Reconnect the original conversation before requesting its native goal.".into(),
            );
            return Task::none();
        }
        runtime.goal_pending = Some(Pending {
            operation,
            input: Some(Input {
                revision: runtime.revision,
                text: runtime.composer.text(),
            }),
            start: draft.then(|| session.clone()),
            binding,
            model: draft
                .then(|| {
                    runtime
                        .snapshot
                        .selected_model
                        .clone()
                        .map(|model| (model, runtime.snapshot.reasoning.clone()))
                })
                .flatten(),
            model_sent: false,
            wait_for_ready: draft,
            stage: Stage::Waiting,
        });
        runtime.error = None;
        runtime.model_picker = None;
        runtime.account_picker = false;
        runtime.reasoning_picker = false;
        if draft {
            runtime.session = session.clone();
            self.replace_chat_conversation(target.card, session.clone());
            self.start_chat(target.card, session, chat::Intent::Start, None, false)
        } else {
            self.dispatch_goal(target)
        }
    }

    pub(super) fn goal_control(&mut self, target: Target, control: Control) -> Task<Message> {
        if !self.goal_source_current(target) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.goal_open = true;
        if let Err(error) = control.intent.operation.validate() {
            runtime.error = Some(error.to_string());
            return Task::none();
        }
        let Some(handle) = runtime.handle().cloned() else {
            return Task::none();
        };
        let valid = control.binding.matches(runtime)
            && runtime.goal_pending.is_none()
            && runtime.pending.is_none()
            && runtime.queued.is_none()
            && current_goal(runtime, &handle).is_some_and(|goal| {
                goal.thread_id == control.thread
                    && goal.revision == control.intent.observation_revision
                    && !goal.pending
                    && (control.intent.operation == Operation::Get || !goal.uncertain)
            });
        if !valid {
            runtime.error = Some("The goal control is stale or its conversation is unavailable. Check the current goal.".into());
            return Task::none();
        }
        runtime.goal_pending = Some(Pending {
            operation: control.intent.operation,
            input: None,
            start: None,
            binding: Some(control.binding),
            model: None,
            model_sent: false,
            wait_for_ready: false,
            stage: Stage::Waiting,
        });
        self.dispatch_goal(target)
    }

    pub(super) fn dispatch_goal(&mut self, target: Target) -> Task<Message> {
        let was_pending = self
            .chat_card_runtime_mut(target)
            .is_some_and(|runtime| runtime.goal_pending.is_some());
        let task = self.dispatch_goal_inner(target);
        if was_pending {
            if let Some(runtime) = self.chat_card_runtime_mut(target) {
                if runtime.goal_pending.is_none() {
                    let _ = runtime.present();
                }
            }
        }
        task
    }

    fn dispatch_goal_inner(&mut self, target: Target) -> Task<Message> {
        if self.chat_start_pending(target.card)
            || self.chat_closed_elsewhere(target.card)
            || self
                .card_state(target.card)
                .is_none_or(|state| !state.existence.is_open())
        {
            return Task::none();
        }
        if !self.goal_source_current(target) {
            if let Some(runtime) = self.chat_card_runtime_mut(target) {
                runtime.goal_pending = None;
                runtime.error = Some(
                    "The goal's original saved conversation changed. Your input is kept.".into(),
                );
            }
            return Task::none();
        }
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        if let Some(handle) = runtime.handle().cloned() {
            runtime.observe_goal_completion(&handle.snapshot());
        }
        let Some(mut pending) = runtime.goal_pending.take() else {
            return Task::none();
        };
        if pending.start.is_some() {
            runtime.goal_pending = Some(pending);
            return Task::none();
        }
        if pending
            .binding
            .as_ref()
            .is_none_or(|binding| !binding.matches(runtime))
            || pending
                .input
                .as_ref()
                .is_some_and(|input| !input.matches(runtime))
            || runtime.selected_subagent.is_some()
        {
            runtime.error = Some(
                "The goal's original conversation or input changed. Submit it explicitly again."
                    .into(),
            );
            return Task::none();
        }
        if !matches!(pending.stage, Stage::Waiting) || runtime.pending.is_some() {
            runtime.goal_pending = Some(pending);
            return Task::none();
        }
        let handle = runtime.handle().unwrap().clone();
        let latest = handle.snapshot();
        if latest.exited || runtime.disconnected {
            runtime.error = Some(
                "The original conversation cannot accept a native goal; your input is kept.".into(),
            );
            return Task::none();
        }
        if (!latest.ready || latest.read_only_reason.is_some())
            && (matches!(
                pending.operation,
                Operation::SetObjective(_) | Operation::Resume
            ) || pending.model.is_some())
        {
            if pending.wait_for_ready && latest.read_only_reason.is_none() {
                runtime.goal_pending = Some(pending);
            } else {
                runtime.error = Some("The original conversation is not ready for goal work. Check, pause or clear remain available; your input is kept.".into());
            }
            return Task::none();
        }
        let model_command = if let Some((model, reasoning)) = &pending.model {
            if latest.selected_model.as_ref() == Some(model) && &latest.reasoning == reasoning {
                pending.model = None;
                None
            } else if pending.model_sent {
                runtime.goal_pending = Some(pending);
                return Task::none();
            } else if latest.models.iter().any(|candidate| &candidate.id == model) {
                Some(Command::SetModel {
                    model: model.clone(),
                    reasoning: reasoning.clone(),
                })
            } else if latest.models.is_empty() {
                runtime.goal_pending = Some(pending);
                return Task::none();
            } else {
                runtime.error = Some("The selected model is unavailable. The goal was not sent; choose a model and submit again.".into());
                return Task::none();
            }
        } else {
            None
        };
        let (command, revision) = if let Some(command) = model_command {
            (command, None)
        } else {
            let Some(goal) = current_goal(runtime, &handle) else {
                runtime.goal_pending = Some(pending);
                return Task::none();
            };
            if goal.pending {
                runtime.goal_pending = Some(pending);
                return Task::none();
            }
            if goal.support == Support::Unsupported {
                runtime.error = Some("Native goals are not supported by this provider connection; your input is kept.".into());
                return Task::none();
            }
            if goal.uncertain && pending.operation != Operation::Get {
                runtime.error = Some("Check the unconfirmed native goal before requesting another change; your input is kept.".into());
                return Task::none();
            }
            (
                Command::Goal {
                    thread_id: goal.thread_id,
                    revision: goal.revision,
                    operation: pending.operation.clone(),
                },
                Some(goal.revision),
            )
        };
        runtime.goal_pending = Some(pending);
        let task = self.chat_send(target, command, false);
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return task;
        };
        if let Some(command) = runtime.pending.as_ref().map(|pending| pending.id) {
            if let Some(pending) = &mut runtime.goal_pending {
                pending.stage = match revision {
                    Some(revision) => Stage::Native {
                        revision,
                        command,
                        accepted: false,
                    },
                    None => {
                        pending.model_sent = true;
                        Stage::Model(command)
                    }
                };
            }
        } else {
            runtime.goal_pending = None;
        }
        let _ = runtime.present();
        task
    }

    pub(super) fn goal_command_finished(
        &mut self,
        target: Target,
        command: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.goal_command_matches(command) {
            return Task::none();
        }
        if let Err(error) = result {
            runtime.goal_pending = None;
            let _ = runtime.present();
            runtime.error = Some(format!(
                "Native goal dispatch was not confirmed: {error}. Your input is kept; check the current goal."
            ));
            return Task::none();
        }
        if let Some(pending) = &mut runtime.goal_pending {
            match &mut pending.stage {
                Stage::Model(_) => pending.stage = Stage::Waiting,
                Stage::Native { accepted, .. } => *accepted = true,
                Stage::Waiting => {}
            }
        }
        runtime.refresh();
        self.dispatch_goal(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const THREAD: &str = "0199a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b";

    fn runtime(app: &Cagents, card: CardId) -> &Runtime {
        match &app.cards[&card] {
            CardRuntime::Chat(runtime) => runtime,
            _ => panic!("chat fixture"),
        }
    }
    fn draft(app: &mut Cagents, provider: Provider) -> (CardId, Target) {
        drop(app.new_native_chat(provider, Some(PathBuf::from("/workspace/chat-fixture"))));
        let card = app.workspace.focused().unwrap();
        (card, runtime(app, card).target)
    }
    fn write(app: &mut Cagents, target: Target, text: &str) {
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::new());
        drop(app.handle_chat_ui(Action::Composer(
            target,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text.into()))),
        )));
    }
    fn goal(revision: u64) -> chat::goal::Snapshot {
        chat::goal::Snapshot {
            thread_id: THREAD.into(),
            revision,
            support: Support::Supported,
            read: ReadState::Known(None),
            pending: false,
            uncertain: false,
            last_result: None,
        }
    }
    fn ready(app: &mut Cagents, card: CardId, snapshot: Snapshot) -> Target {
        let attempt = app.chat_ui.attempt(card).unwrap();
        let target = attempt.target;
        let mut session = attempt.session.clone();
        let proof = crate::model::HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-incarnation".into(),
        };
        if session.launch.provider == Provider::Codex {
            session.claim_owned_codex_listener(Path::new("/daemon-temp-root"), &proof);
        }
        session.proof = Some(proof);
        let handle = Arc::new(ClientHandle::fixture(session, snapshot));
        drop(app.handle_chat_ui(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        target
    }
    fn live() -> (Cagents, CardId, Target) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, _) = draft(&mut app, Provider::Codex);
        let session = runtime(&app, card).session.clone();
        drop(app.start_chat(card, session, chat::Intent::Start, None, false));
        let target = ready(
            &mut app,
            card,
            Snapshot {
                session_id: Some(THREAD.into()),
                ready: true,
                goal: Some(goal(8)),
                ..Snapshot::default()
            },
        );
        assert!(runtime(&app, card).pending.is_none());
        assert!(Binding::capture(runtime(&app, card)).is_some());
        (app, card, target)
    }
    fn acknowledge(app: &mut Cagents, card: CardId, result: Result<(), String>) {
        let runtime = runtime(app, card);
        let (target, id) = (runtime.target, runtime.pending.as_ref().unwrap().id);
        drop(app.handle_chat_ui(Action::CommandFinished(target, id, result)));
    }
    fn publish(app: &mut Cagents, card: CardId, mut snapshot: Snapshot) {
        let target = runtime(app, card).target;
        snapshot.generation = runtime(app, card).handle().unwrap().snapshot().generation + 1;
        runtime(app, card)
            .handle()
            .unwrap()
            .publish_snapshot_for_test(snapshot);
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    }
    fn finish_native(app: &mut Cagents, card: CardId, revision: u64, outcome: Outcome) {
        let mut next = (*runtime(app, card).handle().unwrap().snapshot()).clone();
        let goal = next.goal.as_mut().unwrap();
        goal.revision = revision + 2;
        goal.pending = false;
        goal.uncertain = outcome == Outcome::Uncertain;
        goal.support = if outcome == Outcome::Unsupported {
            Support::Unsupported
        } else {
            Support::Supported
        };
        goal.last_result = Some(chat::goal::Completion { revision, outcome });
        if outcome == Outcome::Confirmed {
            if let Some(Pending {
                operation: Operation::SetObjective(objective),
                ..
            }) = runtime(app, card).goal_pending.as_ref()
            {
                goal.read = ReadState::Known(Some(chat::goal::Goal {
                    thread_id: THREAD.into(),
                    objective: objective.clone(),
                    status: Status::Blocked,
                    token_budget: None,
                    tokens_used: 1,
                    time_used_seconds: 1,
                    created_at: 1,
                    updated_at: 1,
                }));
            }
        }
        publish(app, card, next);
    }
    fn control(runtime: &Runtime, operation: Operation) -> Control {
        let goal = runtime.handle().unwrap().snapshot().goal.clone().unwrap();
        Control {
            binding: Binding::capture(runtime).unwrap(),
            thread: goal.thread_id,
            intent: goal::Intent {
                operation,
                observation_revision: goal.revision,
            },
        }
    }

    #[test]
    fn goal_submit_precedes_automatic_account_routing_and_never_creates_a_prompt_queue() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let alternate = app.account_ui.add_managed_fixture(Provider::Codex, 105);
        let (card, target) = draft(&mut app, Provider::Codex);
        let now = chat::now_ms().unwrap();
        for (id, status) in [
            (
                Account::system(Provider::Codex).id(),
                account::UsageStatus::Limited,
            ),
            (alternate.clone(), account::UsageStatus::Ok),
        ] {
            let owner = app.account_ui.probe_owner(&id).unwrap();
            drop(
                app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
                    owner,
                    id,
                    outcome: account::probe::Outcome::Answered {
                        identity: None,
                        catalog: None,
                        snapshot: account::UsageSnapshot::state(status, "", now),
                    },
                })),
            );
        }
        write(&mut app, target, "ordinary message");
        assert_eq!(
            app.automatic_chat_account(target, PromptMode::Send)
                .unwrap()
                .0
                .id(),
            alternate
        );
        write(&mut app, target, "/GoAl set Keep this exact account");
        let before = runtime(&app, card).session.launch.clone();
        let input = runtime(&app, card).composer.text();
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        let runtime = runtime(&app, card);
        assert!(runtime.queued.is_none());
        assert!(runtime.followups.is_empty());
        assert!(runtime.pending.is_none());
        assert_eq!(runtime.composer.text(), input);
        assert_eq!(runtime.session.launch, before);
        assert_eq!(
            runtime.goal_pending.as_ref().unwrap().operation,
            Operation::SetObjective("Keep this exact account".into())
        );
        assert_eq!(app.chat_ui.attempt(card).unwrap().session.launch, before);
        assert!(submission(&input, &runtime.snapshot, &[], PromptMode::Queue).is_err());
    }

    #[test]
    fn goal_input_refusals_preserve_draft_paste_attachments_and_unrelated_followups() {
        for (provider, mode, text) in [
            (Provider::Claude, PromptMode::Send, "/goal work"),
            (Provider::Codex, PromptMode::Queue, "/goal work"),
            (Provider::Codex, PromptMode::Steer, "/goal work"),
            (Provider::Codex, PromptMode::Send, "/goal pause unexpected"),
        ] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (card, target) = draft(&mut app, provider);
            write(&mut app, target, text);
            let before = runtime(&app, card).composer.text();
            drop(app.handle_chat_ui(Action::Submit(target, mode)));
            let runtime = runtime(&app, card);
            assert_eq!(runtime.composer.text(), before);
            assert!(
                runtime.goal_pending.is_none()
                    && runtime.queued.is_none()
                    && runtime.pending.is_none()
            );
            assert!(runtime.error.is_some());
            assert!(!app.chat_start_pending(card));
        }
        let (mut app, card, target) = live();
        write(&mut app, target, "/goal set preserve accepted paste");
        app.chat_runtime_mut(target).unwrap().pastes_pending = 1;
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        assert_eq!(runtime(&app, card).pastes_pending, 1);
        assert!(runtime(&app, card).submit_after_paste.is_none());
        assert!(runtime(&app, card).goal_pending.is_none());
        let rt = app.chat_runtime_mut(target).unwrap();
        rt.pastes_pending = 0;
        rt.attachments.push(Attachment::file("kept-attachment.txt"));
        let attachments = rt.attachments.clone();
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        assert_eq!(runtime(&app, card).attachments, attachments);
        assert!(runtime(&app, card).goal_pending.is_none());
        let rt = app.chat_runtime_mut(target).unwrap();
        rt.attachments.clear();
        rt.followups.push(Followup {
            id: 1,
            text: "unrelated".into(),
            preview: "unrelated".into(),
            attachments: vec![],
        });
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        assert_eq!(runtime(&app, card).followups[0].text, "unrelated");
        assert!(runtime(&app, card).goal_pending.is_none());
    }

    #[test]
    fn native_confirmation_not_transport_acceptance_consumes_only_unchanged_goal_input() {
        for edit_after_dispatch in [false, true] {
            let (mut app, card, target) = live();
            write(&mut app, target, "/goal set original objective");
            let source = runtime(&app, card).composer.text();
            drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
            let rt = runtime(&app, card);
            assert!(matches!(
                rt.goal_pending.as_ref().unwrap().stage,
                Stage::Native { revision: 8, .. }
            ));
            let pending = rt.pending.as_ref().unwrap();
            assert!(
                pending.sent.is_none()
                    && pending.first_prompt.is_none()
                    && pending.draft_revision.is_none()
                    && pending.queued.is_none()
            );
            acknowledge(&mut app, card, Ok(()));
            assert_eq!(runtime(&app, card).composer.text(), source);
            assert!(runtime(&app, card).goal_pending.is_some());
            assert!(runtime(&app, card).transition_blocker().is_none());
            assert!(
                runtime(&app, card).handoff_blocker().is_none(),
                "native result wait is not a new Return/Close gate"
            );
            if edit_after_dispatch {
                write(&mut app, target, "new unsent draft");
            }
            finish_native(&mut app, card, 8, Outcome::Confirmed);
            let rt = runtime(&app, card);
            assert!(rt.goal_pending.is_none());
            assert_eq!(
                rt.composer.text().trim(),
                if edit_after_dispatch {
                    "new unsent draft"
                } else {
                    ""
                }
            );
            assert!(
                rt.history.is_empty(),
                "a goal is not a prompt-history message"
            );
        }
    }

    #[test]
    fn uncertain_unsupported_wrong_revision_and_rejected_dispatch_never_consume_or_replay_input() {
        for outcome in [Outcome::Uncertain, Outcome::Unsupported] {
            let (mut app, card, target) = live();
            write(&mut app, target, "/goal clear");
            drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
            acknowledge(&mut app, card, Ok(()));
            finish_native(&mut app, card, 8, outcome);
            assert_eq!(runtime(&app, card).composer.text().trim(), "/goal clear");
            assert!(
                runtime(&app, card).goal_pending.is_none() && runtime(&app, card).pending.is_none()
            );
            assert!(runtime(&app, card).error.is_some());
        }
        let (mut app, card, target) = live();
        write(&mut app, target, "/goal pause");
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        acknowledge(&mut app, card, Err("dispatch unknown".into()));
        finish_native(&mut app, card, 8, Outcome::Confirmed);
        assert_eq!(runtime(&app, card).composer.text().trim(), "/goal pause");
        assert!(runtime(&app, card).goal_pending.is_none());
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        let revision = match runtime(&app, card).goal_pending.as_ref().unwrap().stage {
            Stage::Native { revision, .. } => revision,
            _ => panic!("native"),
        };
        acknowledge(&mut app, card, Ok(()));
        finish_native(&mut app, card, revision + 10, Outcome::Confirmed);
        assert_eq!(runtime(&app, card).composer.text().trim(), "/goal pause");
        assert!(runtime(&app, card).goal_pending.is_none());
        let (mut app, card, target) = live();
        write(&mut app, target, "/goal set requested objective");
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        acknowledge(&mut app, card, Ok(()));
        let mut different = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
        let observed = different.goal.as_mut().unwrap();
        observed.revision = 10;
        observed.read = ReadState::Known(Some(chat::goal::Goal {
            thread_id: THREAD.into(),
            objective: "provider's different objective".into(),
            status: Status::Blocked,
            token_budget: None,
            tokens_used: 1,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 1,
        }));
        observed.last_result = Some(chat::goal::Completion {
            revision: 8,
            outcome: Outcome::Confirmed,
        });
        publish(&mut app, card, different);
        assert_eq!(
            runtime(&app, card).composer.text().trim(),
            "/goal set requested objective"
        );
        assert!(
            runtime(&app, card)
                .error
                .as_ref()
                .unwrap()
                .contains("different current goal objective")
        );
        assert!(runtime(&app, card).goal_pending.is_none());
    }

    #[test]
    fn goal_callbacks_cannot_retarget_actor_thread_observation_or_selected_child() {
        let (mut app, card, target) = live();
        write(&mut app, target, "unrelated draft and caret");
        let old = control(runtime(&app, card), Operation::Get);
        let held_original = runtime(&app, card).handle().unwrap().clone();
        let replacement = Arc::new(ClientHandle::fixture(
            runtime(&app, card).session.clone(),
            (*held_original.snapshot()).clone(),
        ));
        app.chat_runtime_mut(target).unwrap().connection = Connection::Live(replacement);
        drop(app.handle_chat_ui(Action::GoalIntent(target, old)));
        assert!(runtime(&app, card).pending.is_none());
        let old = control(runtime(&app, card), Operation::Get);
        let mut newer = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
        newer.goal.as_mut().unwrap().revision += 1;
        publish(&mut app, card, newer);
        drop(app.handle_chat_ui(Action::GoalIntent(target, old)));
        assert!(runtime(&app, card).pending.is_none());
        let mut wrong_thread = control(runtime(&app, card), Operation::Get);
        wrong_thread.thread = "foreign-thread".into();
        drop(app.handle_chat_ui(Action::GoalIntent(target, wrong_thread)));
        assert!(runtime(&app, card).pending.is_none());
        app.chat_runtime_mut(target).unwrap().selected_subagent = Some("child".into());
        let child = control(runtime(&app, card), Operation::Get);
        drop(app.handle_chat_ui(Action::GoalIntent(target, child)));
        assert!(runtime(&app, card).pending.is_none());
        write(&mut app, target, "/goal clear");
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        assert!(runtime(&app, card).pending.is_none());
        app.chat_runtime_mut(target).unwrap().selected_subagent = None;
        write(&mut app, target, "unrelated draft and caret");
        let before = runtime(&app, card).composer.cursor();
        let fresh = control(runtime(&app, card), Operation::Get);
        drop(app.handle_chat_ui(Action::GoalIntent(target, fresh)));
        acknowledge(&mut app, card, Ok(()));
        finish_native(&mut app, card, 9, Outcome::Confirmed);
        assert_eq!(
            runtime(&app, card).composer.text().trim(),
            "unrelated draft and caret"
        );
        assert_eq!(runtime(&app, card).composer.cursor(), before);
        assert!(runtime(&app, card).goal_pending.is_none());
        drop(held_original);
        // An adopted actor may have an authoritative confirmed_thread before
        // native resume is admitted. Read/stop remain available without Ready.
        let mut blocked = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
        blocked.ready = false;
        let mut observed = goal(20);
        observed.read = ReadState::Known(Some(chat::goal::Goal {
            thread_id: THREAD.into(),
            objective: "retained active goal".into(),
            status: Status::Active,
            token_budget: None,
            tokens_used: 1,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 1,
        }));
        blocked.goal = Some(observed);
        publish(&mut app, card, blocked);
        write(&mut app, target, "/goal resume");
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        assert!(runtime(&app, card).pending.is_none());
        assert_eq!(runtime(&app, card).composer.text().trim(), "/goal resume");
        assert!(
            runtime(&app, card)
                .error
                .as_ref()
                .unwrap()
                .contains("not ready for goal work")
        );
        for operation in [Operation::Get, Operation::Pause, Operation::Clear] {
            let requested = control(runtime(&app, card), operation);
            let revision = requested.intent.observation_revision;
            drop(app.handle_chat_ui(Action::GoalIntent(target, requested)));
            assert!(runtime(&app, card).pending.is_some());
            acknowledge(&mut app, card, Ok(()));
            finish_native(&mut app, card, revision, Outcome::Confirmed);
            assert!(!runtime(&app, card).snapshot.ready);
            assert_eq!(runtime(&app, card).composer.text().trim(), "/goal resume");
        }
    }

    #[test]
    fn typed_draft_goal_waits_for_original_start_model_and_probe_without_a_literal_prompt() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = draft(&mut app, Provider::Codex);
        write(&mut app, target, "/goal set Keep selected model");
        {
            let rt = app.chat_runtime_mut(target).unwrap();
            let snapshot = Arc::make_mut(&mut rt.snapshot);
            snapshot.selected_model = Some("chosen-model".into());
            snapshot.reasoning = Some("low".into());
        }
        let original_launch = runtime(&app, card).session.launch.clone();
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        assert_eq!(
            app.chat_ui.attempt(card).unwrap().session.launch,
            original_launch
        );
        let mut checking = goal(1);
        checking.pending = true;
        let target = ready(
            &mut app,
            card,
            Snapshot {
                session_id: Some(THREAD.into()),
                ready: true,
                models: vec![chat::Model {
                    id: "chosen-model".into(),
                    label: "Chosen model".into(),
                    reasoning: vec!["low".into()],
                }],
                goal: Some(checking),
                ..Snapshot::default()
            },
        );
        assert!(runtime(&app, card).queued.is_none());
        assert!(matches!(
            runtime(&app, card).goal_pending.as_ref().unwrap().stage,
            Stage::Model(_)
        ));
        assert_eq!(
            runtime(&app, card).pending.as_ref().unwrap().model,
            Some(("chosen-model".into(), Some("low".into())))
        );
        acknowledge(&mut app, card, Ok(()));
        assert!(
            runtime(&app, card).pending.is_none(),
            "no duplicate model request while its snapshot is pending"
        );
        let mut selected = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
        selected.selected_model = Some("chosen-model".into());
        selected.reasoning = Some("low".into());
        publish(&mut app, card, selected);
        assert!(
            runtime(&app, card).pending.is_none(),
            "goal probe still owns its physical slot"
        );
        assert!(runtime(&app, card).goal_pending.is_some());
        assert_eq!(
            runtime(&app, card).composer.text().trim(),
            "/goal set Keep selected model"
        );
        let mut observed = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
        observed.goal = Some(goal(3));
        publish(&mut app, card, observed);
        assert!(matches!(
            runtime(&app, card).goal_pending.as_ref().unwrap().stage,
            Stage::Native { revision: 3, .. }
        ));
        acknowledge(&mut app, card, Ok(()));
        assert_eq!(
            runtime(&app, card).composer.text().trim(),
            "/goal set Keep selected model"
        );
        finish_native(&mut app, card, 3, Outcome::Confirmed);
        assert!(runtime(&app, card).composer.text().trim().is_empty());
        assert!(runtime(&app, card).queued.is_none());
        assert_eq!(runtime(&app, card).target, target);
    }

    #[test]
    fn failed_start_and_replaced_source_retire_goal_intent_without_retry_or_draft_loss() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = draft(&mut app, Provider::Codex);
        write(&mut app, target, "/goal set original draft");
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        let failed = super::super::refused_switch_message(&app, card, "test refusal");
        drop(app.update(failed));
        assert!(runtime(&app, card).goal_pending.is_none());
        assert_eq!(
            runtime(&app, card).composer.text().trim(),
            "/goal set original draft"
        );
        assert!(runtime(&app, card).queued.is_none());
        let (mut app, card, target) = live();
        write(&mut app, target, "/goal set original source");
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        acknowledge(&mut app, card, Ok(()));
        let replacement = Arc::new(ClientHandle::fixture(
            runtime(&app, card).session.clone(),
            (*runtime(&app, card).handle().unwrap().snapshot()).clone(),
        ));
        app.chat_runtime_mut(target).unwrap().connection = Connection::Live(replacement);
        finish_native(&mut app, card, 8, Outcome::Confirmed);
        assert_eq!(
            runtime(&app, card).composer.text().trim(),
            "/goal set original source"
        );
        assert!(runtime(&app, card).goal_pending.is_none());
    }

    #[test]
    fn native_goal_activity_blocks_automatic_account_handoff_between_turns_without_faking_turn_busy()
     {
        let (mut app, card, target) = live();
        let alternate = app.account_ui.add_managed_fixture(Provider::Codex, 106);
        let now = chat::now_ms().unwrap();
        for (id, status) in [
            (
                Account::system(Provider::Codex).id(),
                account::UsageStatus::Limited,
            ),
            (alternate.clone(), account::UsageStatus::Ok),
        ] {
            let owner = app.account_ui.probe_owner(&id).unwrap();
            drop(
                app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
                    owner,
                    id,
                    outcome: account::probe::Outcome::Answered {
                        identity: None,
                        catalog: None,
                        snapshot: account::UsageSnapshot::state(status, "", now),
                    },
                })),
            );
        }
        write(&mut app, target, "ordinary unsent message");
        assert_eq!(
            app.automatic_chat_account(target, PromptMode::Send)
                .unwrap()
                .0
                .id(),
            alternate
        );
        for (status, blocked) in [
            (Status::Active, true),
            (Status::Blocked, true),
            (Status::UsageLimited, true),
            (Status::BudgetLimited, true),
            (Status::Paused, false),
            (Status::Complete, false),
        ] {
            let mut next = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
            let mut observed = goal(next.generation + 1);
            observed.read = ReadState::Known(Some(chat::goal::Goal {
                thread_id: THREAD.into(),
                objective: "continue task".into(),
                status,
                token_budget: None,
                tokens_used: 0,
                time_used_seconds: 0,
                created_at: 1,
                updated_at: 1,
            }));
            next.goal = Some(observed);
            assert!(!next.busy);
            publish(&mut app, card, next);
            assert_eq!(
                app.automatic_chat_account(target, PromptMode::Send)
                    .is_none(),
                blocked
            );
        }
        for kind in 0..4 {
            let mut next = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
            let mut observed = goal(next.generation + 1);
            match kind {
                0 => observed.pending = true,
                1 => observed.uncertain = true,
                2 => {
                    observed.support = Support::Unknown;
                    observed.read = ReadState::Unknown;
                }
                _ => {
                    observed.support = Support::Unsupported;
                    observed.read = ReadState::Unknown;
                }
            }
            next.goal = Some(observed);
            publish(&mut app, card, next);
            assert!(
                app.automatic_chat_account(target, PromptMode::Send)
                    .is_none()
            );
        }
        let mut next = (*runtime(&app, card).handle().unwrap().snapshot()).clone();
        next.goal = None; // Ordinary pre-goal actors keep the existing routing contract.
        publish(&mut app, card, next);
        assert_eq!(
            app.automatic_chat_account(target, PromptMode::Send)
                .unwrap()
                .0
                .id(),
            alternate
        );
    }
}
