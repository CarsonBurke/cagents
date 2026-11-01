//! Presentation-only incidents. Sources remain in the original snapshot/history.
use super::*;
use chat::failure::{Failure, FailureKind};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Source {
    Item(String),
    ChildDetail(String),
    ChildItem(String, String),
    Note(usize),
    Local(Arc<str>),
    Snapshot,
}

/// A failure received while the reader is looking at an earlier page. Its
/// provider source and receipt belong to that publication, not the page that
/// eventually becomes visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Deferred {
    pub text: String,
    pub error_item: Option<String>,
    pub error_generation: u64,
    pub turn_generation: u64,
}

impl Deferred {
    pub fn new(text: String, snapshot: &Snapshot) -> Self {
        Self {
            error_item: (snapshot.error.as_deref() == Some(text.as_str()))
                .then(|| snapshot.error_item.clone())
                .flatten(),
            text,
            error_generation: snapshot.error_generation,
            turn_generation: snapshot.turn_generation,
        }
    }

    pub fn same_event(&self, other: &Self) -> bool {
        self.text == other.text
            && match (&self.error_item, &other.error_item) {
                (Some(left), Some(right)) => left == right,
                (None, None) => {
                    self.error_generation == other.error_generation
                        && self.turn_generation == other.turn_generation
                }
                _ => false,
            }
    }

    pub fn matches_snapshot(&self, snapshot: &Snapshot) -> bool {
        snapshot.error.as_deref() == Some(self.text.as_str())
            && snapshot.error_generation == self.error_generation
            && snapshot.turn_generation == self.turn_generation
            && self
                .error_item
                .as_ref()
                .is_none_or(|source| snapshot.error_item.as_ref() == Some(source))
    }

    /// An unresolved receipt still has an identity, but no proven transcript
    /// position. Keep its diagnostics distinct without borrowing a later
    /// source or manufacturing a prompt/item to place it after.
    pub fn unresolved_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"deferred-receipt\0");
        let source = self.error_item.as_deref().unwrap_or_default();
        digest.update((source.len() as u64).to_be_bytes());
        digest.update(source.as_bytes());
        digest.update(self.error_generation.to_be_bytes());
        digest.update(self.turn_generation.to_be_bytes());
        digest.update(self.text.as_bytes());
        format!("{:x}", digest.finalize())
    }
}

/// A notice kept in the transcript after its live source has gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Note {
    /// The prompt message it concerns: its identity for deduplication,
    /// incident scope and sign-in recovery.
    pub message: Option<String>,
    /// The item whose group it is drawn after: the transcript's last when
    /// it was recorded on the latest page. That can lie past the message's
    /// own exchange, such as after work a delivery started. An item outlasts
    /// a group key, which paging can change.
    pub after: Option<String>,
    pub text: String,
    pub turn_generation: u64,
    /// Proven provider occurrence, captured while its source is loaded. Keep
    /// this across paging so Details and selection retain the same identity.
    pub event_key: Option<String>,
}

pub(super) struct Incident {
    pub key: String,
    pub summary: String,
    pub guidance: &'static str,
    pub kind: Option<FailureKind>,
    sources: Vec<Source>,
    /// The chronological first transcript source owns the visible notice.
    pub first_item: Option<String>,
    pub anchor: Option<String>,
    pub child_scope: Option<String>,
    pub active_recovery: bool,
}

#[derive(Clone)]
struct Metadata {
    key: String,
    summary: String,
    guidance: &'static str,
    kind: Option<FailureKind>,
}

#[derive(Default)]
pub(super) struct State {
    pub incidents: Vec<Incident>,
    pub expanded: HashSet<String>,
    pub revision: u64,
    pub local_error: Option<Arc<str>>,
    pub find_expanded: HashSet<String>,
    previews: HashMap<String, Preview>,
    item_metadata: HashMap<Source, (Arc<chat::Item>, String, Metadata)>,
}

fn provider_item_text(provider: Provider, item: &chat::Item) -> Option<&str> {
    match &item.content {
        _ if chat::failure::provider_error_text(provider, item).is_some() => {
            chat::failure::provider_error_text(provider, item)
        }
        ItemContent::Tool {
            name,
            output,
            status: chat::ToolStatus::Failed,
            ..
        } if match provider {
            Provider::Claude => matches!(name.as_str(), "Agent" | "Task"),
            Provider::Codex => matches!(name.as_str(), "spawnAgent" | "spawn_agent"),
            Provider::Omp => false,
        } =>
        {
            Some(output)
        }
        _ => None,
    }
}

pub(super) fn diagnostic_item(provider: Provider, item: &chat::Item) -> bool {
    provider_item_text(provider, item).is_some()
}

fn fallback_summary(text: &str) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    if first.starts_with(['{', '[']) || first.starts_with("API Error:") {
        "Request failed".to_owned()
    } else {
        Runtime::short_description(first, 160)
    }
}

fn diagnostic_key(text: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
}

// Shared by the actual bounded preview widget and its transcript catalog.
// The incident key already includes scope/cause; raw diagnostics never enter
// this identity or the collapsed selection catalog.
pub(super) fn diagnostic_preview_identity(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::hash::DefaultHasher::new();
    ("failure-diagnostic-preview", key).hash(&mut hash);
    hash.finish()
}

// Notice identity follows a decoder message, while Turn keys remain display
// destinations. Paging can leave a later part of the same message visible.
pub(super) fn prompt_anchor(provider: Provider, items: &[Arc<chat::Item>]) -> Option<&str> {
    let last = items
        .iter()
        .rev()
        .find(|item| {
            item.role == chat::Role::User && !matches!(item.content, ItemContent::Delivery { .. })
        })
        .map(|item| item.id.as_str())?;
    let Some(carrier) = attachment_draft::message_key(provider, last) else {
        return Some(last);
    };
    items
        .iter()
        .find(|item| {
            item.role == chat::Role::User
                && !matches!(item.content, ItemContent::Delivery { .. })
                && attachment_draft::message_key(provider, &item.id) == Some(carrier)
        })
        .map(|item| item.id.as_str())
}

pub(super) fn same_prompt(provider: Provider, left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => match (
            attachment_draft::message_key(provider, left),
            attachment_draft::message_key(provider, right),
        ) {
            (Some(left), Some(right)) => left == right,
            _ => left == right,
        },
        _ => left == right,
    }
}

/// The prompt items in `turn` that the reader typed, not deliveries.
fn typed_prompt<'a>(runtime: &'a Runtime, turn: &'a turn::Turn) -> impl Iterator<Item = &'a str> {
    turn.prompt.iter().filter_map(|index| {
        runtime
            .snapshot
            .items
            .get(*index)
            .filter(|item| {
                item.role == chat::Role::User
                    && !matches!(item.content, ItemContent::Delivery { .. })
            })
            .map(|item| item.id.as_str())
    })
}

/// The last item of the transcript's last group, where a notice recorded
/// now is drawn after. `None` while an earlier page is shown, whose end is
/// the middle of the conversation rather than its latest work.
pub(super) fn closing_item<'a>(snapshot: &'a Snapshot, turns: &[turn::Turn]) -> Option<&'a str> {
    if snapshot.has_later {
        return None;
    }
    let turn = turns.last()?;
    let last = turn
        .prompt
        .iter()
        .chain(
            turn.steps
                .iter()
                .chain(&turn.prose)
                .map(|placed| &placed.index),
        )
        .max()?;
    snapshot.items.get(*last).map(|item| item.id.as_str())
}

