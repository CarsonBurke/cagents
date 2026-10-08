//! Store internal widget state in a state tree to ensure continuity.
use crate::Widget;
use crate::id::{Id, Internal};
use std::any::{self, Any};
use std::borrow::{Borrow, BorrowMut, Cow};
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::{fmt, mem};

thread_local! {
    /// A map of named widget states.
pub static NAMED: std::cell::RefCell<HashMap<Cow<'static, str>, (State, Vec<(usize, Tree)>)>> = std::cell::RefCell::new(HashMap::new());
}

/// A persistent state widget tree.
///
/// A [`Tree`] is normally associated with a specific widget in the widget tree.
#[derive(Debug)]
pub struct Tree {
    /// The tag of the [`Tree`].
    pub tag: Tag,

    /// the Id of the [`Tree`]
    pub id: Option<Id>,

    /// The [`State`] of the [`Tree`].
    pub state: State,

    /// The children of the root widget of the [`Tree`].
    pub children: Vec<Tree>,
}

impl Tree {
    /// Creates an empty, stateless [`Tree`] with no children.
    pub fn empty() -> Self {
        Self {
            id: None,
            tag: Tag::stateless(),
            state: State::None,
            children: Vec::new(),
        }
    }

    /// Creates a new [`Tree`] for the provided [`Widget`].
    pub fn new<'a, Message, Theme, Renderer>(
        widget: impl Borrow<dyn Widget<Message, Theme, Renderer> + 'a>,
    ) -> Self
    where
        Renderer: crate::Renderer,
    {
        let widget = widget.borrow();

        Self {
            id: widget.id(),
            tag: widget.tag(),
            state: widget.state(),
            children: widget.children(),
        }
    }

    /// Takes all named widgets from the tree.
    pub fn take_all_named(
        &mut self,
    ) -> HashMap<Cow<'static, str>, (State, Vec<(usize, Tree)>)> {
        let mut named = HashMap::new();
        struct Visit {
            parent: Cow<'static, str>,
            index: usize,
            visited: bool,
        }
        // tree traversal to find all named widgets
        // and keep their state and children
        let mut stack = vec![(self, None)];
        while let Some((tree, visit)) = stack.pop() {
            if let Some(Id(Internal::Custom(_, n))) = tree.id.clone() {
                let state = mem::replace(&mut tree.state, State::None);
                let children_count = tree.children.len();
                let children =
                    tree.children.iter_mut().enumerate().rev().map(|(i, c)| {
                        if matches!(c.id, Some(Id(Internal::Custom(_, _)))) {
                            (c, None)
                        } else {
                            (
                                c,
                                Some(Visit {
                                    index: i,
                                    parent: n.clone(),
                                    visited: false,
                                }),
                            )
                        }
                    });
                _ = named.insert(
                    n.clone(),
                    (state, Vec::with_capacity(children_count)),
                );
                stack.extend(children);
            } else if let Some(visit) = visit {
                if visit.visited {
                    named.get_mut(&visit.parent).unwrap().1.push((
                        visit.index,
                        mem::replace(
                            tree,
                            Tree {
                                id: tree.id.clone(),
                                tag: tree.tag,
                                ..Tree::empty()
                            },
                        ),
                    ));
                } else {
                    let ptr = tree as *mut Tree;

                    stack.push((
                        // TODO remove this unsafe block
                        #[allow(unsafe_code)]
                        // SAFETY: when the reference is finally accessed, all the children references will have been processed first.
                        unsafe {
                            ptr.as_mut().unwrap()
                        },
                        Some(Visit {
                            visited: true,
                            ..visit
                        }),
                    ));
                    stack.extend(tree.children.iter_mut().map(|c| (c, None)));
                }
            } else {
                stack.extend(tree.children.iter_mut().map(|s| (s, None)));
            }
        }

        named
    }

    /// Finds a widget state in the tree by its id.
    pub fn find<'a>(&'a self, id: &Id) -> Option<&'a Tree> {
        if self.id == Some(id.clone()) {
            return Some(self);
        }

        for child in self.children.iter() {
            if let Some(tree) = child.find(id) {
                return Some(tree);
            }
        }

        None
    }
    /// Reconciliates the current tree with the provided [`Widget`].
    ///
    /// If the tag of the [`Widget`] matches the tag of the [`Tree`], then the
    /// [`Widget`] proceeds with the reconciliation (i.e. [`Widget::diff`] is called).
    ///
    /// Otherwise, the whole [`Tree`] is recreated.
    ///
    /// [`Widget::diff`]: crate::Widget::diff
    pub fn diff<'a, Message, Theme, Renderer>(
        &mut self,
        mut new: impl BorrowMut<dyn Widget<Message, Theme, Renderer> + 'a>,
    ) where
        Renderer: crate::Renderer,
    {
        let borrowed: &mut dyn Widget<Message, Theme, Renderer> =
            new.borrow_mut();

        let mut tag_match = self.tag == borrowed.tag();

        if tag_match {
            if let Some(Id(Internal::Custom(_, n))) = borrowed.id() {
                if let Some((mut state, children)) = NAMED
                    .with(|named| named.borrow_mut().remove(&n))
                    .or_else(|| {
                        //check self.id
                        if let Some(Id(Internal::Custom(_, ref name))) = self.id
                        {
                            if name == &n {
                                Some((
                                    mem::replace(&mut self.state, State::None),
                                    self.children
                                        .iter_mut()
                                        .map(|s| {
                                            // take the data
                                            mem::replace(
                                                s,
                                                Tree {
                                                    id: s.id.clone(),
                                                    tag: s.tag,
                                                    ..Tree::empty()
                                                },
                                            )
                                        })
                                        .enumerate()
                                        .collect(),
                                ))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                {
                    std::mem::swap(&mut self.state, &mut state);
                    let widget_children = borrowed.children();
                    if !tag_match {
                        self.children = widget_children;
                    } else {
                        for (old_i, mut old) in children {
                            let Some(my_state) = self.children.get_mut(old_i)
                            else {
                                continue;
                            };
                            if my_state.tag != old.tag || {
                                !match (&old.id, &my_state.id) {
                                    (
                                        Some(Id(Internal::Custom(_, old_name))),
                                        Some(Id(Internal::Custom(_, my_name))),
                                    ) => old_name == my_name,
                                    (
                                        Some(Id(Internal::Set(a))),
                                        Some(Id(Internal::Set(b))),
                                    ) => a.len() == b.len(),
                                    (
                                        Some(Id(Internal::Unique(_))),
                                        Some(Id(Internal::Unique(_))),
                                    ) => true,
                                    (None, None) => true,
                                    _ => false,
                                }
                            } {
                                continue;
                            }

                            mem::swap(my_state, &mut old);
                        }
                    }
                } else {
                    tag_match = false;
                }
            } else {
                if let Some(id) = self.id.clone() {
                    borrowed.set_id(id);
                }
            }
        }
        if tag_match {
            borrowed.diff(self);
        } else {
            *self = Self::new(borrowed);
            let borrowed = new.borrow_mut();
            borrowed.diff(self);
        }
    }

    /// Reconciles the children of the tree with the provided list of widgets.
    pub fn diff_children<'a, Message, Theme, Renderer>(
        &mut self,
        new_children: &mut [impl BorrowMut<
            dyn Widget<Message, Theme, Renderer> + 'a,
        >],
    ) where
        Renderer: crate::Renderer,
    {
        self.diff_children_custom(
            new_children,
            new_children.iter().map(|c| c.borrow().id()).collect(),
            |tree, widget| {
                let borrowed: &mut dyn Widget<_, _, _> = widget.borrow_mut();

                tree.diff(borrowed);
            },
            |widget| {
                let borrowed: &dyn Widget<_, _, _> = widget.borrow();
                Self::new(borrowed)
            },
        );
    }

    /// Reconciles the children of the tree with the provided list of widgets using custom
    /// logic both for diffing and creating new widget state.
    pub fn diff_children_custom<T>(
        &mut self,
        new_children: &mut [T],
        new_ids: Vec<Option<Id>>,
        diff: impl Fn(&mut Tree, &mut T),
        new_state: impl Fn(&T) -> Self,
    ) {
        // Most rows/columns have no named children. Preserve their vector
        // allocation and positional reconciliation without building an index.
        let is_named =
            |id: &Option<Id>| matches!(id, Some(Id(Internal::Custom(_, _))));
        if !new_ids.iter().any(is_named)
            && !self.children.iter().any(|child| is_named(&child.id))
        {
            self.children.truncate(new_children.len());
            self.children.extend(
                new_children[self.children.len()..].iter().map(&new_state),
            );
            for (child, new) in self.children.iter_mut().zip(new_children) {
                diff(child, new);
            }
            return;
        }

        // Match by identity, then assemble in the new widget order. Keeping
        // matched children at their old indices can hand a widget its sibling's
        // state after a reorder; truncating first also loses surviving names.
        let mut named: HashMap<String, VecDeque<Tree>> = HashMap::new();
        let mut positional = Vec::with_capacity(self.children.len());
        for child in mem::take(&mut self.children) {
            if let Some(Id(Internal::Custom(_, name))) = &child.id {
                named.entry(name.to_string()).or_default().push_back(child);
                positional.push(None);
            } else {
                positional.push(Some(child));
            }
        }

        self.children = new_children
            .iter_mut()
            .zip(new_ids)
            .enumerate()
            .map(|(index, (new, id))| {
                let previous = match id {
                    Some(Id(Internal::Custom(_, name))) => named
                        .get_mut(name.as_ref())
                        .and_then(VecDeque::pop_front),
                    _ => positional.get_mut(index).and_then(Option::take),
                };
                let mut child = previous.unwrap_or_else(|| new_state(new));
                diff(&mut child, new);
                child
            })
            .collect();
    }
}

/// Reconciles the `current_children` with the provided list of widgets using
/// custom logic both for diffing and creating new widget state.
///
/// The algorithm will try to minimize the impact of diffing by querying the
/// `maybe_changed` closure.
pub fn diff_children_custom_with_search<T>(
    current_children: &mut Vec<Tree>,
    new_children: &mut [T],
    diff: impl Fn(&mut Tree, &mut T),
    maybe_changed: impl Fn(usize) -> bool,
    new_state: impl Fn(&T) -> Tree,
) {
    if new_children.is_empty() {
        current_children.clear();
        return;
    }

    if current_children.is_empty() {
        current_children.extend(new_children.iter().map(new_state));
        return;
    }

    let first_maybe_changed = maybe_changed(0);
    let last_maybe_changed = maybe_changed(current_children.len() - 1);

    if current_children.len() > new_children.len() {
        if !first_maybe_changed && last_maybe_changed {
            current_children.truncate(new_children.len());
        } else {
            let difference_index = if first_maybe_changed {
                0
            } else {
                (1..current_children.len())
                    .find(|&i| maybe_changed(i))
                    .unwrap_or(0)
            };

            let _ = current_children.splice(
                difference_index
                    ..difference_index
                        + (current_children.len() - new_children.len()),
                std::iter::empty(),
            );
        }
    }

    if current_children.len() < new_children.len() {
        let first_maybe_changed = maybe_changed(0);
        let last_maybe_changed = maybe_changed(current_children.len() - 1);

        if !first_maybe_changed && last_maybe_changed {
            current_children.extend(
                new_children[current_children.len()..].iter().map(new_state),
            );
        } else {
            let difference_index = if first_maybe_changed {
                0
            } else {
                (1..current_children.len())
                    .find(|&i| maybe_changed(i))
                    .unwrap_or(0)
            };

            let _ = current_children.splice(
                difference_index..difference_index,
                new_children[difference_index
                    ..difference_index
                        + (new_children.len() - current_children.len())]
                    .iter()
                    .map(new_state),
            );
        }
    }

    // TODO: Merge loop with extend logic (?)
    for (child_state, new) in
        current_children.iter_mut().zip(new_children.iter_mut())
    {
        diff(child_state, new);
    }
}

/// The identifier of some widget state.
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct Tag(any::TypeId);

impl Tag {
    /// Creates a [`Tag`] for a state of type `T`.
    pub fn of<T>() -> Self
    where
        T: 'static,
    {
        Self(any::TypeId::of::<T>())
    }

    /// Creates a [`Tag`] for a stateless widget.
    pub fn stateless() -> Self {
        Self::of::<()>()
    }
}

/// The internal [`State`] of a widget.
pub enum State {
    /// No meaningful internal state.
    None,

    /// Some meaningful internal state.
    Some(Box<dyn Any>),
}

impl State {
    /// Creates a new [`State`].
    pub fn new<T>(state: T) -> Self
    where
        T: 'static,
    {
        State::Some(Box::new(state))
    }

    /// Downcasts the [`State`] to `T` and returns a reference to it.
    ///
    /// # Panics
    /// This method will panic if the downcast fails or the [`State`] is [`State::None`].
    pub fn downcast_ref<T>(&self) -> &T
    where
        T: 'static,
    {
        match self {
            State::None => panic!("Downcast on stateless state"),
            State::Some(state) => {
                state.downcast_ref().expect("Downcast widget state")
            }
        }
    }

    /// Downcasts the [`State`] to `T` and returns a mutable reference to it.
    ///
    /// # Panics
    /// This method will panic if the downcast fails or the [`State`] is [`State::None`].
    pub fn downcast_mut<T>(&mut self) -> &mut T
    where
        T: 'static,
    {
        match self {
            State::None => panic!("Downcast on stateless state"),
            State::Some(state) => {
                state.downcast_mut().expect("Downcast widget state")
            }
        }
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "State::None"),
            Self::Some(_) => write!(f, "State::Some"),
        }
    }
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;

    fn reconcile(tree: &mut Tree, entries: &[(Option<&str>, u32)]) {
        let mut entries = entries.to_vec();
        let ids = entries
            .iter()
            .map(|(name, _)| name.map(|name| Id::new(name.to_owned())))
            .collect();
        tree.diff_children_custom(
            &mut entries,
            ids,
            |_, _| {},
            |(name, value)| Tree {
                id: name.map(|name| Id::new(name.to_owned())),
                tag: Tag::of::<u32>(),
                state: State::new(*value),
                children: Vec::new(),
            },
        );
    }

    fn values(tree: &Tree) -> Vec<u32> {
        tree.children
            .iter()
            .map(|child| *child.state.downcast_ref::<u32>())
            .collect()
    }

    #[test]
    fn named_and_positional_children_reassemble_in_widget_order() {
        let mut tree = Tree::empty();
        reconcile(&mut tree, &[(Some("button"), 1), (None, 2)]);
        reconcile(&mut tree, &[(None, 3), (Some("button"), 4)]);
        assert_eq!(values(&tree), [3, 1]);
        reconcile(&mut tree, &[(Some("button"), 5), (None, 6)]);
        assert_eq!(values(&tree), [1, 6]);
    }

    #[test]
    fn removal_keeps_surviving_names_beyond_the_new_length() {
        let mut tree = Tree::empty();
        reconcile(&mut tree, &[(Some("first"), 1), (Some("last"), 2)]);
        reconcile(&mut tree, &[(Some("last"), 3)]);
        assert_eq!(values(&tree), [2]);
    }

    #[test]
    fn insertion_and_reordering_keep_each_named_state() {
        let mut tree = Tree::empty();
        reconcile(&mut tree, &[(Some("first"), 1), (Some("last"), 2)]);
        reconcile(
            &mut tree,
            &[(Some("last"), 3), (Some("new"), 4), (Some("first"), 5)],
        );
        assert_eq!(values(&tree), [2, 4, 1]);
    }

    #[test]
    fn repeated_names_consume_trees_in_order_without_overwriting() {
        let mut tree = Tree::empty();
        reconcile(&mut tree, &[(Some("same"), 1), (Some("same"), 2)]);
        reconcile(
            &mut tree,
            &[(None, 3), (Some("same"), 4), (Some("same"), 5)],
        );
        assert_eq!(values(&tree), [3, 1, 2]);
    }

    #[test]
    fn unnamed_fast_path_keeps_storage_and_diffs_new_and_retained_children() {
        let mut tree = Tree::empty();
        tree.children = Vec::with_capacity(8);
        let mut initial = [10_u32, 20];
        let make = |value: &u32| Tree {
            id: None,
            tag: Tag::of::<u32>(),
            state: State::new(*value),
            children: Vec::new(),
        };
        let diff = |child: &mut Tree, _: &mut u32| {
            *child.state.downcast_mut::<u32>() += 1;
        };
        tree.diff_children_custom(&mut initial, vec![None; 2], diff, make);
        assert_eq!(values(&tree), [11, 21]);
        let allocation = tree.children.as_ptr();
        let mut expanded = [100, 200, 30];
        tree.diff_children_custom(&mut expanded, vec![None; 3], diff, make);
        assert_eq!(values(&tree), [12, 22, 31]);
        assert_eq!(tree.children.as_ptr(), allocation);
        let mut shortened = [0];
        tree.diff_children_custom(&mut shortened, vec![None], diff, make);
        assert_eq!(values(&tree), [13]);
        assert_eq!(tree.children.as_ptr(), allocation);
    }

    #[test]
    fn unnamed_children_keep_their_index_and_removed_children_are_dropped() {
        let mut tree = Tree::empty();
        reconcile(&mut tree, &[(None, 1), (None, 2), (Some("gone"), 3)]);
        reconcile(&mut tree, &[(None, 4), (None, 5)]);
        assert_eq!(values(&tree), [1, 2]);
        reconcile(&mut tree, &[]);
        assert!(tree.children.is_empty());
    }
}
