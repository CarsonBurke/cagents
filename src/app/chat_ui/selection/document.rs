//! Logical selection data: no widget tree, styled spans or shaped paragraphs.
//!
//! The existing clipboard formatter consumes this same block/structure model.
//! Mounted chunks only contribute sparse hit geometry; endpoints name a whole
//! rendered block and a UTF-8 byte, independently of chunk boundaries.
use super::*;
use std::hash::{Hash, Hasher};

/// A short CURRENT receiver predicate, evaluated only for a selected source
/// interval at native copy time. Producers capture weak attachment identity
/// and admitted metadata, never Runtime, bodies, widgets or newer descriptors.
/// The callback may perform a short receiver metadata check; it must not
/// hydrate, clone bodies or block on external work.
#[derive(Clone)]
pub(in crate::app::chat_ui) struct CopyGuard(Arc<dyn Fn() -> bool + Send + Sync>);
impl CopyGuard {
    pub fn new(check: impl Fn() -> bool + Send + Sync + 'static) -> Self { Self(Arc::new(check)) }
    pub fn current(&self) -> bool { (self.0)() }
    pub fn same_receipt(&self, other:&Self) -> bool { Arc::ptr_eq(&self.0,&other.0) }
}
/// UI-owned lifetime authority. Scopes contain only Weak atomic headers;
/// dropping/replacing a Runtime cannot be kept alive by a retained Catalog.
/// Revocation is distinct from source epochs and ordinary publication clocks.
pub(in crate::app::chat_ui) struct CopyAuthority(RefCell<Arc<AtomicBool>>);
impl Default for CopyAuthority {
    fn default() -> Self { Self(RefCell::new(Arc::new(AtomicBool::new(true)))) }
}
impl CopyAuthority {
    pub fn scope(&self) -> CopyScope {
        let mut current=self.0.borrow_mut();
        if !current.load(Ordering::Acquire) { *current=Arc::new(AtomicBool::new(true)); }
        CopyScope(Arc::downgrade(&current))
    }
    pub fn revoke(&self) { self.0.borrow().store(false,Ordering::Release); }
}
impl Drop for CopyAuthority {
    fn drop(&mut self) { self.0.get_mut().store(false,Ordering::Release); }
}
#[derive(Clone,Default)]
pub(in crate::app::chat_ui) struct CopyScope(std::sync::Weak<AtomicBool>);
impl CopyScope {
    pub fn same_scope(&self, other:&Self) -> bool { std::sync::Weak::ptr_eq(&self.0,&other.0) }
    pub fn current(&self) -> bool { self.0.upgrade().is_some_and(|current|current.load(Ordering::Acquire)) }
    pub fn guard(self, check:impl Fn()->bool+Send+Sync+'static) -> CopyGuard {
        CopyGuard::new(move||self.current() && check())
    }
}
struct CopyInterval { blocks: Range<usize>, guard: CopyGuard }

/// Exact body-free source provenance. An epoch must never infer a source key
/// from a displayed alias: different keys can legitimately carry equal tokens.
/// Typed consumers supply actual key strings and all 16 UUID bytes followed
/// by the u64 revision in little-endian order. No hash or new wire token.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::app::chat_ui) struct SourceStamp(Arc<SourceProvenance>);
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SourceProvenance {
    agent_id: Option<Arc<str>>,
    source_id: Arc<str>,
    epoch: [u8; 24],
    cache_authority: Option<[u8;24]>,
}
impl SourceStamp {
    pub fn new(agent_id: Option<&str>, source_id: &str, epoch: [u8;24]) -> Self {
        Self(Arc::new(SourceProvenance {
            agent_id: agent_id.map(Arc::from), source_id: Arc::from(source_id), epoch, cache_authority: None,
        }))
    }
    /// An additional bounded cache authority captured with the admitted
    /// bytes. None means no such authority applies (for example a child).
    /// Never fill this from a later Runtime publication to relabel old data.
    pub fn with_cache_authority(mut self, authority: Option<[u8;24]>) -> Self {
        Arc::make_mut(&mut self.0).cache_authority = authority;
        self
    }
    fn exposed_bytes(&self) -> usize {
        std::mem::size_of::<SourceProvenance>() + self.0.source_id.len()
            + self.0.agent_id.as_ref().map_or(0, |agent|agent.len())
    }
}

#[derive(Clone, Default)]
pub(in crate::app::chat_ui) struct Bookmark {
    pub anchor: Option<Endpoint>,
    pub end: Option<Endpoint>,
    pub anchor_prefix: Option<[u8; 32]>,
    pub end_prefix: Option<[u8; 32]>,
    pub anchor_source: Option<SourceStamp>,
    pub end_source: Option<SourceStamp>,
}
impl Bookmark {
    /// A coalesced replacement can become Ready before any loading layout.
    /// Exact provenance must therefore invalidate even identical selected bytes.
    pub(super) fn source_changed(&self, catalog: &Catalog) -> bool {
        [(self.anchor, self.anchor_source.as_ref()), (self.end, self.end_source.as_ref())].into_iter()
            .any(|(point, saved)| point.is_some_and(|point| catalog.declares(point.identity)
                && catalog.source_stamps.get(&point.identity) != saved))
    }
    pub(super) fn record(&mut self, catalog: Option<&Catalog>, anchor: Option<Endpoint>, end: Option<Endpoint>) {
        let fingerprint = |point: Option<Endpoint>, old: Option<Endpoint>, prefix| {
            point.and_then(|point| {
                let catalog = catalog?;
                catalog.fingerprint(point).or_else(|| {
                    if old == Some(point) && catalog.is_pending(point.identity) { prefix } else { None }
                })
            })
        };
        *self = Self {
            anchor, end,
            anchor_prefix: fingerprint(anchor, self.anchor, self.anchor_prefix),
            end_prefix: fingerprint(end, self.end, self.end_prefix),
            anchor_source: anchor.and_then(|point| catalog?.source_stamp(point.identity)),
            end_source: end.and_then(|point| catalog?.source_stamp(point.identity)),
        };
    }
}

/// A previously displayed source interval, with only root provenance and
/// selected endpoint identities. No plaintext/AST, leaf list, or Item owner.
struct SourceInterval {
    item: Arc<str>,
    stamp: SourceStamp,
    blocks: Range<usize>,
}

struct SuspendedItem {
    item: Arc<str>,
    stamp: SourceStamp,
    endpoints: [Option<u64>; 2],
}

/// Body-free intent captured before dropping a complete selection Catalog.
/// The producer must keep this in the same transcript context, clear it on
/// removal/rewrite/Reset, and independently validate current source authority.
/// Never use it to declare a first load or to retag an old admitted prefix.
#[derive(Default)]
pub(in crate::app::chat_ui) struct Suspended {
    items: Vec<SuspendedItem>,
    positions: HashMap<Arc<str>, usize>,
}
impl Suspended {
    /// Declare one unavailable previously displayed root in document order.
    /// The full exact stamp is necessary in addition to the producer's current
    /// admitted-source/receiver check. A mismatch emits no recoverable leaves.
    pub fn declare(&self, item: &str, stamp: SourceStamp, builder: &mut Builder) -> bool {
        let Some(source) = self.positions.get(item).map(|index| &self.items[*index]) else { return false; };
        if source.stamp != stamp { return false; }
        builder.stamped(stamp.clone(), |builder| {
            // The interval itself needs no invented selectable identity.
            // Only actual saved endpoint IDs claim recoverable provenance.
            builder.unavailable();
            for identity in source.endpoints.into_iter().flatten() {
                builder.pending_identity(identity);
            }
        });
        true
    }

