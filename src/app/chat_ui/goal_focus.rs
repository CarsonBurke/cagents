//! UI-only native focus custody. This leaf never dispatches a Goal RPC.
use super::*;
use iced::advanced::widget::operation::{Outcome, focusable};
use iced::advanced::widget::{Operation as WidgetOperation, operation};
use std::sync::Weak;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Control {
    Check,
    Pause,
    Resume,
    Clear,
}
impl Control {
    fn index(self) -> usize {
        match self {
            Self::Check => 0,
            Self::Pause => 1,
            Self::Resume => 2,
            Self::Clear => 3,
        }
    }
    pub(super) fn from_index(index: usize) -> Self {
        [Self::Check, Self::Pause, Self::Resume, Self::Clear][index]
    }
    pub(super) fn key(self) -> [u8; 32] {
        [self.index() as u8 + 1; 32]
    }
    fn from_key(key: [u8; 32]) -> Option<Self> {
        (0..4)
            .map(Self::from_index)
            .find(|control| control.key() == key)
    }
    fn id(self, target: Target) -> widget::Id {
        card_control_id(
            target.card,
            [
                "chat-goal-check",
                "chat-goal-pause",
                "chat-goal-resume",
                "chat-goal-clear",
            ][self.index()],
        )
    }
}

#[derive(Clone)]
struct Source {
    target: Target,
    session: chat::Session,
    actor: Weak<ClientHandle>,
    goal: chat::goal::Snapshot,
    work_ready: bool,
}
impl Source {
    fn capture(runtime: &Runtime, presentation: goal::Presentation<'_>) -> Option<Self> {
        if !presentation.current || presentation.pending || presentation.uncertain {
            return None;
        }
        let goal = runtime.snapshot.goal.as_ref()?;
        if !matches!(goal.read, chat::goal::ReadState::Known(Some(_))) {
            return None;
        }
        Some(Self {
            target: runtime.target,
            session: runtime.session.clone(),
            actor: Arc::downgrade(runtime.handle()?),
            goal: goal.clone(),
            work_ready: presentation.work_ready,
        })
    }
    fn same_actor(&self, runtime: &Runtime) -> bool {
        self.target == runtime.target
            && self.session == runtime.session
            && self
                .actor
                .upgrade()
                .zip(runtime.handle())
                .is_some_and(|(old, current)| {
                    Arc::ptr_eq(&old, current)
                        && current.is_connected()
                        && current.session() == self.session
                })
    }
    /// Publication can precede its UI Wake. Keep evidence alone across a
    /// provably accounting-only lag; the stale panel remains disabled/unmarked.
    fn accounting_lag(&self, runtime: &Runtime, presentation: goal::Presentation<'_>) -> bool {
        if presentation.current
            || presentation.pending
            || presentation.uncertain
            || runtime.pending.is_some()
            || runtime.goal_pending.is_some()
            || !runtime.goal_open
            || !runtime.visible.load(Ordering::Relaxed)
            || !self.same_actor(runtime)
            || runtime.snapshot.goal.as_ref() != Some(&self.goal)
        {
            return false;
        }
        let Some(handle) = runtime.handle() else {
            return false;
        };
        let Some(goal) = goal_dispatch::current_goal(runtime, handle) else {
            return false;
        };
        let next = Self {
            target: runtime.target,
            session: runtime.session.clone(),
            actor: Arc::downgrade(handle),
            goal,
            work_ready: presentation.work_ready,
        };
        next.goal.revision > self.goal.revision && self.continues(&next)
    }
    fn same_subject(&self, next: &Self) -> bool {
        use chat::goal::ReadState;
        self.target == next.target
            && self.session == next.session
            && self.actor.ptr_eq(&next.actor)
            && self.work_ready == next.work_ready
            && self.goal.thread_id == next.goal.thread_id
            && self.goal.support == next.goal.support
            && self.goal.pending == next.goal.pending
            && self.goal.uncertain == next.goal.uncertain
            && self.goal.last_result == next.goal.last_result
            && match (&self.goal.read, &next.goal.read) {
                (ReadState::Known(Some(a)), ReadState::Known(Some(b))) => {
                    a.thread_id == b.thread_id
                        && a.objective == b.objective
                        && a.status == b.status
                        && a.token_budget == b.token_budget
                        && a.created_at == b.created_at
                }
                _ => false,
            }
    }
    fn continues(&self, next: &Self) -> bool {
        use chat::goal::ReadState;
        self.same_subject(next)
            && if self.goal.revision == next.goal.revision {
                self.goal == next.goal
            } else if next.goal.revision > self.goal.revision {
                matches!((&self.goal.read, &next.goal.read), (ReadState::Known(Some(before)), ReadState::Known(Some(after)))
                if after.tokens_used >= before.tokens_used
                    && after.time_used_seconds >= before.time_used_seconds
                    && after.updated_at >= before.updated_at)
            } else {
                false
            }
    }
}

