//! Pure snapshot projection shared by the resident actor and view regressions.
use super::{child_diagnostics, child_text_copy, observed_at};
use crate::chat::{Event, Item, ItemContent, Phase, Snapshot, Subagent, ToolStatus};
use std::{collections::BTreeMap, sync::Arc};

pub(super) fn apply_projected_event(
    state: &mut Snapshot,
    indices: &mut BTreeMap<String, usize>,
    event: Event,
    preview_observed: bool,
) {
    // Frame events have already reached the resident preview before Store.
    // Do not consume a restored start boundary or append a delta twice.
    if !preview_observed {
        Arc::make_mut(&mut state.notification_preview).observe(&event);
    }
    let child_copy_update = child_text_copy::prepare(state, &event);
    let latest_history = matches!(&event, Event::LatestHistory { .. });
    let child_changed = matches!(
        &event,
        Event::Subagent { .. }
            | Event::SubagentItem { .. }
            | Event::SubagentTextDelta { .. }
            | Event::SubagentOutputDelta { .. }
            | Event::SubagentItemsRemoved { .. }
    );
    match event {
        Event::OutputDelta { id, offset, delta } => {
            if let Some(&index) = indices.get(&id) {
                let item = Arc::make_mut(&mut state.items[index]);
                let body = match &mut item.content {
                    ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => {
                        Some(output)
                    }
                    _ => None,
                };
                if let Some(body) = body.filter(|body| body.len() == offset) {
                    body.push_str(&delta);
                    let at = observed_at();
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                }
            }
        }
        Event::TurnStarted => {
            state.prompt_suggestion = None;
            state.turn_generation = state.turn_generation.wrapping_add(1);
            state.finished_at = None;
        }
        Event::TurnSettled | Event::TurnFinished { .. } | Event::Execution { .. } => {}
        Event::ItemsRemoved { ids } => {
            let removed: std::collections::BTreeSet<_> = ids.into_iter().collect();
            state.items.retain(|item| !removed.contains(&item.id));
            indices.clear();
            indices.extend(
                state
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (item.id.clone(), index)),
            );
        }
        Event::QueueChanged(messages) => state.queued_messages = messages,
        Event::QueueRemoval(removal) => {
            state.queue_removals.push(removal);
            let excess = state
                .queue_removals
                .len()
                .saturating_sub(crate::chat::QueueRemoval::LIMIT);
            state.queue_removals.drain(..excess);
        }
        Event::HistoryLoading(loading) => state.history_loading = loading,
        Event::LatestHistoryStarted => state.history_loading = true,
        Event::HistoryError(message) => state.error = Some(message),
        Event::HistoryRecovered => {}
        Event::InterruptMode(mode) => state.interrupt_mode = Some(mode),
        Event::BranchPoints(points) => state.branch_points = points,
        Event::Forked(id) => state.forked = Some(id),
        Event::Reset { reason } => {
            state.finished_at = None;
            state.prompt_suggestion = None;
            state.branch_points.clear();
            state.forked = None;
            state.interrupt_mode = None;
            state.queued_messages.clear();
            state.queue_removals.clear();
            state.items.clear();
            state.subagents.clear();
            indices.clear();
            state.requests.clear();
            state.usage = Default::default();
            state.session_id = None;
            state.title = None;
            state.has_earlier = false;
            state.has_later = false;
            state.history_loading = false;
            state.detail = reason;
            state.busy = false;
            state.waiting = false;
            state.error = None;
            state.read_only_reason = None;
            state.plan.clear();
            state.permission_mode = None;
            state.permissions = None;
            state.permission_denials.clear();
        }
        Event::Session { id, title } => {
            if state.session_id.as_deref() != Some(id.as_str()) {
                state.prompt_suggestion = None;
                state.title = title;
            } else if title.is_some() {
                // Identity-only frames carry no new naming information.
                state.title = title;
            }
            state.session_id = Some(id);
        }
        Event::Plan { steps } => state.plan = steps,
        // About the account rather than the conversation: `step` forwards it
        // to the surfaces before the store, so it never reaches a snapshot.
        Event::AccountUsage(_)
        | Event::AgentAddress(_)
        | Event::LoginRecovered { .. }
        | Event::LoginVerificationFailed { .. } => {}
        Event::Capabilities(value) => state.capabilities = value,
        Event::PermissionMode(mode) => state.permission_mode = mode,
        Event::PermissionSettings(settings) => state.permissions = Some(settings),
        Event::PermissionDenied(mut denial) => {
            if let Some(previous) = state
                .permission_denials
                .iter_mut()
                .find(|old| old.tool_use_id == denial.tool_use_id)
            {
                if denial.tool_input.is_none() {
                    denial.tool_input = previous.tool_input.take();
                }
                if denial.reason.is_empty() {
                    denial.reason = std::mem::take(&mut previous.reason);
                }
                if denial.reason_type.is_none() {
                    denial.reason_type = previous.reason_type.take();
                }
                if denial.agent_id.is_none() {
                    denial.agent_id = previous.agent_id.take();
                }
                *previous = denial;
            } else {
                if state.permission_denials.len() >= crate::chat::PermissionDenial::LIMIT {
                    state.permission_denials.remove(0);
                }
                state.permission_denials.push(denial);
            }
        }
        Event::PermissionDenialDismissed { tool_use_id } => state
            .permission_denials
            .retain(|denial| denial.tool_use_id != tool_use_id),
        Event::Models {
            models,
            selected,
            reasoning,
        } => {
            if !models.is_empty() {
                state.models = models;
            }
            if selected.is_some() {
                state.selected_model = selected;
                state.reasoning = reasoning;
            } else if reasoning.is_some() {
                state.reasoning = reasoning;
            }
        }
        Event::ActiveModel(model) => state.active_model = model,
        Event::FastMode(value) => state.fast_mode = Some(value),
        Event::Commands(value) => state.commands = value,
        Event::Skills(value) => state.skills = value,
        Event::Item(mut item) => {
            // The adapters do not know wall-clock time, so the item is stamped
            // here, where the daemon actually observes the change.
            item.at = observed_at();
            item.started_at = item.at;
            if let Some(&index) = indices.get(&item.id) {
                item.started_at = state.items[index]
                    .started_at
                    .or(state.items[index].at)
                    .or(item.at);
                if let (
                    ItemContent::Tool { name, input, .. },
                    ItemContent::Tool {
                        name: old_name,
                        input: old_input,
                        ..
                    },
                ) = (&mut item.content, &state.items[index].content)
                {
                    if name.is_empty() {
                        name.clone_from(old_name);
                    }
                    if input.is_empty() {
                        input.clone_from(old_input);
                    }
                }
                state.items[index] = Arc::new(item);
            } else {
                indices.insert(item.id.clone(), state.items.len());
                state.items.push(Arc::new(item));
            }
        }
        Event::Subagent {
            id,
            kind,
            description,
            status,
            background,
            model,
            waiting,
            detail,
        } => {
            let agent = subagent(state, &id);
            if let Some(background) = background {
                agent.background = background;
            }
            if !kind.is_empty() {
                agent.kind = kind;
            }
            if !description.is_empty() {
                agent.description = description;
            }
            if let Some(model) = model {
                agent.model = Some(model);
            }
            if let Some(waiting) = waiting {
                agent.waiting = waiting;
            }
            let has_detail = detail.is_some();
            if let Some(detail) = detail {
                agent.detail = detail;
            }
            if let Some(status) = status {
                agent.status = status;
                if status != ToolStatus::Running {
                    agent.waiting = false;
                    if !has_detail {
                        agent.detail.clear();
                    }
                }
            }
        }
        Event::SubagentItem { id, mut item } => {
            item.at = observed_at();
            item.started_at = item.at;
            let agent = subagent(state, &id);
            if let Some(old) = agent.items.iter_mut().find(|old| old.id == item.id) {
                item.started_at = old.started_at.or(old.at).or(item.at);
                *old = Arc::new(item);
            } else {
                agent.items.push(Arc::new(item));
            }
            trim_subagent(agent);
        }
        Event::SubagentTextDelta {
            id,
            item_id,
            role,
            thinking,
            delta,
        } => {
            let agent = subagent(state, &id);
            if let Some(item) = agent.items.iter_mut().find(|item| item.id == item_id) {
                let item = Arc::make_mut(item);
                if let ItemContent::Text { text, .. } = &mut item.content {
                    text.push_str(&delta);
                    let at = observed_at();
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                }
            } else {
                let at = observed_at();
                agent.items.push(Arc::new(Item {
                    id: item_id,
                    role,
                    content: ItemContent::Text {
                        text: delta,
                        thinking,
                        phase: Phase::Final,
                    },
                    started_at: at,
                    at,
                }));
            }
            trim_subagent(agent);
        }
        Event::SubagentOutputDelta {
            id,
            item_id,
            offset,
            delta,
        } => {
            let agent = subagent(state, &id);
            if let Some(item) = agent.items.iter_mut().find(|item| item.id == item_id) {
                let item = Arc::make_mut(item);
                if let ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } =
                    &mut item.content
                    && output.len() == offset
                {
                    output.push_str(&delta);
                    let at = observed_at();
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                }
            }
            trim_subagent(agent);
        }
        Event::SubagentItemsRemoved { id, ids } => {
            if let Some(agent) = state.subagents.iter_mut().find(|agent| agent.id == id) {
                agent.items.retain(|item| !ids.contains(&item.id));
            }
        }
        Event::History { items, has_earlier } | Event::LatestHistory { items, has_earlier } => {
            state.history_loading = false;
            let mut run = Vec::new();
            let mut after = None;
            let mut seen = std::collections::BTreeSet::new();
            for item in items {
                if !seen.insert(item.id.clone()) {
                    continue;
                }
                if let Some(index) = state.items.iter().position(|cached| cached.id == item.id) {
                    state
                        .items
                        .splice(index..index, run.drain(..).map(Arc::new));
                    after = Some(item.id);
                } else {
                    run.push(item);
                }
            }
            let insertion = after
                .and_then(|id| {
                    state
                        .items
                        .iter()
                        .position(|item| item.id == id)
                        .map(|index| index + 1)
                })
                .unwrap_or(if latest_history { state.items.len() } else { 0 });
            state
                .items
                .splice(insertion..insertion, run.into_iter().map(Arc::new));
            indices.clear();
            indices.extend(
                state
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (item.id.clone(), index)),
            );
            state.has_earlier = has_earlier;
        }
        Event::TextDelta {
            id,
            role,
            thinking,
            delta,
        } => {
            let at = observed_at();
            if let Some(&index) = indices.get(&id) {
                let item = Arc::make_mut(&mut state.items[index]);
                if let ItemContent::Text { text, .. } = &mut item.content {
                    text.push_str(&delta);
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                } else {
                    state.error = Some(format!(
                        "Provider reused non-text item ID {id} for a text delta"
                    ));
                }
            } else {
                indices.insert(id.clone(), state.items.len());
                state.items.push(Arc::new(Item {
                    id,
                    role,
                    // The delta stream carries no phase, so the item created to
                    // receive the text declares the default instead of guessing
                    // that an unfinished message is narration.
                    content: ItemContent::Text {
                        text: delta,
                        thinking,
                        phase: Phase::Final,
                    },
                    started_at: at,
                    at,
                }));
            }
        }
        Event::Request(request) => {
            if let Some(old) = state.requests.iter_mut().find(|r| r.id == request.id) {
                *old = request;
            } else {
                state.requests.push(request);
            }
        }
        Event::RequestResolved { id } => state.requests.retain(|request| request.id != id),
        Event::Waiting(waiting) => state.waiting = waiting,
        Event::Status { busy, detail } => {
            if busy {
                state.prompt_suggestion = None;
            }
            state.busy = busy;
            state.detail = detail;
        }
        Event::Usage(value) => {
            if value.input_tokens != 0 {
                state.usage.input_tokens = value.input_tokens;
            }
            if value.output_tokens != 0 {
                state.usage.output_tokens = value.output_tokens;
            }
            if value.context_tokens != 0 {
                state.usage.context_tokens = value.context_tokens;
            }
            if value.context_limit != 0 {
                state.usage.context_limit = value.context_limit;
            }
            if value.cost_microusd.is_some() {
                state.usage.cost_microusd = value.cost_microusd;
            }
            if value.cost_estimate.is_some() {
                state.usage.cost_estimate = value.cost_estimate;
            }
        }
        Event::PromptSuggestion {
            session_id,
            source,
            text,
        } => {
            if !state.busy
                && !state.exited
                && state.session_id.as_deref() == Some(session_id.as_str())
                && crate::chat::PromptSuggestion::valid_text(&text)
            {
                state.prompt_suggestion = Some(crate::chat::PromptSuggestion {
                    session_id,
                    source,
                    turn_generation: state.turn_generation,
                    text,
                });
            }
        }
        Event::PromptSuggestionCleared => state.prompt_suggestion = None,
        Event::Composer { text } => state.composer = Some(text),
        Event::OpenUrl { url, instructions } => state.open_url = Some((url, instructions)),
        Event::Error { message, fatal } => {
            state.error = Some(message);
            if fatal {
                state.prompt_suggestion = None;
                state.busy = false;
                state.history_loading = false;
            }
        }
        Event::Exited { .. } => {
            state.prompt_suggestion = None;
            state.exited = true;
            for agent in &mut state.subagents {
                if agent.status == ToolStatus::Waiting {
                    agent.status = ToolStatus::Stopped;
                    agent.detail =
                        "The provider exited before its background work reported back".to_owned();
                }
            }
            state.waiting = false;
            state.busy = false;
            state.ready = false;
            state.history_loading = false;
        }
        Event::ReadOnlyHistory { reason } => state.read_only_reason = Some(reason),
        Event::Ready(ready) => {
            if !ready {
                state.prompt_suggestion = None;
            }
            state.ready = ready;
        }
    }
    if child_changed {
        trim_subagents(state);
    }
    child_text_copy::finish(state, child_copy_update);
}

