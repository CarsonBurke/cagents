//! Rendering a turn's activity as a strip of steps.
//!
//! The surface this replaces gave every transcript item the same weight: a
//! provider-named heading, a row of twelve unconditional icon controls, and a
//! body. A turn that read one file and ran one command therefore occupied more
//! vertical space than the answer it produced, and the tool rows it spent that
//! space on said only `read · Completed` - the verb without the object.
//!
//! A step here is one line - verb, target, and the outcome metadata a reader
//! would act on - that discloses its own detail when asked. Nothing is
//! rendered per step that the reader has not asked for.

use super::*;
use crate::chat::{ChangeKind, FileChange, SearchResult, ToolStatus};

/// The surface's spacing rhythm.
///
/// Four values, not an ad-hoc number per call site. Everything a reader sees
/// as "related" sits at `GAP_XS`, "part of the same block" at `GAP_S`, "a
/// block of its own" at `GAP_M`, and "a different exchange" at `GAP_L`. That
/// progression is what makes structure legible without drawing a box around
/// every group.
pub(super) const GAP_XS: f32 = design::GRID_UNIT;
pub(super) const GAP_S: f32 = design::GRID_UNIT * 2.0;
pub(super) const GAP_M: f32 = design::GRID_UNIT * 3.0;
pub(super) const GAP_L: f32 = design::GRID_UNIT * 7.0;

/// The rail a turn's steps hang from, and the space between it and them.
///
/// A continuous hairline down the left of the strip is what turns a list of
/// rows into one sequence of work. It is drawn as the container's left edge
/// rather than per row, so it cannot break between rows.
const RAIL_INSET: f32 = design::GRID_UNIT * 3.0;

/// Where a group's thread line hangs: under the centre of the header's
/// disclosure chevron, which sits one control inset from the header's edge.
const THREAD_RAIL_OFFSET: f32 =
    GAP_S + design::COMPACT_ICON_SIZE as f32 / 2.0 - design::BORDER_WIDTH / 2.0;

/// The indicator and tone a group's status draws as.
///
/// Status is information, not a control, so it is a plain glyph rather than
/// one of the buttons that used to line every row. Running is the exception:
/// a static glyph for work in flight says only that something was running
/// when the frame was drawn, so it is the animated indicator the rest of the
/// shell uses for the same state.
fn status_indicator<'a>(status: ToolStatus) -> (Element<'a, Message>, Option<design::Tone>) {
    let glyph = |name: &'static str, tone: Option<design::Tone>| {
        (
            widget::icon(crate::ui_controls::symbolic_icon(name))
                .size(design::COMPACT_ICON_SIZE)
                .into(),
            tone,
        )
    };
    match status {
        ToolStatus::Running => (
            crate::ui_controls::activity_spinner_tone(
                f32::from(design::COMPACT_ICON_SIZE),
                design::warning_color,
            ),
            Some(design::Tone::Themed(design::warning_color)),
        ),
        ToolStatus::Succeeded => glyph("emblem-ok-symbolic", None),
        ToolStatus::Failed => glyph(
            "dialog-error-symbolic",
            Some(design::Tone::Themed(design::destructive_color)),
        ),
        ToolStatus::Denied => glyph(
            "action-unavailable-symbolic",
            Some(design::Tone::Themed(design::muted_color)),
        ),
    }
}

/// The glyph that stands for what a step was doing, so a reader scanning the
/// left edge of a strip can tell reading from running from editing without
/// reading any text.
fn kind_glyph(content: &ItemContent) -> &'static str {
    match content {
        ItemContent::Text { .. } => "view-private-symbolic",
        ItemContent::Command { action, .. } => match action {
            chat::CommandAction::Read => "text-x-generic-symbolic",
            chat::CommandAction::Search => "system-search-symbolic",
            chat::CommandAction::List => "folder-symbolic",
            chat::CommandAction::Run => "utilities-terminal-symbolic",
        },
        ItemContent::Changes { .. } => "document-edit-symbolic",
        ItemContent::WebSearch { .. } => "web-browser-symbolic",
        ItemContent::Tool { .. } => "applications-utilities-symbolic",
        ItemContent::Image { .. } => "image-x-generic-symbolic",
    }
}

