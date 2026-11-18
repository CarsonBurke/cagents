//! The top of a chat's or terminal's inspector: what its agent is doing,
//! where it works and, for a chat, how its subagents stand.
//!
//! The block has the same rows whatever its pane reports, so a status that
//! arrives, changes or has nothing to say yet never moves the sections
//! beneath it.

use super::*;

/// Between the block's rows, and above and below them.
const ROW_GAP: f32 = space::XXS;

/// A chat's block: its status and place, then its subagents.
const CHAT_HEIGHT: f32 = design::list::TWO_LINE_HEIGHT + design::DENSE_ROW_HEIGHT + 3.0 * ROW_GAP;
/// A terminal's block: its status and place.
const TERMINAL_HEIGHT: f32 = design::list::TWO_LINE_HEIGHT + 2.0 * ROW_GAP;

/// What the block says the pane's agent is doing.
struct Status {
    glyph: Icon,
    label: String,
}

impl Cagents {
    /// `card`'s activity block, ending its first row with the inspector's
    /// `toggle`, and its height; none for a pane with no agent to describe,
    /// or while the settings leave it out.
    pub(super) fn inspector_activity<'a>(
        &'a self,
        card: CardId,
        toggle: Element<'a, Message>,
    ) -> Result<(Element<'a, Message>, f32), Element<'a, Message>> {
        if !self.settings.inspector_show_activity {
            return Err(toggle);
        }
        let Some(state) = self.card_state(card) else {
            return Err(toggle);
        };
        let (status, subagents) = match (&state.kind, self.cards.get(&card)) {
            (CardKind::Chat(_), Some(CardRuntime::Chat(runtime))) => {
                let (glyph, label) = runtime.inspector_status();
                let status = Status {
                    glyph: glyph.unwrap_or(Icon::Chat),
                    label: if label.trim().is_empty() {
                        "Ready".to_owned()
                    } else {
                        label
                    },
                };
                (
                    status,
                    Some(self.subagents_row(
                        runtime.subagent_tally(),
                        Some(runtime.open_subagent_picker()),
                    )),
                )
            }
            // The row stays while the conversation connects, so it is not
            // added under the reader once it has.
            (CardKind::Chat(_), _) => (
                Status {
                    glyph: Icon::Chat,
                    label: "Connecting…".to_owned(),
                },
                Some(self.subagents_row(Default::default(), None)),
            ),
            (CardKind::Terminal(_), _) => (self.terminal_status(card), None),
            _ => return Err(toggle),
        };
        let place = self.pane_place(state);
        let mark: Element<'a, Message> = if status.glyph == Icon::AgentWorking {
            crate::ui_controls::activity_spinner(
                f32::from(design::ROW_ICON_SIZE),
                design::accent_color,
            )
        } else {
            crate::ui_controls::passive_icon(status.glyph, design::ROW_ICON_SIZE)
        };
        let text = widget::column::with_children(vec![
            crate::ui_controls::fade_text(status.label.clone()).into(),
            crate::interaction::tooltip(
                crate::ui_controls::FadeText::caption(place.label.clone())
                    .class(cosmic::theme::Text::Custom(design::muted_text)),
                place.path,
            ),
        ])
        .width(Length::Fill);
        let first = grid_row(
            mark,
            text.into(),
            Some(toggle),
            design::list::TWO_LINE_HEIGHT,
        );
        let mut rows: Vec<Element<'a, Message>> = vec![first];
        let height = if let Some(subagents) = subagents {
            rows.push(subagents);
            CHAT_HEIGHT
        } else {
            TERMINAL_HEIGHT
        };
        let block = widget::column::with_children(rows)
            .spacing(ROW_GAP)
            .padding([ROW_GAP, design::list::GUTTER])
            .width(Length::Fill)
            .height(Length::Fixed(height));
        Ok((block.into(), height))
    }

    /// The subagents row: how many there are and how many need the reader,
    /// opening the pane's list of them. Without any, it says so, so the row
    /// is there before the first one starts.
    fn subagents_row<'a>(
        &self,
        tally: chat_ui::SubagentTally,
        open: Option<Message>,
    ) -> Element<'a, Message> {
        if tally.total == 0 {
            return grid_row(
                crate::ui_controls::passive_icon(Icon::Agent, design::ROW_ICON_SIZE),
                crate::ui_controls::fade_text("No subagents")
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
                None,
                design::DENSE_ROW_HEIGHT,
            );
        }
        let mut parts = vec![format!(
            "{} subagent{}",
            tally.total,
            if tally.total == 1 { "" } else { "s" }
        )];
        if tally.working > 0 {
            parts.push(format!("{} working", tally.working));
        }
        if tally.attention > 0 {
            parts.push(format!("{} need attention", tally.attention));
        }
        let label = parts.join(" · ");
        let content = widget::row::with_children(vec![
            widget::container(crate::ui_controls::passive_icon(
                if tally.working > 0 {
                    Icon::AgentWorking
                } else {
                    Icon::Agent
                },
                design::ROW_ICON_SIZE,
            ))
            .center_x(Length::Fixed(design::list::ICON))
            .center_y(Length::Fill)
            .into(),
            widget::container(crate::ui_controls::fade_text(label.clone()))
                .width(Length::Fill)
                .center_y(Length::Fill)
                .into(),
            widget::container(crate::ui_controls::passive_icon(
                Icon::ChevronRight,
                design::COMPACT_ICON_SIZE,
            ))
            .center_y(Length::Fill)
            .into(),
        ])
        .spacing(design::list::GAP)
        .height(Length::Fill);
        crate::interaction::hover(
            widget::button::custom(content)
                .name(format!("Show subagents: {label}"))
                .class(crate::ui_controls::control_style(false, None))
                .padding([0.0, design::list::INSET])
                .height(design::DENSE_ROW_HEIGHT)
                .width(Length::Fill)
                .on_press_maybe(open.filter(|_| !self.modal_is_open())),
        )
    }

    /// What a terminal's agent is doing, as its pane reports it, or that it
    /// is a terminal without one.
    fn terminal_status(&self, card: CardId) -> Status {
        use crate::agent::ActivityState;
        if self.sidebar_state.card_attention(card).state() == Some(sidebar::AttentionState::Limited)
        {
            return Status {
                glyph: Icon::Warning,
                label: "Stopped by a usage limit".to_owned(),
            };
        }
        match self.sidebar_state.card_activity(card) {
            Some(activity) => match activity.state {
                ActivityState::Working => Status {
                    glyph: Icon::AgentWorking,
                    label: format!("{} working", activity.provider.label()),
                },
                ActivityState::Waiting => Status {
                    glyph: Icon::AgentWaiting,
                    label: "Waiting for your input".to_owned(),
                },
                ActivityState::Idle | ActivityState::Unknown => Status {
                    glyph: Icon::Agent,
                    label: format!("{} ready", activity.provider.label()),
                },
            },
            None => Status {
                glyph: Icon::Terminal,
                label: "Terminal".to_owned(),
            },
        }
    }

    /// Where `card` works: its branch and project, as Projects lists them,
    /// else its folder, with the full path for the tooltip.
    fn pane_place(&self, card: &CardState) -> Place {
        let Some(directory) = super::worktree_ui::card_directory(card) else {
            return Place {
                label: "No folder".to_owned(),
                path: String::new(),
            };
        };
        let path = directory.display().to_string();
        let folder = || {
            directory
                .file_name()
                .map_or_else(|| path.clone(), |name| name.to_string_lossy().into_owned())
        };
        let listed = self.sidebar_model.worktree_at(directory).and_then(|key| {
            let project = self.sidebar_model.project(key.project_id)?;
            let worktree = project
                .worktrees
                .iter()
                .find(|worktree| worktree.id == key.worktree_id)?;
            Some((project.name.clone(), worktree.branch.clone()))
        });
        let label = match listed {
            Some((project, Some(branch))) => format!("{branch} · {project}"),
            Some((project, None)) => format!("{} · {project}", folder()),
            None => folder(),
        };
        Place { label, path }
    }
}

struct Place {
    label: String,
    path: String,
}

/// A row on the list grid: `mark` in the icon column, `text` on the label
/// column, then `trailing`.
fn grid_row<'a>(
    mark: Element<'a, Message>,
    text: Element<'a, Message>,
    trailing: Option<Element<'a, Message>>,
    height: f32,
) -> Element<'a, Message> {
    let mut children = vec![
        widget::container(mark)
            .center_x(Length::Fixed(design::list::ICON))
            .center_y(Length::Fill)
            .into(),
        widget::container(text)
            .width(Length::Fill)
            .center_y(Length::Fill)
            .into(),
    ];
    if let Some(trailing) = trailing {
        children.push(
            widget::container(trailing)
                .align_y(Alignment::Start)
                .height(Length::Fill)
                .into(),
        );
    }
    widget::container(
        widget::row::with_children(children)
            .spacing(design::list::GAP)
            .height(Length::Fill),
    )
    .padding([0.0, design::list::INSET])
    .height(Length::Fixed(height))
    .width(Length::Fill)
    .into()
}
