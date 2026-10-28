use super::*;
use cosmic::iced::{
    Point,
    advanced::{Layout, Shell, clipboard, layout, widget::Tree},
    futures::StreamExt,
};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;

async fn drain_app_tasks(app: &mut Cagents, task: Task<Message>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut tasks = VecDeque::from([task]);
        while let Some(task) = tasks.pop_front() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                        tasks.push_back(app.update(message));
                    }
                }
            }
        }
    })
    .await
    .expect("local fixture tasks must settle without backend work");
}

/// Isolate globals and paths without clearing or replacing caller authority.
/// A real App init, its subscriptions and mounted view must not even attempt
/// a refused service call. The socket only rejects; it supplies no identity,
/// protocol response, proof, provider or successful backend operation.
#[test]
fn actual_app_fixture_mounts_and_interacts_without_service_attempts() {
    const CHILD: &str = "CAGENTS_NATIVE_ISOLATION_TEST_CHILD";
    const EXPECTED_AUTHORITY: &str = "CAGENTS_NATIVE_ISOLATION_EXPECT_AUTHORITY";
    let authority_present = std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some();
    if std::env::var_os(CHILD).is_none() {
        let private = tempfile::tempdir().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "app::latency_ui::isolation_tests::actual_app_fixture_mounts_and_interacts_without_service_attempts", "--nocapture"])
            .env(CHILD, "1")
            .env(EXPECTED_AUTHORITY, if authority_present { "present" } else { "absent" })
            .env(crate::latency_fixture::INTERACTIVE_ENV, "1");
        for (variable, directory) in [
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
        ] {
            let path = private.path().join(directory);
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            command.env(variable, path);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated actual-App diagnostic contract timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "isolated actual-App diagnostic contract failed"
        );
        return;
    }
    assert_eq!(
        std::env::var(EXPECTED_AUTHORITY).unwrap(),
        if authority_present {
            "present"
        } else {
            "absent"
        }
    );
    assert!(
        std::env::var_os("CAGENTS_LATENCY_OUTPUT").is_none(),
        "run the interactive fixture test without a latency report request"
    );
    let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let rejecting_listener = UnixListener::bind(&socket).unwrap();
    rejecting_listener.set_nonblocking(true).unwrap();
    let before = crate::latency_fixture::service_attempts();
    let window = window::Id::unique();
    let mut core = Core::default();
    core.set_main_window_id(Some(window));
    let startup = Cagents::load_startup().unwrap();
    let (mut app, initialize) = Cagents::init(core, (None, startup));
    assert!(app.session.is_none());
    assert!(app.settings_path.is_none() && app.settings_write_blocked);
    assert!(
        app.workspace_model_tx.is_closed(),
        "no publisher may own this queue"
    );
    assert!(
        app.hosted_terminals_rx.is_closed(),
        "no inventory observer may own this queue"
    );
    assert!(
        app.workspace_model_rx.is_closed(),
        "no shared-model observer may own this queue"
    );
    let count = app.durable.cards.len();
    assert_eq!(count, crate::latency_fixture::CARD_COUNT);
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Execute actual initialization tasks and poll every actual subscription.
    // Service subscriptions would attempt the guarded socket even if a future
    // fixture dispatcher discarded their completion messages.
    executor.block_on(async {
        drain_app_tasks(&mut app, initialize).await;
        let recipes = iced::advanced::subscription::into_recipes(app.subscription());
        let mut streams = iced::futures::stream::SelectAll::new();
        for recipe in recipes {
            streams.push(recipe.stream(iced::futures::stream::pending().boxed()));
        }
        let deadline = tokio::time::sleep(Duration::from_millis(250));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                message = streams.next(), if !streams.is_empty() => {
                    if let Some(message) = message {
                        let task = app.update(message);
                        drain_app_tasks(&mut app, task).await;
                    }
                }
            }
        }
    });
    macro_rules! send {
        ($message:expr) => {{
            let task = app.update($message);
            executor.block_on(drain_app_tasks(&mut app, task));
        }};
    }
    let renderer = super::super::tests::headless_renderer();
    let mut tree = Tree::empty();
    let mut native_scroll = false;
    for (width, height) in [(1440.0, 900.0), (900.0, 600.0), (1600.0, 1000.0)] {
        send!(window_geometry_message(&window::Event::Resized(Size::new(width, height))).unwrap());
        let mut element = cosmic::ApplicationExt::view_main(&app);
        element.as_widget_mut().diff(&mut tree);
        let size = Size::new(width, height);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        assert!(node.size().width > 0.0 && node.size().height > 0.0);
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -240.0, y: 0.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(width / 2.0, height / 2.0)),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(size),
        );
        drop(element);
        for message in messages {
            if let cosmic::Action::App(message) = message {
                native_scroll |= matches!(
                    &message,
                    Message::Viewport {
                        event: WorkspaceViewportEvent::Scrolled(_),
                        ..
                    }
                );
                send!(message);
            }
        }
        assert_eq!((app.window_width, app.window_height), (width, height));
    }
    assert!(
        native_scroll,
        "native wheel input must reach the mounted workspace scroll callback"
    );
    let first = app.workspace.focused().unwrap();
    send!(Message::FocusAdjacent(FocusDirection::Right));
    assert_ne!(app.workspace.focused(), Some(first));
    send!(Message::Sidebar(SidebarAction::Resize { width: 310.0 }));
    assert_eq!(app.settings.sidebar_width, 310.0);
    send!(Message::ToggleContextInspector);
    assert!(app.context_inspector_open);
    send!(Message::ContextInspector(InspectorAction::Close));
    assert!(!app.context_inspector_open);
    // Feed real backend commands through the same public App dispatcher.
    send!(Message::NewTerminal);
    send!(Message::RetryCard(first));
    send!(Message::Sidebar(SidebarAction::OpenSettings));
    send!(Message::RefreshUi(refresh_ui::Action::Focused(
        window, true
    )));
    assert_eq!(app.durable.cards.len(), count);
    assert_eq!(
        crate::latency_fixture::service_attempts(),
        before,
        "App startup, input or subscriptions attempted service work"
    );
    drop(app);
    assert_eq!(
        crate::latency_fixture::service_attempts(),
        before,
        "fixture shutdown attempted service work"
    );
    // Direct APIs must refuse before either opening the rejection-only socket
    // or starting a hosted worker, including explicit-socket constructors.
    assert_eq!(
        crate::control::connect_control_socket(&socket, Duration::from_millis(50))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(crate::terminal::ensure_daemon_ready().is_err());
    assert!(
        crate::terminal::HostedTerminalHandle::connect_at(&socket, TerminalId::new(), None, |_| {})
            .is_err()
    );
    assert_eq!(
        rejecting_listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        std::env::var(EXPECTED_AUTHORITY).unwrap(),
        if std::env::var_os(crate::agent_execution::AUTHORITY_ENV).is_some() {
            "present"
        } else {
            "absent"
        }
    );
}
