//! Immutable rendered-text inputs and cancellable per-item match caching.
//!
//! The worker owns the cache. UI messages carry cheap Arc clones, never a
//! widget tree, renderer, or shaped paragraph.
use super::matched_ranges;
use std::{collections::{HashMap, HashSet}, ops::Range, sync::{Arc, atomic::{AtomicU64, Ordering}}};

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
    pub turn: String,
    pub item: String,
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
    pub fn search(&mut self, documents: &[Document], query: &str,
        generation: u64, current: &AtomicU64) -> Option<Vec<Match>> {
        if current.load(Ordering::Relaxed) != generation { return None; }
        if self.query != query {
            self.query = query.to_owned();
            self.items.clear();
        }
        if query.is_empty() { self.items.clear(); return Some(Vec::new()); }
        let present: HashSet<_> = documents.iter().map(|document| document.item.as_str()).collect();
        self.items.retain(|item, _| present.contains(item.as_str()));
        let mut result = Vec::new();
        for document in documents {
            if current.load(Ordering::Relaxed) != generation { return None; }
            if self.items.get(&document.item).is_none_or(|cached| cached.revision != document.revision) {
                let mut ranges = Vec::new();
                for block in document.blocks.iter() {
                    let found = super::matched_ranges_cancellable(&block.text, query, usize::MAX,
                        || current.load(Ordering::Relaxed) == generation)?;
                    ranges.extend(found.ranges.into_iter().map(|range| (block.identity, range)));
                }
                self.items.insert(document.item.clone(), Cached { revision: document.revision, ranges: ranges.into() });
            }
            for (identity, range) in self.items[&document.item].ranges.iter() {
                result.push(Match { turn: document.turn.clone(), item: document.item.clone(),
                    identity: *identity, range: range.clone() });
            }
        }
        (current.load(Ordering::Relaxed) == generation).then_some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document(item: &str, turn: &str, revision: u64, blocks: &[(u64, &str)]) -> Document {
        Document { item: item.into(), turn: turn.into(), revision,
            blocks: blocks.iter().map(|(identity, text)| Block { identity: *identity, text: Arc::from(*text) }).collect() }
    }
    #[test]
    fn each_occurrence_keeps_render_order_and_utf8_offsets() {
        let mut index = Index::default();
        let generation = AtomicU64::new(4);
        let documents = [document("a", "first", 1, &[(11, "é cat cat"), (12, "CAT")]),
            document("b", "second", 1, &[(21, "cat")])];
        let matches = index.search(&documents, "cat", 4, &generation).unwrap();
        assert_eq!(matches.iter().map(|found| (found.item.as_str(), found.identity, found.range.clone())).collect::<Vec<_>>(),
            vec![("a",11,3..6),("a",11,7..10),("a",12,0..3),("b",21,0..3)]);
    }
    #[test]
    fn changed_item_invalidates_only_its_cached_ranges_and_removed_items_leave() {
        let mut index = Index::default(); let generation = AtomicU64::new(1);
        let mut documents = vec![document("a","t",1,&[(1,"cat")]),document("b","t",1,&[(2,"cat cat")])];
        index.search(&documents,"cat",1,&generation).unwrap();
        let retained = index.items["a"].ranges.clone();
        documents[1] = document("b","t",2,&[(2,"dog")]);
        assert_eq!(index.search(&documents,"cat",1,&generation).unwrap().len(),1);
        assert!(Arc::ptr_eq(&retained,&index.items["a"].ranges));
        documents.remove(0); index.search(&documents,"cat",1,&generation).unwrap();
        assert!(!index.items.contains_key("a"));
    }
    #[test]
    fn superseded_query_never_publishes() {
        let mut index = Index::default(); let generation = AtomicU64::new(2);
        assert!(index.search(&[document("a","t",1,&[(1,"cat")])],"cat",1,&generation).is_none());
    }
    #[test]
    fn match_count_is_not_the_paint_budget() {
        let text = "x ".repeat(5000); let generation = AtomicU64::new(1);
        let mut index = Index::default();
        assert_eq!(index.search(&[document("a","t",1,&[(1,&text)])],"x",1,&generation).unwrap().len(),5000);
        assert_eq!(matched_ranges(&text,"x",4096).ranges.len(),4096);
    }
}
