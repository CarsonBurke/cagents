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
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

#[derive(Clone, Debug, PartialEq)]
enum Message {
    Review(bool),
    Focus,
    Shortcut,
    Paste,
}
pub(crate) struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    requests: crossbeam_channel::Receiver<ClientRequest>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Peer {
    pub(crate) fn new() -> Self {
        Self::with_generation(TerminalId::new(),uuid::Uuid::new_v4(),HostSessionProof {
            host_session_id: HostSessionId::new("review-test-session"),
            boot_id: BootId::new("review-test-boot"),
            host_incarnation: HostIncarnation::new("review-test-host"),
        })
    }
    pub(crate) fn surface(&self) -> TerminalSurface { self.handle.clone().into() }
    pub(crate) fn with_generation(id:TerminalId,worker:uuid::Uuid,proof:HostSessionProof) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("review.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut descriptor = TerminalDescriptor::relaunchable(
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
        descriptor.execution_instance=worker;
        descriptor.session.continuity=crate::model::TerminalContinuity::Live {
            host_session_id:proof.host_session_id.clone(),boot_id:proof.boot_id.clone(),host_incarnation:proof.host_incarnation.clone(),
        };
        let handle =
            HostedTerminalHandle::connect_at(socket, id, Some(proof.clone()), |_| {}).unwrap();
        let (tx, requests) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
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
            let mut connected = false;
            while !stopping.load(Ordering::Relaxed) {
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
                    tx.send(request.request).unwrap();
                    ServerResponse::Accepted
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
        });
        handle.wait_until_attached(Duration::from_secs(3)).unwrap();
        Self {
            _directory: directory,
            handle,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn fence(&self) -> Vec<ClientRequest> {
        let marker = b"accessibility-test-fence".to_vec();
        self.handle.input(marker.clone()).unwrap();
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
        let _ = self.handle.shutdown();
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
    let mut widget = TerminalWidget::new(peer.handle.clone())
        .id(Id::new(name.to_owned()))
        .model_focused(focused)
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
fn widget_tree(widget: &TerminalWidget<Message>) -> Tree {
    let mut tree = Tree::new(widget as &dyn Widget<Message, Theme, Renderer>);
    // These direct-widget fixtures model the native focus handoff performed by
    // the application after selecting a pane. Model selection alone is not it.
    if widget.model_focused { operation::Focusable::focus(tree.state.downcast_mut::<WidgetState>()); }
    tree
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
    let mut old_tree = widget_tree(&old_widget);
    let mut new_tree = widget_tree(&new_widget);
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
    operation::Focusable::focus(old_tree.state.downcast_mut::<WidgetState>());
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
    let mut tree = widget_tree(&widget);
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
    let mut tree = widget_tree(&widget);
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
    let mut tree = widget_tree(&widget);
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

#[test]
fn review_bar_activation_changes_geometry_without_removing_accessible_action() {
    let peer = Peer::new();
    let mut widget = widget(
        &peer,
        "review-bar",
        ReviewInputBarrier::default(),
        true,
        false,
    );
    let tree = widget_tree(&widget);
    let bounds = Rectangle {
        x: 15.0,
        y: 20.0,
        width: 300.0,
        height: 200.0,
    };
    assert_eq!(widget.grid_bounds(bounds), bounds);
    let layout = layout::Node::new(bounds.size());
    for active in [false, true, false] {
        widget.assistive_technology_active = active;
        assert_eq!(
            widget.grid_bounds(bounds).height,
            bounds.height - if active { REVIEW_BAR_HEIGHT } else { 0.0 }
        );
        let nodes = widget.a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable);
        assert!(nodes.children().iter().any(|node| node.node().role()
            == iced_accessibility::accesskit::Role::Button
            && node.node().supports_action(Action::Click)));
    }
    widget.always_show_review_bar = true;
    assert_eq!(
        widget.grid_bounds(bounds).height,
        bounds.height - REVIEW_BAR_HEIGHT
    );
    widget.always_show_review_bar = false;
    widget.review_active = true;
    assert_eq!(
        widget.grid_bounds(bounds).height,
        bounds.height - REVIEW_BAR_HEIGHT
    );
}

#[test]
fn configured_review_shortcut_toggles_both_ways_without_reaching_the_pty() {
    let peer = Peer::new();
    let renderer = renderer();
    let mut widget = widget(
        &peer,
        "review-shortcut",
        ReviewInputBarrier::default(),
        true,
        false,
    )
    .review_shortcuts(&["Ctrl+Alt+Shift+R".into()]);
    let mut tree = widget_tree(&widget);
    let press = || {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("r".into()),
            modified_key: keyboard::Key::Character("R".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyR),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL
                | keyboard::Modifiers::ALT
                | keyboard::Modifiers::SHIFT,
            text: Some("R".into()),
            repeat: false,
        })
    };
    let release = || {
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: keyboard::Key::Character("r".into()),
            modified_key: keyboard::Key::Character("R".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyR),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL
                | keyboard::Modifiers::ALT
                | keyboard::Modifiers::SHIFT,
        })
    };
    assert!(dispatch(&mut widget, &mut tree, &renderer, press()).contains(&Message::Review(true)));
    assert!(reviewing(&tree));
    dispatch(&mut widget, &mut tree, &renderer, release());
    assert!(dispatch(&mut widget, &mut tree, &renderer, press()).contains(&Message::Review(false)));
    assert!(!reviewing(&tree));
    dispatch(&mut widget, &mut tree, &renderer, release());
    assert_no_pty_input(&peer.fence());
}


