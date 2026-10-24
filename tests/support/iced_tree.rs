use std::{cell::Cell, rc::Rc};

#[derive(Debug)]
struct Remembered {
    value: u32,
    focused: bool,
}

struct Counted {
    name: &'static str,
    named: bool,
    value: u32,
    constructed: Rc<Cell<usize>>,
    diffed: Rc<Cell<usize>>,
    children: Vec<Counted>,
}

impl Widget<(), (), Renderer> for Counted {
    fn size(&self) -> iced::Size<iced::Length> {
        iced::Size::new(iced::Length::Shrink, iced::Length::Shrink)
    }

    fn layout(&mut self, _: &mut Tree, _: &Renderer, _: &layout::Limits) -> layout::Node {
        layout::Node::new(iced::Size::ZERO)
    }

    fn draw(
        &self,
        _: &Tree,
        _: &mut Renderer,
        _: &(),
        _: &renderer::Style,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &iced::Rectangle,
    ) {
    }

    fn id(&self) -> Option<Id> {
        self.named.then(|| Id::new(self.name))
    }

    fn tag(&self) -> Tag {
        Tag::of::<Remembered>()
    }

    fn state(&self) -> State {
        State::new(Remembered {
            value: self.value,
            focused: false,
        })
    }

    fn children(&self) -> Vec<Tree> {
        self.constructed.set(self.constructed.get() + 1);
        self.children
            .iter()
            .map(|child| Tree::new(child as &dyn Widget<(), (), Renderer>))
            .collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.diffed.set(self.diffed.get() + 1);
        let mut children: Vec<&mut dyn Widget<(), (), Renderer>> = self
            .children
            .iter_mut()
            .map(|child| child as &mut dyn Widget<(), (), Renderer>)
            .collect();
        tree.diff_children(&mut children);
    }
}

fn counted(entries: &[(&'static str, u32)]) -> Counted {
    let constructed = Rc::new(Cell::new(0));
    let diffed = Rc::new(Cell::new(0));
    Counted {
        name: "restoration-parent",
        named: true,
        value: 0,
        constructed: constructed.clone(),
        diffed: diffed.clone(),
        children: entries
            .iter()
            .map(|(name, value)| Counted {
                name,
                named: true,
                value: *value,
                constructed: constructed.clone(),
                diffed: diffed.clone(),
                children: Vec::new(),
            })
            .collect(),
    }
}

#[test]
fn same_named_tree_reuses_children_without_discarded_construction() {
    let mut widget = counted(&[("restoration-first", 1), ("restoration-last", 2)]);
    let mut tree = Tree::new(&widget as &dyn Widget<(), (), Renderer>);
    tree.children[0].state.downcast_mut::<Remembered>().focused = true;
    assert_eq!(widget.constructed.get(), 3);
    for _ in 0..3 {
        tree.diff(&mut widget as &mut dyn Widget<(), (), Renderer>);
        assert_eq!(
            widget.constructed.get(),
            3,
            "a reused subtree is never constructed again"
        );
        assert!(tree.children[0].state.downcast_ref::<Remembered>().focused);
        assert_eq!(tree.children[0].state.downcast_ref::<Remembered>().value, 1);
    }
    assert_eq!(
        widget.diffed.get(),
        9,
        "each real widget still receives diff"
    );
}

#[test]
fn extracted_named_children_keep_focus_across_reorder_and_insertion() {
    let mut widget = counted(&[("restoration-first", 1), ("restoration-last", 2)]);
    let mut tree = Tree::new(&widget as &dyn Widget<(), (), Renderer>);
    tree.children[0].state.downcast_mut::<Remembered>().focused = true;
    NAMED.with(|named| {
        assert!(named.borrow().is_empty());
        *named.borrow_mut() = tree.take_all_named();
    });
    widget.children.swap(0, 1);
    let mut inserted = counted(&[]);
    inserted.name = "restoration-new";
    inserted.value = 3;
    inserted.constructed = widget.constructed.clone();
    inserted.diffed = widget.diffed.clone();
    widget.children.insert(1, inserted);
    tree.diff(&mut widget as &mut dyn Widget<(), (), Renderer>);
    assert_eq!(
        widget.constructed.get(),
        4,
        "only the inserted child is constructed"
    );
    assert_eq!(widget.diffed.get(), 4);
    let values: Vec<_> = tree
        .children
        .iter()
        .map(|child| {
            let state = child.state.downcast_ref::<Remembered>();
            (state.value, state.focused)
        })
        .collect();
    assert_eq!(values, [(2, false), (3, false), (1, true)]);
    NAMED.with(|named| assert!(named.borrow().is_empty()));
}

#[test]
fn extracted_unnamed_child_restores_state_without_construction() {
    let mut widget = counted(&[("restoration-first", 1), ("restoration-last", 2)]);
    widget.children[0].named = false;
    let mut tree = Tree::new(&widget as &dyn Widget<(), (), Renderer>);
    tree.children[0].state.downcast_mut::<Remembered>().focused = true;
    NAMED.with(|named| {
        assert!(named.borrow().is_empty());
        *named.borrow_mut() = tree.take_all_named();
    });
    tree.diff(&mut widget as &mut dyn Widget<(), (), Renderer>);
    assert_eq!(widget.constructed.get(), 3);
    assert_eq!(widget.diffed.get(), 3);
    let restored = tree.children[0].state.downcast_ref::<Remembered>();
    assert_eq!(restored.value, 1);
    assert!(restored.focused);
    assert_eq!(tree.children[1].state.downcast_ref::<Remembered>().value, 2);
    NAMED.with(|named| assert!(named.borrow().is_empty()));
}

#[test]
fn changed_name_still_replaces_the_child_state() {
    let mut widget = counted(&[("restoration-first", 1)]);
    let mut tree = Tree::new(&widget as &dyn Widget<(), (), Renderer>);
    tree.children[0].state.downcast_mut::<Remembered>().focused = true;
    widget.children[0].name = "restoration-replacement";
    widget.children[0].value = 9;
    tree.diff(&mut widget as &mut dyn Widget<(), (), Renderer>);
    let replacement = tree.children[0].state.downcast_ref::<Remembered>();
    assert_eq!(replacement.value, 9);
    assert!(!replacement.focused);
    assert_eq!(widget.constructed.get(), 3);
}
