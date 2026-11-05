//! One logical answer, parsed as one Markdown document.
//!
//! The transcript store presents a long message as parts of at most
//! [`chat::TRANSCRIPT_PART_BYTES`]. Parsed one at a time, a code fence, table,
//! list or emphasis that crosses a cut breaks, and a link reference defined in
//! a later part is lost. Before presentation the parts a window holds are
//! joined back into the message they came from. When the window holds only
//! some of them, the complete message is read through the same cache-only
//! source reads that Copy uses, and shown once it is whole; until then the
//! parts that are present are joined as they are.
use super::*;

/// Complete messages kept for windows that show only part of them, in total.
/// Copy's own limit bounds any single one, so the larger of two at a window's
/// edges can still be read while the other stays resident.
const HYDRATED_BYTES: usize = 2 * chat::TEXT_COPY_LIMIT_BYTES;
/// Reads that fail are retried after a growing delay, then left alone until
/// their source changes.
const ATTEMPTS: u32 = 3;

/// The identity a complete read is valid for. An append keeps it, so a read
/// made before an append still supplies the prefix.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    cache: uuid::Uuid,
    token: u64,
    revision: u64,
}
impl Key {
    fn of(source: &chat::TextCopySource) -> Option<Self> {
        (source.scope == chat::TextCopyScope::Conversation).then_some(Self {
            cache: source.cache,
            token: source.token,
            revision: source.revision,
        })
    }
}

struct Merged {
    parts: Vec<usize>,
    hydrated: usize,
    item: Arc<chat::Item>,
}

#[derive(Default)]
pub(super) struct State {
    hydrated: HashMap<Key, Arc<str>>,
    failures: HashMap<Key, u32>,
    request: Option<(chat::TextCopySource, u32)>,
    merged: HashMap<String, Merged>,
    /// The last publication projected, the state it was projected under, and
    /// the result: an unchanged publication keeps its exact presented Arc.
    last: Option<(Arc<Snapshot>, u64, Arc<Snapshot>)>,
    epoch: u64,
}

/// A run of adjacent parts of one message that a window presents.
struct Run<'a> {
    item: &'a str,
    start: usize,
    end: usize,
    parts: std::ops::Range<usize>,
}

fn text(item: &chat::Item) -> Option<(&str, bool, chat::Phase)> {
    match &item.content {
        ItemContent::Text {
            text,
            thinking,
            phase,
        } => Some((text, *thinking, *phase)),
        _ => None,
    }
}

/// The part that `next` claims to be must continue `previous` exactly: same
/// item, the next position, the next bytes, and the same text kind.
fn continues(
    previous: &chat::Item,
    next: &chat::Item,
    item: &str,
    index: usize,
    end: usize,
) -> bool {
    let Some(part) = chat::TranscriptPart::parse(&next.id) else {
        return false;
    };
    let (Some((_, thinking, phase)), Some((body, next_thinking, next_phase))) =
        (text(previous), text(next))
    else {
        return false;
    };
    part.item == item
        && part.index == index
        && part.layout == "text"
        && part.fields == [(end, end + body.len()), (0, 0)]
        && thinking == next_thinking
        && phase == next_phase
        && previous.role == next.role
}

fn run_at<'a>(
    items: &'a [Arc<chat::Item>],
    index: usize,
    source: Option<&chat::TextCopySource>,
) -> Option<Run<'a>> {
    let first = &items[index];
    let (body, ..) = text(first)?;
    let (item, mut position, start) = match chat::TranscriptPart::parse(&first.id) {
        Some(part)
            if part.layout == "text"
                && part.fields == [(part.fields[0].0, part.fields[0].0 + body.len()), (0, 0)] =>
        {
            (part.item, part.index, part.fields[0].0)
        }
        _ => (first.id.as_str(), 0, 0),
    };
    let mut end = start + body.len();
    let mut next = index + 1;
    while let Some(candidate) = items.get(next)
        && continues(&items[next - 1], candidate, item, position + 1, end)
    {
        end += text(candidate).map_or(0, |(body, ..)| body.len());
        position += 1;
        next += 1;
    }
    // A lone first part shows its own whole message unless its source says
    // the message continues past the window.
    // A single later part is only worth presenting differently once the
    // whole message can be read.
    let cut = next - index > 1
        || source.is_some_and(|source| {
            start > 0
                || (source.bytes as usize > end && body.len() + 4 > chat::TRANSCRIPT_PART_BYTES)
        });
    cut.then_some(Run {
        item,
        start,
        end,
        parts: index..next,
    })
}