#[test]
fn actual_sidebar_text_focus_owns_ignored_control_keys_and_terminal_pairs_held_releases() {
    use cosmic::iced::widget::{row, text_input};
    let peer = Peer::new();
    let renderer = renderer();
    let terminal_id = Id::new("focus-terminal");
    let field_id = Id::new("sidebar-rename");
    let mut terminal = widget(&peer, "focus-terminal", ReviewInputBarrier::default(), true, false)
        .review_shortcuts(&["Ctrl+F".into()]);
    terminal.on_shortcut = None;
    let mut root: Element<'_, Message> = row![
        text_input("Rename", "").id(field_id.clone()).on_input(|_| Message::Focus).width(180),
        terminal
    ].into();
    let mut tree = Tree::new(&root);
    let layout = root.as_widget_mut().layout(&mut tree, &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(800.0, 320.0)));
    let focus = |root: &mut Element<'_, Message>, tree: &mut Tree, id: Id| {
        let mut operation = operation::focusable::focus::<()>(id);
        root.as_widget_mut().operate(tree, Layout::new(&layout), &renderer, &mut operation);
    };
    let dispatch = |root: &mut Element<'_, Message>, tree: &mut Tree, event: Event| {
        let mut messages = Vec::new();
        root.as_widget_mut().update(tree, &event, Layout::new(&layout), mouse::Cursor::Unavailable,
            &renderer, &mut clipboard::Null, &mut Shell::new(&mut messages), &Rectangle::with_size(Size::new(800.0, 320.0)));
        messages
    };
    let control = |value: &str, code, down: bool| Event::Keyboard(if down {
        keyboard::Event::KeyPressed { key: keyboard::Key::Character(value.into()), modified_key: keyboard::Key::Character(value.into()),
            physical_key: keyboard::key::Physical::Code(code), location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL, text: None, repeat: false }
    } else {
        keyboard::Event::KeyReleased { key: keyboard::Key::Character(value.into()), modified_key: keyboard::Key::Character(value.into()),
            physical_key: keyboard::key::Physical::Code(code), location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL }
    });
    // Actual native focus enables a freshly mounted terminal.
    focus(&mut root, &mut tree, terminal_id.clone());
    dispatch(&mut root, &mut tree, control("t", keyboard::key::Code::KeyT, true));
    let admitted = peer.fence();
    assert_eq!(admitted.iter().filter(|request| matches!(request, ClientRequest::Key { input, .. } if input.action == KeyAction::Press)).count(), 1);
    // A programmatic sidebar handoff must release that held key even if the
    // parent withholds future raw keyboard events from the terminal subtree.
    focus(&mut root, &mut tree, field_id);
    dispatch(&mut root, &mut tree, Event::Window(window::Event::RedrawRequested(Instant::now())));
    let blurred = peer.fence();
    assert_eq!(blurred.iter().filter(|request| matches!(request, ClientRequest::Key { input, .. } if input.action == KeyAction::Release)).count(), 1);
    for (value, code) in [("f", keyboard::key::Code::KeyF), ("t", keyboard::key::Code::KeyT), ("y", keyboard::key::Code::KeyY)] {
        let messages = dispatch(&mut root, &mut tree, control(value, code, true));
        assert!(!messages.iter().any(|message| matches!(message, Message::Review(_))), "review chord cannot escape sidebar focus");
        dispatch(&mut root, &mut tree, control(value, code, false));
    }
    dispatch(&mut root, &mut tree, Event::InputMethod(input_method::Event::Commit("sidebar composition".into())));
    assert!(!peer.fence().iter().any(|request| matches!(request, ClientRequest::Key { .. } | ClientRequest::Input { .. } | ClientRequest::Paste { .. })));
    focus(&mut root, &mut tree, terminal_id);
    dispatch(&mut root, &mut tree, control("t", keyboard::key::Code::KeyT, true));
    dispatch(&mut root, &mut tree, control("t", keyboard::key::Code::KeyT, false));
    let resumed = peer.fence();
    assert_eq!(resumed.iter().filter(|request| matches!(request, ClientRequest::Key { input, .. } if input.action == KeyAction::Press)).count(), 1);
    assert_eq!(resumed.iter().filter(|request| matches!(request, ClientRequest::Key { input, .. } if input.action == KeyAction::Release)).count(), 1);
    // Native window activation preserves the selected widget, while the
    // pointer outside the terminal explicitly transfers native focus.
    dispatch(&mut root, &mut tree, Event::Window(window::Event::Unfocused));
    dispatch(&mut root, &mut tree, Event::Window(window::Event::Focused));
    dispatch(&mut root, &mut tree, control("t", keyboard::key::Code::KeyT, true));
    dispatch(&mut root, &mut tree, control("t", keyboard::key::Code::KeyT, false));
    assert_eq!(peer.fence().iter().filter(|request| matches!(request, ClientRequest::Key { .. })).count(), 2);
    let mut messages = Vec::new();
    root.as_widget_mut().update(&mut tree, &Event::Mouse(mouse::Event::ButtonPressed(IcedMouseButton::Left)),
        Layout::new(&layout), mouse::Cursor::Available(Point::new(30.0, 10.0)), &renderer,
        &mut clipboard::Null, &mut Shell::new(&mut messages), &Rectangle::with_size(Size::new(800.0, 320.0)));
    dispatch(&mut root, &mut tree, control("t", keyboard::key::Code::KeyT, true));
    dispatch(&mut root, &mut tree, control("t", keyboard::key::Code::KeyT, false));
    assert!(!peer.fence().iter().any(|request| matches!(request, ClientRequest::Key { .. })));
}


