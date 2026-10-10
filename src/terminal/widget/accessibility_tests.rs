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
struct Peer {
    _directory: tempfile::TempDir,
    handle: HostedTerminalHandle,
    requests: crossbeam_channel::Receiver<ClientRequest>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Peer {
    fn new() -> Self {
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
        .review_boundary(barrier, suppressed, Message::Review)
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
