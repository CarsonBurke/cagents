//! Exercise the real widget and hosted client through a private Unix socket.
//! The fake peer records decoded production requests; fences are ordered input
//! sent directly by the test, never widget events or real PTY input.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalId},
    terminal::{
        HostedTerminalHandle,
        ipc::{
            ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES,
            PROTOCOL_VERSION, ServerEnvelope, ServerMessage, ServerResponse, TerminalDescriptor,
            write_frame,
        },
    },
};
use cosmic::iced::advanced::renderer::Headless;
use iced_accessibility::{
    A11yId,
    accesskit::{Action, ActionData, ActionRequest, TreeId},
};
use std::{
    os::unix::net::UnixListener,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
};

#[derive(Clone, Debug, PartialEq)]
enum Message {
    Review(bool),
    Focus,
    Shortcut,
    Paste,
    Edited(String),
}
pub(crate) struct Peer {
    _directory: tempfile::TempDir,
    handle: Option<HostedTerminalHandle>,
    release_ready: Arc<AtomicBool>,
    release_accept: Arc<AtomicBool>,
    reconnect: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    requests: crossbeam_channel::Receiver<ClientRequest>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Peer {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("review.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let id = TerminalId::new();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("review-test-session"),
            boot_id: BootId::new("review-test-boot"),
            host_incarnation: HostIncarnation::new("review-test-host"),
        };
        let descriptor = TerminalDescriptor::relaunchable(
            id,
            LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: Vec::new(),
                working_directory: "/".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            None,
        );
        let handle =
            HostedTerminalHandle::connect_at(socket, id, Some(proof.clone()), |_| {}).unwrap();
        let (tx, requests) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let release_ready = Arc::new(AtomicBool::new(true));
        let release_accept = Arc::new(AtomicBool::new(true));
        let reconnect = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let ready = release_ready.clone();
        let accepted = release_accept.clone();
        let reconnecting = reconnect.clone();
        let connected_count = connections.clone();
        let worker = thread::spawn(move || {
            loop {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if stopping.load(Ordering::Relaxed) {
                                return;
                            }
                            assert!(Instant::now() < deadline, "hosted client did not connect");
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("accept private peer: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
                connected_count.fetch_add(1, Ordering::AcqRel);
                let mut connected = false;
                let mut retry = false;
                while !stopping.load(Ordering::Relaxed) {
                    if reconnecting.swap(false, Ordering::AcqRel) {
                        retry = true;
                        break;
                    }
                    let request: ClientEnvelope = match codec.read(&mut stream) {
                        Ok(Some(request)) => request,
                        Ok(None) => break,
                        Err(FrameError::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue;
                        }
                        Err(error) => panic!("decode production request: {error}"),
                    };
                    let response = if !connected {
                        assert!(matches!(request.request, ClientRequest::Connect { .. }));
                        connected = true;
                        let mut descriptor = descriptor.clone();
                        descriptor.attached = true;
                        ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![descriptor],
                        }
                    } else {
                        let release = matches!(&request.request, ClientRequest::Key { input, .. } if input.action == KeyAction::Release);
                        tx.send(request.request).unwrap();
                        if release {
                            let deadline = Instant::now() + Duration::from_secs(3);
                            while !ready.load(Ordering::Acquire)
                                && !stopping.load(Ordering::Relaxed)
                            {
                                assert!(Instant::now() < deadline, "release ACK gate timed out");
                                thread::sleep(Duration::from_millis(1));
                            }
                        }
                        if release && !accepted.load(Ordering::Acquire) {
                            ServerResponse::Snapshot {
                                terminal_id: id,
                                snapshot: None,
                            }
                        } else {
                            ServerResponse::Accepted
                        }
                    };
                    if write_frame(
                        &mut stream,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: proof.clone(),
                            message: ServerMessage::Response { response },
                        },
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                if !retry {
                    break;
                }
            }
        });
        handle.wait_until_attached(Duration::from_secs(3)).unwrap();
        Self {
            _directory: directory,
            handle: Some(handle),
            release_ready,
            release_accept,
            reconnect,
            connections,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    pub(crate) fn handle(&self) -> &HostedTerminalHandle {
        self.handle.as_ref().unwrap()
    }
    pub(crate) fn fence(&self) -> Vec<ClientRequest> {
        let marker = b"accessibility-test-fence".to_vec();
        self.handle().input(marker.clone()).unwrap();
        let mut requests = Vec::new();
        loop {
            let request = self
                .requests
                .recv_timeout(Duration::from_secs(3))
                .expect("ordered wire fence");
            if matches!(&request,ClientRequest::Input { bytes,.. } if bytes==&marker) {
                break;
            }
            requests.push(request);
        }
        requests
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            let _ = handle.shutdown();
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
fn renderer() -> Renderer {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(<Renderer as Headless>::new(
            Font::DEFAULT,
            Pixels(16.0),
            Some("tiny-skia"),
        ))
        .unwrap()
}
fn widget(
    peer: &Peer,
    name: &str,
    barrier: ReviewInputBarrier,
    focused: bool,
    suppressed: bool,
) -> TerminalWidget<Message> {
    let mut widget = TerminalWidget::new(peer.handle().clone())
        .id(Id::new(name.to_owned()))
        .model_focused(focused)
        .native_activation_allowed(true)
        .review_boundary(
            barrier,
            suppressed,
            suppressed && focused,
            0,
            Message::Review,
        )
        .on_focus(Message::Focus)
        .on_shortcut(|_| Some(Message::Shortcut))
        .on_paste(|_| Message::Paste);
    widget.snapshot = Arc::new(RenderSnapshot {
        generation: 1,
        rows: 1,
        columns: 4,
        lines: vec![Arc::new(RowSnapshot {
            cells: ["漢", "", "e\u{301}", "x"]
                .map(|text| CellSnapshot {
                    text: super::super::CellText::new(text),
                    ..Default::default()
                })
                .to_vec(),
            ..Default::default()
        })]
        .into(),
        ..RenderSnapshot::empty()
    });
    widget
}
fn dispatch(
    widget: &mut TerminalWidget<Message>,
    tree: &mut Tree,
    renderer: &Renderer,
    event: Event,
) -> Vec<Message> {
    let size = Size::new(640.0, 320.0);
    let layout = layout::Node::new(size);
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    let mut clipboard = clipboard::Null;
    widget.update(
        tree,
        &event,
        Layout::new(&layout),
        mouse::Cursor::Available(Point::new(80.0, 80.0)),
        renderer,
        &mut clipboard,
        &mut shell,
        &Rectangle::with_size(size),
    );
    messages
}
fn action(id: impl Into<A11yId>, action: Action, data: Option<ActionData>) -> Event {
    // Match the native Winit conversion, which delivers a numeric Unique ID
    // even when the published widget owns a named Custom ID.
    let target_node: iced_accessibility::accesskit::NodeId = id.into().into();
    Event::A11y(
        Id::from(target_node.0),
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node,
            data,
        },
    )
}
fn key(named: keyboard::key::Named, code: keyboard::key::Code, down: bool) -> Event {
    let key = keyboard::Key::Named(named);
    let physical_key = keyboard::key::Physical::Code(code);
    Event::Keyboard(if down {
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key,
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        }
    } else {
        keyboard::Event::KeyReleased {
            key: key.clone(),
            modified_key: key,
            physical_key,
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        }
    })
}
fn assert_no_pty_input(requests: &[ClientRequest]) {
    assert!(
        !requests.iter().any(|request| matches!(
            request,
            ClientRequest::Key { .. }
                | ClientRequest::Input { .. }
                | ClientRequest::Paste { .. }
                | ClientRequest::Mouse { .. }
                | ClientRequest::Focus { .. }
                | ClientRequest::Scroll { .. }
        )),
        "assistive reading emitted input: {requests:?}"
    );
}
#[test]
fn real_widget_assistive_focus_selection_and_navigation_emit_no_pty_commands_on_either_peer() {
    let old = Peer::new();
    let new = Peer::new();
    let renderer = renderer();
    let shared = ReviewInputBarrier::default();
    let mut old_widget = widget(&old, "old-card", shared.clone(), true, false);
    let mut new_widget = widget(&new, "new-card", shared.clone(), false, false);
    let mut old_tree = Tree::new(&old_widget as &dyn Widget<Message, Theme, Renderer>);
    let mut new_tree = Tree::new(&new_widget as &dyn Widget<Message, Theme, Renderer>);
    native_focus(&mut old_widget, &mut old_tree, &renderer);
    let redraw = || Event::Window(window::Event::RedrawRequested(Instant::now()));
    dispatch(&mut old_widget, &mut old_tree, &renderer, redraw());
    dispatch(&mut new_widget, &mut new_tree, &renderer, redraw());
    assert!(
        old.fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Focus { focused: true, .. }))
    );
    new.fence();
    let layout = layout::Node::new(Size::new(640.0, 320.0));
    let accessible =
        new_widget.a11y_nodes(Layout::new(&layout), &new_tree, mouse::Cursor::Unavailable);

    let shifted = new_widget.a11y_nodes(
        Layout::new(&layout).with_virtual_offset(cosmic::iced::Vector::new(11.0, 37.0)),
        &new_tree,
        mouse::Cursor::Unavailable,
    );
    assert!(!accessible.root().is_empty());
    assert_eq!(accessible.children().len(), shifted.children().len());
    for (normal, shifted) in accessible
        .root()
        .iter()
        .chain(accessible.children())
        .zip(shifted.root().iter().chain(shifted.children()))
    {
        assert_eq!(normal.id(), shifted.id());
        if let Some(bounds) = normal.node().bounds() {
            let moved = shifted.node().bounds().unwrap();
            assert!((moved.x0 - (bounds.x0 - 11.0)).abs() < 0.0001);
            assert!((moved.x1 - (bounds.x1 - 11.0)).abs() < 0.0001);
            assert!((moved.y0 - (bounds.y0 - 37.0)).abs() < 0.0001);
            assert!((moved.y1 - (bounds.y1 - 37.0)).abs() < 0.0001);
        }
    }

    let published =
        new_widget.a11y_nodes(Layout::new(&layout), &new_tree, mouse::Cursor::Unavailable);
    let focus_id = published.root()[0].id().clone();
    assert_eq!(
        dispatch(
            &mut new_widget,
            &mut new_tree,
            &renderer,
            action(focus_id.clone(), Action::Focus, None)
        ),
        vec![Message::Review(true)]
    );
    // Apply the app-owned handoff to ALL mounted widgets, just as the terminal
    // builder does after TerminalReviewMode updates workspace focus.
    old_widget.model_focused = false;
    old_widget.suppress_focus_reports = true;
    new_widget.model_focused = true;
    new_widget.suppress_focus_reports = true;
    old_widget.review_active = false;
    old_widget.review_revision = 1;
    new_widget.review_active = true;
    new_widget.review_revision = 1;
    // Apply the actual widget diff, which retires per-native-batch activation
    // once the App's logical owner projection has changed.
    old_tree.diff(&mut old_widget as &mut dyn Widget<Message, Theme, Renderer>);
    new_tree.diff(&mut new_widget as &mut dyn Widget<Message, Theme, Renderer>);
    dispatch(&mut old_widget, &mut old_tree, &renderer, redraw());
    dispatch(&mut new_widget, &mut new_tree, &renderer, redraw());
    let selection = {
        let state = new_tree.state.downcast_ref::<WidgetState>();
        let mut a11y = state.a11y.borrow_mut();
        a11y.move_review(0, 1, false);
        a11y.selection.clone().unwrap()
    };
    dispatch(
        &mut new_widget,
        &mut new_tree,
        &renderer,
        action(
            focus_id,
            Action::SetTextSelection,
            Some(ActionData::SetTextSelection(selection)),
        ),
    );
    for event in [
        key(
            keyboard::key::Named::ArrowRight,
            keyboard::key::Code::ArrowRight,
            true,
        ),
        key(
            keyboard::key::Named::ArrowRight,
            keyboard::key::Code::ArrowRight,
            false,
        ),
        Event::InputMethod(input_method::Event::Commit("must not paste".into())),
        Event::Mouse(mouse::Event::ButtonPressed(IcedMouseButton::Right)),
        Event::Mouse(mouse::Event::ButtonReleased(IcedMouseButton::Right)),
    ] {
        assert!(
            !dispatch(&mut new_widget, &mut new_tree, &renderer, event)
                .iter()
                .any(|message| matches!(
                    message,
                    Message::Paste | Message::Shortcut | Message::Focus
                ))
        );
    }
    assert_no_pty_input(&old.fence());
    assert_no_pty_input(&new.fence());
    // Reconstruct the named widget and diff retained state before addressing
    // the actual published control node by its native numeric ID.
    new_widget = widget(&new, "new-card", shared.clone(), true, true);
    new_widget.review_revision = 1;
    new_tree.diff(&mut new_widget as &mut dyn Widget<Message, Theme, Renderer>);
    let published =
        new_widget.a11y_nodes(Layout::new(&layout), &new_tree, mouse::Cursor::Unavailable);
    let control = published
        .children()
        .iter()
        .find(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        .unwrap()
        .id()
        .clone();
    assert_eq!(
        dispatch(
            &mut new_widget,
            &mut new_tree,
            &renderer,
            action(control, Action::Click, None)
        ),
        vec![Message::Review(false)]
    );
    old_widget.suppress_focus_reports = false;
    new_widget.suppress_focus_reports = false;
    old_widget.review_active = false;
    old_widget.review_revision = 2;
    new_widget.review_active = false;
    new_widget.review_revision = 2;
    dispatch(&mut old_widget, &mut old_tree, &renderer, redraw());
    dispatch(&mut new_widget, &mut new_tree, &renderer, redraw());
    assert!(
        old.fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Focus { focused: false, .. }))
    );
    assert!(
        new.fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Focus { focused: true, .. }))
    );
    new_widget.on_shortcut = None;
    dispatch(
        &mut new_widget,
        &mut new_tree,
        &renderer,
        key(
            keyboard::key::Named::Enter,
            keyboard::key::Code::Enter,
            true,
        ),
    );
    assert!(
        new.fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. }))
    );
    // A key held during review may release after ordinary focus moves to the
    // other card. The shared app barrier must consume it there, then retire it.
    let new_id = new_widget.id.clone().unwrap();
    assert_eq!(
        dispatch(
            &mut new_widget,
            &mut new_tree,
            &renderer,
            action(new_id, Action::Focus, None)
        ),
        vec![Message::Review(true)]
    );
    old_widget.suppress_focus_reports = true;
    new_widget.suppress_focus_reports = true;
    old_widget.review_active = false;
    old_widget.review_revision = 3;
    new_widget.review_active = true;
    new_widget.review_revision = 3;
    dispatch(
        &mut new_widget,
        &mut new_tree,
        &renderer,
        key(
            keyboard::key::Named::ArrowDown,
            keyboard::key::Code::ArrowDown,
            true,
        ),
    );
    assert_no_pty_input(&new.fence());
    old_widget.model_focused = true;
    new_widget.model_focused = false;
    old_widget.suppress_focus_reports = false;
    new_widget.suppress_focus_reports = false;
    old_widget.review_active = false;
    old_widget.review_revision = 4;
    new_widget.review_active = false;
    new_widget.review_revision = 4;
    dispatch(&mut old_widget, &mut old_tree, &renderer, redraw());
    dispatch(&mut new_widget, &mut new_tree, &renderer, redraw());
    old.fence();
    new.fence();
    dispatch(
        &mut old_widget,
        &mut old_tree,
        &renderer,
        key(
            keyboard::key::Named::ArrowDown,
            keyboard::key::Code::ArrowDown,
            false,
        ),
    );
    assert_no_pty_input(&old.fence());
    old_widget.on_shortcut = None;
    native_focus(&mut old_widget, &mut old_tree, &renderer);
    dispatch(
        &mut old_widget,
        &mut old_tree,
        &renderer,
        key(
            keyboard::key::Named::ArrowDown,
            keyboard::key::Code::ArrowDown,
            true,
        ),
    );
    dispatch(
        &mut old_widget,
        &mut old_tree,
        &renderer,
        key(
            keyboard::key::Named::ArrowDown,
            keyboard::key::Code::ArrowDown,
            false,
        ),
    );
    assert_eq!(
        old.fence()
            .iter()
            .filter(|request| matches!(request, ClientRequest::Key { .. }))
            .count(),
        2
    );
}

