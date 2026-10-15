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

use super::coordination::Coordinated;
use super::*;
use crate::chat::{ChangeKind, FileChange, SearchResult, ToolStatus};

/// The surface's spacing rhythm.
///
/// Four values, not an ad-hoc number per call site. Everything a reader sees
/// as "related" sits at `GAP_XS`, "part of the same block" at `GAP_S`, "a
/// block of its own" at `GAP_M`, and "a different exchange" at `GAP_L`. That
/// progression is what makes structure legible without drawing a box around
/// every group.
pub(super) const GAP_XS: f32 = space::XS;
pub(super) const GAP_S: f32 = space::S;
pub(super) const GAP_M: f32 = space::M;
pub(super) const GAP_L: f32 = design::GRID_UNIT * 7.0;

/// The rail a turn's steps hang from, and the space between it and them.
///
/// A continuous hairline down the left of the strip is what turns a list of
/// rows into one sequence of work. It is drawn as the container's left edge
/// rather than per row, so it cannot break between rows.
const RAIL_INSET: f32 = space::M;

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
    let glyph = |name: Icon, tone: Option<design::Tone>| {
        (
            widget::icon(name.handle())
                .size(design::COMPACT_ICON_SIZE)
                .into(),
            tone,
        )
    };
    match status {
        ToolStatus::Running => (
            crate::ui_controls::activity_spinner(
                f32::from(design::COMPACT_ICON_SIZE),
                design::accent_color,
            ),
            Some(design::Tone::Themed(design::accent_color)),
        ),
        ToolStatus::Succeeded => glyph(Icon::Check, None),
        ToolStatus::Failed => glyph(
            Icon::Error,
            Some(design::Tone::Themed(design::destructive_color)),
        ),
        ToolStatus::Denied => glyph(
            Icon::Unavailable,
            Some(design::Tone::Themed(design::muted_color)),
        ),
        ToolStatus::Stopped => glyph(
            Icon::Cancel,
            Some(design::Tone::Themed(design::muted_color)),
        ),
    }
}

/// The glyph that stands for what a step was doing, so a reader scanning the
/// left edge of a strip can tell reading from running from editing without
/// reading any text.
fn kind_glyph(content: &ItemContent) -> Icon {
    match content {
        ItemContent::Text { .. } => Icon::Private,
        ItemContent::Command { action, .. } => match action {
            chat::CommandAction::Read => Icon::TextFile,
            chat::CommandAction::Search => Icon::Search,
            chat::CommandAction::List => Icon::Folder,
            chat::CommandAction::Run => Icon::Terminal,
        },
        ItemContent::Changes { .. } => Icon::Changes,
        ItemContent::WebSearch { .. } => Icon::WebBrowser,
        ItemContent::Tool { .. } => Icon::Utilities,
        ItemContent::Image { .. } => Icon::Image,
        ItemContent::Delivery {
            kind: chat::DeliveryKind::Notice,
            ..
        } => Icon::Attention,
        ItemContent::Delivery { .. } => Icon::Chat,
    }
}

/// Monospaced content inside its own surface.
///
/// Uses the shared transcript selection because the transcript's whole purpose is
/// handing output to the reader: a command's output that cannot be selected
/// forces them to use a copy button for a single word.
///
/// `caption` names what the block is - the language, or `Output` - in a header
/// that shares the block's top corners, so the label belongs to the code
/// rather than floating above it. `copy` puts a copy control on that same
/// header, which is where a reader looks for it and the only way to take a
/// command's output without dragging a selection through a scrolling box.
pub(super) fn code_surface(
    text: &str,
    caption: Option<&str>,
    limit: Option<f32>,
    copy: Option<Action>,
    target: Target,
) -> Element<'static, Message> {
    let body = widget::container(selectable_prose::plain(
        text,
        design::TEXT_BODY,
        iced::Font::MONOSPACE,
        target,
    ))
    .width(Length::Fill)
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
    code_frame(body, caption.map(str::to_owned), copy)
}

pub(super) fn code_frame<'a>(
    body: Element<'a, Message>,
    caption: Option<String>,
    copy: Option<Action>,
) -> Element<'a, Message> {
    code_frame_label(
        body,
        caption.map(|caption| design::caption(caption).into()),
        copy,
    )
}