/// The failure scope of a prompt the reader typed: its decoder message, so
/// that text and images sent together are one scope. An opaque ID is its
/// own scope, which cannot collide with a message of the same spelling.
fn prompt_scope(provider: Provider, id: &str) -> String {
    match attachment_draft::message_key(provider, id) {
        Some(carrier) => format!("message\0{carrier}"),
        None => format!("item\0{id}"),
    }
}

/// An explicit diagnostic owns its occurrence, independently of which prompt
/// happens to be loaded. Multipart provider messages share their proven carrier.
fn event_scope(provider: Provider, item: &chat::Item) -> String {
    match chat::failure::provider_error_carrier(provider, item) {
        Some(carrier) => format!("event\0carrier\0{carrier}"),
        None => format!("event\0item\0{}", item.id),
    }
}

/// Notices are inserted between groups. Close a group at the recorded item so
/// later prompts or streamed prose cannot grow across an earlier failure.
pub(super) fn boundaries(asked_after: &HashSet<String>, notes: &[Note]) -> HashSet<String> {
    let mut boundaries = asked_after.clone();
    boundaries.extend(notes.iter().filter_map(|note| note.after.clone()));
    boundaries
}

/// A scalar failure can repeat an explicit diagnostic carried in the same
/// publication. Join only the recorded item and its proven multipart carrier,
/// with the complete body the adapter used, never another equal-text incident.
pub(super) fn notice_key(
    provider: Provider,
    snapshot: &Snapshot,
    after: Option<&str>,
    notice: &str,
) -> Option<String> {
    let item = snapshot
        .items
        .iter()
        .find(|item| Some(item.id.as_str()) == after)?;
    provider_item_text(provider, item)?;
    let carrier = chat::failure::provider_error_carrier(provider, item);
    let sources: Vec<_> = snapshot
        .items
        .iter()
        .filter(|source| {
            source.id == item.id
                || carrier.is_some_and(|carrier| {
                    chat::failure::provider_error_carrier(provider, source) == Some(carrier)
                })
        })
        .filter_map(|source| provider_item_text(provider, source))
        .collect();
    let body = sources.join("\n");
    let scalar_body = if provider == Provider::Claude {
        notice
            .strip_prefix("Claude: ")
            .and_then(|text| text.split_once('\n'))
            .map_or(notice, |(_, body)| body)
    } else {
        notice
    };
    if scalar_body != body {
        return None;
    }
    item_key(provider, snapshot, &item.id)
}

/// Canonical metadata for an explicitly linked provider occurrence.
pub(super) fn item_key(provider: Provider, snapshot: &Snapshot, id: &str) -> Option<String> {
    let item = snapshot.items.iter().find(|item| item.id == id)?;
    provider_item_text(provider, item)?;
    let scope = event_scope(provider, item);
    let carrier = chat::failure::provider_error_carrier(provider, item);
    let mut chosen = None;
    let sources = snapshot
        .items
        .iter()
        .filter(|source| {
            source.id == id
                || carrier.is_some_and(|carrier| {
                    chat::failure::provider_error_carrier(provider, source) == Some(carrier)
                })
        })
        .filter_map(|source| provider_item_text(provider, source));
    for text in sources {
        if let Some(meta) = State::metadata(provider, text, false, &scope)
            && chosen
                .as_ref()
                .is_none_or(|previous: &Metadata| previous.kind.is_none() && meta.kind.is_some())
        {
            chosen = Some(meta);
        }
    }
    chosen.map(|meta| meta.key)
}

/// Where a retained note is drawn: after the group containing the item it
/// was recorded after, while that item is visible. Otherwise a visible part
/// of the note's message stands in, and the note goes after the last visible
/// group before the reader's next prompt. `None` when neither is visible, so
/// the note never borrows another exchange.
fn note_destination<'a>(
    runtime: &'a Runtime,
    note: &Note,
    item_groups: &HashMap<&str, &'a str>,
    user_carriers: &HashMap<&str, String>,
) -> Option<&'a str> {
    if let Some(group) = note
        .after
        .as_deref()
        .and_then(|after| item_groups.get(after))
    {
        return Some(group);
    }
    let anchor = note.message.as_deref()?;
    let provider = runtime.session.launch.provider;
    let carrier = attachment_draft::message_key(provider, anchor)
        .filter(|carrier| user_carriers.contains_key(carrier));
    let start = runtime.turns.iter().position(|turn| {
        typed_prompt(runtime, turn).any(|id| {
            id == anchor
                || carrier.is_some_and(|carrier| {
                    attachment_draft::message_key(provider, id) == Some(carrier)
                })
        })
    })?;
    let replies = runtime.turns[start + 1..]
        .iter()
        .take_while(|turn| typed_prompt(runtime, turn).next().is_none())
        .count();
    Some(runtime.turns[start + replies].key.as_str())
}

impl State {
    fn metadata(
        provider: Provider,
        text: &str,
        known_only: bool,
        context: &str,
    ) -> Option<Metadata> {
        if text.trim().is_empty() {
            return None;
        }
        let parsed = Failure::parse(provider, text);
        if known_only && parsed.is_none() {
            return None;
        }
        let (key, summary, guidance, kind) = match parsed {
            Some(failure) => (
                diagnostic_key(failure.key.as_ref()),
                failure.summary,
                failure.action,
                Some(failure.kind),
            ),
            None => (
                diagnostic_key(text),
                fallback_summary(text),
                "Review the details before trying again.",
                None,
            ),
        };
        let meta = Metadata {
            key: diagnostic_key(&format!("{context}\0{key}")),
            summary,
            guidance,
            kind,
        };
        Some(meta)
    }

    fn push(
        &mut self,
        provider: Provider,
        text: &str,
        source: Source,
        known_only: bool,
        context: &str,
    ) -> Option<Metadata> {
        let meta = Self::metadata(provider, text, known_only, context)?;
        self.push_metadata(meta.clone(), source);
        Some(meta)
    }

    fn push_metadata(&mut self, meta: Metadata, source: Source) {
        if let Some(incident) = self
            .incidents
            .iter_mut()
            .find(|incident| incident.key == meta.key)
        {
            if !incident.sources.contains(&source) {
                incident.sources.push(source);
            }
        } else {
            let first_item = match &source {
                Source::Item(id) => Some(id.clone()),
                _ => None,
            };
            self.incidents.push(Incident {
                key: meta.key,
                summary: meta.summary,
                guidance: meta.guidance,
                kind: meta.kind,
                sources: vec![source],
                first_item,
                anchor: None,
                child_scope: None,
                active_recovery: false,
            });
        }
    }

    fn metadata_for_item(
        runtime: &Runtime,
        item: &Arc<chat::Item>,
        source: &Source,
        context: &str,
    ) -> Option<Metadata> {
        if let Some((previous, old_context, meta)) = runtime.failure_ui.item_metadata.get(source)
            && Arc::ptr_eq(previous, item)
            && old_context == context
        {
            return Some(meta.clone());
        }
        Self::metadata(
            runtime.session.launch.provider,
            provider_item_text(runtime.session.launch.provider, item)?,
            false,
            context,
        )
    }