fn click_review(
    widget: &mut TerminalWidget<Message>,
    tree: &mut Tree,
    renderer: &Renderer,
) -> Vec<Message> {
    let layout = layout::Node::new(Size::new(640.0, 320.0));
    let published = widget.a11y_nodes(Layout::new(&layout), tree, mouse::Cursor::Unavailable);
    let control = published
        .children()
        .iter()
        .find(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        .unwrap()
        .id()
        .clone();
    dispatch(widget, tree, renderer, action(control, Action::Click, None))
}
fn reviewing(tree: &Tree) -> bool {
    tree.state
        .downcast_ref::<WidgetState>()
        .a11y
        .borrow()
        .review
}
#[test]
fn native_review_entry_survives_old_view_events_until_acknowledged_and_cancelled() {
    let peer = Peer::new();
    let renderer = renderer();
    let mut widget = widget(
        &peer,
        "preack-card",
        ReviewInputBarrier::default(),
        true,
        false,
    );
    let mut tree = Tree::new(&widget as &dyn Widget<Message, Theme, Renderer>);
    let redraw = || Event::Window(window::Event::RedrawRequested(Instant::now()));
    dispatch(&mut widget, &mut tree, &renderer, redraw());
    peer.fence();
    assert_eq!(
        click_review(&mut widget, &mut tree, &renderer),
        vec![Message::Review(true)]
    );
    dispatch(&mut widget, &mut tree, &renderer, redraw());
    tree.diff(&mut widget as &mut dyn Widget<Message, Theme, Renderer>);
    assert!(
        reviewing(&tree),
        "old-view redraw and rebuild cannot undo pending entry"
    );
    for down in [true, false] {
        dispatch(
            &mut widget,
            &mut tree,
            &renderer,
            key(
                keyboard::key::Named::ArrowLeft,
                keyboard::key::Code::ArrowLeft,
                down,
            ),
        );
    }
    assert_no_pty_input(&peer.fence());
    widget.review_active = true;
    widget.suppress_focus_reports = true;
    widget.review_revision = 1;
    tree.diff(&mut widget as &mut dyn Widget<Message, Theme, Renderer>);
    assert!(reviewing(&tree));
    assert_eq!(
        click_review(&mut widget, &mut tree, &renderer),
        vec![Message::Review(false)]
    );
    // A later true acknowledgment must not resurrect a locally requested exit.
    widget.review_revision = 2;
    tree.diff(&mut widget as &mut dyn Widget<Message, Theme, Renderer>);
    assert!(!reviewing(&tree));
    widget.review_active = false;
    widget.suppress_focus_reports = false;
    widget.review_revision = 3;
    tree.diff(&mut widget as &mut dyn Widget<Message, Theme, Renderer>);
    assert!(!reviewing(&tree));
}
#[test]
fn same_card_cancellation_clears_pending_review_without_a_true_view_acknowledgment() {
    let peer = Peer::new();
    let renderer = renderer();
    let mut widget = widget(
        &peer,
        "cancel-card",
        ReviewInputBarrier::default(),
        true,
        false,
    );
    let mut tree = Tree::new(&widget as &dyn Widget<Message, Theme, Renderer>);
    assert_eq!(
        click_review(&mut widget, &mut tree, &renderer),
        vec![Message::Review(true)]
    );
    // The parent processes entry and explicit same-card FocusCard before rebuilding.
    // The final view is still focused, false, and globally unsuppressed.
    widget.review_revision = 2;
    tree.diff(&mut widget as &mut dyn Widget<Message, Theme, Renderer>);
    assert!(!reviewing(&tree));
    widget.on_shortcut = None;
    dispatch(
        &mut widget,
        &mut tree,
        &renderer,
        key(
            keyboard::key::Named::Enter,
            keyboard::key::Code::Enter,
            true,
        ),
    );
    assert!(
        peer.fence()
            .iter()
            .any(|request| matches!(request, ClientRequest::Key { .. }))
    );
}
#[test]
fn remounted_owner_blocks_input_and_identity_replacement_drops_selection() {
    let peer = Peer::new();
    let replacement = Peer::new();
    let renderer = renderer();
    let barrier = ReviewInputBarrier::default();
    let mut widget = widget(&peer, "remount-card", barrier.clone(), true, true);
    widget.review_revision = 5;
    let mut tree = Tree::new(&widget as &dyn Widget<Message, Theme, Renderer>);
    assert!(reviewing(&tree));
    dispatch(
        &mut widget,
        &mut tree,
        &renderer,
        key(
            keyboard::key::Named::ArrowDown,
            keyboard::key::Code::ArrowDown,
            true,
        ),
    );
    assert_no_pty_input(&peer.fence());
    // The projection is intentionally lazy until a native tree is requested.
    // Mount that tree before addressing its actual emitted text-run node.
    let layout = layout::Node::new(Size::new(640.0, 320.0));
    let published = widget.a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable);
    let run = published
        .children()
        .iter()
        .find(|node| node.node().role() == iced_accessibility::accesskit::Role::TextRun)
        .unwrap()
        .id()
        .clone()
        .into();
    let selection = iced_accessibility::accesskit::TextSelection {
        anchor: iced_accessibility::accesskit::TextPosition {
            node: run,
            character_index: 0,
        },
        focus: iced_accessibility::accesskit::TextPosition {
            node: run,
            character_index: 1,
        },
    };
    assert_eq!(
        dispatch(
            &mut widget,
            &mut tree,
            &renderer,
            action(
                published.root()[0].id().clone(),
                Action::SetTextSelection,
                Some(ActionData::SetTextSelection(selection))
            )
        ),
        vec![Message::Review(true)]
    );
    assert!(
        tree.state
            .downcast_ref::<WidgetState>()
            .a11y
            .borrow()
            .selection
            .is_some()
    );
    assert_no_pty_input(&peer.fence());
    widget = self::widget(&replacement, "remount-card", barrier, true, true);
    widget.review_revision = 5;
    tree.diff(&mut widget as &mut dyn Widget<Message, Theme, Renderer>);
    assert!(
        reviewing(&tree),
        "active owner blocks input until replacement cancellation arrives"
    );
    assert!(
        tree.state
            .downcast_ref::<WidgetState>()
            .a11y
            .borrow()
            .selection
            .is_none()
    );
    dispatch(
        &mut widget,
        &mut tree,
        &renderer,
        key(
            keyboard::key::Named::ArrowLeft,
            keyboard::key::Code::ArrowLeft,
            true,
        ),
    );
    assert_no_pty_input(&replacement.fence());
    widget.review_active = false;
    widget.suppress_focus_reports = false;
    widget.review_revision = 6;
    tree.diff(&mut widget as &mut dyn Widget<Message, Theme, Renderer>);
    assert!(!reviewing(&tree));
}