fn code_frame_label<'a>(
    body: Element<'a, Message>,
    caption: Option<Element<'a, Message>>,
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
    let body = widget::container(body)
        .class(design::surface(
            Some(design::sunken),
            [design::RADIUS_S; 4],
            true,
        ))
        .width(Length::Fill)
        .into();
    let block = widget::column::with_children(vec![
        widget::container(caption)
            .padding(iced::Padding::ZERO.right(24))
            .width(Length::Fill)
            .height(24)
            .align_y(Alignment::Center)
            .into(),
        body,
    ])
    .spacing(GAP_XS)
    .width(Length::Fill);
    match copy {
        Some(copy) => iced::widget::hover(
            block,
            widget::container(small_icon(Icon::Copy, "Copy this block", copy))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::Start),
        ),
        None => block.into(),
    }
}

fn change_verb(kind: &ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "Added",
        ChangeKind::Deleted => "Deleted",
        ChangeKind::Updated => "Edited",
        ChangeKind::Renamed { .. } => "Renamed",
    }
}

fn change_view(
    change: &FileChange,
    target: Target,
    item: &str,
    index: usize,
    paint: Option<Arc<find::Paint>>,
) -> Element<'static, Message> {
    let mut rows: Vec<Element<'_, Message>> = vec![
        widget::row::with_children(vec![
            design::caption(change_verb(&change.kind)).into(),
            widget::container(find::text(
                target,
                item,
                &[7, index as u32, 0],
                change.path.clone(),
                design::TEXT_BODY,
                iced::Font::MONOSPACE,
                paint.clone(),
            ))
            .width(Length::Fill)
            .into(),
        ])
        .spacing(GAP_S)
        .align_y(Alignment::Center)
        .into(),
    ];
    if let ChangeKind::Renamed { from } = &change.kind {
        rows.push(
            widget::row::with_children(vec![
                design::caption("from").into(),
                find::text(
                    target,
                    item,
                    &[7, index as u32, 1],
                    from.clone(),
                    design::TEXT_CAPTION,
                    iced::Font::default(),
                    paint.clone(),
                ),
            ])
            .spacing(GAP_XS)
            .into(),
        );
    }
    widget::column::with_children(rows)
        .spacing(GAP_XS)
        .width(Length::Fill)
        .into()
}

fn search_result_view(
    result: &SearchResult,
    target: Target,
    item: &str,
    index: usize,
    paint: Option<Arc<find::Paint>>,
) -> Element<'static, Message> {
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let title = if result.title.is_empty() {
        result.url.as_str()
    } else {
        result.title.as_str()
    };
    if result.url.is_empty() {
        rows.push(find::text(
            target,
            item,
            &[8, 1, index as u32, 0],
            title,
            design::TEXT_BODY,
            iced::Font::default(),
            paint.clone(),
        ));
    } else {
        rows.push(selectable_prose::logical_text(
            vec![
                iced::widget::text::Span::new(title.to_owned())
                    .link(result.url.clone())
                    .color(design::accent_color(&cosmic::theme::active())),
            ]
            .into(),
            design::TEXT_BODY.into(),
            iced::Font::default(),
            target,
            None,
            selectable_prose::Logical {
                identity: selection::logical_identity(
                    item,
                    &[
                        8,
                        1,
                        index as u32,
                        if result.title.is_empty() { 1 } else { 0 },
                    ],
                ),
                start: 0,
                paint: paint.clone(),
            },
        ));
        if !result.title.is_empty() {
            rows.push(find::text(
                target,
                item,
                &[8, 1, index as u32, 1],
                result.url.as_str(),
                design::TEXT_CAPTION,
                iced::Font::default(),
                paint.clone(),
            ));
        }
    }
    if !result.snippet.is_empty() {
        rows.push(find::text(
            target,
            item,
            &[8, 1, index as u32, 2],
            result.snippet.as_str(),
            design::TEXT_BODY,
            iced::Font::default(),
            paint.clone(),
        ));
    }
    widget::column::with_children(rows)
        .spacing(space::XS)
        .width(Length::Fill)
        .into()
}