#[test]
fn key_released_during_review_pairs_after_exit_without_requiring_a_blur() {
    let peer = Peer::new();
    let renderer = renderer();
    let mut widget = widget(&peer, "held-review-key", ReviewInputBarrier::default(), true, false);
    widget.on_shortcut = None;
    let mut tree = widget_tree(&widget);
    dispatch(&mut widget, &mut tree, &renderer,
        key(keyboard::key::Named::Enter, keyboard::key::Code::Enter, true));
    assert_eq!(peer.fence().iter().filter(|request| matches!(request,
        ClientRequest::Key { input, .. } if input.action == KeyAction::Press)).count(), 1);
    let id = widget.id.clone().unwrap();
    dispatch(&mut widget, &mut tree, &renderer, action(id, Action::Focus, None));
    widget.review_active = true;
    widget.suppress_focus_reports = true;
    widget.review_revision = 1;
    dispatch(&mut widget, &mut tree, &renderer,
        key(keyboard::key::Named::Enter, keyboard::key::Code::Enter, false));
    assert_no_pty_input(&peer.fence());
    // This is the app's confirmed Review-off view; native keyboard focus stays
    // on the terminal, so an ordinary blur alone cannot repair the release.
    widget.review_active = false;
    widget.suppress_focus_reports = false;
    widget.review_revision = 2;
    dispatch(&mut widget, &mut tree, &renderer, Event::Window(window::Event::RedrawRequested(Instant::now())));
    assert_eq!(peer.fence().iter().filter(|request| matches!(request,
        ClientRequest::Key { input, .. } if input.action == KeyAction::Release)).count(), 1);
    dispatch(&mut widget, &mut tree, &renderer, Event::Window(window::Event::RedrawRequested(Instant::now())));
    assert!(!peer.fence().iter().any(|request| matches!(request, ClientRequest::Key { .. })));
}


