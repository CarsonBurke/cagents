//! Logical selection data: no widget tree, styled spans or shaped paragraphs.
//!
//! The existing clipboard formatter consumes this same block/structure model.
//! Mounted chunks only contribute sparse hit geometry; endpoints name a whole
//! rendered block and a UTF-8 byte, independently of chunk boundaries.
use super::*;
use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, Default)]
pub(in crate::app::chat_ui) struct Bookmark {
    pub anchor: Option<Endpoint>,
    pub end: Option<Endpoint>,
    pub anchor_prefix: Option<[u8; 32]>,
    pub end_prefix: Option<[u8; 32]>,
}

pub(in crate::app::chat_ui) struct Catalog {
    read: Read,
    positions: HashMap<u64, usize>,
    plain_bytes: usize,
    prefix_hashes: HashMap<u64, Arc<[[u8; 32]]>>,
}
impl Catalog {
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
        }
    }
    pub fn endpoints(&self) -> Option<(Endpoint, Endpoint)> {
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
    pub fn copy(&self, anchor: Endpoint, end: Endpoint) -> Option<String> {
        Some(self.read.copy(&self.read.ranges(anchor, end)?))
    }
    pub fn text(&self, identity: u64) -> Option<Arc<str>> {
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
}
impl Builder {
    pub fn logical(&mut self, identity: u64, text: Arc<str>) {
        let mut logical = LogicalText {
            identity,
            plain: text,
        };
        self.read.custom(None, Rectangle::default(), &mut logical);
    }
    pub fn plain(&mut self, item: &str, path: &[u32], text: Arc<str>) {
        self.logical(identity(item, path), text);
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
                    self.plain(item, &key, text.plain())
                }
                markdown::Item::CodeBlock { code, .. } => {
                    self.plain(item, &key, Arc::from(code.as_str()))
                }
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
                        self.plain(item, &part, alt.plain());
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
                (block.identity, Arc::from(prefixes))
            })
            .collect();
        Arc::new(Catalog {
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
}