impl State {
    /// Presents `raw` with each message's parts joined. Returns `raw` itself
    /// when no message in it is cut.
    pub(super) fn project(&mut self, raw: Arc<Snapshot>) -> Arc<Snapshot> {
        if let Some((previous, epoch, projected)) = &self.last
            && Arc::ptr_eq(previous, &raw)
            && *epoch == self.epoch
        {
            return projected.clone();
        }
        let sources = &raw.text_copy_sources.conversation;
        let aligned = sources.len() == raw.items.len();
        let source_at = |index: usize| aligned.then(|| sources[index].as_ref()).flatten();
        let mut items = Vec::with_capacity(raw.items.len());
        let mut projected_sources = Vec::with_capacity(sources.len());
        let mut merged = HashMap::new();
        let mut wanted = Vec::new();
        let mut needed = HashSet::new();
        let mut changed = false;
        let mut index = 0;
        while index < raw.items.len() {
            let source = source_at(index);
            let Some(run) = run_at(&raw.items, index, source) else {
                items.push(raw.items[index].clone());
                if aligned {
                    projected_sources.push(sources[index].clone());
                }
                index += 1;
                continue;
            };
            let key = source.and_then(Key::of);
            let complete =
                source.is_none_or(|source| run.start == 0 && source.bytes as usize <= run.end);
            // A complete read supplies everything up to its own length; parts
            // the window holds beyond it are a later append.
            let hydrated = (!complete)
                .then(|| key.as_ref().and_then(|key| self.hydrated.get(key)))
                .flatten()
                .filter(|text| text.len() >= run.start)
                .cloned();
            if !complete && let (Some(source), Some(key)) = (source, key) {
                let covered = hydrated.as_ref().map_or(0, |text| text.len().max(run.end));
                if covered < source.bytes as usize {
                    wanted.push(source.clone());
                }
                needed.insert(key);
            }
            let parts = &raw.items[run.parts.clone()];
            let pointers: Vec<usize> = parts
                .iter()
                .map(|part| Arc::as_ptr(part) as usize)
                .collect();
            let hydrated_len = hydrated.as_ref().map_or(0, |text| text.len());
            let item = match self.merged.remove(run.item) {
                Some(previous)
                    if previous.parts == pointers && previous.hydrated == hydrated_len =>
                {
                    previous.item
                }
                _ => {
                    let mut body = String::new();
                    let mut from = run.start;
                    if let Some(hydrated) = &hydrated {
                        body.push_str(hydrated);
                        from = hydrated.len();
                    }
                    let mut at = run.start;
                    for part in parts {
                        let (text, ..) = text(part).expect("a run holds only text");
                        let skip = from.saturating_sub(at).min(text.len());
                        body.push_str(&text[skip..]);
                        at += text.len();
                    }
                    let mut item = parts[0].as_ref().clone();
                    item.id = run.item.to_owned();
                    if let ItemContent::Text { text, .. } = &mut item.content {
                        *text = body;
                    }
                    Arc::new(item)
                }
            };
            merged.insert(
                run.item.to_owned(),
                Merged {
                    parts: pointers,
                    hydrated: hydrated_len,
                    item: item.clone(),
                },
            );
            items.push(item);
            if aligned {
                projected_sources.push(sources[run.parts.start].clone());
            }
            changed = true;
            index = run.parts.end;
        }
        self.merged = merged;
        // Keep complete reads only for messages this window still cuts.
        self.hydrated.retain(|key, _| needed.contains(key));
        self.failures.retain(|key, _| needed.contains(key));
        let budget =
            HYDRATED_BYTES.saturating_sub(self.hydrated.values().map(|text| text.len()).sum());
        let next = wanted.into_iter().find(|source| {
            source.bytes as usize <= chat::TEXT_COPY_LIMIT_BYTES
                && source.bytes as usize <= budget
                && Key::of(source).is_some_and(|key| {
                    self.failures
                        .get(&key)
                        .is_none_or(|failed| *failed < ATTEMPTS)
                })
        });
        self.request = match (self.request.take(), next) {
            // An append keeps the read in flight: its prefix and the parts
            // the window holds after it still make the whole message.
            (Some((current, attempt)), Some(next)) if Key::of(&current) == Key::of(&next) => {
                Some((current, attempt))
            }
            (_, Some(next)) => {
                let attempt = Key::of(&next)
                    .and_then(|key| self.failures.get(&key).copied())
                    .unwrap_or(0);
                Some((next, attempt))
            }
            (_, None) => None,
        };
        let projected = if changed {
            let mut snapshot = (*raw).clone();
            snapshot.items = items;
            if aligned {
                snapshot.text_copy_sources.conversation = projected_sources;
            }
            Arc::new(snapshot)
        } else {
            raw.clone()
        };
        self.last = Some((raw, self.epoch, projected.clone()));
        projected
    }