#[test]
fn unmounted_pressed_keys_remain_owned_across_review_and_replacement_widgets() {
    let original=Peer::new(); let replacement=Peer::new(); let renderer=renderer();
    let shared=ReviewInputBarrier::default();
    let mut old=widget(&original,"same-card",shared.clone(),true,false);
    old.on_shortcut=None;
    let mut old_tree=widget_tree(&old);
    dispatch(&mut old,&mut old_tree,&renderer,key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,true));
    assert_eq!(original.fence().iter().filter(|request| matches!(request,ClientRequest::Key { input,.. } if input.action==KeyAction::Press)).count(),1);
    drop(old_tree); drop(old);
    assert!(!original.fence().iter().any(|request| matches!(request,ClientRequest::Key { .. })),"tree Drop must not emit PTY input");
    shared.flush_orphans(true);
    let release=key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,false);
    let Event::Keyboard(released)=&release else { unreachable!() };
    shared.observe_key_event(released);
    shared.flush_orphans(true);
    assert!(!original.fence().iter().any(|request| matches!(request,ClientRequest::Key { .. })),"Review keeps orphan cleanup deferred");
    let mut current=widget(&replacement,"same-card",shared.clone(),true,true);
    current.on_shortcut=None;
    let mut current_tree=widget_tree(&current);
    dispatch(&mut current,&mut current_tree,&renderer,Event::Window(window::Event::RedrawRequested(Instant::now())));
    assert!(!original.fence().iter().any(|request| matches!(request,ClientRequest::Key { .. })));
    current.suppress_focus_reports=false; current.review_active=false; current.review_revision=1;
    dispatch(&mut current,&mut current_tree,&renderer,Event::Window(window::Event::RedrawRequested(Instant::now())));
    assert_eq!(original.fence().iter().filter(|request| matches!(request,ClientRequest::Key { input,.. } if input.action==KeyAction::Release)).count(),1);
    dispatch(&mut current,&mut current_tree,&renderer,release);
    assert!(!replacement.fence().iter().any(|request| matches!(request,ClientRequest::Key { .. })),"an old physical release cannot target a replacement attachment");
    shared.flush_orphans(false);
    assert!(!original.fence().iter().any(|request| matches!(request,ClientRequest::Key { .. })),"cleanup is admitted once");
}