    // A provider may split one diagnostic into a cause block and raw request
    // blocks. Unknown sibling blocks share the proven carrier's cause; distinct
    // classified causes still retain their own identity and complete originals.
    fn carrier_causes(
        runtime: &Runtime,
        items: &[Arc<chat::Item>],
        source: impl Fn(&chat::Item) -> Source,
        context: impl Fn(&chat::Item) -> String,
    ) -> HashMap<String, Metadata> {
        let mut causes: HashMap<String, Metadata> = HashMap::new();
        for item in items {
            if let Some(carrier) =
                chat::failure::provider_error_carrier(runtime.session.launch.provider, item)
                && let Some(meta) =
                    Self::metadata_for_item(runtime, item, &source(item), &context(item))
            {
                let meta = if meta.kind.is_none()
                    && matches!(source(item), Source::Item(_))
                    && runtime
                        .failure_ui
                        .item_metadata
                        .get(&source(item))
                        .is_some_and(|(_, old_context, _)| old_context == &context(item))
                    && let Some(incident) = runtime
                        .failure_ui
                        .item(&item.id)
                        .filter(|incident| incident.kind.is_some())
                {
                    Metadata {
                        key: incident.key.clone(),
                        summary: incident.summary.clone(),
                        guidance: incident.guidance,
                        kind: incident.kind,
                    }
                } else {
                    meta
                };
                let prior = causes
                    .entry(carrier.to_owned())
                    .or_insert_with(|| meta.clone());
                if prior.kind.is_none() && meta.kind.is_some() {
                    *prior = meta;
                }
            }
        }
        causes
    }

    fn push_item(
        &mut self,
        runtime: &Runtime,
        item: &Arc<chat::Item>,
        source: Source,
        context: &str,
        causes: &HashMap<String, Metadata>,
    ) {
        if let Some(mut meta) = Self::metadata_for_item(runtime, item, &source, context) {
            // Cache intrinsic classification, never a sibling's projected cause.
            self.item_metadata.insert(
                source.clone(),
                (item.clone(), context.to_owned(), meta.clone()),
            );
            if meta.kind.is_none()
                && let Some(cause) =
                    chat::failure::provider_error_carrier(runtime.session.launch.provider, item)
                        .and_then(|carrier| causes.get(carrier))
            {
                meta = cause.clone();
            }
            self.push_metadata(meta.clone(), source.clone());
        }
    }

