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
fn code_surface<'a>(text: &'a str, limit: Option<f32>) -> Element<'a, Message> {
    let inner = widget::container(
        widget::selectable_text(text)
            .size(design::TEXT_BODY)
            .font(iced::Font::MONOSPACE)
            .width(Length::Fill),
    )
    .padding(design::GRID_UNIT * 2.0);
    let scrolled: Element<'_, Message> = match limit {
        Some(height) => widget::container(
            widget::scrollable(inner)
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .height(Length::Shrink),
        )
        .max_height(height)
        .into(),
        None => inner.into(),
    };
    widget::container(scrolled)
        .class(cosmic::theme::Container::Card)
        .width(Length::Fill)
        .into()
}

/// One line of a unified patch, tinted by what it does.
///
/// The provider hands us a patch, not the two file revisions
/// `crate::diff_view` consumes, and a patch only describes the regions it
/// touches. Reconstructing both full revisions from it would mean inventing
/// the untouched remainder, so the patch is rendered as what it is, with the
/// same added and removed tones the rest of the shell uses.
fn patch_line(line: &str) -> Element<'_, Message> {
    let tone = match line.as_bytes().first() {
        Some(b'+') => Some(design::Tone::Themed(design::diff_added_color)),
        Some(b'-') => Some(design::Tone::Themed(design::diff_removed_color)),
        _ => None,
    };
    let text = widget::selectable_text(line)
        .size(design::TEXT_BODY)
        .font(iced::Font::MONOSPACE)
        .width(Length::Fill);
    let text: Element<'_, Message> = match line.as_bytes().first() {
        // A hunk header is structural rather than content, so it recedes to
        // the same weight as the other secondary labels on the surface.
        Some(b'@') => text
            .class(cosmic::theme::Text::Custom(design::muted_text))
            .into(),
        _ => text.into(),
    };
    match tone {
        Some(tone) => widget::container(text)
            .class(cosmic::theme::Container::custom(move |theme| {
                cosmic::widget::container::Style {
                    background: Some(iced::Background::Color(tone.resolve(theme))),
                    ..Default::default()
                }
            }))
            .padding([0, design::GRID_UNIT as u16])
            .width(Length::Fill)
            .into(),
        None => widget::container(text)
            .padding([0, design::GRID_UNIT as u16])
            .width(Length::Fill)
            .into(),
    }
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
            design::caption(format!(
                "{} further lines withheld from the inline patch.",
                total - lines.len()
            ))
            .into(),
        );
    }
    widget::container(widget::column::with_children(lines).width(Length::Fill))
        .class(cosmic::theme::Container::Card)
        .padding([design::GRID_UNIT as u16, 0])
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
            design::body(change.path.as_str())
                .width(Length::Fill)
                .into(),
        ])
        .spacing(design::GRID_UNIT * 2.0)
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
        .spacing(design::GRID_UNIT)
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
        let summary = self.strip_summary(turn);
        let (glyph, tone) = turn
            .outcome
            .map_or(("emblem-ok-symbolic", None), status_glyph);
        let header = widget::button::custom(
            widget::row::with_children(vec![
                widget::icon(crate::ui_controls::symbolic_icon(if open {
                    "pan-down-symbolic"
                } else {
                    "pan-end-symbolic"
                }))
                .size(design::ROW_ICON_SIZE)
                .into(),
                widget::icon(crate::ui_controls::symbolic_icon(glyph))
                    .size(design::ROW_ICON_SIZE)
                    .into(),
                design::caption(summary.clone()).width(Length::Fill).into(),
            ])
            .spacing(design::GRID_UNIT * 2.0)
            .align_y(Alignment::Center),
        )
        .name(summary)
        .class(crate::ui_controls::control_style(false, tone))
        .padding([design::GRID_UNIT as u16, design::GRID_UNIT as u16 * 2])
        .width(Length::Fill)
        .on_press(action(Action::ToggleTurn(target, key.to_owned())));
        if !open {
            return header.into();
        }
        let mut rows: Vec<Element<'a, Message>> = vec![header.into()];
        for placed in &turn.steps {
            let Some(item) = self.snapshot.items.get(placed.index) else {
                continue;
            };
            let Some(cached) = self.cache.get(&item.id) else {
                continue;
            };
            rows.push(self.step_view(item, cached, settings));
        }
        widget::column::with_children(rows)
            .spacing(design::GRID_UNIT / 2.0)
            .width(Length::Fill)
            .into()
    }

    /// The collapsed strip's own line.
    ///
    /// A single step names itself, because "1 step" tells the reader nothing
    /// they could not see from the row's absence. Several steps are counted,
    /// with a failure called out, because that is the one thing a collapsed
    /// strip must not hide.
    fn strip_summary(&self, turn: &turn::Turn) -> String {
        let mut summary = match turn.steps.as_slice() {
            [only] => match self.snapshot.items.get(only.index) {
                Some(item) => {
                    let verb = turn::step_verb(&item.content);
                    let label = turn::step_label(&item.content);
                    if verb.is_empty() {
                        label.to_owned()
                    } else {
                        format!("{verb} {label}")
                    }
                }
                None => "1 step".to_owned(),
            },
            steps => format!("{} steps", steps.len()),
        };
        match turn.outcome {
            Some(ToolStatus::Failed) => summary.push_str(" · failed"),
            Some(ToolStatus::Denied) => summary.push_str(" · denied"),
            Some(ToolStatus::Running) => summary.push_str(" · working"),
            Some(ToolStatus::Succeeded) | None => {}
        }
        summary
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
                .size(design::ROW_ICON_SIZE)
                .into(),
        ];
        if !verb.is_empty() {
            line.push(design::caption(verb).into());
        }
        line.push(
            design::body(label)
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
                .spacing(design::GRID_UNIT * 2.0)
                .align_y(Alignment::Center),
        )
        .name(name)
        .class(crate::ui_controls::control_style(open, None))
        .padding([design::GRID_UNIT as u16, design::GRID_UNIT as u16 * 2])
        .width(Length::Fill)
        .on_press(action(Action::Toggle(target, item.id.clone())));
        if !open {
            return widget::container(header)
                .padding([0, 0, 0, design::GRID_UNIT as u16 * 4])
                .width(Length::Fill)
                .into();
        }
        let detail = self.step_detail(item, cached, settings);
        widget::container(
            widget::column::with_children(vec![header.into(), detail])
                .spacing(design::GRID_UNIT)
                .width(Length::Fill),
        )
        .padding([0, 0, 0, design::GRID_UNIT as u16 * 4])
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
            ItemContent::Text { text, .. } => {
                if self.raw.contains(&item.id) {
                    rows.push(code_surface(text.as_str(), Some(Self::OUTPUT_HEIGHT)));
                } else {
                    rows.push(
                        markdown::view(cached.markdown.items(), settings)
                            .map(|uri| action(Action::OpenUrl(uri.to_string())))
                            .into(),
                    );
                }
            }
            ItemContent::Command {
                command,
                working_directory,
                output,
                ..
            } => {
                rows.push(code_surface(command.as_str(), None));
                if !working_directory.is_empty() {
                    rows.push(design::caption(format!("in {working_directory}")).into());
                }
                if output.is_empty() {
                    rows.push(design::caption("No output.").into());
                } else {
                    rows.push(code_surface(output.as_str(), Some(Self::OUTPUT_HEIGHT)));
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
                    rows.push(design::caption("Input").into());
                    rows.push(code_surface(input.as_str(), Some(Self::OUTPUT_HEIGHT)));
                }
                if output.is_empty() {
                    rows.push(design::caption("No output.").into());
                } else {
                    rows.push(design::caption("Output").into());
                    rows.push(code_surface(output.as_str(), Some(Self::OUTPUT_HEIGHT)));
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
        // A step's controls appear with its detail rather than beside every
        // collapsed row, which is what put twelve unconditional buttons on a
        // surface whose reader wanted two.
        let mut controls: Vec<Element<'a, Message>> = Vec::new();
        if matches!(item.content, ItemContent::Text { .. }) {
            controls.push(icon(
                "text-x-generic-symbolic",
                if self.raw.contains(&item.id) {
                    "Show formatted text"
                } else {
                    "Show Markdown source"
                },
                true,
                Action::Raw(target, item.id.clone()),
            ));
        }
        controls.push(icon(
            "edit-copy-symbolic",
            "Copy this step",
            true,
            Action::Copy(target, item.id.clone(), CopyPart::Text),
        ));
        rows.push(
            widget::row::with_children(controls)
                .spacing(design::GRID_UNIT)
                .align_y(Alignment::Center)
                .into(),
        );
        widget::column::with_children(rows)
            .spacing(design::GRID_UNIT)
            .width(Length::Fill)
            .into()
    }
}
