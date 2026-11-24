//! The rail an open inspector collapses to in a pane too narrow to dock it.
//!
//! It keeps the pane's agent's status and how much work it did in view along
//! the pane's trailing edge, and pressing it expands the whole inspector
//! over the pane's content. Everything it shows is read from state the
//! update already keeps: the pane's activity, and the Work count its
//! conversation's list was counted for when it arrived.

use super::*;

/// How wide the rail is, its rule included.
pub(super) const RAIL_WIDTH: f32 = 40.0;

/// What the rail says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Summary {
    /// The activity block's glyph, or the inspector's own icon for a pane
    /// whose activity is not shown.
    pub(super) glyph: Icon,
    /// What the activity block's first row says, if it is shown.
    pub(super) status: Option<String>,
    /// How many rows the Work section lists, once it has listed any.
    pub(super) work: Option<usize>,
}

impl Summary {
    /// The rail's tooltip and accessible name.
    pub(super) fn label(&self) -> String {
        let mut parts: Vec<String> = self.status.iter().cloned().collect();
        if let Some(work) = self.work {
            parts.push(format!("{work} in Work"));
        }
        if parts.is_empty() {
            "Show inspector".to_owned()
        } else {
            format!("{}\nShow inspector", parts.join(" · "))
        }
    }
}

impl Cagents {
    /// What `card`'s rail shows.
    pub(super) fn inspector_rail_summary(&self, card: CardId) -> Summary {
        let status = self
            .settings
            .inspector_show_activity
            .then(|| {
                self.card_state(card)
                    .and_then(|state| self.activity_status(card, state))
            })
            .flatten();
        // As the Work section's header counts it: only once listed, and
        // only when there is something to count.
        let work = (self.tracks_work(card)
            && self.work_phase(card) == pull_request_ui::WorkPhase::Listed)
            .then(|| self.work_count(card))
            .filter(|count| *count > 0);
        Summary {
            glyph: status
                .as_ref()
                .map_or(Icon::SplitPanel, |status| status.glyph),
            status: status.map(|status| status.label),
            work,
        }
    }

    /// `card`'s rail: a rule against the pane's content, then one control
    /// the rail's height that expands the inspector over the content.
    pub(super) fn inspector_rail(&self, card: CardId) -> Element<'_, Message> {
        let summary = self.inspector_rail_summary(card);
        let label = summary.label();
        let mut marks: Vec<Element<'_, Message>> = vec![
            widget::container(inspector_activity::status_mark(summary.glyph))
                .center_x(Length::Fill)
                .center_y(Length::Fixed(design::PRIMARY_CONTROL_SIZE))
                .into(),
        ];
        if let Some(work) = summary.work {
            // The pull request glyph with how many rows Work lists on its
            // corner, as every glyph that carries a count shows it.
            marks.push(
                widget::container(crate::ui_controls::badged(
                    Icon::PullRequest,
                    design::ROW_ICON_SIZE,
                    design::Tone::Themed(design::muted_icon_color),
                    work,
                    crate::ui_controls::BadgeFill::NEUTRAL,
                ))
                .center_x(Length::Fill)
                .into(),
            );
        }
        let content = widget::column::with_children(marks)
            .spacing(space::XS)
            .width(Length::Fill)
            .height(Length::Fill);
        let expand = (!self.modal_is_open()).then_some(Message::ExpandPaneInspector(card));
        let button = widget::button::custom(content)
            .name(label.replace('\n', ". "))
            .id(self.pane_inspector_rail_id(card))
            .class(crate::ui_controls::control_style(false, None))
            .padding([space::XXS, 0.0])
            .width(Length::Fill)
            .height(Length::Fill)
            .on_press_maybe(expand.clone());
        // Space presses it as Enter does.
        let button = crate::interaction::focused_key(button, move |event| {
            expand
                .clone()
                .filter(|_| crate::interaction::is_space_press(event))
        });
        // As tall as the pane, it shows its tooltip beside it, level with the
        // pointer, rather than above it at the window's top edge.
        let button = crate::interaction::tooltip_beside(crate::interaction::hover(button), label);
        widget::container(
            widget::row::with_children(vec![
                design::vertical_rule(),
                widget::container(button)
                    .padding(space::XXS)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into(),
            ])
            .width(Length::Fill)
            .height(Length::Fill),
        )
        .class(cosmic::theme::Container::Background)
        .width(RAIL_WIDTH)
        .height(Length::Fill)
        .into()
    }
}
