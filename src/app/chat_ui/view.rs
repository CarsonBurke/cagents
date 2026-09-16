use super::*;

fn body<'a>(text: impl Into<Cow<'a, str>> + 'a) -> Element<'a, Message> {
    widget::selectable_text(text)
        .size(design::TEXT_BODY)
        .width(Length::Fill)
        .into()
}
/// The copy control's label for code block `index`.
///
/// The transcript is rebuilt on every frame of a streaming turn, so the labels
/// a message plausibly needs are spelled out instead of formatted per frame.
fn code_label(index: usize) -> Cow<'static, str> {
    const LABELS: [&str; 8] = [
        "Copy code block 1",
        "Copy code block 2",
        "Copy code block 3",
        "Copy code block 4",
        "Copy code block 5",
        "Copy code block 6",
        "Copy code block 7",
        "Copy code block 8",
    ];
    LABELS.get(index).map_or_else(
        || Cow::Owned(format!("Copy code block {}", index + 1)),
        |label| Cow::Borrowed(*label),
    )
}
/// The composer's own line height and inner padding.
///
/// Named because the composer's height is computed from the draft rather than
/// fixed, so the two constants have to agree with what the editor draws.
const COMPOSER_LINE_HEIGHT: f32 = 18.0;
const COMPOSER_PADDING: f32 = design::GRID_UNIT * 2.0;

/// The widest a user's message is drawn.
///
/// Narrower than the measure prose is held to, because a prompt is an aside
/// in the conversation rather than the body of it: keeping it clearly short
/// of the full column is what makes the alternation of asking and answering
/// visible while scrolling.
const PROMPT_MEASURE: f32 = 560.0;

/// The context gauge's track: wide enough to read a proportion off, short
/// enough that it stays a detail in the status line.
const GAUGE_WIDTH: f32 = 72.0;
const GAUGE_HEIGHT: f32 = 6.0;

/// The padding a status-line control is drawn with.
///
/// The default dropdown padding is sized for a settings page; on a status line
/// it made a caption-height row twice the height of its own text.
const CONTROL_PADDING: [f32; 2] = [2.0, 6.0];

/// How tall the completion list is allowed to grow.
///
/// The list sits between the transcript and the composer, so it is bounded:
/// an agent with forty commands must not push the conversation off screen.
const COMPLETION_CEILING: f32 = 180.0;

/// How tall the conversation chooser is allowed to grow.
///
/// The panel sits over the card, so it is bounded and scrolls: an account with
/// twenty open conversations must not cover the whole surface.
const BINDING_CEILING: f32 = 360.0;

/// The composer editor, drawing no surface of its own.
///
/// The composer is one surface that holds the editor and its controls
/// together, the way a reader sees it. Letting the editor paint its own filled
/// bordered box inside that surface is what produced a box inside a box, which
/// is most of what made the old composer look like stacked slabs.
fn composer_editor_style(
    theme: &cosmic::Theme,
    _status: iced::widget::text_editor::Status,
) -> iced::widget::text_editor::Style {
    let cosmic = theme.cosmic();
    iced::widget::text_editor::Style {
        background: iced::Background::Color(iced::Color::TRANSPARENT),
        border: iced::Border::default(),
        placeholder: design::muted_color(theme),
        value: cosmic.palette.neutral_9.into(),
        selection: cosmic.accent.base.into(),
    }
}

/// The icon and tooltip for a submit control.
///
/// `slot` is the control's place among the modes the provider currently
/// allows, and `enter_sends` decides which chord the first of them answers to,
/// so the tooltip names the key the composer is actually bound to rather than
/// a fixed one.
fn submit_control(mode: PromptMode, slot: usize, enter_sends: bool) -> (&'static str, String) {
    let primary = if enter_sends { "Enter" } else { "Ctrl+Enter" };
    let (name, verb, chord) = match (mode, slot) {
        (PromptMode::Send, _) => ("mail-send-symbolic", "Send message", primary),
        (PromptMode::Steer, 0) => ("go-jump-symbolic", "Steer the active turn", primary),
        (PromptMode::Steer, _) => (
            "go-jump-symbolic",
            "Steer the active turn",
            "Ctrl+Shift+Enter",
        ),
        (PromptMode::Queue, 0) => ("list-add-symbolic", "Queue for the next turn", primary),
        (PromptMode::Queue, _) => (
            "list-add-symbolic",
            "Queue for the next turn",
            "Ctrl+Shift+Enter",
        ),
    };
    (name, format!("{verb} — {chord}"))
}
/// The submit mode a composer key press asks for, or `None` when the press is
/// ordinary editing.
///
/// `primary` and `alternate` are the modes the send controls accept right now;
/// the chords must reach exactly those and nothing else. The binding is
/// consulted for every key press in the window, so an unfocused composer must
/// not answer for one.
///
/// `enter_sends` decides which key carries the common action. With it on -
/// the default, and what every chat surface a user arrives from does - bare
/// Enter sends and Shift+Enter breaks the line. With it off, Enter and
/// Shift+Enter both break the line and sending moves to the modified chord,
/// which suits composing a long prompt. Either way the modified chord always
/// submits, so a user who learned Ctrl+Enter never loses it, and the
/// alternate mode is reached by adding Shift to that chord rather than by
/// overloading a bare key with two unrelated meanings.
/// `command` is Ctrl everywhere but macOS, where it is Cmd, so a single arm
/// covers both platforms.
fn submit_chord(
    press: &text_editor::KeyPress,
    primary: Option<PromptMode>,
    alternate: Option<PromptMode>,
    enter_sends: bool,
) -> Option<PromptMode> {
    if !matches!(press.status, text_editor::Status::Focused { .. })
        || !matches!(press.key, keyboard::Key::Named(keyboard::key::Named::Enter))
        || press.modifiers.alt()
    {
        return None;
    }
    if press.modifiers.command() {
        return if press.modifiers.shift() {
            alternate
        } else {
            primary
        };
    }
    // An unmodified Enter only submits when the preference asks it to, and
    // Shift+Enter is always the line break so there is one keystroke that
    // means the same thing under either preference.
    if enter_sends && !press.modifiers.shift() {
        primary
    } else {
        None
    }
}

