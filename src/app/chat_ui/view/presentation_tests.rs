//! Native presentation contracts, with real prepared content and local reducers.
//! No provider, portal, clipboard or asynchronous task is executed.
use super::*;
use iced::advanced::{
    clipboard,
    widget::{Operation, operation},
};
use iced::runtime::{UserInterface, user_interface::Cache};
use iced_accessibility::accesskit::Role;

const SOURCE: &str = "tool-source-opaque";
const PROMPT_END: &str = "Full prompt remains readable at the end.";

fn item(id: &str, content: ItemContent) -> chat::Item {
    chat::Item {
        id: id.into(),
        role: if matches!(content, ItemContent::Delivery { .. }) {
            chat::Role::User
        } else {
            chat::Role::Assistant
        },
        content,
        at: None,
        started_at: None,
    }
}

fn fixture(
    items: &[chat::Item],
    name: &str,
    status: chat::ToolStatus,
    waiting: bool,
) -> (Cagents, Target) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.context_inspector_open = false;
    drop(app.new_native_chat(Provider::Claude, Some(PathBuf::from("/project"))));
    let card = app.workspace.focused().unwrap();
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(900.0),
    }));
    let target = match app.cards.get(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime.target,
        _ => panic!("chat"),
    };
    let (mut runtime, handle) = frame_cost::live_runtime(items);
    runtime.target = target;
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents = vec![chat::Subagent {
        id: SOURCE.into(),
        kind: name.into(),
        description: "Internal task description is not identity".into(),
        model: Some("internal-model-label".into()),
        status,
        waiting,
        detail: String::new(),
        background: true,
        items: Vec::new(),
    }];
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    runtime.show(true);
    preparation::drain_fixture(&mut runtime);
    runtime.replace_draft(text_editor::Content::with_text(
        "Keep this selected λ draft",
    ));
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 9 },
            selection: Some(text_editor::Position { line: 0, column: 5 }),
        },
    );
    runtime
        .attachments
        .push(Attachment::file("preserved-attachment.txt"));
    app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
    (app, target)
}

fn runtime(app: &Cagents, target: Target) -> &Runtime {
    match app.cards.get(&target.card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => panic!("chat"),
    }
}

fn surface(app: &Cagents, target: Target, id: &str) -> Element<'static, Message> {
    let runtime = runtime(app, target);
    let source = runtime.active_item(id).expect("published source");
    let cached = runtime
        .presentation_cache()
        .get(id)
        .expect("prepared source");
    assert!(!cached.placeholder && !cached.preparing);
    let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE);
    if let Some(coordinated) = cached.coordination.as_deref() {
        // Every child element owns its presentation, as the retained turn does.
        runtime.coordination_card(source, cached, coordinated, settings, None)
    } else if matches!(source.content, ItemContent::Delivery { .. }) {
        runtime.delivery_view(source, cached, settings)
    } else {
        runtime.step_header(source, cached, runtime.item_expanded(id))
    }
}

#[derive(Default)]
struct Mounted {
    texts: Vec<String>,
    focused: Vec<widget::Id>,
}
impl Operation for Mounted {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
        self.texts.push(text.into());
    }
    fn custom(
        &mut self,
        _: Option<&widget::Id>,
        _: iced::Rectangle,
        state: &mut dyn std::any::Any,
    ) {
        if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
            self.texts.push(prose.plain.to_string());
        }
    }
    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        _: iced::Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if state.is_focused()
            && let Some(id) = id
        {
            self.focused.push(id.clone());
        }
    }
}

type Ui = UserInterface<'static, Message, cosmic::Theme, cosmic::Renderer>;

fn labels(ui: &mut Ui) -> Vec<(Role, String, iced::Rectangle)> {
    let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| {
            let label = node.node().label().or_else(|| node.node().value())?;
            let bounds = node.node().bounds()?;
            Some((
                node.node().role(),
                label.to_owned(),
                iced::Rectangle {
                    x: bounds.x0 as f32,
                    y: bounds.y0 as f32,
                    width: (bounds.x1 - bounds.x0) as f32,
                    height: (bounds.y1 - bounds.y0) as f32,
                },
            ))
        })
        .collect()
}

