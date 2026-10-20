//! Immutable publication roots shared by UI factories without cloning subtrees.
//!
//! Text has UI-local caches, so roots use Rc rather than claiming Sync. Only
//! the raw Send graph crosses to the persistent reclaimer at its final release.
use super::*;
use std::rc::Rc;

pub(super) struct Data {
    items: Vec<markdown::Item>,
    #[cfg(test)]
    pub(super) created_on: std::thread::ThreadId,
    #[cfg(test)]
    disposed: Option<std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>>,
}
impl Data {
    /// Full transcript publications and rich coordination parts snapshot on
    /// admitted parser workers. The bounded inline cache path also uses this
    /// storage constructor; admission and input bounds belong to its caller.
    pub(super) fn snapshot(content: &markdown::Content) -> Self {
        Self::items(content.items().to_vec())
    }
    fn items(items: Vec<markdown::Item>) -> Self {
        Self {
            items,
            #[cfg(test)]
            created_on: std::thread::current().id(),
            #[cfg(test)]
            disposed: None,
        }
    }
    #[cfg(test)]
    pub(super) fn watch_disposal(
        &mut self,
        sender: std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>,
    ) {
        self.disposed = Some(sender);
    }
}
impl Drop for Data {
    fn drop(&mut self) {
        let items = std::mem::take(&mut self.items);
        #[cfg(test)]
        let count = items.len();
        // This is the actual graph, not an unrelated lease/token. Reclaimer
        // batches drop raw Data inline; no nested retirement can bypass a FIFO
        // generation barrier or release admission before these destructors.
        drop(items);
        #[cfg(test)]
        if let Some(disposed) = self.disposed.take() {
            let _ = disposed.send((std::thread::current().id(), count));
        }
    }
}
struct Owner(Option<Data>);
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(data) = self.0.take() {
            preparation::retire_document(data);
        }
    }
}
#[derive(Clone)]
pub(super) struct Document(Rc<Owner>);
pub(super) enum Node<'a> {
    Items(&'a [markdown::Item]),
    Item(&'a markdown::Item),
    Bullet(&'a markdown::Bullet),
}
impl Document {
    pub(super) fn new(data: Data) -> Self {
        Self(Rc::new(Owner(Some(data))))
    }
    /// Snapshot CachedItem::extend's bounded inline cache input, including
    /// empty loading placeholders, once per cache change. Row factories never
    /// call this; full worker publications transfer Data through Self::new.
    pub(super) fn snapshot(content: &markdown::Content) -> Self {
        Self::new(Data::snapshot(content))
    }
    #[cfg(test)]
    pub(super) fn from_items(items: &[markdown::Item]) -> Self {
        Self::new(Data::items(items.to_vec()))
    }
    pub(super) fn items(&self) -> &[markdown::Item] {
        &self.0.0.as_ref().expect("live document root").items
    }
    pub(super) fn node(&self, path: &[u32]) -> Option<Node<'_>> {
        fn descend<'a>(items: &'a [markdown::Item], path: &[u32]) -> Option<Node<'a>> {
            let Some((&index, rest)) = path.split_first() else {
                return Some(Node::Items(items));
            };
            let item = items.get(index as usize)?;
            if rest.is_empty() {
                return Some(Node::Item(item));
            }
            match item {
                markdown::Item::Quote(items) => descend(items, rest),
                markdown::Item::List { bullets, .. } => {
                    let (&index, rest) = rest.split_first()?;
                    let bullet = bullets.get(index as usize)?;
                    if rest.is_empty() {
                        return Some(Node::Bullet(bullet));
                    }
                    let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                        bullet;
                    descend(items, rest)
                }
                markdown::Item::Table { columns, rows } => {
                    let (&row, rest) = rest.split_first()?;
                    let (&column, rest) = rest.split_first()?;
                    let items: &[markdown::Item] = if row == 0 {
                        &columns.get(column as usize)?.header
                    } else {
                        rows.get(row as usize - 1)?
                            .cells
                            .get(column as usize)
                            .map(Vec::as_slice)
                            .unwrap_or(&[])
                    };
                    descend(items, rest)
                }
                _ => None,
            }
        }
        descend(self.items(), path)
    }
}
