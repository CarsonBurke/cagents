//! One logical answer, parsed as one Markdown document.
//!
//! The transcript store presents a long message as parts of at most
//! [`chat::TRANSCRIPT_PART_BYTES`]. Parsed one at a time, a code fence, table,
//! list or emphasis that crosses a cut breaks, and a link reference defined in
//! a later part is lost. Before presentation the parts a window holds are
//! joined back into the message they came from. When the window holds only
//! some of them, the complete message is read through the same cache-only
//! source reads that Copy uses, and shown once it is whole; until then the
//! parts that are present are joined as they are. A message larger than a
//! whole window is never read whole: like the rest of the transcript, it is
//! paged, and only the parts the window holds are joined.
use super::*;
use std::sync::Weak;

/// The largest message read whole. Every message presented costs the view
/// work for each of its blocks on every frame; the store's window bounds that
/// work, so a message read whole may add at most another window's worth.
const READ_BYTES: usize = chat::TRANSCRIPT_WINDOW_BYTES;
/// Complete messages kept for windows that show only part of them, across the
/// application: the larger of two at a window's edges can still be read while
/// the other stays resident.
const HYDRATED_BYTES: usize = 2 * READ_BYTES;
/// Reads that fail are retried after a growing delay, then left alone until
/// the message is rewritten or its pane is shown again.
const ATTEMPTS: u32 = 3;

fn hydrated_budget() -> Arc<tokio::sync::Semaphore> {
    application_capacity!(HYDRATED_BYTES)
}

/// Complete reads for presentation run one at a time across the application,
/// so they never take the slots Copy needs.
fn read_slots() -> Arc<tokio::sync::Semaphore> {
    application_capacity!(1)
}

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

/// A complete message, charged to the application's budget while held.
struct Hydrated {
    text: Arc<str>,
    _budget: tokio::sync::OwnedSemaphorePermit,
}

/// The bytes of one message that a presented text item holds. Within one
/// [`Key`] the source only grows by appending: the store bumps the revision on
/// any change it has not verified as an append, and publishes each item with
/// its source. Nothing here checks the text again, so an item holding a longer
/// range from the same start extends one holding a shorter range on the
/// strength of that store rule alone.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    item: String,
    key: Key,
    start: usize,
    end: usize,
}

struct Merged {
    parts: Vec<usize>,
    hydrated: usize,
    item: Arc<chat::Item>,
}

