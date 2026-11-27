//! Which key repeats the real widget sends the hosted terminal, as its peer
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

fn down(key: keyboard::Key, code: keyboard::key::Code, repeat: bool) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        modified_key: key.clone(),
        key,
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
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

/// Escape closes the terminal's context menu on its key-down, which the menu
/// takes. Held a moment longer, the key repeats into the terminal, which never
/// had its press: the program must not take that repeat as an Escape of its
/// own (#4348). A key the terminal did send keeps repeating.
#[test]
fn a_repeat_whose_press_went_elsewhere_is_not_sent_to_the_program() {
    use keyboard::key::{Code, Named};
    let peer = Peer::new();
    let renderer = renderer();
    let mut widget = widget(&peer);
    let mut tree = Tree::new(&widget as &dyn Widget<(), Theme, Renderer>);
    let mut send = |event| dispatch(&mut widget, &mut tree, &renderer, event);
    send(Event::Window(
        window::Event::RedrawRequested(Instant::now()),
    ));
    peer.fence();

    let escape = keyboard::Key::Named(Named::Escape);
    send(down(escape.clone(), Code::Escape, true));
    send(down(escape.clone(), Code::Escape, true));
    assert_eq!(keys(&peer), [], "the menu took this key's press");
    // The worker pairs releases with the presses it wrote, so the release
    // is its to drop.
    send(up(escape.clone(), Code::Escape));
    assert_eq!(keys(&peer), [(KeyAction::Release, TerminalKey::Escape)]);

    send(down(escape.clone(), Code::Escape, false));
    send(down(escape.clone(), Code::Escape, true));
    send(up(escape.clone(), Code::Escape));
    assert_eq!(
        keys(&peer),
        [
            (KeyAction::Press, TerminalKey::Escape),
            (KeyAction::Repeat, TerminalKey::Escape),
            (KeyAction::Release, TerminalKey::Escape),
        ]
    );
    // Released, the key is nobody's again.
    send(down(escape, Code::Escape, true));
    assert_eq!(keys(&peer), []);
}