fn enter(ui: &mut Ui, renderer: &mut cosmic::Renderer, id: widget::Id) -> Action {
    ui.operate(renderer, &mut operation::focusable::focus(id.clone()));
    let mut probe = Mounted::default();
    ui.operate(renderer, &mut probe);
    assert_eq!(probe.focused, [id.clone()], "real native button focus");
    let mut messages = Vec::new();
    for event in [
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        },
        keyboard::Event::KeyReleased {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        },
    ] {
        ui.update(
            &[iced::Event::Keyboard(event)],
            iced::mouse::Cursor::Unavailable,
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
    }
    assert_eq!(
        messages.len(),
        1,
        "one native activation, no incidental dispatch"
    );
    let Message::ChatUi(action) = messages.pop().unwrap() else {
        panic!("chat action")
    };
    action
}

#[test]
fn named_agent_rows_are_compact_in_all_states_and_keyboard_details_show_the_entire_prompt() {
    let prompt = format!(
        "{}\n{PROMPT_END}",
        "Readable prompt line with Unicode λ.\n\n".repeat(32)
    );
    let source = item(SOURCE, ItemContent::Tool {
        name: "Agent".into(), input: serde_json::json!({"prompt":prompt,"name":"Launch handle",
            "description":"Internal task description is not identity", "subagent_type":"general-purpose",
            "model":"internal-launch-model", "run_in_background":true}).to_string(),
        output: "Async agent launched successfully. agentId: transport-only".into(),
        status: chat::ToolStatus::Succeeded,
    });
    let mut renderer = crate::app::tests::headless_renderer();
    for width in [260.0, 620.0, 900.0] {
        for name in ["Scout".to_owned(), "長い実際の名前 λ ".repeat(8)] {
            for (status, waiting) in [
                (chat::ToolStatus::Running, false),
                (chat::ToolStatus::Running, true),
                (chat::ToolStatus::Succeeded, false),
                (chat::ToolStatus::Failed, false),
                (chat::ToolStatus::Denied, false),
                (chat::ToolStatus::Stopped, false),
            ] {
                let (mut app, target) = fixture(&[source.clone()], &name, status, waiting);
                let before = {
                    let r = runtime(&app, target);
                    (
                        r.composer.text(),
                        r.composer.cursor(),
                        r.attachments.clone(),
                        r.revision,
                    )
                };
                let mut ui = UserInterface::build(
                    surface(&app, target, SOURCE),
                    iced::Size::new(width, 4000.0),
                    Cache::new(),
                    &mut renderer,
                );
                let mut mounted = Mounted::default();
                ui.operate(&renderer, &mut mounted);
                let visible = mounted.texts.join("\n");
                assert!(
                    visible.contains(name.trim()),
                    "actual reported name remains readable at {width}"
                );
                for hidden in [
                    PROMPT_END,
                    "Readable prompt line",
                    "general-purpose",
                    "internal-model-label",
                    "internal-launch-model",
                    "Internal task description",
                    "Native subagent",
                    "transport-only",
                ] {
                    assert!(
                        !visible.contains(hidden),
                        "default row exposes internal content: {hidden}"
                    );
                }
                let accessible = labels(&mut ui);
                let controls: Vec<_> = accessible
                    .iter()
                    .filter(|(role, _, _)| *role == Role::Button)
                    .collect();
                assert_eq!(
                    controls.len(),
                    2,
                    "one transcript action and one disclosure, no duplicate copy/avatar controls"
                );
                let hovered = controls.last().unwrap().2.center();
                let mut messages = Vec::new();
                ui.update(
                    &[iced::Event::Mouse(iced::mouse::Event::CursorMoved {
                        position: hovered,
                    })],
                    iced::mouse::Cursor::Available(hovered),
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert!(
                    messages.is_empty(),
                    "hover does not activate a disclosure or transcript"
                );
                for (_, _, bounds) in &controls {
                    assert!(
                        bounds.width > 0.0
                            && bounds.height > 0.0
                            && bounds.x >= 0.0
                            && bounds.x + bounds.width <= width + 1.0,
                        "narrow controls stay within the actual component: {bounds:?}"
                    );
                }
                if name == "Scout" {
                    assert!(
                        controls
                            .iter()
                            .all(|(_, _, bounds)| bounds.y + bounds.height < 100.0),
                        "default prompt must not consume a card-height preview"
                    );
                }
                if status != chat::ToolStatus::Succeeded || waiting {
                    continue;
                }
                let details = card_control_id(
                    target.card,
                    &format!("coordination-{}-{SOURCE}-details", target.generation),
                );
                let activation = enter(&mut ui, &mut renderer, details);
                assert!(
                    matches!(&activation, Action::Toggle(found, id) if *found == target && id == SOURCE)
                );
                let cache = ui.into_cache();
                assert_eq!(app.handle_chat_ui(activation).units(), 0);
                preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
                let mut open = UserInterface::build(
                    surface(&app, target, SOURCE),
                    iced::Size::new(width, 4000.0),
                    cache,
                    &mut renderer,
                );
                let mut mounted = Mounted::default();
                open.operate(&renderer, &mut mounted);
                assert!(
                    mounted.texts.iter().any(|text| text.contains(PROMPT_END)),
                    "full expanded native prose, not logical source-only text"
                );
                let exposed = labels(&mut open);
                assert!(
                    exposed
                        .iter()
                        .any(|(_, text, bounds)| text.contains(PROMPT_END)
                            && bounds.height > 0.0
                            && bounds.y > 180.0),
                    "expanded full prompt suffix has actual native geometry beyond the old faded cutoff"
                );
                let r = runtime(&app, target);
                assert_eq!(
                    (
                        r.composer.text(),
                        r.composer.cursor(),
                        r.attachments.clone(),
                        r.revision
                    ),
                    before
                );
                assert!(r.pending.is_none() && r.queued.is_none() && r.followups.is_empty());
            }
        }
    }
}

#[test]
fn reports_keep_distinct_sources_single_controls_and_stale_actions_cannot_change_replacement() {
    let body = "A real independent result.\n\nSecond paragraph remains exact.";
    let report = |id| {
        item(
            id,
            ItemContent::Delivery {
                kind: chat::DeliveryKind::Report,
                from: "Scout".into(),
                sender: "transport-only-identifier".into(),
                summary: "Finished review".into(),
                body: body.into(),
            },
        )
    };
    let (mut app, target) = fixture(
        &[report("report-one"), report("report-two")],
        "Scout",
        chat::ToolStatus::Succeeded,
        false,
    );
    let mut renderer = crate::app::tests::headless_renderer();
    for id in ["report-one", "report-two"] {
        let mut ui = UserInterface::build(
            surface(&app, target, id),
            iced::Size::new(260.0, 1000.0),
            Cache::new(),
            &mut renderer,
        );
        let nodes = labels(&mut ui);
        assert_eq!(
            nodes
                .iter()
                .filter(|(role, _, _)| *role == Role::Button)
                .count(),
            2
        );
        assert!(
            !nodes
                .iter()
                .any(|(_, text, _)| text.contains("transport-only"))
        );
        let copy = card_control_id(
            target.card,
            &format!("delivery-{}-{id}-copy", target.generation),
        );
        let activation = enter(&mut ui, &mut renderer, copy);
        assert!(
            matches!(activation, Action::Copy(found, ref source, CopyPart::Text) if found == target && source == id)
        );
        assert_eq!(
            copy_text(
                CopyPart::Text,
                &runtime(&app, target).active_item(id).unwrap().content
            )
            .as_deref(),
            Some(body)
        );
    }
    assert_eq!(
        runtime(&app, target).snapshot.items.len(),
        2,
        "identical authentic reports never deduplicate by text"
    );
    let (mut replacement, _) = frame_cost::live_runtime(&[report("report-one")]);
    replacement.target = Target {
        generation: target.generation + 1,
        ..target
    };
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(replacement)));
    assert_eq!(
        app.handle_chat_ui(Action::Toggle(target, "report-one".into()))
            .units(),
        0
    );
    let Some(CardRuntime::Chat(replacement)) = app.cards.get(&target.card) else {
        panic!("replacement")
    };
    assert!(!replacement.item_expanded("report-one"));
}