/// Marks the steps still running or waiting as stopped, copying only those
/// items. Nothing a provider that ended was doing outlives it; a step it left
/// going would otherwise read as still working. A subagent waiting on its
/// background work is ended by [`Event::Exited`], which says why.
#[cfg(feature = "host")]
pub(super) fn stop_snapshot_steps(state: &mut Snapshot) {
    stop_running_steps(&mut state.items);
    for agent in &mut state.subagents {
        if agent.status == ToolStatus::Running {
            agent.status = ToolStatus::Stopped;
            agent.waiting = false;
        }
        stop_running_steps(&mut agent.items);
    }
}

#[cfg(feature = "host")]
fn stop_running_steps(items: &mut [Arc<Item>]) {
    let now = observed_at();
    for item in items {
        if item.content.status().is_some_and(ToolStatus::is_live) {
            let item = Arc::make_mut(item);
            if let Some(status) = item.content.status_mut() {
                *status = ToolStatus::Stopped;
                // It ended now, so its duration runs to the provider's exit.
                item.started_at = item.started_at.or(item.at);
                item.at = now;
            }
        }
    }
}

/// Running task identities authorize interruption and idle waits. Retiring
/// presentation history must never retire that ownership evidence. Keep only
/// the newest 32 rich child transcripts, with older active entries remaining
/// available for status and selective Stop.
fn trim_subagents(state: &mut Snapshot) {
    const RICH_HISTORY: usize = 32;
    while state.subagents.len() > RICH_HISTORY {
        let Some(finished) = state
            .subagents
            .iter()
            .position(|agent| !agent.status.is_live())
        else {
            break;
        };
        state.subagents.remove(finished);
    }
    for agent in state.subagents.iter_mut().rev().skip(RICH_HISTORY) {
        agent.items.clear();
    }
}