/// The editing binding for a word-wise delete chord, or `None` for any other
/// press.
///
/// The default bindings map Ctrl+Backspace to deleting one character, so the
/// chord every other text field on the desktop uses to remove a word silently
/// did the same thing as Backspace alone. Selecting the word first and then
/// deleting the selection is how the editor already removes a range, so this
/// needs no new editing primitive. Ctrl+Delete is included because the two
/// directions are one habit.
fn word_delete(press: &text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
    if !matches!(press.status, text_editor::Status::Focused { .. })
        || !press.modifiers.command()
        || press.modifiers.alt()
        || press.modifiers.shift()
    {
        return None;
    }
    let motion = match press.key {
        keyboard::Key::Named(keyboard::key::Named::Backspace) => {
            iced::advanced::text::editor::Motion::WordLeft
        }
        keyboard::Key::Named(keyboard::key::Named::Delete) => {
            iced::advanced::text::editor::Motion::WordRight
        }
        _ => return None,
    };
    Some(text_editor::Binding::Sequence(vec![
        text_editor::Binding::Select(motion),
        text_editor::Binding::Backspace,
    ]))
}

/// The completion query the draft ends on: the text after a `/` that opens the
/// draft's last word, or `None` when the reader is not naming something.
///
/// The trigger follows the end of the draft rather than the start of it,
/// because that is where typing happens, and a reader who wants a command
/// after a sentence of context should still get the list.
fn mention_query(draft: &str) -> Option<&str> {
    draft
        .rsplit(|character: char| character.is_whitespace())
        .next()
        .and_then(|word| word.strip_prefix('/'))
}
/// One row of the composer's completion list.
///
/// A completion row names the thing and says what it does, because a list of
/// bare slugs makes a reader guess.
fn completion_row<'a>(
    label: String,
    description: &'a str,
    message: Option<Message>,
) -> Element<'a, Message> {
    widget::button::custom(
        widget::row::with_children(vec![
            design::body(label).into(),
            design::caption(description)
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        ])
        .spacing(step::GAP_S)
        .align_y(Alignment::Center),
    )
    .class(cosmic::theme::Button::MenuItem)
    .padding(iced::Padding::from([step::GAP_XS, step::GAP_S]))
    .width(Length::Fill)
    .on_press_maybe(message)
    .into()
}
fn command(target: Target, value: Command) -> Message {
    action(Action::Command(target, value))
}

impl Cagents {
    /// Choosing which of a Codex account's open conversations a terminal is
    /// looking at.
    ///
    /// This used to be a bare text field demanding a 36-character UUID copied
    /// out of another program's `/status` output. The account's own server
    /// knows what it is holding open, so the reader picks from that list, and
    /// the common case - one conversation rooted in this terminal's directory
    /// - resolves without the panel appearing at all.
    pub(in crate::app) fn chat_binding_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let binding = self.chat_ui.bindings.get(&card)?;
        // A panel that appears for a moment and then answers its own question
        // is worse than no panel: the reader reads a heading, loses the
        // terminal they were looking at, and gets the conversation they asked
        // for anyway. While the resolution is still unambiguous the card is
        // left exactly as it was.
        if binding.auto {
            return None;
        }
        let valid =
            binding.input.trim().len() == 36 && uuid::Uuid::parse_str(binding.input.trim()).is_ok();
        let here = binding
            .session
            .terminal
            .as_ref()
            .map(|terminal| terminal.launch.working_directory.clone());
        let mut rows: Vec<Element<'_, Message>> =
            vec![design::heading("Which conversation is this terminal viewing?").into()];
        // One sentence about the state of the list, not two: an error and an
        // "it is empty" line together told the reader two different stories
        // about the same failure.
        if binding.scanning {
            rows.push(body("Reading this account's open conversations…"));
        } else if let Some(error) = &binding.error {
            rows.push(body(error.as_str()));
        } else if binding.candidates.is_empty() {
            rows.push(body(
                "This account's shared Codex server is holding no conversation open. Start one in the terminal, or paste its identifier below.",
            ));
        } else {
            rows.push(body(
                "Reading a conversation is read-only; nothing is resumed or sent until you confirm.",
            ));
        }
        // The terminal's own directory comes first, because a conversation
        // rooted anywhere else is almost never the one being looked at.
        let mut listed = binding.candidates.iter().collect::<Vec<_>>();
        listed.sort_by_key(|candidate| here.as_ref() != Some(&candidate.working_directory));
        for candidate in listed {
            let chosen = binding.input.trim() == candidate.session_id;
            let directory = candidate
                .working_directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| candidate.working_directory.display().to_string());
            rows.push(
                widget::button::custom(
                    widget::column::with_children(vec![
                        design::body(candidate.title.as_str())
                            .wrapping(iced::widget::text::Wrapping::None)
                            .into(),
                        design::caption(directory)
                            .class(cosmic::theme::Text::Custom(design::muted_text))
                            .into(),
                    ])
                    .width(Length::Fill),
                )
                .class(if chosen {
                    cosmic::theme::Button::Suggested
                } else {
                    cosmic::theme::Button::MenuItem
                })
                .padding(iced::Padding::from([step::GAP_XS, step::GAP_S]))
                .width(Length::Fill)
                .on_press_maybe((!binding.loading).then_some(action(Action::BindingChoose(
                    card,
                    candidate.session_id.clone(),
                ))))
                .into(),
            );
        }
        if let Some(preview) = &binding.preview {
            rows.push(body(format!(
                "{}\n{}\n{}",
                preview.title,
                preview.session_id,
                preview.working_directory.display()
            )));
            rows.push(body(
                "Confirming attaches to this conversation's existing shared backend with its current permissions. The source terminal stays running; no duplicate agent is launched.",
            ));
        }
        // Manual entry stays as the last resort, for a conversation the server
        // is not holding open and can therefore not list.
        rows.push(
            design::caption("Not listed? Paste its identifier.")
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        );
        rows.push(
            iced::widget::text_input("Conversation identifier", &binding.input)
                .on_input(move |input| action(Action::BindingInput(card, input)))
                .on_submit_maybe(
                    (valid && !binding.loading).then_some(action(Action::BindingPreview(card))),
                )
                .into(),
        );
        rows.push(
            widget::row::with_children(vec![
                widget::button::suggested("Confirm")
                    .on_press_maybe(
                        (binding.preview.is_some() && !binding.loading)
                            .then_some(action(Action::BindingConfirm(card))),
                    )
                    .into(),
                widget::button::text(if binding.loading {
                    "Reading…"
                } else {
                    "Read conversation"
                })
                .on_press_maybe(
                    (valid && !binding.loading).then_some(action(Action::BindingPreview(card))),
                )
                .into(),
                // Attaching needs the account's shared server. Where there is
                // none, this is the only thing that can still be done here,
                // and it was previously left for the reader to work out.
                widget::button::text("New conversation here")
                    .on_press_maybe((!binding.loading).then_some(action(Action::BindingNew(card))))
                    .into(),
                widget::button::text("Cancel")
                    .on_press(action(Action::BindingCancel(card)))
                    .into(),
            ])
            .spacing(step::GAP_S)
            .into(),
        );
        Some(
            widget::container(
                widget::scrollable(
                    widget::column::with_children(rows)
                        .spacing(step::GAP_S)
                        .padding(step::GAP_M),
                )
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .height(Length::Shrink),
            )
            .max_height(BINDING_CEILING)
            .width(Length::Fill)
            .into(),
        )
    }

    /// Where a conversation is working, as the status line reports it.
    ///
    /// A chat pane names no location of its own, so a reader with several
    /// panes open could not tell which worktree an agent was about to change.
    /// The directory's own name is used rather than its path, because the path
    /// is long, repetitive between worktrees, and already in the tab tooltip.
    fn chat_place(&self, directory: &Path) -> Place {
        Place {
            name: directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            branch: self
                .sidebar_model
                .projects
                .iter()
                .flat_map(|project| project.worktrees.iter())
                .find(|worktree| worktree.path == directory)
                .and_then(|worktree| worktree.branch.clone()),
        }
    }

    pub(in crate::app) fn chat_view(&self, card: CardId) -> Element<'_, Message> {
        match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => {
                let place = self.chat_place(&runtime.session.launch.recipe.working_directory);
                runtime.view(
                    self.chat_start_pending(card) || self.chat_ui.closed.contains(&card),
                    self.settings.chat_enter_sends,
                    place,
                )
            }
            _ => widget::text::body("Connecting to native chat…").into(),
        }
    }
}

