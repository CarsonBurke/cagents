//! Actual native Goal controls and real app Wake; no provider Task is polled.
use super::*;
use chat::goal::{Goal, ReadState, Status, Support};
use iced::futures::{FutureExt, StreamExt};
use iced::{
    advanced::clipboard,
    keyboard, mouse,
    runtime::{UserInterface, user_interface::Cache},
};
const THREAD: &str = "original-goal-thread";

fn fixture(status: Status) -> (Cagents, Target) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.context_inspector_open = false;
    drop(app.new_native_chat(Provider::Codex, Some(PathBuf::from("/project"))));
    let card = app.workspace.focused().unwrap();
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(1600.0),
    }));
    let target = match app.cards.get(&card).unwrap() {
        CardRuntime::Chat(r) => r.target,
        _ => panic!(),
    };
    let old = app.chat_runtime_mut(target).unwrap();
    let target = old.target;
    let mut session = old.session.clone();
    session.launch.session_id = Some(THREAD.into());
    let proof = crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    };
    session.claim_owned_codex_listener(Path::new("/daemon-temp-root"), &proof);
    session.proof = Some(proof);
    let snapshot = Snapshot {
        ready: true,
        session_id: Some(THREAD.into()),
        goal: Some(chat::goal::Snapshot {
            thread_id: THREAD.into(),
            revision: 8,
            support: Support::Supported,
            read: ReadState::Known(Some(Goal {
                thread_id: THREAD.into(),
                objective: "Keep the original objective λ".into(),
                status,
                token_budget: Some(500),
                tokens_used: 20,
                time_used_seconds: 4,
                created_at: 1,
                updated_at: 2,
            })),
            pending: false,
            uncertain: false,
            last_result: None,
        }),
        ..Snapshot::default()
    };
    let handle = Arc::new(ClientHandle::fixture(session.clone(), snapshot));
    let mut runtime = Runtime::new(
        target,
        session.clone(),
        Connection::Live(handle),
        true,
        app.settings.editor_syntax_theme,
    );
    runtime.goal_open = true;
    runtime.replace_draft(text_editor::Content::with_text("Keep selected draft λ"));
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 4 },
            selection: Some(text_editor::Position { line: 0, column: 0 }),
        },
    );
    runtime
        .attachments
        .push(Attachment::file("preserved-reference.txt"));
    runtime.refresh();
    assert!(runtime.goal_controls().contains(&Control::Check.id(target)));
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(session);
    app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
    (app, target)
}
fn runtime(app: &Cagents, target: Target) -> &Runtime {
    match app.cards.get(&target.card).unwrap() {
        CardRuntime::Chat(r) => r,
        _ => panic!(),
    }
}
fn publish(app: &mut Cagents, target: Target, change: impl FnOnce(&mut Snapshot)) -> Task<Message> {
    let handle = runtime(app, target).handle().unwrap().clone();
    let mut next = (*handle.snapshot()).clone();
    next.generation += 1;
    change(&mut next);
    handle.publish_snapshot_for_test(next);
    app.handle_chat_ui(Action::Wake(target, Wake::Changed))
}
fn accounting(snapshot: &mut Snapshot) {
    let goal = snapshot.goal.as_mut().unwrap();
    goal.revision += 1;
    let ReadState::Known(Some(goal)) = &mut goal.read else {
        panic!()
    };
    goal.tokens_used += 3;
    goal.time_used_seconds += 1;
    goal.updated_at += 1;
}
fn input(app: &Cagents, target: Target) -> (String, text_editor::Cursor, Vec<Attachment>, u64) {
    let r = runtime(app, target);
    (
        r.composer.text(),
        r.composer.cursor(),
        r.attachments.clone(),
        r.revision,
    )
}
type Ui<'a> = UserInterface<'a, Message, cosmic::Theme, cosmic::Renderer>;
fn mount<'a>(
    app: &'a Cagents,
    target: Target,
    cache: Cache,
    renderer: &mut cosmic::Renderer,
    width: f32,
) -> Ui<'a> {
    UserInterface::build(
        runtime(app, target).goal_panel().unwrap(),
        iced::Size::new(width, 650.0),
        cache,
        renderer,
    )
}
#[derive(Default)]
struct Focus(Vec<Option<widget::Id>>);
impl WidgetOperation for Focus {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn WidgetOperation)) {
        visit(self);
    }
    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        _: iced::Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if state.is_focused() {
            self.0.push(id.cloned());
        }
    }
}
fn focused(ui: &mut Ui<'_>, renderer: &cosmic::Renderer) -> Vec<Option<widget::Id>> {
    let mut probe = Focus::default();
    ui.operate(renderer, &mut probe);
    probe.0
}
fn focus(ui: &mut Ui<'_>, renderer: &cosmic::Renderer, id: widget::Id) {
    ui.operate(renderer, &mut focusable::focus(id.clone()));
    assert_eq!(
        focused(ui, renderer),
        [Some(id)],
        "actual native focus, never picker_focus inference"
    );
}
fn apply(task: Task<Message>, ui: &mut Ui<'_>, renderer: &cosmic::Renderer) {
    assert_eq!(
        task.units(),
        1,
        "restoration is exactly one widget operation, no provider or future"
    );
    let mut stream = iced::runtime::task::into_stream(task).unwrap();
    while let Some(action) = stream
        .next()
        .now_or_never()
        .expect("widget task must be immediately ready")
    {
        let iced::runtime::Action::Widget(mut op) = action else {
            panic!("focus restoration must not issue a Goal RPC or app action")
        };
        let mut traversals = 0;
        loop {
            traversals += 1;
            assert!(traversals <= 4, "bounded native restore operation chain");
            ui.operate(renderer, op.as_mut());
            match op.finish() {
                Outcome::Chain(next) => op = next,
                Outcome::None => break,
                Outcome::Some(_) => panic!("focus operation cannot emit a Goal/UI action"),
            }
        }
    }
}
fn enter(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer) -> Vec<Message> {
    let mut messages = vec![];
    ui.update(
        &[iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })],
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    messages
}
fn pointer(
    ui: &mut Ui<'_>,
    renderer: &mut cosmic::Renderer,
    point: iced::Point,
    down: bool,
) -> Vec<Message> {
    let mut messages = vec![];
    ui.update(
        &[iced::Event::Mouse(if down {
            mouse::Event::ButtonPressed(mouse::Button::Left)
        } else {
            mouse::Event::ButtonReleased(mouse::Button::Left)
        })],
        mouse::Cursor::Available(point),
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    messages
}
fn hit(ui: &Ui<'_>, control: Control) -> iced::Point {
    let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let wanted = [
        "Check native goal",
        "Pause native goal",
        "Resume native goal",
        "Clear native goal",
    ][control.index()];
    let node = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|n| {
            n.node().role() == iced_accessibility::accesskit::Role::Button
                && n.node().label() == Some(wanted)
        })
        .unwrap();
    assert!(!node.node().is_disabled());
    let b = node.node().bounds().unwrap();
    assert!(b.x1 > b.x0 && b.y1 > b.y0);
    iced::Point::new(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32)
}

