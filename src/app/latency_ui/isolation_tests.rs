use super::*;
use cosmic::app::cosmic::Cosmic;
use cosmic::iced::{
    Point,
    advanced::{Layout, Shell, clipboard, layout, widget::Tree},
    futures::StreamExt,
};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;

async fn drain_app_tasks(shell: &mut Cosmic<Cagents>, task: Task<Message>) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut tasks = VecDeque::from([task]);
        let mut copied = Vec::new();
        while let Some(task) = tasks.pop_front() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    match action {
                        iced::runtime::Action::Output(message) => {
                            tasks.push_back(shell.update(message))
                        }
                        iced::runtime::Action::Clipboard(
                            iced::runtime::clipboard::Action::Write { contents, .. },
                        ) => copied.push(contents),
                        _ => {}
                    }
                }
            }
        }
        copied
    })
    .await
    .expect("local fixture tasks must settle without backend work")
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
        for (scale, status_bar) in [("1", "on"), ("1", "off"), ("0.8", "on"), ("0.8", "off")] {
            let private = tempfile::tempdir().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "app::latency_ui::isolation_tests::actual_app_fixture_mounts_and_interacts_without_service_attempts", "--nocapture"])
            .env(CHILD, "1")
            .env(EXPECTED_AUTHORITY, if authority_present { "present" } else { "absent" })
            .env(crate::latency_fixture::INTERACTIVE_ENV, "1")
            .env(crate::latency_fixture::SCALE_ENV, scale)
            .env(crate::latency_fixture::STATUS_BAR_ENV, status_bar);
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
        }
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
    let (app, initialize) = Cagents::init(core, (None, startup));
    let mut shell = Cosmic::new(app);
    assert!(shell.app.session.is_none());
    assert!(shell.app.settings_path.is_none() && shell.app.settings_write_blocked);
    assert!(
        shell.app.workspace_model_tx.is_closed(),
        "no publisher may own this queue"
    );
    assert!(
        shell.app.hosted_terminals_rx.is_closed(),
        "no inventory observer may own this queue"
    );
    assert!(
        shell.app.workspace_model_rx.is_closed(),
        "no shared-model observer may own this queue"
    );
    let count = shell.app.durable.cards.len();
    assert_eq!(
        count, 3,
        "the interactive resident payload has two long chats and one empty chat"
    );
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Execute actual initialization tasks and poll every actual subscription.
    // Service subscriptions would attempt the guarded socket even if a future
    // fixture dispatcher discarded their completion messages.
    executor.block_on(async {
        drain_app_tasks(&mut shell, initialize).await;
        let recipes = iced::advanced::subscription::into_recipes(shell.app.subscription());
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
                        let task = shell.app.update(message);
                        drain_app_tasks(&mut shell, task).await;
                    }
                }
            }
        }
    });
    macro_rules! send {
        ($message:expr) => {{
            let task = shell.app.update($message);
            executor.block_on(drain_app_tasks(&mut shell, task));
        }};
    }
    let expected_scale = if std::env::var(crate::latency_fixture::SCALE_ENV).unwrap() == "0.8" {
        0.8
    } else {
        1.0
    };
    assert_eq!(
        shell.app.core.scale_factor(),
        expected_scale,
        "real Cosmic ScaleFactor action must reach Core"
    );
    assert_eq!(shell.app.settings.ui_scale, expected_scale);
    assert_eq!(
        shell.app.settings.show_status_bar,
        std::env::var(crate::latency_fixture::STATUS_BAR_ENV).unwrap() == "on"
    );
    assert_eq!(shell.app.settings.app_theme, AppTheme::Dark);
    let renderer = super::super::tests::headless_renderer();
    let mut tree = Tree::empty();
    let mut native_scroll = false;
    for (width, height) in [(1440.0, 900.0), (900.0, 600.0), (1600.0, 1000.0)] {
        send!(window_geometry_message(&window::Event::Resized(Size::new(width, height))).unwrap());
        let mut element = cosmic::ApplicationExt::view_main(&shell.app);
        tree.diff(&element);
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
        assert_eq!(
            (shell.app.window_width, shell.app.window_height),
            (width, height)
        );
    }
    assert!(
        native_scroll,
        "native wheel input must reach the mounted workspace scroll callback"
    );
    let first = shell.app.workspace.focused().unwrap();
    send!(Message::FocusAdjacent(FocusDirection::Right));
    assert_ne!(shell.app.workspace.focused(), Some(first));
    send!(Message::Sidebar(SidebarAction::Resize { width: 310.0 }));
    assert_eq!(shell.app.settings.sidebar_width, 310.0);
    send!(Message::ContextInspector(InspectorAction::Close));
    assert!(!shell.app.context_inspector_open);
    let target = chat_ui::Target {
        card: first,
        generation: 0,
    };
    let copy = shell
        .app
        .update(Message::ChatUi(chat_ui::Action::CopyMessage(
            target,
            "offline-fixture:0:0:question".into(),
            None,
        )));
    let copied = executor.block_on(drain_app_tasks(&mut shell, copy));
    assert!(
        copied
            .iter()
            .any(|text| text.contains("Offline fixture question 1")),
        "copy must use a real resident fixture body"
    );
    send!(Message::FocusCard(first));
    let find_task = shell.app.update(Message::ChatUi(chat_ui::Action::Find(
        target,
        "rail marker".into(),
    )));
    {
        use cosmic::iced::advanced::widget::{Operation, operation::Focusable};
        struct FindControl {
            id: String,
            mounted: bool,
        }
        impl Operation for FindControl {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                _: &mut dyn Focusable,
            ) {
                self.mounted |= id.is_some_and(|id| id.to_string() == self.id)
                    && bounds.width > 0.0
                    && bounds.height > 0.0;
            }
        }
        let mut element = cosmic::ApplicationExt::view_main(&shell.app);
        tree.diff(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1000.0)),
        );
        let mut find = FindControl {
            id: card_control_id(first, "chat-find").to_string(),
            mounted: false,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut find);
        assert!(find.mounted, "Find must be mounted in the actual App tree");
    }
    // This headless contract mounts and closes Find. Native match navigation
    // still needs widget-operation replies from a real event loop. Keep the
    // genuine deadline task owned until CloseFind cancels its intent, then
    // drain both tasks; never hide a backend future by dropping it unpolled.
    let close_find = shell
        .app
        .update(Message::ChatUi(chat_ui::Action::CloseFind(target)));
    executor.block_on(drain_app_tasks(
        &mut shell,
        Task::batch([find_task, close_find]),
    ));
    send!(Message::ToggleContextInspector);
    assert!(shell.app.context_inspector_open);
    send!(Message::ContextInspector(InspectorAction::Close));
    assert!(!shell.app.context_inspector_open);
    // Feed real backend commands through the same public App dispatcher.
    send!(Message::NewTerminal);
    send!(Message::RetryCard(first));
    send!(Message::Sidebar(SidebarAction::OpenSettings));
    send!(Message::RefreshUi(refresh_ui::Action::Focused(
        window, true
    )));
    assert_eq!(shell.app.durable.cards.len(), count);
    assert_eq!(
        crate::latency_fixture::service_attempts(),
        before,
        "App startup, input or subscriptions attempted service work"
    );
    drop(shell);
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