#[derive(Clone)]
struct Scope {
    source: Source,
    markers: [widget::Id; 4],
}
#[derive(Clone)]
struct Witness {
    scope: Scope,
    control: Control,
}
#[derive(Default)]
struct State {
    scope: Option<Scope>,
    witness: Option<Witness>,
}
/// One bounded native witness; observing focus needs no channel, task or thread.
#[derive(Default)]
pub(super) struct Controller {
    state: Arc<Mutex<State>>,
}
#[derive(Clone)]
pub(super) struct Controls {
    controller: Arc<Mutex<State>>,
    scope: Scope,
}
impl Controls {
    pub(super) fn marker(&self, index: usize) -> widget::Id {
        self.scope.markers[index].clone()
    }
    pub(super) fn observe(&self, row: usize, key: Option<[u8; 32]>) {
        let control = key
            .and_then(Control::from_key)
            .filter(|control| control.index() / 2 == row);
        let mut state = self.controller.lock().unwrap_or_else(|p| p.into_inner());
        let Some(current) = state.scope.as_ref() else {
            return;
        };
        if !current.source.same_subject(&self.scope.source)
            || current.source.goal != self.scope.source.goal
        {
            return;
        }
        // The unfocused second row must not clear the first row's witness.
        match control {
            Some(control) => {
                if state.witness.as_ref().is_none_or(|old| {
                    old.control != control || old.scope.source.goal != self.scope.source.goal
                }) {
                    state.witness = Some(Witness {
                        scope: self.scope.clone(),
                        control,
                    });
                }
            }
            None if state
                .witness
                .as_ref()
                .is_some_and(|old| old.control.index() / 2 == row) =>
            {
                state.witness = None;
            }
            None => {}
        }
    }
}
#[derive(Clone)]
pub(super) struct Ticket(Witness);
impl Controller {
    pub(super) fn clear(&self) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = State::default();
    }
    pub(super) fn controls(
        &self,
        runtime: &Runtime,
        presentation: goal::Presentation<'_>,
    ) -> Option<Controls> {
        let Some(source) = Source::capture(runtime, presentation) else {
            let accounting_lag = self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .witness
                .as_ref()
                .is_some_and(|witness| witness.scope.source.accounting_lag(runtime, presentation));
            if !accounting_lag {
                // Actual unavailable epochs retire delayed restores even if
                // this same human objective later becomes available again.
                self.clear();
            }
            // Keeping private focus evidence never enables stale controls or
            // mounts a destination for an already queued native operation.
            return None;
        };
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let markers = state
            .scope
            .as_ref()
            .filter(|old| old.source.continues(&source))
            .map(|old| old.markers.clone())
            .unwrap_or_else(|| {
                state.witness = None;
                let epoch = uuid::Uuid::new_v4();
                std::array::from_fn(|index| {
                    widget::Id::new(format!("goal-focus-scope-{epoch}-{index}"))
                })
            });
        let scope = Scope { source, markers };
        state.scope = Some(scope.clone());
        Some(Controls {
            controller: self.state.clone(),
            scope,
        })
    }
    pub(super) fn before_refresh(&self, runtime: &Runtime) -> Option<Ticket> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let witness = state.witness.as_ref()?;
        (runtime.goal_open
            && witness.scope.source.same_actor(runtime)
            && runtime.snapshot.goal.as_ref() == Some(&witness.scope.source.goal))
        .then(|| Ticket(witness.clone()))
    }
}

