//! Native goal presentation only. The parent owns actor/Target admission and
//! must compare the captured observation revision again when handling an intent.
use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use crate::{
    chat::goal::{Operation, ReadState, Status, Support},
    design::{self, space},
    ui_controls::{self, button},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Intent {
    pub operation: Operation,
    /// The actor's local observation epoch, never the SDK's updatedAt field.
    pub observation_revision: u64,
}

#[derive(Clone, Copy)]
pub(super) struct Presentation<'a> {
    pub support: Support,
    pub read: &'a ReadState,
    pub pending: bool,
    pub uncertain: bool,
    /// Exact current actor/thread/context, established by the caller.
    pub current: bool,
    /// Read/stop ownership does not by itself authorize new goal work.
    pub work_ready: bool,
    pub observation_revision: u64,
}

impl Presentation<'_> {
    pub(super) fn control_enabled(self, operation: &Operation) -> bool {
        let goal = match self.read {
            ReadState::Known(Some(goal)) => Some(goal),
            _ => None,
        };
        let free = self.current && !self.pending;
        let confirmed = free && self.support == Support::Supported && !self.uncertain;
        match operation {
            Operation::Get => free && self.support != Support::Unsupported,
            Operation::Pause => confirmed && goal.is_some_and(|goal| goal.status == Status::Active),
            Operation::Resume => {
                confirmed
                    && self.work_ready
                    && goal.is_some_and(|goal| {
                        matches!(
                            goal.status,
                            Status::Paused
                                | Status::Blocked
                                | Status::UsageLimited
                                | Status::BudgetLimited
                        )
                    })
            }
            Operation::Clear => confirmed && goal.is_some(),
            Operation::SetObjective(_) => false,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ControlsKey {
    subject: Option<(String, String, Status)>,
    support: Support,
    pending: bool,
    uncertain: bool,
    current: bool,
    work_ready: bool,
    revision: u64,
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Active => "Active",
        Status::Paused => "Paused",
        Status::Blocked => "Blocked",
        Status::UsageLimited => "Usage limited",
        Status::BudgetLimited => "Budget limited",
        Status::Complete => "Complete",
    }
}

/// `on_action` constructs messages; it must not perform provider work itself.
/// Wrap this panel in the parent's exact Target owner as well: local widget
/// ownership prevents retained gestures migrating, not already-queued messages.
pub(super) fn panel<'a, Message: Clone + 'a>(
    state: Presentation<'a>,
    on_action: impl Fn(Intent) -> Message,
) -> Element<'a, Message> {
    panel_with_ids(
        state,
        std::array::from_fn(|_| widget::Id::unique()),
        on_action,
    )
}

pub(super) fn panel_with_ids<'a, Message: Clone + 'a>(
    state: Presentation<'a>,
    ids: [widget::Id; 4],
    on_action: impl Fn(Intent) -> Message,
) -> Element<'a, Message> {
    panel_with_focus(state, ids, None, on_action)
}