fn native_focus(widget: &mut TerminalWidget<Message>, tree: &mut Tree, renderer: &Renderer) {
    let layout = layout::Node::new(Size::new(640.0, 320.0));
    let mut operation = operation::focusable::focus::<()>(widget.id.clone().unwrap());
    widget.operate(tree, Layout::new(&layout), renderer, &mut operation);
}
fn raw_letter(
    code: keyboard::key::Code,
    letter: &str,
    down: bool,
    modifiers: keyboard::Modifiers,
) -> Event {
    let key = keyboard::Key::Character(letter.to_owned().into());
    let physical_key = keyboard::key::Physical::Code(code);
    Event::Keyboard(if down {
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key,
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }
    } else {
        keyboard::Event::KeyReleased {
            key: key.clone(),
            modified_key: key,
            physical_key,
            location: keyboard::Location::Standard,
            modifiers,
        }
    })
}

#[test]
fn native_field_owns_ignored_keys_and_ime_while_original_terminal_release_stays_paired() {
    use crate::input_trace::{InputTrace, KeyboardRegion, observe_keyboard_events};
    use crate::workspace_widget::FocusedPane;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    for field_first in [false, true] {
        let peer = Peer::new();
        let mut renderer = renderer();
        let barrier = ReviewInputBarrier::default();
        let trace = InputTrace::default();
        trace.bind_terminal_keys(barrier.clone());
        let terminal_id = Id::unique();
        let field_id = Id::unique();
        let mut terminal = widget(&peer, "raw-pane", barrier.clone(), true, false);
        terminal.id = Some(terminal_id.clone());
        terminal.on_shortcut = None;
        let terminal: Element<'_, Message> = terminal.into();
        let field: Element<'_, Message> = cosmic::widget::text_input("Query", "retained")
            .id(field_id.clone())
            .on_input(Message::Edited)
            .width(160.0)
            .into();
        let row = cosmic::widget::row::with_children(if field_first {
            vec![field, terminal]
        } else {
            vec![terminal, field]
        });
        let element = observe_keyboard_events(
            row,
            &trace,
            vec![
                (
                    terminal_id.clone(),
                    KeyboardRegion {
                        pane: FocusedPane::Terminal,
                        card: Some(crate::model::CardId::new()),
                    },
                ),
                (
                    field_id.clone(),
                    KeyboardRegion {
                        pane: FocusedPane::TextInput,
                        card: None,
                    },
                ),
            ],
            |_, _, _, _| Message::Shortcut,
        );
        let mut ui = UserInterface::build(
            element,
            Size::new(800.0, 320.0),
            Cache::new(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(terminal_id.clone()),
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &[raw_letter(
                keyboard::key::Code::KeyR,
                "r",
                true,
                keyboard::Modifiers::empty(),
            )],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let requests = peer.fence();
        let keys: Vec<_> = requests
            .iter()
            .filter_map(|r| match r {
                ClientRequest::Key { input, .. } => Some(input),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 1, "{requests:?}");
        assert_eq!(keys[0].action, KeyAction::Press);
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(field_id.clone()),
        );
        messages.clear();
        // The stock field captures key-up. The observer must still retire the
        // original transport lease, using the current release modifiers.
        let _ = ui.update(
            &[
                raw_letter(
                    keyboard::key::Code::KeyR,
                    "r",
                    false,
                    keyboard::Modifiers::CTRL,
                ),
                raw_letter(
                    keyboard::key::Code::KeyF,
                    "f",
                    true,
                    keyboard::Modifiers::CTRL,
                ),
                raw_letter(
                    keyboard::key::Code::KeyT,
                    "t",
                    true,
                    keyboard::Modifiers::CTRL,
                ),
                raw_letter(
                    keyboard::key::Code::KeyZ,
                    "z",
                    true,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                ),
                Event::InputMethod(input_method::Event::Commit("ime".into())),
            ],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let requests = peer.fence();
        let keys: Vec<_> = requests
            .iter()
            .filter_map(|r| match r {
                ClientRequest::Key { input, .. } => Some(input),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 1, "{requests:?}");
        assert_eq!(keys[0].action, KeyAction::Release);
        assert_eq!(keys[0].key, TerminalKey::Character('r'));
        assert!(keys[0].modifiers.control);
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, ClientRequest::Input { .. } | ClientRequest::Paste { .. })),
            "{requests:?}"
        );
        let edited: Vec<_> = messages
            .iter()
            .filter_map(|m| {
                if let Message::Edited(value) = m {
                    Some(value)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(edited.len(), 1, "{messages:?}");
        assert_eq!(edited[0].matches("ime").count(), 1);
        assert_eq!(edited[0].replacen("ime", "", 1), "retained");
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(terminal_id),
        );
        let _ = ui.update(
            &[
                raw_letter(
                    keyboard::key::Code::KeyR,
                    "r",
                    false,
                    keyboard::Modifiers::empty(),
                ),
                raw_letter(
                    keyboard::key::Code::KeyA,
                    "a",
                    true,
                    keyboard::Modifiers::empty(),
                ),
                raw_letter(
                    keyboard::key::Code::KeyA,
                    "a",
                    false,
                    keyboard::Modifiers::empty(),
                ),
                Event::InputMethod(input_method::Event::Commit("raw-ime".into())),
            ],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut Vec::new(),
        );
        let requests = peer.fence();
        let actions: Vec<_> = requests
            .iter()
            .filter_map(|r| match r {
                ClientRequest::Key { input, .. } => Some((input.key, input.action)),
                _ => None,
            })
            .collect();
        assert_eq!(
            actions,
            vec![
                (TerminalKey::Character('a'), KeyAction::Press),
                (TerminalKey::Character('a'), KeyAction::Release)
            ],
            "{requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, ClientRequest::Input { bytes, .. } if bytes == b"raw-ime")),
            "{requests:?}"
        );
    }
}

#[test]
fn native_window_event_overrides_stale_builder_only_until_the_next_widget_diff() {
    let peer = Peer::new();
    let renderer = renderer();
    let barrier = ReviewInputBarrier::default();
    let mut terminal = widget(&peer, "window-pane", barrier, true, false).window_focused(false);
    terminal.on_shortcut = None;
    let mut tree = Tree::new(&terminal as &dyn Widget<Message, Theme, Renderer>);
    native_focus(&mut terminal, &mut tree, &renderer);
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        Event::Window(window::Event::Focused),
    );
    for down in [true, false] {
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                down,
                keyboard::Modifiers::empty(),
            ),
        );
    }
    assert_eq!(
        peer.fence()
            .iter()
            .filter(|r| matches!(r, ClientRequest::Key { .. }))
            .count(),
        2
    );
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        Event::Window(window::Event::Unfocused),
    );
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        raw_letter(
            keyboard::key::Code::KeyB,
            "b",
            true,
            keyboard::Modifiers::empty(),
        ),
    );
    assert!(
        !peer
            .fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. }))
    );
    assert!(
        tree.state.downcast_ref::<WidgetState>().input_focused,
        "window blur preserves logical native focus"
    );
    // A parked tree can miss Window::Focused: a new authoritative view must
    // reset the per-native-batch false observation too.
    terminal.window_focused = true;
    tree.diff(&mut terminal as &mut dyn Widget<Message, Theme, Renderer>);
    for down in [true, false] {
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            raw_letter(
                keyboard::key::Code::KeyC,
                "c",
                down,
                keyboard::Modifiers::empty(),
            ),
        );
    }
    assert_eq!(
        peer.fence()
            .iter()
            .filter(|r| matches!(r, ClientRequest::Key { .. }))
            .count(),
        2
    );
    // Conversely an old in-batch true observation cannot defeat a newer
    // inactive builder after the tree was parked during blur.
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        Event::Window(window::Event::Focused),
    );
    terminal.window_focused = false;
    tree.diff(&mut terminal as &mut dyn Widget<Message, Theme, Renderer>);
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        raw_letter(
            keyboard::key::Code::KeyD,
            "d",
            true,
            keyboard::Modifiers::empty(),
        ),
    );
    assert!(
        !peer
            .fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. }))
    );
}