#[derive(Default)]
pub(super) struct State {
    hydrated: HashMap<Key, Hydrated>,
    failures: HashMap<Key, u32>,
    request: Option<(chat::TextCopySource, u32)>,
    merged: HashMap<String, Merged>,
    /// Spans of presented items that are still alive, by item address.
    spans: HashMap<usize, (Weak<chat::Item>, Span)>,
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

/// The message a presented text item belongs to and where its bytes start:
/// a later part names both in its ID, anything else is its own message.
fn place(item: &chat::Item) -> Option<(&str, usize, usize)> {
    let (body, ..) = text(item)?;
    Some(match chat::TranscriptPart::parse(&item.id) {
        Some(part)
            if part.layout == "text"
                && part.fields == [(part.fields[0].0, part.fields[0].0 + body.len()), (0, 0)] =>
        {
            (part.item, part.index, part.fields[0].0)
        }
        _ => (item.id.as_str(), 0, 0),
    })
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

/// `sources` is `None` when the publication's sources do not align with its
/// items; then only the IDs vouch for a run.
fn run_at<'a>(
    items: &'a [Arc<chat::Item>],
    index: usize,
    sources: Option<&'a [Option<chat::TextCopySource>]>,
) -> Option<Run<'a>> {
    let source_at = |index: usize| sources.and_then(|sources| sources.get(index)?.as_ref());
    let first = &items[index];
    let (item, mut position, start) = place(first)?;
    let (body, ..) = text(first)?;
    let source = source_at(index);
    let key = source.and_then(Key::of);
    let mut end = start + body.len();
    let mut next = index + 1;
    // Every real part of a message carries that message's source.
    while let Some(candidate) = items.get(next)
        && continues(&items[next - 1], candidate, item, position + 1, end)
        && (sources.is_none() || source_at(next).and_then(Key::of) == key)
    {
        end += text(candidate).map_or(0, |(body, ..)| body.len());
        position += 1;
        next += 1;
    }
    // A source that does not reach the bytes the parts claim is not theirs.
    if source.is_some_and(|source| (source.bytes as usize) < end) {
        return None;
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
    fn record(&mut self, item: &Arc<chat::Item>, span: Span) {
        self.spans
            .entry(Arc::as_ptr(item) as usize)
            .or_insert_with(|| (Arc::downgrade(item), span));
    }

    fn span(&self, item: &Arc<chat::Item>) -> Option<&Span> {
        let (weak, span) = self.spans.get(&(Arc::as_ptr(item) as usize))?;
        // A live item at this address is the one recorded there.
        (std::ptr::eq(weak.as_ptr(), Arc::as_ptr(item)) && weak.strong_count() > 0).then_some(span)
    }

    /// Whether `current` presents the same bytes of the same message as
    /// `previous` and possibly more after them. A joined message is a new
    /// item on every publication, so the client's wire lineage cannot vouch
    /// for it; the source's own append-only revision does.
    pub(super) fn proves(&self, previous: &Arc<chat::Item>, current: &Arc<chat::Item>) -> bool {
        match (self.span(previous), self.span(current)) {
            (Some(old), Some(new)) => {
                old.item == new.item
                    && old.key == new.key
                    && old.start == new.start
                    && old.end <= new.end
            }
            _ => false,
        }
    }

    /// Whether `current` reaches at least as far into the same revision of
    /// the same message as `previous`. It may start elsewhere, so it is not an
    /// append, but everything `previous` shows is still true of the message:
    /// it can stay on screen while `current` is prepared.
    pub(super) fn supersedes(&self, previous: &Arc<chat::Item>, current: &Arc<chat::Item>) -> bool {
        match (self.span(previous), self.span(current)) {
            (Some(old), Some(new)) => {
                old.item == new.item && old.key == new.key && old.end <= new.end
            }
            _ => false,
        }
    }

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
        let aligned_sources = aligned.then_some(sources.as_slice());
        let mut items = Vec::with_capacity(raw.items.len());
        let mut projected_sources = Vec::with_capacity(sources.len());
        let mut merged = HashMap::new();
        let mut wanted = Vec::new();
        let mut needed = HashSet::new();
        let mut spans = Vec::new();
        let mut changed = false;
        let mut index = 0;
        while index < raw.items.len() {
            let source = aligned_sources.and_then(|sources| sources[index].as_ref());
            let key = source.and_then(Key::of);
            let run = run_at(&raw.items, index, aligned_sources);
            let hydrated = run.as_ref().and_then(|run| {
                let complete =
                    source.is_none_or(|source| run.start == 0 && source.bytes as usize <= run.end);
                let readable =
                    !complete && source.is_some_and(|source| source.bytes as usize <= READ_BYTES);
                // A complete read supplies everything up to its own length;
                // parts the window holds beyond it are a later append.
                let stored = readable
                    .then(|| key.as_ref().and_then(|key| self.hydrated.get(key)))
                    .flatten()
                    .map(|hydrated| hydrated.text.clone());
                let hydrated = stored.clone().filter(|text| text.len() >= run.start);
                if readable && let (Some(source), Some(key)) = (source, &key) {
                    // While a message streams, the window slides forward past
                    // what the last read covers. Read it again once the window
                    // has moved half its length, so a fresh read arrives
                    // before the stored one stops reaching the window.
                    let slack = (run.end - run.start) / 2;
                    if stored.is_none_or(|text| text.len() < run.start + slack) {
                        wanted.push(source.clone());
                    }
                    needed.insert(key.clone());
                }
                // A message whose reads have all failed will not be read
                // whole either, until it is rewritten or shown again.
                let exhausted = key
                    .as_ref()
                    .and_then(|key| self.failures.get(key))
                    .is_some_and(|failed| *failed >= ATTEMPTS);
                Some((hydrated, readable && !exhausted))
            });
            let mut unjoined = index + 1;
            // Joining never makes a message larger than preparation accepts,
            // and never presents one message twice: such a run's parts all
            // stay separate. Nor does it join a run that starts inside a
            // message that will not be read whole: parsed from a cut, one
            // structure misread at the start would run through the whole
            // window, and each slide of the window would prepare it all again.
            // Its parts stay separate, each preparing on its own.
            let run = run.zip(hydrated).filter(|(run, (hydrated, readable))| {
                if run.start > 0 && hydrated.is_none() && !readable {
                    unjoined = run.parts.end;
                    return false;
                }
                let joined = hydrated
                    .as_ref()
                    .map_or(run.end - run.start, |text| text.len().max(run.end));
                let fits = joined <= preparation::SOURCE_BYTES;
                if !fits && let Some(key) = &key {
                    // A read this message has outgrown is never used again.
                    needed.remove(key);
                }
                let joins = fits && !merged.contains_key(run.item);
                if !joins {
                    unjoined = run.parts.end;
                }
                joins
            });
            let Some((run, (hydrated, _))) = run else {
                for (offset, item) in raw.items[index..unjoined].iter().enumerate() {
                    let key = aligned_sources
                        .and_then(|sources| sources[index + offset].as_ref())
                        .and_then(Key::of);
                    if let (Some(key), Some((message, _, start))) = (key, place(item)) {
                        let end = start + text(item).map_or(0, |(body, ..)| body.len());
                        spans.push((
                            item.clone(),
                            Span {
                                item: message.to_owned(),
                                key,
                                start,
                                end,
                            },
                        ));
                    }
                    items.push(item.clone());
                    if aligned {
                        projected_sources.push(sources[index + offset].clone());
                    }
                }
                index = unjoined;
                continue;
            };
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
            if let Some(key) = &key {
                let start = if hydrated.is_some() { 0 } else { run.start };
                let end = start + text(&item).map_or(0, |(body, ..)| body.len());
                spans.push((
                    item.clone(),
                    Span {
                        item: run.item.to_owned(),
                        key: key.clone(),
                        start,
                        end,
                    },
                ));
            }
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
        self.spans.retain(|_, (item, _)| item.strong_count() > 0);
        for (item, span) in spans {
            self.record(&item, span);
        }
        // Keep complete reads only for messages this window still cuts.
        self.hydrated.retain(|key, _| needed.contains(key));
        self.failures.retain(|key, _| needed.contains(key));
        let budget = hydrated_budget().available_permits();
        let next = wanted.into_iter().find(|source| {
            source.bytes as usize <= budget
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
            // Copy finds a message's source by position. Without aligned
            // sources none can be trusted, so Copy uses what is shown.
            snapshot.text_copy_sources.conversation = if aligned {
                projected_sources
            } else {
                Vec::new()
            };
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
        let budget = match &result {
            Ok(text) if text.len() == source.bytes as usize => u32::try_from(text.len())
                .ok()
                .and_then(|bytes| hydrated_budget().try_acquire_many_owned(bytes).ok()),
            _ => None,
        };
        match (result, budget) {
            (Ok(text), Some(budget)) => {
                self.failures.remove(&key);
                self.hydrated.insert(
                    key,
                    Hydrated {
                        text: text.into(),
                        _budget: budget,
                    },
                );
            }
            _ => {
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
    /// Not part of the read's identity.
    slots: Arc<tokio::sync::Semaphore>,
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
            slots: read_slots(),
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
        // The slot stays taken until the blocking read returns, even when a
        // hidden pane drops this subscription first.
        let slot = read.slots.clone().acquire_owned().await;
        let result = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            handle.copy_text(source)
        })
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
    fn a_window_sliding_past_its_read_asks_for_a_fresh_one_before_losing_it() {
        let message = "streamed line\n".repeat(16 * PART / 14);
        let window = |end: usize| {
            let count = end.div_ceil(PART);
            snapshot(
                parts("answer", &message[..end], count - 3..count),
                Some(source(end)),
            )
        };
        let mut state = State::default();
        state.project(window(6 * PART));
        assert!(state.complete(&source(6 * PART), Ok(message[..6 * PART].to_owned())));
        let mut end = 6 * PART;
        // While the window still starts inside the read, every publication
        // shows the whole message; a fresh read is asked for before it ends.
        let asked = loop {
            end += PART / 4;
            let projected = state.project(window(end));
            assert_eq!(body(&projected.items[0]), &message[..end]);
            if let Some((read, _)) = state.request() {
                break read.clone();
            }
        };
        assert_eq!(asked, source(end));
        let start = (end.div_ceil(PART) - 3) * PART;
        assert!(
            start + PART <= 6 * PART,
            "asked with {} bytes of the read still ahead of the window",
            6 * PART - start
        );
        assert!(state.complete(&asked, Ok(message[..end].to_owned())));
        assert!(state.request().is_none());
    }

    #[test]
    fn failed_reads_retry_a_bounded_number_of_times_and_rewrites_drop_old_text() {
        let message = message();
        let raw = snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(source(message.len())),
        );
        let mut state = State::default();
        // While a read may still come, the parts the window holds are joined.
        assert_eq!(state.project(raw.clone()).items.len(), 1);
        for attempt in 0..ATTEMPTS {
            assert_eq!(state.request().map(|(_, attempt)| attempt), Some(attempt));
            assert!(state.complete(&source(message.len()), Err("busy".into())));
            state.project(raw.clone());
        }
        assert!(state.request().is_none());
        // Once none will, they are parsed one by one rather than from the cut.
        let exhausted = state.project(raw.clone());
        assert!(Arc::ptr_eq(&exhausted, &raw));
        assert!(state.request().is_none());
        let mut rewritten = source(message.len());
        rewritten.revision += 1;
        let raw = snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(rewritten.clone()),
        );
        assert_eq!(state.project(raw).items.len(), 1);
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
    fn a_growing_message_proves_each_joined_publication_appends_to_the_last() {
        let message = message();
        let mut state = State::default();
        // Streaming starts with one part, shown as it is.
        let head = &message[..PART / 4];
        let first = snapshot(
            parts("answer", head, 0..usize::MAX),
            Some(source(head.len())),
        );
        let first = state.project(first);
        assert_eq!(first.items.len(), 1);
        let mut previous = first.items[0].clone();
        let mut grown_to = head.len();
        for end in [PART + PART / 3, 2 * PART + 7, message.len()] {
            let mut end = end.min(message.len());
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            let body = &message[..end];
            let projected = state.project(snapshot(
                parts("answer", body, 0..usize::MAX),
                Some(source(body.len())),
            ));
            let current = projected.items[0].clone();
            assert!(!Arc::ptr_eq(&previous, &current), "a joined item is new");
            assert!(
                state.proves(&previous, &current),
                "{grown_to} -> {end} bytes appends"
            );
            assert!(!state.proves(&current, &previous), "shrinking is no append");
            previous = current;
            grown_to = end;
        }
    }

    #[test]
    fn a_rewrite_or_a_window_from_another_start_is_not_an_append() {
        let message = message();
        let mut state = State::default();
        let whole = state.project(snapshot(
            parts("answer", &message, 0..usize::MAX),
            Some(source(message.len())),
        ));
        let whole = whole.items[0].clone();
        let mut rewritten = source(message.len());
        rewritten.revision += 1;
        let replaced = state.project(snapshot(
            parts("answer", &message, 0..usize::MAX),
            Some(rewritten),
        ));
        assert!(!state.proves(&whole, &replaced.items[0]));
        // A window paged past the first part presents bytes from elsewhere.
        let later = state.project(snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(source(message.len())),
        ));
        assert!(!state.proves(&whole, &later.items[0]));
        // Items this state never presented prove nothing.
        assert!(!state.proves(&text_item("a", "x"), &text_item("a", "xy")));
    }

    #[test]
    fn parts_whose_sources_disagree_or_fall_short_are_not_joined() {
        let message = message();
        let cut = parts("answer", &message, 0..usize::MAX);
        let mut other = source(message.len());
        other.token += 1;
        let mut sources: Vec<_> = cut.iter().map(|_| Some(source(message.len()))).collect();
        *sources.last_mut().unwrap() = Some(other);
        let mut raw = Snapshot::default();
        raw.items = cut.clone();
        raw.text_copy_sources.conversation = sources;
        let projected = State::default().project(Arc::new(raw));
        assert_eq!(
            projected.items.len(),
            2,
            "the last part is another message's"
        );
        assert!(Arc::ptr_eq(&projected.items[1], cut.last().unwrap()));
        // A lone later part whose source ends before its own bytes is forged.
        let tail = parts("answer", &message, 2..3);
        let raw = snapshot(tail.clone(), Some(source(PART)));
        let mut state = State::default();
        let projected = state.project(raw.clone());
        assert!(Arc::ptr_eq(&projected, &raw));
        assert!(state.request().is_none());
    }

    #[test]
    fn joining_without_aligned_sources_leaves_copy_to_what_is_shown() {
        let message = message();
        let mut raw = Snapshot::default();
        raw.items = parts("answer", &message, 0..usize::MAX);
        raw.text_copy_sources.conversation = vec![Some(source(message.len()))];
        let projected = State::default().project(Arc::new(raw));
        assert_eq!(body(&projected.items[0]), message);
        assert!(projected.text_copy_sources.conversation.is_empty());
    }

    #[test]
    fn a_message_larger_than_preparation_accepts_keeps_its_parts() {
        let message = "word ".repeat(preparation::SOURCE_BYTES / 5 + PART);
        let cut = parts("answer", &message, 0..usize::MAX);
        let raw = snapshot(cut.clone(), Some(source(message.len())));
        let projected = State::default().project(raw.clone());
        assert!(Arc::ptr_eq(&projected, &raw));
    }

    #[test]
    fn a_message_larger_than_a_window_is_paged_not_read_whole() {
        let message = "word ".repeat((READ_BYTES + PART) / 5);
        let count = message.len().div_ceil(PART);
        // A window that starts inside it keeps its parts as they are.
        let tail = snapshot(
            parts("answer", &message, count - 4..count),
            Some(source(message.len())),
        );
        let mut state = State::default();
        assert!(Arc::ptr_eq(&state.project(tail.clone()), &tail));
        assert!(state.request().is_none());
        // A window holding its start joins what it holds.
        let head = snapshot(parts("answer", &message, 0..4), Some(source(message.len())));
        let projected = state.project(head);
        assert_eq!(projected.items.len(), 1);
        assert_eq!(body(&projected.items[0]), &message[..4 * PART]);
        assert!(state.request().is_none());
    }

    #[test]
    fn complete_reads_share_one_slot_and_one_budget_across_panes() {
        assert!(Arc::ptr_eq(&read_slots(), &read_slots()));
        assert_eq!(read_slots().available_permits(), 1);
        let budget = hydrated_budget();
        let before = budget.available_permits();
        let message = message();
        let mut state = State::default();
        state.project(snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(source(message.len())),
        ));
        assert!(state.complete(&source(message.len()), Ok(message.clone())));
        assert_eq!(budget.available_permits(), before - message.len());
        state.release();
        assert_eq!(budget.available_permits(), before);
        // A read the budget cannot hold counts as a failed attempt.
        let held = budget
            .clone()
            .try_acquire_many_owned(before as u32)
            .unwrap();
        state.project(snapshot(
            parts("answer", &message, 1..usize::MAX),
            Some(source(message.len())),
        ));
        assert!(
            state.request().is_none(),
            "no read starts without room for its text"
        );
        drop(held);
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