#[test]
fn skill_headers_name_prepared_exact_target_and_not_internal_arguments() {
    let mut renderer = crate::app::tests::headless_renderer();
    for status in [
        chat::ToolStatus::Running,
        chat::ToolStatus::Succeeded,
        chat::ToolStatus::Failed,
    ] {
        let source = item(
            "skill",
            ItemContent::Tool {
                name: "Skill".into(),
                input:
                    r#"{"skill":"writing-copy","args":"Internal framing must remain disclosed"}"#
                        .into(),
                output: if status == chat::ToolStatus::Failed {
                    "Permission denied".into()
                } else {
                    String::new()
                },
                status,
            },
        );
        let (app, target) = fixture(&[source], "", status, false);
        let mut ui = UserInterface::build(
            surface(&app, target, "skill"),
            iced::Size::new(260.0, 1000.0),
            Cache::new(),
            &mut renderer,
        );
        let nodes = labels(&mut ui);
        assert!(nodes.iter().any(|(role, text, _)| *role == Role::Button && text.contains("skill writing-copy")), "accessible action names the actual skill");
        let mut mounted = Mounted::default();
        ui.operate(&renderer, &mut mounted);
        let text = mounted.texts.join("\n");
        assert!(text.contains("writing-copy") && !text.contains("Internal framing"));
        if status == chat::ToolStatus::Failed {
            assert!(text.contains("Permission denied"));
        }
    }
}

