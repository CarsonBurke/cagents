//! Fixed offline Claude initialization scenes in the real development UI.
//! The ordinary draft/send/client/HOST/runtime owns every actual provider
//! result and close. This fixture supplies no Ready, outcome or elapsed time.
use super::*;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};

const SELECTOR: &str = "CAGENTS_DEVELOPMENT_PREINIT_FIXTURE";
const PROMPT: &str = "Show the offline initialization control.";
const FIXTURES: &[(&str, &str, &[u8])] = &[
    (
        "healthy-advisory",
        "claude-preinit-healthy-advisory.py",
        include_bytes!("../../../tests/fixtures/claude-preinit-healthy-advisory.py"),
    ),
    (
        "preinit-advisory-pending",
        "claude-preinit-advisory-pending.py",
        include_bytes!("../../../tests/fixtures/claude-preinit-advisory-pending.py"),
    ),
    (
        "preinit-eof",
        "claude-preinit-eof.py",
        include_bytes!("../../../tests/fixtures/claude-preinit-eof.py"),
    ),
    (
        "preinit-noise-eight",
        "claude-preinit-noise-eight.py",
        include_bytes!("../../../tests/fixtures/claude-preinit-noise-eight.py"),
    ),
    (
        "healthy-noise-reset",
        "claude-preinit-noise-reset.py",
        include_bytes!("../../../tests/fixtures/claude-preinit-noise-reset.py"),
    ),
];

struct Source {
    path: PathBuf,
    original: std::fs::File,
    expected: &'static [u8],
}

impl Source {
    fn open(path: PathBuf, expected: &'static [u8]) -> Result<Self, String> {
        let original = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let source = Self {
            path,
            original,
            expected,
        };
        source.check()?;
        Ok(source)
    }

    fn check(&self) -> Result<(), String> {
        let held = self.original.metadata().map_err(|e| e.to_string())?;
        let named = std::fs::symlink_metadata(&self.path).map_err(|e| e.to_string())?;
        if !held.is_file()
            || !named.is_file()
            || held.uid() != unsafe { libc::geteuid() }
            || held.mode() & 0o022 != 0
            || held.mode() & 0o111 == 0
            || (held.dev(), held.ino()) != (named.dev(), named.ino())
            || held.len() != self.expected.len() as u64
            || self.path.canonicalize().map_err(|e| e.to_string())? != self.path
        {
            return Err("offline provider source identity or permissions changed".into());
        }
        let mut bytes = vec![0; self.expected.len()];
        self.original
            .read_exact_at(&mut bytes, 0)
            .map_err(|e| e.to_string())?;
        if bytes != self.expected {
            return Err("offline provider source differs from the compiled fixed fixture".into());
        }
        Ok(())
    }
}

pub(in crate::app) struct Context {
    root: PathBuf,
    scene: String,
    source: Source,
    helper: Source,
}

fn private_directory(path: &Path) -> Result<(), String> {
    let info = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !info.is_dir()
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o7777 != 0o700
        || path.canonicalize().map_err(|e| e.to_string())? != path
    {
        return Err("offline fixture needs a canonical owned mode0700 directory".into());
    }
    Ok(())
}