    /// Exposed metadata payload, excluding container/allocator overhead and
    /// Arc sharing. This retains no old source text or rendered leaf backing.
    pub fn retained_metadata_bytes(&self) -> usize {
        self.items.capacity() * std::mem::size_of::<SuspendedItem>()
            + self.positions.capacity() * std::mem::size_of::<(Arc<str>, usize)>()
            + self.items.iter().map(|item| item.item.len() + item.stamp.exposed_bytes()).sum::<usize>()
    }
}

pub(in crate::app::chat_ui) struct Catalog {
    read: Read,
    positions: HashMap<u64, usize>,
    plain_bytes: usize,
    prefix_hashes: HashMap<u64, Arc<Vec<[u8; 32]>>>,
    // Same-source rehydration only: no plaintext, widget or paragraph owner.
    pending: HashMap<u64, usize>,
    pending_boundaries: Vec<usize>,
    source_stamps: HashMap<u64, SourceStamp>,
    source_intervals: Vec<SourceInterval>,
    copy_intervals: Vec<CopyInterval>,
}
impl Catalog {
    /// Capture only the source roots intersecting the exact saved selection.
    /// A whole-root interval replaces arbitrarily many hidden leaf descriptors;
    /// only the two actual endpoint identities survive Catalog eviction.
    /// Unknown provenance, holes, overlaps, pending data and changed prefix
    /// proofs fail closed rather than inventing a recoverable source.
    pub fn suspend(&self, bookmark: &Bookmark) -> Option<Suspended> {
        let anchor = bookmark.anchor?;
        let end = bookmark.end?;
        if bookmark.source_changed(self)
            || self.copy_pending(anchor, end)
            || self.fingerprint(anchor)? != bookmark.anchor_prefix?
            || self.fingerprint(end)? != bookmark.end_prefix?
            || self.source_intervals.windows(2).any(|pair| pair[0].blocks.end > pair[1].blocks.start)
        { return None; }
        let (lo, hi) = self.bounds(anchor, end)?;
        let anchor_at = *self.positions.get(&anchor.identity)?;
        let end_at = *self.positions.get(&end.identity)?;
        let mut next = lo.0;
        let mut items = Vec::new();
        let mut positions = HashMap::new();
        for interval in &self.source_intervals {
            if interval.blocks.end <= lo.0 { continue; }
            if interval.blocks.start > hi.0 { break; }
            if interval.blocks.start > next { return None; }
            let endpoints = [
                interval.blocks.contains(&anchor_at).then_some(anchor.identity),
                interval.blocks.contains(&end_at).then_some(end.identity),
            ];
            if endpoints.into_iter().flatten().any(|identity| self.source_stamp(identity) != Some(interval.stamp.clone())) {
                return None;
            }
            if positions.insert(interval.item.clone(), items.len()).is_some() { return None; }
            items.push(SuspendedItem { item: interval.item.clone(), stamp: interval.stamp.clone(), endpoints });
            next = interval.blocks.end;
            if next > hi.0 { break; }
        }
        if next <= hi.0 { return None; }
        Some(Suspended { items, positions })
    }
    fn declares(&self, identity: u64) -> bool { self.positions.contains_key(&identity) || self.is_pending(identity) }
    pub fn source_stamp(&self, identity: u64) -> Option<SourceStamp> { self.source_stamps.get(&identity).cloned() }
    pub fn source_stamp_at(&self, point: Endpoint) -> Option<SourceStamp> {
        self.source_stamp(point.identity)
    }
    pub fn is_pending(&self, identity: u64) -> bool {
        self.pending.contains_key(&identity)
    }
    pub fn has_pending(&self) -> bool { !self.pending.is_empty() }
    /// Any unavailable source interval blocks whole-document copy. It need
    /// not carry recoverable endpoint identity (for example a first load).
    pub fn is_incomplete(&self) -> bool { !self.pending_boundaries.is_empty() }
    pub fn awaiting(&self, anchor: Endpoint, end: Endpoint) -> bool {
        (self.is_pending(anchor.identity) || self.is_pending(end.identity))
            && [anchor, end].into_iter().all(|point| {
                self.is_pending(point.identity) || self.positions.get(&point.identity)
                    .is_some_and(|index| self.read.blocks[*index].plain.is_char_boundary(point.byte))
            })
    }
    /// Clipboard data is incomplete if either endpoint or an intervening
    /// logical leaf is still rehydrating. Never copy an apparently complete
    /// Ready-to-Ready interval with a pending hole in its middle.
    pub fn copy_pending(&self, anchor: Endpoint, end: Endpoint) -> bool {
        if self.is_pending(anchor.identity) || self.is_pending(end.identity) { return true; }
        let Some((lo, hi)) = self.bounds(anchor, end) else { return false; };
        let next = self.pending_boundaries.partition_point(|boundary| *boundary <= lo.0);
        self.pending_boundaries.get(next).is_some_and(|boundary| *boundary <= hi.0)
    }
    pub fn retained_plain_bytes(&self) -> usize {
        self.plain_bytes
    }
    /// Cheap immutable input for background Find indexing. This clones Arc
    /// handles only, and never creates styled spans or font paragraphs.
    pub fn search_blocks(&self) -> Vec<(u64, Arc<str>)> {
        self.read
            .blocks
            .iter()
            .map(|block| (block.identity, block.plain.clone()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.read.blocks.is_empty()
    }
    pub fn fingerprint(&self, point: Endpoint) -> Option<[u8; 32]> {
        if self.is_pending(point.identity) { return None; }
        const STRIDE: usize = 4096;
        let block = &self.read.blocks[*self.positions.get(&point.identity)?];
        block.plain.get(..point.byte)?;
        let chunk = point.byte / STRIDE;
        let prefix = self.prefix_hashes.get(&point.identity)?.get(chunk)?;
        let mut hash = blake3::Hasher::new();
        hash.update(prefix);
        hash.update(&block.plain.as_bytes()[chunk * STRIDE..point.byte]);
        Some(*hash.finalize().as_bytes())
    }
    pub fn builder() -> Builder {
        Builder {
            read: Read::new(None),
            prefixes: HashMap::new(),
            pending: HashMap::new(),
            source_stamps: HashMap::new(),
            active_stamp: None,
            source_intervals: Vec::new(),
            unavailable: Vec::new(),
            copy_intervals: Vec::new(),
        }
    }
    pub fn endpoints(&self) -> Option<(Endpoint, Endpoint)> {
        if self.is_incomplete() { return None; }
        Some((
            Endpoint {
                identity: self.read.blocks.first()?.identity,
                byte: 0,
            },
            Endpoint {
                identity: self.read.blocks.last()?.identity,
                byte: self.read.blocks.last()?.plain.len(),
            },
        ))
    }
    pub fn position(&self, identity: u64) -> Option<usize> {
        self.positions.get(&identity).copied()
    }
    pub fn bounds(
        &self,
        anchor: Endpoint,
        end: Endpoint,
    ) -> Option<((usize, usize), (usize, usize))> {
        let locate = |endpoint: Endpoint| {
            if self.is_pending(endpoint.identity) { return None; }
            let index = *self.positions.get(&endpoint.identity)?;
            self.read.blocks[index]
                .plain
                .is_char_boundary(endpoint.byte)
                .then_some((index, endpoint.byte))
        };
        let a = locate(anchor)?;
        let b = locate(end)?;
        Some(if a <= b { (a, b) } else { (b, a) })
    }
    pub fn range(&self, anchor: Endpoint, end: Endpoint, identity: u64) -> Range<usize> {
        let Some((lo, hi)) = self.bounds(anchor, end) else {
            return 0..0;
        };
        let Some(&index) = self.positions.get(&identity) else {
            return 0..0;
        };
        if index < lo.0 || index > hi.0 {
            return 0..0;
        }
        (if index == lo.0 { lo.1 } else { 0 })..(if index == hi.0 {
            hi.1
        } else {
            self.read.blocks[index].plain.len()
        })
    }
    /// Validate every protected root touching either endpoint or between
    /// them before constructing any clipboard text. Old Catalog stamps alone
    /// cannot observe a receiver revocation delivered before its UI Wake.
    pub fn copy_current(&self, anchor: Endpoint, end: Endpoint) -> bool {
        let Some((lo, hi)) = self.bounds(anchor, end) else { return false; };
        self.copy_intervals.iter().all(|interval| interval.blocks.end <= lo.0
            || interval.blocks.start > hi.0 || interval.guard.current())
    }
    pub fn copy(&self, anchor: Endpoint, end: Endpoint) -> Option<String> {
        if self.copy_pending(anchor, end) || !self.copy_current(anchor, end) { return None; }
        Some(self.read.copy(&self.read.ranges(anchor, end)?))
    }
    pub fn text(&self, identity: u64) -> Option<Arc<str>> {
        if self.is_pending(identity) { return None; }
        Some(
            self.read.blocks[*self.positions.get(&identity)?]
                .plain
                .clone(),
        )
    }
}

pub(in crate::app::chat_ui) fn identity(item: &str, path: &[u32]) -> u64 {
    let mut hash = std::hash::DefaultHasher::new();
    ("native-rendered-block", item, path).hash(&mut hash);
    hash.finish()
}

pub(in crate::app::chat_ui) struct Builder {
    read: Read,
    prefixes: HashMap<u64, Arc<Vec<[u8; 32]>>>,
    pending: HashMap<u64, usize>,
    source_stamps: HashMap<u64, SourceStamp>,
    active_stamp: Option<SourceStamp>,
    source_intervals: Vec<SourceInterval>,
    unavailable: Vec<usize>,
    copy_intervals: Vec<CopyInterval>,
}
impl Builder {
    /// A document-order gap that has not produced current plaintext yet.
    /// This blocks partial whole-document/intervening copy without claiming
    /// that any old endpoint is recoverable. Use pending only when a distinct
    /// exact-source receipt actually proves prior displayed leaf identity.
    pub fn unavailable(&mut self) {
        self.unavailable.push(self.read.blocks.len());
    }
    /// Attach one body-free receipt to an actual rendered source interval.
    /// Nested guards all apply. Reusing text/prefix backing never reuses a
    /// previous Catalog's guard: each producer supplies its admitted receipt.
    pub fn guard_copy(&mut self, guard: CopyGuard, build: impl FnOnce(&mut Self)) {
        let start = self.read.blocks.len();
        build(self);
        let end = self.read.blocks.len();
        if start < end { self.copy_intervals.push(CopyInterval { blocks: start..end, guard }); }
    }
    /// Associate a complete rendered item with its actual admitted epoch.
    /// Call once around the whole item, without nesting source_item scopes.
    /// This is separate from a leaf's stable logical identity and does not
    /// inspect or clone its body. Ready and pending use the same exact stamp.
    pub fn source_item(&mut self, item: &str, stamp: SourceStamp, build: impl FnOnce(&mut Self)) {
        let start = self.read.blocks.len();
        self.stamped(stamp.clone(), build);
        let end = self.read.blocks.len();
        if start < end {
            self.source_intervals.push(SourceInterval { item: Arc::from(item), stamp, blocks: start..end });
        }
    }
    /// Apply the exact same producer provenance to Ready AND pending leaves.
    /// Nesting restores the previous scope. Never derive this from Item IDs,
    /// body length/digests, request counters or presentation generations.
    pub fn stamped(&mut self, stamp: SourceStamp, build: impl FnOnce(&mut Self)) {
        let previous = self.active_stamp.replace(stamp);
        build(self);
        self.active_stamp = previous;
    }
    fn record_source(&mut self, identity: u64) {
        if let Some(stamp) = &self.active_stamp { self.source_stamps.insert(identity, stamp.clone()); }
        else { self.source_stamps.remove(&identity); }
    }

    /// Declare a previously displayed leaf whose exact source/epoch is being
    /// rehydrated. Callers must not use this for first loads, rewrites or reset.
    /// Invoke in document order. Metadata is bounded by the caller's active
    /// source/manifest limits; it retains no old body or synthetic text.
    pub fn pending(&mut self, item: &str, path: &[u32]) {
        self.pending_identity(identity(item, path));
    }
    /// For a source-fenced producer that already holds the exact logical ID
    /// (for example a saved selection endpoint). The same provenance and
    /// document-order requirements as `pending` apply.
    pub fn pending_identity(&mut self, identity: u64) {
        self.record_source(identity);
        self.pending.entry(identity).or_insert(self.read.blocks.len());
    }
    pub fn logical(&mut self, identity: u64, text: Arc<str>) {
        self.record_source(identity);
        let mut logical = LogicalText {
            identity,
            plain: text,
        };
        self.read.custom(None, Rectangle::default(), &mut logical);
    }
    pub fn plain(&mut self, item: &str, path: &[u32], text: Arc<str>) {
        self.logical(identity(item, path), text);
    }
    pub fn indexed(
        &mut self,
        item: &str,
        path: &[u32],
        text: Arc<str>,
        prefixes: Arc<Vec<[u8; 32]>>,
    ) {
        let key = identity(item, path);
        self.logical(key, text);
        self.prefixes.insert(key, prefixes);
    }
    fn enter(&mut self, item: &str, path: &[u32], kind: Kind) {
        self.read.custom(
            None,
            Rectangle::default(),
            &mut boundary::Enter {
                identity: identity(item, path),
                kind,
            },
        );
    }
    fn exit(&mut self) {
        self.read
            .custom(None, Rectangle::default(), &mut boundary::Exit);
    }
    pub fn markdown(&mut self, item: &str, path: &[u32], items: &[markdown::Item]) {
        for (index, node) in items.iter().enumerate() {
            let mut key = path.to_vec();
            key.push(index as u32);
            match node {
                markdown::Item::Heading(_, text) | markdown::Item::Paragraph(text) => {
                    self.indexed(item, &key, text.plain(), text.plain_prefixes())
                }
                markdown::Item::CodeBlock {
                    source,
                    reader_source,
                    ..
                } => self.indexed(item, &key, source.clone(), reader_source.prefixes()),
                markdown::Item::Quote(items) => self.markdown(item, &key, items),
                markdown::Item::List { start, bullets } => {
                    for (index, bullet) in bullets.iter().enumerate() {
                        let mut key = key.clone();
                        key.push(index as u32);
                        let (items, done) = match bullet {
                            markdown::Bullet::Point { items } => (items, None),
                            markdown::Bullet::Task { items, done } => (items, Some(*done)),
                        };
                        let mut marker = start.map_or_else(
                            || "-".to_owned(),
                            |start| format!("{}.", start + index as u64),
                        );
                        if let Some(done) = done {
                            marker.push_str(if done { " [x]" } else { " [ ]" });
                        }
                        self.enter(item, &key, Kind::ListItem(marker));
                        self.markdown(item, &key, items);
                        self.exit();
                    }
                }
                markdown::Item::Table { columns, rows } => {
                    self.enter(
                        item,
                        &key,
                        Kind::Table {
                            columns: columns.len(),
                            rows: rows.len() + 1,
                        },
                    );
                    for (column, header) in columns.iter().enumerate() {
                        let mut cell = key.clone();
                        cell.extend([0, column as u32]);
                        self.enter(item, &cell, Kind::Cell { row: 0, column });
                        self.markdown(item, &cell, &header.header);
                        self.exit();
                    }
                    for (row, cells) in rows.iter().enumerate() {
                        for (column, nodes) in cells.cells.iter().enumerate() {
                            let mut cell = key.clone();
                            cell.extend([row as u32 + 1, column as u32]);
                            self.enter(
                                item,
                                &cell,
                                Kind::Cell {
                                    row: row + 1,
                                    column,
                                },
                            );
                            self.markdown(item, &cell, nodes);
                            self.exit();
                        }
                    }
                    self.exit();
                }
                markdown::Item::Image { title, alt, .. } => {
                    let mut part = key.clone();
                    part.push(0);
                    if !alt.plain().is_empty() {
                        self.indexed(item, &part, alt.plain(), alt.plain_prefixes());
                    }
                    if !title.is_empty() {
                        part.pop();
                        part.push(1);
                        self.plain(item, &part, Arc::from(title.as_str()));
                    }
                }
                markdown::Item::Rule => {}
            }
        }
    }
    pub fn finish(self) -> Arc<Catalog> {
        self.finish_reusing(None)
    }
    pub fn finish_reusing(self, previous: Option<&Catalog>) -> Arc<Catalog> {
        let positions = self
            .read
            .blocks
            .iter()
            .enumerate()
            .map(|(index, block)| (block.identity, index))
            .collect();
        let plain_bytes = self.read.blocks.iter().map(|block| block.plain.len()).sum();
        let prefix_hashes = self
            .read
            .blocks
            .iter()
            .map(|block| {
                if let Some(prefixes) = self.prefixes.get(&block.identity) {
                    return (block.identity, prefixes.clone());
                }
                if let Some(previous) = previous
                    && previous
                        .text(block.identity)
                        .is_some_and(|plain| Arc::ptr_eq(&plain, &block.plain))
                    && let Some(prefixes) = previous.prefix_hashes.get(&block.identity)
                {
                    return (block.identity, prefixes.clone());
                }
                let mut hash = blake3::Hasher::new();
                let mut prefixes = vec![*hash.finalize().as_bytes()];
                for chunk in block.plain.as_bytes().chunks_exact(4096) {
                    hash.update(chunk);
                    prefixes.push(*hash.finalize().as_bytes());
                }
                (block.identity, Arc::new(prefixes))
            })
            .collect();
        let mut pending_boundaries = self.unavailable;
        pending_boundaries.extend(self.pending.values().copied());
        pending_boundaries.sort_unstable();
        pending_boundaries.dedup();
        Arc::new(Catalog {
            pending: self.pending,
            pending_boundaries,
            source_stamps: self.source_stamps,
            source_intervals: self.source_intervals,
            copy_intervals: self.copy_intervals,
            read: self.read,
            positions,
            plain_bytes,
            prefix_hashes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_scopes_are_weak_and_never_rearm_retired_catalog_receipts() {
        fn send_sync<T:Send+Sync>() {} send_sync::<CopyScope>();
        let owner=CopyAuthority::default();let old=owner.scope().guard(||true);assert!(old.current());
        owner.revoke();assert!(!old.current());let fresh=owner.scope().guard(||true);assert!(fresh.current());
        assert!(!old.current());drop(owner);assert!(!fresh.current());assert!(!CopyScope::default().guard(||true).current());
    }

    #[test]
    fn copy_receipts_validate_selected_middle_roots_and_do_not_reuse_stale_guards() {
        fn send_sync<T: Send + Sync>() {} send_sync::<CopyGuard>(); send_sync::<Catalog>();
        let current=Arc::new(std::sync::atomic::AtomicBool::new(true));
        let weak=Arc::downgrade(&current);
        let make=|guard:CopyGuard,old:Option<&Catalog>| {
            let mut builder=Catalog::builder();
            builder.plain("before",&[0],Arc::from("before"));
            builder.guard_copy(guard,|builder|builder.plain("middle",&[0],Arc::from("middle")));
            builder.plain("after",&[0],Arc::from("after"));
            builder.finish_reusing(old)
        };
        let captured=current.clone();let catalog=make(CopyGuard::new(move||captured.load(Ordering::Acquire)),None);
        let (start,end)=catalog.endpoints().unwrap();
        assert_eq!(catalog.copy(start,end).as_deref(),Some("before\n\nmiddle\n\nafter"));
        assert_eq!(catalog.copy(end,start),catalog.copy(start,end));
        current.store(false,Ordering::Release); // receiver changes, Catalog and endpoints do not
        assert!(catalog.copy(start,end).is_none());assert!(catalog.copy(end,start).is_none());
        let before=Endpoint {identity:identity("before",&[0]),byte:6};
        assert_eq!(catalog.copy(start,before).as_deref(),Some("before"),"unrelated root receipt must not gate this subrange");
        let fresh=make(CopyGuard::new(||true),Some(&catalog));
        assert!(fresh.copy(start,end).is_some(),"prefix reuse cannot inherit an old guard");
        assert!(catalog.copy(start,end).is_none(),"new Catalog cannot reauthorize an old captured source");
        drop(current);assert!(weak.upgrade().is_some());drop(catalog);assert!(weak.upgrade().is_none());
        let mut nested=Catalog::builder();
        nested.guard_copy(CopyGuard::new(||false),|builder|builder.guard_copy(CopyGuard::new(||true),|builder|
            builder.plain("source",&[0],Arc::from("text"))));
        let nested=nested.finish();let (a,b)=nested.endpoints().unwrap();assert!(nested.copy(a,b).is_none());
    }

    #[test]
    fn source_stamp_clones_share_only_exact_body_free_key_and_epoch() {
        let first=SourceStamp::new(Some("agent"),"opaque-source",[5;24]);
        let clone=first.clone();
        assert!(Arc::ptr_eq(&first.0,&clone.0),"leaf/bookmark clones must share source metadata");
        assert_eq!(first,SourceStamp::new(Some("agent"),"opaque-source",[5;24]));
        assert_ne!(first,SourceStamp::new(None,"opaque-source",[5;24]));
        assert_ne!(first,SourceStamp::new(Some("agent"),"different-source",[5;24]));
        assert_ne!(first,SourceStamp::new(Some("agent"),"opaque-source",[6;24]));
        let changed=first.clone().with_cache_authority(Some([9;24]));
        assert_ne!(first,changed);assert!(first.0.cache_authority.is_none());
        assert_eq!(changed.0.cache_authority,Some([9;24]));
        assert!(Arc::ptr_eq(&first.0.source_id,&changed.0.source_id),"authority COW shares exact key metadata");
        assert_ne!(changed,first.clone().with_cache_authority(Some([10;24])));
        drop(changed);
        let weak=Arc::downgrade(&first.0);drop(first);assert!(weak.upgrade().is_some());
        drop(clone);assert!(weak.upgrade().is_none());
    }

    #[test]
    fn first_load_gaps_block_partial_copy_without_preserving_old_endpoints() {
        let mut full = Catalog::builder();
        full.plain("before", &[0], Arc::from("before"));
        full.plain("after", &[0], Arc::from("after"));
        let full = full.finish();
        let (anchor,end) = full.endpoints().unwrap();
        let mut builder = Catalog::builder();
        builder.plain("before", &[0], Arc::from("before"));
        builder.unavailable();
        builder.plain("after", &[0], Arc::from("after"));
        let loading = builder.finish();
        assert!(loading.is_incomplete());
        assert!(!loading.has_pending(),"first loads cannot claim recoverable old identities");
        assert!(!loading.awaiting(anchor,end));
        assert!(loading.endpoints().is_none(),"Select All must not present a partial document as complete");
        assert!(loading.copy(anchor,end).is_none(),"copy cannot concatenate around a first-load hole");
        assert_eq!(loading.copy(anchor,anchor).as_deref(),Some(""),"complete subranges remain valid");
        let mut empty = Catalog::builder();empty.unavailable();let empty=empty.finish();
        assert!(empty.is_empty() && empty.is_incomplete() && !empty.has_pending());
        assert!(!empty.awaiting(anchor,end),"an anonymous gap cannot resurrect missing endpoints");
    }

    #[test]
    fn suspension_captures_only_roots_and_endpoints_and_blocks_middle_copy_holes() {
        let stamp = SourceStamp::new(None,"source",[7;24]);
        let body: Arc<str> = Arc::from("old rendered leaf");
        let weak = Arc::downgrade(&body);
        let mut builder = Catalog::builder();
        builder.source_item("before", stamp.clone(), |builder| builder.plain("before", &[0], Arc::from("before")));
        builder.source_item("middle", stamp.clone(), |builder| {
            for index in 0..10_000 { builder.plain("middle", &[0,index], body.clone()); }
        });
        builder.source_item("after", stamp.clone(), |builder| builder.plain("after", &[0], Arc::from("after")));
        let full = builder.finish();
        let (anchor,end) = full.endpoints().unwrap();
        let mut bookmark = Bookmark::default();
        bookmark.record(Some(&full), Some(anchor), Some(end));
        let suspended = full.suspend(&bookmark).unwrap();
        assert_eq!(suspended.items.len(),3);
        assert_eq!(suspended.items.iter().flat_map(|item| item.endpoints).flatten().count(),2,
            "10k hidden leaves retain only actual selected endpoint identities");
        assert!(suspended.retained_metadata_bytes() < 1024);
        drop(full);drop(body);
        assert!(weak.upgrade().is_none(),"suspended sources must not own plaintext");
        let mut builder = Catalog::builder();
        builder.source_item("before", stamp.clone(), |builder| builder.plain("before", &[0], Arc::from("before")));
        assert!(suspended.declare("middle",stamp.clone(),&mut builder));
        builder.source_item("after", stamp.clone(), |builder| builder.plain("after", &[0], Arc::from("after")));
        let pending = builder.finish();
        assert!(!bookmark.source_changed(&pending));
        assert!(pending.copy_pending(anchor,end));
        assert!(pending.copy(anchor,end).is_none(),"Ready endpoints cannot copy across an unavailable source root");
        assert_eq!(pending.retained_plain_bytes(),"beforeafter".len());
        let mut builder = Catalog::builder();
        assert!(!suspended.declare("middle",SourceStamp::new(None,"source",[8;24]),&mut builder));
        assert!(!suspended.declare("never-displayed",stamp.clone(),&mut builder));
        assert!(!builder.finish().has_pending(),"replacement and first-load sources cannot claim old intent");
    }

    #[test]
    fn suspension_refuses_unknown_or_changed_endpoint_provenance_and_uncovered_intervals() {
        let stamp=SourceStamp::new(None,"source",[1;24]);
        let mut builder=Catalog::builder();
        builder.source_item("before",stamp.clone(),|builder|builder.plain("before",&[0],Arc::from("before")));
        builder.plain("unknown",&[0],Arc::from("unproved middle"));
        builder.source_item("after",stamp.clone(),|builder|builder.plain("after",&[0],Arc::from("after")));
        let full=builder.finish();let(anchor,end)=full.endpoints().unwrap();
        let mut bookmark=Bookmark::default();bookmark.record(Some(&full),Some(anchor),Some(end));
        assert!(full.suspend(&bookmark).is_none(),"every intervening root needs actual provenance");
        let mut changed=bookmark.clone();changed.anchor_source=Some(SourceStamp::new(None,"source",[2;24]));
        assert!(full.suspend(&changed).is_none());
        let mut changed=bookmark.clone();changed.end_prefix=Some([3;32]);
        assert!(full.suspend(&changed).is_none());
    }

    #[test]
    fn worker_prepared_prefix_checkpoints_are_adopted_without_body_hashing() {
        let plain: Arc<str> = "prepared λ paragraph ".repeat(100_000).into();
        let source = super::super::super::reader_source::Source::new(&plain);
        let prefixes = source.prefixes();
        let mut builder = Catalog::builder();
        builder.indexed("source", &[0], plain.clone(), prefixes.clone());
        let catalog = builder.finish();
        let identity = identity("source", &[0]);
        assert!(Arc::ptr_eq(&catalog.prefix_hashes[&identity], &prefixes));
        let mut reference = Catalog::builder();
        reference.plain("source", &[0], plain.clone());
        let reference = reference.finish();
        for byte in [0, 4095, 8192, plain.len()] {
            if plain.is_char_boundary(byte) {
                let point = Endpoint { identity, byte };
                assert_eq!(catalog.fingerprint(point), reference.fingerprint(point));
            }
        }
    }
    #[test]
    fn unchanged_logical_blocks_reuse_validation_hashes_but_rewrites_do_not() {
        let first: Arc<str> = "historical source ".repeat(100000).into();
        let mut builder = Catalog::builder();
        builder.plain("history", &[0], first.clone());
        let original = builder.finish();
        let mut builder = Catalog::builder();
        builder.plain("history", &[0], first);
        builder.plain("tail", &[0], Arc::from("new streamed tail"));
        let next = builder.finish_reusing(Some(&original));
        let identity = identity("history", &[0]);
        assert!(Arc::ptr_eq(
            &original.prefix_hashes[&identity],
            &next.prefix_hashes[&identity]
        ));
        let mut builder = Catalog::builder();
        builder.plain("history", &[0], Arc::from("rewritten source"));
        let rewritten = builder.finish_reusing(Some(&next));
        assert!(!Arc::ptr_eq(
            &next.prefix_hashes[&identity],
            &rewritten.prefix_hashes[&identity]
        ));
    }
    #[test]
    fn pending_leaves_keep_no_text_and_block_only_incomplete_copy_intervals() {
        let mut full = Catalog::builder();
        full.plain("before", &[2], Arc::from("before"));
        let body: Arc<str> = String::from("selected source").into();
        let weak = Arc::downgrade(&body);
        full.plain("selected", &[10, 0], body);
        full.plain("after", &[2], Arc::from("after"));
        let full = full.finish();
        let point = Endpoint { identity: identity("selected", &[10, 0]), byte: 3 };
        let mut bookmark = Bookmark::default();
        bookmark.record(Some(&full), Some(point), Some(point));
        let prefix = bookmark.anchor_prefix;
        assert!(prefix.is_some());
        drop(full);
        let mut builder = Catalog::builder();
        builder.plain("before", &[2], Arc::from("before"));
        builder.pending("selected", &[10, 0]);
        builder.plain("after", &[2], Arc::from("after"));
        let pending = builder.finish();
        assert!(weak.upgrade().is_none(), "pending metadata retains no old body");
        assert!(pending.is_pending(point.identity));
        assert_eq!(pending.search_blocks().len(), 2, "no synthetic selectable text");
        assert_eq!(pending.retained_plain_bytes(), "beforeafter".len());
        assert!(pending.endpoints().is_none(), "Select All cannot pretend the document is complete");
        let before = Endpoint { identity: identity("before", &[2]), byte: 0 };
        let before_end = Endpoint { byte: 6, ..before };
        let after = Endpoint { identity: identity("after", &[2]), byte: 5 };
        assert_eq!(pending.copy(before, before_end).as_deref(), Some("before"));
        assert!(pending.copy(before, after).is_none(), "no partial clipboard across a middle hole");
        assert!(pending.copy(after, before).is_none());
        assert!(pending.awaiting(point, before));
        assert!(!pending.awaiting(point, Endpoint { identity: u64::MAX, byte: 0 }),
            "one pending endpoint cannot hide the removal of the other");
        for _ in 0..3 { bookmark.record(Some(&pending), Some(point), Some(point)); }
        assert_eq!(bookmark.anchor_prefix, prefix);
        assert_eq!(bookmark.end_prefix, prefix);
        let mut restored = Catalog::builder();
        restored.plain("selected", &[10, 0], Arc::from("selected source"));
        let restored = restored.finish();
        assert_eq!(restored.fingerprint(point), prefix);
        let mut rewritten = Catalog::builder();
        rewritten.plain("selected", &[10, 0], Arc::from("REWRITTEN source"));
        assert_ne!(rewritten.finish().fingerprint(point), prefix);
    }

    #[test]
    fn exact_source_stamp_survives_pending_but_rejects_coalesced_identical_replacement() {
        let make_stamp=|agent,source,epoch|SourceStamp::new(agent,source,epoch).with_cache_authority(Some([3;24]));
        let epoch=[0u8;24];let mut revision_epoch=epoch;revision_epoch[16]=1;let mut incarnation_epoch=epoch;incarnation_epoch[0]=1;
        let a=make_stamp(None,"source",epoch);let revision=make_stamp(None,"source",revision_epoch);let incarnation=make_stamp(None,"source",incarnation_epoch);
        let id=identity("source",&[10,0]);
        let build=|stamp:Option<SourceStamp>,pending| {
            let mut builder=Catalog::builder();
            let leaf=|builder:&mut Builder|if pending {builder.pending("source",&[10,0]);}
                else {builder.plain("source",&[10,0],Arc::from("identical bytes"));};
            if let Some(stamp)=stamp {builder.stamped(stamp,leaf);}else{leaf(&mut builder);}
            builder.finish()
        };
        let first=build(Some(a.clone()),false);let (anchor,end)=first.endpoints().unwrap();
        let mut saved=Bookmark::default();saved.record(Some(&first),Some(anchor),Some(end));
        assert_eq!(saved.anchor_source,Some(a.clone()));let prefix=saved.end_prefix;
        let pending=build(Some(a.clone()),true);assert!(!saved.source_changed(&pending));
        saved.record(Some(&pending),Some(anchor),Some(end));
        assert_eq!(saved.end_prefix,prefix);assert_eq!(saved.end_source,Some(a.clone()));
        assert!(!saved.source_changed(&build(Some(a.clone()),false)));
        for stamp in [Some(a.clone().with_cache_authority(Some([4;24]))),Some(revision),Some(incarnation.clone()),Some(make_stamp(None,"other-source",epoch)),Some(make_stamp(Some("different-agent"),"source",epoch)),None] {
            let replaced=build(stamp.clone(),false);
            assert_eq!(replaced.search_blocks()[0].0,id,"Find identity remains stable");
            assert_eq!(replaced.fingerprint(end),prefix,"identical prefix cannot prove source epoch");
            assert!(saved.source_changed(&replaced),"key, revision, incarnation and lost proof each revoke selection");
            assert!(saved.source_changed(&build(stamp,true)),"a pending declaration cannot hide changed provenance");
        }
        let mut builder=Catalog::builder();
        builder.stamped(a.clone(),|builder|builder.plain("source",&[10,0],Arc::from("identical bytes")));
        builder.stamped(incarnation.clone(),|builder|builder.plain("other",&[10,0],Arc::from("other source")));
        assert!(!saved.source_changed(&builder.finish()),"unrelated source changes do not clear selection");
        let body:Arc<str>=Arc::from("released body");let weak=Arc::downgrade(&body);
        let mut builder=Catalog::builder();builder.stamped(a.clone(),|builder|builder.plain("source",&[10,0],body));
        drop(builder.finish());
        let pending=build(Some(a.clone()),true);assert!(weak.upgrade().is_none());
        assert_eq!(pending.retained_plain_bytes(),0);assert_eq!(pending.source_stamp(id),Some(a.clone()));
    }

}
