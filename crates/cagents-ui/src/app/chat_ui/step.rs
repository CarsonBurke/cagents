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
use crate::chat::{ChangeKind, FileChange, ToolStatus};

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

/// Where a group's thread line hangs: under the centre of its kind icon.
const THREAD_RAIL_OFFSET: f32 =
    GAP_S + design::LEADING_ICON_SIZE as f32 / 2.0 - design::BORDER_WIDTH / 2.0;

/// The glyph that stands for what a step was doing, so a reader scanning the
/// left edge of a strip can tell reading from running from editing without
/// reading any text.
fn kind_glyph(content: &ItemContent) -> Icon {
    match content {
        _ if chat::is_stopped_notice(content) => Icon::Ended,
        ItemContent::Text { .. } => Icon::Reasoning,
        ItemContent::Command { action, .. } => match action {
            chat::CommandAction::Read => Icon::TextFile,
            chat::CommandAction::Search => Icon::Search,
            chat::CommandAction::List => Icon::Folder,
            chat::CommandAction::Run => Icon::Terminal,
        },
        ItemContent::Changes { .. } => Icon::Changes,
        ItemContent::WebSearch { .. } => Icon::Browser,
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
    code_frame(
        body,
        caption.map(str::to_owned),
        copy,
        super::text_copy::CopyState::Ready,
    )
}

/// `body` in its surface, under a header with its `caption` and, with `copy`,
/// a Copy showing `state`: what its last copy said.
pub(super) fn code_frame<'a>(
    body: Element<'a, Message>,
    caption: Option<String>,
    copy: Option<Action>,
    state: super::text_copy::CopyState,
) -> Element<'a, Message> {
    code_frame_with(body, caption, copy, state, None)
}

/// How wide a code frame's header keeps clear for a control beside its
/// Copy, such as Copy as sent.
const ALSO_WIDTH: f32 = 112.0;