#[test]
fn presentation_controls_keep_native_focus_and_held_presses_with_the_exact_runtime_generation() {
    let agent = item(
        SOURCE,
        ItemContent::Tool {
            name: "Agent".into(),
            input:
                r#"{"prompt":"Read the complete prompt","name":"Scout","run_in_background":true}"#
                    .into(),
            output: "Async agent launched successfully. agentId: transport-only".into(),
            status: chat::ToolStatus::Succeeded,
        },
    );
    let report = item(
        "report",
        ItemContent::Delivery {
            kind: chat::DeliveryKind::Report,
            from: "Scout".into(),
            sender: "transport-only".into(),
            summary: "Review finished".into(),
            body: "The exact authentic report.".into(),
        },
    );
    let skill = item(
        "skill",
        ItemContent::Tool {
            name: "Skill".into(),
            input: r#"{"skill":"writing-copy"}"#.into(),
            output: String::new(),
            status: chat::ToolStatus::Succeeded,
        },
    );
    let mut renderer = crate::app::tests::headless_renderer();
    for (source, role) in [(agent, "details"), (report, "copy"), (skill, "skill")] {
        for replace in [false, true] {
            let (mut app, target) = fixture(
                &[source.clone()],
                "Scout",
                chat::ToolStatus::Succeeded,
                false,
            );
            let control = |target: Target| {
                card_control_id(
                    target.card,
                    &match role {
                        "details" => {
                            format!("coordination-{}-{}-details", target.generation, source.id)
                        }
                        "copy" => format!("delivery-{}-{}-copy", target.generation, source.id),
                        _ => format!("chat-step-{}-{}", target.generation, source.id),
                    },
                )
            };
            let mut ui = UserInterface::build(
                surface(&app, target, &source.id),
                iced::Size::new(620.0, 1000.0),
                Cache::new(),
                &mut renderer,
            );
            let point = labels(&mut ui)
                .into_iter()
                .find(|(kind, label, bounds)| {
                    *kind == Role::Button
                        && bounds.width > 0.0
                        && bounds.height > 0.0
                        && match role {
                            "details" => label.starts_with("Show details"),
                            "copy" => label.starts_with("Copy original report"),
                            _ => label.contains("skill writing-copy"),
                        }
                })
                .expect("actual native control geometry")
                .2
                .center();
            let mut messages = Vec::new();
            ui.update(
                &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                    iced::mouse::Button::Left,
                ))],
                iced::mouse::Cursor::Available(point),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                messages.is_empty(),
                "a physical Down must not activate {role}"
            );
            ui.operate(&renderer, &mut operation::focusable::focus(control(target)));
            let mut focused = Mounted::default();
            ui.operate(&renderer, &mut focused);
            assert_eq!(focused.focused, [control(target)]);
            let cache = ui.into_cache();
            let next = if replace {
                Target {
                    generation: target.generation + 1,
                    ..target
                }
            } else {
                target
            };
            app.chat_runtime_mut(target).unwrap().target = next;
            // No intermediate frame: the replacement inherits the old cache
            // at the same hit point, which must not migrate a held press.
            let mut ui = UserInterface::build(
                surface(&app, next, &source.id),
                iced::Size::new(620.0, 1000.0),
                cache,
                &mut renderer,
            );
            let mut focused = Mounted::default();
            ui.operate(&renderer, &mut focused);
            assert_eq!(
                focused.focused,
                if replace {
                    vec![]
                } else {
                    vec![control(target)]
                }
            );
            ui.update(
                &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                ))],
                iced::mouse::Cursor::Available(point),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            if replace {
                assert!(
                    messages.is_empty(),
                    "old physical press cannot activate replacement {role}"
                );
            } else {
                assert_eq!(
                    messages.len(),
                    1,
                    "unchanged {role} preserves real release activation"
                );
                assert!(
                    match messages.pop().unwrap() {
                        Message::ChatUi(Action::Toggle(found, id)) if role != "copy" =>
                            found == target && id == source.id,
                        Message::ChatUi(Action::Copy(found, id, CopyPart::Text))
                            if role == "copy" =>
                            found == target && id == source.id,
                        _ => false,
                    },
                    "release retains the exact source callback"
                );
            }
            let r = runtime(&app, next);
            assert_eq!(r.composer.text(), "Keep this selected λ draft");
            assert_eq!(r.attachments.len(), 1);
            assert!(r.pending.is_none() && r.queued.is_none() && r.followups.is_empty());
        }
    }
}