#[test]
fn accounting_wake_restores_exact_enabled_native_control_and_fresh_enter_uses_new_revision() {
    let mut renderer = crate::app::tests::headless_renderer();
    for width in [280.0, 640.0] {
        for control in [
            Control::Check,
            Control::Pause,
            Control::Resume,
            Control::Clear,
        ] {
            let (mut app, target) = fixture(if control == Control::Resume {
                Status::Paused
            } else {
                Status::Active
            });
            let before = input(&app, target);
            let mut ui = mount(&app, target, Cache::new(), &mut renderer, width);
            focus(&mut ui, &renderer, control.id(target));
            let mut old = enter(&mut ui, &mut renderer);
            assert!(
                matches!(old.as_slice(), [Message::ChatUi(Action::GoalIntent(t, _))] if *t == target)
            );
            let cache = ui.into_cache();
            let task = publish(&mut app, target, accounting);
            let mut ui = mount(&app, target, cache, &mut renderer, width);
            assert!(
                focused(&mut ui, &renderer).is_empty(),
                "revision Owned really remounted native state"
            );
            apply(task, &mut ui, &renderer);
            assert_eq!(focused(&mut ui, &renderer), [Some(control.id(target))]);
            let mut fresh = enter(&mut ui, &mut renderer);
            assert!(
                matches!(fresh.as_slice(), [Message::ChatUi(Action::GoalIntent(t, _))] if *t == target)
            );
            drop(ui);
            assert_eq!(input(&app, target), before);
            assert!(runtime(&app, target).goal_pending.is_none());
            let Message::ChatUi(old) = old.pop().unwrap() else {
                panic!()
            };
            assert_eq!(
                app.handle_chat_ui(old).units(),
                0,
                "queued old intent still fails the revision fence"
            );
            assert!(runtime(&app, target).goal_pending.is_none());
            let Message::ChatUi(fresh) = fresh.pop().unwrap() else {
                panic!()
            };
            let accepted = app.handle_chat_ui(fresh);
            assert!(
                accepted.units() > 0 && runtime(&app, target).goal_pending.is_some(),
                "fresh native Enter is accepted against the new observation"
            );
            drop(accepted); // explicit user operation is never polled in this fixture
        }
    }
}

