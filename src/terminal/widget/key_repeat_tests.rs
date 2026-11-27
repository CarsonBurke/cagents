//! Which key events the real widget sends the hosted terminal, as its peer
//! receives them.
use super::accessibility_tests::{Peer, renderer};
use super::*;
use crate::terminal::{KeyAction, TerminalKey, ipc::ClientRequest};

/// A focused widget with no shortcuts, so every key is the child's.
fn widget(peer: &Peer) -> TerminalWidget<()> {
    TerminalWidget::new(peer.handle.clone())
        .id(Id::new("card"))
        .model_focused(true)
}

fn dispatch(widget: &mut TerminalWidget<()>, tree: &mut Tree, renderer: &Renderer, event: Event) {
    let size = Size::new(640.0, 320.0);
    let layout = layout::Node::new(size);
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    widget.update(
        tree,
        &event,
        Layout::new(&layout),
        mouse::Cursor::Available(Point::new(80.0, 80.0)),
        renderer,
        &mut clipboard::Null,
        &mut shell,
        &Rectangle::with_size(size),
    );
}

fn down(
    key: keyboard::Key,
    code: keyboard::key::Code,
    modifiers: keyboard::Modifiers,
    repeat: bool,
) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        modified_key: key.clone(),
        key,
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat,
    })
}

fn up(key: keyboard::Key, code: keyboard::key::Code) -> Event {
    Event::Keyboard(keyboard::Event::KeyReleased {
        modified_key: key.clone(),
        key,
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
    })
}

/// The keys the peer received since the last fence.
fn keys(peer: &Peer) -> Vec<(KeyAction, TerminalKey)> {
    peer.fence()
        .into_iter()
        .filter_map(|request| match request {
            ClientRequest::Key { input, .. } => Some((input.action, input.key)),
            _ => None,
        })
        .collect()
}

/// A mounted, focused widget and the peer its keys go to.
struct Mounted {
    peer: Peer,
    renderer: Renderer,
    widget: TerminalWidget<()>,
    tree: Tree,
}

impl Mounted {
    fn new() -> Self {
        let peer = Peer::new();
        let widget = widget(&peer);
        let tree = Tree::new(&widget as &dyn Widget<(), Theme, Renderer>);
        let mut mounted = Self {
            peer,
            renderer: renderer(),
            widget,
            tree,
        };
        mounted.send(Event::Window(
            window::Event::RedrawRequested(Instant::now()),
        ));
        mounted.peer.fence();
        mounted
    }

    fn send(&mut self, event: Event) {
        dispatch(&mut self.widget, &mut self.tree, &self.renderer, event);
    }

    fn state(&mut self) -> &mut WidgetState {
        self.tree.state.downcast_mut::<WidgetState>()
    }
}

#[test]
fn alternate_screen_wheel_sends_one_key_tap_per_row() {
    let mut terminal = Mounted::new();
    Arc::make_mut(&mut terminal.widget.snapshot)
        .scroll
        .alternate_screen = true;
    terminal.widget.scroll_size = 2;
    terminal.send(Event::Mouse(mouse::Event::WheelScrolled {
        delta: ScrollDelta::Lines { x: 0.0, y: -1.0 },
    }));
    assert_eq!(
        keys(&terminal.peer),
        [(KeyAction::Tap, TerminalKey::ArrowDown); 2]
    );
}

/// Escape closes the terminal's context menu on its key-down, which the menu
/// takes. Held a moment longer, the key repeats into the terminal, which never
/// had its press: the program must not take that repeat as an Escape of its
/// own (#4348). A key the terminal did send keeps repeating.
#[test]
fn a_repeat_whose_press_went_elsewhere_is_not_sent_to_the_program() {
    use keyboard::key::{Code, Named};
    let plain = keyboard::Modifiers::empty();
    let escape = keyboard::Key::Named(Named::Escape);
    let mut terminal = Mounted::new();

    terminal.send(down(escape.clone(), Code::Escape, plain, true));
    terminal.send(down(escape.clone(), Code::Escape, plain, true));
    assert_eq!(keys(&terminal.peer), [], "the menu took this key's press");
    // The worker pairs releases with the presses it wrote, so the release
    // is its to drop.
    terminal.send(up(escape.clone(), Code::Escape));
    assert_eq!(
        keys(&terminal.peer),
        [(KeyAction::Release, TerminalKey::Escape)]
    );

    terminal.send(down(escape.clone(), Code::Escape, plain, false));
    terminal.send(down(escape.clone(), Code::Escape, plain, true));
    terminal.send(up(escape.clone(), Code::Escape));
    assert_eq!(
        keys(&terminal.peer),
        [
            (KeyAction::Press, TerminalKey::Escape),
            (KeyAction::Repeat, TerminalKey::Escape),
            (KeyAction::Release, TerminalKey::Escape),
        ]
    );
    // Released, the key is nobody's again.
    terminal.send(down(escape, Code::Escape, plain, true));
    assert_eq!(keys(&terminal.peer), []);
}

/// The copy chord is the widget's. When its Shift (or Command) comes up first,
/// the held key repeats as the program's own chord, which for Control+C is an
/// interrupt: the program never had the press, so it gets no repeat either.
#[test]
fn a_repeat_after_a_clipboard_chord_is_not_sent_to_the_program() {
    use keyboard::{Modifiers, key::Code};
    #[cfg(target_os = "linux")]
    let (chord, left) = (Modifiers::CTRL | Modifiers::SHIFT, Modifiers::CTRL);
    #[cfg(target_os = "macos")]
    let (chord, left) = (Modifiers::LOGO, Modifiers::empty());
    let c = keyboard::Key::Character("c".into());
    let mut terminal = Mounted::new();

    terminal.send(down(c.clone(), Code::KeyC, chord, false));
    terminal.send(down(c.clone(), Code::KeyC, left, true));
    assert_eq!(keys(&terminal.peer), []);
    // Pressed on its own, the same key is the program's.
    terminal.send(up(c.clone(), Code::KeyC));
    terminal.peer.fence();
    terminal.send(down(c.clone(), Code::KeyC, left, false));
    terminal.send(down(c, Code::KeyC, left, true));
    assert_eq!(
        keys(&terminal.peer)
            .into_iter()
            .map(|(action, _)| action)
            .collect::<Vec<_>>(),
        [KeyAction::Press, KeyAction::Repeat]
    );
}

/// With text selected, Escape drops the selection and is not the program's.
/// The repeat of an Escape that closed the context menu is not a second
/// Escape, so it leaves the selection; and held after dropping one, the key
/// does not start reaching the program.
#[test]
fn an_escape_repeat_neither_drops_a_selection_nor_reaches_the_program() {
    use keyboard::key::{Code, Named};
    let plain = keyboard::Modifiers::empty();
    let escape = keyboard::Key::Named(Named::Escape);
    let mut terminal = Mounted::new();
    let selection = Selection::new(
        SelectionCell { row: 0, column: 0 },
        SelectionMode::Character,
    );
    terminal.state().selection = Some(selection);

    terminal.send(down(escape.clone(), Code::Escape, plain, true));
    assert_eq!(terminal.state().selection, Some(selection));
    terminal.send(up(escape.clone(), Code::Escape));
    terminal.peer.fence();

    terminal.send(down(escape.clone(), Code::Escape, plain, false));
    assert_eq!(terminal.state().selection, None);
    terminal.send(down(escape.clone(), Code::Escape, plain, true));
    terminal.send(up(escape, Code::Escape));
    assert_eq!(keys(&terminal.peer), []);
}
