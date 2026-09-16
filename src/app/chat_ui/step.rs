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

/// The gutter column that carries a patch line's marker.
const PATCH_GUTTER_WIDTH: f32 = 8.0;

/// The rail a turn's steps hang from, and the space between it and them.
///
/// A continuous hairline down the left of the strip is what turns a list of
/// rows into one sequence of work. It is drawn as the container's left edge
/// rather than per row, so it cannot break between rows.
const RAIL_INSET: f32 = design::GRID_UNIT * 3.0;

/// The glyph and tone a step's status draws as.
///
/// Status is information, not a control, so it is a plain icon rather than one
/// of the buttons that used to line every row.
fn status_glyph(status: ToolStatus) -> (&'static str, Option<design::Tone>) {
    match status {
        ToolStatus::Running => (
            "content-loading-symbolic",
            Some(design::Tone::Themed(design::warning_color)),
        ),
        ToolStatus::Succeeded => ("emblem-ok-symbolic", None),
        ToolStatus::Failed => (
            "dialog-error-symbolic",
            Some(design::Tone::Themed(design::destructive_color)),
        ),
        ToolStatus::Denied => (
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
fn code_surface<'a>(
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
                        icon("edit-copy-symbolic", "Copy this block", true, copy),
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

/// One line of a unified patch, marked in its own gutter column.
///
/// The provider hands us a patch, not the two file revisions
/// `crate::diff_view` consumes, and a patch only describes the regions it
/// touches. Reconstructing both full revisions from it would mean inventing
/// the untouched remainder, so the patch is rendered as what it is.
///
/// The marker the patch already carries is promoted to its own column and
/// tinted, and the line gets a faint wash behind it. Neither the marker nor
/// the wash is drawn at full accent strength, because a saturated fill behind
/// monospace text costs the text the contrast that marking a change is for.
/// Both parts size themselves to their own text, so a wrapped line keeps its
/// marker aligned with its first row instead of stretching a sibling to a
/// height the row has not resolved yet.
fn patch_line(line: &str) -> Element<'_, Message> {
    let marker = line.as_bytes().first().copied();
    let (wash, ink): (
        Option<fn(&cosmic::Theme) -> iced::Color>,
        Option<fn(&cosmic::Theme) -> cosmic::iced::widget::text::Style>,
    ) = match marker {
        Some(b'+') => (Some(design::diff_added_color), Some(design::added_text)),
        Some(b'-') => (Some(design::diff_removed_color), Some(design::removed_text)),
        _ => (None, None),
    };
    // The marker is split off so the body keeps the reader's ink while the
    // gutter carries the color, and so the columns line up down the patch.
    let (gutter, rest) = match marker {
        Some(b'+') | Some(b'-') | Some(b' ') => line.split_at(1),
        _ => ("", line),
    };
    let body = widget::selectable_text(rest)
        .size(design::TEXT_BODY)
        .font(iced::Font::MONOSPACE)
        .width(Length::Fill);
    // A hunk header is structure rather than content, so it recedes to the
    // weight of the surface's other secondary labels.
    let body: Element<'_, Message> = if marker == Some(b'@') {
        body.class(cosmic::theme::Text::Custom(design::muted_text))
            .into()
    } else {
        body.into()
    };
    widget::container(
        widget::row::with_children(vec![
            widget::text::body(gutter)
                .size(design::TEXT_BODY)
                .font(iced::Font::MONOSPACE)
                .width(PATCH_GUTTER_WIDTH)
                .class(match ink {
                    Some(ink) => cosmic::theme::Text::Custom(ink),
                    None => cosmic::theme::Text::Default,
                })
                .into(),
            body,
        ])
        .spacing(GAP_XS),
    )
    .class(cosmic::theme::Container::custom(move |theme| {
        cosmic::widget::container::Style {
            background: wash.map(|wash| iced::Background::Color(wash(theme))),
            ..Default::default()
        }
    }))
    .padding([0, GAP_S as u16])
    .width(Length::Fill)
    .into()
}

/// How much of one file's patch is rendered before the rest is withheld.
///
/// A single generated-file edit can be tens of thousands of lines. Rendering
/// all of them costs one widget per line in a transcript that is rebuilt
/// whenever the turn advances, so a long patch is truncated with an explicit
/// count rather than silently.
const MAX_PATCH_LINES: usize = 400;

fn patch_view(diff: &str) -> Element<'_, Message> {
    let mut lines: Vec<Element<'_, Message>> = Vec::new();
    let mut total = 0usize;
    for line in diff.lines() {
        total += 1;
        if lines.len() < MAX_PATCH_LINES {
            lines.push(patch_line(line));
        }
    }
    if total > lines.len() {
        lines.push(
            widget::container(design::caption(format!(
                "{} further lines withheld from the inline patch.",
                total - lines.len()
            )))
            .padding([GAP_XS as u16, GAP_M as u16])
            .into(),
        );
    }
    widget::container(widget::column::with_children(lines).width(Length::Fill))
        .class(design::surface(
            Some(design::sunken),
            [design::RADIUS_S; 4],
            true,
        ))
        .padding([GAP_XS as u16, 0])
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
    if !change.diff.is_empty() {
        rows.push(patch_view(&change.diff));
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
    /// A turn's activity: one summary line, disclosing the individual steps.
    ///
    /// A finished turn collapses, because what the reader wants from it is the
    /// answer. A running turn opens, because what they want from it is to see
    /// what is happening. An explicit toggle overrides both.
    pub(super) fn step_strip<'a>(
        &'a self,
        key: &'a str,
        turn: &'a turn::Turn,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let target = self.target;
        let open = self.opened.get(key).copied().unwrap_or(turn.running);
        let mut line: Vec<Element<'a, Message>> = vec![
            widget::icon(crate::ui_controls::symbolic_icon(if open {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            }))
            .size(12)
            .into(),
        ];
        let tone = match turn.glyph().map(status_glyph) {
            Some((glyph, tone)) => {
                line.push(
                    widget::icon(crate::ui_controls::symbolic_icon(glyph))
                        .size(12)
                        .into(),
                );
                tone
            }
            None => None,
        };
        line.push(design::caption(turn.summary.as_str()).into());
        let header = widget::button::custom(
            widget::row::with_children(line)
                .spacing(GAP_S)
                .align_y(Alignment::Center),
        )
        .name(turn.summary.clone())
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
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        for placed in &turn.steps {
            let Some(item) = self.snapshot.items.get(placed.index) else {
                continue;
            };
            let Some(cached) = self.cache.get(&item.id) else {
                continue;
            };
            rows.push(self.step_view(item, cached, settings));
        }
        widget::column::with_children(vec![
            header.into(),
            // One hairline beside the rows rather than a border on each of
            // them, so the sequence reads as continuous. `Border` carries a
            // single width for all four edges, so a left-only edge has to be
            // its own sliver.
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
        let target = self.target;
        let open = self.expanded.contains(&item.id);
        let verb = turn::step_verb(&item.content);
        let label = turn::step_label(&item.content);
        let mut line: Vec<Element<'a, Message>> = vec![
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
        line.push(
            widget::text::body(label)
                .size(design::TEXT_BODY)
                .font(iced::Font::MONOSPACE)
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
        if let Some(detail) = &cached.detail {
            line.push(design::caption(detail.as_str()).into());
        }
        let name = if verb.is_empty() {
            label.to_owned()
        } else {
            format!("{verb} {label}")
        };
        let header = widget::button::custom(
            widget::row::with_children(line)
                .spacing(GAP_S)
                .align_y(Alignment::Center),
        )
        .name(name)
        .class(crate::ui_controls::control_style(open, None))
        .padding([GAP_XS as u16, GAP_S as u16])
        .width(Length::Fill)
        .on_press(action(Action::Toggle(target, item.id.clone())));
        if !open {
            return header.into();
        }
        let detail = self.step_detail(item, cached, settings);
        widget::column::with_children(vec![
            header.into(),
            widget::container(detail)
                .padding(iced::Padding::from([0.0, GAP_S]).bottom(GAP_S))
                .width(Length::Fill)
                .into(),
        ])
        .spacing(GAP_XS)
        .width(Length::Fill)
        .into()
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
        let target = self.target;
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        match &item.content {
            ItemContent::Text { .. } => {
                rows.push(
                    markdown::view(cached.markdown.items(), settings)
                        .map(|uri| action(Action::OpenUrl(uri.to_string())))
                        .into(),
                );
            }
            ItemContent::Command {
                command,
                working_directory,
                output,
                ..
            } => {
                rows.push(code_surface(
                    command.as_str(),
                    (!working_directory.is_empty()).then_some(working_directory.as_str()),
                    None,
                    Some(Action::Copy(target, item.id.clone(), CopyPart::Input)),
                ));
                if output.is_empty() {
                    rows.push(design::caption("No output.").into());
                } else {
                    rows.push(code_surface(
                        output.as_str(),
                        Some("Output"),
                        Some(Self::OUTPUT_HEIGHT),
                        Some(Action::Copy(target, item.id.clone(), CopyPart::Output)),
                    ));
                }
            }
            ItemContent::Changes { changes, .. } => {
                if changes.is_empty() {
                    rows.push(design::caption("No file changes were reported.").into());
                }
                rows.extend(changes.iter().map(change_view));
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
                    rows.push(code_surface(
                        input.as_str(),
                        Some("Input"),
                        Some(Self::OUTPUT_HEIGHT),
                        Some(Action::Copy(target, item.id.clone(), CopyPart::Input)),
                    ));
                }
                if output.is_empty() {
                    rows.push(design::caption("No output.").into());
                } else {
                    rows.push(code_surface(
                        output.as_str(),
                        Some("Output"),
                        Some(Self::OUTPUT_HEIGHT),
                        Some(Action::Copy(target, item.id.clone(), CopyPart::Output)),
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