/// Called before ordinary startup reads and again before construction. Only
/// the explicit selector and fixture-owned routing values are examined.
pub(in crate::app) fn context() -> Result<Option<Context>, String> {
    let Some(scene) = std::env::var_os(SELECTOR) else {
        return Ok(None);
    };
    let scene = scene.into_string().map_err(|_| "invalid offline scene")?;
    let (_, wrapper, expected) = FIXTURES
        .iter()
        .find(|(name, _, _)| *name == scene)
        .ok_or("unknown offline Claude scene")?;
    if crate::instance::APPLICATION_ID != "dev.cagents.Cagents.Development"
        || [
            "CAGENTS_LATENCY_OUTPUT",
            "CAGENTS_DEVELOPMENT_ACCOUNT_FIXTURE",
            "CAGENTS_DEVELOPMENT_AUTOMATION_FIXTURE",
            crate::instance::UI_LAUNCH_HANDSHAKE_ENV,
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Err("offline fixture needs the isolated development runner without another fixture or launcher handshake".into());
    }
    let names = [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ];
    let children = ["data", "config", "cache", "state", "runtime"];
    let paths = names
        .iter()
        .map(|name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .ok_or("offline fixture needs all five private XDG directories")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let root = paths[0]
        .parent()
        .ok_or("missing offline namespace")?
        .to_path_buf();
    let allowed = [
        format!("cagents-1859-native-{scene}-wide-before"),
        format!("cagents-1859-native-{scene}-wide-after"),
        format!("cagents-1859-native-{scene}-narrow-before"),
        format!("cagents-1859-native-{scene}-narrow-after"),
    ];
    if root.parent() != Some(Path::new("/tmp"))
        || !allowed
            .iter()
            .any(|name| root.file_name() == Some(std::ffi::OsStr::new(name)))
        || paths
            .iter()
            .zip(children)
            .any(|(path, child)| *path != root.join(child))
        || std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from) != Some(root.join("claude"))
    {
        return Err("offline fixture needs its fixed scene/viewport/side namespace and private Claude directory".into());
    }
    private_directory(&root)?;
    for path in &paths {
        private_directory(path)?;
    }
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    Ok(Some(Context {
        root,
        scene,
        source: Source::open(directory.join(wrapper), expected)?,
        helper: Source::open(
            directory.join("claude-preinit-contract.py"),
            include_bytes!("../../../tests/fixtures/claude-preinit-contract.py"),
        )?,
    }))
}

pub(in crate::app) fn bootstrap(context: &Context) -> Result<Bootstrap, String> {
    use std::io::Write;
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(context.root.join("preinit-native-started"))
        .map_err(|e| format!("offline namespace already used or unavailable: {e}"))?;
    writeln!(marker, "OFFLINE Claude initialization: {}", context.scene)
        .map_err(|e| e.to_string())?;
    for child in ["public-project", "claude"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(context.root.join(child))
            .map_err(|e| e.to_string())?;
    }
    let mut settings = AppSettings::default();
    settings.app_theme = AppTheme::Dark;
    settings.default_native_pane_fraction = 1.0;
    Ok(Bootstrap {
        workspace: WorkspaceState::default(),
        session: None,
        projects: Vec::new(),
        worktrees: Vec::new(),
        settings,
        settings_path: None,
        settings_write_blocked: true,
        status: None,
        save_health: None,
        restore_warnings: Vec::new(),
        settings_warnings: Vec::new(),
    })
}

pub(in crate::app) struct Fixture {
    context: Context,
    card: CardId,
    failed: bool,
}

pub(in crate::app) fn install(app: &mut Cagents, context: Context) -> Task<Message> {
    let card = CardId::new();
    let session = chat::Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: context.source.path.to_string_lossy().into_owned(),
                arguments: vec![
                    "--permission-mode".into(),
                    "default".into(),
                    "--disable-slash-commands".into(),
                ],
                working_directory: context.root.join("public-project"),
                environment: [(
                    "CLAUDE_CONFIG_DIR".into(),
                    context.root.join("claude").display().to_string(),
                )]
                .into(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: None,
        terminal: None,
    };
    let state = CardState {
        id: card,
        generation: 0,
        title: "Offline Claude initialization".into(),
        kind: CardKind::Chat(session.clone()),
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Hot,
        width_fraction: 1.0,
    };
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(state.clone());
    durable.focused = Some(card);
    let update = app
        .workspace
        .add_card(
            WorkspaceCard::new(card, state.title, CardWidth::Fraction(1.0))
                .expect("fixed offline card"),
            0,
            true,
        )
        .expect("empty offline workspace");
    let target = app.chat_ui.next_target(card);
    let mut snapshot = draft_snapshot(Provider::Claude, Vec::new(), None, None);
    snapshot.permission_mode = Some("manual".into());
    let mut runtime = Runtime::draft(
        target,
        session,
        snapshot,
        true,
        app.settings.editor_syntax_theme,
    );
    runtime.replace_draft(text_editor::Content::with_text(PROMPT));
    app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
    app.context_inspector_open = false;
    app.native_preinit_fixture = Some(Fixture {
        context,
        card,
        failed: false,
    });
    record(app, "installed");
    Task::batch([
        app.apply_viewport(update, true, false),
        app.sync_chrome(),
        iced::widget::operation::focus(card_control_id(card, "chat-composer"))
            .map(cosmic::Action::App),
    ])
}