#[test]
fn unnamed_failed_children_keep_actionable_details_and_source_identity_out_of_the_default_row() {
    let source = item(
        SOURCE,
        ItemContent::Tool {
            name: "Agent".into(),
            input: r#"{"prompt":"Private full prompt","run_in_background":true}"#.into(),
            output: "Async agent launched successfully. agentId: transport-only".into(),
            status: chat::ToolStatus::Succeeded,
        },
    );
    let (mut app, target) = fixture(&[source], "", chat::ToolStatus::Failed, false);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let detail = format!(
        "Permission denied while reading the requested file.\n{}\nFailure detail remains complete.",
        "Diagnostic context λ.\n".repeat(24)
    );
    Arc::make_mut(&mut runtime.snapshot).subagents[0].detail = detail.clone();
    let mut renderer = crate::app::tests::headless_renderer();
    let mut ui = UserInterface::build(
        surface(&app, target, SOURCE),
        iced::Size::new(260.0, 4000.0),
        Cache::new(),
        &mut renderer,
    );
    let mut mounted = Mounted::default();
    ui.operate(&renderer, &mut mounted);
    let visible = mounted.texts.join("\n");
    assert!(visible.contains("Agent") && visible.contains("Permission denied while reading"));
    assert!(
        !visible.contains(SOURCE)
            && !visible.contains("transport-only")
            && !visible.contains("Private full prompt")
    );
    assert!(!visible.contains("Failure detail remains complete."));
    let activation = enter(
        &mut ui,
        &mut renderer,
        card_control_id(
            target.card,
            &format!("coordination-{}-{SOURCE}-details", target.generation),
        ),
    );
    let cache = ui.into_cache();
    assert_eq!(app.handle_chat_ui(activation).units(), 0);
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    let mut ui = UserInterface::build(
        surface(&app, target, SOURCE),
        iced::Size::new(260.0, 4000.0),
        cache,
        &mut renderer,
    );
    let mut mounted = Mounted::default();
    ui.operate(&renderer, &mut mounted);
    assert!(
        mounted.texts.iter().any(|text| text.contains(&detail)),
        "expanded failure keeps all original diagnostic content"
    );
    assert!(
        mounted
            .texts
            .iter()
            .any(|text| text.contains("Private full prompt"))
    );
}