#[test]
fn unmounted_owned_key_waits_for_review_exit_and_releases_only_the_original_attachment() {
    let old = Peer::new();
    let new = Peer::new();
    let renderer = renderer();
    let barrier = ReviewInputBarrier::default();
    let mut terminal = widget(&old, "original-pane", barrier.clone(), true, false);
    terminal.on_shortcut = None;
    let mut tree = Tree::new(&terminal as &dyn Widget<Message, Theme, Renderer>);
    native_focus(&mut terminal, &mut tree, &renderer);
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        raw_letter(
            keyboard::key::Code::KeyA,
            "a",
            true,
            keyboard::Modifiers::empty(),
        ),
    );
    assert_eq!(old.fence().iter().filter(|r| matches!(r, ClientRequest::Key { input, .. } if input.action == KeyAction::Press)).count(), 1);
    barrier.note_review(old.handle().terminal_id(), true);
    drop(tree);
    assert!(
        !barrier.flush_owned_keys(),
        "Review suppresses original cleanup input"
    );
    assert!(
        !old.fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. }))
    );
    let up = raw_letter(
        keyboard::key::Code::KeyA,
        "a",
        false,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
    );
    let Event::Keyboard(up) = up else {
        unreachable!()
    };
    barrier.observe_key_event(&up);
    barrier.note_review(old.handle().terminal_id(), false);
    assert!(barrier.flush_owned_keys());
    let requests = old.fence();
    let keys: Vec<_> = requests
        .iter()
        .filter_map(|r| {
            if let ClientRequest::Key { input, .. } = r {
                Some(input)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(keys.len(), 1, "{requests:?}");
    assert_eq!(keys[0].key, TerminalKey::Character('a'));
    assert_eq!(keys[0].action, KeyAction::Release);
    assert!(keys[0].modifiers.control && keys[0].modifiers.shift);
    settle_cleanup(&barrier);
    assert!(
        !old.fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. }))
    );
    assert!(
        !new.fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. })),
        "cleanup cannot retarget another attachment"
    );
}