    pub fn collect(runtime: &Runtime) -> Self {
        use std::hash::{Hash, Hasher};
        let mut state = Self::default();
        let provider = runtime.session.launch.provider;
        // Only explicit provider/system failures and failed operations are
        // classified. User messages and ordinary assistant prose remain verbatim.
        let mut contexts = HashMap::new();
        let mut context = String::new();
        let mut user_carriers = HashMap::new();
        for item in &runtime.snapshot.items {
            if item.role == chat::Role::User
                && !matches!(item.content, ItemContent::Delivery { .. })
            {
                // One decoder message can contain text and several images.
                // Keep its exact carrier, rather than making every part a
                // different failure scope or merging adjacent submissions.
                if let Some(carrier) = attachment_draft::message_key(provider, &item.id) {
                    let scope = user_carriers
                        .entry(carrier)
                        .or_insert_with(|| prompt_scope(provider, &item.id));
                    context.clone_from(scope);
                } else {
                    context = prompt_scope(provider, &item.id);
                }
            }
            contexts.insert(item.id.as_str(), context.clone());
        }
        let causes = Self::carrier_causes(
            runtime,
            &runtime.snapshot.items,
            |item| Source::Item(item.id.clone()),
            |item| event_scope(provider, item),
        );
        for item in &runtime.snapshot.items {
            if provider_item_text(provider, item).is_none() {
                continue;
            }
            state.push_item(
                runtime,
                item,
                Source::Item(item.id.clone()),
                &event_scope(provider, item),
                &causes,
            );
        }
        for child in &runtime.snapshot.subagents {
            let child_context = contexts
                .get(child.id.as_str())
                .cloned()
                .unwrap_or_else(|| format!("child:{}", child.id));
            // Child inputs are not a trustworthy turn boundary: some providers
            // expose them as System text. Preserve separate typed error IDs.
            let item_context = |item: &chat::Item| {
                let carrier =
                    chat::failure::provider_error_carrier(provider, item).unwrap_or(&item.id);
                format!("{child_context}:child:{}:error:{carrier}", child.id)
            };
            let causes = Self::carrier_causes(
                runtime,
                &child.items,
                |item| Source::ChildItem(child.id.clone(), item.id.clone()),
                &item_context,
            );
            let mut latest = None;
            for item in &child.items {
                if let Some(text) = provider_item_text(provider, item) {
                    let carrier =
                        chat::failure::provider_error_carrier(provider, item).unwrap_or(&item.id);
                    let context = format!("{child_context}:child:{}:error:{carrier}", child.id);
                    state.push_item(
                        runtime,
                        item,
                        Source::ChildItem(child.id.clone(), item.id.clone()),
                        &context,
                        &causes,
                    );
                    latest = Some((context, text));
                }
            }
            if (matches!(
                child.status,
                chat::ToolStatus::Failed | chat::ToolStatus::Denied
            ) || (child.status == chat::ToolStatus::Running
                && child.waiting
                && Failure::parse(provider, &child.detail).is_some()))
                && !child.detail.trim().is_empty()
            {
                // Only a detail containing the exact latest proven error may
                // join that historical incident. No inference from quoted text.
                let context = if let Some((context, body)) = latest
                    && child.detail.ends_with(body)
                {
                    context
                } else if Failure::parse(provider, &child.detail)
                    .is_some_and(|failure| failure.kind == FailureKind::AccountLimit)
                {
                    child_context.clone()
                } else {
                    format!("{child_context}:child:{}:detail", child.id)
                };
                state.push(
                    provider,
                    &child.detail,
                    Source::ChildDetail(child.id.clone()),
                    false,
                    &context,
                );
            }
        }

        let item_groups: HashMap<&str, &str> = if runtime.failures.is_empty() {
            HashMap::new()
        } else {
            runtime
                .turns
                .iter()
                .flat_map(|turn| {
                    turn.prompt
                        .iter()
                        .chain(
                            turn.steps
                                .iter()
                                .chain(&turn.prose)
                                .map(|placed| &placed.index),
                        )
                        .filter_map(|index| runtime.snapshot.items.get(*index))
                        .map(|item| (item.id.as_str(), turn.key.as_str()))
                })
                .collect()
        };
        // A note keeps its message's scope after paging removes the message,
        // so its incident identity, and an expanded notice, stay put.
        let note_contexts: Vec<String> = runtime
            .failures
            .iter()
            .map(|note| {
                format!(
                    "note\0{}\0{}\0{}",
                    note.message
                        .as_deref()
                        .map(|id| prompt_scope(provider, id))
                        .unwrap_or_default(),
                    note.after.as_deref().unwrap_or(""),
                    note.turn_generation
                )
            })
            .collect();
        for (index, note) in runtime.failures.iter().enumerate() {
            if !runtime.expected_remote_close_error(&note.text) {
                if let Some(mut meta) =
                    Self::metadata(provider, &note.text, false, &note_contexts[index])
                {
                    if let Some(key) = &note.event_key {
                        meta.key.clone_from(key);
                    }
                    state.push_metadata(meta, Source::Note(index));
                }
            }
        }
        // The current error joins the note recorded for it, wherever paging
        // has left that note's message, but only a note drawn here: an error
        // still awaiting the latest page has no note yet, and must not adopt
        // an older one with the same text.
        let snapshot_note = runtime
            .snapshot
            .error
            .as_deref()
            .filter(|_| {
                !runtime
                    .unplaced_failures
                    .iter()
                    .any(|notice| notice.matches_snapshot(&runtime.snapshot))
            })
            .and_then(|error| {
                runtime.failures.iter().rposition(|note| {
                    note.text == error
                        && note_destination(runtime, note, &item_groups, &user_carriers).is_some()
                })
            });
        let snapshot_context =
            snapshot_note.map_or(context.as_str(), |index| note_contexts[index].as_str());
        let snapshot_key = runtime
            .snapshot
            .error_item
            .as_deref()
            .and_then(|id| state.item(id).map(|incident| incident.key.clone()))
            .or_else(|| snapshot_note.and_then(|index| runtime.failures[index].event_key.clone()));
        let local_note = runtime.error.as_deref().and_then(|error| {
            runtime.failures.iter().rposition(|note| {
                note.text == error
                    && note_destination(runtime, note, &item_groups, &user_carriers).is_some()
            })
        });
        state.local_error = if runtime.failure_ui.local_error.as_deref() == runtime.error.as_deref()
        {
            runtime.failure_ui.local_error.clone()
        } else {
            runtime.error.as_deref().map(Arc::<str>::from)
        };
        for (source, text) in [
            (
                state
                    .local_error
                    .as_ref()
                    .map(|raw| Source::Local(raw.clone())),
                runtime.error.as_deref(),
            ),
            (Some(Source::Snapshot), runtime.snapshot.error.as_deref()),
        ] {
            if let Some(text) = text
                && let Some(source) = source
                && !runtime.expected_remote_close_error(text)
            {
                let scope = match source {
                    Source::Snapshot => snapshot_context,
                    Source::Local(_) => {
                        local_note.map_or(context.as_str(), |index| note_contexts[index].as_str())
                    }
                    _ => context.as_str(),
                };
                if let Some(mut meta) = Self::metadata(provider, text, false, scope) {
                    if matches!(source, Source::Snapshot)
                        && let Some(key) = &snapshot_key
                    {
                        meta.key.clone_from(key);
                    }
                    if matches!(source, Source::Local(_))
                        && let Some(key) =
                            local_note.and_then(|index| runtime.failures[index].event_key.as_ref())
                    {
                        meta.key.clone_from(key);
                    }
                    state.push_metadata(meta, source);
                }
            }
        }
        let recovering = runtime.auth_error();
        for incident in &mut state.incidents {
            incident.active_recovery = recovering.is_some()
                && incident.kind == Some(FailureKind::Authentication)
                && incident.sources.iter().any(|source| match source {
                    Source::Local(_) => recovering == runtime.error.as_deref(),
                    Source::Snapshot => recovering == runtime.snapshot.error.as_deref(),
                    Source::Note(index) => {
                        let note = &runtime.failures[*index];
                        (snapshot_note == Some(*index) || local_note == Some(*index))
                            && runtime.active_auth_notice(&note.message, &note.text)
                    }
                    // A historical item in the same prompt is not the active
                    // failure. Its proven scalar/note source owns recovery.
                    Source::Item(_) => false,
                    _ => false,
                });
            let child_history_only = incident
                .sources
                .iter()
                .any(|source| matches!(source, Source::ChildItem(..)))
                && incident.sources.iter().all(|source| {
                    matches!(source, Source::ChildItem(..) | Source::ChildDetail(..))
                });
            if incident.kind != Some(FailureKind::AccountLimit) || child_history_only {
                incident.child_scope = incident.sources.iter().find_map(|source| match source {
                    Source::ChildDetail(child) | Source::ChildItem(child, _) => Some(child.clone()),
                    _ => None,
                });
            }
            incident.anchor = incident
                .first_item
                .as_ref()
                .and_then(|id| {
                    runtime
                        .turns
                        .iter()
                        .find(|turn| {
                            turn.prompt
                                .iter()
                                .chain(
                                    turn.steps
                                        .iter()
                                        .chain(&turn.prose)
                                        .map(|placed| &placed.index),
                                )
                                .any(|index| {
                                    runtime
                                        .snapshot
                                        .items
                                        .get(*index)
                                        .is_some_and(|item| &item.id == id)
                                })
                        })
                        .map(|turn| turn.key.clone())
                })
                .or_else(|| {
                    // The latest note places the incident: a cause recurring
                    // with new wording is drawn after its newest work.
                    incident
                        .sources
                        .iter()
                        .filter_map(|source| match source {
                            Source::Note(index) => Some(*index),
                            _ => None,
                        })
                        .max()
                        .map(|index| {
                            let note = &runtime.failures[index];
                            // An unresolved note keeps a key no visible
                            // group has, which hides it; only a note with
                            // no placement at all leads the transcript.
                            note_destination(runtime, note, &item_groups, &user_carriers)
                                .or(note.after.as_deref())
                                .or(note.message.as_deref())
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| runtime.turns.last().map(|turn| turn.key.clone()))
                });
        }
        state.expanded = runtime
            .failure_ui
            .expanded
            .iter()
            .filter(|key| state.incidents.iter().any(|incident| &incident.key == *key))
            .cloned()
            .collect();
        state.find_expanded = runtime
            .failure_ui
            .find_expanded
            .intersection(&state.expanded)
            .cloned()
            .collect();
        let mut hash = std::hash::DefaultHasher::new();
        for incident in &state.incidents {
            (
                &incident.key,
                &incident.summary,
                &incident.sources,
                &incident.first_item,
                &incident.anchor,
                &incident.active_recovery,
                &incident.child_scope,
            )
                .hash(&mut hash);
            for source in &incident.sources {
                if let Some((item, _, _)) = state.item_metadata.get(source) {
                    Arc::as_ptr(item).hash(&mut hash);
                } else {
                    runtime.failure_source(source).hash(&mut hash);
                }
            }
        }
        state.revision = hash.finish();
        if state.revision == runtime.failure_ui.revision {
            state.previews = runtime.failure_ui.previews.clone();
        }
        state
    }

    pub fn child_item(&self, child: &str, id: &str) -> Option<(&Incident, bool)> {
        self.incidents.iter().find_map(|incident| {
            let first = incident.sources.iter().find_map(|source| match source {
                Source::ChildItem(owner, item) if owner == child => Some(item.as_str()), _ => None,
            })?;
            incident.sources.iter().any(|source| matches!(source, Source::ChildItem(owner, item) if owner == child && item == id))
                .then_some((incident, first == id))
        })
    }

    pub fn item(&self, id: &str) -> Option<&Incident> {
        self.incidents.iter().find(|incident| {
            incident
                .sources
                .iter()
                .any(|source| matches!(source, Source::Item(item) if item == id))
        })
    }
}

#[derive(Clone)]
struct Preview {
    query: Option<String>,
    text: String,
    omitted: bool,
}

impl Runtime {
    pub(super) fn reconcile_failure_ui(&mut self) -> bool {
        if self.failure_ui.local_error.as_deref() == self.error.as_deref() {
            return false;
        }
        self.refresh_failure_ui();
        true
    }

    pub(super) fn refresh_failure_ui(&mut self) {
        self.failure_ui = State::collect(self);
        self.refresh_failure_previews();
    }

    pub(super) fn refresh_failure_previews(&mut self) {
        let keys: Vec<_> = self.failure_ui.expanded.iter().cloned().collect();
        for key in keys {
            self.cache_failure_preview(&key);
        }
    }