#[test]
fn accounting_restore_cancels_held_native_press_but_unchanged_observation_keeps_release() {
    let mut renderer = crate::app::tests::headless_renderer();
    for changed in [false, true] {
        let (mut app, target) = fixture(Status::Active);
        let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
        let point = hit(&ui, Control::Pause);
        assert!(pointer(&mut ui, &mut renderer, point, true).is_empty());
        // A focus operation changes only is_focused in the pinned button; it
        // leaves the actual held physical press intact until replacement.
        focus(&mut ui, &renderer, Control::Pause.id(target));
        let cache = ui.into_cache();
        let task = changed.then(|| publish(&mut app, target, accounting));
        let mut ui = mount(&app, target, cache, &mut renderer, 360.0);
        if let Some(task) = task {
            apply(task, &mut ui, &renderer);
        }
        let released = pointer(&mut ui, &mut renderer, point, false);
        if changed {
            assert!(
                released.is_empty(),
                "restored focus cannot restore the old pressed state"
            );
        } else {
            assert!(
                matches!(released.as_slice(), [Message::ChatUi(Action::GoalIntent(t, _))] if *t == target)
            );
        }
        assert_eq!(
            focused(&mut ui, &renderer),
            [Some(Control::Pause.id(target))]
        );
        assert!(
            matches!(enter(&mut ui, &mut renderer).as_slice(), [Message::ChatUi(Action::GoalIntent(t, _))] if *t == target)
        );
    }
}

#[test]
fn queued_focus_restore_cannot_survive_an_unavailable_epoch_or_reset_marker_identity() {
    let mut renderer = crate::app::tests::headless_renderer();
    for invalid in [
        "pending",
        "uncertain",
        "unsupported",
        "counter-reset",
        "budget",
        "objective",
        "created",
        "last-result",
    ] {
        let (mut app, target) = fixture(Status::Active);
        let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
        focus(&mut ui, &renderer, Control::Pause.id(target));
        let cache = ui.into_cache();
        let old_restore = publish(&mut app, target, accounting);
        let mut ui = mount(&app, target, cache, &mut renderer, 360.0);
        let cache = ui.into_cache();
        let valid = (*runtime(&app, target).handle().unwrap().snapshot()).clone();
        drop(publish(&mut app, target, |snapshot| {
            let state = snapshot.goal.as_mut().unwrap();
            state.revision += 1;
            match invalid {
                "pending" => state.pending = true,
                "uncertain" => state.uncertain = true,
                "unsupported" => state.support = Support::Unsupported,
                "last-result" => {
                    state.last_result = Some(chat::goal::Completion {
                        revision: 99,
                        outcome: chat::goal::Outcome::Confirmed,
                    })
                }
                _ => {
                    let ReadState::Known(Some(goal)) = &mut state.read else {
                        panic!()
                    };
                    match invalid {
                        "counter-reset" => goal.tokens_used = 1,
                        "budget" => goal.token_budget = Some(999),
                        "objective" => goal.objective = "Replacement objective".into(),
                        "created" => goal.created_at += 1,
                        _ => unreachable!(),
                    }
                }
            }
        }));
        let ui = mount(&app, target, cache, &mut renderer, 360.0); // invalid render retires old markers
        let cache = ui.into_cache();
        drop(publish(&mut app, target, |snapshot| {
            let revision = snapshot.goal.as_ref().unwrap().revision + 1;
            snapshot.goal = valid.goal.clone();
            snapshot.goal.as_mut().unwrap().revision = revision;
        }));
        let mut ui = mount(&app, target, cache, &mut renderer, 360.0);
        apply(old_restore, &mut ui, &renderer);
        assert!(
            focused(&mut ui, &renderer).is_empty(),
            "old restore cannot focus re-enabled {invalid} controls"
        );
        focus(&mut ui, &renderer, Control::Pause.id(target));
        let cache = ui.into_cache();
        let fresh = publish(&mut app, target, accounting);
        let mut ui = mount(&app, target, cache, &mut renderer, 360.0);
        apply(fresh, &mut ui, &renderer);
        assert_eq!(
            focused(&mut ui, &renderer),
            [Some(Control::Pause.id(target))],
            "fresh valid witness remains usable after {invalid}"
        );
    }
}