/// The worktree a conversation runs in.
struct Place {
    name: String,
    branch: Option<String>,
}

impl Runtime {
    fn view(&self, transitioning: bool, enter_sends: bool, place: Place) -> Element<'_, Message> {
        let target = self.target;
        let available =
            !transitioning && !self.disconnected && !self.snapshot.exited && self.pending.is_none();
        let snapshot = &self.snapshot;
        // One theme lookup and one settings value per frame, rather than one
        // per rendered message.
        let markdown_settings = markdown::Settings::with_text_size(
            design::TEXT_BODY,
            markdown::Style::from(if cosmic::theme::is_dark() {
                iced::Theme::Dark
            } else {
                iced::Theme::Light
            }),
        );
        let mut transcript = Vec::new();
        if snapshot.has_earlier {
            transcript.push(
                widget::button::text("Load earlier history")
                    .on_press_maybe(
                        (!transitioning && !self.disconnected && self.pending.is_none())
                            .then_some(command(target, Command::LoadEarlier)),
                    )
                    .into(),
            );
        }
        if self.turns.is_empty() {
            // The empty state says what to do. It used to show whatever the
            // adapter's last status detail was, which meant an empty
            // conversation greeted the reader with a connection state instead
            // of an invitation; the status line is where a state belongs.
            transcript.push(
                widget::container(
                    design::body("Your conversation appears here. Send a message to begin.")
                        .class(cosmic::theme::Text::Custom(design::muted_text)),
                )
                .center_x(Length::Fill)
                .padding(design::GRID_UNIT * 8.0)
                .into(),
            );
        }
        for turn in &self.turns {
            transcript.push(self.turn_view(turn, markdown_settings));
        }
        // A notice belongs to the session, so it is pinned here: inside the
        // transcript it scrolled away from the reader it was addressed to,
        // and a single sentence about the account does not need a glyph and an
        // accent sliver to be read.
        //
        // Only the newest one is pinned. A retrying or hook-heavy provider
        // emits a notice per frame, and every one of them held above a
        // transcript that fills the rest of the pane squeezes the
        // conversation itself out of the card.
        let mut content: Vec<Element<'_, Message>> = snapshot
            .title
            .iter()
            .map(|title| -> Element<'_, Message> { design::heading(title.as_str()).into() })
            .chain(
                snapshot
                    .items
                    .iter()
                    .rev()
                    .filter(|item| item.role == chat::Role::System)
                    .find_map(|item| match &item.content {
                        ItemContent::Text { text, .. } if !text.trim().is_empty() => Some(
                            design::caption(turn::first_line(text.as_str()))
                                .class(cosmic::theme::Text::Custom(design::muted_text))
                                // One row, whatever the provider put in it: a
                                // wrapped paragraph of frame JSON would push
                                // the transcript out of the card.
                                .wrapping(iced::widget::text::Wrapping::None)
                                .width(Length::Fill)
                                .into(),
                        ),
                        _ => None,
                    }),
            )
            .collect();
        content.push(
            widget::scrollable(
                widget::column::with_children(transcript)
                    // Turns are held apart further than anything inside one,
                    // so the whitespace itself says where an exchange ends.
                    // That is the separation a divider between messages was
                    // doing badly.
                    .spacing(step::GAP_L)
                    .width(Length::Fill)
                    // Message rows carry copy and expand controls on their
                    // right edge; the thumb floats over that column.
                    .padding(design::with_vertical_rail(iced::Padding::ZERO)),
            )
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .id(card_control_id(target.card, "chat-transcript"))
            .anchor_bottom()
            .height(Length::Fill)
            .width(Length::Fill)
            .into(),
        );
        if snapshot.has_later {
            content.push(
                widget::button::text("Jump to latest")
                    .on_press_maybe(
                        (!transitioning && !self.disconnected && self.pending.is_none())
                            .then_some(command(target, Command::Refresh)),
                    )
                    .into(),
            );
        }
        if transitioning {
            content.push(body(
                "Switching or closing this session… Editing and session controls are disabled. You can still read and copy the conversation.",
            ));
        }
        if let Some(error) = self.error.as_ref().or(snapshot.error.as_ref()) {
            content.push(body(error.as_str()));
        }
        if self.disconnected || snapshot.exited {
            content.push(
                widget::row::with_children(vec![
                    crate::design::caption(if snapshot.exited {
                        "Agent process exited. The transcript and draft are retained."
                    } else {
                        "Disconnected. The daemon may still be working; your draft is retained."
                    })
                    .width(Length::Fill)
                    .into(),
                    icon(
                        "view-refresh-symbolic",
                        "Reconnect to this session",
                        !transitioning
                            && self.pending.is_none()
                            && !self.attachment_chooser_pending,
                        Action::Reconnect(target),
                    ),
                ])
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        if let Some((url, instructions)) = &snapshot.open_url {
            content.push(
                widget::column::with_children(vec![
                    body(instructions.as_str()),
                    widget::button::text("Open authorization page")
                        .on_press(action(Action::OpenUrl(url.clone())))
                        .into(),
                ])
                .spacing(4)
                .into(),
            );
        }
        // Provider requests stay on the native surface until their explicit resolution.
        // No action is synthesized from a default, a timeout, or closing a pane.
        if !snapshot.requests.is_empty() {
            content.push(
                widget::container(
                    widget::scrollable(
                        widget::column::with_children(
                            snapshot
                                .requests
                                .iter()
                                .map(|request| self.request_view(request, available))
                                .collect::<Vec<_>>(),
                        )
                        .spacing(12)
                        .padding(crate::design::with_vertical_rail(iced::Padding::ZERO)),
                    )
                    .direction(
                        cosmic::iced::widget::scrollable::Direction::Vertical(
                            crate::design::scrollbar(),
                        ),
                    ),
                )
                .max_height(260)
                .into(),
            );
        }
        let draft = self.composer.text();
        if let Some(provided) = &snapshot.composer
            && !provided.trim().is_empty()
            && !draft.contains(provided.as_str())
        {
            content.push(
                widget::button::text("Append provider's suggested draft")
                    .on_press_maybe(
                        (!transitioning).then_some(action(Action::AppendProviderDraft(target))),
                    )
                    .into(),
            );
        }
        if let Some(query) = mention_query(&draft) {
            let matches = |candidate: &str| {
                // Matching case-insensitively, because a reader typing a
                // completion query is not thinking about how the skill's
                // author capitalized it.
                candidate.to_lowercase().starts_with(&query.to_lowercase())
            };
            let mut rows: Vec<Element<'_, Message>> = snapshot
                .commands
                .iter()
                .filter(|command| matches(command.name.trim_start_matches('/')))
                .map(|command| {
                    completion_row(
                        format!("/{}", command.name.trim_start_matches('/')),
                        command.description.as_str(),
                        (!transitioning)
                            .then_some(action(Action::Slash(target, command.name.clone()))),
                    )
                })
                .collect();
            // Skills sit in the same list as the commands, because a reader
            // opening it is asking what this agent can be told to do, not
            // which of those things happens to be a built-in command. The
            // row shows the invocation the provider actually answers to.
            rows.extend(
                snapshot
                    .skills
                    .iter()
                    .filter(|skill| matches(&skill.name))
                    .map(|skill| {
                        completion_row(
                            skill.invocation.clone(),
                            skill.description.as_str(),
                            (!transitioning).then_some(action(Action::Mention(
                                target,
                                skill.invocation.clone(),
                            ))),
                        )
                    }),
            );
            if !rows.is_empty() {
                content.push(
                    widget::container(
                        widget::scrollable(
                            widget::column::with_children(rows)
                                .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                        )
                        .direction(
                            iced::widget::scrollable::Direction::Vertical(design::scrollbar()),
                        ),
                    )
                    .max_height(COMPLETION_CEILING)
                    .class(design::surface(
                        Some(design::raised),
                        [design::RADIUS_M; 4],
                        true,
                    ))
                    .into(),
                );
            }
        }
        for (index, attachment) in self.attachments.iter().enumerate() {
            content.push(
                widget::row::with_children(vec![
                    crate::design::caption(attachment.path.display().to_string())
                        .width(Length::Fill)
                        .into(),
                    icon(
                        "edit-delete-symbolic",
                        "Remove attachment",
                        !transitioning,
                        Action::RemoveAttachment(target, index),
                    ),
                ])
                .align_y(Alignment::Center)
                .spacing(4)
                .into(),
            );
        }
        // Keep the named composer in a fixed sibling slot as provider controls,
        // requests, and attachments arrive. The pinned Iced child reconciler
        // finds named siblings but does not move their trees to new indices.
        let mut content = vec![
            widget::column::with_children(content)
                .spacing(8)
                .height(Length::Fill)
                .into(),
        ];
        let has_draft = !draft.trim().is_empty() || !self.attachments.is_empty();
        // The submit modes the send controls expose, in the order the keyboard
        // reaches them: the first is Ctrl+Enter, the second Ctrl+Shift+Enter.
        let modes: &[PromptMode] = if snapshot.busy {
            match (snapshot.capabilities.steer, snapshot.capabilities.queue) {
                (true, true) => &[PromptMode::Steer, PromptMode::Queue],
                (true, false) => &[PromptMode::Steer],
                (false, true) => &[PromptMode::Queue],
                (false, false) => &[],
            }
        } else {
            &[PromptMode::Send]
        };
        // The placeholder names what the field is for. Spelling out both
        // chords on it meant the one piece of text in an empty composer was
        // keyboard documentation, repeated on every pane, and it went stale
        // the moment the preference changed. The steering and queueing modes
        // still say so, because there the mode is not what a reader expects
        // Enter to do.
        let placeholder = match modes.first() {
            Some(PromptMode::Send) => "Message",
            Some(PromptMode::Steer) => "Steer this turn",
            Some(PromptMode::Queue) => "Queue for the next turn",
            None => "This turn accepts no further input",
        };
        // A chord only submits what a send control would accept right now.
        let submit = if available && has_draft {
            (modes.first().copied(), modes.get(1).copied())
        } else {
            (None, None)
        };
        // The composer grows with the draft rather than holding a fixed pane:
        // a one-line message should not reserve six lines of the transcript,
        // and a long one should not be read three lines at a time through a
        // scrollbar. `Shrink` measures the editor's wrapped extent, so a
        // single long line that wraps counts as the rows it actually draws,
        // which counting newlines could not see. The ceiling keeps a very
        // long paste from squeezing the transcript out, past which the editor
        // scrolls as before.
        let composer_ceiling = 12.0 * COMPOSER_LINE_HEIGHT + 2.0 * COMPOSER_PADDING;
        let editor: Element<'_, Message> = if transitioning {
            // The reservation belongs inside the scroll viewport. An inset on
            // the outer container moves the viewport itself, and the thumb
            // floats at that viewport's right edge either way, so the draft
            // text would still run underneath it.
            widget::container(
                widget::scrollable(widget::container(body(draft)).padding(
                    crate::design::with_vertical_rail(iced::Padding::from(COMPOSER_PADDING)),
                ))
                .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
                    crate::design::scrollbar(),
                )),
            )
            .max_height(composer_ceiling)
            .width(Length::Fill)
            .into()
        } else {
            iced::widget::text_editor::TextEditor::new(&self.composer)
                .id(card_control_id(target.card, "chat-composer"))
                .placeholder(placeholder)
                .size(design::TEXT_BODY)
                .height(Length::Shrink)
                .max_height(composer_ceiling)
                // The editor scrolls once the draft passes the ceiling, and
                // its thumb floats over the text's right edge, so the gutter
                // is reserved here as well as in the read-only branch.
                .padding(design::with_vertical_rail(iced::Padding::from(
                    COMPOSER_PADDING,
                )))
                .class(cosmic::theme::iced::TextEditor::Custom(Box::new(
                    composer_editor_style,
                )))
                .on_action(move |event| action(Action::Composer(target, event)))
                .key_binding(move |press| {
                    if let Some(mode) = submit_chord(&press, submit.0, submit.1, enter_sends) {
                        return Some(text_editor::Binding::Custom(action(Action::Submit(
                            target, mode,
                        ))));
                    }
                    word_delete(&press).or_else(|| text_editor::Binding::from_key_press(press))
                })
                .into()
        };
        let mut actions = Vec::new();
        if snapshot.capabilities.attachments {
            actions.push(icon(
                "mail-attachment-symbolic",
                "Attach files",
                available && !self.attachment_chooser_pending,
                Action::Attach(target),
            ));
        }
        if snapshot.capabilities.compact {
            actions.push(icon(
                "view-restore-symbolic",
                "Compact conversation context",
                available && !snapshot.busy,
                Action::Command(target, Command::Compact),
            ));
        }
        // Commands are reached by typing `/`, which is how every agent surface
        // a user arrives from works and what the composer already answers to.
        // A dropdown listing them duplicated that path in a control that was
        // permanently present for something used occasionally.
        actions.push(iced::widget::Space::new().width(Length::Fill).into());
        for (slot, mode) in modes.iter().enumerate() {
            let (name, label) = submit_control(*mode, slot, enter_sends);
            actions.push(icon(
                name,
                label,
                available && has_draft,
                Action::Submit(target, *mode),
            ));
        }
        if snapshot.busy {
            actions.push(icon(
                "media-playback-stop-symbolic",
                "Cancel active turn",
                available,
                Action::Command(target, Command::Interrupt),
            ));
        }
        // The editor and its controls are one object: a single raised surface
        // with the controls tucked inside its lower edge, so the composer
        // reads as one place to type rather than a field with a toolbar
        // bolted under it.
        content.push(
            widget::container(
                widget::column::with_children(vec![
                    editor,
                    widget::container(
                        widget::row::with_children(actions)
                            .spacing(step::GAP_XS)
                            .align_y(Alignment::Center),
                    )
                    .padding(iced::Padding::from([0.0, step::GAP_XS]).bottom(step::GAP_XS))
                    .into(),
                ])
                .width(Length::Fill),
            )
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                true,
            ))
            .width(Length::Fill)
            .into(),
        );
        let usage = &snapshot.usage;
        // The status line is one caption-height row: what the agent is doing,
        // where it is doing it, the settings a reader changes mid-conversation,
        // and what the conversation has cost. It scrolls sideways rather than
        // wrapping, because a chat pane is often narrow and a status line that
        // grows to three rows takes the space from the transcript.
        let mut status: Vec<Element<'_, Message>> = Vec::new();
        let activity = if transitioning {
            Some(("agent-working-symbolic", "Switching session"))
        } else if self.attachment_chooser_pending {
            Some(("agent-working-symbolic", "Choosing attachments"))
        } else if self.pending.is_some() {
            Some(("agent-working-symbolic", "Awaiting acknowledgement"))
        } else if snapshot.waiting || !snapshot.requests.is_empty() {
            Some(("agent-waiting-symbolic", "Waiting for you"))
        } else if snapshot.busy {
            Some(("agent-working-symbolic", "Replying"))
        } else {
            // An idle agent needs no announcement. "Ready" was on screen for
            // the whole time nothing was happening, which is most of the time,
            // and said nothing the empty composer did not already say.
            None
        };
        if let Some((glyph, state)) = activity {
            status.push(
                widget::icon(crate::ui_controls::symbolic_icon(glyph))
                    .size(12)
                    .into(),
            );
            status.push(
                design::caption(if snapshot.busy && !snapshot.detail.is_empty() {
                    snapshot.detail.as_str()
                } else {
                    state
                })
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            );
        }
        if !place.name.is_empty() {
            status.push(
                design::caption(place.name.clone())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            );
        }
        if let Some(branch) = &place.branch {
            status.push(
                widget::row::with_children(vec![
                    widget::icon(crate::sidebar::worktree_icon())
                        .size(12)
                        .into(),
                    design::caption(branch.clone())
                        .class(cosmic::theme::Text::Custom(design::muted_text))
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        status.extend(self.session_controls(transitioning));
        if usage.context_limit > 0 {
            // A context budget is a quantity a reader acts on - compact now,
            // or lose the thread - so it is drawn as a gauge rather than
            // buried in a sentence of digits. A capsule, because a squared
            // bar four pixels tall reads as a crack in the layout.
            let used = (usage.context_tokens as f32 / usage.context_limit as f32).clamp(0.0, 1.0);
            let radius = GAUGE_HEIGHT / 2.0;
            status.push(
                widget::container(
                    // The fill and the remainder both carry a portion, because
                    // a single proportional child of a container is resolved
                    // against the whole width and would draw a full gauge at
                    // any occupancy.
                    widget::row::with_children(vec![
                        widget::container(iced::widget::Space::new().height(Length::Fill))
                            .width(Length::FillPortion((used * 1000.0).round() as u16))
                            .height(Length::Fill)
                            .class(cosmic::theme::Container::custom(move |theme| {
                                cosmic::widget::container::Style {
                                    background: Some(iced::Background::Color(if used > 0.9 {
                                        design::destructive_color(theme)
                                    } else if used > 0.75 {
                                        design::warning_color(theme)
                                    } else {
                                        design::accent_color(theme)
                                    })),
                                    border: iced::Border {
                                        radius: radius.into(),
                                        ..Default::default()
                                    },
                                    ..Default::default()
                                }
                            }))
                            .into(),
                        iced::widget::Space::new()
                            .width(Length::FillPortion(((1.0 - used) * 1000.0).round() as u16))
                            .into(),
                    ])
                    .height(Length::Fill),
                )
                .width(GAUGE_WIDTH)
                .height(GAUGE_HEIGHT)
                .class(design::surface(Some(design::track), [radius; 4], false))
                .into(),
            );
            status.push(design::caption(format!("{:.0}%", used * 100.0)).into());
        }
        status.push(
            design::caption(format!(
                "{} in · {} out",
                usage.input_tokens, usage.output_tokens
            ))
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
        );
        if let Some(cost) = usage.cost_microusd {
            // Two decimals, because the figure is money a reader is spending
            // and four decimals of a dollar is a number nobody reads.
            status.push(design::caption(format!("${:.2}", cost as f64 / 1_000_000.0)).into());
        }
        content.push(
            widget::scrollable(
                widget::row::with_children(status)
                    .spacing(step::GAP_S)
                    .align_y(Alignment::Center)
                    // The thumb floats over the row it belongs to, so its rail
                    // is reserved below the content the way every other
                    // scrollable in the app reserves one.
                    .padding(design::with_horizontal_rail(iced::Padding::ZERO)),
            )
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            ))
            .width(Length::Fill)
            .into(),
        );
        widget::container(
            widget::column::with_children(content)
                .spacing(step::GAP_S)
                .height(Length::Fill)
                .width(Length::Fill),
        )
        .padding(step::GAP_M)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// The model, reasoning effort, and permission mode, at the status line's
    /// own scale.
    ///
    /// These were three full-size dropdowns stacked above the composer, which
    /// gave settings a reader changes occasionally more room than the message
    /// they were about to send. While a session is switching they are drawn as
    /// plain text: the values still matter, and a row of disabled controls
    /// says nothing a reader can act on.
    fn session_controls(&self, transitioning: bool) -> Vec<Element<'_, Message>> {
        let target = self.target;
        let snapshot = &self.snapshot;
        let mut controls: Vec<Element<'_, Message>> = Vec::new();
        if snapshot.capabilities.model_selection && !snapshot.models.is_empty() {
            let selected = snapshot
                .models
                .iter()
                .position(|model| Some(&model.id) == snapshot.selected_model.as_ref());
            if transitioning {
                controls.push(
                    design::caption(
                        selected.map_or("Model", |index| snapshot.models[index].label.as_str()),
                    )
                    .into(),
                );
            } else {
                let labels = snapshot
                    .models
                    .iter()
                    .map(|model| model.label.clone())
                    .collect::<Vec<_>>();
                let ids = snapshot
                    .models
                    .iter()
                    .map(|model| model.id.clone())
                    .collect::<Vec<_>>();
                controls.push(
                    widget::dropdown(labels, selected, move |index| {
                        action(Action::Model(target, ids[index].clone()))
                    })
                    .text_size(design::TEXT_CAPTION)
                    .padding(CONTROL_PADDING)
                    .into(),
                );
            }
        }
        if snapshot.capabilities.reasoning_selection
            && let Some(model) = snapshot
                .models
                .iter()
                .find(|model| Some(&model.id) == snapshot.selected_model.as_ref())
            && !model.reasoning.is_empty()
        {
            let selected = model
                .reasoning
                .iter()
                .position(|reasoning| Some(reasoning) == snapshot.reasoning.as_ref());
            if transitioning {
                controls.push(
                    design::caption(
                        selected.map_or("Reasoning", |index| model.reasoning[index].as_str()),
                    )
                    .into(),
                );
            } else {
                let reasoning = model.reasoning.clone();
                let model_id = model.id.clone();
                controls.push(
                    widget::dropdown(model.reasoning.clone(), selected, move |index| {
                        action(Action::Reasoning(
                            target,
                            model_id.clone(),
                            reasoning[index].clone(),
                        ))
                    })
                    .text_size(design::TEXT_CAPTION)
                    .padding(CONTROL_PADDING)
                    .into(),
                );
            }
        }
        if !snapshot.capabilities.permission_modes.is_empty() {
            let modes = &snapshot.capabilities.permission_modes;
            let selected = modes
                .iter()
                .position(|mode| Some(mode) == self.permission_mode.as_ref());
            if transitioning {
                controls.push(
                    design::caption(selected.map_or("Permissions", |index| modes[index].as_str()))
                        .into(),
                );
            } else {
                let chosen = modes.clone();
                controls.push(
                    widget::dropdown(modes.clone(), selected, move |index| {
                        action(Action::PermissionMode(target, chosen[index].clone()))
                    })
                    .text_size(design::TEXT_CAPTION)
                    .padding(CONTROL_PADDING)
                    .into(),
                );
            }
        }
        controls
    }

    /// One exchange: what was asked, what was done, and what came back.
    fn turn_view<'a>(
        &'a self,
        turn: &'a turn::Turn,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        for index in &turn.prompt {
            if let Some(item) = self.snapshot.items.get(*index) {
                rows.push(self.prompt_view(item));
            }
        }
        if !turn.steps.is_empty() {
            rows.push(self.step_strip(turn.key.as_str(), turn, settings));
        }
        for placed in &turn.prose {
            let Some(item) = self.snapshot.items.get(placed.index) else {
                continue;
            };
            let Some(cached) = self.cache.get(&item.id) else {
                continue;
            };
            rows.push(self.prose_view(item, cached, placed.slot, settings));
        }
        widget::column::with_children(rows)
            .spacing(step::GAP_M)
            .width(Length::Fill)
            .into()
    }

    /// What to call an attachment the reader just sent.
    ///
    /// A file has a name, and the name is all the reader needs: the directories
    /// above it are the ones they were just browsing. An inline attachment has
    /// no name at all, because its "path" is the payload itself, so the tail of
    /// it is base64 and the provider's description is the only thing that reads
    /// as a label.
    fn attachment_label<'a>(path: &'a str, description: &'a str) -> &'a str {
        if !path.starts_with("data:") {
            if let Some(name) = path.rsplit('/').next().filter(|name| !name.is_empty()) {
                return name;
            }
        }
        if description.is_empty() {
            "Attachment"
        } else {
            description
        }
    }

    /// The user's own message.
    ///
    /// Set on the accent wash and tucked to the right, with the corner nearest
    /// its author squared off. That asymmetry is what identifies the author
    /// without printing "You" above every message, and the narrower measure
    /// keeps a turn's opening findable when scrolling back.
    fn prompt_view<'a>(&'a self, item: &'a chat::Item) -> Element<'a, Message> {
        let attachment = matches!(item.content, ItemContent::Image { .. });
        let text = match &item.content {
            ItemContent::Text { text, .. } => text.as_str(),
            ItemContent::Image { path, description } => Self::attachment_label(path, description),
            content => turn::step_label(content),
        };
        // An attachment sits under its prompt as an outlined chip rather than
        // another filled bubble, so a message with two images does not read as
        // three separate things the reader said.
        let body: Element<'a, Message> = if attachment {
            widget::row::with_children(vec![
                widget::icon(crate::ui_controls::symbolic_icon(
                    "mail-attachment-symbolic",
                ))
                .size(12)
                .into(),
                design::caption(text).into(),
            ])
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center)
            .into()
        } else {
            widget::selectable_text(text)
                .size(design::TEXT_BODY)
                .width(Length::Shrink)
                .into()
        };
        widget::container(
            widget::container(body)
                .class(design::surface(
                    (!attachment).then_some(design::accent_wash),
                    [
                        design::RADIUS_L,
                        design::RADIUS_L,
                        design::RADIUS_XS,
                        design::RADIUS_L,
                    ],
                    attachment,
                ))
                .padding([step::GAP_S as u16, step::GAP_M as u16])
                .max_width(PROMPT_MEASURE),
        )
        .align_x(Alignment::End)
        .width(Length::Fill)
        .into()
    }

    /// An assistant answer, its narration, or a system notice.
    ///
    /// Commentary recedes: it outnumbers answers several times over, and
    /// giving both the same weight is why a reader could not find the answer
    /// in the surface this replaces.
    fn prose_view<'a>(
        &'a self,
        item: &'a chat::Item,
        cached: &'a CachedItem,
        slot: turn::Slot,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let target = self.target;
        if slot == turn::Slot::Commentary {
            // Narration is a passing remark, so it gets one line at caption
            // weight, indented to sit under the activity it describes. Given
            // a paragraph of its own it drowned out the answer.
            return widget::container(
                design::caption(turn::first_line(match &item.content {
                    ItemContent::Text { text, .. } => text.as_str(),
                    content => turn::step_label(content),
                }))
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::None),
            )
            .padding(iced::Padding::ZERO.left(step::GAP_M))
            .width(Length::Fill)
            .into();
        }
        let ItemContent::Text { .. } = &item.content else {
            // Anything else in the answer slot is a step the grouping did not
            // recognize; render it the way a step renders rather than
            // inventing a second treatment for it.
            return self.step_view(item, cached, settings);
        };
        let mut rows: Vec<Element<'a, Message>> = vec![
            markdown::view(cached.markdown.items(), settings)
                .map(|uri| action(Action::OpenUrl(uri.to_string())))
                .into(),
        ];
        // An answer's footer says when it finished and offers to copy it, and
        // nothing else: a reader of an answer reaches for those two, and the
        // Markdown source of a rendered answer was a developer's control on a
        // reader's surface.
        let mut controls: Vec<Element<'a, Message>> = Vec::new();
        if let Some(stamp) = item.at.map(clock_label).filter(|at| !at.is_empty()) {
            controls.push(
                design::caption(stamp)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
        }
        controls.push(small_icon(
            "edit-copy-symbolic",
            "Copy this message",
            Action::Copy(target, item.id.clone(), CopyPart::Text),
        ));
        for index in 0..cached.code.codes.len() {
            controls.push(small_icon(
                "edit-copy-symbolic",
                code_label(index),
                Action::Copy(target, item.id.clone(), CopyPart::Code(index)),
            ));
        }
        rows.push(
            widget::row::with_children(controls)
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
        );
        // The answer is bare prose on the pane's own surface. Wrapping it in
        // a panel would put a frame around the one thing the reader came for,
        // and would make it look like just another block.
        widget::container(
            widget::column::with_children(rows)
                .spacing(step::GAP_S)
                .max_width(design::READABLE_MEASURE),
        )
        .width(Length::Fill)
        .into()
    }

    fn request_view<'a>(
        &'a self,
        request: &'a chat::Request,
        available: bool,
    ) -> Element<'a, Message> {
        let target = self.target;
        let mut rows = vec![
            design::heading(request.title.as_str()).into(),
            body(request.description.as_str()),
        ];
        for field in &request.fields {
            rows.push(crate::design::caption(field.label.as_str()).into());
            if !field.description.is_empty() {
                rows.push(body(field.description.as_str()));
            }
            let values = self.answers.get(&(request.id.clone(), field.id.clone()));
            {
                let value = values
                    .and_then(|values| {
                        values
                            .iter()
                            .find(|value| !field.choices.iter().any(|choice| &choice.id == *value))
                    })
                    .map(String::as_str)
                    .unwrap_or("");
                let request_id = request.id.clone();
                let field_id = field.id.clone();
                rows.push(
                    iced::widget::text_input(
                        if field.choices.is_empty() {
                            field.label.as_str()
                        } else {
                            "Other answer (if supported)"
                        },
                        value,
                    )
                    .id(widget::Id::new(format!(
                        "chat.{}.{}.{}",
                        target.card, request.id, field.id
                    )))
                    .secure(field.secret)
                    .on_input_maybe(available.then_some(move |value| {
                        action(Action::Field(
                            target,
                            request_id.clone(),
                            field_id.clone(),
                            value,
                        ))
                    }))
                    .into(),
                );
            }
            if !field.choices.is_empty() {
                for choice in &field.choices {
                    let selected = values.is_some_and(|values| values.contains(&choice.id));
                    let message = action(Action::Choice(
                        target,
                        request.id.clone(),
                        field.id.clone(),
                        choice.id.clone(),
                    ));
                    rows.push(
                        widget::button::custom(widget::text(choice.label.as_str()))
                            .name(choice.label.as_str())
                            .class(cosmic::theme::Button::MenuItem)
                            .selected(selected)
                            .on_press_maybe(available.then_some(message))
                            .into(),
                    );
                    if !choice.description.is_empty() {
                        rows.push(crate::design::caption(choice.description.as_str()).into());
                    }
                }
            }
        }
        for choice in &request.actions {
            rows.push(
                widget::button::standard(choice.label.as_str())
                    .on_press_maybe(available.then_some(action(Action::Answer(
                        target,
                        request.id.clone(),
                        choice.id.clone(),
                    ))))
                    .into(),
            );
            if !choice.description.is_empty() {
                rows.push(crate::design::caption(choice.description.as_str()).into());
            }
        }
        widget::container(
            widget::column::with_children(rows)
                .spacing(6)
                .width(Length::Fill),
        )
        .class(cosmic::theme::Container::Secondary)
        .padding(10)
        .width(Length::Fill)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(modifiers: keyboard::Modifiers, focused: bool) -> text_editor::KeyPress {
        let key = keyboard::Key::Named(keyboard::key::Named::Enter);
        text_editor::KeyPress {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            modifiers,
            text: None,
            status: if focused {
                text_editor::Status::Focused { is_hovered: false }
            } else {
                text_editor::Status::Active
            },
        }
    }

    /// The composer is the product's primary input: the chords must reach the
    /// send controls, and nothing else may submit a half-written message.
    #[test]
    fn the_modified_chord_submits_under_either_preference() {
        let send = Some(PromptMode::Send);
        let queue = Some(PromptMode::Queue);

        for enter_sends in [true, false] {
            assert_eq!(
                submit_chord(
                    &press(keyboard::Modifiers::COMMAND, true),
                    send,
                    None,
                    enter_sends
                ),
                send,
                "Ctrl+Enter must keep sending whichever key the preference favours"
            );
            assert_eq!(
                submit_chord(
                    &press(
                        keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
                        true
                    ),
                    send,
                    queue,
                    enter_sends
                ),
                queue
            );
            // No alternate mode on offer: the chord falls back to editing.
            assert_eq!(
                submit_chord(
                    &press(
                        keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
                        true
                    ),
                    send,
                    None,
                    enter_sends
                ),
                None
            );
            for modifiers in [
                // Shift+Enter is the line break under either preference, so
                // there is one keystroke that always means the same thing.
                keyboard::Modifiers::SHIFT,
                keyboard::Modifiers::ALT,
                keyboard::Modifiers::COMMAND | keyboard::Modifiers::ALT,
            ] {
                assert_eq!(
                    submit_chord(&press(modifiers, true), send, queue, enter_sends),
                    None,
                    "{modifiers:?} must edit the draft rather than submit it"
                );
            }
            // The binding is offered every key press in the window.
            assert_eq!(
                submit_chord(
                    &press(keyboard::Modifiers::COMMAND, false),
                    send,
                    queue,
                    enter_sends
                ),
                None
            );
            // Nothing to submit: the chord falls back to inserting a line.
            assert_eq!(
                submit_chord(
                    &press(keyboard::Modifiers::COMMAND, true),
                    None,
                    None,
                    enter_sends
                ),
                None
            );
        }
    }

    /// The preference is the whole point of the setting: it has to change what
    /// a bare Enter does and nothing else.
    #[test]
    fn a_bare_enter_submits_only_when_the_preference_asks() {
        let send = Some(PromptMode::Send);
        assert_eq!(
            submit_chord(
                &press(keyboard::Modifiers::default(), true),
                send,
                None,
                true
            ),
            send
        );
        assert_eq!(
            submit_chord(
                &press(keyboard::Modifiers::default(), true),
                send,
                None,
                false
            ),
            None
        );
        // Even when Enter sends, an unfocused composer must not answer for a
        // key press meant for another widget.
        assert_eq!(
            submit_chord(
                &press(keyboard::Modifiers::default(), false),
                send,
                None,
                true
            ),
            None
        );
    }

    /// Every submit control's tooltip has to name the chord bound to it, or the
    /// keyboard path stays invisible.
    #[test]
    fn submit_tooltips_name_the_chord_the_preference_bound() {
        for (mode, slot, enter_sends, chord) in [
            (PromptMode::Send, 0, true, "Enter"),
            (PromptMode::Send, 0, false, "Ctrl+Enter"),
            (PromptMode::Steer, 0, true, "Enter"),
            (PromptMode::Queue, 0, false, "Ctrl+Enter"),
            (PromptMode::Steer, 1, true, "Ctrl+Shift+Enter"),
            (PromptMode::Queue, 1, false, "Ctrl+Shift+Enter"),
        ] {
            let (_, label) = submit_control(mode, slot, enter_sends);
            assert!(
                label.ends_with(chord),
                "{mode:?} in slot {slot} is bound to {chord} but reads {label:?}"
            );
        }
    }

    /// A chip the reader cannot read is worse than no chip: the point of it is
    /// to confirm which file they just sent.
    #[test]
    fn an_attachment_is_named_by_something_a_reader_recognises() {
        assert_eq!(
            Runtime::attachment_label("/home/reader/holiday/beach.png", "Image attachment"),
            "beach.png",
            "a file on disk is identified by its name, not the browsing above it"
        );
        assert_eq!(
            Runtime::attachment_label("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg", "Diagram"),
            "Diagram",
            "an inline payload has no name, so the description has to carry it"
        );
        assert_eq!(
            Runtime::attachment_label("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg", ""),
            "Attachment",
            "a payload no provider described still needs a readable label"
        );
    }
}