#[test]
fn observed_window_return_admits_first_fresh_down_when_old_physical_up_was_outside_the_app() {
    use crate::input_trace::{InputTrace, observe};
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    let peer = Peer::new();
    let mut renderer = renderer();
    let barrier = ReviewInputBarrier::default();
    let trace = InputTrace::default();
    trace.bind_terminal_keys(barrier.clone());
    let terminal_id = Id::unique();
    let mut terminal = widget(&peer, "return-pane", barrier, true, false);
    terminal.id = Some(terminal_id.clone());
    terminal.on_shortcut = None;
    let mut ui = UserInterface::build(
        observe(terminal, &trace),
        Size::new(640.0, 320.0),
        Cache::new(),
        &mut renderer,
    );
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(terminal_id),
    );
    let _ = ui.update(
        &[
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                true,
                keyboard::Modifiers::empty(),
            ),
            Event::Window(window::Event::Unfocused),
        ],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut Vec::new(),
    );
    let requests = peer.fence();
    let actions: Vec<_> = requests
        .iter()
        .filter_map(|r| {
            if let ClientRequest::Key { input, .. } = r {
                Some(input.action)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        actions,
        vec![KeyAction::Press, KeyAction::Release],
        "{requests:?}"
    );
    // A held repeat after reactivation must not manufacture a second raw down.
    let mut repeat = raw_letter(
        keyboard::key::Code::KeyA,
        "a",
        true,
        keyboard::Modifiers::empty(),
    );
    if let Event::Keyboard(keyboard::Event::KeyPressed { repeat, .. }) = &mut repeat {
        *repeat = true;
    }
    let _ = ui.update(
        &[Event::Window(window::Event::Focused), repeat],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut Vec::new(),
    );
    assert!(
        !peer
            .fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. }))
    );
    // No old up is delivered. This first actual new press must succeed.
    let _ = ui.update(
        &[
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                true,
                keyboard::Modifiers::empty(),
            ),
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                false,
                keyboard::Modifiers::empty(),
            ),
        ],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut Vec::new(),
    );
    let requests = peer.fence();
    let actions: Vec<_> = requests
        .iter()
        .filter_map(|r| {
            if let ClientRequest::Key { input, .. } = r {
                Some((input.key, input.action))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        actions,
        vec![
            (TerminalKey::Character('a'), KeyAction::Press),
            (TerminalKey::Character('a'), KeyAction::Release)
        ],
        "{requests:?}"
    );
}

#[test]
fn repeat_keeps_original_release_identity_and_window_blur_ends_review_before_cleanup() {
    let peer = Peer::new();
    let renderer = renderer();
    let barrier = ReviewInputBarrier::default();
    let mut terminal = widget(&peer, "review-window-pane", barrier.clone(), true, false);
    terminal.on_shortcut = None;
    let mut tree = Tree::new(&terminal as &dyn Widget<Message, Theme, Renderer>);
    native_focus(&mut terminal, &mut tree, &renderer);
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        raw_letter(
            keyboard::key::Code::KeyA,
            "a",
            true,
            keyboard::Modifiers::empty(),
        ),
    );
    let mut repeat = raw_letter(
        keyboard::key::Code::KeyA,
        "b",
        true,
        keyboard::Modifiers::SHIFT,
    );
    if let Event::Keyboard(keyboard::Event::KeyPressed { repeat, .. }) = &mut repeat {
        *repeat = true;
    }
    dispatch(&mut terminal, &mut tree, &renderer, repeat);
    let requests = peer.fence();
    let keys: Vec<_> = requests
        .iter()
        .filter_map(|r| {
            if let ClientRequest::Key { input, .. } = r {
                Some(input)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(keys.len(), 2, "{requests:?}");
    assert_eq!(keys[0].action, KeyAction::Press);
    assert_eq!(keys[0].unshifted_codepoint, Some('a'));
    assert_eq!(keys[1].action, KeyAction::Repeat);
    assert_eq!(
        keys[1].unshifted_codepoint,
        Some('b'),
        "repeat payload itself remains current"
    );
    let id = terminal.id.clone().unwrap();
    assert_eq!(
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            action(id, Action::Focus, None)
        ),
        vec![Message::Review(true)]
    );
    assert!(reviewing(&tree));
    let messages = dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        Event::Window(window::Event::Unfocused),
    );
    assert!(messages.contains(&Message::Review(false)), "{messages:?}");
    assert!(!reviewing(&tree));
    let requests = peer.fence();
    let keys: Vec<_> = requests
        .iter()
        .filter_map(|r| {
            if let ClientRequest::Key { input, .. } = r {
                Some(input)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(keys.len(), 1, "{requests:?}");
    assert_eq!(keys[0].key, TerminalKey::Character('a'));
    assert_eq!(keys[0].unshifted_codepoint, Some('a'));
    assert_eq!(keys[0].action, KeyAction::Release);
    assert!(keys[0].modifiers.shift);
    settle_cleanup(&barrier);
    assert!(
        !peer
            .fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. }))
    );
}

fn settle_cleanup(barrier: &ReviewInputBarrier) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while barrier.flush_owned_keys() {
        assert!(
            Instant::now() < deadline,
            "original transport cleanup did not settle"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn wait_peer_finished(peer: &Peer) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !peer.worker.as_ref().unwrap().is_finished() {
        assert!(
            Instant::now() < deadline,
            "original socket attachment did not retire"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn last_attachment_is_retained_until_actual_cleanup_response_and_unknown_completion_is_kept() {
    for accepted in [true, false] {
        let mut peer = Peer::new();
        peer.release_ready.store(false, Ordering::Release);
        peer.release_accept.store(accepted, Ordering::Release);
        let renderer = renderer();
        let barrier = ReviewInputBarrier::default();
        let mut terminal = widget(&peer, "last-owner", barrier.clone(), true, false);
        terminal.on_shortcut = None;
        let mut tree = Tree::new(&terminal as &dyn Widget<Message, Theme, Renderer>);
        native_focus(&mut terminal, &mut tree, &renderer);
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                true,
                keyboard::Modifiers::empty(),
            ),
        );
        assert!(peer.fence().iter().any(
            |r| matches!(r, ClientRequest::Key { input, .. } if input.action == KeyAction::Press)
        ));
        // The ledger now owns the LAST actual HostedTerminalHandle, not merely
        // a clone alongside the test peer or still-mounted TerminalWidget.
        drop(peer.handle.take());
        drop(tree);
        drop(terminal);
        assert!(barrier.flush_owned_keys());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let request = peer
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("original release must reach the real private socket");
            if matches!(request, ClientRequest::Key { input, .. } if input.action == KeyAction::Release)
            {
                break;
            }
        }
        assert!(
            !peer.worker.as_ref().unwrap().is_finished(),
            "pending cleanup retains its original transport through dispatch"
        );
        peer.release_ready.store(true, Ordering::Release);
        settle_cleanup(&barrier);
        if accepted {
            assert!(barrier.take_key_cleanup_failures().is_empty());
            wait_peer_finished(&peer);
        } else {
            assert!(
                !barrier.take_key_cleanup_failures().is_empty(),
                "unexpected response cannot certify cleanup"
            );
            assert!(
                !peer.worker.as_ref().unwrap().is_finished(),
                "unknown cleanup keeps the original attachment owned"
            );
            drop(barrier);
            wait_peer_finished(&peer);
        }
    }
}

#[test]
fn same_terminal_id_and_host_proof_after_real_reconnect_cannot_receive_old_cleanup() {
    let peer = Peer::new();
    let renderer = renderer();
    let barrier = ReviewInputBarrier::default();
    let mut terminal = widget(&peer, "reconnect-owner", barrier.clone(), true, false);
    terminal.on_shortcut = None;
    let mut tree = Tree::new(&terminal as &dyn Widget<Message, Theme, Renderer>);
    native_focus(&mut terminal, &mut tree, &renderer);
    let proof = peer.handle().proof().unwrap();
    let original_epoch = peer.handle().cleanup_epoch().unwrap();
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        raw_letter(
            keyboard::key::Code::KeyA,
            "a",
            true,
            keyboard::Modifiers::empty(),
        ),
    );
    assert!(peer.fence().iter().any(
        |r| matches!(r, ClientRequest::Key { input, .. } if input.action == KeyAction::Press)
    ));
    peer.reconnect.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(3);
    while peer
        .handle()
        .cleanup_epoch()
        .is_none_or(|epoch| epoch == original_epoch)
    {
        assert!(
            Instant::now() < deadline,
            "production client did not reconnect"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(peer.connections.load(Ordering::Acquire), 2);
    assert_eq!(
        peer.handle().proof(),
        Some(proof),
        "negative must retain the exact same advertised host proof and TerminalId"
    );
    drop(tree);
    drop(terminal);
    settle_cleanup(&barrier);
    assert!(!barrier.take_key_cleanup_failures().is_empty());
    assert!(
        !peer
            .fence()
            .iter()
            .any(|r| matches!(r, ClientRequest::Key { .. })),
        "old cleanup cannot be replayed into the new same-ID socket attachment"
    );
}

#[test]
fn fresh_owned_input_waits_for_all_prior_cleanup_responses_and_refuses_after_unknown() {
    for accepted in [true, false] {
        let peer = Peer::new();
        peer.release_ready.store(false, Ordering::Release);
        peer.release_accept.store(accepted, Ordering::Release);
        let renderer = renderer();
        let barrier = ReviewInputBarrier::default();
        let mut terminal = widget(&peer, "cleanup-fence", barrier.clone(), true, false);
        terminal.on_shortcut = None;
        let mut tree = Tree::new(&terminal as &dyn Widget<Message, Theme, Renderer>);
        native_focus(&mut terminal, &mut tree, &renderer);
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                true,
                keyboard::Modifiers::empty(),
            ),
        );
        assert!(peer.fence().iter().any(
            |r| matches!(r, ClientRequest::Key { input, .. } if input.action == KeyAction::Press)
        ));
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                false,
                keyboard::Modifiers::empty(),
            ),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let request = peer
                .requests
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("gated cleanup frame");
            if matches!(request, ClientRequest::Key { input, .. } if input.action == KeyAction::Release)
            {
                break;
            }
        }
        // This is genuinely accepted to the UI queue while the old up has no
        // response. Its frame may not overtake or presume that response.
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            raw_letter(
                keyboard::key::Code::KeyB,
                "b",
                true,
                keyboard::Modifiers::empty(),
            ),
        );
        assert!(barrier.owns_raw_key(
            keyboard::key::Physical::Code(keyboard::key::Code::KeyB),
            &terminal.handle
        ));
        peer.release_ready.store(true, Ordering::Release);
        let requests = peer.fence();
        let new_downs: Vec<_> = requests
            .iter()
            .filter_map(|r| {
                if let ClientRequest::Key { input, .. } = r {
                    Some(input)
                } else {
                    None
                }
            })
            .collect();
        if accepted {
            assert_eq!(new_downs.len(), 1, "{requests:?}");
            assert_eq!(new_downs[0].key, TerminalKey::Character('b'));
            assert_eq!(new_downs[0].action, KeyAction::Press);
        } else {
            assert!(
                new_downs.is_empty(),
                "failed cleanup cannot authorize a fresh raw down: {requests:?}"
            );
        }
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            raw_letter(
                keyboard::key::Code::KeyB,
                "b",
                false,
                keyboard::Modifiers::empty(),
            ),
        );
        let requests = peer.fence();
        let new_ups: Vec<_> = requests
            .iter()
            .filter_map(|r| {
                if let ClientRequest::Key { input, .. } = r {
                    Some(input)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(new_ups.len(), usize::from(accepted), "{requests:?}");
        if accepted {
            assert_eq!(new_ups[0].action, KeyAction::Release);
        }
        settle_cleanup(&barrier);
        assert_eq!(barrier.take_key_cleanup_failures().is_empty(), accepted);
    }
}

#[test]
fn inactive_native_terminal_click_admits_the_first_key_and_ime_before_app_selection_diff() {
    use crate::input_trace::{InputTrace, KeyboardRegion, observe_keyboard_events};
    use crate::workspace_widget::FocusedPane;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    use std::{cell::RefCell, rc::Rc};
    let peer = Peer::new();
    let mut renderer = renderer();
    let trace = InputTrace::default();
    let barrier = ReviewInputBarrier::default();
    trace.bind_terminal_keys(barrier.clone());
    let id = Id::unique();
    let card = crate::model::CardId::new();
    let samples = Rc::new(RefCell::new(Vec::new()));
    let make_view = || {
        let mut terminal = widget(&peer, "inactive-native", barrier.clone(), false, false);
        terminal.id = Some(id.clone());
        terminal.on_shortcut = None;
        let samples = samples.clone();
        observe_keyboard_events(
            terminal,
            &trace,
            vec![(
                id.clone(),
                KeyboardRegion {
                    pane: FocusedPane::Terminal,
                    card: Some(card),
                },
            )],
            move |event, _, region, widget| {
                if matches!(event, keyboard::Event::KeyPressed { .. }) {
                    samples.borrow_mut().push((region, widget));
                }
                Message::Shortcut
            },
        )
    };
    let mut ui = UserInterface::build(
        make_view(),
        Size::new(640.0, 320.0),
        Cache::new(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    let _ = ui.update(
        &[
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                true,
                keyboard::Modifiers::empty(),
            ),
            raw_letter(
                keyboard::key::Code::KeyA,
                "a",
                false,
                keyboard::Modifiers::empty(),
            ),
            Event::InputMethod(input_method::Event::Commit("native-ime".into())),
        ],
        mouse::Cursor::Available(Point::new(80.0, 80.0)),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        messages.contains(&Message::Focus),
        "the real click queued the App focus message, which remains deliberately unapplied"
    );
    assert_eq!(
        samples.borrow().as_slice(),
        &[(
            KeyboardRegion {
                pane: FocusedPane::Terminal,
                card: Some(card)
            },
            Some(id.clone())
        )]
    );
    let requests = peer.fence();
    let keys: Vec<_> = requests
        .iter()
        .filter_map(|request| match request {
            ClientRequest::Key { input, .. } => Some((input.key, input.action)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        [
            (TerminalKey::Character('a'), KeyAction::Press),
            (TerminalKey::Character('a'), KeyAction::Release)
        ],
        "{requests:?}"
    );
    assert!(requests.iter().any(|request| matches!(request, ClientRequest::Input { bytes, .. } if bytes == b"native-ime")), "{requests:?}");
    let cache = ui.into_cache();
    samples.borrow_mut().clear();
    // A subsequent diff saying this pane is still inactive supersedes the
    // observation, including a parked tree remount after another pane won.
    let mut ui = UserInterface::build(make_view(), Size::new(640.0, 320.0), cache, &mut renderer);
    let _ = ui.update(
        &[
            raw_letter(
                keyboard::key::Code::KeyB,
                "b",
                true,
                keyboard::Modifiers::empty(),
            ),
            Event::InputMethod(input_method::Event::Commit("must-not-send".into())),
        ],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut Vec::new(),
    );
    let requests = peer.fence();
    assert!(
        !requests.iter().any(|request| matches!(
            request,
            ClientRequest::Key { .. } | ClientRequest::Input { .. }
        )),
        "stale click authority survived a fresh inactive diff: {requests:?}"
    );
    assert_eq!(samples.borrow()[0].0.card, None);
}

#[test]
fn inactive_native_terminal_review_survives_a11y_publication_before_the_app_focus_diff() {
    let peer = Peer::new();
    let renderer = renderer();
    let barrier = ReviewInputBarrier::default();
    let mut terminal = widget(&peer, "inactive-review", barrier.clone(), false, false);
    let mut tree = Tree::new(&terminal as &dyn Widget<Message, Theme, Renderer>);
    let layout = layout::Node::new(Size::new(640.0, 320.0));

    // The actual click activates the mounted pane. Deliberately leave the App
    // focus callback unapplied, so the builder still describes an inactive pane.
    let messages = dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        Event::Mouse(mouse::Event::ButtonPressed(IcedMouseButton::Left)),
    );
    assert!(messages.contains(&Message::Focus));
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        Event::Mouse(mouse::Event::ButtonReleased(IcedMouseButton::Left)),
    );
    native_focus(&mut terminal, &mut tree, &renderer);
    assert!(tree.state.downcast_ref::<WidgetState>().input_focused);
    assert!(!terminal.model_focused);
    let published = terminal.a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable);
    let id = published.root()[0].id().clone();
    assert_eq!(
        dispatch(
            &mut terminal,
            &mut tree,
            &renderer,
            action(id, Action::Focus, None),
        ),
        vec![Message::Review(true)]
    );
    dispatch(
        &mut terminal,
        &mut tree,
        &renderer,
        key(
            keyboard::key::Named::ArrowRight,
            keyboard::key::Code::ArrowRight,
            true,
        ),
    );
    let selection = tree
        .state
        .downcast_ref::<WidgetState>()
        .a11y
        .borrow()
        .selection
        .clone()
        .expect("native Review selection");
    assert!(reviewing(&tree));
    peer.fence();

    // Native accessibility publication can occur before the queued App update.
    // It must use the same observed owner as update(), preserving Review state.
    let published = terminal.a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable);
    assert!(reviewing(&tree));
    assert_eq!(
        tree.state
            .downcast_ref::<WidgetState>()
            .a11y
            .borrow()
            .selection,
        Some(selection)
    );
    assert!(
        published.root()[0]
            .node()
            .description()
            .unwrap()
            .contains("Review mode")
    );
    assert!(
        !published.root()[0]
            .node()
            .label()
            .unwrap()
            .contains("not focused")
    );
    assert_no_pty_input(&peer.fence());

    // An actual subsequent inactive App diff supersedes the native observation.
    terminal = widget(&peer, "inactive-review", barrier, false, false);
    tree.diff(&mut terminal as &mut dyn Widget<Message, Theme, Renderer>);
    let published = terminal.a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable);
    assert!(!reviewing(&tree));
    assert!(
        tree.state
            .downcast_ref::<WidgetState>()
            .a11y
            .borrow()
            .selection
            .is_none()
    );
    assert!(
        published.root()[0]
            .node()
            .description()
            .unwrap()
            .contains("Terminal input")
    );
    assert_no_pty_input(&peer.fence());
}