    /// The complete read this window still needs, if any.
    pub(super) fn request(&self) -> Option<(&chat::TextCopySource, u32)> {
        self.request
            .as_ref()
            .map(|(source, attempt)| (source, *attempt))
    }

    /// Accepts a finished read. Returns whether the presentation changes.
    pub(super) fn complete(
        &mut self,
        source: &chat::TextCopySource,
        result: Result<String, String>,
    ) -> bool {
        if self
            .request
            .as_ref()
            .is_none_or(|(current, _)| current != source)
        {
            return false;
        }
        let Some(key) = Key::of(source) else {
            return false;
        };
        self.request = None;
        match result {
            Ok(text) if text.len() == source.bytes as usize => {
                self.failures.remove(&key);
                self.hydrated.insert(key, text.into());
            }
            Ok(_) | Err(_) => {
                *self.failures.entry(key).or_default() += 1;
            }
        }
        // Projecting again either uses the text or schedules the next attempt.
        self.epoch = self.epoch.wrapping_add(1);
        true
    }

    /// The raw publication last presented, to project again after a read.
    pub(super) fn raw(&self) -> Option<Arc<Snapshot>> {
        self.last.as_ref().map(|(raw, ..)| raw.clone())
    }

    /// Drops every body this state holds: a hidden pane keeps none.
    pub(super) fn release(&mut self) {
        *self = Self::default();
    }
}

/// One complete read, run while its window still asks for it.
#[derive(Clone)]
pub(super) struct Read {
    target: Target,
    handle: Arc<ClientHandle>,
    source: chat::TextCopySource,
    attempt: u32,
}
impl PartialEq for Read {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && Arc::ptr_eq(&self.handle, &other.handle)
            && self.source == other.source
            && self.attempt == other.attempt
    }
}
impl Eq for Read {}
impl std::hash::Hash for Read {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
        self.target.hash(state);
        Arc::as_ptr(&self.handle).hash(state);
        self.source.cache.hash(state);
        self.source.token.hash(state);
        self.source.revision.hash(state);
        self.source.bytes.hash(state);
        self.attempt.hash(state);
    }
}

pub(super) fn subscriptions(runtime: &Runtime) -> Option<Subscription<Message>> {
    let (source, attempt) = runtime.logical_prose.request()?;
    let handle = runtime.handle()?.clone();
    Some(Subscription::run_with(
        Read {
            target: runtime.target,
            handle,
            source: source.clone(),
            attempt,
        },
        read,
    ))
}

fn read(read: &Read) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    let read = read.clone();
    Box::pin(iced::futures::stream::once(async move {
        if read.attempt > 0 {
            tokio::time::sleep(Duration::from_secs(u64::from(read.attempt))).await;
        }
        let handle = read.handle.clone();
        let source = read.source.clone();
        let result = tokio::task::spawn_blocking(move || handle.copy_text(source))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        Message::ChatUi(Action::LogicalProseRead(read.target, read.source, result))
    }))
}