/// Monospaced content inside its own surface.
///
/// Built from `selectable_text` because the transcript's whole purpose is
/// handing output to the reader: a command's output that cannot be selected
/// forces them to use a copy button for a single word.
///
/// `caption` names what the block is - the language, or `Output` - in a header
/// that shares the block's top corners, so the label belongs to the code
/// rather than floating above it. `copy` puts a copy control on that same
/// header, which is where a reader looks for it and the only way to take a
/// command's output without dragging a selection through a scrolling box.
pub(super) fn code_surface<'a>(
    text: &'a str,
    caption: Option<&'a str>,
    limit: Option<f32>,
    copy: Option<Action>,
) -> Element<'a, Message> {
    let body = widget::container(
        widget::selectable_text(text)
            .size(design::TEXT_BODY)
            .font(iced::Font::MONOSPACE)
            .width(Length::Fill),
    )
    .padding([GAP_S as u16, GAP_M as u16]);
    let body: Element<'_, Message> = match limit {
        Some(height) => widget::container(
            widget::scrollable(body)
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .height(Length::Shrink),
        )
        .max_height(height)
        .into(),
        None => body.into(),
    };
    code_frame(body, caption, copy)
}

fn code_frame<'a>(
    body: Element<'a, Message>,
    caption: Option<&'a str>,
    copy: Option<Action>,
) -> Element<'a, Message> {
    let Some(caption) = caption else {
        return widget::container(body)
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_S; 4],
                true,
            ))
            .width(Length::Fill)
            .into();
    };
    widget::container(
        widget::column::with_children(vec![
            widget::container(
                widget::row::with_children(match copy {
                    Some(copy) => vec![
                        design::caption(caption).width(Length::Fill).into(),
                        // At the primary control size this one button was
                        // taller than the caption row it sits in, which made
                        // every code block look like it carried a toolbar.
                        small_icon("edit-copy-symbolic", "Copy this block", copy),
                    ],
                    None => vec![design::caption(caption).into()],
                })
                .align_y(Alignment::Center)
                .spacing(GAP_S),
            )
            .padding([GAP_XS as u16, GAP_M as u16])
            .width(Length::Fill)
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_S, design::RADIUS_S, 0.0, 0.0],
                false,
            ))
            .into(),
            body,
        ])
        .width(Length::Fill),
    )
    .class(design::surface(
        Some(design::sunken),
        [design::RADIUS_S; 4],
        true,
    ))
    .width(Length::Fill)
    .into()
}

fn change_verb(kind: &ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "Added",
        ChangeKind::Deleted => "Deleted",
        ChangeKind::Updated => "Edited",
        ChangeKind::Renamed { .. } => "Renamed",
    }
}

fn change_view(change: &FileChange) -> Element<'_, Message> {
    let mut rows: Vec<Element<'_, Message>> = vec![
        widget::row::with_children(vec![
            design::caption(change_verb(&change.kind)).into(),
            widget::text::body(change.path.as_str())
                .size(design::TEXT_BODY)
                .font(iced::Font::MONOSPACE)
                .width(Length::Fill)
                .into(),
        ])
        .spacing(GAP_S)
        .align_y(Alignment::Center)
        .into(),
    ];
    if let ChangeKind::Renamed { from } = &change.kind {
        rows.push(design::caption(format!("from {from}")).into());
    }
    widget::column::with_children(rows)
        .spacing(GAP_XS)
        .width(Length::Fill)
        .into()
}

