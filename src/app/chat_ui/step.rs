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
        ToolStatus::AwaitingApproval => glyph(Icon::AgentWaiting, None),
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

fn summary_spans(summary: &str) -> Vec<iced::widget::text::Span<'static, String>> {
    let mut spans = Vec::new();
    for (index, part) in summary.split(" · ").enumerate() {
        if index > 0 {
            spans.push(iced::widget::text::Span::new(" · "));
        }
        let span = iced::widget::text::Span::new(part.to_owned());
        spans.push(if index > 0 && part.ends_with(" failed") {
            span.color(design::destructive_color(&cosmic::theme::active()))
        } else {
            span
        });
    }
    spans
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
pub(super) fn code_surface<'a>(
    text: &'a str,
    caption: Option<&'a str>,
    limit: Option<f32>,
    copy: Option<Action>,
    target: Target,
) -> Element<'a, Message> {
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
        widget::container(design::caption(caption))
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

/// Keep outside paths absolute and never shorten lexical siblings of the cwd.
fn display_path<'a>(path: &'a str, cwd: &std::path::Path) -> &'a str {
    std::path::Path::new(path)
        .strip_prefix(cwd)
        .ok()
        .filter(|relative| {
            !relative.as_os_str().is_empty()
                && !relative
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
        })
        .and_then(std::path::Path::to_str)
        .unwrap_or(path)
}

struct PathLabel {
    display: String,
    absolute: String,
}

fn path_label(path: &str, cwd: &std::path::Path) -> PathLabel {
    let path = std::path::Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let absolute = absolute.to_string_lossy().into_owned();
    let display = display_path(&absolute, cwd).to_owned();
    PathLabel { display, absolute }
}

fn content_path(content: &ItemContent) -> Option<&str> {
    match content {
        ItemContent::Command {
            action: chat::CommandAction::Read,
            ..
        } => Some(turn::step_label(content)),
        ItemContent::Changes { changes, .. } if changes.len() == 1 => Some(&changes[0].path),
        _ => None,
    }
}

/// Resolve Read targets from the tool's directory while displaying paths
/// relative to the conversation. These directories can name different trees.
fn content_path_label(content: &ItemContent, cwd: &std::path::Path) -> Option<PathLabel> {
    let path = content_path(content)?;
    let source_cwd = match content {
        ItemContent::Command {
            working_directory, ..
        } => cwd.join(working_directory),
        _ => cwd.to_path_buf(),
    };
    let mut label = path_label(path, &source_cwd);
    label.display = display_path(&label.absolute, cwd).to_owned();
    Some(label)
}

fn diff_stat(changes: &[FileChange]) -> (usize, usize) {
    let (mut added, mut removed) = (0, 0);
    for change in changes {
        let mut hunk = false;
        for line in change.diff.lines() {
            if line.starts_with("diff --git ") {
                hunk = false;
            }
            if line.starts_with("@@") {
                hunk = true;
            }
            if line.starts_with('+') && (hunk || !line.starts_with("+++ ")) {
                added += 1;
            }
            if line.starts_with('-') && (hunk || !line.starts_with("--- ")) {
                removed += 1;
            }
        }
    }
    (added, removed)
}