#[cfg(test)]
/// Presents `message` exactly as the store's window does: the first part
/// under the item's ID and each later one under its part ID.
pub(super) fn parts(
    id: &str,
    message: &str,
    window: std::ops::Range<usize>,
) -> Vec<Arc<chat::Item>> {
    let mut cuts = vec![0];
    while *cuts.last().unwrap() < message.len() {
        let mut end = (cuts.last().unwrap() + chat::TRANSCRIPT_PART_BYTES).min(message.len());
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        cuts.push(end);
    }
    cuts.windows(2)
        .enumerate()
        .filter(|(index, _)| window.contains(index))
        .map(|(index, range)| {
            let part_id = if index == 0 {
                id.to_owned()
            } else {
                chat::TranscriptPart {
                    item: id,
                    index,
                    layout: "text",
                    fields: [(range[0], range[1]), (0, 0)],
                }
                .id()
            };
            tests::text_item(&part_id, &message[range[0]..range[1]])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PART: usize = chat::TRANSCRIPT_PART_BYTES;

    fn source(bytes: usize) -> chat::TextCopySource {
        chat::TextCopySource {
            cache: uuid::Uuid::from_u128(7),
            scope: chat::TextCopyScope::Conversation,
            token: 3,
            revision: 1,
            bytes: bytes as u64,
        }
    }

    pub(super) fn text_item(id: &str, body: &str) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        })
    }

    fn snapshot(
        items: Vec<Arc<chat::Item>>,
        source: Option<chat::TextCopySource>,
    ) -> Arc<Snapshot> {
        let mut snapshot = Snapshot::default();
        snapshot.text_copy_sources.conversation = items.iter().map(|_| source.clone()).collect();
        snapshot.items = items;
        Arc::new(snapshot)
    }

    fn message() -> String {
        // A fence opens in the first part and closes in the second; the
        // multibyte filler makes the cut land inside a character.
        let mut message = "λ".repeat(PART / 2 - 10);
        message.push_str("\n\n```rust\n");
        message.push_str(&"let crab = 🦀;\n".repeat(PART / 16));
        message.push_str("```\n\nAfter the fence.\n");
        message.push_str(&"tail ".repeat(PART / 5));
        message
    }

    fn body(item: &chat::Item) -> &str {
        text(item).unwrap().0
    }

    #[test]
    fn resident_parts_join_into_their_message_and_keep_its_source() {
        let message = message();
        let mut items = vec![text_item("before", "short")];
        items.extend(parts("answer", &message, 0..usize::MAX));
        items.push(text_item("after", "short"));
        assert!(items.len() > 4);
        let raw = snapshot(items, Some(source(message.len())));
        let mut state = State::default();
        let projected = state.project(raw.clone());
        let ids: Vec<_> = projected
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(ids, ["before", "answer", "after"]);
        assert_eq!(body(&projected.items[1]), message);
        assert_eq!(projected.text_copy_sources.conversation.len(), 3);
        assert_eq!(
            projected.text_copy_sources.conversation[1],
            Some(source(message.len()))
        );
        assert!(
            state.request().is_none(),
            "a resident message needs no read"
        );
        assert!(Arc::ptr_eq(&state.project(raw), &projected));
    }

    #[test]
    fn a_window_without_cut_messages_is_presented_unchanged() {
        let raw = snapshot(
            vec![text_item("a", "one"), text_item("b", "two")],
            Some(source(3)),
        );
        let mut state = State::default();
        assert!(Arc::ptr_eq(&state.project(raw.clone()), &raw));
        assert!(state.request().is_none());
    }

    #[test]
    fn a_message_the_window_cuts_is_read_whole_then_shown_complete() {
        let message = message();
        let tail = parts("answer", &message, 1..usize::MAX);
        assert_eq!(
            tail.len() + 1,
            parts("answer", &message, 0..usize::MAX).len()
        );
        assert!(tail.len() > 1, "the window holds several later parts");
        let raw = snapshot(tail, Some(source(message.len())));
        let mut state = State::default();
        let projected = state.project(raw.clone());
        assert_eq!(projected.items.len(), 1);
        assert_eq!(projected.items[0].id, "answer");
        assert_eq!(
            body(&projected.items[0]),
            &message[message.len() - body(&projected.items[0]).len()..]
        );
        let (request, attempt) = state.request().unwrap();
        assert_eq!((request.clone(), attempt), (source(message.len()), 0));
        assert!(
            !state.complete(&source(1), Ok("stale".into())),
            "only the requested read is accepted"
        );
        assert!(state.complete(&source(message.len()), Ok(message.clone())));
        let projected = state.project(raw);
        assert_eq!(body(&projected.items[0]), message);
        assert!(state.request().is_none());
    }

    #[test]
    fn a_lone_first_part_is_read_when_its_message_continues_past_the_window() {
        let message = message();
        let head = parts("answer", &message, 0..1);
        let raw = snapshot(head, Some(source(message.len())));
        let mut state = State::default();
        let projected = state.project(raw.clone());
        assert!(
            Arc::ptr_eq(&projected.items[0], &raw.items[0])
                || body(&projected.items[0]) == body(&raw.items[0])
        );
        assert!(state.request().is_some());
        assert!(state.complete(&source(message.len()), Ok(message.clone())));
        assert_eq!(body(&state.project(raw).items[0]), message);
    }

    #[test]
    fn a_complete_read_is_extended_by_parts_appended_after_it() {
        let message = message();
        let longer = format!("{message}{}", "more ".repeat(PART / 4));
        let mut state = State::default();
        let first = snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(source(message.len())),
        );
        state.project(first);
        assert!(state.complete(&source(message.len()), Ok(message.clone())));
        let grown = snapshot(
            parts("answer", &longer, 1..usize::MAX),
            Some(source(longer.len())),
        );
        let projected = state.project(grown);
        assert_eq!(body(&projected.items[0]), longer);
        assert!(
            state.request().is_none(),
            "the resident tail completes the earlier read"
        );
    }

    #[test]
    fn failed_reads_retry_a_bounded_number_of_times_and_rewrites_drop_old_text() {
        let message = message();
        let raw = snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(source(message.len())),
        );
        let mut state = State::default();
        state.project(raw.clone());
        for attempt in 0..ATTEMPTS {
            assert_eq!(state.request().map(|(_, attempt)| attempt), Some(attempt));
            assert!(state.complete(&source(message.len()), Err("busy".into())));
            state.project(raw.clone());
        }
        assert!(state.request().is_none());
        let mut rewritten = source(message.len());
        rewritten.revision += 1;
        let raw = snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(rewritten.clone()),
        );
        state.project(raw);
        assert_eq!(
            state.request().map(|(source, _)| source.clone()),
            Some(rewritten)
        );
    }

    #[test]
    fn claimed_part_ids_that_do_not_continue_their_bytes_stay_separate() {
        let message = message();
        let mut items = parts("answer", &message, 0..usize::MAX);
        // Same claimed item, but the bytes skip ahead.
        let forged = chat::TranscriptPart {
            item: "answer",
            index: items.len(),
            layout: "text",
            fields: [(message.len() + 5, message.len() + 9), (0, 0)],
        }
        .id();
        items.push(text_item(&forged, "nope"));
        items.push(text_item("opaque:native-part:1", "provider id"));
        let raw = snapshot(items, None);
        let mut state = State::default();
        let projected = state.project(raw);
        let ids: Vec<_> = projected
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(ids, ["answer", forged.as_str(), "opaque:native-part:1"]);
        assert_eq!(body(&projected.items[0]), message);
    }

    #[test]
    fn joined_markdown_keeps_a_fence_that_crosses_a_part_boundary() {
        let message = message();
        let lines = PART / 16;
        let fences = |body: &str| -> Vec<usize> {
            markdown::Content::parse(body)
                .items()
                .iter()
                .filter_map(|item| match item {
                    markdown::Item::CodeBlock { code, .. } => {
                        Some(code.matches("let crab").count())
                    }
                    _ => None,
                })
                .collect()
        };
        let cut = parts("answer", &message, 0..usize::MAX);
        assert!(
            cut.iter().all(|part| fences(body(part)) != [lines]),
            "no single part holds the whole fence"
        );
        let mut state = State::default();
        let projected = state.project(snapshot(cut, Some(source(message.len()))));
        assert_eq!(fences(body(&projected.items[0])), [lines]);
    }
}