impl Cagents {
    pub(super) fn restore_goal_focus(&self, ticket: Ticket) -> Task<Message> {
        let Witness { scope, control } = ticket.0;
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&scope.source.target.card) else {
            return Task::none();
        };
        if self.workspace.focused() != Some(runtime.target.card)
            || !runtime.visible.load(Ordering::Relaxed)
            || !runtime.goal_open
            || self.workspace_inputs_blocked()
            || self.update_prepared
            || self.chat_ui.image_viewer.is_some()
            || self.pane_close_owned(runtime.target.card)
            || !scope.source.same_actor(runtime)
            || runtime.pending.is_some()
            || runtime.goal_pending.is_some()
            || runtime.selected_subagent.is_some()
            || self.chat_start_pending(runtime.target.card)
            || runtime.account_picker
            || runtime.model_picker.is_some()
            || runtime.reasoning_picker
            || runtime.permission_picker
            || runtime.footer_picker
            || runtime.composer_actions
            || runtime.branch_picker
            || runtime.child_picker.is_some()
            || !runtime
                .goal_controls()
                .contains(&control.id(runtime.target))
        {
            return Task::none();
        }
        let Some(handle) = runtime.handle() else {
            return Task::none();
        };
        let latest = handle.snapshot();
        let Some(goal) = runtime.snapshot.goal.as_ref() else {
            return Task::none();
        };
        if latest.goal.as_ref() != Some(goal) || goal.revision <= scope.source.goal.revision {
            return Task::none();
        }
        let next = Source {
            target: runtime.target,
            session: runtime.session.clone(),
            actor: Arc::downgrade(handle),
            goal: goal.clone(),
            work_ready: latest.ready && latest.read_only_reason.is_none(),
        };
        if !scope.source.continues(&next) {
            return Task::none();
        }
        restore(
            scope.markers[control.index()].clone(),
            control.id(runtime.target),
        )
    }
}

/// Query the whole window first (including anonymous focus), then prove the
/// exact enabled subject marker is mounted before a scoped focus operation.
fn restore(marker: widget::Id, target: widget::Id) -> Task<Message> {
    struct Foreign {
        marker: widget::Id,
        target: widget::Id,
        foreign: bool,
    }
    impl WidgetOperation<Message> for Foreign {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn WidgetOperation<Message>)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.foreign |= state.is_focused() && id != Some(&self.target);
        }
        fn finish(&self) -> Outcome<Message> {
            if self.foreign {
                Outcome::None
            } else {
                Outcome::Chain(Box::new(Mounted {
                    query: Box::new(operation::scoped(self.marker.clone(), focusable::count())),
                    marker: self.marker.clone(),
                    target: self.target.clone(),
                }))
            }
        }
    }
    struct Mounted {
        query: Box<dyn WidgetOperation<focusable::Count>>,
        marker: widget::Id,
        target: widget::Id,
    }
    impl WidgetOperation<Message> for Mounted {
        fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
            self.query.container(id, bounds);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.query.focusable(id, bounds, state);
        }
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn WidgetOperation<Message>)) {
            self.query
                .traverse(&mut |query| visit(&mut operation::black_box(query)));
        }
        fn finish(&self) -> Outcome<Message> {
            match self.query.finish() {
                Outcome::Some(count) if count.total == 1 => Outcome::Chain(Box::new(
                    operation::scoped(self.marker.clone(), focusable::focus(self.target.clone())),
                )),
                _ => Outcome::None,
            }
        }
    }
    iced::advanced::widget::operate(Foreign {
        marker,
        target,
        foreign: false,
    })
    .map(cosmic::Action::App)
}

#[cfg(test)]
mod tests;