pub(super) fn subagent<'a>(state: &'a mut Snapshot, id: &str) -> &'a mut Subagent {
    if let Some(index) = state.subagents.iter().position(|agent| agent.id == id) {
        return &mut state.subagents[index];
    }
    state.subagents.push(Subagent {
        model: None,
        waiting: false,
        detail: String::new(),
        id: id.into(),
        kind: String::new(),
        description: String::new(),
        status: ToolStatus::Running,
        background: false,
        items: Vec::new(),
    });
    state.subagents.last_mut().expect("subagent was inserted")
}

fn trim_subagent(agent: &mut Subagent) {
    const MAX_BYTES: usize = 64 * 1024;
    const MAX_ITEMS: usize = 64;
    if let Some(last) = agent.items.last_mut()
        && last.content.byte_len() > MAX_BYTES
        && child_diagnostics::marked(last)
    {
        child_diagnostics::project(Arc::make_mut(last));
    }
    if let Some(last) = agent.items.last_mut()
        && last.content.byte_len() > MAX_BYTES
    {
        Arc::make_mut(last).content = ItemContent::Text {
            text: "[This large item is available in the provider transcript.]".into(),
            thinking: false,
            phase: Phase::Commentary,
        };
    }
    let mut bytes = agent
        .items
        .iter()
        .map(|item| item.content.byte_len())
        .sum::<usize>();
    while agent.items.len() > MAX_ITEMS || bytes > MAX_BYTES {
        let removed = agent.items.remove(0);
        bytes = bytes.saturating_sub(removed.content.byte_len());
    }
}