#[test]
fn window_activation_property_survives_a_real_inactive_window_diff() {
    let peer=Peer::new(); let renderer=renderer();
    let mut widget=widget(&peer,"window-focus",ReviewInputBarrier::default(),true,false);
    widget.on_shortcut=None;
    let mut tree=widget_tree(&widget);
    dispatch(&mut widget,&mut tree,&renderer,Event::Window(window::Event::Unfocused));
    widget.window_focused=false;
    widget.diff(&mut tree);
    dispatch(&mut widget,&mut tree,&renderer,Event::Window(window::Event::RedrawRequested(Instant::now())));
    assert!(operation::Focusable::is_focused(tree.state.downcast_ref::<WidgetState>()),"window activation must not erase native widget focus");
    dispatch(&mut widget,&mut tree,&renderer,key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,true));
    assert!(!peer.fence().iter().any(|request| matches!(request,ClientRequest::Key { .. })));
    widget.window_focused=true; widget.diff(&mut tree);
    dispatch(&mut widget,&mut tree,&renderer,Event::Window(window::Event::Focused));
    dispatch(&mut widget,&mut tree,&renderer,key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,true));
    dispatch(&mut widget,&mut tree,&renderer,key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,false));
    assert_eq!(peer.fence().iter().filter(|request| matches!(request,ClientRequest::Key { .. })).count(),2);
}

#[test]
fn window_key_ownership_has_a_fixed_admission_budget() {
    let peer=Peer::new(); let surface=TerminalSurface::from(peer.handle.clone());
    let barrier=ReviewInputBarrier::default();
    let input=key_press(&keyboard::Key::Named(keyboard::key::Named::Enter),&keyboard::Key::Named(keyboard::key::Named::Enter),
        keyboard::key::Physical::Code(keyboard::key::Code::Enter),keyboard::Modifiers::empty(),None,false);
    for code in 0..512 {
        let physical=keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Xkb(code));
        barrier.press(physical,&surface,input.clone()).unwrap();
        // Drain each receipt so this measures the window's 512 held leases,
        // independently of the transport's 256 in-flight admission budget.
        peer.fence();
    }
    let extra=keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Xkb(513));
    assert_eq!(barrier.press(extra,&surface,input.clone()),Err(TerminalSendError::Full));
    let first=keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Xkb(0));
    let mut release=input.clone();release.action=KeyAction::Release;release.text=None;
    barrier.release(first,&surface,release).unwrap();
    // Completion must actually be observed before its bounded slot is reusable.
    peer.fence();
    barrier.press(extra,&surface,input).unwrap();
}


#[test]
fn native_release_observation_cannot_clear_a_newer_review_press_from_an_old_app_message() {
    let peer=Peer::new(); let renderer=renderer(); let shared=ReviewInputBarrier::default();
    let trace=crate::input_trace::InputTrace::default(); trace.bind_terminal_keys(shared.clone());
    let mut old=widget(&peer,"batched-old",shared.clone(),true,false); old.on_shortcut=None;
    let mut old_tree=widget_tree(&old);
    dispatch(&mut old,&mut old_tree,&renderer,key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,true));
    peer.fence(); drop(old_tree); drop(old);
    let release=key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,false);
    trace.record(&release);
    // A later native press can run before the queued message for the release.
    let mut current=widget(&peer,"batched-new",shared.clone(),true,true);
    let mut tree=widget_tree(&current);
    let press=key(keyboard::key::Named::Enter,keyboard::key::Code::Enter,true);
    trace.record(&press);
    dispatch(&mut current,&mut tree,&renderer,press.clone());
    assert!(shared.owns_keyboard(&press));
    // App update only attempts deferred cleanup. It never re-observes an old
    // physical release after the newer native press has already been seen.
    shared.flush_orphans(true);
    assert!(shared.owns_keyboard(&press));
    assert_no_pty_input(&peer.fence());
}


