//! Fixed development-only native renderer workload. No host or account exists.
//! The same scenario can be applied to an older renderer: Snapshot additions
//! live in JSON overlays, which older snapshots ignore when deserializing.
use super::*;
use serde_json::{Value, json};

const MODE: &str = "capacity-chronology-2014";
const LONG_MODE: &str = "capacity-chronology-2014-long";
const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";
const DIAGNOSTIC: &str = "Selected model is at capacity. Please try a different model.\nrequest-id: synthetic-capacity-incident\nOriginal provider diagnostics remain available.";
const SCENARIO: &str = r#"{
  "parts": ["Recovery resumed. ", "I am checking the saved state. ", "The earlier failure stays before this follow-up."],
  "command": "printf 'Synthetic recovery check passed\\n'",
  "output": "Synthetic recovery check passed\n",
  "answer": "The synthetic recovery check passed. This turn is complete."
}"#;

fn selected(value: Option<&str>) -> bool {
    matches!(value, Some(MODE | LONG_MODE))
}

fn mode() -> Option<bool> {
    static SELECTED_MODE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *SELECTED_MODE.get_or_init(|| {
        let value = std::env::var("CAGENTS_CHAT_CHRONOLOGY_FIXTURE").ok();
        selected(value.as_deref()).then_some(value.as_deref() == Some(LONG_MODE))
    })
}

pub(in crate::app) fn enabled() -> bool {
    mode().is_some()
}

fn long() -> bool {
    mode() == Some(true)
}

fn session(index: usize) -> chat::Session {
    chat::Session {
        id: format!("11111111-1111-4111-8111-{index:012}")
            .parse()
            .unwrap(),
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                program: "/offline-chronology-fixture".into(),
                arguments: Vec::new(),
                working_directory: "/".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    }
}

pub(in crate::app) fn bootstrap() -> Bootstrap {
    bootstrap_for(long())
}

fn bootstrap_for(long: bool) -> Bootstrap {
    let mut workspace = WorkspaceState::default();
    for index in 0..if long { 5 } else { 1 } {
        workspace.cards.push(CardState {
            id: format!("22222222-2222-4222-8222-{index:012}")
                .parse()
                .unwrap(),
            generation: 1,
            title: if index == 0 {
                "Offline chronology fixture · renderer only".into()
            } else {
                format!("Offline peer {index}")
            },
            kind: CardKind::Chat(session(index)),
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Hot,
            width_fraction: 1.0,
        });
    }
    workspace.focused = workspace.cards.first().map(|card| card.id);
    Bootstrap {
        workspace,
        session: None,
        projects: Vec::new(),
        worktrees: Vec::new(),
        settings: AppSettings::default(),
        settings_path: None,
        settings_write_blocked: true,
        status: None,
        save_health: None,
        restore_warnings: Vec::new(),
        settings_warnings: Vec::new(),
    }
}

