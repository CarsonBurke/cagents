//! Immutable rendered-text inputs and cancellable per-item match caching.
//!
//! The worker owns the cache. UI messages carry cheap Arc clones, never a
//! widget tree, renderer, or shaped paragraph.
use super::matched_ranges;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

// Poll every bounded batch of entries, including cache hits and result maps.
// A finished phase also checks before publishing or moving to an allocation.
const CANCEL_BATCH: usize = 256;
struct Cancellation<F> {
    current: F,
    visited: usize,
}
impl<F: FnMut(usize) -> bool> Cancellation<F> {
    fn new(current: F) -> Self {
        Self {
            current,
            visited: 0,
        }
    }
    fn check(&mut self) -> Option<()> {
        (self.current)(self.visited).then_some(())
    }
    fn entry(&mut self) -> Option<()> {
        self.visited = self.visited.wrapping_add(1);
        if self.visited % CANCEL_BATCH == 0 {
            self.check()?;
        }
        Some(())
    }
}

#[derive(Clone, Debug)]
pub(in crate::app::chat_ui) struct Block {
    pub identity: u64,
    pub text: Arc<str>,
}

#[derive(Clone, Debug)]
pub(in crate::app::chat_ui) struct Document {
    pub item: String,
    pub revision: u64,
    pub turn: String,
    pub blocks: Arc<[Block]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app::chat_ui) struct Match {
    pub turn: Arc<str>,
    pub item: Arc<str>,
    pub identity: u64,
    pub range: Range<usize>,
}

#[derive(Default)]
pub(in crate::app::chat_ui) struct Index {
    query: String,
    items: HashMap<String, Cached>,
}

struct Cached {
    revision: u64,
    ranges: Arc<[(u64, Range<usize>)]>,
}

