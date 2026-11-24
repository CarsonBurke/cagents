//! From the daemon's types to the mobile API's.
//!
//! Everything here is a pure function of what the daemon reported, so the
//! translation can be tested without one. The daemon's types change with its
//! protocol; the mobile API's must not, which is why every `match` below
//! names each daemon variant: a new one fails to compile here until someone
//! decides how a phone sees it.

use std::collections::HashMap;

use cagents_mobile_api as api;

use crate::{
    agent::{ActivityState, Provider},
    agent_execution::{State, directory},
    chat, notifications,
};

/// Tool output is clipped to its last this many bytes: the end of a build or
/// test run says how it went.
const OUTPUT_TAIL_BYTES: usize = 4 * 1024;
/// Tool input, such as a JSON argument object, is clipped to its start.
const INPUT_HEAD_BYTES: usize = 2 * 1024;
/// A delivered report is often the point of a conversation, so it keeps more.
const DELIVERY_HEAD_BYTES: usize = 32 * 1024;

pub fn session(member: &directory::Member) -> api::Session {
    let entry = &member.session;
    api::Session {
        id: entry.id.to_string(),
        lineage: entry.lineage.to_string(),
        surface: match entry.surface {
            directory::Surface::Chat => api::Surface::Chat,
            directory::Surface::Terminal => api::Surface::Terminal,
        },
        provider: entry.provider.map(|provider| provider_id(provider).into()),
        provider_label: entry.provider.map(|provider| provider.label().into()),
        title: entry.title.clone(),
        project: entry.project_name.clone(),
        directory: entry.directory.as_ref().map(|path| path_text(path)),
        activity: activity(entry.state),
        exited: member.exited,
        failure: member.failure.clone(),
        active_at: member.active_at,
        agent: entry.delegation.as_ref().map(|delegation| api::Agent {
            id: delegation.agent.to_string(),
            owner: match &delegation.owner {
                directory::Owner::User => api::AgentOwner::User,
                directory::Owner::Session(_) => api::AgentOwner::Session,
                directory::Owner::Automation(_) => api::AgentOwner::Automation,
            },
            owner_lineage: match &delegation.owner {
                directory::Owner::Session(lineage) => Some(lineage.to_string()),
                directory::Owner::User | directory::Owner::Automation(_) => None,
            },
            run: delegation.run_state.map(run_state),
        }),
        watchable: entry.surface == directory::Surface::Chat && !member.exited,
    }
}

/// A provider's ID in the mobile API, fixed here rather than borrowed from
/// an internal name that could change.
fn provider_id(provider: Provider) -> &'static str {
    match provider {
        Provider::Omp => "omp",
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Antigravity => "antigravity",
        Provider::Muse => "muse",
    }
}