impl Runtime {
    fn tool_code_surface(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        part: CopyPart,
        caption: &str,
        limit: Option<f32>,
    ) -> Element<'static, Message> {
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
        // What a command or a tool printed is followed from its end. A patch,
        // an input, and what a read or a search returned are read from the
        // top, whether or not the step is still running.
        let follows = matches!(part, CopyPart::Output)
            && !matches!(
                item.content,
                ItemContent::Command {
                    action: chat::CommandAction::Read
                        | chat::CommandAction::Search
                        | chat::CommandAction::List,
                    ..
                }
            );
        code_frame_label(
            block.view_logical(
                self.syntax_theme,
                limit.map(|height| (height, follows)),
                target,
                self.find.as_deref(),
                selectable_prose::Logical {
                    identity: selection::logical_identity(
                        &item.id,
                        &match part {
                            CopyPart::Input => vec![4],
                            CopyPart::Change(index) => vec![6, index as u32],
                            _ => vec![5],
                        },
                    ),
                    start: 0,
                    paint: self.find_paint.clone(),
                },
            ),
            Some(
                if matches!(part, CopyPart::Input)
                    && matches!(&item.content,ItemContent::Command {working_directory,..} if !working_directory.is_empty())
                {
                    find::text(
                        target,
                        &item.id,
                        &[3, 1],
                        caption,
                        design::TEXT_CAPTION,
                        iced::Font::default(),
                        self.find_paint.clone(),
                    )
                } else {
                    design::caption(caption.to_owned()).into()
                },
            ),
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
        steps: impl FnOnce() -> Vec<virtualized::Row<'a>>,
    ) -> Element<'a, Message> {
        let target = self.target;
        let key = turn.key.as_str();
        // Explicit reader choices override the configured default.
        let open = self
            .opened
            .get(key)
            .copied()
            .unwrap_or(self.tool_groups_expanded);
        let mut line: Vec<Element<'static, Message>> = vec![
            widget::icon(Icon::handle(if open {
                Icon::PanDown
            } else {
                Icon::PanEnd
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
        line.push(
            design::heading(turn.summary.clone())
                .width(Length::Fill)
                .into(),
        );
        // The one-second chat tick advances running timers even while a tool
        // is quiet. The label shares its cadence with the retained-turn cache.
        let elapsed = self
            .source_now_ms()
            .and_then(|now| turn.duration_label(now));
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
                virtualized::column(rows, Default::default()),
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
    pub(super) fn step_view(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
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
        // A step that worked with another agent says what it did in the
        // conversation's own names rather than reprinting its command.
        let coordinated = cached.coordination.as_deref();
        let (verb, label) = match coordinated {
            Some(coordinated) => coordinated.line(&|id| self.agent_name(id)),
            None => (
                turn::step_verb(&item.content).to_owned(),
                turn::step_label(&item.content).to_owned(),
            ),
        };
        let glyph = coordinated
            .and_then(Coordinated::glyph)
            .unwrap_or_else(|| kind_glyph(&item.content));
        let mut line: Vec<Element<'static, Message>> = vec![
            widget::icon(Icon::handle(if open {
                Icon::PanDown
            } else {
                Icon::PanEnd
            }))
            .size(design::COMPACT_ICON_SIZE)
            .into(),
            widget::icon(glyph.handle()).size(14).into(),
        ];
        if !verb.is_empty() {
            line.push(design::caption(verb.clone()).into());
        }
        // The target is a path, a pattern, or a command, so it is set in the
        // same face the reader would see it in a terminal or an editor. That
        // is also what separates it at a glance from the verb beside it.
        // A long command wraps onto the next line rather than running under
        // the outcome beside it: a pane is often narrow, and a row whose end
        // is hidden is a row the reader cannot check.
        // What the step is comes first and stays: the tool's name and
        // input, or the command. What it last printed follows on a muted
        // line of its own, so a running build can be followed without the
        // row forgetting what is being built.
        let mut preview: Vec<Element<'static, Message>> = Vec::new();
        if let Some(coordinated) = coordinated {
            preview.push(design::body(label.clone()).into());
            let arrived = coordinated.reading.outcome.messages.len();
            if arrived > 0 {
                preview.push(
                    design::caption(if arrived == 1 {
                        "1 message arrived".to_owned()
                    } else {
                        format!("{arrived} messages arrived")
                    })
                    .into(),
                );
            }
        } else if matches!(item.content, ItemContent::Tool { .. }) {
            preview.push(find::text(
                target,
                &item.id,
                &[3, 0],
                label.clone(),
                design::TEXT_BODY,
                iced::Font::default(),
                self.find_paint.clone(),
            ));
            if !cached.preview.text.is_empty() {
                preview.push(cached.preview.view());
            }
        } else if matches!(&item.content,ItemContent::Command {label,command,..} if !label.is_empty() && label != command)
        {
            preview.push(find::text(
                target,
                &item.id,
                &[3, 0],
                label.clone(),
                design::TEXT_BODY,
                iced::Font::MONOSPACE,
                self.find_paint.clone(),
            ));
        } else {
            preview.push(cached.preview.view());
        }
        preview.extend(cached.preview.tail_view());
        line.push(
            widget::column::with_children(preview)
                .spacing(GAP_XS)
                .width(Length::Fill)
                .into(),
        );
        if let Some(detail) = &cached.detail {
            line.push(design::caption(detail.clone()).into());
        }
        let name = if verb.is_empty() {
            label
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
        hover::view(
            widget::row::with_children(vec![header.into(), widget::Space::new().width(24).into()])
                .align_y(Alignment::Center)
                .spacing(GAP_XS)
                .width(Length::Fill),
            widget::container(self.message_copy(item))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::End)
                .align_y(Alignment::Center),
        )
    }

    /// The bounded height a step's raw output is allowed to occupy before it
    /// scrolls inside its own surface, so one long build log cannot bury the
    /// steps that follow it.
    const OUTPUT_HEIGHT: f32 = 320.0;

    pub(super) fn step_detail(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        let mut rows: Vec<Element<'static, Message>> = Vec::new();
        // Messages a coordinating step collected read as messages, ahead of
        // the command and output they were printed in.
        if let Some(coordinated) = cached
            .coordination
            .as_deref()
            .filter(|coordinated| !coordinated.is_card())
        {
            rows.extend(self.arrived(item, cached, coordinated, "From", settings));
        }
        match &item.content {
            // A notice's body is a log line or a raw result, drawn verbatim.
            ItemContent::Delivery {
                kind: chat::DeliveryKind::Notice,
                body,
                ..
            } => {
                if !body.is_empty() {
                    rows.push(code_surface(
                        body,
                        None,
                        Some(Self::OUTPUT_HEIGHT),
                        None,
                        self.target,
                    ));
                }
            }
            ItemContent::Text { .. } | ItemContent::Delivery { .. } => {
                rows.push(prose::view(
                    cached,
                    settings,
                    self.target,
                    self.find.as_deref(),
                ));
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
                    Some(Self::OUTPUT_HEIGHT),
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
                    rows.push(change_view(
                        change,
                        self.target,
                        &item.id,
                        index,
                        self.find_paint.clone(),
                    ));
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
                rows.push(find::text(
                    self.target,
                    &item.id,
                    &[8, 0],
                    query.as_str(),
                    design::TEXT_BODY,
                    iced::Font::default(),
                    self.find_paint.clone(),
                ));
                if results.is_empty() {
                    rows.push(design::caption("No results were reported.").into());
                }
                rows.extend(results.iter().enumerate().map(|(index, result)| {
                    search_result_view(
                        result,
                        self.target,
                        &item.id,
                        index,
                        self.find_paint.clone(),
                    )
                }));
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
                    rows.push(find::text(
                        self.target,
                        &item.id,
                        &[9, 0],
                        description.as_str(),
                        design::TEXT_BODY,
                        iced::Font::default(),
                        self.find_paint.clone(),
                    ));
                }
                if path.starts_with("http://") || path.starts_with("https://") {
                    rows.push(
                        widget::button::link("Open image")
                            .on_press(action(Action::OpenUrl(path.clone())))
                            .into(),
                    );
                } else if !path.is_empty() && !path.starts_with("data:") {
                    rows.push(find::text(
                        self.target,
                        &item.id,
                        &[9, 1],
                        path.as_str(),
                        design::TEXT_CAPTION,
                        iced::Font::default(),
                        self.find_paint.clone(),
                    ));
                }
            }
        }
        // A step carries no controls of its own. Copying belongs to an answer,
        // and the pieces of a step a reader actually copies - a command, its
        // output, a tool's input - each carry their own control on the surface
        // that holds them.
        widget::column::with_children(rows)
            .spacing(space::XS)
            .width(Length::Fill)
            .into()
    }
}