impl Index {
    /// A generation changes on input or close. A superseded job returns no
    /// result, including while scanning one large rendered block.
    pub fn search(
        &mut self,
        documents: &[Document],
        query: &str,
        generation: u64,
        current: &AtomicU64,
    ) -> Option<Vec<Match>> {
        self.search_checked(
            documents,
            query,
            &mut Cancellation::new(|_| current.load(Ordering::Relaxed) == generation),
        )
    }
    fn search_checked(
        &mut self,
        documents: &[Document],
        query: &str,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<Vec<Match>> {
        let mut result = Vec::new();
        self.fill_checked(documents, query, &mut result, cancel)?;
        Some(result)
    }
    fn fill_checked(
        &mut self,
        documents: &[Document],
        query: &str,
        result: &mut Vec<Match>,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<()> {
        cancel.check()?;
        if self.query != query {
            self.query = query.to_owned();
            self.items.clear();
        }
        if query.is_empty() {
            self.items.clear();
            return Some(());
        }
        let mut present = HashSet::new();
        for document in documents {
            cancel.entry()?;
            present.insert(document.item.as_str());
        }
        let mut removed = Vec::new();
        for item in self.items.keys() {
            cancel.entry()?;
            if !present.contains(item.as_str()) {
                removed.push(item.clone());
            }
        }
        for item in removed {
            cancel.entry()?;
            self.items.remove(&item);
        }
        for document in documents {
            cancel.check()?;
            if self
                .items
                .get(&document.item)
                .is_none_or(|cached| cached.revision != document.revision)
            {
                let mut ranges = Vec::new();
                for block in document.blocks.iter() {
                    let found =
                        super::matched_ranges_cancellable(&block.text, query, usize::MAX, || {
                            cancel.check().is_some()
                        })?;
                    for range in found.ranges {
                        cancel.entry()?;
                        ranges.push((block.identity, range));
                    }
                }
                cancel.check()?;
                self.items.insert(
                    document.item.clone(),
                    Cached {
                        revision: document.revision,
                        ranges: ranges.into(),
                    },
                );
            }
            let turn: Arc<str> = Arc::from(document.turn.as_str());
            let item: Arc<str> = Arc::from(document.item.as_str());
            for (identity, range) in self.items[&document.item].ranges.iter() {
                cancel.entry()?;
                result.push(Match {
                    turn: turn.clone(),
                    item: item.clone(),
                    identity: *identity,
                    range: range.clone(),
                });
            }
        }
        cancel.check()?;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document(item: &str, turn: &str, revision: u64, blocks: &[(u64, &str)]) -> Document {
        Document {
            item: item.into(),
            turn: turn.into(),
            revision,
            blocks: blocks
                .iter()
                .map(|(identity, text)| Block {
                    identity: *identity,
                    text: Arc::from(*text),
                })
                .collect(),
        }
    }
    #[test]
    fn each_occurrence_keeps_render_order_and_utf8_offsets() {
        let mut index = Index::default();
        let generation = AtomicU64::new(4);
        let documents = [
            document("a", "first", 1, &[(11, "é cat cat"), (12, "CAT")]),
            document("b", "second", 1, &[(21, "cat")]),
        ];
        let matches = index.search(&documents, "cat", 4, &generation).unwrap();
        assert_eq!(
            matches
                .iter()
                .map(|found| (found.item.as_ref(), found.identity, found.range.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("a", 11, 3..6),
                ("a", 11, 7..10),
                ("a", 12, 0..3),
                ("b", 21, 0..3)
            ]
        );
    }
    #[test]
    fn changed_item_invalidates_only_its_cached_ranges_and_removed_items_leave() {
        let mut index = Index::default();
        let generation = AtomicU64::new(1);
        let mut documents = vec![
            document("a", "t", 1, &[(1, "cat")]),
            document("b", "t", 1, &[(2, "cat cat")]),
        ];
        index.search(&documents, "cat", 1, &generation).unwrap();
        let retained = index.items["a"].ranges.clone();
        documents[1] = document("b", "t", 2, &[(2, "dog")]);
        assert_eq!(
            index
                .search(&documents, "cat", 1, &generation)
                .unwrap()
                .len(),
            1
        );
        assert!(Arc::ptr_eq(&retained, &index.items["a"].ranges));
        documents.remove(0);
        index.search(&documents, "cat", 1, &generation).unwrap();
        assert!(!index.items.contains_key("a"));
    }
    #[test]
    fn superseded_query_never_publishes() {
        let mut index = Index::default();
        let generation = AtomicU64::new(2);
        assert!(
            index
                .search(
                    &[document("a", "t", 1, &[(1, "cat")])],
                    "cat",
                    1,
                    &generation
                )
                .is_none()
        );
    }
    #[test]
    fn match_count_is_not_the_paint_budget() {
        let text = "x ".repeat(5000);
        let generation = AtomicU64::new(1);
        let mut index = Index::default();
        assert_eq!(
            index
                .search(&[document("a", "t", 1, &[(1, &text)])], "x", 1, &generation)
                .unwrap()
                .len(),
            5000
        );
        assert_eq!(matched_ranges(&text, "x", 4096).ranges.len(), 4096);
    }
}

/// One accepted search result, shared by mounted fragments. Ranges refer to
/// whole logical blocks; a fragment intersects them without rescanning text.
/// An authoritative rendered sidecar for an item whose immutable metadata
/// does not itself own the complete displayed text. Callers prepare and memoize
/// this owner off-thread, retaining original logical leaf order and identities.
#[derive(Debug)]
pub(in crate::app::chat_ui) struct External {
    pub item: Arc<str>,
    pub revision: u64,
    pub blocks: Arc<[Block]>,
}

#[derive(Clone, Debug)]
pub(in crate::app::chat_ui) struct SourceProof {
    pub item: Weak<crate::chat::Item>,
    pub growing: bool,
    pub external: Option<Weak<External>>,
    pub supplemental: Option<Weak<External>>,
}
impl SourceProof {
    pub fn matches_supplemental(&self, current: Option<&Arc<External>>) -> bool {
        match (&self.supplemental, current) {
            (None, None) => true,
            (Some(proof), Some(current)) => proof.as_ptr() == Arc::as_ptr(current),
            _ => false,
        }
    }
    pub fn matches_external(&self, current: Option<&Arc<External>>) -> bool {
        match (&self.external, current) {
            (None, None) => true,
            (Some(proof), Some(current)) => proof.as_ptr() == Arc::as_ptr(current),
            _ => false,
        }
    }
}

#[derive(Clone, Default, Debug)]
pub(in crate::app::chat_ui) struct Paint {
    pub generation: u64,
    pub blocks: Arc<HashMap<u64, Arc<[Range<usize>]>>>,
    pub current: Option<Match>,
    pub sources: Arc<HashMap<String, SourceProof>>,
    pub signatures: Arc<HashMap<Arc<str>, u64>>,
}
impl Paint {
    pub fn new(generation: u64, matches: &[Match], current: Option<Match>) -> Self {
        Self::build(
            generation,
            matches,
            current,
            &mut Cancellation::new(|_| true),
        )
        .expect("uncancelled paint construction")
    }
    fn build(
        generation: u64,
        matches: &[Match],
        current: Option<Match>,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<Self> {
        cancel.check()?;
        let mut blocks: HashMap<u64, Vec<Range<usize>>> = HashMap::new();
        for found in matches {
            cancel.entry()?;
            blocks
                .entry(found.identity)
                .or_default()
                .push(found.range.clone());
        }
        let mut shared: HashMap<u64, Arc<[Range<usize>]>> = HashMap::new();
        for (identity, ranges) in blocks {
            cancel.entry()?;
            // Arc conversion is one allocation; check on both sides without
            // truncating the exact complete range slice shared by fragments.
            cancel.check()?;
            shared.insert(identity, Arc::from(ranges));
        }
        cancel.check()?;
        Some(Self {
            generation,
            blocks: Arc::new(shared),
            current,
            sources: Default::default(),
            signatures: Default::default(),
        })
    }
    /// Byte ranges belong to the exact immutable source parsed by the worker.
    /// A stable item/leaf ID alone is not a rendered-source revision proof.
    pub fn for_item(self: &Arc<Self>, item: &crate::chat::Item) -> Option<Arc<Self>> {
        self.sources
            .get(&item.id)
            .filter(|source| source.item.as_ptr() == item as *const _ && source.external.is_none())
            .filter(|_| self.signatures.contains_key(item.id.as_str()))
            .map(|_| self.clone())
    }
    pub fn for_external_item(
        self: &Arc<Self>,
        item: &crate::chat::Item,
        external: &Arc<External>,
    ) -> Option<Arc<Self>> {
        self.sources
            .get(&item.id)
            .filter(|source| {
                source.item.as_ptr() == item as *const _ && source.matches_external(Some(external))
            })
            .filter(|_| {
                external.item.as_ref() == item.id && self.signatures.contains_key(item.id.as_str())
            })
            .map(|_| self.clone())
    }
    pub fn fragment(
        &self,
        identity: u64,
        start: usize,
        length: usize,
    ) -> (Vec<Range<usize>>, Option<Range<usize>>) {
        let end = start.saturating_add(length);
        let clip = |range: &Range<usize>| {
            (range.start < end && range.end > start)
                .then(|| range.start.max(start) - start..range.end.min(end) - start)
        };
        let ranges = self
            .blocks
            .get(&identity)
            .map(|ranges| {
                let first = ranges.partition_point(|range| range.end <= start);
                ranges[first..]
                    .iter()
                    .take_while(|range| range.start < end)
                    .filter_map(clip)
                    .collect()
            })
            .unwrap_or_default();
        let current = self
            .current
            .as_ref()
            .filter(|found| found.identity == identity)
            .and_then(|found| clip(&found.range));
        (ranges, current)
    }
}

#[derive(Clone)]
pub(in crate::app::chat_ui) struct Results {
    pub matches: Arc<Vec<Match>>,
    pub blocks: Arc<HashMap<u64, Arc<[Range<usize>]>>>,
    pub revisions: HashMap<String, u64>,
    pub sources: Arc<HashMap<String, SourceProof>>,
    pub signatures: Arc<HashMap<Arc<str>, u64>>,
    pub turns: Arc<HashSet<Arc<str>>>,
    pub items: Arc<HashSet<Arc<str>>>,
    pub scope: u64,
    positions: HashMap<(Arc<str>, u64, usize, usize), usize>,
}
impl Results {
    pub fn new(matches: Vec<Match>) -> Self {
        Self::build(matches, &mut Cancellation::new(|_| true))
            .expect("uncancelled result construction")
    }
    fn build(
        matches: Vec<Match>,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<Self> {
        use std::hash::{Hash, Hasher};
        cancel.check()?;
        let mut item_signatures: HashMap<Arc<str>, std::hash::DefaultHasher> = HashMap::new();
        for found in &matches {
            cancel.entry()?;
            (found.identity, found.range.start, found.range.end)
                .hash(item_signatures.entry(found.item.clone()).or_default());
        }
        let mut signatures = HashMap::new();
        for (item, hash) in item_signatures {
            cancel.entry()?;
            signatures.insert(item, hash.finish());
        }
        let blocks = Paint::build(0, &matches, None, cancel)?.blocks;
        let mut positions = HashMap::new();
        let mut turns = HashSet::new();
        let mut items = HashSet::new();
        for (index, found) in matches.iter().enumerate() {
            cancel.entry()?;
            positions.insert(
                (
                    found.item.clone(),
                    found.identity,
                    found.range.start,
                    found.range.end,
                ),
                index,
            );
            turns.insert(found.turn.clone());
            items.insert(found.item.clone());
        }
        cancel.check()?;
        Some(Self {
            matches: Arc::new(matches),
            blocks,
            positions,
            revisions: HashMap::new(),
            sources: Default::default(),
            signatures: Arc::new(signatures),
            turns: Arc::new(turns),
            items: Arc::new(items),
            scope: 0,
        })
    }
    pub fn with_revisions(matches: Vec<Match>, inputs: &[Input], scope: u64) -> Self {
        Self::with_external_revisions(matches, inputs, scope, &HashMap::new())
    }
    pub fn with_external_revisions(
        matches: Vec<Match>,
        inputs: &[Input],
        scope: u64,
        external: &HashMap<String, Arc<External>>,
    ) -> Self {
        Self::with_composed_revisions(matches, inputs, scope, external, &HashMap::new())
    }
    pub fn with_composed_revisions(
        matches: Vec<Match>,
        inputs: &[Input],
        scope: u64,
        external: &HashMap<String, Arc<External>>,
        supplemental: &HashMap<String, Arc<External>>,
    ) -> Self {
        Self::build_composed(
            matches,
            inputs,
            scope,
            external,
            supplemental,
            &mut Cancellation::new(|_| true),
        )
        .expect("uncancelled source proof construction")
    }
    fn build_composed(
        matches: Vec<Match>,
        inputs: &[Input],
        scope: u64,
        external: &HashMap<String, Arc<External>>,
        supplemental: &HashMap<String, Arc<External>>,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<Self> {
        let mut result = Self::build(matches, cancel)?;
        let mut sources = HashMap::new();
        // Negative results need source proofs too: a previously unmatched
        // item may have changed while the worker was running.
        for input in inputs {
            cancel.entry()?;
            if result.items.contains(input.item.id.as_str()) {
                result
                    .revisions
                    .insert(input.item.id.clone(), input.revision);
            }
            sources.insert(
                input.item.id.clone(),
                SourceProof {
                    item: Arc::downgrade(&input.item),
                    growing: input.growing,
                    external: external.get(&input.item.id).map(Arc::downgrade),
                    supplemental: supplemental.get(&input.item.id).map(Arc::downgrade),
                },
            );
        }
        cancel.check()?;
        result.sources = Arc::new(sources);
        result.scope = scope;
        Some(result)
    }
    pub fn position(&self, found: &Match) -> Option<usize> {
        self.positions
            .get(&(
                found.item.clone(),
                found.identity,
                found.range.start,
                found.range.end,
            ))
            .copied()
    }
}

pub(in crate::app::chat_ui) struct Input {
    pub item: Arc<crate::chat::Item>,
    pub revision: u64,
    pub turn: String,
    pub growing: bool,
}
#[derive(Default)]
pub(in crate::app::chat_ui) struct Worker {
    index: Index,
    documents: HashMap<String, CachedDocument>,
    document_revision: u64,
}
struct CachedDocument {
    source: Weak<crate::chat::Item>,
    growing: bool,
    provider: crate::agent::Provider,
    external: Option<Weak<External>>,
    supplemental: Option<Weak<External>>,
    document: Document,
}
impl Worker {
    pub fn text_bytes(&self) -> usize {
        self.documents
            .values()
            .map(|cached| {
                cached
                    .document
                    .blocks
                    .iter()
                    .map(|block| block.text.len())
                    .sum::<usize>()
            })
            .sum()
    }
    pub fn search(
        &mut self,
        inputs: &[Input],
        provider: crate::agent::Provider,
        query: &str,
        generation: u64,
        current: &AtomicU64,
    ) -> Option<Vec<Match>> {
        self.search_with_external(
            inputs,
            provider,
            query,
            generation,
            current,
            &HashMap::new(),
        )
    }
    pub fn search_with_external(
        &mut self,
        inputs: &[Input],
        provider: crate::agent::Provider,
        query: &str,
        generation: u64,
        current: &AtomicU64,
        external: &HashMap<String, Arc<External>>,
    ) -> Option<Vec<Match>> {
        self.search_composed(
            inputs,
            provider,
            query,
            generation,
            current,
            external,
            &HashMap::new(),
        )
    }
    /// Supplemental blocks precede the ordinary document or its authoritative
    /// replacement, matching the Original disclosure before normal tool details.
    /// Each source family must use distinct stable logical leaf identities.
    pub fn search_composed(
        &mut self,
        inputs: &[Input],
        provider: crate::agent::Provider,
        query: &str,
        generation: u64,
        current: &AtomicU64,
        external: &HashMap<String, Arc<External>>,
        supplemental: &HashMap<String, Arc<External>>,
    ) -> Option<Vec<Match>> {
        self.search_composed_checked(
            inputs,
            provider,
            query,
            external,
            supplemental,
            &mut Cancellation::new(|_| current.load(Ordering::Relaxed) == generation),
        )
    }
    /// The mutex owns mutable caches only. Result materialization can be large
    /// and must neither hold it nor finish after its generation was cancelled.
    pub fn search_results(
        worker: &Mutex<Self>,
        inputs: &[Input],
        provider: crate::agent::Provider,
        query: &str,
        scope: u64,
        external: &HashMap<String, Arc<External>>,
        supplemental: &HashMap<String, Arc<External>>,
        mut current: impl FnMut() -> bool,
    ) -> Option<Results> {
        let mut cancel = Cancellation::new(|_| current());
        cancel.check()?;
        // Keep even a cancelled partial output outside the cache lock: dropping
        // its per-match Arc references can be as costly as constructing them.
        let mut matches = Vec::new();
        Self::fill_matches(
            worker,
            inputs,
            provider,
            query,
            external,
            supplemental,
            &mut matches,
            &mut cancel,
        )?;
        Results::build_composed(matches, inputs, scope, external, supplemental, &mut cancel)
    }
    /// Only the status crosses the lock boundary. The caller owns the partial
    /// output until after this returns, including when filling is cancelled.
    fn fill_matches(
        worker: &Mutex<Self>,
        inputs: &[Input],
        provider: crate::agent::Provider,
        query: &str,
        external: &HashMap<String, Arc<External>>,
        supplemental: &HashMap<String, Arc<External>>,
        matches: &mut Vec<Match>,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<()> {
        let mut worker = worker.lock().ok()?;
        worker.fill_composed_checked(
            inputs,
            provider,
            query,
            external,
            supplemental,
            matches,
            cancel,
        )
    }
    fn search_composed_checked(
        &mut self,
        inputs: &[Input],
        provider: crate::agent::Provider,
        query: &str,
        external: &HashMap<String, Arc<External>>,
        supplemental: &HashMap<String, Arc<External>>,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<Vec<Match>> {
        let mut matches = Vec::new();
        self.fill_composed_checked(
            inputs,
            provider,
            query,
            external,
            supplemental,
            &mut matches,
            cancel,
        )?;
        Some(matches)
    }
    fn fill_composed_checked(
        &mut self,
        inputs: &[Input],
        provider: crate::agent::Provider,
        query: &str,
        external: &HashMap<String, Arc<External>>,
        supplemental: &HashMap<String, Arc<External>>,
        matches: &mut Vec<Match>,
        cancel: &mut Cancellation<impl FnMut(usize) -> bool>,
    ) -> Option<()> {
        cancel.check()?;
        let mut present = HashSet::new();
        for input in inputs {
            cancel.entry()?;
            present.insert(input.item.id.as_str());
        }
        let mut removed = Vec::new();
        for id in self.documents.keys() {
            cancel.entry()?;
            if !present.contains(id.as_str()) {
                removed.push(id.clone());
            }
        }
        for id in removed {
            cancel.entry()?;
            self.documents.remove(&id);
        }
        let mut documents = Vec::with_capacity(inputs.len());
        for input in inputs {
            cancel.check()?;
            let extra = external.get(&input.item.id);
            let prefix = supplemental.get(&input.item.id);
            if extra
                .into_iter()
                .chain(prefix)
                .any(|owner| owner.item.as_ref() != input.item.id)
            {
                return None;
            }
            if self.documents.get(&input.item.id).is_none_or(|cached| {
                let same_external = match (&cached.external, extra) {
                    (None, None) => {
                        cached.source.as_ptr() == Arc::as_ptr(&input.item)
                            && cached.growing == input.growing
                    }
                    (Some(previous), Some(current)) => previous.as_ptr() == Arc::as_ptr(current),
                    _ => false,
                };
                let same_supplemental = match (&cached.supplemental, prefix) {
                    (None, None) => true,
                    (Some(previous), Some(current)) => previous.as_ptr() == Arc::as_ptr(current),
                    _ => false,
                };
                !same_external || !same_supplemental || cached.provider != provider
            }) {
                // A raw append can arrive while the UI still displays its old
                // prepared prefix and keeps that prefix's layout revision.
                // Index the actual immutable input, and invalidate the range
                // cache independently of that lagging presentation revision.
                self.document_revision = self.document_revision.wrapping_add(1);
                let mut parsed = extra.map_or_else(
                    || document(input, provider),
                    |owner| Document {
                        item: input.item.id.clone(),
                        revision: self.document_revision,
                        turn: input.turn.clone(),
                        blocks: owner.blocks.clone(),
                    },
                );
                if let Some(prefix) = prefix {
                    let mut blocks = Vec::new();
                    for block in prefix.blocks.iter().chain(parsed.blocks.iter()) {
                        cancel.entry()?;
                        blocks.push(block.clone());
                    }
                    cancel.check()?;
                    parsed.blocks = blocks.into();
                }
                parsed.revision = self.document_revision;
                self.documents.insert(
                    input.item.id.clone(),
                    CachedDocument {
                        source: Arc::downgrade(&input.item),
                        growing: input.growing,
                        provider,
                        external: extra.map(Arc::downgrade),
                        supplemental: prefix.map(Arc::downgrade),
                        document: parsed,
                    },
                );
            }
            let mut document = self.documents[&input.item.id].document.clone();
            document.turn.clone_from(&input.turn);
            documents.push(document);
        }
        self.index.fill_checked(&documents, query, matches, cancel)
    }
}
fn document(input: &Input, provider: crate::agent::Provider) -> Document {
    use crate::app::chat_ui::{coordination, markdown, markdown_source, selection};
    use crate::chat::{DeliveryKind, ItemContent, Role};
    let item = &input.item;
    let coordinated =
        crate::chat::coordination::recognize(item, provider).map(coordination::Coordinated::new);
    let mut builder = selection::Catalog::builder();
    if let Some(coordinated) = &coordinated {
        for (index, content) in coordinated.contents().enumerate() {
            builder.markdown(&item.id, &[1, index as u32], content.items());
        }
    }
    let mut parse = |body: &str| {
        let source = match markdown_source::Source::default().advance(body, input.growing) {
            markdown_source::Step::Append(text) | markdown_source::Step::Restart(text) => text,
        };
        let parsed = markdown::Content::parse(&source);
        builder.markdown(&item.id, &[0], parsed.items());
    };
    match &item.content {
        ItemContent::Text { text, .. } if item.role == Role::User => {
            builder.plain(&item.id, &[2], Arc::from(text.as_str()))
        }
        ItemContent::Text { text, .. } => parse(text),
        ItemContent::Delivery { kind, body, .. } if *kind != DeliveryKind::Notice => parse(body),
        ItemContent::Delivery { body, .. } => {
            builder.plain(&item.id, &[2], Arc::from(body.as_str()))
        }
        ItemContent::Command {
            command,
            label,
            working_directory,
            output,
            ..
        } => {
            if coordinated.is_none() && !label.is_empty() && label != command {
                builder.plain(
                    &item.id,
                    &[3, 0],
                    Arc::from(crate::app::chat_ui::turn::step_label(&item.content)),
                );
            }
            if !working_directory.is_empty() {
                builder.plain(&item.id, &[3, 1], Arc::from(working_directory.as_str()));
            }
            builder.plain(&item.id, &[4], Arc::from(command.as_str()));
            builder.plain(&item.id, &[5], Arc::from(output.as_str()));
        }
        ItemContent::Tool {
            name,
            input,
            output,
            ..
        } => {
            if coordinated.is_none() {
                builder.plain(&item.id, &[3, 0], Arc::from(name.as_str()));
            }
            builder.plain(&item.id, &[4], Arc::from(input.as_str()));
            builder.plain(&item.id, &[5], Arc::from(output.as_str()));
        }
        ItemContent::Changes { changes, .. } => {
            for (index, change) in changes.iter().enumerate() {
                builder.plain(
                    &item.id,
                    &[7, index as u32, 0],
                    Arc::from(change.path.as_str()),
                );
                if let crate::chat::ChangeKind::Renamed { from } = &change.kind {
                    builder.plain(&item.id, &[7, index as u32, 1], Arc::from(from.as_str()));
                }
                builder.plain(
                    &item.id,
                    &[6, index as u32],
                    Arc::from(change.diff.as_str()),
                );
            }
        }
        ItemContent::WebSearch { query, results } => {
            builder.plain(&item.id, &[8, 0], Arc::from(query.as_str()));
            for (index, result) in results.iter().enumerate() {
                if !result.title.is_empty() {
                    builder.plain(
                        &item.id,
                        &[8, 1, index as u32, 0],
                        Arc::from(result.title.as_str()),
                    );
                }
                if !result.url.is_empty() {
                    builder.plain(
                        &item.id,
                        &[8, 1, index as u32, 1],
                        Arc::from(result.url.as_str()),
                    );
                }
                if !result.snippet.is_empty() {
                    builder.plain(
                        &item.id,
                        &[8, 1, index as u32, 2],
                        Arc::from(result.snippet.as_str()),
                    );
                }
            }
        }
        ItemContent::Image { path, description } if item.role == Role::User => {
            builder.plain(
                &item.id,
                &[2],
                Arc::from(crate::app::chat_ui::Runtime::attachment_label(
                    path,
                    description,
                )),
            );
        }
        ItemContent::Image { path, description } => {
            if !description.is_empty() {
                builder.plain(&item.id, &[9, 0], Arc::from(description.as_str()));
            }
            if let Some(path) = super::image_path_caption(path) {
                builder.plain(&item.id, &[9, 1], Arc::from(path));
            }
        }
    }
    let blocks = builder
        .finish()
        .search_blocks()
        .into_iter()
        .map(|(identity, text)| Block { identity, text })
        .collect();
    Document {
        item: item.id.clone(),
        revision: input.revision,
        turn: input.turn.clone(),
        blocks,
    }
}

#[cfg(test)]
mod fragment_tests {
    use super::*;
    #[test]
    fn a_match_crossing_mount_chunks_keeps_both_halves_and_one_current_identity() {
        let found = Match {
            turn: "t".into(),
            item: "i".into(),
            identity: 7,
            range: 2046..2054,
        };
        let paint = Paint::new(1, &[found.clone()], Some(found));
        assert_eq!(
            paint.fragment(7, 0, 2048),
            (vec![2046..2048], Some(2046..2048))
        );
        assert_eq!(paint.fragment(7, 2048, 2048), (vec![0..6], Some(0..6)));
        assert_eq!(paint.fragment(8, 2048, 2048), (Vec::new(), None));
    }
}

#[cfg(test)]
mod rendered_tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{CommandAction, Item, ItemContent, Phase, Role, ToolStatus},
    };
    fn input(content: ItemContent) -> Input {
        Input {
            item: Arc::new(Item {
                id: "rendered-item".into(),
                role: Role::Assistant,
                content,
                at: None,
                started_at: None,
            }),
            revision: 1,
            turn: "turn".into(),
            growing: false,
        }
    }
    #[test]
    fn markdown_matches_plain_prose_and_fenced_code_in_render_order() {
        let input = input(ItemContent::Text {
            text: "## Heading\n\nA **needle** and inline `code`.\n\n```text\nneedle\n```\n".into(),
            thinking: false,
            phase: Phase::Final,
        });
        let document = document(&input, Provider::Claude);
        let generation = AtomicU64::new(1);
        let mut index = Index::default();
        let matches = index
            .search(&[document.clone()], "needle", 1, &generation)
            .unwrap();
        assert_eq!(matches.len(), 2);
        assert_ne!(matches[0].identity, matches[1].identity);
        for found in matches {
            let block = document
                .blocks
                .iter()
                .find(|block| block.identity == found.identity)
                .unwrap();
            assert_eq!(&block.text[found.range], "needle");
            assert!(!block.text.contains("**"));
        }
    }
    #[test]
    fn displayed_command_label_and_directory_remain_searchable() {
        let input = input(ItemContent::Command {
            label: "Verify RELEASE".into(),
            command: "true".into(),
            working_directory: "/Workspace/Project".into(),
            output: "done".into(),
            action: CommandAction::Run,
            status: ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: None,
        });
        let mut worker = Worker::default();
        let generation = AtomicU64::new(1);
        let label = worker
            .search(&[input], Provider::Claude, "release", 1, &generation)
            .unwrap();
        assert_eq!(label.len(), 1);
        assert_eq!(
            label[0].identity,
            crate::app::chat_ui::selection::logical_identity("rendered-item", &[3, 0])
        );
    }
    #[test]
    fn find_geometry_only_revision_preserves_the_parsed_document_and_match_ranges() {
        let mut source = input(ItemContent::Text {
            text: "A **needle** in unchanged rich prose.".repeat(400),
            thinking: false,
            phase: Phase::Final,
        });
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let before = worker
            .search(
                std::slice::from_ref(&source),
                Provider::Claude,
                "needle",
                1,
                &current,
            )
            .unwrap();
        let blocks = worker.documents[&source.item.id].document.blocks.clone();
        let ranges = worker.index.items[&source.item.id].ranges.clone();
        // FlowReady invalidates retained widget geometry, not rendered text.
        source.revision += 1;
        let after = worker
            .search(
                std::slice::from_ref(&source),
                Provider::Claude,
                "needle",
                1,
                &current,
            )
            .unwrap();
        assert_eq!(before, after);
        assert!(
            Arc::ptr_eq(&blocks, &worker.documents[&source.item.id].document.blocks),
            "geometry completion must not reparse unchanged source"
        );
        assert!(
            Arc::ptr_eq(&ranges, &worker.index.items[&source.item.id].ranges),
            "unchanged parsed leaves reuse their indexed occurrences"
        );
    }

    #[test]
    fn find_pending_markdown_append_does_not_reuse_the_old_layout_revision_document() {
        let source = |body: &str| {
            input(ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Phase::Final,
            })
        };
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let mut old = source("a *needle");
        old.growing = true;
        let first = worker
            .search(
                std::slice::from_ref(&old),
                Provider::Claude,
                "needle",
                1,
                &current,
            )
            .unwrap();
        assert_eq!(first[0].range, 3..9);
        let mut appended = source("a *needle*");
        appended.growing = true;
        assert_eq!(
            old.revision, appended.revision,
            "the renderer still owns its old prepared-prefix revision"
        );
        let next = worker
            .search(&[appended], Provider::Claude, "needle", 1, &current)
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(
            next[0].range,
            2..8,
            "closing Markdown syntax changes rendered offsets despite raw append lineage"
        );
    }

    #[test]
    fn find_image_local_path_matches_its_caption_without_indexing_hidden_payloads() {
        for (path, count) in [
            ("screens/needle.png", 1),
            ("https://images.example/needle.png", 0),
            ("http://images.example/needle.png", 0),
            ("data:image/png;needle", 0),
            ("", 0),
        ] {
            let source = input(ItemContent::Image {
                path: path.into(),
                description: String::new(),
            });
            let document = document(&source, Provider::Claude);
            let mut index = Index::default();
            let current = AtomicU64::new(1);
            let matches = index.search(&[document], "needle", 1, &current).unwrap();
            assert_eq!(
                matches.len(),
                count,
                "only the actual rendered path caption is indexed: {path}"
            );
            if let Some(found) = matches.first() {
                assert_eq!(
                    found.identity,
                    crate::app::chat_ui::selection::logical_identity(&source.item.id, &[9, 1])
                );
                assert_eq!(&path[found.range.clone()], "needle");
            }
        }
    }

    #[test]
    fn obsolete_query_cancels_inside_one_large_block() {
        let mut checks = 0;
        let result = super::super::matched_ranges_cancellable(
            &"x".repeat(300_000),
            "absent",
            usize::MAX,
            || {
                checks += 1;
                checks < 4
            },
        );
        assert!(result.is_none());
        assert_eq!(checks, 4);
    }
}

#[cfg(test)]
mod external_tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{Item, ItemContent, Role, ToolStatus},
    };
    fn input(id: &str) -> Input {
        Input {
            item: Arc::new(Item {
                id: id.into(),
                role: Role::Assistant,
                at: None,
                started_at: None,
                content: ItemContent::Changes {
                    status: ToolStatus::Succeeded,
                    changes: Vec::new(),
                },
            }),
            revision: 7,
            turn: format!("turn-{id}"),
            growing: false,
        }
    }
    fn owner(id: &str, revision: u64, blocks: &[(u64, &str)]) -> Arc<External> {
        Arc::new(External {
            item: id.into(),
            revision,
            blocks: blocks
                .iter()
                .map(|(identity, text)| Block {
                    identity: *identity,
                    text: Arc::from(*text),
                })
                .collect(),
        })
    }
    #[test]
    fn find_supplemental_preserves_output_or_changes_in_displayed_prefix_order() {
        let mut original = input("a");
        Arc::make_mut(&mut original.item).content = ItemContent::Tool {
            name: "tool".into(),
            input: "{}".into(),
            output: "needle output".into(),
            status: ToolStatus::Succeeded,
        };
        let inputs = [original];
        let raw_id = crate::app::chat_ui::selection::logical_identity("a", &[10, 0]);
        let output_id = crate::app::chat_ui::selection::logical_identity("a", &[5]);
        let raw = owner("a", 1, &[(raw_id, "needle original")]);
        let prefix = HashMap::from([("a".into(), raw.clone())]);
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let ordinary = worker
            .search_composed(
                &inputs,
                Provider::Claude,
                "needle",
                1,
                &current,
                &HashMap::new(),
                &prefix,
            )
            .unwrap();
        assert_eq!(
            ordinary
                .iter()
                .map(|found| found.identity)
                .collect::<Vec<_>>(),
            vec![raw_id, output_id]
        );
        let patches = HashMap::from([("a".into(), owner("a", 2, &[(60, "needle patch")]))]);
        let composed = worker
            .search_composed(
                &inputs,
                Provider::Claude,
                "needle",
                1,
                &current,
                &patches,
                &prefix,
            )
            .unwrap();
        assert_eq!(
            composed
                .iter()
                .map(|found| found.identity)
                .collect::<Vec<_>>(),
            vec![raw_id, 60]
        );
        assert!(Arc::ptr_eq(
            &worker.documents["a"].document.blocks[0].text,
            &raw.blocks[0].text
        ));
        assert!(Arc::ptr_eq(
            &worker.documents["a"].document.blocks[1].text,
            &patches["a"].blocks[0].text
        ));
        let result = Results::with_composed_revisions(composed, &inputs, 0, &patches, &prefix);
        assert!(result.sources["a"].matches_external(patches.get("a")));
        assert!(result.sources["a"].matches_supplemental(prefix.get("a")));
        assert!(!result.sources["a"].matches_supplemental(Some(&owner(
            "a",
            3,
            &[(raw_id, "needle rewritten")]
        ))));
        assert!(!result.sources["a"].matches_external(Some(&owner(
            "a",
            4,
            &[(60, "needle new patch")]
        ))));
    }

    #[test]
    fn find_supplemental_negative_proof_and_base_cache_both_track_exact_sources() {
        let mut inputs = [input("a")];
        Arc::make_mut(&mut inputs[0].item).content = ItemContent::Tool {
            name: "tool".into(),
            input: "{}".into(),
            output: "old".into(),
            status: ToolStatus::Succeeded,
        };
        let prefix = HashMap::from([("a".into(), owner("a", 1, &[(10, "original")]))]);
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let matches = worker
            .search_composed(
                &inputs,
                Provider::Claude,
                "needle",
                1,
                &current,
                &HashMap::new(),
                &prefix,
            )
            .unwrap();
        assert!(matches.is_empty());
        let old = Results::with_composed_revisions(matches, &inputs, 0, &HashMap::new(), &prefix);
        assert!(
            !old.sources["a"].matches_supplemental(None),
            "removing a prefix fences negative results too"
        );
        let previous = worker.documents["a"].document.revision;
        Arc::make_mut(&mut inputs[0].item).content = ItemContent::Tool {
            name: "tool".into(),
            input: "{}".into(),
            output: "needle".into(),
            status: ToolStatus::Succeeded,
        };
        let matches = worker
            .search_composed(
                &inputs,
                Provider::Claude,
                "needle",
                1,
                &current,
                &HashMap::new(),
                &prefix,
            )
            .unwrap();
        assert_eq!(
            matches.len(),
            1,
            "unchanged Original owner cannot hide a changed normal output"
        );
        assert_ne!(worker.documents["a"].document.revision, previous);
        let wrong = HashMap::from([("a".into(), owner("foreign", 2, &[(10, "needle")]))]);
        assert!(
            worker
                .search_composed(
                    &inputs,
                    Provider::Claude,
                    "needle",
                    1,
                    &current,
                    &HashMap::new(),
                    &wrong
                )
                .is_none()
        );
    }

    #[test]
    fn find_supplemental_proofs_are_weak_but_cached_actual_text_lives_until_release() {
        let inputs = [input("a")];
        let raw = owner("a", 1, &[(10, "needle original")]);
        let header = Arc::downgrade(&raw);
        let text = Arc::downgrade(&raw.blocks[0].text);
        let prefix = HashMap::from([("a".into(), raw)]);
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let matches = worker
            .search_composed(
                &inputs,
                Provider::Claude,
                "needle",
                1,
                &current,
                &HashMap::new(),
                &prefix,
            )
            .unwrap();
        let proof = Results::with_composed_revisions(matches, &inputs, 0, &HashMap::new(), &prefix);
        drop(prefix);
        assert!(
            header.upgrade().is_none(),
            "results retain no supplemental header"
        );
        assert!(
            text.upgrade().is_some(),
            "the worker still owns its composed text blocks"
        );
        drop(worker);
        assert!(
            text.upgrade().is_none(),
            "dropping idle worker releases actual body allocation"
        );
        assert!(
            proof.sources["a"]
                .supplemental
                .as_ref()
                .unwrap()
                .upgrade()
                .is_none()
        );
    }

    #[test]
    fn find_external_complete_documents_keep_original_root_file_and_leaf_order() {
        let inputs = [input("a"), input("b")];
        let a = owner("a", 1, &[(70, "needle.txt"), (60, "needle needle")]);
        let b = owner("b", 1, &[(61, "needle")]);
        let external = HashMap::from([("b".into(), b), ("a".into(), a.clone())]);
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let matches = worker
            .search_with_external(&inputs, Provider::Claude, "needle", 1, &current, &external)
            .unwrap();
        assert_eq!(
            matches
                .iter()
                .map(|m| (m.item.as_ref(), m.identity, m.range.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("a", 70, 0..6),
                ("a", 60, 0..6),
                ("a", 60, 7..13),
                ("b", 61, 0..6)
            ]
        );
        assert!(
            Arc::ptr_eq(&worker.documents["a"].document.blocks, &a.blocks),
            "worker adopts handles without cloning whole patch text"
        );
        let result = Results::with_external_revisions(matches, &inputs, 0, &external);
        let paint = Arc::new(Paint {
            generation: 1,
            blocks: result.blocks,
            current: result.matches.first().cloned(),
            sources: result.sources,
            signatures: result.signatures,
        });
        assert!(
            paint.for_item(&inputs[0].item).is_none(),
            "ordinary metadata cannot bypass sidecar proof"
        );
        assert!(paint.for_external_item(&inputs[0].item, &a).is_some());
        assert!(
            paint
                .for_external_item(&inputs[0].item, &owner("a", 2, &[(60, "needle")]))
                .is_none()
        );
    }
    #[test]
    fn find_external_same_metadata_replacement_invalidates_cached_negative_and_positive_results() {
        let inputs = [input("a")];
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let previous = owner("a", 1, &[(60, "no match")]);
        let mut external = HashMap::from([("a".into(), previous.clone())]);
        let negative = worker
            .search_with_external(&inputs, Provider::Claude, "needle", 1, &current, &external)
            .unwrap();
        assert!(negative.is_empty());
        let old = Results::with_external_revisions(negative, &inputs, 0, &external);
        let replacement = owner("a", 2, &[(60, "needle needle")]);
        external.insert("a".into(), replacement.clone());
        assert!(
            !old.sources["a"].matches_external(Some(&replacement)),
            "negative results also require current exact sidecar owner"
        );
        let matches = worker
            .search_with_external(&inputs, Provider::Claude, "needle", 1, &current, &external)
            .unwrap();
        assert_eq!(matches.len(), 2);
        let ranges = worker.index.items["a"].ranges.clone();
        let newer_metadata = [input("a")]; // Same complete source, a replacement metadata Arc.
        worker
            .search_with_external(
                &newer_metadata,
                Provider::Claude,
                "needle",
                1,
                &current,
                &external,
            )
            .unwrap();
        assert!(
            Arc::ptr_eq(&ranges, &worker.index.items["a"].ranges),
            "metadata/layout churn must not reindex unchanged complete text"
        );
        external.insert("a".into(), owner("a", 3, &[(60, "needle")]));
        assert_eq!(
            worker
                .search_with_external(&inputs, Provider::Claude, "needle", 1, &current, &external)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn find_external_cancelled_inflight_owner_retires_actual_body_after_worker_finishes() {
        let inputs = vec![input("a")];
        let body = owner("a", 1, &[(60, "needle")]);
        let text = Arc::downgrade(&body.blocks[0].text);
        let blocks = Arc::downgrade(&body.blocks);
        let external = HashMap::from([("a".into(), body)]);
        let current = Arc::new(AtomicU64::new(1));
        let worker = Arc::new(std::sync::Mutex::new(Worker::default()));
        worker
            .lock()
            .unwrap()
            .search_with_external(&inputs, Provider::Claude, "needle", 1, &current, &external)
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let started = barrier.clone();
        let running_worker = worker.clone();
        let generation = current.clone();
        let thread = std::thread::spawn(move || {
            started.wait();
            assert!(
                running_worker
                    .lock()
                    .unwrap()
                    .search_with_external(
                        &inputs,
                        Provider::Claude,
                        "needle",
                        1,
                        &generation,
                        &external
                    )
                    .is_none()
            );
        });
        current.store(2, Ordering::Relaxed);
        drop(worker); // Runtime has retired its worker while a captured task still owns it.
        assert!(text.upgrade().is_some() && blocks.upgrade().is_some());
        barrier.wait();
        thread.join().unwrap();
        assert!(
            text.upgrade().is_none() && blocks.upgrade().is_none(),
            "cancellation and task completion retire full document owners, not only Weak header identity"
        );
    }

    #[test]
    fn find_external_wrong_root_refuses_a_document_and_proofs_hold_no_body_owner() {
        let inputs = [input("a")];
        let current = AtomicU64::new(1);
        let mut worker = Worker::default();
        let wrong = HashMap::from([("a".into(), owner("other", 1, &[(60, "needle")]))]);
        assert!(
            worker
                .search_with_external(&inputs, Provider::Claude, "needle", 1, &current, &wrong)
                .is_none()
        );
        let body = owner("a", 1, &[(60, "needle")]);
        let weak = Arc::downgrade(&body);
        let mut external = HashMap::from([("a".into(), body)]);
        let result = Results::with_external_revisions(Vec::new(), &inputs, 0, &external);
        external.clear();
        assert!(
            weak.upgrade().is_none(),
            "result proof cannot extend hidden sidecar lifetime"
        );
        assert!(
            result.sources["a"]
                .external
                .as_ref()
                .unwrap()
                .upgrade()
                .is_none()
        );
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::{
        agent::Provider,
        chat::{CommandAction, Item, ItemContent, Role, ToolStatus},
    };
    use std::{cell::Cell, sync::mpsc, thread, time::Duration};

    const UNIT: &str = "é Straße İ ﬃ ";
    const OCCURRENCES: usize = super::super::HIGHLIGHT_LIMIT + 4;

    fn input(id: &str, output: String) -> Input {
        Input {
            item: Arc::new(Item {
                id: id.into(),
                role: Role::Assistant,
                content: ItemContent::Command {
                    command: "true".into(),
                    label: "true".into(),
                    working_directory: String::new(),
                    output,
                    action: CommandAction::Run,
                    status: ToolStatus::Running,
                    exit_code: None,
                    duration_ms: None,
                },
                started_at: None,
                at: None,
            }),
            revision: 7,
            turn: "turn".into(),
            growing: true,
        }
    }
    fn inputs() -> Vec<Input> {
        vec![
            input("hits", UNIT.repeat(OCCURRENCES)),
            input("negative", "no matching output".into()),
        ]
    }
    fn owner(item: &str, identity: u64, text: &str) -> Arc<External> {
        Arc::new(External {
            item: item.into(),
            revision: 8,
            blocks: vec![Block {
                identity,
                text: Arc::from(text),
            }]
            .into(),
        })
    }

    #[test]
    fn warm_cached_occurrences_cancel_within_two_batches_without_losing_valid_ranges() {
        let inputs = inputs();
        let worker = Mutex::new(Worker::default());
        let external = HashMap::from([(
            "negative".into(),
            owner("negative", 90, "authoritative empty result"),
        )]);
        let supplemental = HashMap::from([(
            "negative".into(),
            owner("negative", 91, "original empty result"),
        )]);
        let warm = Worker::search_results(
            &worker,
            &inputs,
            Provider::Claude,
            "STRASSE",
            13,
            &external,
            &supplemental,
            || true,
        )
        .unwrap();
        assert_eq!(
            warm.matches.len(),
            OCCURRENCES,
            "discovery is never truncated to the paint budget"
        );
        let cached = {
            let mut worker = worker.lock().unwrap();
            let cached = worker.index.items["hits"].ranges.clone();
            let documents = [worker.documents["hits"].document.clone()];
            let mut cancel = Cancellation::new(|visited| visited < CANCEL_BATCH * 2);
            assert!(
                worker
                    .index
                    .search_checked(&documents, "STRASSE", &mut cancel)
                    .is_none()
            );
            assert_eq!(
                cancel.visited,
                CANCEL_BATCH * 2,
                "stop in cached expansion, not only after all hits were cloned"
            );
            assert!(Arc::ptr_eq(&cached, &worker.index.items["hits"].ranges));
            cached
        };
        let complete = Worker::search_results(
            &worker,
            &inputs,
            Provider::Claude,
            "STRASSE",
            13,
            &external,
            &supplemental,
            || true,
        )
        .unwrap();
        assert_eq!(complete.matches.as_ref(), warm.matches.as_ref());
        assert!(Arc::ptr_eq(
            &cached,
            &worker.lock().unwrap().index.items["hits"].ranges
        ));
        for (index, found) in complete.matches.iter().enumerate() {
            assert_eq!(found.range, index * UNIT.len() + 3..index * UNIT.len() + 10);
            assert_eq!(complete.position(found), Some(index));
            assert_eq!(complete.blocks[&found.identity][index], found.range);
        }
        assert_eq!(complete.scope, 13);
        assert_eq!(
            complete.sources.len(),
            2,
            "negative documents retain their exact source proofs"
        );
        assert!(complete.sources["negative"].matches_external(external.get("negative")));
        assert!(complete.sources["negative"].matches_supplemental(supplemental.get("negative")));
        assert!(Arc::ptr_eq(
            &complete.sources["hits"].item.upgrade().unwrap(),
            &inputs[0].item
        ));
        assert!(complete.sources["hits"].growing);
        assert!(!complete.revisions.contains_key("negative"));
        // U+0130 is two source bytes and folds to three UTF-8 bytes. The
        // second query still uses original source offsets, not folded offsets.
        let dotted = Worker::search_results(
            &worker,
            &inputs,
            Provider::Claude,
            "i\u{307}",
            13,
            &external,
            &supplemental,
            || true,
        )
        .unwrap();
        assert_eq!(dotted.matches.len(), OCCURRENCES);
        let start = UNIT.find('İ').unwrap();
        for (index, found) in dotted.matches.iter().enumerate() {
            assert_eq!(
                found.range,
                index * UNIT.len() + start..index * UNIT.len() + start + 'İ'.len_utf8()
            );
        }
    }

    #[test]
    fn late_cancelled_partial_output_keeps_its_owners_until_after_worker_unlock() {
        let inputs = inputs();
        let worker = Mutex::new(Worker::default());
        let empty = HashMap::new();
        let warm = Worker::search_results(
            &worker,
            &inputs,
            Provider::Claude,
            "STRASSE",
            1,
            &empty,
            &empty,
            || true,
        )
        .unwrap();
        assert_eq!(warm.matches.len(), OCCURRENCES);
        drop(warm);

        // Use the same locked filling boundary as search_results. Cancel late
        // in warm expansion, with most of the per-hit Arc owners accumulated.
        let cutoff = CANCEL_BATCH * 14;
        let cancelled_under_lock = Cell::new(false);
        let mut cancel = Cancellation::new(|visited| {
            if visited >= cutoff {
                cancelled_under_lock.set(worker.try_lock().is_err());
                false
            } else {
                true
            }
        });
        let mut partial = Vec::new();
        assert!(
            Worker::fill_matches(
                &worker,
                &inputs,
                Provider::Claude,
                "STRASSE",
                &empty,
                &empty,
                &mut partial,
                &mut cancel,
            )
            .is_none()
        );
        assert_eq!(cancel.visited, cutoff);
        assert!(cancelled_under_lock.get());
        assert!(partial.len() >= CANCEL_BATCH * 13 && partial.len() < OCCURRENCES);
        let item = Arc::downgrade(&partial[0].item);
        let turn = Arc::downgrade(&partial[0].turn);
        assert_eq!(item.strong_count(), partial.len());
        assert_eq!(turn.strong_count(), partial.len());
        assert!(
            worker.try_lock().is_ok(),
            "the cache lock must already be free while every partial-output owner is still alive"
        );
        let fresh = Worker::search_results(
            &worker,
            &inputs,
            Provider::Claude,
            "absent",
            2,
            &empty,
            &empty,
            || true,
        )
        .unwrap();
        assert!(fresh.matches.is_empty());
        assert_eq!(fresh.sources.len(), inputs.len());
        assert_eq!(item.strong_count(), partial.len());
        assert_eq!(turn.strong_count(), partial.len());
        drop(partial);
        assert!(item.upgrade().is_none() && turn.upgrade().is_none());
    }

    #[test]
    fn result_and_paint_materialization_cancel_in_each_bounded_map_phase() {
        let inputs = inputs();
        let worker = Mutex::new(Worker::default());
        let empty = HashMap::new();
        let warm = Worker::search_results(
            &worker,
            &inputs,
            Provider::Claude,
            "STRASSE",
            1,
            &empty,
            &empty,
            || true,
        )
        .unwrap();
        let mut complete = Cancellation::new(|_| true);
        let accepted = Results::build_composed(
            warm.matches.as_ref().clone(),
            &inputs,
            1,
            &empty,
            &empty,
            &mut complete,
        )
        .unwrap();
        assert_eq!(accepted.matches.len(), OCCURRENCES);
        // Exercise the actual constructor with cancellation at every batch,
        // including later phases after hashing and grouping have completed.
        for cutoff in (CANCEL_BATCH..=complete.visited).step_by(CANCEL_BATCH) {
            let mut cancel = Cancellation::new(|visited| visited < cutoff);
            assert!(
                Results::build_composed(
                    warm.matches.as_ref().clone(),
                    &inputs,
                    1,
                    &empty,
                    &empty,
                    &mut cancel
                )
                .is_none()
            );
            assert_eq!(cancel.visited, cutoff);
        }
        let mut cancel = Cancellation::new(|visited| visited < CANCEL_BATCH * 2);
        assert!(Paint::build(1, &warm.matches, None, &mut cancel).is_none());
        assert_eq!(cancel.visited, CANCEL_BATCH * 2);
        // A query with no hits must still poll while constructing proofs for
        // many negative documents; the hit loops cannot provide these checks.
        let negative = (0..CANCEL_BATCH * 3)
            .map(|index| input(&format!("negative-{index}"), String::new()))
            .collect::<Vec<_>>();
        let mut cancel = Cancellation::new(|visited| visited < CANCEL_BATCH * 2);
        assert!(
            Results::build_composed(Vec::new(), &negative, 2, &empty, &empty, &mut cancel)
                .is_none()
        );
        assert_eq!(cancel.visited, CANCEL_BATCH * 2);
    }

    #[test]
    fn production_result_builder_releases_worker_before_it_waits_or_is_cancelled() {
        let inputs = Arc::new(inputs());
        let worker = Arc::new(Mutex::new(Worker::default()));
        let current = Arc::new(AtomicU64::new(1));
        let empty = HashMap::new();
        Worker::search_results(
            &worker,
            &inputs,
            Provider::Claude,
            "STRASSE",
            1,
            &empty,
            &empty,
            || true,
        )
        .unwrap();
        let (entered, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let search_worker = worker.clone();
        let probe = worker.clone();
        let search_inputs = inputs.clone();
        let search_generation = current.clone();
        let job = thread::spawn(move || {
            let saw_locked_worker = Cell::new(false);
            let mut paused = false;
            Worker::search_results(
                &search_worker,
                &search_inputs,
                Provider::Claude,
                "STRASSE",
                1,
                &HashMap::new(),
                &HashMap::new(),
                || {
                    let worker_locked = probe.try_lock().is_err();
                    if worker_locked {
                        saw_locked_worker.set(true);
                    } else if saw_locked_worker.get() && !paused {
                        paused = true;
                        let _ = entered.send(());
                        if released.recv_timeout(Duration::from_secs(2)).is_err() {
                            return false;
                        }
                    }
                    search_generation.load(Ordering::Relaxed) == 1
                },
            )
        });
        let entered = waiting.recv_timeout(Duration::from_secs(2)).is_ok();
        let unlocked = entered && worker.try_lock().is_ok();
        current.store(2, Ordering::Relaxed);
        let fresh = unlocked.then(|| {
            Worker::search_results(
                &worker,
                &inputs,
                Provider::Claude,
                "absent",
                2,
                &empty,
                &empty,
                || current.load(Ordering::Relaxed) == 2,
            )
        });
        // Always release/join before assertions, including a failed lock proof.
        let _ = release.send(());
        let obsolete = job.join().unwrap();
        assert!(
            entered && unlocked,
            "a real result-construction checkpoint must run outside the worker mutex"
        );
        assert!(obsolete.is_none(), "superseded results cannot reach the UI");
        let fresh = fresh.unwrap().unwrap();
        assert!(fresh.matches.is_empty());
        assert_eq!(fresh.sources.len(), 2);
        assert_eq!(fresh.scope, 2);
    }
}