pub(super) fn panel_with_focus<'a, Message: Clone + 'a>(
    state: Presentation<'a>,
    ids: [widget::Id; 4],
    focus: Option<super::goal_focus::Controls>,
    on_action: impl Fn(Intent) -> Message,
) -> Element<'a, Message> {
    let goal = match state.read {
        ReadState::Known(Some(goal)) => Some(goal),
        _ => None,
    };
    let confirmed =
        state.current && state.support == Support::Supported && !state.uncertain && !state.pending;
    let check = state.control_enabled(&Operation::Get);
    let change = state.control_enabled(&Operation::Clear);
    let intent = |operation| Intent {
        operation,
        observation_revision: state.observation_revision,
    };
    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    if let Some(goal) = goal {
        // The human objective is the panel's primary heading. Status and
        // accounting qualify it rather than competing as repeated titles.
        rows.push(
            widget::container(
                widget::scrollable(
                    design::heading(goal.objective.as_str())
                        .width(Length::Fill)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                )
                .height(Length::Shrink)
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                )),
            )
            .width(Length::Fill)
            .max_height(96.0)
            .into(),
        );
        rows.push(
            design::caption(format!(
                "{} · {}",
                if confirmed {
                    "Goal"
                } else {
                    "Last observed goal"
                },
                status_label(goal.status)
            ))
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .into(),
        );
        let mut accounting = format!(
            "Tokens used: {} · Time used: {}s",
            goal.tokens_used, goal.time_used_seconds
        );
        if let Some(budget) = goal.token_budget {
            accounting.push_str(&format!(" · Token budget: {budget}"));
        }
        rows.push(
            design::caption(accounting)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    } else if matches!(state.read, ReadState::Known(None)) {
        rows.push(
            design::caption(if confirmed {
                "No native goal is set for this conversation."
            } else {
                "The last check found no native goal; the current goal is unconfirmed."
            })
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::Word)
            .into(),
        );
    } else if state.support != Support::Unsupported {
        rows.push(design::caption("The current goal has not been read.").into());
    }
    match state.support {
        Support::Unsupported => rows.push(
            design::caption("Native goal controls are not supported by this conversation.")
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        ),
        Support::Unknown => rows.push(
            design::caption("Native goal support has not been checked for this conversation.")
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        ),
        Support::Supported => {}
    }
    if state.pending {
        rows.push(
            design::caption("Goal request awaiting provider confirmation.")
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    } else if state.uncertain {
        rows.push(
            design::caption("Current goal is unconfirmed. Check before changing it.")
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    }
    if !state.current && !state.pending && !state.uncertain && state.support != Support::Unsupported
    {
        rows.push(
            design::caption("The current goal is unconfirmed for this conversation. Goal controls are unavailable.")
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    }
    let pause = state.control_enabled(&Operation::Pause);
    let resume = state.control_enabled(&Operation::Resume);
    if confirmed
        && !state.work_ready
        && goal.is_some_and(|goal| {
            matches!(
                goal.status,
                Status::Paused | Status::Blocked | Status::UsageLimited | Status::BudgetLimited
            )
        })
    {
        rows.push(
            design::caption("Resume is unavailable because this conversation is not ready for new work. Check and Clear remain available.")
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
        );
    }
    let mark = |index: usize, content: Element<'a, Message>| {
        if let Some(focus) = &focus
            && [check, pause, resume, change][index]
        {
            widget::container(content)
                .id(focus.marker(index))
                .width(Length::Fill)
                .into()
        } else {
            content
        }
    };
    let control_row = |row: usize, children: Vec<Element<'a, Message>>| -> Element<'a, Message> {
        if let Some(focus) = focus.clone() {
            widget::container(crate::interaction::keyed_control_row(
                children
                    .into_iter()
                    .enumerate()
                    .map(|(index, child)| {
                        (
                            super::goal_focus::Control::from_index(row * 2 + index).key(),
                            child,
                        )
                    })
                    .collect(),
                space::S,
                move |key| focus.observe(row, key),
            ))
            .width(Length::Fill)
            .into()
        } else {
            widget::row::with_children(children)
                .width(Length::Fill)
                .spacing(space::S)
                .into()
        }
    };
    let controls = widget::column::with_children(vec![
        control_row(
            0,
            vec![
                mark(
                    0,
                    button::secondary("Check")
                        .id(ids[0].clone())
                        .name("Check native goal")
                        .width(Length::Fill)
                        .on_press_maybe(check.then(|| on_action(intent(Operation::Get))))
                        .into(),
                ),
                mark(
                    1,
                    button::secondary("Pause")
                        .id(ids[1].clone())
                        .name("Pause native goal")
                        .width(Length::Fill)
                        .on_press_maybe(pause.then(|| on_action(intent(Operation::Pause))))
                        .into(),
                ),
            ],
        ),
        control_row(
            1,
            vec![
                mark(
                    2,
                    button::secondary("Resume")
                        .id(ids[2].clone())
                        .name("Resume native goal")
                        .width(Length::Fill)
                        .on_press_maybe(resume.then(|| on_action(intent(Operation::Resume))))
                        .into(),
                ),
                mark(
                    3,
                    button::secondary("Clear")
                        .id(ids[3].clone())
                        .name("Clear native goal")
                        .width(Length::Fill)
                        .on_press_maybe(change.then(|| on_action(intent(Operation::Clear))))
                        .into(),
                ),
            ],
        ),
    ])
    .width(Length::Fill)
    .spacing(space::XS);
    // Keep the control owner in a fixed slot. Inserting a status caption before
    // it would recreate the owner and restore old named button state before
    // the new owner can compare its action key.
    let presentation = widget::column::with_children(rows)
        .width(Length::Fill)
        .spacing(space::XS);
    let mut rows: Vec<Element<'a, Message>> = vec![
        presentation.into(),
        ui_controls::owned(
            ControlsKey {
                subject: goal
                    .map(|goal| (goal.thread_id.clone(), goal.objective.clone(), goal.status)),
                support: state.support,
                pending: state.pending,
                uncertain: state.uncertain,
                current: state.current,
                work_ready: state.work_ready,
                revision: state.observation_revision,
            },
            controls,
        ),
    ];
    if state.support != Support::Unsupported {
        rows.push(
            design::caption("Change the objective with /goal set <objective> in the composer. Pausing or clearing a goal does not stop the current turn.")
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    }
    widget::column::with_children(rows)
        .width(Length::Fill)
        .spacing(space::XS)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::goal::Goal;
    use iced::{advanced::clipboard, mouse, runtime::user_interface::Cache};

    type Ui<'a> = iced::runtime::UserInterface<'a, Intent, cosmic::Theme, cosmic::Renderer>;

    fn goal(objective: &str, status: Status) -> ReadState {
        ReadState::Known(Some(Goal {
            thread_id: "original-thread".into(),
            objective: objective.into(),
            status,
            token_budget: None,
            tokens_used: 123,
            time_used_seconds: 7,
            created_at: 1,
            updated_at: 2,
        }))
    }

    fn presentation(read: &ReadState) -> Presentation<'_> {
        Presentation {
            support: Support::Supported,
            read,
            pending: false,
            uncertain: false,
            current: true,
            work_ready: true,
            observation_revision: 17,
        }
    }

    fn cached<'a>(
        state: Presentation<'a>,
        cache: Cache,
        renderer: &mut cosmic::Renderer,
        width: f32,
    ) -> Ui<'a> {
        let mut ui = iced::runtime::UserInterface::build(
            panel(state, |intent| intent),
            iced::Size::new(width, 650.0),
            cache,
            renderer,
        );
        ui.draw(
            renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        ui
    }

    fn bounds(ui: &Ui<'_>, name: &str) -> iced::Rectangle {
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let found = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(name))
            .unwrap_or_else(|| panic!("missing actual goal control: {name}"));
        let rect = found.node().bounds().unwrap();
        iced::Rectangle {
            x: rect.x0 as f32,
            y: rect.y0 as f32,
            width: (rect.x1 - rect.x0) as f32,
            height: (rect.y1 - rect.y0) as f32,
        }
    }

    fn events(
        ui: &mut Ui<'_>,
        renderer: &mut cosmic::Renderer,
        point: iced::Point,
        buttons: &[mouse::Event],
    ) -> Vec<Intent> {
        let events = std::iter::once(iced::Event::Mouse(mouse::Event::CursorMoved {
            position: point,
        }))
        .chain(buttons.iter().cloned().map(iced::Event::Mouse))
        .collect::<Vec<_>>();
        let mut messages = Vec::new();
        let _ = ui.update(
            &events,
            mouse::Cursor::Available(point),
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
    }

    fn click(ui: &mut Ui<'_>, renderer: &mut cosmic::Renderer, name: &str) -> Vec<Intent> {
        let point = bounds(ui, name).center();
        events(
            ui,
            renderer,
            point,
            &[
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ],
        )
    }

    #[test]
    fn actual_controls_require_confirmed_free_current_goal_and_capture_observation_revision() {
        let active = goal("Keep the original objective", Status::Active);
        let paused = goal("Keep the original objective", Status::Paused);
        let empty = ReadState::Known(None);
        let unknown = ReadState::Unknown;
        let mut renderer = ui_controls::accessibility_probe::headless_renderer();
        let base = presentation(&active);
        for (state, expected) in [
            (
                base,
                vec![Operation::Get, Operation::Pause, Operation::Clear],
            ),
            (
                presentation(&paused),
                vec![Operation::Get, Operation::Resume, Operation::Clear],
            ),
            (
                Presentation {
                    work_ready: false,
                    ..presentation(&paused)
                },
                vec![Operation::Get, Operation::Clear],
            ),
            (
                Presentation {
                    work_ready: false,
                    ..base
                },
                vec![Operation::Get, Operation::Pause, Operation::Clear],
            ),
            (presentation(&empty), vec![Operation::Get]),
            (
                Presentation {
                    support: Support::Unknown,
                    ..presentation(&unknown)
                },
                vec![Operation::Get],
            ),
            (
                Presentation {
                    uncertain: true,
                    ..base
                },
                vec![Operation::Get],
            ),
            (
                Presentation {
                    pending: true,
                    ..base
                },
                vec![],
            ),
            (
                Presentation {
                    current: false,
                    ..base
                },
                vec![],
            ),
            (
                Presentation {
                    support: Support::Unsupported,
                    ..base
                },
                vec![],
            ),
        ] {
            let mut ui = cached(state, Cache::new(), &mut renderer, 280.0);
            for (name, operation) in [
                ("Check native goal", Operation::Get),
                ("Pause native goal", Operation::Pause),
                ("Resume native goal", Operation::Resume),
                ("Clear native goal", Operation::Clear),
            ] {
                let messages = click(&mut ui, &mut renderer, name);
                assert_eq!(
                    messages,
                    if expected.contains(&operation) {
                        vec![Intent {
                            operation,
                            observation_revision: 17,
                        }]
                    } else {
                        vec![]
                    }
                );
            }
        }
    }

    #[test]
    fn held_goal_control_cannot_migrate_to_a_replacement_but_same_observation_remains_interactive()
    {
        let original = goal("Goal A", Status::Active);
        let replacement = goal("Goal B", Status::Active);
        let mut renderer = ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached(presentation(&original), Cache::new(), &mut renderer, 280.0);
        let point = bounds(&ui, "Clear native goal").center();
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonPressed(mouse::Button::Left)]
            )
            .is_empty()
        );
        let mut ui = cached(
            Presentation {
                observation_revision: 18,
                ..presentation(&replacement)
            },
            ui.into_cache(),
            &mut renderer,
            280.0,
        );
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonReleased(mouse::Button::Left)]
            )
            .is_empty()
        );
        assert_eq!(
            click(&mut ui, &mut renderer, "Clear native goal"),
            vec![Intent {
                operation: Operation::Clear,
                observation_revision: 18
            }]
        );
        let point = bounds(&ui, "Pause native goal").center();
        assert!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonPressed(mouse::Button::Left)]
            )
            .is_empty()
        );
        let mut ui = cached(
            Presentation {
                observation_revision: 18,
                ..presentation(&replacement)
            },
            ui.into_cache(),
            &mut renderer,
            280.0,
        );
        assert_eq!(
            events(
                &mut ui,
                &mut renderer,
                point,
                &[mouse::Event::ButtonReleased(mouse::Button::Left)]
            ),
            vec![Intent {
                operation: Operation::Pause,
                observation_revision: 18
            }]
        );
    }

    #[test]
    fn long_objective_keeps_actual_narrow_controls_visible_without_an_implicit_budget() {
        use iced::advanced::widget::Operation as WidgetOperation;
        #[derive(Default)]
        struct Texts(Vec<String>);
        impl WidgetOperation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn WidgetOperation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
                self.0.push(text.into());
            }
        }
        let observed = goal(&"Long Unicode 🦀 objective. ".repeat(100), Status::Active);
        let mut renderer = ui_controls::accessibility_probe::headless_renderer();
        let mut ui = cached(presentation(&observed), Cache::new(), &mut renderer, 280.0);
        for name in [
            "Check native goal",
            "Pause native goal",
            "Resume native goal",
            "Clear native goal",
        ] {
            let bounds = bounds(&ui, name);
            assert!(
                bounds.width > 0.0
                    && bounds.height > 0.0
                    && bounds.x >= 0.0
                    && bounds.y >= 0.0
                    && bounds.x + bounds.width <= 280.0
                    && bounds.y + bounds.height <= 650.0
            );
        }
        let mut text = Texts::default();
        ui.operate(&renderer, &mut text);
        assert!(
            text.0
                .iter()
                .any(|text| text.contains("Tokens used: 123") && text.contains("Time used: 7s"))
        );
        assert!(!text.0.iter().any(|text| text.contains("Token budget:")));
        let mut budgeted = observed.clone();
        let ReadState::Known(Some(goal)) = &mut budgeted else {
            unreachable!()
        };
        goal.token_budget = Some(7_777);
        let mut ui = cached(
            presentation(&budgeted),
            ui.into_cache(),
            &mut renderer,
            280.0,
        );
        let mut text = Texts::default();
        ui.operate(&renderer, &mut text);
        assert!(
            text.0
                .iter()
                .any(|text| text.contains("Token budget: 7777"))
        );
    }

    #[test]
    fn objective_leads_native_panel_and_availability_never_invents_reconnect_advice() {
        use iced::advanced::widget::Operation as WidgetOperation;
        #[derive(Default)]
        struct Texts(Vec<(String, iced::Rectangle)>);
        impl WidgetOperation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn WidgetOperation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                self.0.push((text.to_owned(), bounds));
            }
        }
        let observed = goal("Preserve the actual objective λ", Status::Paused);
        let mut renderer = ui_controls::accessibility_probe::headless_renderer();
        for width in [280.0, 620.0] {
            for (state, expected, excluded) in [
                (presentation(&observed), "Goal · Paused", "unconfirmed"),
                (
                    Presentation {
                        current: false,
                        ..presentation(&observed)
                    },
                    "The current goal is unconfirmed for this conversation",
                    "awaiting provider confirmation",
                ),
                (
                    Presentation {
                        current: false,
                        pending: true,
                        ..presentation(&observed)
                    },
                    "Goal request awaiting provider confirmation",
                    "Goal controls are unavailable",
                ),
                (
                    Presentation {
                        current: false,
                        uncertain: true,
                        ..presentation(&observed)
                    },
                    "Current goal is unconfirmed. Check before changing it",
                    "awaiting provider confirmation",
                ),
                (
                    Presentation {
                        work_ready: false,
                        ..presentation(&observed)
                    },
                    "Resume is unavailable because this conversation is not ready for new work",
                    "unconfirmed",
                ),
            ] {
                let mut ui = cached(state, Cache::new(), &mut renderer, width);
                let mut text = Texts::default();
                ui.operate(&renderer, &mut text);
                let (objective, objective_bounds) =
                    text.0.first().expect("native objective heading");
                assert_eq!(objective, "Preserve the actual objective λ");
                let (_, status_bounds) = text
                    .0
                    .iter()
                    .find(|(text, _)| text.contains(" · Paused"))
                    .unwrap();
                assert!(
                    objective_bounds.height > status_bounds.height,
                    "human objective has the primary native text hierarchy"
                );
                assert!(
                    text.0.iter().any(|(text, _)| text.contains(expected)),
                    "actual panel explains the observed availability at {width}"
                );
                assert!(
                    !text
                        .0
                        .iter()
                        .any(|(text, _)| text.contains(excluded) || text.contains("Reconnect")),
                    "presentation cannot infer a disconnect from current=false"
                );
                for name in [
                    "Check native goal",
                    "Pause native goal",
                    "Resume native goal",
                    "Clear native goal",
                ] {
                    let bounds = bounds(&ui, name);
                    assert!(
                        bounds.width > 0.0
                            && bounds.height > 0.0
                            && bounds.x >= 0.0
                            && bounds.y >= 0.0
                            && bounds.x + bounds.width <= width
                            && bounds.y + bounds.height <= 650.0
                    );
                }
                if state.current && !state.work_ready {
                    assert!(click(&mut ui, &mut renderer, "Resume native goal").is_empty());
                    assert_eq!(
                        click(&mut ui, &mut renderer, "Check native goal"),
                        vec![Intent {
                            operation: Operation::Get,
                            observation_revision: 17
                        }]
                    );
                    assert_eq!(
                        click(&mut ui, &mut renderer, "Clear native goal"),
                        vec![Intent {
                            operation: Operation::Clear,
                            observation_revision: 17
                        }]
                    );
                }
            }
        }
    }
}
