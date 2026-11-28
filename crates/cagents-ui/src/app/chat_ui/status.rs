//! On-demand status shared by a conversation's transcript, tab and inspector.

use super::*;
use crate::agent::ActivityState;
use std::sync::Weak;

type Label = (Option<Icon>, String);

#[derive(Clone, Copy)]
enum Scope {
    Own,
    Children,
    ChildrenWithFailures,
}

/// The fields that can change the wording or glyph. Keep the key independent
/// of transcript revisions: a new chunk of a reply can still say Responding.
#[derive(PartialEq, Eq)]
struct State {
    provider: Provider,
    closed: Option<bool>,
    starting: bool,
    reconnecting: bool,
    choosing_attachments: bool,
    sending: bool,
    disconnected: bool,
    exited: bool,
    owns_listener: bool,
    wait: Option<ActivityState>,
    busy: bool,
    activity: &'static str,
    finished: Option<&'static str>,
    draft: bool,
    ready: bool,
    children: Children,
}

/// Text is compared by value and copied only when the inputs change.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Text<T> {
    error: Option<T>,
    provider_error: Option<T>,
    detail: T,
    resets: Option<T>,
    failed_task: Option<T>,
}

impl<T> Text<T> {
    fn map<U>(self, mut f: impl FnMut(T) -> U) -> Text<U> {
        Text {
            error: self.error.map(&mut f),
            provider_error: self.provider_error.map(&mut f),
            detail: f(self.detail),
            resets: self.resets.map(&mut f),
            failed_task: self.failed_task.map(f),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Children {
    pub(super) working: usize,
    pub(super) waiting: usize,
    pub(super) on_background_work: usize,
    pub(super) failed: usize,
}

#[derive(Default)]
struct SnapshotFacts {
    wait: Option<ActivityState>,
    children: Children,
    first_failed: Option<usize>,
}

impl SnapshotFacts {
    fn read(snapshot: &Snapshot) -> Self {
        let mut facts = Self {
            wait: (snapshot.waiting || !snapshot.requests.is_empty()).then(|| snapshot.activity()),
            ..Self::default()
        };
        for (index, agent) in snapshot.subagents.iter().enumerate() {
            match agent.status {
                chat::ToolStatus::Running if agent.is_running() && agent.waiting => {
                    facts.children.waiting += 1;
                }
                chat::ToolStatus::Running if agent.is_running() => facts.children.working += 1,
                chat::ToolStatus::Waiting => facts.children.on_background_work += 1,
                chat::ToolStatus::Failed => {
                    facts.children.failed += 1;
                    facts.first_failed.get_or_insert(index);
                }
                _ => {}
            }
        }
        facts
    }
}

#[derive(Default)]
pub(super) struct Memo {
    source: Weak<Snapshot>,
    facts: SnapshotFacts,
    key: Option<(State, Text<String>)>,
    labels: [[Option<Label>; 3]; 2],
    #[cfg(test)]
    pub(super) builds: usize,
}

impl Memo {
    fn snapshot_facts(
        &mut self,
        snapshot: &Arc<Snapshot>,
    ) -> (Option<ActivityState>, Children, Option<usize>) {
        // A weak reference pins the identity without owning the transcript.
        // Arc::make_mut dissociates it even if the runtime is the sole owner.
        if self.source.as_ptr() != Arc::as_ptr(snapshot) {
            self.facts = SnapshotFacts::read(snapshot);
            self.source = Arc::downgrade(snapshot);
        }
        (
            self.facts.wait,
            self.facts.children,
            self.facts.first_failed,
        )
    }

    fn label(
        &mut self,
        state: State,
        text: Text<&str>,
        transitioning: bool,
        scope: Scope,
    ) -> Label {
        if !self.key.as_ref().is_some_and(|(old, strings)| {
            old == &state
                && strings.error.as_deref() == text.error
                && strings.provider_error.as_deref() == text.provider_error
                && strings.detail == text.detail
                && strings.resets.as_deref() == text.resets
                && strings.failed_task.as_deref() == text.failed_task
        }) {
            self.key = Some((state, text.map(str::to_owned)));
            self.labels = Default::default();
        }
        let slot = &mut self.labels[usize::from(transitioning)][scope as usize];
        slot.get_or_insert_with(|| {
            #[cfg(test)]
            {
                self.builds += 1;
            }
            let (state, _) = self.key.as_ref().unwrap();
            state.label(text, transitioning, scope)
        })
        .clone()
    }
}

impl Runtime {
    pub(super) fn status_children(&self) -> Children {
        self.status.borrow_mut().snapshot_facts(&self.snapshot).1
    }

    pub(super) fn status_is_working(&self) -> bool {
        self.snapshot.busy || {
            let children = self.status_children();
            children.working > 0 || children.waiting > 0
        }
    }

    /// Completion requires provider evidence. An unstarted conversation is a
    /// draft; an idle attached one is ready, while an attachment is connecting.
    pub(super) fn conversation_status(&self, transitioning: bool) -> Label {
        self.conversation_status_with_failures(transitioning, true)
    }

    pub(super) fn conversation_status_with_failures(
        &self,
        transitioning: bool,
        include_failures: bool,
    ) -> Label {
        let scope = if include_failures {
            Scope::ChildrenWithFailures
        } else {
            Scope::Children
        };
        self.status_label(transitioning, scope)
    }

    /// The conversation's status for surfaces that list delegated work
    /// separately.
    pub(super) fn own_conversation_status(&self) -> Label {
        self.status_label(false, Scope::Own)
    }

    fn status_label(&self, transitioning: bool, scope: Scope) -> Label {
        let mut memo = self.status.borrow_mut();
        let (state, text) = self.status_inputs(&mut memo);
        memo.label(state, text, transitioning, scope)
    }

    #[cfg(test)]
    pub(super) fn conversation_status_base(&self, transitioning: bool) -> (Option<Icon>, &str) {
        let (state, text) = self.status_inputs(&mut self.status.borrow_mut());
        state.base(text, transitioning)
    }

    fn status_inputs(&self, memo: &mut Memo) -> (State, Text<&str>) {
        let snapshot = &self.snapshot;
        let (wait, children, first_failed) = memo.snapshot_facts(snapshot);
        let working = snapshot.busy || children.working + children.waiting > 0;
        let choosing_attachments = self.attachment_chooser_pending.is_some();
        let state = State {
            provider: self.session.launch.provider,
            closed: self
                .closed_elsewhere
                .then(|| self.has_unsent_text() || !self.attachments.is_empty()),
            starting: self.queued.is_some(),
            reconnecting: self.disconnected && self.reattaching && !choosing_attachments,
            choosing_attachments,
            sending: self.on_its_way() && !(working && self.queue_sending()),
            disconnected: self.disconnected,
            exited: snapshot.exited,
            owns_listener: snapshot.exited && self.session.owns_codex_listener(),
            wait,
            busy: snapshot.busy,
            activity: view::activity_label(snapshot),
            finished: snapshot.finished_at.map(|at| {
                self.finished_stamp
                    .as_ref()
                    .filter(|stamp| stamp.at == at)
                    .map_or_else(|| view::turn_outcome(snapshot), |stamp| stamp.outcome)
            }),
            draft: self.is_draft(),
            ready: snapshot.ready,
            children,
        };
        let text = Text {
            error: diagnostic_key(self.error.as_deref()),
            provider_error: diagnostic_key(snapshot.error.as_deref()),
            detail: snapshot.detail.as_str(),
            resets: self
                .usage_limit
                .as_ref()
                .and_then(|limit| limit.reset.as_deref()),
            failed_task: first_failed
                .filter(|_| children.failed == 1)
                .map(|index| Self::agent_failure_task(&snapshot.subagents[index])),
        };
        (state, text)
    }
}

/// Status only reads the kind from Failure::parse, which classifies the first
/// 8,192 characters. Keep enough bytes for those characters even in UTF-8, and
/// check notice framing on the entire message before bounding the key.
fn diagnostic_key(message: Option<&str>) -> Option<&str> {
    const MAX_BYTES: usize = 8_192 * 4;
    message
        .filter(|message| !chat::failure::is_notice(message))
        .map(|message| &message[..message.floor_char_boundary(MAX_BYTES)])
}

impl<'a> Text<&'a str> {
    fn reported_errors(self) -> impl Iterator<Item = &'a str> {
        self.error.into_iter().chain(self.provider_error)
    }
}

impl State {
    fn base<'a>(&self, text: Text<&'a str>, transitioning: bool) -> (Option<Icon>, &'a str) {
        if let Some(retained) = self.closed {
            (
                Some(Icon::Close),
                if retained {
                    "Closed · draft retained"
                } else {
                    "Closed"
                },
            )
        } else if self.starting {
            (Some(Icon::AgentWorking), "Starting")
        } else if self.reconnecting {
            (Some(Icon::AgentWorking), "Reconnecting")
        } else if transitioning {
            (Some(Icon::AgentWorking), "Switching session")
        } else if self.choosing_attachments {
            (Some(Icon::AgentWorking), "Choosing attachments")
        } else if self.sending {
            (Some(Icon::AgentWorking), "Sending")
        } else if self.disconnected {
            (Some(Icon::Warning), "Disconnected")
        } else if self.exited && self.owns_listener {
            (Some(Icon::Warning), "Unavailable")
        } else if self.exited {
            (Some(Icon::Warning), "Exited")
        } else if let Some(wait) = self.wait {
            match wait {
                ActivityState::Approval => (Some(Icon::Permission), "Waiting for your permission"),
                ActivityState::Question => (Some(Icon::Question), "Waiting for your answer"),
                _ => (Some(Icon::AgentWaiting), "Waiting for you"),
            }
        } else if !self.busy && self.children.waiting > 0 && self.children.working == 0 {
            (Some(Icon::AgentWaiting), "Subagents waiting")
        } else if self.busy && text.detail == chat::TURN_STOPPING {
            (Some(Icon::AgentWorking), chat::TURN_STOPPING)
        } else if self.busy && text.detail == chat::WAITING_ON_BACKGROUND_WORK {
            (
                Some(Icon::AgentBackground),
                chat::WAITING_ON_BACKGROUND_WORK,
            )
        } else if self.busy || self.children.working > 0 {
            (
                Some(Icon::AgentWorking),
                if !self.busy {
                    "Subagents working"
                } else if self.activity != "Working" || text.detail.is_empty() {
                    self.activity
                } else {
                    text.detail
                },
            )
        } else if let Some(outcome) = self.finished {
            let label = if outcome == "Finished" && text.reported_errors().next().is_some() {
                "Finished with error"
            } else {
                outcome
            };
            (
                Some(match label {
                    view::TURN_STOPPED => Icon::Ended,
                    "Finished" => Icon::Check,
                    _ => Icon::Error,
                }),
                label,
            )
        } else if text.reported_errors().next().is_some() {
            (Some(Icon::Error), "Error")
        } else if self.draft {
            (None, "Draft")
        } else if self.ready {
            (None, "Ready")
        } else {
            (Some(Icon::AgentWorking), "Connecting")
        }
    }

    fn label(&self, text: Text<&str>, transitioning: bool, scope: Scope) -> Label {
        let (glyph, label) = self.base(text, transitioning);
        if self.closed.is_some()
            || self.starting
            || transitioning
            || self.choosing_attachments
            || self.sending
            || self.disconnected
            || self.exited
        {
            return (glyph, label.to_owned());
        }
        let Children {
            working,
            waiting,
            on_background_work,
            failed,
        } = if matches!(scope, Scope::Own) {
            Children::default()
        } else {
            self.children
        };
        let active = text
            .reported_errors()
            .find_map(|message| chat::failure::Failure::parse(self.provider, message));
        // The reset is when the conversation can carry on, so a limit that
        // still holds, as the daemon judges it, says it.
        let limit_reset = text
            .resets
            .filter(|_| {
                active
                    .as_ref()
                    .is_some_and(|failure| failure.kind == chat::failure::FailureKind::AccountLimit)
            })
            .map(|reset| format!("Account limit reached · resets {reset}"));
        let failure_label = active.as_ref().and_then(|failure| match failure.kind {
            chat::failure::FailureKind::AccountLimit => Some("Account limit reached"),
            chat::failure::FailureKind::Authentication => Some("Sign-in required"),
            chat::failure::FailureKind::NoAllowance => Some("No usage allowance"),
            chat::failure::FailureKind::Capacity => Some("Provider at capacity"),
            chat::failure::FailureKind::Connection => Some("Connection interrupted"),
            chat::failure::FailureKind::Interrupted => Some("Interrupted"),
            chat::failure::FailureKind::OutOfMemory => Some("Stopped: out of memory"),
            // The agent carried on; its notice is in the transcript.
            chat::failure::FailureKind::CommandOutOfMemory => None,
            chat::failure::FailureKind::Exited => Some("Stopped unexpectedly"),
            chat::failure::FailureKind::Unresponsive => Some("Not responding"),
        });
        let mut parts = Vec::new();
        if let Some(label) = limit_reset.as_deref().or(failure_label) {
            parts.push(label.to_owned());
        } else if self.busy
            || self.wait.is_some()
            || (working == 0 && waiting == 0 && on_background_work == 0 && failed == 0)
        {
            parts.push(label.to_owned());
        }
        let mut agent_parts = Vec::new();
        if working > 0 {
            agent_parts.push(format!(
                "{working} {} working",
                if working == 1 {
                    "subagent"
                } else {
                    "subagents"
                }
            ));
        }
        if waiting > 0 {
            agent_parts.push(if agent_parts.is_empty() {
                format!(
                    "{waiting} {} waiting",
                    if waiting == 1 {
                        "subagent"
                    } else {
                        "subagents"
                    }
                )
            } else {
                format!("{waiting} waiting")
            });
        }
        if on_background_work > 0 {
            agent_parts.push(if agent_parts.is_empty() {
                format!(
                    "{on_background_work} {} waiting on background work",
                    if on_background_work == 1 {
                        "subagent"
                    } else {
                        "subagents"
                    }
                )
            } else {
                format!("{on_background_work} waiting on background work")
            });
        }
        parts.extend(agent_parts);
        if matches!(scope, Scope::ChildrenWithFailures) && failed > 0 {
            parts.push(match text.failed_task {
                Some(task) => format!("1 failed: {task}"),
                None => format!("{failed} failed"),
            });
        }
        if working + waiting + on_background_work + failed > 0 || failure_label.is_some() {
            return (
                if working > 0 || self.busy {
                    Some(Icon::AgentWorking)
                } else if active
                    .as_ref()
                    .is_some_and(|failure| failure.kind == chat::failure::FailureKind::AccountLimit)
                {
                    // A usage limit waits on a reset or another account,
                    // not on a fix: the limit's own mark, not a failure's.
                    Some(Icon::UsageLimit)
                } else if failure_label.is_some() || failed > 0 {
                    Some(Icon::Error)
                } else if waiting > 0 {
                    // The conversation's own wait says what it waits for;
                    // a subagent's alone is unnamed.
                    glyph
                        .filter(|glyph| {
                            crate::sidebar::AttentionState::of_glyph(*glyph)
                                .is_some_and(crate::sidebar::AttentionState::waits_on_user)
                        })
                        .or(Some(Icon::AgentWaiting))
                } else {
                    None
                },
                parts.join(" · "),
            );
        }
        (glyph, parts.join(" · "))
    }
}