#[test]
fn restoration_preserves_named_and_anonymous_foreign_native_focus_and_requires_a_witness() {
    let mut renderer = crate::app::tests::headless_renderer();
    for anonymous in [false, true] {
        let (mut app, target) = fixture(Status::Active);
        assert_eq!(
            publish(&mut app, target, accounting).units(),
            0,
            "unfocused Goal must not invent a restoration"
        );
        let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
        focus(&mut ui, &renderer, Control::Pause.id(target));
        let cache = ui.into_cache();
        let task = publish(&mut app, target, accounting);
        let foreign = text_editor::Content::with_text("Foreign native editor");
        let field = widget::text_editor(&foreign);
        let field: Element<'_, Message> = if anonymous {
            field.into()
        } else {
            field.id(widget::Id::new("foreign-native-editor")).into()
        };
        let mut ui = UserInterface::build(
            widget::column::with_children(vec![runtime(&app, target).goal_panel().unwrap(), field])
                .into(),
            iced::Size::new(640.0, 900.0),
            cache,
            &mut renderer,
        );
        if anonymous {
            struct Anonymous;
            impl WidgetOperation for Anonymous {
                fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn WidgetOperation)) {
                    visit(self);
                }
                fn focusable(
                    &mut self,
                    id: Option<&widget::Id>,
                    _: iced::Rectangle,
                    state: &mut dyn operation::Focusable,
                ) {
                    if id.is_none() {
                        state.focus();
                    } else {
                        state.unfocus();
                    }
                }
            }
            ui.operate(&renderer, &mut Anonymous);
            assert_eq!(
                focused(&mut ui, &renderer),
                [None],
                "real anonymous native editor owns focus"
            );
        } else {
            focus(&mut ui, &renderer, widget::Id::new("foreign-native-editor"));
        }
        let before = focused(&mut ui, &renderer);
        apply(task, &mut ui, &renderer);
        assert_eq!(
            focused(&mut ui, &renderer),
            before,
            "accounting never steals an existing foreign focus owner"
        );
    }
}

#[test]
fn queued_restore_never_targets_replacement_actor_target_thread_or_a_disabled_action() {
    let mut renderer = crate::app::tests::headless_renderer();
    for replacement in ["target", "actor", "thread", "disabled", "not-ready"] {
        let (mut app, target) = fixture(Status::Active);
        let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
        focus(&mut ui, &renderer, Control::Pause.id(target));
        let cache = ui.into_cache();
        let restore = publish(&mut app, target, accounting);
        let mut next_target = target;
        match replacement {
            "target" => {
                next_target.generation += 1;
                app.chat_runtime_mut(target).unwrap().target = next_target;
            }
            "actor" | "thread" => {
                let r = app.chat_runtime_mut(target).unwrap();
                let mut session = r.session.clone();
                let mut snapshot = (*r.handle().unwrap().snapshot()).clone();
                if replacement == "thread" {
                    session.launch.session_id = Some("replacement-thread".into());
                    snapshot.session_id = Some("replacement-thread".into());
                    let state = snapshot.goal.as_mut().unwrap();
                    state.thread_id = "replacement-thread".into();
                    let ReadState::Known(Some(goal)) = &mut state.read else {
                        panic!()
                    };
                    goal.thread_id = "replacement-thread".into();
                }
                r.session = session.clone();
                r.snapshot = Arc::new(snapshot.clone());
                r.connection = Connection::Live(Arc::new(ClientHandle::fixture(session, snapshot)));
            }
            "disabled" => {
                drop(publish(&mut app, target, |snapshot| {
                    let state = snapshot.goal.as_mut().unwrap();
                    state.revision += 1;
                    let ReadState::Known(Some(goal)) = &mut state.read else {
                        panic!()
                    };
                    goal.status = Status::Paused;
                }));
            }
            "not-ready" => {
                drop(publish(&mut app, target, |snapshot| {
                    snapshot.read_only_reason = Some("New work is unavailable".into())
                }));
            }
            _ => unreachable!(),
        }
        let mut ui = mount(&app, next_target, cache, &mut renderer, 360.0);
        apply(restore, &mut ui, &renderer);
        assert!(
            focused(&mut ui, &renderer).is_empty(),
            "old delayed operation cannot focus {replacement} replacement"
        );
    }
}