#[test]
fn completed_turn_status_qualifies_every_native_goal_state_without_faking_turn_activity() {
    use chat::goal::{Goal, ReadState, Status, Support};
    let (mut runtime, _) = frame_cost::live_runtime(&[]);
    for (status, label, glyph) in [
        (Status::Active, "Goal active", Some(Icon::AgentWorking)),
        (Status::Blocked, "Goal blocked", Some(Icon::Warning)),
        (
            Status::UsageLimited,
            "Goal usage limited",
            Some(Icon::Warning),
        ),
        (
            Status::BudgetLimited,
            "Goal budget limited",
            Some(Icon::Warning),
        ),
        (Status::Paused, "Goal paused", None),
        (Status::Complete, "Goal complete", Some(Icon::Check)),
    ] {
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.finished_at = Some(42);
        snapshot.goal = Some(chat::goal::Snapshot {
            thread_id: "original-thread".into(),
            revision: 1,
            support: Support::Supported,
            read: ReadState::Known(Some(Goal {
                thread_id: "original-thread".into(),
                objective: "Keep working on the actual goal".into(),
                status,
                token_budget: None,
                tokens_used: 12,
                time_used_seconds: 3,
                created_at: 1,
                updated_at: 2,
            })),
            pending: false,
            uncertain: false,
            last_result: None,
        });
        assert_eq!(
            runtime.conversation_status(false),
            (glyph, format!("{label} · Last turn finished"))
        );
        assert!(
            !runtime.snapshot.busy && !runtime.snapshot.is_working(),
            "Goal presentation must not fabricate a provider turn"
        );
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        assert_ne!(
            runtime.conversation_status(false).1,
            format!("{label} · Last turn finished"),
            "real turn work remains the leading status"
        );
        Arc::make_mut(&mut runtime.snapshot).busy = false;
        Arc::make_mut(&mut runtime.snapshot)
            .goal
            .as_mut()
            .unwrap()
            .pending = true;
        assert_eq!(
            runtime.conversation_status(false).1,
            "Goal updating · Last turn finished"
        );
        let goal = Arc::make_mut(&mut runtime.snapshot).goal.as_mut().unwrap();
        goal.pending = false;
        goal.uncertain = true;
        assert_eq!(
            runtime.conversation_status(false).1,
            "Goal unconfirmed · Last turn finished"
        );
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.goal.as_mut().unwrap().uncertain = false;
        snapshot.detail = "Turn failed".into();
        assert_eq!(
            runtime.conversation_status(false).1,
            format!("{label} · Last turn failed")
        );
        Arc::make_mut(&mut runtime.snapshot).detail.clear();
        runtime.disconnected = true;
        assert_eq!(runtime.conversation_status(false).1, "Disconnected");
        runtime.disconnected = false;
    }
    Arc::make_mut(&mut runtime.snapshot)
        .subagents
        .push(chat::Subagent {
            id: "failed-child".into(),
            kind: "Scout".into(),
            description: String::new(),
            status: chat::ToolStatus::Failed,
            model: None,
            waiting: false,
            detail: "Permission denied".into(),
            background: true,
            items: vec![],
        });
    assert_eq!(
        runtime.conversation_status(false),
        (
            Some(Icon::Warning),
            "Goal complete · Last turn finished · 1 failed".into()
        ),
        "Goal status preserves the independent child failure"
    );
    Arc::make_mut(&mut runtime.snapshot).subagents.clear();
    let goal = Arc::make_mut(&mut runtime.snapshot).goal.as_mut().unwrap();
    goal.read = ReadState::Known(None);
    assert_eq!(
        runtime.conversation_status(false).1,
        "Finished",
        "cleared goals do not invent an active goal"
    );
    Arc::make_mut(&mut runtime.snapshot)
        .goal
        .as_mut()
        .unwrap()
        .support = Support::Unknown;
    assert_eq!(
        runtime.conversation_status(false).1,
        "Goal unconfirmed · Last turn finished"
    );
    Arc::make_mut(&mut runtime.snapshot)
        .goal
        .as_mut()
        .unwrap()
        .support = Support::Unsupported;
    assert_eq!(
        runtime.conversation_status(false).1,
        "Finished",
        "unsupported native goals are not active goals"
    );
}