    fn failure_source<'a>(&'a self, source: &'a Source) -> Option<&'a str> {
        match source {
            Source::Item(id) => self
                .snapshot
                .items
                .iter()
                .find(|item| &item.id == id)
                .and_then(|item| match &item.content {
                    ItemContent::Text { text, .. } => Some(text.as_str()),
                    ItemContent::Delivery { body, .. } => Some(body.as_str()),
                    ItemContent::Tool { output, .. } => Some(output.as_str()),
                    _ => None,
                }),
            Source::ChildItem(child, id) => self
                .snapshot
                .subagents
                .iter()
                .find(|agent| &agent.id == child)
                .and_then(|agent| agent.items.iter().find(|item| &item.id == id))
                .and_then(|item| match &item.content {
                    ItemContent::Text { text, .. } => Some(text.as_str()),
                    ItemContent::Delivery { body, .. } => Some(body.as_str()),
                    ItemContent::Tool { output, .. } => Some(output.as_str()),
                    _ => None,
                }),
            Source::ChildDetail(id) => self
                .snapshot
                .subagents
                .iter()
                .find(|child| &child.id == id)
                .map(|child| child.detail.as_str()),
            Source::Note(index) => self.failures.get(*index).map(|note| note.text.as_str()),
            Source::Local(raw) => Some(raw),
            Source::Snapshot => self.snapshot.error.as_deref(),
        }
    }

    pub(super) fn failure_preview(&self, key: &str) -> Option<(&str, bool)> {
        self.failure_ui
            .previews
            .get(key)
            .map(|preview| (preview.text.as_str(), preview.omitted))
    }

    pub(super) fn cache_failure_preview(&mut self, key: &str) {
        if self
            .failure_ui
            .previews
            .get(key)
            .is_some_and(|preview| preview.query == self.find)
        {
            return;
        }
        const CAP: usize = 8192;
        let Some(incident) = self
            .failure_ui
            .incidents
            .iter()
            .find(|incident| incident.key == key)
        else {
            return;
        };
        let mut sources = Vec::new();
        for source in &incident.sources {
            if let Some(text) = self.failure_source(source)
                && !sources.contains(&text)
            {
                sources.push(text);
            }
        }
        let matched = self
            .find
            .as_deref()
            .filter(|query| !query.is_empty())
            .and_then(|query| {
                sources.iter().enumerate().find_map(|(index, text)| {
                    find::highlights(text, query)
                        .ranges
                        .first()
                        .map(|range| (index, range.start))
                })
            });
        let mut order: Vec<_> = (0..sources.len()).collect();
        if let Some((index, _)) = matched {
            order.remove(index);
            order.insert(0, index);
        }
        let mut preview = String::with_capacity(CAP);
        let mut omitted = false;
        for (position, index) in order.iter().copied().enumerate() {
            let text = sources[index];
            let separator = if preview.is_empty() {
                ""
            } else {
                "\n\n---\n\n"
            };
            if preview.len() + separator.len() >= CAP {
                omitted = true;
                break;
            }
            preview.push_str(separator);
            let mut start = matched
                .filter(|(source, _)| *source == index)
                .map_or(0, |(_, at)| at.saturating_sub(CAP / 4));
            while !text.is_char_boundary(start) {
                start += 1;
            }
            let mut end = (start + CAP.saturating_sub(preview.len())).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            preview.push_str(&text[start..end]);
            omitted |= start > 0 || end < text.len();
            if preview.len() == CAP {
                omitted |= position + 1 < order.len();
                break;
            }
        }
        self.failure_ui.previews.insert(
            key.to_owned(),
            Preview {
                query: self.find.clone(),
                text: preview,
                omitted,
            },
        );
    }

    pub(super) fn reveal_failure_for_find(&mut self, item: &str) -> Option<String> {
        let incident = match self.selected_subagent.as_deref() {
            Some(child) => self
                .failure_ui
                .child_item(child, item)
                .map(|(incident, _)| incident),
            None => self.failure_ui.item(item),
        }?;
        let key = incident.key.clone();
        let anchor = match self.selected_subagent.as_deref() {
            Some(child) => incident.sources.iter().find_map(|source| match source {
                Source::ChildItem(owner, id) if owner == child => Some(id.clone()),
                _ => None,
            }),
            None => incident.first_item.clone(),
        }
        .unwrap_or_else(|| item.to_owned());
        if self.failure_ui.expanded.insert(key.clone()) {
            self.failure_ui.find_expanded.insert(key.clone());
        }
        self.cache_failure_preview(&key);
        Some(anchor)
    }

    pub(super) fn restore_failure_find(&mut self) {
        for key in self.failure_ui.find_expanded.drain() {
            self.failure_ui.expanded.remove(&key);
        }
        self.failure_ui
            .previews
            .retain(|key, _| self.failure_ui.expanded.contains(key));
        self.refresh_failure_previews();
    }

    pub(super) fn incident_has_child_detail(&self, incident: &Incident, child: &str) -> bool {
        incident
            .sources
            .iter()
            .any(|source| matches!(source, Source::ChildDetail(owner) if owner == child))
    }

    pub(super) fn failure_details(&self, key: &str) -> Option<String> {
        let incident = self
            .failure_ui
            .incidents
            .iter()
            .find(|incident| incident.key == key)?;
        let mut details = Vec::new();
        for source in &incident.sources {
            if let Some(text) = self.failure_source(source)
                && !details.contains(&text)
            {
                details.push(text);
            }
        }
        Some(details.join("\n\n---\n\n"))
    }