fn change_view<'a>(change: &'a FileChange, cwd: &std::path::Path) -> Element<'a, Message> {
    let path = path_label(&change.path, cwd);
    let mut rows: Vec<Element<'_, Message>> = vec![
        widget::row::with_children(vec![
            design::caption(change_verb(&change.kind)).into(),
            crate::interaction::tooltip(
                design::body(path.display)
                    .font(iced::Font::MONOSPACE)
                    .width(Length::Fill),
                path.absolute,
            ),
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

fn search_result_view(result: &SearchResult, target: Target) -> Element<'_, Message> {
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let title = if result.title.is_empty() {
        result.url.as_str()
    } else {
        result.title.as_str()
    };
    if result.url.is_empty() {
        rows.push(selectable_prose::plain(
            title,
            design::TEXT_BODY,
            iced::Font::default(),
            target,
        ));
    } else {
        rows.push(selectable_prose::text(
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
        ));
        rows.push(selectable_prose::plain(
            result.url.as_str(),
            design::TEXT_CAPTION,
            iced::Font::default(),
            target,
        ));
    }
    if !result.snippet.is_empty() {
        rows.push(selectable_prose::plain(
            result.snippet.as_str(),
            design::TEXT_BODY,
            iced::Font::default(),
            target,
        ));
    }
    widget::column::with_children(rows)
        .spacing(space::XS)
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
        code_frame(
            match limit {
                Some(height) if !follows => {
                    block.view_from_start(self.syntax_theme, height, target, self.find.as_deref())
                }
                limit => block.view(self.syntax_theme, limit, target, self.find.as_deref()),
            },
            Some(caption.to_owned()),
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
        let summary = turn
            .steps
            .first()
            .filter(|_| turn.steps.len() == 1)
            .and_then(|placed| self.snapshot.items.get(placed.index))
            .filter(|item| {
                self.cache
                    .get(&item.id)
                    .and_then(|cached| cached.coordination.as_deref())
                    .is_none()
            })
            .and_then(|item| {
                let path = content_path(&item.content)?;
                let label = content_path_label(
                    &item.content,
                    &self.session.launch.recipe.working_directory,
                )?;
                Some(turn.summary.replacen(path, &label.display, 1))
            })
            .unwrap_or_else(|| turn.summary.clone());
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
            None => {
                line.push(
                    widget::icon(Icon::Utilities.handle())
                        .size(design::COMPACT_ICON_SIZE)
                        .into(),
                );
                None
            }
        };
        line.push(if turn.steps.len() > 1 && summary.contains(" failed") {
            iced::widget::rich_text(summary_spans(&summary))
                .font(design::heading_font())
                .size(design::TEXT_BODY)
                .width(Length::Fill)
                .into()
        } else {
            design::heading(summary.clone()).width(Length::Fill).into()
        });
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
            Some(elapsed) => format!("{summary} · {elapsed}"),
            None => summary.clone(),
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
        // A step that worked with another agent says what it did in the
        // conversation's own names rather than reprinting its command.
        let coordinated = cached.coordination.as_deref();
        let (verb, mut label) = match coordinated {
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
        // Keep primary commands and paths on one ellipsized line. Their
        // complete source stays available through expansion and copying.
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
            preview.push(design::body(label.clone()).into());
            if !cached.preview.text.is_empty() {
                preview.push(cached.preview.view());
            }
        } else {
            preview.push(cached.preview.view());
        }
        let cwd = &self.session.launch.recipe.working_directory;
        let path = coordinated
            .is_none()
            .then(|| content_path_label(&item.content, cwd))
            .flatten();
        if let Some(path) = &path {
            label = path.display.clone();
            preview = vec![crate::interaction::tooltip(
                design::body(path.display.clone())
                    .font(iced::Font::MONOSPACE)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::advanced::text::Ellipsize::End(
                        iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill),
                path.absolute.clone(),
            )];
        }
        match &item.content {
            ItemContent::Command {
                label: description,
                command,
                action: chat::CommandAction::Run,
                ..
            } if coordinated.is_none() && !description.is_empty() && description != command => {
                preview.push(
                    design::caption(turn::first_line(description).to_owned())
                        .wrapping(iced::widget::text::Wrapping::None)
                        .ellipsize(iced::advanced::text::Ellipsize::End(
                            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .width(Length::Fill)
                        .into(),
                );
            }
            _ => {}
        }
        preview.extend(cached.preview.tail_view());
        line.push(
            widget::column::with_children(preview)
                .spacing(GAP_XS)
                .width(Length::Fill)
                .into(),
        );
        if let ItemContent::Changes { changes, .. } = &item.content {
            let (added, removed) = diff_stat(changes);
            line.push(design::caption(format!("+{added} −{removed}")).into());
        }
        if let Some(status) = item.content.status() {
            let (indicator, _) = status_indicator(status);
            line.push(indicator);
        }
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
        .class(crate::ui_controls::control_style(
            open,
            match item.content.status() {
                Some(ToolStatus::Failed) => Some(design::Tone::Themed(design::destructive_color)),
                Some(ToolStatus::Denied | ToolStatus::Stopped) => {
                    Some(design::Tone::Themed(design::muted_color))
                }
                _ => None,
            },
        ))
        .padding([GAP_XS as u16, GAP_S as u16])
        .width(Length::Fill)
        .on_press(action(Action::Toggle(target, item.id.clone())));
        let mut controls = vec![self.message_copy(item)];
        if let Some(path) = &path {
            controls.insert(
                0,
                small_icon(
                    Icon::Copy,
                    "Copy full path",
                    Action::CopyLiteral(path.absolute.clone()),
                ),
            );
        }
        hover::view(
            widget::row::with_children(vec![
                header.into(),
                widget::Space::new()
                    .width(if path.is_some() { 48 } else { 24 })
                    .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(GAP_XS)
            .width(Length::Fill),
            widget::container(widget::row::with_children(controls).spacing(GAP_XS))
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

    pub(super) fn step_detail<'a>(
        &'a self,
        item: &'a chat::Item,
        cached: &'a CachedItem,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
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
                command,
                output,
                action: command_action,
                ..
            } => {
                if *command_action != chat::CommandAction::Read
                    || command != turn::step_label(&item.content)
                {
                    rows.push(self.tool_code_surface(
                        item,
                        cached,
                        CopyPart::Input,
                        "Command",
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
            ItemContent::Changes { changes, .. } => {
                if changes.is_empty() {
                    rows.push(design::caption("No file changes were reported.").into());
                }
                for (index, change) in changes.iter().enumerate() {
                    if changes.len() > 1 {
                        rows.push(change_view(
                            change,
                            &self.session.launch.recipe.working_directory,
                        ));
                    } else if let ChangeKind::Renamed { from } = &change.kind {
                        rows.push(design::caption(format!("from {from}")).into());
                    }
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
                rows.push(selectable_prose::plain(
                    query.as_str(),
                    design::TEXT_BODY,
                    iced::Font::default(),
                    self.target,
                ));
                if results.is_empty() {
                    rows.push(design::caption("No results were reported.").into());
                }
                rows.extend(
                    results
                        .iter()
                        .map(|result| search_result_view(result, self.target)),
                );
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
                    rows.push(selectable_prose::plain(
                        description.as_str(),
                        design::TEXT_BODY,
                        iced::Font::default(),
                        self.target,
                    ));
                }
                if path.starts_with("http://") || path.starts_with("https://") {
                    rows.push(
                        widget::button::link("Open image")
                            .on_press(action(Action::OpenUrl(path.clone())))
                            .into(),
                    );
                } else if !path.is_empty() && !path.starts_with("data:") {
                    rows.push(selectable_prose::plain(
                        path.as_str(),
                        design::TEXT_CAPTION,
                        iced::Font::default(),
                        self.target,
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

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn command_row_renders_command_before_description() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let item = chat::Item {
            id: "bash".into(),
            role: chat::Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Command {
                label: "Run tests for module 295".into(),
                command: "cargo test --lib module_295".into(),
                working_directory: "/project".into(),
                action: chat::CommandAction::Run,
                status: ToolStatus::Succeeded,
                exit_code: Some(0),
                duration_ms: Some(120),
                output: String::new(),
            },
        };
        let session = fresh_chat_session(
            Provider::Claude,
            TerminalId::new(),
            "/project".into(),
            &Account::system(Provider::Claude),
            false,
        )
        .unwrap();
        let handle = Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                items: vec![Arc::new(item.clone())],
                ..Snapshot::default()
            },
        ));
        let runtime = Runtime::new(
            Target {
                card: CardId::new(),
                generation: 1,
            },
            session,
            Connection::Live(handle),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        let mut element = runtime.step_header(&item, &runtime.cache[&item.id], false);
        let mut tree = Tree::new(&element);
        let renderer = crate::app::tests::headless_renderer();
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(956.0, 500.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let nodes = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .collect::<Vec<_>>();
        assert!(
            nodes
                .iter()
                .any(|node| node.node().label() == Some("Ran cargo test --lib module_295"))
        );
        assert!(nodes.iter().any(
            |node| node.node().label() == Some("Run tests for module 295")
                || node.node().value() == Some("Run tests for module 295")
        ));
    }

    #[test]
    fn mixed_summary_colors_only_the_failed_count() {
        let spans = summary_spans("Ran 5 commands · 4 ok · 1 failed");
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>(),
            "Ran 5 commands · 4 ok · 1 failed"
        );
        assert!(
            spans[..spans.len() - 1]
                .iter()
                .all(|span| span.color.is_none())
        );
        assert!(spans.last().unwrap().color.is_some());
    }

    #[test]
    fn relative_provider_paths_have_absolute_tooltips_and_copy_values() {
        let path = path_label("src/main.rs", std::path::Path::new("/repo/worktree"));
        assert_eq!(path.display, "src/main.rs");
        assert_eq!(path.absolute, "/repo/worktree/src/main.rs");
        let outside = path_label("/outside/main.rs", std::path::Path::new("/repo/worktree"));
        assert_eq!(outside.display, "/outside/main.rs");
        assert_eq!(outside.absolute, "/outside/main.rs");
    }

    #[test]
    fn read_headers_and_copy_use_the_tool_directory() {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced_accessibility::accesskit::{
            Action as NativeAction, ActionRequest, NodeId, Role, TreeId,
        };

        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(500.0, 500.0));
        for (directory, path, display, absolute) in [
            (
                "/repo/subdir",
                "file.rs",
                "subdir/file.rs",
                "/repo/subdir/file.rs",
            ),
            (
                "/outside",
                "file.rs",
                "/outside/file.rs",
                "/outside/file.rs",
            ),
            (
                "subdir",
                "file.rs",
                "subdir/file.rs",
                "/repo/subdir/file.rs",
            ),
            ("", "file.rs", "file.rs", "/repo/file.rs"),
            ("/outside", "/repo/src.rs", "src.rs", "/repo/src.rs"),
        ] {
            let item = chat::Item {
                id: "read".into(),
                role: chat::Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Command {
                    label: path.into(),
                    command: format!("cat {path}"),
                    working_directory: directory.into(),
                    action: chat::CommandAction::Read,
                    status: ToolStatus::Succeeded,
                    exit_code: Some(0),
                    duration_ms: None,
                    output: String::new(),
                },
            };
            let session = fresh_chat_session(
                Provider::Claude,
                TerminalId::new(),
                "/repo".into(),
                &Account::system(Provider::Claude),
                false,
            )
            .unwrap();
            let handle = Arc::new(ClientHandle::fixture(
                session.clone(),
                Snapshot {
                    ready: true,
                    items: vec![Arc::new(item.clone())],
                    ..Snapshot::default()
                },
            ));
            let runtime = Runtime::new(
                Target {
                    card: CardId::new(),
                    generation: 1,
                },
                session,
                Connection::Live(handle),
                true,
                crate::settings::EditorSyntaxTheme::default(),
            );
            let name = format!("Read {display}");
            let mut element = runtime.step_header(&item, &runtime.cache[&item.id], false);
            let mut tree = Tree::new(element.as_widget());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let nodes = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            assert!(
                nodes
                    .root()
                    .iter()
                    .chain(nodes.children())
                    .any(|node| node.node().label() == Some(name.as_str()))
            );
            let copy = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| {
                    node.node().role() == Role::Button
                        && node.node().label() == Some("Copy full path")
                })
                .expect("the actual row advertises its full-path copy control");
            let target_node: NodeId = copy.id().clone().into();
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &iced::Event::A11y(
                    widget::Id::from(target_node.0),
                    ActionRequest {
                        action: NativeAction::Click,
                        target_tree: TreeId::ROOT,
                        target_node,
                        data: None,
                    },
                ),
                Layout::new(&node),
                iced::mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut iced::advanced::Shell::new(&mut messages),
                &node.bounds(),
            );
            assert!(
                matches!(messages.as_slice(),
                [Message::ChatUi(Action::CopyLiteral(copied))] if copied == absolute),
                "the actual copy action uses the recorded tool directory"
            );

            let mut group = runtime.step_strip(&runtime.turns[0], Vec::new);
            let mut tree = Tree::new(group.as_widget());
            let node = group.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let nodes = group.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            assert!(
                nodes
                    .root()
                    .iter()
                    .chain(nodes.children())
                    .any(|node| node.node().label() == Some(name.as_str())),
                "the single-step group and row name the same file"
            );
        }
    }

    #[test]
    fn paths_are_relative_only_under_the_session_directory() {
        let cwd = std::path::Path::new("/repo/worktree");
        assert_eq!(
            display_path("/repo/worktree/src/main.rs", cwd),
            "src/main.rs"
        );
        assert_eq!(
            display_path("/repo/worktree-other/main.rs", cwd),
            "/repo/worktree-other/main.rs"
        );
        assert_eq!(
            display_path("/elsewhere/main.rs", cwd),
            "/elsewhere/main.rs"
        );
        assert_eq!(display_path("src/main.rs", cwd), "src/main.rs");
    }

    #[test]
    fn diff_statistics_exclude_headers_and_count_prefixes_in_hunks() {
        let changes = vec![FileChange {
            path: "src/main.rs".into(), kind: ChangeKind::Updated,
            diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+++ prefix in source\n context\n".into(),
        }];
        assert_eq!(diff_stat(&changes), (2, 1));
    }
}