fn search_result_view(result: &SearchResult) -> Element<'_, Message> {
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let title = if result.title.is_empty() {
        result.url.as_str()
    } else {
        result.title.as_str()
    };
    if result.url.is_empty() {
        rows.push(design::body(title).into());
    } else {
        rows.push(
            widget::button::link(title.to_owned())
                .on_press(action(Action::OpenUrl(result.url.clone())))
                .into(),
        );
        rows.push(design::caption(result.url.as_str()).into());
    }
    if !result.snippet.is_empty() {
        rows.push(design::body(result.snippet.as_str()).into());
    }
    widget::column::with_children(rows)
        .spacing(design::GRID_UNIT)
        .width(Length::Fill)
        .into()
}

impl Runtime {
    fn tool_code_surface<'a>(
        &'a self,
        item: &'a chat::Item,
        cached: &'a CachedItem,
        part: CopyPart,
        caption: &'a str,
        limit: Option<f32>,
    ) -> Element<'a, Message> {
        let block = match part {
            CopyPart::Input => cached.tool_input.as_ref(),
            CopyPart::Change(index) => cached.patches.get(index),
            _ => cached.tool_output.as_ref(),
        };
        let Some(block) = block else {
            return design::caption("No content.").into();
        };
        let target = self.target;
        let id = item.id.clone();
        let copy = Action::Copy(target, id.clone(), part.clone());
        code_frame(
            block.view(self.syntax_theme, limit, move |event| {
                action(Action::ToolText(target, id.clone(), part.clone(), event))
            }),
            Some(caption),
            Some(copy),
        )
    }

    /// One group's activity: a summary line and how long the work took,
    /// disclosing the individual steps.
    ///
    /// A group opens while it is running, because watching the work is the
    /// point, and keeps whatever state it had reached when it stopped. The
    /// automatic state is recorded once, when the group is first seen, so
    /// this only reads it: resolving it here on every frame is what made a
    /// strip the reader was reading spring shut the instant its last step
    /// finished.
    ///
    /// `steps` draws the rows of an open strip, and is only asked for them
    /// when the strip is open.
    pub(super) fn step_strip<'a>(
        &self,
        turn: &turn::Turn,
        steps: impl FnOnce() -> Vec<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let target = self.target;
        let key = turn.key.as_str();
        // Every rendered group has a record, written when the group was first
        // seen. This fallback is the same value that record was seeded with,
        // so the two cannot disagree.
        let open = self.opened.get(key).copied().unwrap_or(turn.running);
        let mut line: Vec<Element<'static, Message>> = vec![
            widget::icon(crate::ui_controls::symbolic_icon(if open {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            }))
            .size(design::COMPACT_ICON_SIZE)
            .into(),
        ];
        let tone = match turn.glyph().map(status_indicator) {
            Some((indicator, tone)) => {
                line.push(indicator);
                tone
            }
            None => None,
        };
        line.push(design::caption(turn.summary.clone()).into());
        // How long this stretch of work has taken. It is measured as the view
        // is built rather than stored on the group, so a running group's label
        // advances with the frames the arriving provider items already cause
        // and needs no clock of its own.
        let elapsed = chat::now_ms()
            .and_then(|now| turn.working_for(now))
            .map(format_duration);
        if let Some(elapsed) = &elapsed {
            line.push(design::caption(elapsed.clone()).into());
        }
        // The duration is part of what the control says, so it belongs in the
        // name as well: a reader on a screen reader is the one who cannot see
        // the caption beside it.
        let name = match &elapsed {
            Some(elapsed) => format!("{} · {elapsed}", turn.summary),
            None => turn.summary.clone(),
        };
        let header = widget::button::custom(
            widget::row::with_children(line)
                .spacing(GAP_S)
                .align_y(Alignment::Start),
        )
        .name(name)
        .class(crate::ui_controls::control_style(false, tone))
        .padding([GAP_XS as u16, GAP_S as u16])
        .on_press(action(Action::ToggleTurn(target, key.to_owned())));
        // The header is sized to its own text rather than stretched across the
        // pane. A full-width control for a four-word summary is what made the
        // surface read as a stack of bars.
        let header = widget::container(header).width(Length::Shrink);
        if !open {
            return header.into();
        }
        let rows = steps();
        widget::column::with_children(vec![
            header.into(),
            // One hairline beside the rows rather than a border on each of
            // them, so the sequence reads as continuous. `Border` carries a
            // single width for all four edges, so a left-only edge has to be
            // its own sliver.
            // The hairline hangs from the header's disclosure chevron, so the
            // steps read as a thread that opens from it rather than a block
            // set off to the left of the summary they belong to.
            widget::row::with_children(vec![
                widget::container(iced::widget::Space::new().height(Length::Fill))
                    .width(design::BORDER_WIDTH)
                    .height(Length::Fill)
                    .class(cosmic::theme::Container::custom(|theme| {
                        cosmic::widget::container::Style {
                            background: Some(iced::Background::Color(design::hairline(theme))),
                            ..Default::default()
                        }
                    }))
                    .into(),
                widget::column::with_children(rows)
                    .spacing(GAP_XS)
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(RAIL_INSET)
            .padding(iced::Padding::ZERO.left(THREAD_RAIL_OFFSET))
            .width(Length::Fill)
            .into(),
        ])
        .spacing(GAP_XS)
        .width(Length::Fill)
        .into()
    }

    /// One step: a summary line that discloses its own detail.
    pub(super) fn step_view<'a>(
        &'a self,
        item: &'a chat::Item,
        cached: &'a CachedItem,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let open = self.expanded.contains(&item.id);
        let header = self.step_header(item, cached, open);
        if !open {
            return header;
        }
        let detail = self.step_detail(item, cached, settings);
        widget::column::with_children(vec![
            header,
            widget::container(detail)
                .padding(iced::Padding::from([0.0, GAP_S]).bottom(GAP_S))
                .width(Length::Fill)
                .into(),
        ])
        .spacing(GAP_XS)
        .width(Length::Fill)
        .into()
    }

    /// A step's summary line, which is the whole of a closed step and owns
    /// what it draws, so a finished exchange can keep it between frames.
    pub(super) fn step_header(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        open: bool,
    ) -> Element<'static, Message> {
        let target = self.target;
        let verb = turn::step_verb(&item.content);
        let label = turn::step_label(&item.content);
        let mut line: Vec<Element<'static, Message>> = vec![
            widget::icon(crate::ui_controls::symbolic_icon(if open {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            }))
            .size(design::COMPACT_ICON_SIZE)
            .into(),
            widget::icon(crate::ui_controls::symbolic_icon(kind_glyph(&item.content)))
                .size(14)
                .into(),
        ];
        if !verb.is_empty() {
            line.push(design::caption(verb).into());
        }
        // The target is a path, a pattern, or a command, so it is set in the
        // same face the reader would see it in a terminal or an editor. That
        // is also what separates it at a glance from the verb beside it.
        // A long command wraps onto the next line rather than running under
        // the outcome beside it: a pane is often narrow, and a row whose end
        // is hidden is a row the reader cannot check.
        if matches!(item.content, ItemContent::Tool { .. }) {
            let mut preview = vec![design::body(label.to_owned()).into()];
            if !cached.preview.text.is_empty() {
                preview.push(cached.preview.view());
            }
            line.push(
                widget::column::with_children(preview)
                    .spacing(GAP_XS)
                    .width(Length::Fill)
                    .into(),
            );
        } else {
            line.push(cached.preview.view());
        }
        if let Some(detail) = &cached.detail {
            line.push(design::caption(detail.clone()).into());
        }
        let name = if verb.is_empty() {
            label.to_owned()
        } else {
            format!("{verb} {label}")
        };
        let header = widget::button::custom(
            widget::row::with_children(line)
                .spacing(GAP_S)
                .align_y(Alignment::Start),
        )
        .name(name)
        .class(crate::ui_controls::control_style(open, None))
        .padding([GAP_XS as u16, GAP_S as u16])
        .width(Length::Fill)
        .on_press(action(Action::Toggle(target, item.id.clone())));
        header.into()
    }

    /// The bounded height a step's raw output is allowed to occupy before it
    /// scrolls inside its own surface, so one long build log cannot bury the
    /// steps that follow it.
    const OUTPUT_HEIGHT: f32 = 320.0;

    fn step_detail<'a>(
        &'a self,
        item: &'a chat::Item,
        cached: &'a CachedItem,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        match &item.content {
            ItemContent::Text { .. } => {
                rows.push(prose::view(cached, settings));
            }
            ItemContent::Command {
                working_directory,
                output,
                ..
            } => {
                rows.push(self.tool_code_surface(
                    item,
                    cached,
                    CopyPart::Input,
                    if working_directory.is_empty() {
                        "Command"
                    } else {
                        working_directory
                    },
                    None,
                ));
                if output.is_empty() {
                    rows.push(design::caption("No output.").into());
                } else {
                    rows.push(self.tool_code_surface(
                        item,
                        cached,
                        CopyPart::Output,
                        "Output",
                        Some(Self::OUTPUT_HEIGHT),
                    ));
                }
            }
            ItemContent::Changes { changes, .. } => {
                if changes.is_empty() {
                    rows.push(design::caption("No file changes were reported.").into());
                }
                for (index, change) in changes.iter().enumerate() {
                    rows.push(change_view(change));
                    if !change.diff.is_empty() {
                        rows.push(self.tool_code_surface(
                            item,
                            cached,
                            CopyPart::Change(index),
                            "Patch",
                            Some(Self::OUTPUT_HEIGHT),
                        ));
                    }
                }
            }
            ItemContent::WebSearch { query, results } => {
                rows.push(design::body(query.as_str()).into());
                if results.is_empty() {
                    rows.push(design::caption("No results were reported.").into());
                }
                rows.extend(results.iter().map(search_result_view));
            }
            ItemContent::Tool { input, output, .. } => {
                if !input.is_empty() {
                    rows.push(self.tool_code_surface(
                        item,
                        cached,
                        CopyPart::Input,
                        "Input",
                        Some(Self::OUTPUT_HEIGHT),
                    ));
                }
                if output.is_empty() {
                    rows.push(design::caption("No output.").into());
                } else {
                    rows.push(self.tool_code_surface(
                        item,
                        cached,
                        CopyPart::Output,
                        "Output",
                        Some(Self::OUTPUT_HEIGHT),
                    ));
                }
            }
            ItemContent::Image { path, description } => {
                if let Some(handle) = &cached.image {
                    rows.push(
                        iced::widget::image(handle.clone())
                            .width(Length::Fill)
                            .height(240)
                            .content_fit(iced::ContentFit::Contain)
                            .into(),
                    );
                } else if path.starts_with("data:") {
                    rows.push(
                        design::caption(
                            "Image could not be decoded or exceeds the display size bound.",
                        )
                        .into(),
                    );
                }
                if !description.is_empty() {
                    rows.push(design::body(description.as_str()).into());
                }
                if path.starts_with("http://") || path.starts_with("https://") {
                    rows.push(
                        widget::button::link("Open image")
                            .on_press(action(Action::OpenUrl(path.clone())))
                            .into(),
                    );
                } else if !path.is_empty() && !path.starts_with("data:") {
                    rows.push(design::caption(path.as_str()).into());
                }
            }
        }
        // A step carries no controls of its own. Copying belongs to an answer,
        // and the pieces of a step a reader actually copies - a command, its
        // output, a tool's input - each carry their own control on the surface
        // that holds them.
        widget::column::with_children(rows)
            .spacing(design::GRID_UNIT)
            .width(Length::Fill)
            .into()
    }
}
