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

/// Where a retained note is drawn: the last group of its message's exchange,
/// as when it was recorded. A note names its message for identity, while a
/// reply can span several groups. After paging, a visible later part of the
/// same message stands in for an absent first part. `None` when no visible
/// prompt belongs to the note.
fn note_destination<'a>(
    runtime: &'a Runtime,
    anchor: &str,
    user_carriers: &HashMap<&str, String>,
) -> Option<&'a str> {
    let provider = runtime.session.launch.provider;
    let carrier = attachment_draft::message_key(provider, anchor)
        .filter(|carrier| user_carriers.contains_key(carrier));
    let start = runtime.turns.iter().position(|turn| {
        turn.prompt.iter().any(|index| {
            runtime.snapshot.items.get(*index).is_some_and(|item| {
                item.role == chat::Role::User
                    && !matches!(item.content, ItemContent::Delivery { .. })
                    && (item.id == anchor
                        || carrier.is_some_and(|carrier| {
                            attachment_draft::message_key(provider, &item.id) == Some(carrier)
                        }))
            })
        })
    })?;
    let replies = runtime.turns[start + 1..]
        .iter()
        .take_while(|turn| turn.prompt.is_empty())
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
                        .or_insert_with(|| format!("message\0{carrier}"));
                    context.clone_from(scope);
                } else {
                    // An opaque legacy ID cannot collide with a normalized
                    // message carrier that happens to have the same spelling.
                    context = format!("item\0{}", item.id);
                }
            }
            contexts.insert(item.id.as_str(), context.clone());
        }
        let causes = Self::carrier_causes(
            runtime,
            &runtime.snapshot.items,
            |item| Source::Item(item.id.clone()),
            |item| contexts.get(item.id.as_str()).cloned().unwrap_or_default(),
        );
        for item in &runtime.snapshot.items {
            state.push_item(
                runtime,
                item,
                Source::Item(item.id.clone()),
                contexts
                    .get(item.id.as_str())
                    .map(String::as_str)
                    .unwrap_or(""),
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

        for (index, (_, text)) in runtime.failures.iter().enumerate() {
            if !runtime.expected_remote_close_error(text) {
                state.push(
                    provider,
                    text,
                    Source::Note(index),
                    false,
                    runtime.failures[index]
                        .0
                        .as_deref()
                        .and_then(|id| {
                            contexts.get(id).or_else(|| {
                                // Paging may retain an image while the first
                                // prompt part anchoring this note is absent.
                                // Only a visible genuine User carrier proves
                                // this relation; never infer it from prose or
                                // a tool/group key alone.
                                attachment_draft::message_key(provider, id)
                                    .and_then(|carrier| user_carriers.get(carrier))
                            })
                        })
                        .map(String::as_str)
                        .unwrap_or(""),
                );
            }
        }
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
                state.push(provider, text, source, false, &context);
            }
        }
        let recovering = runtime.auth_error().is_some();
        for incident in &mut state.incidents {
            incident.active_recovery = recovering
                && incident.kind == Some(FailureKind::Authentication)
                && incident.sources.iter().any(|source| match source {
                    Source::Local(_) | Source::Snapshot => true,
                    Source::Note(index) => runtime.active_auth_notice(
                        &runtime.failures[*index].0,
                        &runtime.failures[*index].1,
                    ),
                    Source::Item(id) => contexts
                        .get(id.as_str())
                        .is_some_and(|owner| owner == &context),
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
                    incident
                        .sources
                        .iter()
                        .find_map(|source| match source {
                            Source::Note(index) => {
                                Some(runtime.failures[*index].0.as_deref().map(|anchor| {
                                    note_destination(runtime, anchor, &user_carriers)
                                        .unwrap_or(anchor)
                                        .to_owned()
                                }))
                            }
                            _ => None,
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
            Source::Note(index) => self.failures.get(*index).map(|(_, text)| text.as_str()),
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
        publish(runtime, handle, |next| next.error = Some(error.into()));
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
            let key = runtime.failure_ui.incidents[0].key.clone();
            assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw));

            let revision = runtime.failure_ui.revision;
            runtime.refresh();
            assert_eq!(runtime.failure_ui.revision, revision);
        }
    }

    #[test]
    fn notice_stays_below_a_reply_that_spans_several_groups() {
        let provider = Provider::Claude;
        let message = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        let mut tool = text("reply-tool", chat::Role::Assistant, "");
        tool.content = ItemContent::Tool {
            name: "Bash".into(),
            input: "ls".into(),
            output: String::new(),
            status: chat::ToolStatus::Succeeded,
        };
        let (mut runtime, handle) = live_fixture(
            provider,
            vec![
                text(&part(provider, message, 0), chat::Role::User, "Inspect"),
                image(&part(provider, message, 1)),
                text("reply-prose", chat::Role::Assistant, "Checking the images."),
                tool,
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
        assert_eq!(runtime.failures[0].0.as_deref(), Some(first.as_str()));
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
                runtime.failures[0].0.as_deref(),
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
            assert_eq!(runtime.failures[0].0.as_deref(), Some("legacy-second"));
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

            // Without its item there is no proof of where the note belongs.
            publish(&mut runtime, &handle, |next| {
                next.items.retain(|item| item.id != "legacy-second");
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
        assert!(!runtime.active_auth_notice(&runtime.failures[0].0, &runtime.failures[0].1));
        assert!(runtime.active_auth_notice(&runtime.failures[1].0, &runtime.failures[1].1));

        // Paging B's first part away keeps B's scope for dedup and recovery.
        publish(&mut runtime, &handle, |page| {
            page.has_earlier = true;
            page.items.retain(|item| item.id == part(provider, b, 1));
        });
        assert!(runtime.active_auth_notice(&runtime.failures[1].0, &runtime.failures[1].1));
        runtime.note(runtime.failures[1].1.clone());
        assert_eq!(runtime.failures.len(), 2);

        publish(&mut runtime, &handle, |next| {
            next.error = None;
            next.busy = false;
        });
        assert_eq!(runtime.failures, vec![first_note]);
    }
}
