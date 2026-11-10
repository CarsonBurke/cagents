//! Display the recorded main conversation across resume branches while keeping
//! tool correlation confined to the ancestry of each result.
use super::{CompactRecord, HistoryNode, Skipped};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

type Call = (u64, u64, usize, usize);
pub(super) type ResultCalls = HashMap<u64, Vec<(usize, Call)>>;
pub(super) type ToolIdentities = HashMap<(u64, usize), String>;

pub(super) struct Index {
    pub chain: Vec<(u64, u64, usize)>,
    pub result_calls: ResultCalls,
    pub tool_identities: ToolIdentities,
    pub ambiguous: HashSet<String>,
}

/// Resolve damaged parent links once, using the same main-message and
/// anonymous-prefix boundaries as the tolerant reader. Cached skipped chains
/// cannot turn a large damaged log into one ancestry scan per visible turn.
pub(super) struct Parents<'a> {
    nodes: &'a HashMap<String, HistoryNode>,
    skipped: &'a [Skipped],
    by_uuid: HashMap<&'a str, usize>,
    anonymous: Vec<(usize, usize)>,
    written: &'a [(usize, &'a str)],
    resolved: HashMap<usize, Option<&'a str>>,
}
impl<'a> Parents<'a> {
    pub fn new(
        nodes: &'a HashMap<String, HistoryNode>,
        skipped: &'a [Skipped],
        written: &'a [(usize, &'a str)],
    ) -> Self {
        let mut by_uuid = HashMap::new();
        let mut anonymous = Vec::new();
        for (index, record) in skipped.iter().enumerate() {
            if let Some(uuid) = &record.uuid {
                by_uuid.entry(uuid.as_str()).or_insert(index);
            } else if !record.sidechain && !record.team {
                anonymous.push((record.at, index));
            }
        }
        anonymous.sort_unstable();
        Self {
            nodes,
            skipped,
            by_uuid,
            anonymous,
            written,
            resolved: HashMap::new(),
        }
    }
    fn preceding(&self, before: usize) -> Option<&'a str> {
        self.written[..self.written.partition_point(|(order, _)| *order < before)]
            .last()
            .map(|(_, id)| *id)
    }
    fn missing(&self, parent: &str, before: usize) -> Option<usize> {
        if let Some(index) = self.by_uuid.get(parent) {
            return Some(*index);
        }
        let &(at, index) =
            self.anonymous[..self.anonymous.partition_point(|(at, _)| *at < before)].last()?;
        let previous = self.written[..self.written.partition_point(|(order, _)| *order < before)]
            .last()
            .map(|(order, _)| *order);
        previous
            .is_none_or(|previous| previous < at)
            .then_some(index)
    }
    pub fn resolve(&mut self, mut parent: &'a str, mut before: usize) -> Option<&'a str> {
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        let resolved = loop {
            if let Some((uuid, node)) = self.nodes.get_key_value(parent) {
                break node.main.then_some(uuid.as_str());
            }
            let Some(index) = self.missing(parent, before) else {
                break None;
            };
            if let Some(resolved) = self.resolved.get(&index) {
                break *resolved;
            }
            if !seen.insert(index) {
                break None;
            }
            path.push(index);
            let record = &self.skipped[index];
            if record.sidechain || record.team {
                break None;
            }
            if record.compaction() {
                let logical = record
                    .logical_parent
                    .as_deref()
                    .and_then(|id| self.nodes.get_key_value(id))
                    .filter(|(_, node)| node.main && node.first_order < record.at)
                    .map(|(uuid, _)| uuid.as_str());
                break logical.or_else(|| self.preceding(record.at));
            }
            match &record.parent {
                Some(Some(next)) => {
                    parent = next;
                    before = record.at;
                }
                Some(None) => break None,
                None => break self.preceding(record.at),
            }
        };
        for index in path {
            self.resolved.insert(index, resolved);
        }
        resolved
    }
}