fn path_text(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn activity(state: ActivityState) -> api::Activity {
    match state {
        ActivityState::Unknown => api::Activity::Unknown,
        ActivityState::Idle => api::Activity::Idle,
        ActivityState::Working => api::Activity::Working,
        // The mobile API names no kind of wait; the request itself says.
        ActivityState::Waiting | ActivityState::Approval | ActivityState::Question => {
            api::Activity::Waiting
        }
    }
}

fn run_state(state: State) -> api::RunState {
    match state {
        State::Accepted => api::RunState::Accepted,
        State::Running => api::RunState::Running,
        State::Waiting => api::RunState::Waiting,
        State::Succeeded => api::RunState::Succeeded,
        State::Failed => api::RunState::Failed,
        State::Cancelled => api::RunState::Cancelled,
        State::Interrupted => api::RunState::Interrupted,
    }
}

pub fn conversation_state(snapshot: &chat::Snapshot) -> api::ConversationState {
    api::ConversationState {
        title: snapshot.title.clone(),
        activity: activity(snapshot.activity()),
        busy: snapshot.busy,
        ready: snapshot.ready,
        exited: snapshot.exited,
        error: snapshot.error.clone(),
        read_only: snapshot.read_only_reason.clone(),
        has_earlier: snapshot.has_earlier,
        loading_earlier: snapshot.history_loading,
        model: snapshot.selected_model.clone(),
        requests: snapshot.requests.iter().map(request).collect(),
        queued: snapshot
            .queued_messages
            .iter()
            .map(|message| api::QueuedMessage {
                id: message.id.clone(),
                text: message.text.clone(),
            })
            .collect(),
        plan: snapshot
            .plan
            .iter()
            .map(|step| api::PlanStep {
                text: step.text.clone(),
                status: match step.status {
                    chat::PlanState::Pending => api::PlanStatus::Pending,
                    chat::PlanState::Active => api::PlanStatus::Active,
                    chat::PlanState::Done => api::PlanStatus::Done,
                },
            })
            .collect(),
        subagents: snapshot
            .subagents
            .iter()
            .map(|agent| api::Subagent {
                id: agent.id.clone(),
                kind: agent.kind.clone(),
                description: agent.description.clone(),
                status: tool_status(agent.status),
                waiting: agent.waiting,
            })
            .collect(),
    }
}

fn request(request: &chat::Request) -> api::PendingRequest {
    api::PendingRequest {
        id: request.id.clone(),
        kind: match request.kind {
            chat::RequestKind::Approval => api::RequestKind::Approval,
            chat::RequestKind::Question => api::RequestKind::Question,
            chat::RequestKind::Consent => api::RequestKind::Consent,
        },
        title: request.title.clone(),
        description: request.description.clone(),
        fields: request
            .fields
            .iter()
            .map(|field| api::RequestField {
                id: field.id.clone(),
                label: field.label.clone(),
                description: field.description.clone(),
                choices: field.choices.iter().map(choice).collect(),
                multiple: field.multiple,
                secret: field.secret,
                initial: field.initial.clone(),
            })
            .collect(),
        actions: request.actions.iter().map(choice).collect(),
    }
}

fn choice(choice: &chat::Choice) -> api::Choice {
    api::Choice {
        id: choice.id.clone(),
        label: choice.label.clone(),
        description: choice.description.clone(),
    }
}

fn tool_status(status: chat::ToolStatus) -> api::ToolStatus {
    match status {
        chat::ToolStatus::Running => api::ToolStatus::Running,
        chat::ToolStatus::Succeeded => api::ToolStatus::Succeeded,
        chat::ToolStatus::Failed => api::ToolStatus::Failed,
        chat::ToolStatus::Denied => api::ToolStatus::Denied,
        chat::ToolStatus::Stopped => api::ToolStatus::Stopped,
        chat::ToolStatus::Waiting => api::ToolStatus::Waiting,
        chat::ToolStatus::Ended => api::ToolStatus::Ended,
    }
}

pub fn item(item: &chat::Item) -> api::Item {
    let body = match &item.content {
        chat::ItemContent::Text {
            text,
            thinking,
            phase,
        } => api::ItemBody::Text {
            text: text.clone(),
            thinking: *thinking,
            commentary: match phase {
                chat::Phase::Final => false,
                chat::Phase::Commentary => true,
            },
        },
        chat::ItemContent::Command {
            label,
            command,
            working_directory: _,
            action,
            status,
            exit_code,
            duration_ms,
            output,
        } => {
            let (output, clipped) = tail(output, OUTPUT_TAIL_BYTES);
            api::ItemBody::Command {
                label: label.clone(),
                command: command.clone(),
                action: match action {
                    chat::CommandAction::Run => api::CommandAction::Run,
                    chat::CommandAction::Read => api::CommandAction::Read,
                    chat::CommandAction::Search => api::CommandAction::Search,
                    chat::CommandAction::List => api::CommandAction::List,
                },
                status: tool_status(*status),
                exit_code: *exit_code,
                duration_ms: *duration_ms,
                output: output.into(),
                clipped,
            }
        }
        chat::ItemContent::Changes { changes, status } => api::ItemBody::Changes {
            files: changes.iter().map(file_change).collect(),
            status: tool_status(*status),
        },
        chat::ItemContent::WebSearch { query, results } => api::ItemBody::WebSearch {
            query: query.clone(),
            results: results
                .iter()
                .map(|result| api::SearchResult {
                    title: result.title.clone(),
                    url: result.url.clone(),
                })
                .collect(),
        },
        chat::ItemContent::Tool {
            name,
            input,
            output,
            status,
        } => {
            let (input, input_clipped) = head(input, INPUT_HEAD_BYTES);
            let (output, output_clipped) = tail(output, OUTPUT_TAIL_BYTES);
            api::ItemBody::Tool {
                name: name.clone(),
                input: input.into(),
                output: output.into(),
                clipped: input_clipped || output_clipped,
                status: tool_status(*status),
            }
        }
        chat::ItemContent::Image {
            path: _,
            description,
            reference: _,
        } => api::ItemBody::Image {
            description: description.clone(),
        },
        chat::ItemContent::Delivery {
            kind,
            from,
            sender: _,
            summary,
            body,
        } => {
            let (body, clipped) = head(body, DELIVERY_HEAD_BYTES);
            api::ItemBody::Delivery {
                delivery: match kind {
                    chat::DeliveryKind::Report => api::DeliveryKind::Report,
                    chat::DeliveryKind::Message => api::DeliveryKind::Message,
                    chat::DeliveryKind::Notice => api::DeliveryKind::Notice,
                },
                from: from.clone(),
                summary: summary.clone(),
                body: body.into(),
                clipped,
            }
        }
    };
    api::Item {
        id: item.id.clone(),
        role: match item.role {
            chat::Role::User => api::Role::User,
            chat::Role::Assistant => api::Role::Assistant,
            chat::Role::System => api::Role::System,
        },
        body,
        started_at: item.started_at,
        at: item.at,
    }
}

fn file_change(change: &chat::FileChange) -> api::FileChange {
    let (mut added_lines, mut removed_lines) = (0, 0);
    for line in change.diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added_lines += 1;
        } else if line.starts_with('-') {
            removed_lines += 1;
        }
    }
    let (change_kind, from) = match &change.kind {
        chat::ChangeKind::Added => (api::ChangeKind::Added, None),
        chat::ChangeKind::Deleted => (api::ChangeKind::Deleted, None),
        chat::ChangeKind::Updated => (api::ChangeKind::Updated, None),
        chat::ChangeKind::Renamed { from } => (api::ChangeKind::Renamed, Some(from.clone())),
    };
    api::FileChange {
        path: change.path.clone(),
        change: change_kind,
        from,
        added_lines,
        removed_lines,
    }
}