/// A real framed peer whose key ACKs are controlled by the test. It owns no
/// client handle, so last-owner tests cannot accidentally pass because a
/// fixture retained an extra attachment.
struct HeldKeyPeer {
    _directory: tempfile::TempDir,
    keys: crossbeam_channel::Receiver<KeyInput>,
    acknowledge: crossbeam_channel::Sender<bool>,
    closed: crossbeam_channel::Receiver<()>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl HeldKeyPeer {
    fn connect() -> (TerminalSurface, Self) {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = directory.path().join("held-key.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let id = TerminalId::new();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("held-key-session"),
            boot_id: BootId::new("held-key-boot"),
            host_incarnation: HostIncarnation::new("held-key-host"),
        };
        let handle = HostedTerminalHandle::connect_at(socket, id, Some(proof.clone()), |_| {}).unwrap();
        let (keys_tx, keys) = crossbeam_channel::unbounded();
        let (acknowledge, acks) = crossbeam_channel::bounded(8);
        let (closed_tx, closed) = crossbeam_channel::bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            while !stopping.load(Ordering::Relaxed) {
                let envelope: ClientEnvelope = match codec.read(&mut stream) {
                    Ok(Some(value)) => value,
                    Ok(None) => { let _ = closed_tx.send(()); break; }
                    Err(FrameError::Io(error)) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                    Err(error) => panic!("held-key frame: {error}"),
                };
                let message = match envelope.request {
                    ClientRequest::Connect { .. } => {
                        let mut descriptor = TerminalDescriptor::relaunchable(id, LaunchRecipe {
                            program: "/bin/sh".into(), arguments: Vec::new(), working_directory: "/".into(),
                            environment: Default::default(), unset_environment: Default::default(),
                        }, None);
                        descriptor.execution_instance = uuid::Uuid::new_v4();
                        descriptor.attached = true;
                        descriptor.session.continuity = crate::model::TerminalContinuity::Live {
                            host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone(),
                        };
                        ServerMessage::Response { response: ServerResponse::Connected { proof: proof.clone(), resumed: vec![descriptor] } }
                    }
                    ClientRequest::Key { input, .. } => {
                        keys_tx.send(input).unwrap();
                        let accepted = loop {
                            match acks.recv_timeout(Duration::from_millis(50)) {
                                Ok(accepted) => break accepted,
                                Err(_) if stopping.load(Ordering::Relaxed) => return,
                                Err(_) => continue,
                            }
                        };
                        if accepted { ServerMessage::Response { response: ServerResponse::Accepted } }
                        else { ServerMessage::Error(crate::terminal::ipc::ProtocolError::new(crate::terminal::ipc::ProtocolErrorCode::QueueFull, "held-key fixture refusal", true)) }
                    }
                    _ => ServerMessage::Response { response: ServerResponse::Accepted },
                };
                if write_frame(&mut stream, &ServerEnvelope { version: PROTOCOL_VERSION, request_id: Some(envelope.request_id), proof: proof.clone(), message }).is_err() { break; }
            }
        });
        handle.wait_until_attached(Duration::from_secs(3)).unwrap();
        (handle.into(), Self { _directory: directory, keys, acknowledge, closed, stop, worker: Some(worker) })
    }
    fn next(&self, action: KeyAction) {
        assert_eq!(self.keys.recv_timeout(Duration::from_secs(3)).unwrap().action, action);
    }
    fn ack(&self, accepted: bool) { self.acknowledge.send(accepted).unwrap(); }
}
impl Drop for HeldKeyPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() { worker.join().unwrap(); }
    }
}
fn held_input(action: KeyAction) -> KeyInput {
    let mut input = key_press(&keyboard::Key::Character("a".into()), &keyboard::Key::Character("a".into()),
        keyboard::key::Physical::Code(keyboard::key::Code::KeyA), keyboard::Modifiers::empty(), Some("a"), false);
    input.action = action;
    if action == KeyAction::Release { input.text = None; }
    input
}
fn wait_key_counts(barrier: &ReviewInputBarrier, expected: (usize, usize)) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while barrier.pending_counts() != expected {
        assert!(Instant::now() < deadline, "key ownership did not reach {expected:?}");
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn key_owner_keeps_the_last_surface_until_release_is_acknowledged() {
    let (surface, peer) = HeldKeyPeer::connect();
    let barrier = ReviewInputBarrier::default();
    let physical = keyboard::key::Physical::Code(keyboard::key::Code::KeyA);
    barrier.press(physical, &surface, held_input(KeyAction::Press)).unwrap();
    peer.next(KeyAction::Press); peer.ack(true);
    barrier.unmounted(&surface);
    drop(surface); // Only the window owner now retains this exact attachment.
    barrier.flush_orphans(true);
    assert!(peer.keys.try_recv().is_err(), "Review cannot emit orphan input");
    barrier.flush_orphans(false);
    peer.next(KeyAction::Release);
    assert_eq!(barrier.pending_counts(), (0, 1));
    assert!(peer.closed.try_recv().is_err(), "local queue admission cannot retire the last surface");
    peer.ack(true);
    wait_key_counts(&barrier, (0, 0));
    peer.closed.recv_timeout(Duration::from_secs(3)).expect("final actor retires after exact worker ACK");
    assert!(barrier.take_failures().is_empty());
}
#[test]
fn pending_release_allows_a_new_press_and_keeps_original_fifo() {
    let (surface, peer) = HeldKeyPeer::connect();
    let barrier = ReviewInputBarrier::default();
    let physical = keyboard::key::Physical::Code(keyboard::key::Code::KeyA);
    barrier.press(physical, &surface, held_input(KeyAction::Press)).unwrap();
    barrier.release(physical, &surface, held_input(KeyAction::Release)).unwrap();
    barrier.press(physical, &surface, held_input(KeyAction::Press)).unwrap();
    assert_eq!(barrier.pending_counts(), (1, 1));
    peer.next(KeyAction::Press); peer.ack(true);
    peer.next(KeyAction::Release); peer.ack(true);
    peer.next(KeyAction::Press); peer.ack(true);
    wait_key_counts(&barrier, (1, 0));
    barrier.release(physical, &surface, held_input(KeyAction::Release)).unwrap();
    peer.next(KeyAction::Release); peer.ack(true);
    wait_key_counts(&barrier, (0, 0));
    assert!(barrier.take_failures().is_empty());
}
#[test]
fn refused_repeat_does_not_erase_original_press_authority() {
    let (surface, peer) = HeldKeyPeer::connect();
    let barrier = ReviewInputBarrier::default();
    let physical = keyboard::key::Physical::Code(keyboard::key::Code::KeyA);
    barrier.press(physical, &surface, held_input(KeyAction::Press)).unwrap();
    barrier.press(physical, &surface, held_input(KeyAction::Repeat)).unwrap();
    barrier.release(physical, &surface, held_input(KeyAction::Release)).unwrap();
    peer.next(KeyAction::Press); peer.ack(true);
    peer.next(KeyAction::Repeat); peer.ack(false);
    peer.next(KeyAction::Release); peer.ack(true);
    wait_key_counts(&barrier, (0, 0));
    assert!(barrier.take_failures().iter().any(|reason| reason.contains("fixture refusal")));
}
#[test]
fn refused_press_skips_its_release_and_reports_failure() {
    let (surface, peer) = HeldKeyPeer::connect();
    let barrier = ReviewInputBarrier::default();
    let physical = keyboard::key::Physical::Code(keyboard::key::Code::KeyA);
    barrier.press(physical, &surface, held_input(KeyAction::Press)).unwrap();
    barrier.release(physical, &surface, held_input(KeyAction::Release)).unwrap();
    barrier.press(physical, &surface, held_input(KeyAction::Press)).unwrap();
    peer.next(KeyAction::Press); peer.ack(false);
    peer.next(KeyAction::Press); peer.ack(true);
    wait_key_counts(&barrier, (1, 0));
    assert!(barrier.take_failures().iter().any(|reason| reason.contains("paired press")));
    barrier.release(physical, &surface, held_input(KeyAction::Release)).unwrap();
    peer.next(KeyAction::Release); peer.ack(true);
    wait_key_counts(&barrier, (0, 0));
}