    pub(super) fn failure_view(&self, incident: &Incident) -> Element<'static, Message> {
        let mut rows = vec![
            widget::row::with_children(vec![
                crate::ui_controls::passive_icon(Icon::Warning, design::COMPACT_ICON_SIZE),
                design::body(incident.summary.clone())
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Start)
            .into(),
            design::caption(incident.guidance)
                .width(Length::Fill)
                .into(),
        ];
        rows.push(self.failure_diagnostics(incident));
        widget::container(
            widget::column::with_children(rows)
                .spacing(space::S)
                .width(Length::Fill),
        )
        .class(design::surface(
            Some(design::sunken),
            [design::RADIUS_M; 4],
            false,
        ))
        .padding([space::S, space::M])
        .width(Length::Fill)
        .max_width(design::READABLE_MEASURE)
        .into()
    }
    pub(super) fn failure_diagnostics(&self, incident: &Incident) -> Element<'static, Message> {
        let target = self.target;
        let open = self.failure_ui.expanded.contains(&incident.key);
        let mut rows = Vec::new();
        rows.push(
            widget::row::with_children(vec![
                crate::ui_controls::button::quiet(if open { "Hide details" } else { "Details" })
                    .name(format!(
                        "{} technical details for {}",
                        if open { "Hide" } else { "Show" },
                        incident.summary
                    ))
                    .on_press(action(Action::ToggleFailure(target, incident.key.clone())))
                    .into(),
                crate::ui_controls::button::quiet("Copy details")
                    .name(format!(
                        "Copy original diagnostics for {}",
                        incident.summary
                    ))
                    .on_press(action(Action::CopyFailure(target, incident.key.clone())))
                    .into(),
            ])
            .spacing(space::S)
            .into(),
        );
        if open {
            // Full bytes are resolved only on expansion/copy. The collapsed
            // surface never clones or shapes a provider's request dump.
            if let Some((preview, omitted)) = self.failure_preview(&incident.key) {
                rows.push(
                    widget::container(
                        widget::scrollable(selectable_prose::logical_text(
                            vec![iced::widget::text::Span::new(preview.to_owned())].into(),
                            design::TEXT_CAPTION.into(),
                            iced::Font::MONOSPACE,
                            target,
                            None,
                            selectable_prose::Logical {
                                identity: diagnostic_preview_identity(&incident.key),
                                start: 0,
                            },
                        ))
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .height(Length::Shrink),
                    )
                    .max_height(240)
                    .into(),
                );
                if omitted {
                    rows.push(design::caption("Diagnostic preview is limited to 8 KiB. Copy details includes the complete originals.").width(Length::Fill).into());
                }
            }
        }
        widget::column::with_children(rows)
            .spacing(space::S)
            .width(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    mod chronology_tests {
        include!("failure/chronology_tests.rs");
    }
    use super::*;

    fn text(id: &str, role: chat::Role, body: &str) -> chat::Item {
        chat::Item {
            id: id.into(),
            role,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        }
    }

    fn image(id: &str) -> chat::Item {
        chat::Item {
            id: id.into(),
            role: chat::Role::User,
            at: None,
            started_at: None,
            content: ItemContent::Image {
                path: String::new(),
                description: "Synthetic image".into(),
                reference: None,
            },
        }
    }

    fn provider_error(provider: Provider, id: &str, body: &str) -> chat::Item {
        let mut item = text(id, chat::Role::System, body);
        assert!(chat::failure::mark_provider_error(provider, &mut item));
        item
    }

    /// The content-part ID a provider's decoder assigns to one message.
    fn part(provider: Provider, message: &str, index: usize) -> String {
        match provider {
            Provider::Codex => format!("{message}/input/{index}"),
            Provider::Claude | Provider::Omp => format!("{message}:{index}"),
        }
    }

    fn delivery(id: &str, summary: &str) -> chat::Item {
        let mut item = text(id, chat::Role::User, "");
        item.content = ItemContent::Delivery {
            kind: chat::DeliveryKind::Notice,
            from: String::new(),
            sender: String::new(),
            summary: summary.into(),
            body: String::new(),
        };
        item
    }

    fn tool(id: &str) -> chat::Item {
        let mut item = text(id, chat::Role::Assistant, "");
        item.content = ItemContent::Tool {
            name: "Bash".into(),
            input: "ls".into(),
            output: String::new(),
            status: chat::ToolStatus::Succeeded,
        };
        item
    }

    fn live_fixture(provider: Provider, items: Vec<chat::Item>) -> (Runtime, Arc<ClientHandle>) {
        let session = fresh_chat_session(
            provider,
            TerminalId::new(),
            "/project".into(),
            &Account::system(provider),
            false,
        )
        .unwrap();
        let snapshot = Snapshot {
            ready: true,
            items: items.into_iter().map(Arc::new).collect(),
            ..Snapshot::default()
        };
        let handle = Arc::new(ClientHandle::fixture(session.clone(), snapshot));
        let runtime = Runtime::new(
            Target {
                card: CardId::new(),
                generation: 1,
            },
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        (runtime, handle)
    }

    fn publish(runtime: &mut Runtime, handle: &ClientHandle, edit: impl FnOnce(&mut Snapshot)) {
        let mut next = (*handle.snapshot()).clone();
        next.generation += 1;
        edit(&mut next);
        handle.publish_snapshot_for_test(next);
        runtime.refresh();
    }

    fn publish_error(runtime: &mut Runtime, handle: &ClientHandle, error: &str) {
        publish(runtime, handle, |next| {
            next.error = Some(error.into());
            next.error_generation += 1;
            next.error_item = None;
        });
    }

    /// Whether the incident's notice is part of the visible transcript.
    fn notice_visible(runtime: &mut Runtime, key: &str) -> bool {
        runtime.failure_ui.expanded.insert(key.to_owned());
        runtime.cache_failure_preview(key);
        runtime
            .selection_document()
            .0
            .text(diagnostic_preview_identity(key))
            .is_some()
    }

    #[test]
    fn multipart_prompt_failure_shows_one_notice() {
        let raw = "Failed to authenticate: OAuth session expired and could not be refreshed";
        let message = "11111111-1111-4111-8111-111111111111";
        for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
            let mut items = vec![text(
                &part(provider, message, 0),
                chat::Role::User,
                "Inspect these images",
            )];
            items.extend((1..=8).map(|index| image(&part(provider, message, index))));
            let (mut runtime, handle) = live_fixture(provider, items);
            publish_error(&mut runtime, &handle, raw);

            // The retained note (first part) and the current error (scoped by
            // the last image part) must resolve to the same message.
            assert_eq!(runtime.failures.len(), 1);
            assert_eq!(
                runtime.failure_ui.incidents.len(),
                1,
                "one notice per message, not per content part: {provider:?}"
            );
            let incident = &runtime.failure_ui.incidents[0];
            assert_eq!(
                incident.anchor.as_deref(),
                Some(part(provider, message, 0).as_str()),
                "drawn after the prompt's only group"
            );
            let key = incident.key.clone();
            assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw));
            assert!(notice_visible(&mut runtime, &key));

            let revision = runtime.failure_ui.revision;
            runtime.refresh();
            assert_eq!(runtime.failure_ui.revision, revision);
        }
    }