/// The last `limit` bytes of `text`, cut at a character boundary.
fn tail(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (&text[start..], true)
}

/// The first `limit` bytes of `text`, cut at a character boundary.
fn head(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// Unacknowledged notices, newest first.
pub fn notices(snapshot: &notifications::Snapshot) -> Vec<api::Notice> {
    snapshot
        .notices
        .iter()
        .rev()
        .filter(|notice| !notice.acknowledged)
        .map(notice)
        .collect()
}

fn notice(notice: &notifications::Notice) -> api::Notice {
    use notifications::Kind;
    let event = &notice.event;
    let content = event.content.as_ref();
    let nonempty = |text: &str| (!text.is_empty()).then(|| text.to_owned());
    api::Notice {
        id: notice.id.to_string(),
        asks: notice.asks(),
        kind: match event.evidence.kind {
            Kind::Succeeded => api::NoticeKind::Succeeded,
            Kind::Failed => api::NoticeKind::Failed,
            Kind::Cancelled => api::NoticeKind::Cancelled,
            Kind::Waiting => api::NoticeKind::Waiting,
            Kind::ReviewRequested => api::NoticeKind::ReviewRequested,
            Kind::Interrupted => api::NoticeKind::Interrupted,
            Kind::MessageReceived => api::NoticeKind::MessageReceived,
            Kind::AutomationPaused => api::NoticeKind::AutomationPaused,
            Kind::AutomationBlocked => api::NoticeKind::AutomationBlocked,
        },
        title: event.evidence.kind.label().into(),
        session: match &event.target {
            notifications::Target::Session(id) => Some(id.to_string()),
            notifications::Target::PullRequest(_)
            | notifications::Target::AgentRun(_)
            | notifications::Target::SessionInbox(_)
            | notifications::Target::Automation { .. } => {
                event.source_session.map(|id| id.to_string())
            }
        },
        conversation: content.and_then(|content| nonempty(&content.conversation)),
        preview: content.and_then(|content| nonempty(&content.preview)),
        request: content
            .and_then(|content| content.request)
            .map(|request| match request {
                notifications::RequestType::Approval => api::RequestKind::Approval,
                notifications::RequestType::Question => api::RequestKind::Question,
                notifications::RequestType::Consent => api::RequestKind::Consent,
            }),
        provider: event.provider.map(|provider| provider_id(provider).into()),
    }
}

/// What a phone holds of one conversation, so that each publication becomes
/// the smallest update that brings it up to date.
#[derive(Debug, Default)]
pub struct Mirror {
    state: Option<api::ConversationState>,
    order: Vec<String>,
    items: HashMap<String, api::Item>,
}

impl Mirror {
    /// The update that takes the phone from what it holds to `snapshot`.
    /// `reset` sends everything, as the first update of a watch must.
    /// `None` when nothing the phone shows changed.
    pub fn update(
        &mut self,
        session: &str,
        snapshot: &chat::Snapshot,
        reset: bool,
    ) -> Option<api::ConversationUpdate> {
        let state = conversation_state(snapshot);
        let order: Vec<String> = snapshot.items.iter().map(|item| item.id.clone()).collect();
        let mut items = Vec::new();
        let mut appends = Vec::new();
        let mut next = HashMap::with_capacity(snapshot.items.len());
        for current in &snapshot.items {
            let current = item(current);
            if !reset && let Some(held) = self.items.get(&current.id) {
                if *held != current {
                    match appended(held, &current) {
                        Some(append) => appends.push(append),
                        None => items.push(current.clone()),
                    }
                }
            } else {
                items.push(current.clone());
            }
            next.insert(current.id.clone(), current);
        }
        let order_changed = reset || order != self.order;
        self.items = next;
        if !order_changed
            && items.is_empty()
            && appends.is_empty()
            && self.state.as_ref() == Some(&state)
        {
            return None;
        }
        if order_changed {
            self.order = order.clone();
        }
        self.state = Some(state.clone());
        Some(api::ConversationUpdate {
            session: session.into(),
            reset,
            state,
            order: order_changed.then_some(order),
            items,
            appends,
        })
    }
}

/// `current` as text appended to `held`, when that is all that changed
/// besides its time.
fn appended(held: &api::Item, current: &api::Item) -> Option<api::TextAppend> {
    let (
        api::ItemBody::Text {
            text: held_text,
            thinking: held_thinking,
            commentary: held_commentary,
        },
        api::ItemBody::Text {
            text,
            thinking,
            commentary,
        },
    ) = (&held.body, &current.body)
    else {
        return None;
    };
    let suffix = text.strip_prefix(held_text.as_str())?;
    (held.role == current.role
        && held.started_at == current.started_at
        && held_thinking == thinking
        && held_commentary == commentary)
        .then(|| api::TextAppend {
            id: current.id.clone(),
            offset: held_text.encode_utf16().count() as u64,
            text: suffix.into(),
            at: current.at,
        })
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
#[path = "translate_tests.rs"]
mod tests;