fn record(app: &Cagents, event: &str) {
    let Some(fixture) = &app.native_preinit_fixture else {
        return;
    };
    let state = match app.cards.get(&fixture.card) {
        Some(CardRuntime::Chat(runtime)) => serde_json::json!({
            "conversation":runtime.session.id, "target_generation":runtime.target.generation,
            "ready":runtime.snapshot.ready,"busy":runtime.snapshot.busy,"exited":runtime.snapshot.exited,
            "error":runtime.error.as_deref().or(runtime.snapshot.error.as_deref())
                .map(|message| message.chars().take(512).collect::<String>()),
            "item_count":runtime.snapshot.items.len(),
            "original_program":runtime.session.launch.recipe.program,
        }),
        _ => serde_json::Value::Null,
    };
    eprintln!(
        "PREINIT_NATIVE {}",
        serde_json::json!({"event":event,"scene":fixture.context.scene,
        "card":fixture.card,"state":state,"qualified":false,
        "boundary":"offline fixed process; actual reducer/runtime only; no provider/auth/tools/history qualification"})
    );
}

fn admitted(action: &Action, card: CardId) -> bool {
    match action {
        Action::Composer(t, _)
        | Action::ComposerUp(t)
        | Action::ComposerCursor(t, _)
        | Action::Submit(t, PromptMode::Send)
        | Action::DispatchQueued(t)
        | Action::CancelQueued(t)
        | Action::Ready(t, _)
        | Action::Wake(t, _)
        | Action::UpdatesAcknowledged(t, _)
        | Action::Retired(t, _)
        | Action::CloseFinished(t, _)
        | Action::RetainedCloseFinished(t, _, _)
        | Action::Reconnect(t)
        | Action::Interrupt(t)
        | Action::DisarmInterrupt(t, _)
        | Action::CommandFinished(t, _, _)
        | Action::Scrolled(t, _, _, _)
        | Action::Latest(t)
        | Action::MarkdownPrepared(t, _, _, _)
        | Action::LocalPromptPrepared(t, _, _)
        | Action::FlowRequest(t, _)
        | Action::FlowReady(t, _, _)
        | Action::RetryMarkdown(t)
        | Action::Toggle(t, _)
        | Action::ToggleTurn(t, _)
        | Action::ToggleFailure(t, _)
        | Action::Unfold(t, _)
        | Action::DraftSkills(t, _, _) => t.card == card,
        Action::PreparationAvailable | Action::FlowAvailable | Action::Tick => true,
        _ => false,
    }
}