    #[test]
    fn notice_stays_below_a_reply_that_spans_several_groups() {
        let provider = Provider::Claude;
        let message = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![
                text(&part(provider, message, 0), chat::Role::User, "Inspect"),
                image(&part(provider, message, 1)),
                text("reply-prose", chat::Role::Assistant, "Checking the images."),
                tool("reply-tool"),
            ],
        );
        assert_eq!(
            runtime.turns.len(),
            2,
            "a tool after narration opens a group"
        );
        publish_error(&mut runtime, &handle, "Opaque provider failure");
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("reply-tool")
        );

        // A later exchange does not pull the retained notice into itself.
        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.items
                .push(Arc::new(text("next-message:0", chat::Role::User, "Next")));
        });
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("reply-tool")
        );
    }

    #[test]
    fn notice_follows_work_that_a_delivery_started_after_the_prompt() {
        let provider = Provider::Claude;
        let message = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![
                text(&part(provider, message, 0), chat::Role::User, "Inspect"),
                image(&part(provider, message, 1)),
                text(
                    "reply",
                    chat::Role::Assistant,
                    "Started a background check.",
                ),
                delivery("finished", "Background command completed"),
                tool("followup-tool"),
                text("followup", chat::Role::Assistant, "Reviewing its output."),
            ],
        );
        assert_eq!(
            runtime.turns.last().map(|turn| turn.key.as_str()),
            Some("finished"),
            "the delivery opens the group of the work it prompted"
        );
        let raw = "Opaque provider failure";
        publish_error(&mut runtime, &handle, raw);
        assert_eq!(
            runtime.failures[0].message.as_deref(),
            Some(part(provider, message, 0).as_str()),
            "a delivery is not the reader's prompt"
        );
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("finished"),
            "below the delivery's work, where the reader is"
        );

        // The same failure after more work is another occurrence. The old
        // notice remains beside the work that originally failed.
        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.items.extend([
                Arc::new(delivery("finished-again", "Background command completed")),
                Arc::new(text("more", chat::Role::Assistant, "Checking again.")),
            ]);
        });
        publish_error(&mut runtime, &handle, raw);
        assert_eq!(runtime.failures.len(), 2);
        assert_eq!(runtime.failure_ui.incidents.len(), 2);
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("finished")
        );
        assert_eq!(
            runtime.failure_ui.incidents[1].anchor.as_deref(),
            Some("finished-again")
        );

        // The reader's next prompt does not take the notice with it.
        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.items
                .push(Arc::new(text("next-message:0", chat::Role::User, "Next")));
        });
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("finished")
        );
    }

    /// A prompt, its reply, and work a background command's completion
    /// started, as a Claude transcript carries them.
    fn delivered_work(provider: Provider, message: &str) -> Vec<chat::Item> {
        vec![
            text(
                &part(provider, message, 0),
                chat::Role::User,
                "Run the tests",
            ),
            text(
                "reply",
                chat::Role::Assistant,
                "Started them in the background.",
            ),
            delivery("finished", "Background command completed"),
            tool("followup-tool"),
            text("followup", chat::Role::Assistant, "Reviewing their output."),
        ]
    }

    #[test]
    fn notice_keeps_its_identity_and_place_after_its_prompt_pages_out() {
        let provider = Provider::Claude;
        let message = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
        let raw = "Opaque provider failure";
        let (mut runtime, handle) = live_fixture(provider, delivered_work(provider, message));
        publish_error(&mut runtime, &handle, raw);
        let key = runtime.failure_ui.incidents[0].key.clone();
        runtime.failure_ui.expanded.insert(key.clone());

        // The latest page now starts at the delivery, the error still live.
        publish(&mut runtime, &handle, |page| {
            page.has_earlier = true;
            page.items.drain(..2);
        });
        assert_eq!(
            runtime.failure_ui.incidents.len(),
            1,
            "the live error joins its note"
        );
        let incident = &runtime.failure_ui.incidents[0];
        assert_eq!(incident.key, key);
        assert!(incident.sources.contains(&Source::Snapshot));
        assert_eq!(incident.anchor.as_deref(), Some("finished"));
        assert!(runtime.failure_ui.expanded.contains(&key));
        assert!(notice_visible(&mut runtime, &key));
    }

    #[test]
    fn a_failure_while_an_earlier_page_is_shown_is_recorded_on_the_latest_page() {
        let provider = Provider::Claude;
        let older = "12121212-1212-4212-8212-121212121212";
        let latest = "34343434-3434-4434-8434-343434343434";
        let raw = "Opaque provider failure";
        let mut items = vec![
            text(&part(provider, older, 0), chat::Role::User, "Earlier"),
            text("older-reply", chat::Role::Assistant, "Done."),
        ];
        items.extend(delivered_work(provider, latest));
        let (mut runtime, handle) = live_fixture(provider, items.clone());

        // The reader pages back to the older exchange; the failure arrives.
        publish(&mut runtime, &handle, |page| {
            page.has_later = true;
            page.items.truncate(2);
            page.error = Some(raw.into());
        });
        assert!(
            runtime.failures.is_empty(),
            "not attributed to the older prompt"
        );
        let key = runtime.failure_ui.incidents[0].key.clone();
        assert!(
            notice_visible(&mut runtime, &key),
            "the live error stays visible"
        );

        // Back on the latest page, it belongs to the latest prompt and is
        // drawn after the latest work.
        publish(&mut runtime, &handle, |page| {
            page.has_later = false;
            page.items = items.into_iter().map(Arc::new).collect();
        });
        assert_eq!(runtime.failures.len(), 1);
        assert_eq!(
            runtime.failures[0].message.as_deref(),
            Some(part(provider, latest, 0).as_str())
        );
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        let incident = &runtime.failure_ui.incidents[0];
        assert!(incident.sources.contains(&Source::Snapshot));
        assert_eq!(incident.anchor.as_deref(), Some("finished"));
        let key = incident.key.clone();
        assert!(notice_visible(&mut runtime, &key));
    }

    #[test]
    fn a_cause_recurring_with_new_wording_preserves_each_occurrence() {
        let provider = Provider::Claude;
        let message = "abababab-abab-4bab-8bab-abababababab";
        let first =
            "You've hit your limit · resets 1:30pm (America/Los_Angeles)\nrequest-id: first";
        let second = "API Error: rate_limit_error; resets at 1:30pm (America/Los_Angeles)\nrequest-id: second";
        let (mut runtime, handle) = live_fixture(provider, delivered_work(provider, message));
        publish_error(&mut runtime, &handle, first);
        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.items.extend([
                Arc::new(delivery("finished-again", "Background command completed")),
                Arc::new(text("more", chat::Role::Assistant, "Checking again.")),
            ]);
        });
        publish_error(&mut runtime, &handle, second);
        assert_eq!(runtime.failures.len(), 2);
        assert_eq!(runtime.failure_ui.incidents.len(), 2, "two occurrences");
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("finished")
        );

        // The first wording after still more work is a third occurrence.
        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.items.extend([
                Arc::new(delivery("finished-third", "Background command completed")),
                Arc::new(text("still-more", chat::Role::Assistant, "Once more.")),
            ]);
        });
        publish_error(&mut runtime, &handle, first);
        assert_eq!(runtime.failures.len(), 3);
        assert_eq!(runtime.failure_ui.incidents.len(), 3);
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some("finished")
        );
        assert_eq!(
            runtime.failure_ui.incidents[2].anchor.as_deref(),
            Some("finished-third")
        );
    }

    #[test]
    fn paged_out_first_part_keeps_the_notice_beside_the_remaining_part() {
        let provider = Provider::Claude;
        let message = "22222222-2222-4222-8222-222222222222";
        let first = part(provider, message, 0);
        let last = part(provider, message, 8);
        let raw = "Opaque provider failure\nexact original details";
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![text(&first, chat::Role::User, "Prompt"), image(&last)],
        );
        publish_error(&mut runtime, &handle, raw);
        let key = runtime.failure_ui.incidents[0].key.clone();

        publish(&mut runtime, &handle, |page| {
            page.has_earlier = true;
            page.items.retain(|item| item.id == last);
        });
        assert_eq!(runtime.failures[0].message.as_deref(), Some(first.as_str()));
        assert_eq!(runtime.failure_ui.incidents.len(), 1);
        assert_eq!(runtime.failure_ui.incidents[0].key, key);
        assert_eq!(
            runtime.failure_ui.incidents[0].anchor.as_deref(),
            Some(last.as_str())
        );
        assert!(notice_visible(&mut runtime, &key));

        // An unrelated prompt must not adopt the notice of an absent message.
        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.items = vec![Arc::new(text(
                "another-message:0",
                chat::Role::User,
                "Unrelated",
            ))];
        });
        let key = runtime.failure_ui.incidents[0].key.clone();
        assert!(!notice_visible(&mut runtime, &key));
    }

    #[test]
    fn distinct_messages_and_children_keep_identical_errors_separate() {
        let provider = Provider::Claude;
        let raw = "Opaque provider failure\nidentical complete details";
        let a = "33333333-3333-4333-8333-333333333333";
        let b = "44444444-4444-4444-8444-444444444444";
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![
                text(&part(provider, a, 0), chat::Role::User, "First"),
                image(&part(provider, a, 1)),
                provider_error(provider, "error-a:0", raw),
                text(&part(provider, b, 0), chat::Role::User, "Second"),
                image(&part(provider, b, 1)),
                provider_error(provider, "error-b:0", raw),
            ],
        );
        publish_error(&mut runtime, &handle, raw);
        assert_eq!(runtime.failure_ui.incidents.len(), 2);

        // Child failures reporting the same body after the multipart prompt
        // stay scoped to each child rather than merging into the parent.
        publish(&mut runtime, &handle, |next| {
            for id in ["child-a", "child-b"] {
                let mut call = text(id, chat::Role::Assistant, "");
                call.content = ItemContent::Tool {
                    name: "Agent".into(),
                    input: "Synthetic reviewer".into(),
                    output: String::new(),
                    status: chat::ToolStatus::Failed,
                };
                next.items.push(Arc::new(call));
            }
            next.subagents = ["child-a", "child-b"]
                .map(|id| chat::Subagent {
                    id: id.into(),
                    kind: "reviewer".into(),
                    description: "Synthetic child failure".into(),
                    model: None,
                    status: chat::ToolStatus::Failed,
                    waiting: false,
                    detail: raw.into(),
                    background: true,
                    items: vec![Arc::new(provider_error(provider, "child-error:0", raw))],
                })
                .into();
        });
        let incidents = &runtime.failure_ui.incidents;
        assert_eq!(incidents.len(), 4);
        assert_eq!(
            incidents
                .iter()
                .map(|incident| &incident.key)
                .collect::<HashSet<_>>()
                .len(),
            4
        );
        for id in ["child-a", "child-b"] {
            let scoped: Vec<_> = incidents
                .iter()
                .filter(|incident| incident.child_scope.as_deref() == Some(id))
                .collect();
            assert_eq!(scoped.len(), 1);
            assert_eq!(
                scoped[0].sources,
                vec![
                    Source::ChildItem(id.into(), "child-error:0".into()),
                    Source::ChildDetail(id.into()),
                ]
            );
        }
    }

    #[test]
    fn opaque_item_id_does_not_alias_a_message_with_the_same_spelling() {
        let provider = Provider::Claude;
        let raw = "Opaque provider failure";
        let (runtime, _) = live_fixture(
            provider,
            vec![
                text("m", chat::Role::User, "Legacy opaque item"),
                provider_error(provider, "error-legacy:0", raw),
                text("m:0", chat::Role::User, "Canonical message"),
                provider_error(provider, "error-message:0", raw),
            ],
        );
        assert_eq!(runtime.failure_ui.incidents.len(), 2);
    }

    #[test]
    fn error_is_attributed_to_the_incoming_message_within_a_shared_turn() {
        let raw = "Opaque provider failure\nexact original diagnostic";
        let a = "77777777-7777-4777-8777-777777777777";
        let b = "88888888-8888-4888-8888-888888888888";
        let c = "99999999-9999-4999-8999-999999999999";
        for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
            // One publication carries both adjacent prompts and the failure.
            let (mut runtime, handle) = live_fixture(provider, vec![]);
            publish(&mut runtime, &handle, |next| {
                next.items = vec![
                    Arc::new(text(&part(provider, a, 0), chat::Role::User, "First")),
                    Arc::new(image(&part(provider, a, 1))),
                    Arc::new(text(&part(provider, b, 0), chat::Role::User, "Second")),
                    Arc::new(image(&part(provider, b, 1))),
                ];
                next.error = Some(raw.into());
            });
            assert_eq!(runtime.turns.len(), 1, "adjacent prompts share a turn");
            assert_eq!(
                runtime.failures[0].message.as_deref(),
                Some(part(provider, b, 0).as_str())
            );
            assert_eq!(runtime.failure_ui.incidents.len(), 1, "{provider:?}");
            let incident = &runtime.failure_ui.incidents[0];
            assert!(incident.sources.contains(&Source::Note(0)));
            assert!(incident.sources.contains(&Source::Snapshot));
            assert_eq!(
                incident.anchor.as_deref(),
                Some(runtime.turns[0].key.as_str())
            );

            // A later message reporting the same diagnostic is a new incident.
            publish(&mut runtime, &handle, |next| {
                next.error = None;
                next.items.push(Arc::new(text(
                    &part(provider, c, 0),
                    chat::Role::User,
                    "Third",
                )));
                next.items.push(Arc::new(image(&part(provider, c, 1))));
            });
            publish_error(&mut runtime, &handle, raw);
            assert_eq!(runtime.failures.len(), 2);
            assert_eq!(runtime.failure_ui.incidents.len(), 2);
        }
    }

    #[test]
    fn opaque_note_follows_its_visible_item_and_never_borrows_another() {
        let raw = "Opaque provider failure\ncomplete original diagnostic";
        for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
            let (mut runtime, handle) = live_fixture(
                provider,
                vec![
                    text("legacy-first", chat::Role::User, "First"),
                    text("legacy-second", chat::Role::User, "Second"),
                ],
            );
            publish_error(&mut runtime, &handle, raw);
            // The note belongs to the later item but displays on the turn
            // that contains it, keyed by the first item.
            assert_eq!(
                runtime.failures[0].message.as_deref(),
                Some("legacy-second")
            );
            assert_eq!(
                runtime.failure_ui.incidents[0].anchor.as_deref(),
                Some("legacy-first")
            );
            let key = runtime.failure_ui.incidents[0].key.clone();
            assert!(notice_visible(&mut runtime, &key));

            publish(&mut runtime, &handle, |next| next.error = None);
            assert!(
                notice_visible(&mut runtime, &key),
                "clearing the error keeps the retained note"
            );

            // Without its group or item there is no proof of where the
            // note belongs.
            publish(&mut runtime, &handle, |next| {
                next.items = vec![Arc::new(text("legacy-third", chat::Role::User, "Third"))];
            });
            assert!(!notice_visible(&mut runtime, &key), "{provider:?}");
        }
    }

    #[test]
    fn auth_recovery_after_paging_clears_only_the_current_message_warning() {
        let provider = Provider::Codex;
        let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let raw = "Authentication required. Please sign in again.";
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![
                text(&part(provider, a, 0), chat::Role::User, "First"),
                image(&part(provider, a, 1)),
            ],
        );
        publish_error(&mut runtime, &handle, raw);
        let first_note = runtime.failures[0].clone();

        // A changed error after B joins the same visual turn must not replace A.
        publish(&mut runtime, &handle, |next| {
            next.items.push(Arc::new(text(
                &part(provider, b, 0),
                chat::Role::User,
                "Second",
            )));
            next.items.push(Arc::new(image(&part(provider, b, 1))));
            next.error = Some(chat::sign_in_verification_failure(
                raw,
                "Synthetic verification failure",
            ));
        });
        assert_eq!(runtime.turns.len(), 1);
        assert_eq!(runtime.failures.len(), 2);
        assert_eq!(runtime.failures[0], first_note);
        let active = |runtime: &Runtime, index: usize| {
            let note = &runtime.failures[index];
            runtime.active_auth_notice(&note.message, &note.text)
        };
        assert!(!active(&runtime, 0));
        assert!(active(&runtime, 1));

        // Paging B's first part away keeps B's scope for dedup and recovery.
        publish(&mut runtime, &handle, |page| {
            page.has_earlier = true;
            page.items.retain(|item| item.id == part(provider, b, 1));
        });
        assert!(active(&runtime, 1));
        runtime.note(runtime.failures[1].text.clone());
        assert_eq!(runtime.failures.len(), 2);

        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.busy = false;
        });
        assert_eq!(runtime.failures, vec![first_note]);
    }
}