#[test]
fn counter_regression_and_hidden_or_closed_panels_retire_observation_ownership() {
    let mut renderer = crate::app::tests::headless_renderer();
    for counter in ["tokens", "time", "updated"] {
        let (mut app, target) = fixture(Status::Active);
        let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
        focus(&mut ui, &renderer, Control::Pause.id(target));
        drop(ui);
        let task = publish(&mut app, target, |snapshot| {
            let state = snapshot.goal.as_mut().unwrap();
            state.revision += 1;
            let ReadState::Known(Some(goal)) = &mut state.read else {
                panic!()
            };
            match counter {
                "tokens" => goal.tokens_used -= 1,
                "time" => goal.time_used_seconds -= 1,
                _ => goal.updated_at -= 1,
            }
        });
        assert_eq!(
            task.units(),
            0,
            "decreased {counter} is not an accounting continuation"
        );
    }
    for close in [false, true] {
        let (mut app, target) = fixture(Status::Active);
        let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
        focus(&mut ui, &renderer, Control::Pause.id(target));
        drop(ui);
        let r = runtime(&app, target);
        assert!(r.goal_focus.state.lock().unwrap().witness.is_some());
        let witness = r.goal_focus.state.lock().unwrap().witness.clone().unwrap();
        let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
        focus(&mut ui, &renderer, Control::Pause.id(target));
        drop(ui);
        let repeated = r.goal_focus.state.lock().unwrap().witness.clone().unwrap();
        assert_eq!(witness.control, repeated.control);
        assert_eq!(witness.scope.markers, repeated.scope.markers);
        assert_eq!(witness.scope.source.goal, repeated.scope.source.goal);
        if close {
            drop(app.handle_chat_ui(Action::GoalPanel(target, false)));
        } else {
            app.chat_runtime_mut(target)
                .unwrap()
                .set_visible(false)
                .unwrap();
        }
        let state = runtime(&app, target).goal_focus.state.lock().unwrap();
        assert!(state.scope.is_none() && state.witness.is_none());
    }
}

#[test]
fn queued_restore_with_no_mounted_goal_destination_does_not_invent_focus() {
    let (mut app, target) = fixture(Status::Active);
    let before = input(&app, target);
    let mut renderer = crate::app::tests::headless_renderer();
    let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
    focus(&mut ui, &renderer, Control::Pause.id(target));
    drop(ui);
    let restore = publish(&mut app, target, accounting);
    // Close through the real reducer; deliberately do not apply its explicit
    // composer focus task, to exercise a globally unfocused replacement tree.
    drop(app.handle_chat_ui(Action::GoalPanel(target, false)));
    let mut ui = UserInterface::build(
        app.chat_view(target.card),
        iced::Size::new(640.0, 900.0),
        Cache::new(),
        &mut renderer,
    );
    assert!(focused(&mut ui, &renderer).is_empty());
    apply(restore, &mut ui, &renderer);
    assert!(
        focused(&mut ui, &renderer).is_empty(),
        "absent exact destination cannot blur/focus any replacement control"
    );
    drop(ui);
    assert_eq!(input(&app, target), before);
}