/// Dedicated active-fixture dispatcher. Accepted actions use the unchanged
/// normal reducers and return all their Tasks, including late cleanup. Global
/// App shortcuts/account/provider/history/file actions are not a fixture API.
pub(in crate::app) fn update(app: &mut Cagents, message: Message) -> Task<Message> {
    let fixture = app
        .native_preinit_fixture
        .as_ref()
        .expect("active offline fixture");
    let card = fixture.card;
    let invalid = fixture
        .context
        .source
        .check()
        .and_then(|_| fixture.context.helper.check())
        .err();
    let mut close = Task::none();
    if let Some(error) = invalid {
        // Reject further input, but retain all real late Ready/retirement work.
        if !fixture.failed {
            app.native_preinit_fixture.as_mut().unwrap().failed = true;
            app.notices.record(TransientNotice::error(error));
            close = app.begin_chat_close(card);
        }
    }
    let failed = app.native_preinit_fixture.as_ref().unwrap().failed;
    let modal = app.modal_is_open();
    if modal {
        app.chrome_ui.close();
    }
    let foreground_blocked = user_workspace_message_blocked_while_modal(&message)
        && (modal || (app.chrome_ui.is_open() && !matches!(&message, Message::ChromeUi(_))));
    if foreground_blocked {
        // The normal pure foreground guard must not discard a source-failure
        // cleanup Task which was already created above. Automatic Ready,
        // retirement and close callbacks remain outside this user-input gate.
        app.sync_popup_keyboard_nav();
        record(app, "foreground-refused");
        return Task::batch([close, app.sync_chrome()]);
    }
    let task = match message {
        Message::ChatUi(action) if admitted(&action, card) => {
            let accepted = match &action {
                Action::Ready(..)
                | Action::Retired(..)
                | Action::CloseFinished(..)
                | Action::RetainedCloseFinished(..)
                | Action::Wake(..)
                | Action::UpdatesAcknowledged(..) => true,
                _ if failed => false,
                Action::Submit(t, _) => app.chat_runtime_mut(*t).is_some_and(|runtime| {
                    runtime.composer.text() == PROMPT && runtime.attachments.is_empty()
                }),
                Action::Composer(t, edit) => app.chat_runtime_mut(*t).is_some_and(|runtime| {
                    let mut proposed = text_editor::Content::with_text(&runtime.composer.text());
                    proposed.move_to(runtime.composer.cursor());
                    proposed.perform(edit.clone());
                    PROMPT.starts_with(&proposed.text())
                }),
                _ => true,
            };
            if accepted {
                app.handle_chat_ui(action)
            } else {
                app.notices.record(TransientNotice::warning(
                    "This offline scene accepts only its displayed public prompt.",
                ));
                Task::none()
            }
        }
        Message::CloseCard(id) if id == card => app.begin_chat_close(card),
        Message::WindowCloseRequested(window) if app.core.main_window_id() == Some(window) => {
            if app.exit.is_some() || app.dirty_repository_close.is_some() {
                Task::none()
            } else if app.window_close_transient_blocker().is_some() {
                app.automation_close_generation = app.automation_close_generation.wrapping_add(1);
                app.transient_work_close = Some(window);
                iced::widget::operation::focus(widget::Id::new("dialog.transient-close.cancel"))
                    .map(cosmic::Action::App)
            } else {
                app.transient_work_close = None;
                app.begin_dirty_close(DirtyCloseIntent::Application(window), VecDeque::new())
            }
        }
        Message::CancelTransientWorkClose => {
            app.transient_work_close = None;
            iced::widget::operation::focus(card_control_id(card, "chat-composer"))
                .map(cosmic::Action::App)
        }
        Message::LanguageServersReaped => app.advance_exit(|exit| exit.servers_reaped = true),
        Message::WindowClosed(window) => app.advance_exit(|exit| {
            if exit.closing == Some(window) {
                exit.closing = None;
            }
        }),
        Message::KeyEvent { event, captured } => {
            if let Some(task) = app.chrome_key_event(&event) {
                task
            } else {
                match event {
                    keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        modifiers,
                        ..
                    } if !captured
                        && modifiers.is_empty()
                        && app.transient_work_close.is_some() =>
                    {
                        cosmic::task::message(Message::CancelTransientWorkClose)
                    }
                    keyboard::Event::ModifiersChanged(modifiers) => {
                        app.keyboard_modifiers = modifiers;
                        Task::none()
                    }
                    keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Tab),
                        modifiers,
                        ..
                    } if !modifiers.command() && !modifiers.alt() && !modifiers.logo() => {
                        Task::none() // The actual focused widget owns Tab; global shortcuts are excluded.
                    }
                    _ => Task::none(),
                }
            }
        }
        Message::Viewport { generation, event } if generation == app.workspace_view_generation => {
            if matches!(event, WorkspaceViewportEvent::Reordered { .. }) {
                Task::none()
            } else {
                let (update, synchronize) = update_workspace_viewport(&mut app.workspace, event);
                app.apply_viewport(update, synchronize, false)
            }
        }
        Message::Frame(at) => match app.frame_clock.elapsed(at) {
            Some(elapsed) => {
                let update = app.workspace.tick(elapsed);
                app.apply_viewport(update, true, false)
            }
            None => Task::none(),
        },
        Message::WindowResized(width, height) => {
            if width.is_finite() && width > 0.0 {
                app.window_width = width;
            }
            if height.is_finite() && height > 0.0 {
                app.window_height = height;
            }
            app.content_width_changed()
        }
        Message::WindowScaleFactorChanged(scale) => {
            if scale.is_finite() && scale > 0.0 {
                app.window_scale_factor = scale;
            }
            Task::none()
        }
        Message::RefreshUi(refresh_ui::Action::Focused(window, focused))
            if app.core.main_window_id() == Some(window) =>
        {
            app.refresh_ui.focused = focused;
            Task::none()
        }
        Message::ChromeUi(action @ chrome_ui::Action::ToggleNotices) => app.update_chrome(action),
        Message::ChromeUi(action @ chrome_ui::Action::ShowNoticesFor(id)) if id == card => {
            app.update_chrome(action)
        }
        // The initial chrome popup is None; these are the only popup-opening
        // actions this dispatcher admits. Close therefore reaches only the
        // actual Notices popup or None, never NewTab/Attention/launch menus.
        Message::ChromeUi(chrome_ui::Action::Close) => app.update_chrome(chrome_ui::Action::Close),
        Message::DismissNotice(id) => {
            app.notices.dismiss(id);
            Task::batch([app.reconcile_notice_popup(), app.refocus_notices()])
        }
        Message::DismissStatus | Message::DismissAllNotices => {
            app.status = None;
            app.notices.clear();
            Task::batch([app.reconcile_notice_popup(), app.refocus_notices()])
        }
        Message::StartupWindowOpened(_) | Message::StartupReported(_) => Task::none(),
        _ => Task::none(),
    };
    if let Some(notice) = app.status.take() {
        app.notices.record(notice);
    }
    app.sync_popup_keyboard_nav();
    record(app, "update");
    Task::batch([close, task, app.reconcile_notice_popup(), app.sync_chrome()])
}

pub(in crate::app) fn subscription(app: &Cagents) -> Subscription<Message> {
    let mut subscriptions = vec![
        app.chat_subscription(),
        event::listen_with(|event, status, window| match event {
            iced::Event::Keyboard(event) => Some(Message::KeyEvent {
                event,
                captured: matches!(status, event::Status::Captured),
            }),
            iced::Event::Window(window::Event::CloseRequested) => {
                Some(Message::WindowCloseRequested(window))
            }
            iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(window)),
            iced::Event::Window(window::Event::Focused) => Some(Message::RefreshUi(
                refresh_ui::Action::Focused(window, true),
            )),
            iced::Event::Window(window::Event::Unfocused) => Some(Message::RefreshUi(
                refresh_ui::Action::Focused(window, false),
            )),
            iced::Event::Window(window::Event::Rescaled(scale)) => {
                Some(Message::WindowScaleFactorChanged(scale))
            }
            iced::Event::Window(event) => window_geometry_message(&event),
            _ => None,
        }),
    ];
    if let Some(window) = app.core.main_window_id()
        && app.workspace.is_animating()
    {
        subscriptions.push(workspace_widget::animation_frames(window, Message::Frame));
    }
    Subscription::batch(subscriptions)
}