/// [`code_frame`] with `also` shown beside its Copy while it is hovered,
/// and kept shown while it says what it did.
fn code_frame_with<'a>(
    body: Element<'a, Message>,
    caption: Option<String>,
    copy: Option<Action>,
    state: super::text_copy::CopyState,
    also: Option<(Element<'a, Message>, bool)>,
) -> Element<'a, Message> {
    let telling = state.is_telling() || also.as_ref().is_some_and(|(_, telling)| *telling);
    let also = also.map(|(also, _)| also);
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
    // What the last copy said is in the header, after its caption and clear
    // of the Copy over its end, so a long refusal gives way to both: it is
    // laid out last and ends in an ellipsis rather than taking the Copy's
    // room or drawing over the caption.
    let mut header = vec![design::caption(caption).into()];
    if copy.is_some()
        && let Some(said) = state.said()
    {
        header.push(
            widget::container(said)
                .width(Length::Fill)
                .align_x(Alignment::End)
                .into(),
        );
    }
    let block = widget::column::with_children(vec![
        widget::container(
            widget::row::with_children(header)
                .spacing(GAP_S)
                .align_y(Alignment::Center),
        )
        .padding(
            iced::Padding::ZERO
                .right(24.0 + GAP_XS + if also.is_some() { ALSO_WIDTH } else { 0.0 }),
        )
        .width(Length::Fill)
        .height(24)
        .align_y(Alignment::Center)
        .into(),
        body,
    ])
    .spacing(GAP_XS)
    .width(Length::Fill);
    match copy {
        Some(copy) => super::hover::over_pinned(
            block,
            widget::container(
                widget::row::with_children(also.into_iter().chain([small_icon(
                    state.icon(),
                    state.label("Copy this block"),
                    copy,
                )]))
                .spacing(GAP_XS)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::Start),
            telling,
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

/// What the note beneath an input or output shown as an excerpt says,
/// depending on whether its Copy reads the whole of it.
pub(super) fn excerpt_note(part: &CopyPart, copies_whole: bool) -> &'static str {
    match (part, copies_whole) {
        (CopyPart::Input, _) => {
            "Only the start and end of this input are shown. Copy includes only what is shown."
        }
        (_, true) => "Only the start and end of this output are shown. Copy includes all of it.",
        (_, false) => {
            "Only the start and end of this output are shown. Copy includes only what is shown."
        }
    }
}

fn change_view(change: &FileChange) -> Element<'static, Message> {
    let mut rows: Vec<Element<'_, Message>> = vec![
        widget::row::with_children(vec![
            design::caption(change_verb(&change.kind)).into(),
            design::body(change.path.clone())
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

impl Runtime {
    /// Group indices refer to the selected transcript, including child streams.
    pub(super) fn step_items(&self) -> &[Arc<chat::Item>] {
        self.selected_subagent
            .as_ref()
            .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id))
            .map_or(self.snapshot.items.as_slice(), |agent| {
                agent.items.as_slice()
            })
    }

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
        // A long output's row holds only its start and end; Copy reads the
        // whole of it.
        let copy = match (&part, self.excerpted_output_source(item)) {
            (CopyPart::Output, Some(source)) => {
                Action::CopyStep(target, id.clone(), CopyPart::Output, source)
            }
            _ => Action::Copy(target, id.clone(), part.clone()),
        };
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
        let logical = selectable_prose::Logical {
            identity: selection::logical_identity(
                &item.id,
                &match part {
                    CopyPart::Input => vec![4],
                    CopyPart::Change(index) => vec![6, index as u32],
                    _ => vec![5],
                },
            ),
            start: 0,
        };
        // Where the detail reads a call's input or result as lines, the JSON
        // stays a click away, exact.
        let raw = match (&part, &item.content) {
            (CopyPart::Input, ItemContent::Tool { input: raw, .. }) => Some((
                raw,
                CopyPart::Sent,
                "Copy the input exactly as the provider sent it",
            )),
            (CopyPart::Output, ItemContent::Tool { output: raw, .. }) => Some((
                raw,
                CopyPart::Received,
                "Copy the result exactly as the tool returned it",
            )),
            _ => None,
        }
        .filter(|(raw, ..)| *block.text != ***raw);
        // What a call was given wraps, and so does a result read as lines;
        // what a call printed and a patch keep their columns and scroll
        // sideways.
        let view = if matches!(part, CopyPart::Input) || raw.is_some() {
            tool_text::ToolText::view_wrapped_with_reader
        } else {
            tool_text::ToolText::view_logical_with_reader
        };
        let raw = raw.map(|(_, part, description)| {
            let state = self.copy_state(&item.id, Some(&part));
            (
                crate::ui_controls::button::quiet("Copy JSON")
                    .instead(
                        "Copied",
                        matches!(state, super::text_copy::CopyState::Copied),
                    )
                    .instead(
                        "Copy again",
                        matches!(state, super::text_copy::CopyState::Refused(_)),
                    )
                    .description(description)
                    .on_press(action(Action::Copy(target, id.clone(), part)))
                    .into(),
                state.is_telling(),
            )
        });
        code_frame_with(
            view(
                block,
                self.syntax_theme,
                limit.map(|height| (height, follows)),
                target,
                self.find.as_deref(),
                logical,
                self.scroll_memory.clone(),
                self.selected_subagent.clone(),
            ),
            Some(caption.to_owned()),
            Some(copy),
            self.copy_state(&item.id, Some(&part)),
            raw,
        )
    }

    /// A collapsed strip's line. A lone step that worked with other agents
    /// names them as its open row does, by what this conversation calls them,
    /// which grouping, formed only when the transcript's structure changes,
    /// cannot know.
    #[cfg(test)]
    pub(super) fn strip_summary(&self, turn: &turn::Turn) -> String {
        self.strip_headline(turn).plain()
    }

    /// A collapsed strip's line as it is drawn. A lone step on one file
    /// names it as its open row does, so the two never disagree.
    fn strip_headline(&self, turn: &turn::Turn) -> disclosure::Headline {
        let [only] = turn.steps.as_slice() else {
            return disclosure::Headline::Text(turn.summary.clone());
        };
        let Some(item) = self.step_items().get(only.index) else {
            return disclosure::Headline::Text(turn.summary.clone());
        };
        let cached = self.presentation_cache().get(&item.id);
        if let Some(coordinated) = cached.and_then(|cached| cached.coordination.as_deref()) {
            let line = coordinated.summary_naming(&|id| self.listed_agent_name(id));
            return disclosure::Headline::Text(format!(
                "{}{}",
                turn::headline(line),
                turn.state_suffix()
            ));
        }
        let title = cached.and_then(|cached| cached.title_of(item));
        match self.named_file(item, title) {
            Some((verb, path)) => disclosure::Headline::Path {
                verb: verb.to_owned(),
                path: format!("{path}{}", turn.state_suffix()),
            },
            None => disclosure::Headline::Text(turn.summary.clone()),
        }
    }

    /// Whether a group's strip is open: the reader's choice, or else the
    /// setting.
    pub(super) fn group_open(&self, turn: &turn::Turn) -> bool {
        self.opened
            .get(&turn.key)
            .copied()
            .unwrap_or(self.tool_groups_expanded)
    }

    /// Carries the reader opening or closing a group's only step, drawn as
    /// its own row, to the group: when a second step joins it, the strip the
    /// two share opens over the step the reader was reading, and closing
    /// that step later folds only the step.
    pub(super) fn follow_lone_step(&mut self, id: &str, open: bool) {
        let items = &self.snapshot.items;
        let Some(key) = self
            .turns
            .iter()
            .find(|turn| match turn.steps.as_slice() {
                [only] => items.get(only.index).is_some_and(|item| item.id == id),
                _ => false,
            })
            .map(|turn| turn.key.clone())
        else {
            return;
        };
        self.find_disclosure.manual_turn(&key);
        if open {
            self.opened.edit().insert(key, true);
        } else {
            self.opened.edit().remove(&key);
        }
    }

    /// The step a group is drawn as, when it is drawn as its only step's
    /// row. While the agent thinks after that step with its row closed, the
    /// group is its closed strip instead, of the row's height and at its
    /// place, whose time counts on as the agent works; opening it opens the
    /// row, as the strip stands for that step.
    pub(super) fn lone_step<'t>(&self, turn: &'t turn::Turn) -> Option<&'t turn::Placed> {
        let [only] = turn.steps.as_slice() else {
            return None;
        };
        let open = || {
            self.step_items()
                .get(only.index)
                .is_some_and(|item| self.item_expanded(&item.id))
        };
        (!turn.thinking || open()).then_some(only)
    }

    /// Whether a group's strip stands for its only step, as it does while
    /// the agent thinks after that step with the step's row closed.
    pub(super) fn strip_stands_for_step(&self, turn: &turn::Turn) -> bool {
        turn.steps.len() == 1 && self.lone_step(turn).is_none()
    }

    /// Whether a group's steps are on screen: a lone step always is, as its
    /// own row, and the steps of several when their strip is open.
    pub(super) fn steps_shown(&self, turn: &turn::Turn) -> bool {
        self.lone_step(turn).is_some() || !self.strip_stands_for_step(turn) && self.group_open(turn)
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
        let open = !self.strip_stands_for_step(turn) && self.group_open(turn);
        let elapsed = if turn.running {
            self.source_now_ms()
                .and_then(|now| turn.duration_label(now))
        } else {
            turn.duration_label(0)
        };
        let toggle = Action::ToggleTurn(target, key.to_owned());
        // Messages fold into a row drawn as each of theirs is, which keeps
        // how they stand in view as their words fade.
        let header = match turn
            .messages
            .then(|| self.message_run_row(turn, open, toggle.clone()))
            .flatten()
        {
            Some(header) => header,
            None => {
                let text = self.strip_headline(turn);
                let name = text.plain();
                let mut kinds = turn.steps.iter().filter_map(|step| {
                    self.step_items().get(step.index).map(|item| {
                        self.presentation_cache()
                            .get(&item.id)
                            .and_then(|cached| cached.coordination.as_deref())
                            .and_then(Coordinated::glyph)
                            .unwrap_or_else(|| kind_glyph(&item.content))
                    })
                });
                let first = kinds.next().unwrap_or(Icon::Utilities);
                let kind = if kinds.all(|kind| kind == first) {
                    first
                } else {
                    Icon::Utilities
                };
                disclosure::row(
                    text,
                    name,
                    kind,
                    self.strip_status(turn),
                    elapsed,
                    Some(open),
                    Some(toggle),
                )
            }
        };
        // Keep the header at the same tree path through repeated keyboard
        // toggles. Only the body is appended when the group opens.
        let mut content = vec![header];
        if open {
            let rows = steps();
            content.push(
                // One hairline beside the rows rather than a border on each of
                // them, so the sequence reads as continuous. `Border` carries a
                // single width for all four edges, so a left-only edge has to be
                // its own sliver.
                // The rail hangs from the kind icon so its nested steps read
                // as one thread below the group's summary.
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
            );
        }
        // Each nested row brings the air above it, so the strip has none of
        // its own: its rows are as far apart as any two rows of the
        // timeline, and so are its last row and the row after the strip.
        widget::column::with_children(content)
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
        self.step_row(
            item,
            cached,
            settings,
            self.step_status(item).map(Into::into),
        )
    }

    /// A group's only step, drawn in place of the group. A strip over one
    /// row would repeat that row's title and icon above it, so the row
    /// stands alone, and carries the activity indicator the strip would have.
    pub(super) fn lone_step_view(
        &self,
        turn: &turn::Turn,
        item: &chat::Item,
        cached: &CachedItem,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        // Thinking after the step leaves it as it ended; the closing line
        // says the agent is still at work.
        let status = if turn.thinking {
            turn.outcome.map(Into::into)
        } else {
            self.strip_status(turn)
        };
        self.step_row(item, cached, settings, status)
    }

    fn step_row(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        settings: markdown::Settings,
        status: Option<disclosure::Status>,
    ) -> Element<'static, Message> {
        // Where the turn was stopped: a marker with nothing to disclose and
        // no time of its own.
        if chat::is_stopped_notice(&item.content) {
            return disclosure::row(
                chat::STOPPED_NOTICE.to_string(),
                chat::STOPPED_NOTICE.to_string(),
                Icon::Ended,
                None,
                None,
                None,
                None,
            );
        }
        let open = self.item_expanded(&item.id);
        let mut rows = vec![self.step_button(item, cached, open, status)];
        if open {
            rows.push(
                widget::container(super::view::message_actions(
                    self.step_detail(item, cached, settings),
                    self.message_copy(item),
                    Alignment::Start,
                    self.copy_note_shown(&item.id),
                ))
                .padding(iced::Padding::from([0.0, GAP_S]).bottom(GAP_S))
                .width(Length::Fill)
                .into(),
            );
        }
        widget::column::with_children(rows)
            .spacing(GAP_XS)
            .width(Length::Fill)
            .into()
    }

    /// The header occupies the whole row; copy and time-stamp controls remain
    /// available with the expanded detail instead of shifting its chevron.
    #[cfg(test)]
    pub(super) fn step_header(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        open: bool,
    ) -> Element<'static, Message> {
        self.step_button(item, cached, open, self.step_status(item).map(Into::into))
    }

    /// The verb and file of a step on one file, the file named the way the
    /// agent working in its directory names it, by the part that tells it
    /// apart. A command names the directory it ran in; anything else ran in
    /// the conversation's.
    fn named_file(
        &self,
        item: &chat::Item,
        title: Option<&tool_title::Title>,
    ) -> Option<(&'static str, String)> {
        if !turn::names_path(&item.content, title) {
            return None;
        }
        let (verb, label) = turn::step_title(&item.content, title);
        let directory = match &item.content {
            ItemContent::Command {
                working_directory, ..
            } if !working_directory.is_empty() => std::path::Path::new(working_directory.as_str()),
            _ => self.session.launch.recipe.working_directory.as_path(),
        };
        Some((verb, turn::shown_path(&label, directory).into_owned()))
    }

    /// The summary line's toggle, without the actions placed beside it.
    fn step_button(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        open: bool,
        status: Option<disclosure::Status>,
    ) -> Element<'static, Message> {
        let target = self.target;
        // A step that worked with another agent says what it did in the
        // conversation's own names rather than reprinting its command.
        let coordinated = cached.coordination.as_deref();
        let (verb, label, path) = match coordinated {
            Some(coordinated) => {
                let (verb, label) = coordinated.line(&|id| self.listed_agent_name(id));
                (verb, label, false)
            }
            None => match self.named_file(item, cached.title_of(item)) {
                Some((verb, path)) => (verb.to_owned(), path, true),
                None => {
                    let (verb, label) = turn::step_title(&item.content, cached.title_of(item));
                    (verb.to_owned(), label.into_owned(), false)
                }
            },
        };
        let glyph = coordinated
            .and_then(Coordinated::glyph)
            .unwrap_or_else(|| kind_glyph(&item.content));
        // A step still running without the transcript's one activity
        // indicator is marked as under way all the same, by a still dot.
        let status = status.or_else(|| {
            (item.content.status() == Some(ToolStatus::Running))
                .then_some(disclosure::Status::Working)
        });
        let mut said = label;
        if let ItemContent::Command {
            exit_code: Some(code),
            ..
        } = item.content
            && code != 0
        {
            said.push_str(&format!(" · exit {code}"));
        } else if status.is_none()
            && let Some(word) = item.content.status().and_then(unsettled_word)
        {
            // Where the row marks how it stands, its words do not say it
            // again; only a row left without a mark does.
            said.push_str(&format!(" · {word}"));
        }
        let label = if verb.is_empty() {
            said.clone()
        } else {
            format!("{verb} {said}")
        };
        let elapsed = disclosure::item_duration(item, self.source_now_ms());
        let mut name = label.clone();
        if let Some(description) = turn::command_description(&item.content) {
            name.push_str(&format!(" · {description}"));
        }
        let headline = if path {
            disclosure::Headline::Path { verb, path: said }
        } else {
            disclosure::Headline::Text(turn::headline(label))
        };
        disclosure::row(
            headline,
            name,
            glyph,
            status,
            elapsed,
            Some(open),
            Some(Action::Toggle(target, item.id.clone())),
        )
    }

    /// What a step's row draws for its status. A step inside a strip leaves
    /// the activity indicator to the strip above it; in a subagent's own
    /// transcript, where steps stand alone, only the newest row carries it.
    /// A running step that does not keeps its kind glyph, while its verb and
    /// counting time still say it is running.
    pub(super) fn step_status(&self, item: &chat::Item) -> Option<ToolStatus> {
        let newest = self.selected_subagent.is_some()
            && self
                .step_items()
                .last()
                .is_some_and(|last| last.id == item.id);
        item.content
            .status()
            .filter(|status| *status != ToolStatus::Running || newest)
    }

    /// Says, beneath an input or output shown as an excerpt, that only its
    /// start and end are shown and what Copy takes, so the line marking the
    /// gap is not mistaken for something the step printed.
    fn excerpt_note(&self, item: &chat::Item, part: CopyPart) -> Option<Element<'static, Message>> {
        let shown = match (&part, &item.content) {
            (CopyPart::Input, ItemContent::Tool { input, .. }) => input,
            (
                CopyPart::Output,
                ItemContent::Command { output, .. } | ItemContent::Tool { output, .. },
            ) => output,
            _ => return None,
        };
        let copies_whole =
            matches!(part, CopyPart::Output) && self.excerpted_output_source(item).is_some();
        (copies_whole || chat::is_excerpt(shown)).then(|| {
            design::caption(excerpt_note(&part, copies_whole))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into()
        })
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
        // What a coordinating step said and collected reads as messages,
        // ahead of the command and output they were printed in.
        if let Some(coordinated) = cached.coordination.as_deref() {
            if coordinated.is_card() {
                rows.extend(self.row_detail(item, cached, coordinated, settings));
            } else {
                rows.extend(self.arrived(item, cached, coordinated, "From", settings));
            }
        }
        match &item.content {
            // A notice's body is a log line or a raw result, drawn verbatim.
            ItemContent::Delivery {
                kind: chat::DeliveryKind::Notice,
                ..
            } => rows.push(literal_content::notice_body(
                self,
                item,
                cached,
                settings,
                Some(Self::OUTPUT_HEIGHT),
            )),
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
                // The provider's own account of what the command is for: the
                // closed row names the command, so this is where it is read.
                if let Some(description) = turn::command_description(&item.content) {
                    rows.push(design::body(description.to_owned()).into());
                }
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
                    rows.extend(self.excerpt_note(item, CopyPart::Output));
                }
            }
            ItemContent::Changes { changes, status } => {
                if changes.is_empty() {
                    rows.push(
                        design::caption(if *status == ToolStatus::Running {
                            "The edit is still being written."
                        } else {
                            "No file changes were reported."
                        })
                        .into(),
                    );
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
            ItemContent::WebSearch { .. } => {
                rows.push(literal_content::search_view(self, item, cached, settings))
            }
            ItemContent::Tool { input, output, .. } => {
                if !chat::readable::is_empty(input) {
                    rows.push(self.tool_code_surface(
                        item,
                        cached,
                        CopyPart::Input,
                        "Input",
                        Some(Self::OUTPUT_HEIGHT),
                    ));
                    rows.extend(self.excerpt_note(item, CopyPart::Input));
                }
                if cached
                    .coordination
                    .as_deref()
                    .is_some_and(|coordinated| coordinated.reading.output_is_agent_states())
                {
                    // The agents' own rows say how each one is; the provider's
                    // map of their states is not for a reader.
                } else if output.is_empty() {
                    rows.push(design::caption("No output.").into());
                } else {
                    rows.push(self.tool_code_surface(
                        item,
                        cached,
                        CopyPart::Output,
                        "Output",
                        Some(Self::OUTPUT_HEIGHT),
                    ));
                    rows.extend(self.excerpt_note(item, CopyPart::Output));
                }
            }
            ItemContent::Image { path, .. } => {
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
                if let Some(description) =
                    literal_content::image_field(self, item, cached, 0, settings)
                {
                    rows.push(description);
                }
                if let Some(literal_content::Content::Image { url: Some(url), .. }) =
                    self.prepared_literal(item)
                {
                    rows.push(
                        widget::button::link("Open image")
                            .on_press(action(Action::OpenSharedUrl(self.target, url.clone())))
                            .into(),
                    );
                } else {
                    let mut caption = settings;
                    caption.text_size = design::TEXT_CAPTION.into();
                    if let Some(path) = literal_content::image_field(self, item, cached, 1, caption)
                    {
                        rows.push(path);
                    }
                }
            }
        }
        // Commands, output and tool inputs keep their own copy controls;
        // the expanded step adds whole-message actions beneath this detail.
        widget::column::with_children(rows)
            .spacing(space::XS)
            .width(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree, operation::Focusable},
    };

    #[derive(Default)]
    struct FocusHeader {
        assign: bool,
        focused: Option<bool>,
    }

    impl Operation for FocusHeader {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }

        fn focusable(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn Focusable,
        ) {
            if self.focused.is_none() {
                if self.assign {
                    state.focus();
                }
                self.focused = Some(state.is_focused());
            }
        }
    }

    /// A native Enter press must keep reaching the same header after both
    /// disclosure transitions, including the group that used to change its
    /// root widget when it opened.
    #[test]
    fn step_and_group_headers_retain_keyboard_focus_through_repeated_toggles() {
        let item = chat::Item {
            id: "keyboard-command".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Command {
                label: "Check the fixture".into(),
                command: "git status --short".into(),
                working_directory: "/project".into(),
                action: chat::CommandAction::Run,
                status: ToolStatus::Succeeded,
                exit_code: Some(0),
                duration_ms: Some(12_000),
                output: "Working tree clean".into(),
            },
            started_at: Some(1_000),
            at: Some(13_000),
        };
        let renderer = crate::app::tests::headless_renderer();
        let viewport = iced::Rectangle::with_size(iced::Size::new(275.0, 1_000.0));
        let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
        for group in [false, true] {
            let (mut runtime, _handle) =
                super::super::view::prepared_runtime_for_test(std::slice::from_ref(&item));
            let key = runtime.turns[0].key.clone();
            let build = |runtime: &Runtime| {
                if group {
                    runtime.step_strip(&runtime.turns[0], Vec::new)
                } else {
                    runtime.step_view(&item, &runtime.cache[&item.id], settings)
                }
            };
            let mut tree = Tree::new(build(&runtime).as_widget());
            for (index, open) in [false, true, false].into_iter().enumerate() {
                runtime.opened.edit().insert(key.clone(), open);
                if open {
                    runtime.expanded.edit().insert(item.id.clone());
                } else {
                    runtime.expanded.edit().remove(&item.id);
                }
                let mut element = build(&runtime);
                tree.diff(element.as_widget_mut());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, viewport.size()),
                );
                let mut focus = FocusHeader {
                    assign: index == 0,
                    ..Default::default()
                };
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut focus,
                );
                assert_eq!(focus.focused, Some(true), "group={group}, open={open}");

                let enter = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                    modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                    physical_key: iced::keyboard::key::Physical::Code(
                        iced::keyboard::key::Code::Enter,
                    ),
                    location: iced::keyboard::Location::Standard,
                    modifiers: iced::keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                });
                let mut messages = Vec::new();
                element.as_widget_mut().update(
                    &mut tree,
                    &enter,
                    Layout::new(&node),
                    iced::mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                assert_eq!(messages.len(), 1, "one Enter emits one disclosure action");
                match &messages[0] {
                    Message::ChatUi(Action::ToggleTurn(target, id)) if group => {
                        assert_eq!(*target, runtime.target);
                        assert_eq!(id, &key);
                    }
                    Message::ChatUi(Action::Toggle(target, id)) if !group => {
                        assert_eq!(*target, runtime.target);
                        assert_eq!(id, &item.id);
                    }
                    _ => panic!("the focused header must emit its own disclosure action"),
                }
            }
        }
    }
}