pub(super) fn index<'a>(
    nodes: &'a HashMap<String, HistoryNode>,
    active: &HashSet<&str>,
    parents: &mut Parents<'a>,
) -> Result<Index, String> {
    let mut ordered = nodes.iter().collect::<Vec<_>>();
    ordered.sort_unstable_by_key(|(_, node)| node.order);
    let positions = ordered
        .iter()
        .enumerate()
        .map(|(index, (id, _))| (id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut children = vec![Vec::new(); ordered.len()];
    let mut roots = Vec::new();
    let mut counts = HashMap::new();
    for (_, node) in &ordered {
        if node.visible {
            for (id, _, result) in &node.tool_refs {
                if !result {
                    *counts.entry(id.as_str()).or_insert(0usize) += 1;
                }
            }
        }
    }
    let mut tool_identities = ToolIdentities::new();
    for (index, &(uuid, node)) in ordered.iter().enumerate() {
        // Rewrites move a kept carrier under a compaction summary. Its first
        // parent retains the ancestry that originally introduced its tools.
        let parent = node.original_parent().or_else(|| {
            let compact = node.compaction.as_ref()?;
            compact
                .logical_parent
                .as_deref()
                .filter(|id| {
                    nodes
                        .get(*id)
                        .is_some_and(|parent| parent.first_order < node.first_order)
                })
                .or(compact.preceding.as_deref())
        });
        if let Some(parent) = parent
            .and_then(|id| parents.resolve(id, node.first_order))
            .and_then(|id| positions.get(id))
            .copied()
        {
            children[parent].push(index);
        } else {
            roots.push(index);
        }
        if node.visible && !active.contains(uuid.as_str()) {
            for (raw, block, result) in &node.tool_refs {
                if !result && counts[raw.as_str()] > 1 {
                    tool_identities.insert((node.range.0, *block), identity(uuid, *block));
                }
            }
        }
    }
    enum Visit {
        Enter(usize),
        Leave(usize),
    }
    let mut visits = roots
        .into_iter()
        .rev()
        .map(Visit::Enter)
        .collect::<Vec<_>>();
    let mut visited = vec![false; ordered.len()];
    let mut calls = HashMap::<&str, Call>::new();
    let mut undo = Vec::new();
    let mut result_calls = ResultCalls::new();
    while let Some(visit) = visits.pop() {
        match visit {
            Visit::Enter(index) => {
                if std::mem::replace(&mut visited[index], true) {
                    return Err("Claude transcript contains a cyclic parent chain".into());
                }
                let checkpoint = undo.len();
                let (uuid, node) = ordered[index];
                for (id, block, result) in &node.tool_refs {
                    if *result {
                        if let Some(call) = calls.get(id.as_str()).copied() {
                            result_calls
                                .entry(node.range.0)
                                .or_default()
                                .push((*block, call));
                            if let Some(identity) = tool_identities.get(&(call.0, call.3)).cloned()
                            {
                                tool_identities.insert((node.range.0, *block), identity);
                            }
                        } else {
                            // An unmatched result must not consult another
                            // branch's raw-ID cache in the reducer, including
                            // a future call already loaded by an earlier page.
                            tool_identities.insert((node.range.0, *block), identity(uuid, *block));
                        }
                    } else {
                        let call = (node.range.0, node.range.1, node.range.2, *block);
                        undo.push((id.as_str(), calls.insert(id.as_str(), call)));
                    }
                }

                visits.push(Visit::Leave(checkpoint));
                visits.extend(children[index].iter().rev().copied().map(Visit::Enter));
            }
            Visit::Leave(checkpoint) => {
                while undo.len() > checkpoint {
                    let (id, previous) = undo.pop().unwrap();
                    if let Some(previous) = previous {
                        calls.insert(id, previous);
                    } else {
                        calls.remove(id);
                    }
                }
            }
        }
    }
    if ordered
        .iter()
        .enumerate()
        .any(|(index, (_, node))| node.visible && !visited[index])
    {
        return Err("Claude transcript contains a cyclic parent chain".into());
    }
    let mut visible = ordered
        .into_iter()
        .filter(|(_, node)| node.visible)
        .map(|(_, node)| node)
        .collect::<Vec<_>>();
    // Claude records a manual summary before the command the person typed;
    // display those two as the live stream did. All other carriers keep their
    // recorded order, including prior branches and rewritten kept messages.
    let mut gaps = parents
        .skipped
        .iter()
        .filter(|record| record.shown())
        .map(|record| record.at)
        .collect::<Vec<_>>();
    gaps.sort_unstable();
    let mut index = 0;
    while index + 1 < visible.len() {
        if visible[index].compact_record == Some(CompactRecord::Summary)
            && visible[index + 1].compact_record == Some(CompactRecord::Command)
            && gaps
                .get(gaps.partition_point(|at| *at <= visible[index].order))
                .is_none_or(|at| *at >= visible[index + 1].order)
        {
            visible.swap(index, index + 1);
            index += 1;
        }
        index += 1;
    }
    Ok(Index {
        chain: visible.into_iter().map(|node| node.range).collect(),
        result_calls,
        tool_identities,
        ambiguous: counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(id, _)| id.to_owned())
            .collect(),
    })
}

fn identity(uuid: &str, block: usize) -> String {
    let mut digest = Sha256::new();
    digest.update(uuid.as_bytes());
    digest.update((block as u64).to_be_bytes());
    format!("history:{:x}", digest.finalize())
}

#[derive(Default)]
pub(super) struct AgentCall {
    block: usize,
    name: String,
    background: bool,
}
#[derive(Default)]
pub(super) struct ResultDetails {
    launched: bool,
    agent: Option<String>,
    tasks: Vec<String>,
    errors: HashSet<usize>,
}

/// Retain identity/status metadata, never result bodies or patches. A frame's
/// structured result belongs to a tool only when it carries one result block.
pub(super) fn details(frame: &super::Value) -> (Vec<AgentCall>, ResultDetails) {
    use super::{async_launch, string, structured_result};
    let Some(blocks) = frame["message"]["content"].as_array() else {
        return Default::default();
    };
    let agents = blocks
        .iter()
        .enumerate()
        .filter_map(|(block, value)| {
            (string(frame, "type") == "assistant"
                && matches!(string(value, "type"), "tool_use" | "server_tool_use")
                && string(value, "name") == "Agent")
                .then(|| AgentCall {
                    block,
                    name: string(&value["input"], "name").trim().to_owned(),
                    background: value["input"]["run_in_background"].as_bool() == Some(true),
                })
        })
        .collect();
    let mut result = ResultDetails {
        errors: blocks
            .iter()
            .enumerate()
            .filter_map(|(block, value)| {
                (string(value, "type") == "tool_result"
                    && value["is_error"].as_bool() == Some(true))
                .then_some(block)
            })
            .collect(),
        ..Default::default()
    };
    if blocks
        .iter()
        .filter(|block| string(block, "type") == "tool_result")
        .count()
        == 1
    {
        let metadata = structured_result(frame);
        result.launched = async_launch(frame);
        result.agent = metadata["agentId"].as_str().map(str::to_owned);
        result.tasks = ["agentId", "backgroundTaskId", "taskId"]
            .into_iter()
            .filter_map(|key| {
                metadata[key]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
            })
            .collect();
    }
    (agents, result)
}

pub(super) fn details_bytes(agents: &[AgentCall], result: &ResultDetails) -> usize {
    agents
        .iter()
        .map(|agent| agent.name.len() + 64)
        .sum::<usize>()
        + result.agent.as_ref().map_or(0, String::len)
        + result
            .tasks
            .iter()
            .map(|task| task.len() + 64)
            .sum::<usize>()
        + result.errors.len() * 32
}

/// Build activity from visible occurrences as well as their ancestry. Private,
/// model-only and retracted calls cannot enter via a metadata or sidecar path.
pub(super) fn index_activity(
    history: &mut super::History,
    nodes: &HashMap<String, HistoryNode>,
    ambiguous: &HashSet<String>,
    notices: &[(u64, u64, usize)],
) -> Result<(), String> {
    use super::{ToolStatus, string};
    use std::collections::BTreeMap;
    let mut identities = HashMap::new();
    let mut raw_calls = HashSet::new();
    let mut agent_order = BTreeMap::new();
    for node in nodes.values().filter(|node| node.visible) {
        let mut agents = node.agents.iter().peekable();
        for (raw, block, result) in &node.tool_refs {
            if *result {
                continue;
            }
            raw_calls.insert(raw.as_str());
            let identity = history
                .tool_identities
                .get(&(node.range.0, *block))
                .cloned()
                .unwrap_or_else(|| raw.clone());
            identities.insert((node.range.0, *block), identity.clone());
            if ambiguous.contains(raw) {
                history
                    .agent_raw_tools
                    .insert(identity.clone(), raw.clone());
            }
            while agents.peek().is_some_and(|agent| agent.block < *block) {
                agents.next();
            }
            if let Some(agent) = agents.next_if(|agent| agent.block == *block) {
                agent_order.insert((node.first_order, *block), identity.clone());
                if !agent.name.is_empty() {
                    history
                        .agent_names
                        .insert(identity.clone(), agent.name.clone());
                }
                if agent.background {
                    history.background_agent_tools.insert(identity);
                }
            }
        }
    }
    for raw in ambiguous {
        // Also record the raw namespace even if no current branch owns it.
        history
            .agent_raw_tools
            .entry(raw.clone())
            .or_insert_with(|| raw.clone());
    }
    history.agent_tools = agent_order
        .into_values()
        .enumerate()
        .map(|(order, identity)| (identity, order))
        .collect();
    let by_offset = nodes
        .values()
        .map(|node| (node.range.0, node))
        .collect::<HashMap<_, _>>();
    let mut tasks = HashMap::<String, Option<String>>::new();
    let mut updates = BTreeMap::new();
    for (offset, refs) in &history.result_calls {
        let node = by_offset[offset];
        for (_, call) in refs {
            let Some(identity) = identities.get(&(call.0, call.3)) else {
                continue;
            };
            for task in &node.result.tasks {
                tasks
                    .entry(task.clone())
                    .and_modify(|old| {
                        if old.as_ref() != Some(identity) {
                            *old = None;
                        }
                    })
                    .or_insert_with(|| Some(identity.clone()));
            }
        }
        updates.insert(*offset, None);
    }
    for &(offset, len, base) in notices {
        updates.insert(offset, Some((len, base)));
    }
    let mut ended = HashMap::new();
    let mut warned = HashSet::new();
    for (offset, notice) in updates {
        if let Some(refs) = history.result_calls.get(&offset) {
            let node = by_offset[&offset];
            for (block, call) in refs {
                let Some(identity) = identities.get(&(call.0, call.3)) else {
                    continue;
                };
                if !history.agent_tools.contains_key(identity) {
                    continue;
                }
                let stopped = ended.get(identity).copied();
                if node.result.launched {
                    if stopped.is_none() {
                        history.background_agent_tools.insert(identity.clone());
                    }
                    if let Some(agent) = &node.result.agent {
                        history.agent_ids.insert(identity.clone(), agent.clone());
                    }
                }
                let status = if let Some(stopped) =
                    stopped.filter(|status| node.result.launched || *status == ToolStatus::Stopped)
                {
                    stopped
                } else if node.result.errors.contains(block) {
                    ToolStatus::Failed
                } else if history.background_agent_tools.contains(identity) {
                    ToolStatus::Running
                } else {
                    ToolStatus::Succeeded
                };
                history.agent_status.insert(identity.clone(), status);
            }
        }
        let Some((len, base)) = notice else {
            continue;
        };
        let frame = history.read_record(offset, len, base)?;
        let mut notices = Vec::new();
        if string(&frame, "type") == "system"
            && matches!(
                string(&frame, "subtype"),
                "task_started" | "task_progress" | "task_notification" | "task_updated"
            )
            && let Some(raw) = frame["tool_use_id"].as_str()
        {
            let patch = if frame["patch"].is_object() {
                &frame["patch"]
            } else {
                &frame
            };
            notices.push((
                raw.to_owned(),
                string(&frame, "task_id").to_owned(),
                super::task_status(
                    string(patch, "status"),
                    string(&frame, "subtype") == "task_notification",
                ),
                string(&frame, "subtype") == "task_started",
                false,
                None,
            ));
        }
        if let Some(content) = super::recorded_notification(&frame) {
            for (raw, task, outcome) in super::notified_call_tasks(&content) {
                notices.push((
                    raw,
                    task,
                    Some(outcome.status),
                    false,
                    true,
                    Some(outcome.recorded()),
                ));
            }
            for (raw, task, status) in super::notification_outcomes(&content) {
                notices.push((raw, task, Some(status), false, true, None));
            }
        }
        for (raw, task, status, started, recorded, outcome) in notices {
            let identity = match tasks.get(&task) {
                Some(Some(identity)) => identity.clone(),
                _ if ambiguous.contains(&raw) => {
                    if warned.insert(raw) {
                        tracing::warn!(
                            "A recorded task notification has an ambiguous tool identity; its outcome was not attributed"
                        );
                    }
                    continue;
                }
                _ if raw_calls.contains(raw.as_str()) => raw,
                _ => continue,
            };
            if history.agent_tools.contains_key(&identity) {
                if outcome.is_some() {
                    continue;
                }
                if started {
                    ended.remove(&identity);
                } else if let Some(status) = status
                    && ended.get(&identity) != Some(&ToolStatus::Stopped)
                {
                    ended.insert(identity.clone(), status);
                }
                if recorded || started || history.background_agent_tools.contains(&identity) {
                    history
                        .agent_status
                        .insert(identity, status.unwrap_or(ToolStatus::Running));
                } else if status == Some(ToolStatus::Stopped) {
                    history.agent_status.insert(identity, ToolStatus::Stopped);
                }
            } else if let Some(outcome) = outcome {
                history.call_outcomes.insert(identity, outcome);
            }
        }
    }
    Ok(())
}
