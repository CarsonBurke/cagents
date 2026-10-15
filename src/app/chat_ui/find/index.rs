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
            let turn:Arc<str>=Arc::from(document.turn.as_str());
            let item:Arc<str>=Arc::from(document.item.as_str());
            for (identity, range) in self.items[&document.item].ranges.iter() {
                result.push(Match { turn: turn.clone(), item: item.clone(),
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
        assert_eq!(matches.iter().map(|found| (found.item.as_ref(), found.identity, found.range.clone())).collect::<Vec<_>>(),
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

/// One accepted search result, shared by mounted fragments. Ranges refer to
/// whole logical blocks; a fragment intersects them without rescanning text.
#[derive(Clone, Default, Debug)]
pub(in crate::app::chat_ui) struct Paint {
    pub generation: u64,
    pub blocks: Arc<HashMap<u64, Arc<[Range<usize>]>>>,
    pub current: Option<Match>,
}
impl Paint {
    pub fn new(generation: u64, matches: &[Match], current: Option<Match>) -> Self {
        let mut blocks: HashMap<u64, Vec<Range<usize>>> = HashMap::new();
        for found in matches { blocks.entry(found.identity).or_default().push(found.range.clone()); }
        Self { generation, blocks: Arc::new(blocks.into_iter().map(|(key,ranges)| (key,ranges.into())).collect()), current }
    }
    pub fn fragment(&self, identity: u64, start: usize, length: usize) -> (Vec<Range<usize>>, Option<Range<usize>>) {
        let end = start.saturating_add(length);
        let clip = |range: &Range<usize>| (range.start < end && range.end > start)
            .then(|| range.start.max(start)-start..range.end.min(end)-start);
        let ranges = self.blocks.get(&identity).map(|ranges| {
            let first = ranges.partition_point(|range| range.end <= start);
            ranges[first..].iter().take_while(|range| range.start < end).filter_map(clip).collect()
        }).unwrap_or_default();
        let current = self.current.as_ref().filter(|found| found.identity == identity).and_then(|found| clip(&found.range));
        (ranges,current)
    }
}

#[derive(Clone)]
pub(in crate::app::chat_ui) struct Results {
    pub matches: Arc<Vec<Match>>,
    pub blocks: Arc<HashMap<u64, Arc<[Range<usize>]>>>,
    pub revisions: HashMap<String,u64>,
    pub turns: Arc<HashSet<Arc<str>>>,
    pub items: Arc<HashSet<Arc<str>>>,
    pub scope: u64,
    positions: HashMap<(Arc<str>,u64,usize,usize),usize>,
}
impl Results {
    pub fn new(matches: Vec<Match>) -> Self {
        let blocks = Paint::new(0,&matches,None).blocks;
        let positions = matches.iter().enumerate().map(|(index,found)|
            ((found.item.clone(),found.identity,found.range.start,found.range.end),index)).collect();
        let turns=matches.iter().map(|found|found.turn.clone()).collect();
        let items=matches.iter().map(|found|found.item.clone()).collect();
        Self { matches:Arc::new(matches),blocks,positions,revisions:HashMap::new(),turns:Arc::new(turns),items:Arc::new(items),scope:0 }
    }
    pub fn with_revisions(matches:Vec<Match>,inputs:&[Input],scope:u64) -> Self {
        let mut result=Self::new(matches);
        let matched:HashSet<_>=result.matches.iter().map(|found|found.item.as_ref()).collect();
        result.revisions=inputs.iter().filter(|input|matched.contains(input.item.id.as_str()))
            .map(|input|(input.item.id.clone(),input.revision)).collect();
        result.scope=scope;
        result
    }
    pub fn position(&self, found: &Match) -> Option<usize> {
        self.positions.get(&(found.item.clone(),found.identity,found.range.start,found.range.end)).copied()
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
    documents: HashMap<String, Document>,
}
impl Worker {
    pub fn search(&mut self, inputs: &[Input], provider: crate::agent::Provider,
        query: &str, generation: u64, current: &AtomicU64) -> Option<Vec<Match>> {
        let present: HashSet<_> = inputs.iter().map(|input| input.item.id.as_str()).collect();
        self.documents.retain(|id,_| present.contains(id.as_str()));
        let mut documents = Vec::with_capacity(inputs.len());
        for input in inputs {
            if current.load(Ordering::Relaxed) != generation { return None; }
            if self.documents.get(&input.item.id).is_none_or(|document| document.revision != input.revision) {
                self.documents.insert(input.item.id.clone(), document(input,provider));
            }
            let mut document = self.documents[&input.item.id].clone();
            document.turn.clone_from(&input.turn);
            documents.push(document);
        }
        self.index.search(&documents,query,generation,current)
    }
}
fn document(input: &Input, provider: crate::agent::Provider) -> Document {
    use crate::app::chat_ui::{coordination, markdown, markdown_source, selection};
    use crate::chat::{ItemContent, Role, DeliveryKind};
    let item = &input.item;
    let coordinated = crate::chat::coordination::recognize(item,provider).map(coordination::Coordinated::new);
    let mut builder = selection::Catalog::builder();
    if let Some(coordinated) = &coordinated {
        for (index,content) in coordinated.contents().enumerate() {
            builder.markdown(&item.id,&[1,index as u32],content.items());
        }
    }
    let mut parse = |body: &str| {
        let source = match markdown_source::Source::default().advance(body,input.growing) {
            markdown_source::Step::Append(text) | markdown_source::Step::Restart(text) => text,
        };
        let parsed = markdown::Content::parse(&source);
        builder.markdown(&item.id,&[0],parsed.items());
    };
    match &item.content {
        ItemContent::Text {text,..} if item.role == Role::User => builder.plain(&item.id,&[2],Arc::from(text.as_str())),
        ItemContent::Text {text,..} => parse(text),
        ItemContent::Delivery {kind,body,..} if *kind != DeliveryKind::Notice => parse(body),
        ItemContent::Delivery {body,..} => builder.plain(&item.id,&[2],Arc::from(body.as_str())),
        ItemContent::Command {command,label,working_directory,output,..} => {
            if coordinated.is_none() && !label.is_empty() && label != command {
                builder.plain(&item.id,&[3,0],Arc::from(crate::app::chat_ui::turn::step_label(&item.content)));
            }
            if !working_directory.is_empty() {builder.plain(&item.id,&[3,1],Arc::from(working_directory.as_str()));}
            builder.plain(&item.id,&[4],Arc::from(command.as_str()));
            builder.plain(&item.id,&[5],Arc::from(output.as_str()));
        }
        ItemContent::Tool {name,input,output,..} => {
            if coordinated.is_none() {builder.plain(&item.id,&[3,0],Arc::from(name.as_str()));}
            builder.plain(&item.id,&[4],Arc::from(input.as_str()));
            builder.plain(&item.id,&[5],Arc::from(output.as_str()));
        }
        ItemContent::Changes {changes,..} => for (index,change) in changes.iter().enumerate() {
            builder.plain(&item.id,&[7,index as u32,0],Arc::from(change.path.as_str()));
            if let crate::chat::ChangeKind::Renamed {from}=&change.kind {builder.plain(&item.id,&[7,index as u32,1],Arc::from(from.as_str()));}
            builder.plain(&item.id,&[6,index as u32],Arc::from(change.diff.as_str()));
        },
        ItemContent::WebSearch {query,results} => {
            builder.plain(&item.id,&[8,0],Arc::from(query.as_str()));
            for (index,result) in results.iter().enumerate() {
                if !result.title.is_empty() {builder.plain(&item.id,&[8,1,index as u32,0],Arc::from(result.title.as_str()));}
                if !result.url.is_empty() {builder.plain(&item.id,&[8,1,index as u32,1],Arc::from(result.url.as_str()));}
                if !result.snippet.is_empty() {builder.plain(&item.id,&[8,1,index as u32,2],Arc::from(result.snippet.as_str()));}
            }
        }
        ItemContent::Image {path,description} if item.role == Role::User => {
            builder.plain(&item.id,&[2],Arc::from(crate::app::chat_ui::Runtime::attachment_label(path,description)));
        }
        ItemContent::Image {description,..} => {
            if !description.is_empty() {builder.plain(&item.id,&[9,0],Arc::from(description.as_str()));}
        }
    }
    let blocks = builder.finish().search_blocks().into_iter().map(|(identity,text)| Block {identity,text}).collect();
    Document {item:item.id.clone(),revision:input.revision,turn:input.turn.clone(),blocks}
}

#[cfg(test)]
mod fragment_tests {
    use super::*;
    #[test]
    fn a_match_crossing_mount_chunks_keeps_both_halves_and_one_current_identity() {
        let found = Match {turn:"t".into(),item:"i".into(),identity:7,range:2046..2054};
        let paint = Paint::new(1,&[found.clone()],Some(found));
        assert_eq!(paint.fragment(7,0,2048),(vec![2046..2048],Some(2046..2048)));
        assert_eq!(paint.fragment(7,2048,2048),(vec![0..6],Some(0..6)));
        assert_eq!(paint.fragment(8,2048,2048),(Vec::new(),None));
    }
}

#[cfg(test)]
mod rendered_tests {
    use super::*;
    use crate::{agent::Provider,chat::{Item,ItemContent,Role,Phase,ToolStatus,CommandAction}};
    fn input(content:ItemContent) -> Input {
        Input {item:Arc::new(Item {id:"rendered-item".into(),role:Role::Assistant,content,at:None,started_at:None}),
            revision:1,turn:"turn".into(),growing:false}
    }
    #[test]
    fn markdown_matches_plain_prose_and_fenced_code_in_render_order() {
        let input=input(ItemContent::Text {text:"## Heading\n\nA **needle** and inline `code`.\n\n```text\nneedle\n```\n".into(),thinking:false,phase:Phase::Final});
        let document=document(&input,Provider::Claude);let generation=AtomicU64::new(1);
        let mut index=Index::default();let matches=index.search(&[document.clone()],"needle",1,&generation).unwrap();
        assert_eq!(matches.len(),2);
        assert_ne!(matches[0].identity,matches[1].identity);
        for found in matches {
            let block=document.blocks.iter().find(|block|block.identity==found.identity).unwrap();
            assert_eq!(&block.text[found.range],"needle");
            assert!(!block.text.contains("**"));
        }
    }
    #[test]
    fn displayed_command_label_and_directory_remain_searchable() {
        let input=input(ItemContent::Command {label:"Verify RELEASE".into(),command:"true".into(),working_directory:"/Workspace/Project".into(),
            output:"done".into(),action:CommandAction::Run,status:ToolStatus::Succeeded,exit_code:Some(0),duration_ms:None});
        let mut worker=Worker::default();let generation=AtomicU64::new(1);
        let label=worker.search(&[input],Provider::Claude,"release",1,&generation).unwrap();
        assert_eq!(label.len(),1);
        assert_eq!(label[0].identity,crate::app::chat_ui::selection::logical_identity("rendered-item",&[3,0]));
    }
    #[test]
    fn obsolete_query_cancels_inside_one_large_block() {
        let mut checks=0;
        let result=super::super::matched_ranges_cancellable(&"x".repeat(300_000),"absent",usize::MAX,|| {checks+=1;checks<4});
        assert!(result.is_none());assert_eq!(checks,4);
    }
}