#[test]
fn accounting_publication_before_wake_preserves_only_evidence_across_disabled_native_render() {
    let mut renderer = crate::app::tests::headless_renderer();
    for control in [Control::Pause, Control::Clear] {
        for publication in ["accounting", "pending", "status", "counter-reset"] {
            let (mut app, target) = fixture(Status::Active);
            let before = input(&app, target);
            let mut ui = mount(&app, target, Cache::new(), &mut renderer, 360.0);
            focus(&mut ui, &renderer, control.id(target));
            // Obtain the real queued command before holding the physical press;
            // no keyboard event occurs between Down and the replacement frame.
            let mut stale = enter(&mut ui, &mut renderer);
            assert!(
                matches!(stale.as_slice(), [Message::ChatUi(Action::GoalIntent(t, _))] if *t == target)
            );
            let point = hit(&ui, control);
            assert!(pointer(&mut ui, &mut renderer, point, true).is_empty());
            let marker = runtime(&app, target)
                .goal_focus
                .state
                .lock()
                .unwrap()
                .witness
                .as_ref()
                .unwrap()
                .scope
                .markers[control.index()]
            .clone();
            let cache = ui.into_cache();
            let handle = runtime(&app, target).handle().unwrap().clone();
            let mut next = (*handle.snapshot()).clone();
            next.generation += 1;
            accounting(&mut next);
            let state = next.goal.as_mut().unwrap();
            match publication {
                "pending" => state.pending = true,
                "status" | "counter-reset" => {
                    let ReadState::Known(Some(goal)) = &mut state.read else {
                        panic!()
                    };
                    if publication == "status" {
                        goal.status = Status::Paused;
                    } else {
                        goal.tokens_used = 1;
                    }
                }
                _ => {}
            }
            // Deliberately publish without Wake, then perform an unrelated
            // native rebuild using the stale Runtime observation.
            handle.publish_snapshot_for_test(next);
            let mut ui = mount(&app, target, cache, &mut renderer, 360.0);
            assert_eq!(
                runtime(&app, target).goal_controls(),
                [card_control_id(target.card, "chat-goal-close")],
                "lag never enables a stale command"
            );
            assert!(
                focused(&mut ui, &renderer).is_empty(),
                "current=false Owned remount cancels old focus/press"
            );
            let mut query = operation::scoped(marker, focusable::count());
            ui.operate(&renderer, &mut operation::black_box(&mut query));
            let Outcome::Some(count) = query.finish() else {
                panic!("scoped query must settle")
            };
            assert_eq!(count.total, 0, "lag mounts no enabled destination marker");
            assert!(enter(&mut ui, &mut renderer).is_empty());
            assert!(
                pointer(&mut ui, &mut renderer, point, false).is_empty(),
                "held old press cannot activate disabled lag controls"
            );
            assert_eq!(
                runtime(&app, target)
                    .goal_focus
                    .state
                    .lock()
                    .unwrap()
                    .witness
                    .is_some(),
                publication == "accounting"
            );
            let cache = ui.into_cache();
            let restore = app.handle_chat_ui(Action::Wake(target, Wake::Changed));
            let mut ui = mount(&app, target, cache, &mut renderer, 360.0);
            let mut fresh = if publication == "accounting" {
                apply(restore, &mut ui, &renderer);
                assert_eq!(focused(&mut ui, &renderer), [Some(control.id(target))]);
                enter(&mut ui, &mut renderer)
            } else {
                assert_eq!(
                    restore.units(),
                    0,
                    "non-accounting {publication} retires evidence"
                );
                assert!(focused(&mut ui, &renderer).is_empty());
                vec![]
            };
            drop(ui);
            assert_eq!(input(&app, target), before);
            let Message::ChatUi(stale) = stale.pop().unwrap() else {
                panic!()
            };
            assert_eq!(
                app.handle_chat_ui(stale).units(),
                0,
                "old queued native intent remains revision-fenced"
            );
            assert!(runtime(&app, target).goal_pending.is_none());
            if publication == "accounting" {
                assert!(
                    matches!(fresh.as_slice(), [Message::ChatUi(Action::GoalIntent(t, _))] if *t == target)
                );
                let Message::ChatUi(fresh) = fresh.pop().unwrap() else {
                    panic!()
                };
                let task = app.handle_chat_ui(fresh);
                assert!(
                    task.units() > 0 && runtime(&app, target).goal_pending.is_some(),
                    "fresh native Enter uses latest revision after lag"
                );
                drop(task); // never poll a provider operation
            }
        }
    }
}