fn text(id: impl Into<String>, role: chat::Role, body: impl Into<String>) -> Arc<chat::Item> {
    Arc::new(chat::Item {
        id: id.into(),
        role,
        content: ItemContent::Text {
            text: body.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        started_at: None,
        at: None,
    })
}

fn overlay(snapshot: Snapshot, patch: Value) -> Snapshot {
    let mut value = serde_json::to_value(snapshot).expect("fixture snapshot serializes");
    value
        .as_object_mut()
        .unwrap()
        .extend(patch.as_object().unwrap().clone());
    serde_json::from_value(value).expect("fixed fixture snapshot overlay")
}

fn initial_snapshot(long: bool, peer: bool) -> Snapshot {
    let mut snapshot = Snapshot {
        ready: true,
        ..Snapshot::default()
    };
    if long {
        for index in 0..40 {
            snapshot.items.push(text(
                format!("prior-{index}/prompt"),
                chat::Role::User,
                format!("Review synthetic checkpoint {}.", index + 1),
            ));
            snapshot.items.push(text(format!("prior-{index}/answer"), chat::Role::Assistant,
                format!("Checkpoint {} is ready.\n\nThe synthetic log records the expected inputs, completed checks, and retained diagnostics. This paragraph provides ordinary conversation length for scrolling.\n\n```text\ncheckpoint: {}\nstatus: reviewed\n```\n\nThe next checkpoint can continue without changing earlier results.", index + 1, index + 1)));
        }
    }
    snapshot.items.push(text("fixture-intro", chat::Role::System,
        "Offline native renderer fixture. No provider, account, daemon, or saved history is connected. Send . to resume, then Try the check again., then Continue."));
    snapshot.items.push(text(
        "failed-turn/prompt",
        chat::Role::User,
        "Check the saved state.",
    ));
    if peer {
        overlay(
            snapshot,
            json!({"generation":1,"busy":true,"detail":"Offline peer publication"}),
        )
    } else {
        overlay(
            snapshot,
            json!({"generation":1,"turn_generation":1,"error_generation":1,"error":DIAGNOSTIC}),
        )
    }
}

pub(in crate::app) struct State {
    turn: usize,
    step: Option<usize>,
    peer_ticks: usize,
}

impl State {
    fn new() -> Self {
        Self {
            turn: 1,
            step: None,
            peer_ticks: 0,
        }
    }

    fn submit(&mut self, runtime: &mut Runtime) {
        let draft = runtime.composer.text();
        if draft.trim().is_empty() || self.step.is_some() || self.turn >= 4 {
            return;
        }
        self.turn += 1;
        runtime.remember_with_attachments(draft.clone(), Vec::new());
        runtime.replace_draft(text_editor::Content::new());
        let mut next = (*runtime.snapshot).clone();
        next.items.push(text(
            format!("fixture-turn-{}/prompt", self.turn),
            chat::Role::User,
            draft,
        ));
        let patch = if self.turn == 3 {
            let mut failure =
                (*text("fixture-turn-3/error:0", chat::Role::Assistant, DIAGNOSTIC)).clone();
            chat::failure::mark_provider_error(Provider::Codex, &mut failure);
            next.items.push(Arc::new(failure));
            json!({"busy":false,"error":DIAGNOSTIC,"error_generation":2,"error_item":"fixture-turn-3/error:0"})
        } else {
            next.items.push(text(
                format!("fixture-turn-{}/answer", self.turn),
                chat::Role::Assistant,
                "",
            ));
            self.step = Some(0);
            json!({"busy":true,"error":null,"error_item":null})
        };
        let generation = next.generation + 1;
        let mut next = overlay(next, patch);
        next.generation = generation;
        next = overlay(next, json!({"turn_generation":self.turn}));
        publish(runtime, next);
    }

    fn tick(&mut self, runtime: &mut Runtime) {
        let Some(step) = self.step else {
            return;
        };
        let data: Value = serde_json::from_str(SCENARIO).unwrap();
        let mut next = (*runtime.snapshot).clone();
        let answer = format!("fixture-turn-{}/answer", self.turn);
        let tool_id = format!("fixture-turn-{}/check", self.turn);
        match step {
            0..=2 => {
                let body: String = data["parts"].as_array().unwrap()[..=step]
                    .iter()
                    .map(|part| part.as_str().unwrap())
                    .collect();
                let found = next
                    .items
                    .iter_mut()
                    .find(|item| item.id == answer)
                    .unwrap();
                *found = text(answer, chat::Role::Assistant, body);
            }
            3 | 4 => {
                let item = Arc::new(chat::Item {
                    id: tool_id.clone(),
                    role: chat::Role::Assistant,
                    content: ItemContent::Tool {
                        name: "Bash".into(),
                        input: json!({"command":data["command"]}).to_string(),
                        output: if step == 4 {
                            data["output"].as_str().unwrap().into()
                        } else {
                            "Synthetic recovery ".into()
                        },
                        status: if step == 4 {
                            chat::ToolStatus::Succeeded
                        } else {
                            chat::ToolStatus::Running
                        },
                    },
                    started_at: None,
                    at: None,
                });
                if let Some(old) = next.items.iter_mut().find(|item| item.id == tool_id) {
                    *old = item;
                } else {
                    next.items.push(item);
                }
            }
            _ => {
                next.items.push(text(
                    format!("fixture-turn-{}/followup", self.turn),
                    chat::Role::Assistant,
                    data["answer"].as_str().unwrap(),
                ));
                next.busy = false;
            }
        }
        next.generation += 1;
        self.step = (step < 5).then_some(step + 1);
        publish(runtime, next);
    }
}

fn publish(runtime: &mut Runtime, snapshot: Snapshot) {
    runtime
        .handle()
        .unwrap()
        .development_fixture_publish(snapshot)
        .expect("local fixture publication");
    runtime.refresh();
}

pub(in crate::app) fn install(mut app: Cagents) -> (Cagents, Task<Message>) {
    for (index, card) in app.durable.cards.iter().enumerate() {
        let CardKind::Chat(session) = &card.kind else {
            unreachable!()
        };
        let handle = Arc::new(ClientHandle::development_fixture(
            session.clone(),
            initial_snapshot(long(), index != 0),
        ));
        let mut runtime = Runtime::new(
            Target {
                card: card.id,
                generation: 1,
            },
            session.clone(),
            Connection::Live(handle),
            true,
            app.settings.editor_syntax_theme,
        );
        runtime.files = Some(Arc::new(Vec::new()));
        if index == 0 {
            runtime.chronology_fixture = Some(State::new());
        }
        app.cards
            .insert(card.id, CardRuntime::Chat(Box::new(runtime)));
    }
    app.context_inspector_open = false;
    app.lifecycle = LifecycleManager::new(LifecycleBudget::default());
    let chrome = app.sync_chrome();
    (app, chrome)
}

fn safe(action: &Action) -> bool {
    matches!(
        action,
        Action::PreparationAvailable
            | Action::FlowAvailable
            | Action::FlowRequest(..)
            | Action::FlowReady(..)
            | Action::Composer(..)
            | Action::ComposerCursor(..)
            | Action::ComposerUp(..)
            | Action::HistoryOlder(..)
            | Action::HistoryNewer(..)
            | Action::Toggle(..)
            | Action::ToggleTurn(..)
            | Action::ToggleFailure(..)
            | Action::CopyFailure(..)
            | Action::Copy(..)
            | Action::CopyLiteral(..)
            | Action::CopySharedCode(..)
            | Action::MessageCopied(..)
            | Action::MarkdownPrepared(..)
            | Action::LocalPromptPrepared(..)
            | Action::RetryMarkdown(..)
            | Action::Unfold(..)
            | Action::Find(..)
            | Action::FindNext(..)
            | Action::FindAvailable
            | Action::FindRetry(..)
            | Action::FindGroups(..)
            | Action::FindHighlight(..)
            | Action::FindHighlighted(..)
            | Action::FindResume(..)
            | Action::FindExpired(..)
            | Action::FindIndexed(..)
            | Action::FindLocated(..)
            | Action::CloseFind(..)
            | Action::FindFocused(..)
            | Action::Tick
            | Action::ClearCopied(..)
            | Action::Scrolled(..)
            | Action::CompletionMove(..)
            | Action::CompletionDismiss(..)
            | Action::CompletionScrolled(..)
    )
}

pub(in crate::app) fn update(app: &mut Cagents, message: Message) -> Task<Message> {
    match message {
        Message::ChatUi(Action::Submit(target, _)) => {
            if let Some(runtime) = app.chat_runtime_mut(target)
                && let Some(mut state) = runtime.chronology_fixture.take()
            {
                state.submit(runtime);
                runtime.chronology_fixture = Some(state);
            }
        }
        Message::ChronologyFixtureTick => {
            let mut peers = false;
            for runtime in app.cards.values_mut() {
                let CardRuntime::Chat(runtime) = runtime else {
                    continue;
                };
                if let Some(mut state) = runtime.chronology_fixture.take() {
                    state.tick(runtime);
                    peers = state.peer_ticks < 16;
                    state.peer_ticks += usize::from(peers);
                    runtime.chronology_fixture = Some(state);
                }
            }
            if peers && long() {
                for runtime in app.cards.values_mut() {
                    let CardRuntime::Chat(runtime) = runtime else {
                        continue;
                    };
                    if runtime.chronology_fixture.is_some() {
                        continue;
                    }
                    let mut next = (*runtime.snapshot).clone();
                    next.generation += 1;
                    let id = "offline-peer-progress";
                    let body = format!(
                        "Offline peer update {}. Retained checkpoint content remains unchanged.",
                        next.generation
                    );
                    if let Some(item) = next.items.iter_mut().find(|item| item.id == id) {
                        *item = text(id, chat::Role::Assistant, body);
                    } else {
                        next.items.push(text(id, chat::Role::Assistant, body));
                    }
                    next.busy = next.generation < 17;
                    publish(runtime, next);
                }
            }
        }
        Message::ChatUi(Action::CopyMessage(target, id, _)) => {
            // Every fixture item is complete and untruncated. Use the ordinary
            // local item-copy/clipboard path, never a paged daemon copy receipt.
            return app.handle_chat_ui(Action::Copy(target, id, CopyPart::Text));
        }
        Message::ChatUi(Action::Complete(target, insertion, otherwise)) => {
            let Some(runtime) = app.chat_runtime_mut(target) else {
                return Task::none();
            };
            if !runtime.complete(&insertion, otherwise.is_some()) {
                return otherwise
                    .map_or_else(Task::none, |action| update(app, Message::ChatUi(*action)));
            }
            return iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
                .map(cosmic::Action::App);
        }
        Message::ChatUi(Action::Latest(target)) => {
            if let Some(runtime) = app.chat_runtime_mut(target) {
                return scroll::snap_to_edge(
                    target,
                    scroll::Edge::Latest,
                    runtime.find_navigation.guard(),
                    runtime.find_navigation.document_epoch(),
                )
                .map(cosmic::Action::App);
            }
        }
        Message::ChatUi(action) if safe(&action) => return app.handle_chat_ui(action),
        Message::KeyEvent { event, captured } => {
            if let keyboard::Event::ModifiersChanged(modifiers) = &event {
                app.keyboard_modifiers = *modifiers;
            }
            if let Some(task) = app.chat_picker_key(&event) {
                return task;
            }
            if !captured && let Some(task) = app.chat_scroll_key(&event) {
                return task;
            }
        }
        Message::WindowResized(width, height) => {
            if width.is_finite() && width > 0.0 {
                app.window_width = width;
            }
            if height.is_finite() && height > 0.0 {
                app.window_height = height;
            }
            let content_width = app.content_width();
            let _ = app.workspace.resize_viewport(content_width);
        }
        Message::WindowScaleFactorChanged(scale) if scale.is_finite() && scale > 0.0 => {
            app.window_scale_factor = scale;
        }
        Message::Viewport { generation, event } if generation == app.workspace_view_generation => {
            let _ = update_workspace_viewport(&mut app.workspace, event);
        }
        Message::WindowCloseRequested(id) => {
            super::super::native_fixture_frames::flush();
            let _ = crate::performance::chat_stage::flush();
            return iced::window::close(id)
                .chain(iced::exit())
                .map(cosmic::Action::App);
        }
        Message::WindowClosed(_) => {
            super::super::native_fixture_frames::flush();
            let _ = crate::performance::chat_stage::flush();
            return iced::exit().map(cosmic::Action::App);
        }
        Message::ChromeUi(chrome_ui::Action::Close) => {
            super::super::native_fixture_frames::flush();
            let _ = crate::performance::chat_stage::flush();
            return app.core.main_window_id().map_or_else(Task::none, |id| {
                iced::window::close(id)
                    .chain(iced::exit())
                    .map(cosmic::Action::App)
            });
        }
        _ => {} // Fixture-only fence: no account, provider, files, links, or lifecycle work.
    }
    app.sync_chrome()
}

pub(in crate::app) fn busy(app: &Cagents) -> bool {
    app.cards
        .values()
        .any(|runtime| matches!(runtime, CardRuntime::Chat(runtime) if runtime.snapshot.busy))
}

pub(in crate::app) fn subscription(app: &Cagents) -> Subscription<Message> {
    let running = app.cards.values().any(|card| {
        matches!(card,
        CardRuntime::Chat(runtime) if runtime.chronology_fixture.as_ref().is_some_and(|state|
            state.step.is_some() || (long() && state.peer_ticks < 16)))
    });
    Subscription::batch([
        app.chat_subscription(),
        if running {
            iced::time::every(Duration::from_millis(750)).map(|_| Message::ChronologyFixtureTick)
        } else {
            Subscription::none()
        },
        event::listen_with(|event, status, event_window| match event {
            iced::Event::Keyboard(event) => Some(Message::KeyEvent {
                event,
                captured: matches!(status, event::Status::Captured),
            }),
            iced::Event::Window(window::Event::CloseRequested) => {
                Some(Message::WindowCloseRequested(event_window))
            }
            iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(event_window)),
            iced::Event::Window(window::Event::Rescaled(scale)) => {
                Some(Message::WindowScaleFactorChanged(scale))
            }
            iced::Event::Window(event) => window_geometry_message(&event),
            _ => None,
        }),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original_failure_stays_before_resume(
        runtime: &mut Runtime,
        original: &failure::Note,
        key: &str,
        resumed: bool,
    ) {
        assert_eq!(runtime.failures.first(), Some(original));
        assert_eq!(runtime.failure_ui.incidents[0].key, key);
        assert_eq!(runtime.failure_details(key).as_deref(), Some(DIAGNOSTIC));
        preparation::drain_fixture(runtime);
        runtime.failure_ui.expanded.insert(key.into());
        runtime.cache_failure_preview(key);
        let catalog = runtime.selection_document().0.search_blocks();
        let position = |identity| {
            catalog
                .iter()
                .position(|(found, _)| *found == identity)
                .unwrap_or_else(|| panic!("missing catalog identity {identity}: {catalog:?}"))
        };
        let diagnostic = position(failure::diagnostic_preview_identity(key));
        assert!(position(selection::logical_identity("failed-turn/prompt", &[2])) < diagnostic);
        if resumed {
            assert!(
                diagnostic < position(selection::logical_identity("fixture-turn-2/prompt", &[2]))
            );
        }
    }

    #[test]
    fn mode_is_exact_and_bootstrap_has_no_operational_state() {
        for value in [None, Some("1"), Some("capacity-chronology-2014-extra")] {
            assert!(!selected(value));
        }
        assert!(selected(Some(MODE)) && selected(Some(LONG_MODE)));
        for (long, count) in [(false, 1), (true, 5)] {
            let bootstrap = bootstrap_for(long);
            assert_eq!(bootstrap.workspace.cards.len(), count);
            assert!(
                bootstrap.session.is_none()
                    && bootstrap.settings_path.is_none()
                    && bootstrap.settings_write_blocked
            );
            for card in bootstrap.workspace.cards {
                let CardKind::Chat(session) = card.kind else {
                    panic!("fixture chat")
                };
                assert!(session.proof.is_none() && session.terminal.is_none());
                assert!(session.launch.recipe.environment.is_empty());
                assert_eq!(session.launch.recipe.program, "/offline-chronology-fixture");
            }
        }
    }

    #[test]
    fn real_composer_transitions_publish_streams_without_pending_requests() {
        let session = session(0);
        let handle = Arc::new(ClientHandle::development_fixture(
            session.clone(),
            initial_snapshot(false, false),
        ));
        let mut runtime = Runtime::new(
            Target {
                card: CardId::new(),
                generation: 1,
            },
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        runtime.files = Some(Arc::new(Vec::new()));
        let mut state = State::new();
        assert!(
            runtime
                .snapshot
                .error
                .as_deref()
                .unwrap()
                .starts_with(CAPACITY)
        );
        assert!(
            runtime
                .snapshot
                .items
                .iter()
                .all(|item| item.role != chat::Role::Assistant),
            "the failed turn has no completed assistant group before the capacity error"
        );
        let original = runtime.failures[0].clone();
        let key = runtime.failure_ui.incidents[0].key.clone();
        original_failure_stays_before_resume(&mut runtime, &original, &key, false);
        for draft in [".", "Try the check again.", "Continue."] {
            runtime.replace_draft(text_editor::Content::with_text(draft));
            state.submit(&mut runtime);
            assert!(runtime.composer.text().trim().is_empty());
            original_failure_stays_before_resume(&mut runtime, &original, &key, true);
            while state.step.is_some() {
                state.tick(&mut runtime);
                original_failure_stays_before_resume(&mut runtime, &original, &key, true);
            }
            assert!(runtime.pending.is_none());
        }
        assert_eq!(
            runtime.history,
            [
                "Check the saved state.",
                ".",
                "Try the check again.",
                "Continue."
            ]
        );
        assert_eq!(
            runtime
                .snapshot
                .items
                .iter()
                .filter(|item| chat::failure::provider_error_text(Provider::Codex, item).is_some())
                .count(),
            1
        );
        assert_eq!(
            runtime
                .snapshot
                .items
                .iter()
                .filter(|item| matches!(
                    &item.content,
                    ItemContent::Tool {
                        status: chat::ToolStatus::Succeeded,
                        ..
                    }
                ))
                .count(),
            2
        );
        assert_eq!(runtime.snapshot.error, None);
        assert!(handle.proof().is_none());
        assert!(handle.send(Command::Refresh).is_err());
    }
}
