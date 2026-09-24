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

/// How tall the pinned plan is allowed to grow.
///
/// The plan sits above the transcript, so it stays a reference a reader
/// glances at rather than becoming the pane: past this height it scrolls, and
/// a twenty-step plan therefore costs the conversation no more room than a
/// three-step one.
const PLAN_CEILING: f32 = 120.0;

/// The padding a status-line control is drawn with.
///
/// The default dropdown padding is sized for a settings page; on a status line
/// it made a caption-height row twice the height of its own text.
const CONTROL_PADDING: [f32; 2] = [2.0, 6.0];

/// The height of one completion row.
const COMPLETION_ROW: f32 = design::DENSE_ROW_HEIGHT;

/// How tall the completion list is allowed to grow.
///
/// The list sits between the transcript and the composer, so it is bounded:
/// an agent with forty skills must not push the conversation off screen. A
/// whole number of rows, so the last visible one is never cut in half.
const COMPLETION_CEILING: f32 = 7.0 * COMPLETION_ROW;

/// The completion list's inset from its own edge.
const COMPLETION_INSET: f32 = design::GRID_UNIT;

/// The completion list's scrollable, named so the keyboard can scroll it.
const COMPLETION_LIST: &str = "chat-completion";

/// How tall the conversation chooser is allowed to grow.
///
/// The panel sits over the card, so it is bounded and scrolls: an account with
/// twenty open conversations must not cover the whole surface.

/// How tall the pending questions are allowed to grow.
///
/// A question blocks the agent, so it is given room to be read and answered
/// rather than the caption-sized slot it used to share with the composer:
/// past this it scrolls, so three questions at once still leave the
/// transcript and the composer on screen.
const REQUEST_CEILING: f32 = 420.0;

/// How tall the material a question refers to is allowed to grow.
///
/// A provider hands over the whole payload it is asking about - a command, a
/// diff, a permission set as JSON - and a screenful of that would bury the
/// answers under it.
const REQUEST_CONTEXT_CEILING: f32 = 160.0;

/// How close to an end of the transcript counts as having reached it.
///
/// History is asked for by the gesture rather than by a control, so the
/// boundary has to be reachable by a gesture: a wheel notch or a flung scroll
/// settles a few points short of an exact end, and a reader who has plainly
/// arrived at the oldest message they have should not have to nudge the wheel
/// again to be believed.
const PAGE_REACH: f32 = 24.0;

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
/// Whether a key press asks to recall a sent message: `Some(true)` for the
/// older one, `Some(false)` for the newer. Only the bare arrow keys, and only
/// where the caret's line makes the arrow a walk through history rather than
/// through the draft.
fn history_key(
    press: &text_editor::KeyPress,
    recall_older: bool,
    recall_newer: bool,
) -> Option<bool> {
    if !matches!(press.status, text_editor::Status::Focused { .. })
        || press.modifiers.shift()
        || press.modifiers.control()
        || press.modifiers.alt()
        || press.modifiers.command()
    {
        return None;
    }
    match press.key {
        keyboard::Key::Named(keyboard::key::Named::ArrowUp) if recall_older => Some(true),
        keyboard::Key::Named(keyboard::key::Named::ArrowDown) if recall_newer => Some(false),
        _ => None,
    }
}

/// A failure, as an entry of the transcript: what went wrong, set apart the
/// way a system notice is and read in sequence with what it interrupted.
fn notice_entry<'a>(message: &'a str) -> Element<'a, Message> {
    widget::container(
        widget::row::with_children(vec![
            widget::icon(crate::ui_controls::symbolic_icon("dialog-warning-symbolic"))
                .size(design::COMPACT_ICON_SIZE)
                .into(),
            design::caption(message)
                .class(cosmic::theme::Text::Custom(design::warning_text))
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        ])
        .spacing(step::GAP_S)
        .align_y(Alignment::Start),
    )
    .class(design::surface(
        Some(design::sunken),
        [design::RADIUS_S; 4],
        false,
    ))
    .padding([step::GAP_S as u16, step::GAP_M as u16])
    .max_width(design::READABLE_MEASURE)
    .into()
}

/// What a key press does to an open completion list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionKey {
    /// Moves the highlight down, or up when `false`.
    Move(bool),
    /// Chooses the highlighted row, as Tab always does.
    Choose,
    /// Chooses the highlighted row unless the word is already whole, as
    /// Enter does.
    Confirm,
    Dismiss,
}

/// What the composer says of the skills a draft names that `reach` leaves
/// unloaded, or `None` when it loads them all.
fn unloaded_hint(unloaded: &[&str], reach: chat::SkillReach) -> Option<String> {
    let rule = match reach {
        _ if unloaded.is_empty() => return None,
        chat::SkillReach::Leading => "only a skill that starts the message is loaded with it",
        chat::SkillReach::First => "only the first skill a message names is loaded with it",
        chat::SkillReach::Anywhere => return None,
    };
    let verb = if unloaded.len() == 1 { "goes" } else { "go" };
    Some(format!(
        "{} {verb} as plain text: {rule}",
        unloaded.join(", ")
    ))
}

/// The completion list as the keyboard sees it: the highlighted row's text,
/// and whether Enter chooses it.
#[derive(Debug, Clone)]
struct OpenList {
    insertion: String,
    enter_chooses: bool,
}

/// What `press` does while the completion list is open, or `None` when it
/// goes on to sending, history and the editor. `send` is what the same press
/// would send the message with; Enter does that, or breaks the line, if the
/// row it chose is gone by the time it lands.
fn completion_action(
    target: Target,
    press: &text_editor::KeyPress,
    open: &OpenList,
    send: Option<PromptMode>,
) -> Option<Action> {
    Some(match completion_key(press)? {
        CompletionKey::Move(down) => Action::CompletionMove(target, down),
        CompletionKey::Choose => Action::Complete(target, open.insertion.clone(), None),
        CompletionKey::Confirm if open.enter_chooses => {
            let otherwise = send.map_or(
                Action::Composer(target, text_editor::Action::Edit(text_editor::Edit::Enter)),
                |mode| Action::Submit(target, mode),
            );
            Action::Complete(target, open.insertion.clone(), Some(Box::new(otherwise)))
        }
        CompletionKey::Confirm => return None,
        CompletionKey::Dismiss => Action::CompletionDismiss(target),
    })
}

/// The completion key a press is, or `None` for one the editor keeps.
///
/// Only the bare keys: a chord is the reader reaching past the list, and
/// Ctrl+Enter in particular still sends the message.
fn completion_key(press: &text_editor::KeyPress) -> Option<CompletionKey> {
    if !matches!(press.status, text_editor::Status::Focused { .. })
        || press.modifiers.shift()
        || press.modifiers.control()
        || press.modifiers.alt()
        || press.modifiers.logo()
    {
        return None;
    }
    let keyboard::Key::Named(key) = press.key else {
        return None;
    };
    match key {
        keyboard::key::Named::ArrowDown => Some(CompletionKey::Move(true)),
        keyboard::key::Named::ArrowUp => Some(CompletionKey::Move(false)),
        keyboard::key::Named::Tab => Some(CompletionKey::Choose),
        keyboard::key::Named::Enter => Some(CompletionKey::Confirm),
        keyboard::key::Named::Escape => Some(CompletionKey::Dismiss),
        _ => None,
    }
}

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

/// One row of the composer's completion list.
///
/// A row shows exactly what choosing it puts in the draft, says what that
/// does, and says whether it is a command or a skill: a command is the whole
/// message and a skill is part of one, and a reader deciding where to type
/// next needs to know which they just added.
fn completion_row<'a>(
    row: completion::Candidate<'a>,
    highlighted: bool,
    message: Option<Message>,
) -> Element<'a, Message> {
    widget::button::custom(
        widget::row::with_children(vec![
            design::body(row.insertion)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            design::caption(row.description)
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .width(Length::Fill)
                .wrapping(iced::widget::text::Wrapping::None)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                ))
                .into(),
            design::caption(match row.kind {
                completion::Kind::Command => "Command",
                completion::Kind::Skill => "Skill",
            })
            .class(cosmic::theme::Text::Custom(design::muted_text))
            .into(),
        ])
        .spacing(step::GAP_S)
        .align_y(Alignment::Center),
    )
    .class(crate::ui_controls::control_style(highlighted, None))
    .padding(iced::Padding::from([0.0, step::GAP_S]))
    .height(Length::Fixed(COMPLETION_ROW))
    .width(Length::Fill)
    .on_press_maybe(message)
    .into()
}

/// The height of one completion row and of the list showing `count` of them.
///
/// Rows are a fixed height so the keyboard highlight can be scrolled into view
/// by arithmetic rather than by measuring the laid-out list.
pub(super) fn completion_extent(count: usize) -> (f32, f32) {
    (
        COMPLETION_ROW,
        (count as f32 * COMPLETION_ROW).min(COMPLETION_CEILING),
    )
}

/// Scrolls the completion list to `offset`.
pub(super) fn completion_scroll(target: Target, offset: f32) -> Task<Message> {
    iced::widget::scrollable::scroll_to(
        card_control_id(target.card, COMPLETION_LIST),
        iced::widget::scrollable::AbsoluteOffset {
            x: None,
            y: Some(offset),
        },
    )
}
/// Which end of the loaded window a history request is extending.
///
/// A conversation is paged in both directions - earlier, and back to the
/// newest after the reader has gone back - and the surface says which one it
/// is waiting on at the edge the rows will appear at, so the direction has to
/// travel with the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Page {
    Earlier,
    Newer,
}

/// Where the transcript's viewport sits in the conversation it is scrolling.
///
/// The transcript is anchored to its bottom, because a conversation grows at
/// the newest end, which means the offset the scrollable reports is measured
/// from the newest message. Everything asked of it here - have I reached the
/// top, how far down am I, how much is on screen - is asked from the top, so
/// that conversion happens once, here, rather than at each caller.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scroll {
    from_top: f32,
    content: f32,
    window: f32,
}

impl Scroll {
    fn of(viewport: &iced::widget::scrollable::Viewport) -> Self {
        Self {
            from_top: viewport.absolute_offset_reversed().y,
            content: viewport.content_bounds().height,
            window: viewport.bounds().height,
        }
    }

    /// How far the transcript can be scrolled.
    fn travel(self) -> f32 {
        (self.content - self.window).max(0.0)
    }

    /// Whether the reader has reached the oldest message that is loaded.
    ///
    /// A transcript shorter than the pane it is drawn in counts as reached:
    /// no gesture can take the reader any further up it, so earlier history
    /// has to arrive without one or it can never arrive at all.
    pub(super) fn at_top(self) -> bool {
        self.travel() <= 0.0 || self.from_top <= PAGE_REACH
    }

    /// Whether the reader has reached the newest message that is loaded.
    ///
    /// The newest message a conversation has is not always the newest one
    /// loaded: paging back through history pins the window to an older page,
    /// and reaching the bottom of that page is how a reader asks to be
    /// returned to the live end of the conversation.
    pub(super) fn at_bottom(self) -> bool {
        self.travel() <= 0.0 || self.from_top >= self.travel() - PAGE_REACH
    }

    /// The height of the loaded transcript, which is what tells the page the
    /// reader has already asked for from the one they are asking for now.
    pub(super) fn content(self) -> f32 {
        self.content
    }

    pub(super) fn page_height(self) -> f32 {
        (self.window * 0.9).max(1.0)
    }
}

/// A question, and the material it is about.
///
/// Every provider hands the surface one description string with both in it:
/// the reason it is asking, and whatever the question refers to - a command,
/// a path, a requested permission set, a JSON payload. Drawn as one block
/// that put a shell command in the middle of a sentence. Split, the question
/// can be set as prose and the material as the literal text it is.
///
/// The split is only ever made where the provider put a boundary: a fenced
/// block, or a blank line between paragraphs. A single line break is not a
/// boundary - it is how a sentence wraps - and splitting on one set the
/// second half of a sentence in a monospaced block as though it were an
/// argv. Where the description carries no boundary at all, the whole of it
/// is prose: a payload set as a paragraph is untidy, where a sentence set as
/// a command is a lie about what the provider said.
fn question_and_context(description: &str) -> (&str, &str) {
    let text = description.trim();
    fenced(text)
        .or_else(|| paragraphed(text))
        .unwrap_or((text, ""))
}

/// The question and material either side of the first fenced block.
///
/// A fence is the provider saying in so many words that what follows is not
/// prose, so it is the boundary to prefer where there is one.
fn fenced(text: &str) -> Option<(&str, &str)> {
    const FENCE: &str = "```";
    let start = text.find(FENCE)?;
    // Only a fence that opens a line opens a block. Three backticks inside a
    // sentence are being talked about rather than fencing anything.
    if start != 0 && !text[..start].ends_with('\n') {
        return None;
    }
    // The rest of the opening fence names a language, which is markup rather
    // than material, so the block starts at the line after it.
    let (_, body) = text[start + FENCE.len()..].split_once('\n')?;
    let body = body.trim_end();
    let body = body.strip_suffix(FENCE).unwrap_or(body);
    Some((text[..start].trim_end(), body.trim()))
}

/// The question and material either side of the first blank line.
fn paragraphed(text: &str) -> Option<(&str, &str)> {
    let mut from = 0;
    while let Some(index) = text[from..].find('\n') {
        let at = from + index;
        // A line holding nothing but whitespace is still a blank line, and a
        // carriage return is not content either.
        let rest = text[at + 1..].trim_start_matches([' ', '\t', '\r']);
        if rest.starts_with('\n') {
            return Some((text[..at].trim_end(), rest.trim()));
        }
        from = at + 1;
    }
    None
}

/// How much an answer commits the reader to.
///
/// This is what orders the answers and decides which of them the keyboard
/// reaches first. The model carries no such flag - a provider hands over a
/// list of ids and labels - so it is read from the vocabulary the providers
/// actually use. Reading one wrong costs an answer its place in the row,
/// never its meaning: every answer the provider offered is still drawn, and
/// still sends exactly the decision it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stake {
    /// Refusing, which is the answer that cannot cost the reader anything.
    Refuse,
    /// Agreeing to this one thing.
    Once,
    /// Agreeing to everything like it for the rest of the session.
    Session,
}

/// Whether either half of a choice's identity mentions `needle`.
///
/// Both are searched because a provider naming its decisions
/// `decision-0`,`decision-1` leaves the meaning entirely in the label, while
/// one labelling them from a payload leaves it entirely in the id. Compared
/// without allocating, because this runs for every answer of every pending
/// question on every frame.
fn mentions(choice: &chat::Choice, needle: &str) -> bool {
    let found = |text: &str| {
        text.len() >= needle.len()
            && text
                .as_bytes()
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
    };
    found(&choice.id) || found(&choice.label)
}

fn stake(choice: &chat::Choice) -> Stake {
    const REFUSALS: [&str; 5] = ["deny", "decline", "reject", "cancel", "abort"];
    const BREADTH: [&str; 3] = ["session", "always", "every"];
    if REFUSALS.iter().any(|word| mentions(choice, word)) {
        Stake::Refuse
    } else if BREADTH.iter().any(|word| mentions(choice, word)) {
        Stake::Session
    } else {
        Stake::Once
    }
}

/// The answer a bare Enter sends, where the question has one that cannot
/// grant anything.
///
/// A question with something to refuse gets no keyboard shortcut at all: the
/// keystroke that submits a form must never be the keystroke that approves a
/// command, and a reader typing an answer into a field should not be one
/// stray Return away from granting a permission. What is left is the
/// provider's own affirmative on a question that only asks - a form to
/// submit, an answer to send - which is exactly where Enter belongs.
fn submit_answer(request: &chat::Request) -> Option<&chat::Choice> {
    if request
        .actions
        .iter()
        .any(|choice| stake(choice) == Stake::Refuse)
    {
        return None;
    }
    request
        .actions
        .iter()
        .find(|choice| stake(choice) == Stake::Once)
}

/// The models `query` names, in the order the provider offered them.
///
/// The match is a case-insensitive substring of the whole identifier,
/// including its provider prefix, because that is how a reader recalls a
/// model: they remember `sonnet` or `4-5`, not whether the provider spelled
/// it `anthropic/claude-sonnet-4-5` or `Claude Sonnet 4.5`. Both the
/// identifier and the label are searched, so typing what the row shows finds
/// the row.
fn model_hits<'a>(models: &'a [chat::Model], query: &str) -> Vec<&'a chat::Model> {
    let query = query.trim().to_lowercase();
    models
        .iter()
        .filter(|model| {
            query.is_empty()
                || model.id.to_lowercase().contains(&query)
                || model.label.to_lowercase().contains(&query)
        })
        .collect()
}

impl Cagents {
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
                let accounts = self.chat_accounts(
                    runtime.session.launch.provider,
                    &runtime.session.launch.recipe,
                );
                // A draft chooses among every provider it could start on,
                // its own first; a started conversation only among its own
                // provider's models.
                let current = runtime.session.launch.provider;
                let offers = if runtime.choosing() {
                    std::iter::once(current)
                        .chain(MODEL_FAMILIES.into_iter().filter(|provider| {
                            *provider != current && self.account_ui.offers(*provider)
                        }))
                        .map(|provider| Offer {
                            provider,
                            models: self
                                .account_ui
                                .catalog(provider)
                                .map_or(&[][..], |catalog| catalog.models.as_slice()),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                runtime.view(
                    self.chat_start_pending(card) || self.chat_ui.closed.contains(&card),
                    self.settings.chat_enter_sends,
                    place,
                    !self.workspace_inputs_blocked(),
                    accounts,
                    offers,
                )
            }
            _ => widget::text::body("Connecting to native chat…").into(),
        }
    }
}

/// The providers a draft offers, in the order its picker lists them after
/// its own.
const MODEL_FAMILIES: [Provider; 3] = [Provider::Codex, Provider::Claude, Provider::Omp];

/// One provider's models, as a draft's picker offers them.
struct Offer<'a> {
    provider: Provider,
    /// What the provider listed last, under its default model. Empty where
    /// it never has, and then the picker offers the default alone.
    models: &'a [chat::Model],
}

/// What a provider is called where its models are listed.
fn family_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "ChatGPT",
        Provider::Claude => "Claude",
        Provider::Omp => "Other models",
    }
}

/// The worktree a conversation runs in.
struct Place {
    name: String,
    branch: Option<String>,
}

impl Runtime {
    // Provider descriptions can contain a whole prompt. Keep status controls
    // compact while the full transcript remains available on selection.
    fn short_description(raw: &str, limit: usize) -> String {
        let mut chars = raw.chars();
        let text: String = chars.by_ref().take(limit).collect();
        if chars.next().is_some() {
            format!("{text}…")
        } else {
            text
        }
    }
    fn view<'a>(
        &'a self,
        transitioning: bool,
        enter_sends: bool,
        place: Place,
        accepting_drops: bool,
        accounts: Option<ChatAccounts>,
        offers: Vec<Offer<'a>>,
    ) -> Element<'a, Message> {
        let target = self.target;
        // Nothing is sent past a first message still waiting to go.
        let available = !transitioning
            && !self.disconnected
            && !self.snapshot.exited
            && self.pending.is_none()
            && self.queued.is_none();
        let snapshot = &self.snapshot;
        // One theme lookup and one settings value per frame, rather than one
        // per rendered message.
        // Answers are read at a size above the shell's own captions and
        // controls, and code inside them at the body size rather than the
        // three quarters of it iced defaults to, which put a code span at
        // ten pixels.
        let markdown_settings = markdown::Settings {
            code_size: design::TEXT_BODY.into(),
            ..markdown::Settings::with_text_size(
                design::TEXT_PROSE,
                markdown::Style::from(if cosmic::theme::is_dark() {
                    iced::Theme::Dark
                } else {
                    iced::Theme::Light
                }),
            )
        };
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| snapshot.subagents.iter().find(|agent| &agent.id == id));
        let mut transcript = Vec::new();
        if let Some(agent) = selected {
            let heading = if agent.description.is_empty() {
                if agent.kind.is_empty() {
                    "Subagent".to_owned()
                } else {
                    agent.kind.clone()
                }
            } else {
                format!(
                    "{} · {}",
                    if agent.kind.is_empty() {
                        "Subagent"
                    } else {
                        &agent.kind
                    },
                    Self::short_description(&agent.description, 160),
                )
            };
            transcript.push(
                design::caption(heading)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
            if agent.items.is_empty() {
                transcript.push(body(if agent.status == chat::ToolStatus::Running {
                    "Waiting for subagent activity…"
                } else {
                    "No recent subagent activity is available here."
                }));
            }
            for item in &agent.items {
                let Some(cached) = self.cache.get(&item.id) else {
                    continue;
                };
                transcript.push(match &item.content {
                    ItemContent::Text { thinking, .. } => self.prose_view(
                        item,
                        cached,
                        if *thinking {
                            turn::Slot::Thinking
                        } else if item.role == chat::Role::System {
                            turn::Slot::Commentary
                        } else {
                            turn::Slot::Prose
                        },
                        markdown_settings,
                    ),
                    _ => self.step_view(item, cached, markdown_settings),
                });
            }
        } else {
            if snapshot.has_earlier {
                transcript.push(paging_view(
                    Page::Earlier,
                    self.paging() == Some(Page::Earlier),
                ));
            }
            if self.turns.is_empty() && self.queued.is_none() {
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
            for (_, error) in self.failures.iter().filter(|(anchor, _)| anchor.is_none()) {
                transcript.push(notice_entry(error));
            }
            for turn in &self.turns {
                transcript.push(self.turn_view(turn, markdown_settings));
                for (_, error) in self
                    .failures
                    .iter()
                    .filter(|(anchor, _)| anchor.as_ref() == Some(&turn.key))
                {
                    transcript.push(notice_entry(error));
                }
            }
            if snapshot.has_later {
                transcript.push(paging_view(Page::Newer, self.paging() == Some(Page::Newer)));
            }
            if let Some(queued) = &self.queued {
                transcript.push(self.queued_view(queued));
            }
        }
        // A chat's heading is the conversation's own name or nothing at all.
        //
        // It used to be followed by the newest system item's first line,
        // pinned on every frame. That row was status narration of the kind
        // this surface has been shedding: it cost the transcript a row
        // whatever the provider had put in it, a reader could not act on it,
        // and a genuine failure reaches them through the error surface
        // further down. Notices are prose in the transcript now, beside the
        // rest of what the session said.
        let mut content: Vec<Element<'_, Message>> = snapshot
            .title
            .iter()
            .map(|title| -> Element<'_, Message> { design::heading(title.as_str()).into() })
            .collect();
        // A plan is consulted rather than read, so it is pinned above the
        // transcript that scrolls under it. Inside the transcript it went out
        // of reach with the first thing the agent did after publishing it,
        // which is precisely when a reader starts asking what is left.
        // A failure is part of the conversation's record, so it is the last
        // entry in the transcript rather than a line pinned above the
        // composer: it scrolls away with what it was about instead of
        // staying on screen until the next command happens to clear it.
        if let Some(error) = self.error.as_ref().or(snapshot.error.as_ref())
            && !self.failures.iter().any(|(_, recorded)| recorded == error)
        {
            transcript.push(notice_entry(error));
        }
        content.extend(self.plan_view());
        let transcript = widget::scrollable(
            widget::column::with_children(transcript)
                // Turns are held apart further than anything inside one,
                // so the whitespace itself says where an exchange ends.
                // That is the separation a divider between messages was
                // doing badly.
                .spacing(step::GAP_L)
                .padding(design::with_vertical_rail(iced::Padding::ZERO))
                .width(Length::Fill),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .id(card_control_id(target.card, "chat-transcript"))
        .anchor_bottom()
        // Reaching either end of the loaded window is what asks for the next
        // page of history, so the viewport is the pagination control. The
        // signal is published when the viewport moves rather than once per
        // frame, which is what a streaming turn needs it to be.
        .on_scroll(move |viewport| action(Action::Scrolled(target, Scroll::of(&viewport))))
        .height(Length::Fill)
        .width(Length::Fill);
        content.push(scroll::transcript(
            transcript,
            selected
                .and_then(|agent| agent.items.first())
                .or_else(|| snapshot.items.first())
                .map(|item| item.id.as_str()),
        ));
        // A draft starting for its first message says so on the message
        // itself.
        if transitioning && self.queued.is_none() {
            content.push(body(
                "Switching or closing this session… Editing and session controls are disabled. You can still read and copy the conversation.",
            ));
        }
        if self.disconnected || snapshot.exited {
            let unresumable = self.offers_fresh_conversation();
            let enabled =
                !transitioning && self.pending.is_none() && !self.attachment_chooser_pending;
            let mut controls = vec![
                crate::design::caption(if unresumable {
                    // The provider's own sentence is already above this line,
                    // so this says what it means for the card, and what the
                    // second control does and does not do: the old
                    // conversation is left alone, but this card stops being
                    // the way back to it.
                    "The saved conversation did not load. Retry resumes it, or start a new one beside it - the old conversation stays on disk, but this card stops pointing at it."
                } else if snapshot.exited {
                    "Agent process exited. The transcript and draft are retained."
                } else {
                    "Disconnected. The daemon may still be working; your draft is retained."
                })
                .width(Length::Fill)
                .into(),
                icon(
                    "view-refresh-symbolic",
                    if unresumable {
                        "Resume this conversation again"
                    } else {
                        "Reconnect to this session"
                    },
                    enabled,
                    Action::Reconnect(target),
                ),
            ];
            if unresumable {
                controls.push(icon(
                    "list-add-symbolic",
                    "Start a new conversation beside it",
                    enabled,
                    Action::StartFresh(target.card),
                ));
            }
            content.push(
                widget::row::with_children(controls)
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
        // A question stays on this surface until the reader resolves it: no
        // answer is synthesized from a default, a timeout, or a closing pane.
        //
        // It is pinned here, between the conversation and the composer, which
        // is where the reader is already looking and where their hands
        // already are. It is deliberately not a modal over the pane: a
        // provider may ask an optional question and carry on working while it
        // waits, and nothing in the model tells that apart from a blocking
        // one, so taking the composer away from a reader who could still be
        // typing would cost more than the question being missable.
        if !snapshot.requests.is_empty() {
            content.push(
                widget::container(
                    widget::scrollable(
                        widget::column::with_children(
                            snapshot
                                .requests
                                .iter()
                                .map(|request| {
                                    request_view(self.target, request, &self.answers, available)
                                })
                                .collect::<Vec<_>>(),
                        )
                        .spacing(step::GAP_M)
                        .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                    )
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .height(Length::Shrink),
                )
                .max_height(REQUEST_CEILING)
                .into(),
            );
        }
        if !snapshot.subagents.is_empty() {
            let mut chips: Vec<Element<'_, Message>> = vec![
                widget::button::text("Conversation")
                    .class(crate::ui_controls::control_style(selected.is_none(), None))
                    .on_press(action(Action::SelectSubagent(target, None)))
                    .into(),
            ];
            for agent in snapshot.subagents.iter().rev() {
                let label = if agent.description.is_empty() {
                    if agent.kind.is_empty() {
                        "Subagent".to_owned()
                    } else {
                        agent.kind.clone()
                    }
                } else {
                    agent.description.clone()
                };
                let marker = match agent.status {
                    chat::ToolStatus::Running => "◌",
                    chat::ToolStatus::Succeeded => "✓",
                    chat::ToolStatus::Failed => "!",
                    chat::ToolStatus::Denied => "×",
                };
                chips.push(
                    widget::button::text(format!(
                        "{marker} {}",
                        Self::short_description(&label, 56)
                    ))
                    .class(crate::ui_controls::control_style(
                        selected.is_some_and(|active| active.id == agent.id),
                        None,
                    ))
                    .on_press(action(Action::SelectSubagent(
                        target,
                        Some(agent.id.clone()),
                    )))
                    .into(),
                );
            }
            content.push(
                widget::scrollable(widget::row::with_children(chips).spacing(step::GAP_XS))
                    .direction(iced::widget::scrollable::Direction::Horizontal(
                        design::scrollbar(),
                    ))
                    .width(Length::Fill)
                    .height(Length::Shrink)
                    .into(),
            );
            if selected.is_some() {
                content.push(
                    design::caption(
                        "Viewing a subagent · messages still go to the main conversation",
                    )
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
                );
            }
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
        // What Tab chooses while the completion list is open, and whether
        // Enter does too. A list whose rows cannot be chosen, while the chat
        // is changing hands, leaves the keys to the editor.
        let mut open = None;
        if let Some((token, rows)) = self.completion() {
            let highlighted = self.completion.highlighted.min(rows.len() - 1);
            let row = &rows[highlighted];
            open = (!transitioning).then(|| OpenList {
                insertion: row.insertion.to_string(),
                enter_chooses: self
                    .completion
                    .enter_chooses(&token, row, &self.snapshot.skills),
            });
            let (_, viewport) = completion_extent(rows.len());
            let rows = rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    let message = (!transitioning)
                        .then(|| action(Action::Complete(target, row.insertion.to_string(), None)));
                    completion_row(row, index == highlighted, message)
                })
                .collect::<Vec<_>>();
            content.push(
                widget::container(
                    widget::scrollable(
                        widget::column::with_children(rows)
                            .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                    )
                    .id(card_control_id(target.card, COMPLETION_LIST))
                    .on_scroll(move |viewport| {
                        action(Action::CompletionScrolled(
                            target,
                            viewport.absolute_offset().y,
                        ))
                    })
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .height(Length::Fixed(viewport)),
                )
                .padding(COMPLETION_INSET)
                .class(design::surface(
                    Some(design::raised),
                    [design::RADIUS_M; 4],
                    true,
                ))
                .into(),
            );
        }
        // A skill the provider will not load reaches the agent as words; the
        // draft says so rather than let the name pass for one that runs.
        let reach = snapshot.capabilities.skill_reach;
        if let Some(hint) = unloaded_hint(
            &completion::unloaded(&draft, &snapshot.skills, reach),
            reach,
        ) {
            content.push(
                design::caption(hint)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
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
            None => "Message",
        };
        // Key bindings must remain available while the editor changes its own
        // contents. The action handler checks the current draft before sending;
        // tying the binding to a view-time draft snapshot can leave Enter
        // inserting a newline until another pointer event redraws the pane.
        let key_submit = if available {
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
            // Up on the draft's first line and Down on its last walk the
            // sent messages, as a shell does; inside a longer draft the keys
            // keep moving the caret.
            let caret_line = self.composer.cursor().position.line;
            let on_first_line = caret_line == 0;
            let on_last_line = caret_line + 1 >= self.composer.line_count();
            let recall_older = on_first_line && !self.history.is_empty();
            let recall_newer = on_last_line && self.history_cursor.is_some();
            iced::widget::text_editor::TextEditor::new(&self.composer)
                .id(card_control_id(target.card, "chat-composer"))
                .placeholder(placeholder)
                .size(design::TEXT_PROSE)
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
                    if !press.modifiers.shift()
                        && !press.modifiers.control()
                        && !press.modifiers.alt()
                        && !press.modifiers.logo()
                        && matches!(
                            press.key,
                            keyboard::Key::Named(
                                keyboard::key::Named::PageUp | keyboard::key::Named::PageDown
                            )
                        )
                    {
                        return None;
                    }
                    // An open completion list has the keys a reader uses to
                    // pick from it ahead of sending and history.
                    if let Some(open) = &open
                        && let Some(choice) = completion_action(
                            target,
                            &press,
                            open,
                            submit_chord(&press, key_submit.0, key_submit.1, enter_sends),
                        )
                    {
                        return Some(text_editor::Binding::Custom(action(choice)));
                    }
                    if let Some(mode) =
                        submit_chord(&press, key_submit.0, key_submit.1, enter_sends)
                    {
                        return Some(text_editor::Binding::Custom(action(Action::Submit(
                            target, mode,
                        ))));
                    }
                    if let Some(older) = history_key(&press, recall_older, recall_newer) {
                        return Some(text_editor::Binding::Custom(action(if older {
                            Action::HistoryOlder(target)
                        } else {
                            Action::HistoryNewer(target)
                        })));
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
            // A four-corner collapse glyph reads as a window control, which is
            // what made the one control here that rewrites the conversation
            // look like something that resizes it. The tooltip names the
            // effect rather than the mechanism: what a reader wants from this
            // is the context figure back.
            actions.push(icon(
                "view-compress-symbolic",
                "Compact the conversation: summarize earlier turns to free context",
                available && !snapshot.busy,
                Action::Command(target, Command::Compact),
            ));
        }
        // Commands are reached by typing `/`, which is how every agent surface
        // a user arrives from works and what the composer already answers to.
        // A dropdown listing them duplicated that path in a control that was
        // permanently present for something used occasionally.
        actions.push(iced::widget::Space::new().width(Length::Fill).into());
        // Only the modes that can act right now are drawn. A permanently
        // present send control that is merely disabled is a control a reader
        // cannot use and cannot interpret: the queue button in particular was
        // a bare plus that never responded and never said what it was for.
        // Buttons still appear only when there is a message to send.
        if available && has_draft && key_submit.0.is_some() {
            for (slot, mode) in modes.iter().enumerate() {
                let (name, label) = submit_control(*mode, slot, enter_sends);
                actions.push(icon(name, label, true, Action::Submit(target, *mode)));
            }
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
        // The status line has two halves. What the agent is doing, where it is
        // doing it and the settings a reader changes mid-conversation scroll
        // sideways rather than wrapping, because a chat pane is often narrow
        // and a status line that grows to three rows takes that space from the
        // transcript.
        // The line always says what the conversation is doing, idle
        // included: a reader glancing at a card should never have to infer
        // its state from the absence of a word. The order is the order of
        // what a reader most needs to know first - that the card is between
        // sessions, that something is being sent, that the agent has stopped
        // or failed, that it is waiting on them, that it is working.
        let mut status: Vec<Element<'_, Message>> = Vec::new();
        let failed = self.error.is_some() || snapshot.error.is_some();
        let (glyph, state): (Option<&'static str>, &str) = if self.queued.is_some() {
            (Some("agent-working-symbolic"), "Starting")
        } else if transitioning {
            (Some("agent-working-symbolic"), "Switching session")
        } else if self.attachment_chooser_pending {
            (Some("agent-working-symbolic"), "Choosing attachments")
        } else if self.pending.is_some() {
            (Some("agent-working-symbolic"), "Sending")
        } else if self.disconnected {
            (Some("dialog-warning-symbolic"), "Disconnected")
        } else if snapshot.exited {
            (Some("dialog-warning-symbolic"), "Exited")
        } else if failed {
            (Some("dialog-warning-symbolic"), "Error")
        } else if snapshot.waiting || !snapshot.requests.is_empty() {
            (Some("agent-waiting-symbolic"), "Waiting for you")
        } else if snapshot.busy {
            (
                Some("agent-working-symbolic"),
                if snapshot.detail.is_empty() {
                    "Working"
                } else {
                    snapshot.detail.as_str()
                },
            )
        } else {
            (None, "Ready")
        };
        if let Some(glyph) = glyph {
            status.push(
                widget::icon(crate::ui_controls::symbolic_icon(glyph))
                    .size(12)
                    .into(),
            );
        }
        status.push(
            design::caption(state)
                .class(if failed && !transitioning && self.pending.is_none() {
                    cosmic::theme::Text::Custom(design::warning_text)
                } else {
                    cosmic::theme::Text::default()
                })
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
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
        status.extend(self.session_controls(transitioning, accounts, offers));
        // What the conversation has spent is pinned to the pane's right edge
        // instead of joining that run. The budget and the money are the two
        // figures a reader consults deliberately, and mixing them into the
        // same left-to-right sequence as a transient activity line pushed them
        // out of reach exactly when the agent was busy enough for them to
        // matter.
        let mut spend: Vec<Element<'_, Message>> = Vec::new();
        if usage.context_limit > 0 {
            // A context budget is a quantity a reader acts on - compact now,
            // or lose the thread - so it is drawn as a ring with its figure
            // beside it. The linear capsule this replaces spent the width of a
            // whole caption saying what the percentage already said.
            let used = (usage.context_tokens as f32 / usage.context_limit as f32).clamp(0.0, 1.0);
            spend.push(crate::interaction::tooltip(
                widget::row::with_children(vec![
                    crate::ui_controls::budget_radial(used, design::budget_tone(used)),
                    design::caption(format!("{:.0}%", used * 100.0))
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center),
                // The raw counts are checked once, if ever, so they are held
                // here rather than spending a permanent run of status-line
                // width: the ring and its percentage are what a reader acts on.
                format!(
                    "{} of {} context tokens · {} in · {} out",
                    usage.context_tokens,
                    usage.context_limit,
                    usage.input_tokens,
                    usage.output_tokens
                ),
            ));
        } else if usage.input_tokens + usage.output_tokens > 0 {
            // Without a context window there is no proportion to draw, so the
            // figures the provider does report stay legible as text.
            spend.push(
                design::caption(format!(
                    "{} in · {} out",
                    usage.input_tokens, usage.output_tokens
                ))
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            );
        }
        if let Some(cost) = usage.cost_microusd {
            // Two decimals, because the figure is money a reader is spending
            // and four decimals of a dollar is a number nobody reads.
            spend.push(design::caption(format!("${:.2}", cost as f64 / 1_000_000.0)).into());
        }
        content.push(
            widget::row::with_children(vec![
                widget::scrollable(
                    widget::row::with_children(status)
                        .spacing(step::GAP_S)
                        .align_y(Alignment::Center)
                        // The thumb floats over the row it belongs to, so its
                        // rail is reserved below the content the way every
                        // other scrollable in the app reserves one.
                        .padding(design::with_horizontal_rail(iced::Padding::ZERO)),
                )
                .direction(iced::widget::scrollable::Direction::Horizontal(
                    design::scrollbar(),
                ))
                .width(Length::Fill)
                .into(),
                widget::row::with_children(spend)
                    .spacing(step::GAP_S)
                    .align_y(Alignment::Center)
                    // The scrolling half reserves a rail under itself, so the
                    // pinned half reserves the same strip and the two read as
                    // one line rather than sitting half a rail apart.
                    .padding(design::with_horizontal_rail(iced::Padding::ZERO))
                    .into(),
            ])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .into(),
        );
        let pane = widget::container(
            widget::column::with_children(content)
                .spacing(step::GAP_S)
                .height(Length::Fill)
                .width(Length::Fill),
        )
        .padding(step::GAP_M)
        .width(Length::Fill)
        .height(Length::Fill);
        // The whole pane takes a drop, not just the composer: the transcript
        // is most of what a reader aims at, and a drop lands in the draft
        // wherever it was aimed.
        crate::file_drop::target(
            pane,
            ("chat", target.card),
            accepting_drops && !transitioning,
            move |payload| action(Action::Dropped(target, payload)),
        )
    }

    /// The page of history this conversation is waiting on, if any.
    fn paging(&self) -> Option<Page> {
        self.pending.as_ref().and_then(|pending| pending.page)
    }

    /// The plan the conversation is working to, or nothing where the provider
    /// has published none.
    ///
    /// Only the newest plan is drawn, and only as its steps' current state:
    /// the point of a pinned plan is what is left, not the document the agent
    /// wrote about it. Each step is one unwrapped line, and the block scrolls
    /// past [`PLAN_CEILING`], so a plan cannot take the pane from the
    /// conversation it is about.
    fn plan_view(&self) -> Option<Element<'_, Message>> {
        let plan = self.snapshot.plan.as_slice();
        if plan.is_empty() {
            return None;
        }
        let done = plan
            .iter()
            .filter(|entry| entry.status == chat::PlanState::Done)
            .count();
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::caption(format!("Plan · {done} of {} done", plan.len()))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        ];
        rows.extend(plan.iter().map(|entry| -> Element<'_, Message> {
            // The step in hand carries the same live indicator the rest of the
            // shell uses for work in flight, so a reader can tell where the
            // agent is without reading the plan at all.
            let marker: Element<'_, Message> = match entry.status {
                chat::PlanState::Active => crate::ui_controls::activity_spinner_tone(
                    f32::from(design::COMPACT_ICON_SIZE),
                    design::accent_color,
                ),
                chat::PlanState::Done => {
                    widget::icon(crate::ui_controls::symbolic_icon("emblem-ok-symbolic"))
                        .size(design::COMPACT_ICON_SIZE)
                        .into()
                }
                chat::PlanState::Pending => {
                    widget::icon(crate::ui_controls::symbolic_icon("list-remove-symbolic"))
                        .size(design::COMPACT_ICON_SIZE)
                        .into()
                }
            };
            let text = design::caption(entry.text.as_str())
                .wrapping(iced::widget::text::Wrapping::None)
                .width(Length::Fill);
            widget::row::with_children(vec![
                marker,
                // The step in hand keeps the reader's ink; what is finished and
                // what has not started are context for it.
                match entry.status {
                    chat::PlanState::Active => text,
                    _ => text.class(cosmic::theme::Text::Custom(design::muted_text)),
                }
                .into(),
            ])
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center)
            .into()
        }));
        Some(
            widget::container(
                widget::scrollable(
                    widget::column::with_children(rows)
                        .spacing(step::GAP_XS)
                        .padding(design::with_vertical_rail(iced::Padding::from([
                            step::GAP_XS,
                            step::GAP_S,
                        ]))),
                )
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                )),
            )
            .max_height(PLAN_CEILING)
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                true,
            ))
            .width(Length::Fill)
            .into(),
        )
    }

    /// The model, reasoning effort, and permission mode, at the status line's
    /// own scale.
    ///
    /// These were three full-size dropdowns stacked above the composer, which
    /// gave settings a reader changes occasionally more room than the message
    /// they were about to send. While a session is switching they are drawn as
    /// plain text: the values still matter, and a row of disabled controls
    /// says nothing a reader can act on.
    fn session_controls<'a>(
        &'a self,
        transitioning: bool,
        accounts: Option<ChatAccounts>,
        offers: Vec<Offer<'a>>,
    ) -> Vec<Element<'a, Message>> {
        let target = self.target;
        let snapshot = &self.snapshot;
        let mut controls: Vec<Element<'a, Message>> = Vec::new();
        if let Some(accounts) = accounts {
            controls.push(self.account_picker(accounts, transitioning));
        }
        let draft = self.is_draft();
        if (snapshot.capabilities.model_selection && !snapshot.models.is_empty()) || draft {
            let label: Cow<'a, str> = match snapshot
                .models
                .iter()
                .find(|model| Some(&model.id) == snapshot.selected_model.as_ref())
            {
                Some(model) => model.label.as_str().into(),
                None => match &snapshot.selected_model {
                    // A model the provider no longer lists is still the one
                    // chosen, so it is named rather than hidden.
                    Some(id) => id.as_str().into(),
                    None if draft => format!(
                        "{} · Default model",
                        family_label(self.session.launch.provider)
                    )
                    .into(),
                    None => "Model".into(),
                },
            };
            if transitioning {
                controls.push(design::caption(label).into());
            } else {
                controls.push(self.model_picker(label, offers));
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
                let open = self.reasoning_picker;
                let anchor = widget::button::custom(design::caption(
                    selected.map_or("Reasoning", |index| model.reasoning[index].as_str()),
                ))
                .name("Choose reasoning effort")
                .class(crate::ui_controls::control_style(open, None))
                .padding(CONTROL_PADDING)
                .on_press(action(Action::ReasoningPicker(target, !open)));
                let choices = open.then(|| {
                    widget::column::with_children(
                        model
                            .reasoning
                            .iter()
                            .enumerate()
                            .map(|(index, reasoning)| {
                                widget::button::custom(design::body(reasoning.as_str()))
                                    .class(crate::ui_controls::control_style(
                                        Some(index) == selected,
                                        None,
                                    ))
                                    .width(Length::Fill)
                                    .on_press(action(Action::Reasoning(
                                        target,
                                        model.id.clone(),
                                        reasoning.clone(),
                                    )))
                                    .into()
                            })
                            .collect::<Vec<_>>(),
                    )
                    .width(160)
                    .into()
                });
                controls.push(crate::ui_controls::popover(
                    anchor,
                    choices,
                    widget::popover::Position::Top,
                    action(Action::ReasoningPicker(target, false)),
                ));
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

    /// The account a new conversation runs on, chosen before its first
    /// message and fixed after it: the conversation belongs to the account
    /// that started it, and resuming it anywhere else would find nothing.
    fn account_picker<'a>(
        &'a self,
        accounts: ChatAccounts,
        transitioning: bool,
    ) -> Element<'a, Message> {
        let target = self.target;
        let open = self.account_picker && !transitioning;
        let label = design::caption(accounts.current.clone())
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ));
        let icon = widget::icon(crate::ui_controls::symbolic_icon("avatar-default-symbolic"))
            .size(design::COMPACT_ICON_SIZE);
        if transitioning || !self.choosing() {
            // Fixed: stated, not offered.
            return crate::interaction::tooltip(
                widget::row::with_children(vec![icon.into(), label.into()])
                    .spacing(step::GAP_XS)
                    .align_y(Alignment::Center),
                format!(
                    "This conversation runs on {}. Start a new one to use another account.",
                    accounts.current
                ),
            );
        }
        let anchor = widget::button::custom(
            widget::row::with_children(vec![
                icon.into(),
                label.into(),
                widget::icon(crate::ui_controls::symbolic_icon("pan-down-symbolic"))
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
            ])
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center),
        )
        .name("Choose an account")
        .class(crate::ui_controls::control_style(open, None))
        .padding(CONTROL_PADDING)
        .on_press(action(Action::AccountPicker(target, !open)));
        let choices = open.then(|| {
            widget::column::with_children(
                accounts
                    .choices
                    .into_iter()
                    .map(|choice| {
                        let mut row: Vec<Element<'a, Message>> = Vec::new();
                        match choice.used {
                            Some(used) => row.push(crate::ui_controls::budget_radial(
                                used,
                                design::budget_tone(used),
                            )),
                            None => row.push(
                                iced::widget::Space::new()
                                    .width(design::RADIAL_GAUGE_SIZE)
                                    .into(),
                            ),
                        }
                        let mut text: Vec<Element<'a, Message>> = vec![
                            design::body(choice.label.clone())
                                .wrapping(iced::widget::text::Wrapping::None)
                                .ellipsize(iced::widget::text::Ellipsize::End(
                                    iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                                ))
                                .into(),
                        ];
                        if !choice.detail.is_empty() {
                            text.push(design::caption(choice.detail).into());
                        }
                        row.push(
                            widget::column::with_children(text)
                                .spacing(1)
                                .width(Length::Fill)
                                .into(),
                        );
                        widget::button::custom(
                            widget::row::with_children(row)
                                .spacing(step::GAP_S)
                                .align_y(Alignment::Center),
                        )
                        .name(choice.label)
                        .selected(choice.selected)
                        .class(crate::ui_controls::control_style(choice.selected, None))
                        .padding([step::GAP_XS, step::GAP_S])
                        .width(Length::Fill)
                        .on_press(action(Action::ChooseAccount(target, choice.id)))
                        .into()
                    })
                    .chain(std::iter::once(
                        widget::button::custom(design::caption("Manage accounts…"))
                            .class(crate::ui_controls::control_style(false, None))
                            .padding([step::GAP_XS, step::GAP_S])
                            .width(Length::Fill)
                            .on_press(Message::AccountUi(crate::app::account_ui::Action::OpenPane))
                            .into(),
                    ))
                    .collect::<Vec<_>>(),
            )
            .spacing(2)
            .width(design::MODEL_POPUP_WIDTH)
            .into()
        });
        crate::ui_controls::popover(
            anchor,
            choices,
            widget::popover::Position::Top,
            action(Action::AccountPicker(target, false)),
        )
    }

    /// Choosing a model by naming it rather than by finding it in a list.
    ///
    /// A dropdown is the right control for a handful of choices and the wrong
    /// one for hundreds. Providers now front models by the dozen - one of
    /// them offers every dated snapshot it has ever published - and a popup
    /// listing all of them left the reader scrolling a wall of near-identical
    /// slugs for one they could have named in three keystrokes. The field
    /// takes focus with the panel, narrows the list as they type, answers
    /// Enter with the top match and Escape by dismissing, so a model can be
    /// changed without the pointer ever entering the list.
    ///
    /// The anchor carries the provider's mark, because a draft's model is
    /// also its choice of agent: the one control says both which model and
    /// whose.
    fn model_picker<'a>(
        &'a self,
        label: Cow<'a, str>,
        offers: Vec<Offer<'a>>,
    ) -> Element<'a, Message> {
        let target = self.target;
        let open = self.model_picker.is_some();
        let anchor = widget::button::custom(
            widget::row::with_children(vec![
                widget::icon(crate::ui_controls::provider_icon(
                    self.session.launch.provider,
                ))
                .size(design::COMPACT_ICON_SIZE)
                .into(),
                design::caption(label)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                widget::icon(crate::ui_controls::symbolic_icon("pan-down-symbolic"))
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
            ])
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center),
        )
        .name("Choose a model")
        .class(crate::ui_controls::control_style(open, None))
        .padding(CONTROL_PADDING)
        .on_press(action(Action::ModelPicker(target, !open)));
        crate::ui_controls::popover(
            anchor,
            self.model_search(offers),
            widget::popover::Position::Top,
            action(Action::ModelPicker(target, false)),
        )
    }

    /// The search field and list, pinned above the status line the picker
    /// sits on, or nothing while the picker is closed.
    ///
    /// A started conversation lists its own provider's models. A draft lists
    /// every provider it could start on, in one list under a heading for
    /// each, because which agent to talk to is part of choosing the model
    /// rather than a mode the list is in.
    ///
    /// The shared popover anchors this above its control without changing the
    /// transcript's height or scroll position.
    fn model_search<'a>(&'a self, offers: Vec<Offer<'a>>) -> Option<Element<'a, Message>> {
        let query = self.model_picker.as_deref()?;
        let provider = self.session.launch.provider;
        let draft = self.choosing();
        let offers = if draft {
            offers
        } else {
            vec![Offer {
                provider,
                models: &self.snapshot.models,
            }]
        };
        Some(model_list(
            self.target,
            &offers,
            draft,
            (provider, self.snapshot.selected_model.as_deref()),
            query,
        ))
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

    /// A draft's first message while its conversation starts.
    ///
    /// It is drawn where it will be once sent, as the reader's own message,
    /// with what it is waiting on under it: sending it cleared the composer,
    /// and a message that vanished until the provider answered would read
    /// as lost.
    fn queued_view<'a>(&'a self, queued: &'a Queued) -> Element<'a, Message> {
        let waiting = if self.is_draft() || !self.snapshot.ready {
            format!("Starting {}…", family_label(self.session.launch.provider))
        } else {
            "Sending…".to_owned()
        };
        // Until the message itself is on its way it can still be taken back,
        // to be edited or sent somewhere else.
        let sending = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.queued == Some(super::QueuedStep::Prompt));
        let mut rows: Vec<Element<'a, Message>> = vec![
            widget::container(
                widget::text(queued.text.trim_end())
                    .size(design::TEXT_PROSE)
                    .width(Length::Shrink),
            )
            .class(design::surface(
                Some(design::accent_wash),
                [
                    design::RADIUS_L,
                    design::RADIUS_L,
                    design::RADIUS_XS,
                    design::RADIUS_L,
                ],
                false,
            ))
            .padding([step::GAP_S as u16, step::GAP_M as u16])
            .max_width(PROMPT_MEASURE)
            .into(),
        ];
        for attachment in &queued.attachments {
            rows.push(
                design::caption(
                    attachment
                        .path
                        .file_name()
                        .unwrap_or(attachment.path.as_os_str())
                        .to_string_lossy(),
                )
                .into(),
            );
        }
        rows.push(
            widget::row::with_children(vec![
                crate::ui_controls::activity_spinner_tone(
                    f32::from(design::COMPACT_ICON_SIZE),
                    design::accent_color,
                ),
                design::caption(waiting)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
                crate::ui_controls::compact_icon_control(
                    crate::ui_controls::symbolic_icon("edit-undo-symbolic"),
                    "Take the message back to edit it",
                    (!sending).then(|| action(Action::CancelQueued(self.target))),
                )
                .into(),
            ])
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center)
            .into(),
        );
        widget::column::with_children(rows)
            .spacing(step::GAP_XS)
            .align_x(Alignment::End)
            .width(Length::Fill)
            .into()
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
                .size(design::TEXT_PROSE)
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

    /// An assistant answer, its reasoning, its narration, or a system notice.
    ///
    /// Every prose phase shares the message body size and Markdown renderer,
    /// including partial streaming paragraphs.
    fn prose_view<'a>(
        &'a self,
        item: &'a chat::Item,
        cached: &'a CachedItem,
        slot: turn::Slot,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let target = self.target;
        if slot == turn::Slot::Commentary {
            return widget::container(Prose::view(cached, settings))
                .max_width(design::READABLE_MEASURE)
                .width(Length::Fill)
                .into();
        }
        if slot == turn::Slot::Thinking {
            // Reasoning keeps a subtle surface without shrinking the prose.
            return widget::container(
                widget::container(Prose::view(cached, settings))
                    .class(design::surface(
                        Some(design::sunken),
                        [design::RADIUS_S; 4],
                        false,
                    ))
                    .padding([step::GAP_S as u16, step::GAP_M as u16])
                    .max_width(design::READABLE_MEASURE),
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
        let mut rows: Vec<Element<'a, Message>> = vec![Prose::view(cached, settings)];
        // An answer's footer says when it finished and offers to copy it, and
        // nothing else: a reader of an answer reaches for those two, and the
        // Markdown source of a rendered answer was a developer's control on a
        // reader's surface.
        // The controls lead and the time follows, muted like the time: a
        // reader reaches for copy far more often than they read the stamp,
        // and an icon in the text color beside a muted stamp was the one
        // thing on the row that looked like it wanted attention.
        let mut controls: Vec<Element<'a, Message>> = vec![small_icon(
            "edit-copy-symbolic",
            "Copy this message",
            Action::Copy(target, item.id.clone(), CopyPart::Text),
        )];
        for index in 0..cached.code.codes.len() {
            controls.push(small_icon(
                "edit-copy-symbolic",
                code_label(index),
                Action::Copy(target, item.id.clone(), CopyPart::Code(index)),
            ));
        }
        if let Some(stamp) = item.at.map(clock_label).filter(|at| !at.is_empty()) {
            controls.push(
                design::caption(stamp)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
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
}

/// What sits at the end of the loaded window while the conversation
/// carries on past it.
///
/// There is no control here, because the page is asked for by the gesture
/// that reaches this edge. What a reader needs at it is the two things
/// they cannot otherwise know: that the conversation does not begin, or
/// end, where the transcript does, and that a page they have just asked
/// for by scrolling is on its way. Both belong at the edge the rows will
/// appear at rather than in a button beside it.
fn paging_view(page: Page, loading: bool) -> Element<'static, Message> {
    let mut row: Vec<Element<'_, Message>> = Vec::new();
    if loading {
        row.push(crate::ui_controls::activity_spinner_tone(
            f32::from(design::COMPACT_ICON_SIZE),
            design::muted_color,
        ));
    }
    row.push(
        design::caption(match (page, loading) {
            (Page::Earlier, true) => "Loading earlier history…",
            (Page::Earlier, false) => "Scroll up for earlier history",
            (Page::Newer, true) => "Loading the newest messages…",
            (Page::Newer, false) => "Scroll down for the newest messages",
        })
        .class(cosmic::theme::Text::Custom(design::muted_text))
        .into(),
    );
    widget::container(
        widget::row::with_children(row)
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center),
    )
    .center_x(Length::Fill)
    .padding(step::GAP_S)
    .into()
}

/// What a reader has typed or picked so far, keyed by request and field.
pub(super) type Answers = HashMap<(String, String), Vec<String>>;

/// A question the provider is waiting on, drawn as the decision the
/// reader is making.
///
/// The question comes first and is set at prose width. The material it
/// refers to - a command, a path, a permission set, a payload - is set as
/// the literal text it is rather than crammed into a caption beside it.
/// The answers are real controls in a deliberate order at the foot of it,
/// which is where a reader looks for the thing they have to do.
///
/// What this replaces was a flat column in which the provider's prose, a
/// blob of JSON, a stray text field and the decisions themselves all had
/// the same weight, so the one thing the reader had to do was the hardest
/// thing on it to find.
fn request_view<'a>(
    target: Target,
    request: &'a chat::Request,
    answers: &'a Answers,
    available: bool,
) -> Element<'a, Message> {
    let (question, context) = question_and_context(&request.description);
    let mut rows: Vec<Element<'a, Message>> = vec![
        widget::row::with_children(vec![
            // The same glyph the status line shows while the agent waits
            // on the reader, so the pane and the panel agree about who is
            // holding the conversation up.
            widget::icon(crate::ui_controls::symbolic_icon("agent-waiting-symbolic"))
                .size(design::ROW_ICON_SIZE)
                .into(),
            design::heading(request.title.as_str())
                .width(Length::Fill)
                .into(),
        ])
        .spacing(step::GAP_S)
        .align_y(Alignment::Center)
        .into(),
    ];
    if !question.is_empty() {
        rows.push(
            widget::container(body(question))
                .max_width(design::READABLE_MEASURE)
                .into(),
        );
    }
    if !context.is_empty() {
        // The same monospaced block a command's output is read in, for
        // the same reason: a path, an argv or a diff read as the thing
        // they are only when they are set as text rather than as prose.
        rows.push(step::code_surface(
            context,
            None,
            Some(REQUEST_CONTEXT_CEILING),
            None,
        ));
    }
    for field in &request.fields {
        rows.push(field_view(target, request, field, answers, available));
    }
    rows.push(answers_view(target, request, available));
    widget::container(
        widget::column::with_children(rows)
            .spacing(step::GAP_M)
            .width(Length::Fill),
    )
    .class(design::surface(
        Some(design::raised),
        [design::RADIUS_M; 4],
        true,
    ))
    .padding(step::GAP_M)
    .width(Length::Fill)
    .into()
}

/// One thing a question asks for: its own prompt, the answers the
/// provider offered for it, and the free text it will also take.
///
/// A field's label is a header and its description is the question
/// itself, so they are drawn as those two things rather than as two
/// captions of equal weight. The text input stays whatever choices exist,
/// because a provider that offers options may still accept an answer that
/// is not one of them, and dropping it would silently discard an answer
/// the model supports.
fn field_view<'a>(
    target: Target,
    request: &'a chat::Request,
    field: &'a chat::Field,
    answers: &'a Answers,
    available: bool,
) -> Element<'a, Message> {
    let values = answers.get(&(request.id.clone(), field.id.clone()));
    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    if !field.label.is_empty() {
        rows.push(
            design::caption(field.label.as_str())
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        );
    }
    if !field.description.is_empty() {
        rows.push(
            widget::container(body(field.description.as_str()))
                .max_width(design::READABLE_MEASURE)
                .into(),
        );
    }
    if field.multiple && !field.choices.is_empty() {
        // Whether one answer or several are wanted is something only the
        // field knows, and a reader cannot tell it from a column of
        // identical rows.
        rows.push(
            design::caption("Choose any that apply.")
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        );
    }
    for choice in &field.choices {
        let selected = values.is_some_and(|values| values.contains(&choice.id));
        let mut label: Vec<Element<'a, Message>> = vec![
            design::body(choice.label.as_str())
                .width(Length::Fill)
                .into(),
        ];
        if !choice.description.is_empty() {
            // A choice's own explanation belongs inside the control it
            // explains. On a row of its own it read as one more answer.
            label.push(
                design::caption(choice.description.as_str())
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .width(Length::Fill)
                    .into(),
            );
        }
        rows.push(
            widget::button::custom(
                widget::row::with_children(vec![
                    // The mark keeps its column whether or not this row
                    // carries it, so a list of answers reads as a list
                    // rather than stepping in and out at its left edge.
                    if selected {
                        widget::icon(crate::ui_controls::symbolic_icon("emblem-ok-symbolic"))
                            .size(design::COMPACT_ICON_SIZE)
                            .into()
                    } else {
                        iced::widget::Space::new()
                            .width(f32::from(design::COMPACT_ICON_SIZE))
                            .into()
                    },
                    widget::column::with_children(label)
                        .spacing(step::GAP_XS)
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(step::GAP_S)
                .align_y(Alignment::Center),
            )
            .name(choice.label.as_str())
            .class(crate::ui_controls::control_style(selected, None))
            .selected(selected)
            .padding(iced::Padding::from([step::GAP_XS, step::GAP_S]))
            .width(Length::Fill)
            .on_press_maybe(available.then_some(action(Action::Choice(
                target,
                request.id.clone(),
                field.id.clone(),
                choice.id.clone(),
            ))))
            .into(),
        );
    }
    let typed = values
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
                "Or type an answer"
            },
            typed,
        )
        .id(widget::Id::new(format!(
            "chat.{}.{}.{}",
            target.card, request.id, field.id
        )))
        .secure(field.secret)
        .size(design::TEXT_BODY)
        .on_input_maybe(available.then_some(move |value| {
            action(Action::Field(
                target,
                request_id.clone(),
                field_id.clone(),
                value,
            ))
        }))
        // Enter sends the answer only where the provider offered nothing
        // to refuse: a key press must never be what grants a permission.
        .on_submit_maybe(submit_answer(request).filter(|_| available).map(|choice| {
            action(Action::Answer(
                target,
                request.id.clone(),
                choice.id.clone(),
            ))
        }))
        .into(),
    );
    widget::column::with_children(rows)
        .spacing(step::GAP_S)
        .width(Length::Fill)
        .into()
}

/// The decisions the provider offered, in the order a reader should meet
/// them.
///
/// Refusing comes first, because it is the answer that cannot cost
/// anything and so the one the keyboard should reach first. Agreeing to
/// the one thing comes next. Agreeing for the rest of the session comes
/// last, because it is the widest thing that can be said here. Only a
/// question with nothing to refuse draws its affirmative as the primary
/// control: a consent dialog must not make granting the obvious default.
fn answers_view<'a>(
    target: Target,
    request: &'a chat::Request,
    available: bool,
) -> Element<'a, Message> {
    if request.actions.is_empty() {
        // A provider can ask something this surface has no answer for -
        // a consent dialog it wants to draw itself. Saying so is the
        // difference between a panel with no controls and a panel whose
        // controls failed to appear.
        return design::caption(
            "This request cannot be answered here. Interrupt the turn to cancel it.",
        )
        .class(cosmic::theme::Text::Custom(design::muted_text))
        .into();
    }
    let mut answers: Vec<&chat::Choice> = request.actions.iter().collect();
    // A stable sort, so answers of equal stake keep the order the
    // provider offered them in.
    answers.sort_by_key(|choice| stake(choice));
    let primary = submit_answer(request).map(|choice| &choice.id);
    widget::row::with_children(
        answers
            .into_iter()
            .map(|choice| {
                let message = available.then_some(action(Action::Answer(
                    target,
                    request.id.clone(),
                    choice.id.clone(),
                )));
                let control: Element<'a, Message> = if primary == Some(&choice.id) {
                    widget::button::suggested(choice.label.as_str())
                        .on_press_maybe(message)
                        .into()
                } else {
                    widget::button::standard(choice.label.as_str())
                        .on_press_maybe(message)
                        .into()
                };
                if choice.description.is_empty() {
                    control
                } else {
                    // Providers put the whole payload a decision sends in
                    // its description, which is a paragraph of JSON on a
                    // button. Held here it stays available to a reader who
                    // wants it without being in the way of one who does
                    // not.
                    crate::interaction::tooltip(control, choice.description.as_str())
                }
            })
            .collect::<Vec<_>>(),
    )
    .spacing(step::GAP_S)
    .align_y(Alignment::Center)
    // Answers wrap rather than run off the edge of a narrow pane: an
    // answer a reader cannot see is an answer they do not have.
    .wrap()
    .into()
}

/// One row of the model list: a model, or a provider's default where it has
/// not listed any.
struct ModelRow<'a> {
    provider: Provider,
    model: Option<&'a chat::Model>,
}

impl ModelRow<'_> {
    /// What choosing the row does: a draft is pointed at it, and a started
    /// conversation asked to switch to it.
    fn choose(&self, target: Target, draft: bool) -> Message {
        let id = self.model.map(|model| model.id.clone());
        match id {
            Some(id) if !draft => action(Action::Model(target, id)),
            id => action(Action::DraftModel(target, self.provider, id)),
        }
    }
}

/// The rows `query` names in each of `offers`, grouped by provider.
///
/// A query naming a provider names every model it offers, so "claude" lists
/// Claude's models whatever they are called. A draft can also start on a
/// provider's default model, which heads its group, and is all a provider
/// that has never listed its models offers; a started conversation can only
/// switch to a model it names.
fn model_rows<'a>(
    offers: &[Offer<'a>],
    query: &str,
    draft: bool,
) -> Vec<(Provider, Vec<ModelRow<'a>>)> {
    let needle = query.trim().to_lowercase();
    offers
        .iter()
        .filter_map(|offer| {
            // A letter alone begins too many names to mean one provider.
            let named = needle.chars().count() > 1 && names_provider(offer.provider, &needle);
            let default = (draft || offer.models.is_empty())
                && (needle.is_empty() || named || "default".starts_with(&needle));
            let rows: Vec<ModelRow<'a>> = default
                .then_some(ModelRow {
                    provider: offer.provider,
                    model: None,
                })
                .into_iter()
                .chain(
                    model_hits(offer.models, if named { "" } else { query })
                        .into_iter()
                        .map(|model| ModelRow {
                            provider: offer.provider,
                            model: Some(model),
                        }),
                )
                .collect();
            (!rows.is_empty()).then_some((offer.provider, rows))
        })
        .collect()
}

/// Whether `needle`, lowercased, begins a word the reader could know
/// `provider` by: what its models are listed under, or the agent that runs
/// them.
fn names_provider(provider: Provider, needle: &str) -> bool {
    let names: &[&str] = match provider {
        Provider::Codex => &["chatgpt", "codex", "openai"],
        Provider::Claude => &["claude", "anthropic"],
        Provider::Omp => &["other models", "omp"],
    };
    names.iter().any(|name| {
        name.starts_with(needle) || name.split(' ').any(|word| word.starts_with(needle))
    })
}

/// The search field and the models `query` names among `offers`, under a
/// heading for each provider where there is more than one to choose from.
///
/// `current` is the provider and model the conversation is on. It stays in
/// the list and stays marked, because the first thing a reader opening this
/// wants to know is which one they are already using.
fn model_list<'a>(
    target: Target,
    offers: &[Offer<'a>],
    draft: bool,
    current: (Provider, Option<&str>),
    query: &'a str,
) -> Element<'a, Message> {
    let groups = model_rows(offers, query, draft);
    // Only a search has a best match; Enter on an empty one would move the
    // conversation to whatever happens to be listed first.
    let top = groups
        .first()
        .and_then(|(_, rows)| rows.first())
        .filter(|_| !query.trim().is_empty())
        .map(|row| row.choose(target, draft));
    let headed = draft && offers.len() > 1;
    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    if groups.is_empty() {
        rows.push(
            design::caption("No model matches that.")
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        );
    }
    for (index, (provider, models)) in groups.into_iter().enumerate() {
        if headed {
            rows.push(
                widget::container(
                    widget::row::with_children(vec![
                        widget::icon(crate::ui_controls::provider_icon(provider))
                            .size(design::COMPACT_ICON_SIZE)
                            .into(),
                        design::caption(family_label(provider))
                            .class(cosmic::theme::Text::Custom(design::muted_text))
                            .into(),
                    ])
                    .spacing(step::GAP_XS)
                    .align_y(Alignment::Center),
                )
                // Groups are set apart by the space above their heading
                // rather than by a rule, which would be one more line in a
                // list that is all lines.
                .padding(
                    iced::Padding::from([step::GAP_XS, step::GAP_S]).top(if index == 0 {
                        step::GAP_XS
                    } else {
                        step::GAP_M
                    }),
                )
                .into(),
            );
        }
        for row in models {
            let selected =
                provider == current.0 && row.model.map(|model| model.id.as_str()) == current.1;
            let (title, id, name) = match row.model {
                Some(model) => (
                    model.label.as_str(),
                    Some(model.id.as_str()),
                    Cow::Borrowed(model.label.as_str()),
                ),
                None => (
                    "Default model",
                    None,
                    Cow::Owned(format!("{} default model", family_label(provider))),
                ),
            };
            let mut label: Vec<Element<'a, Message>> = vec![
                design::body(title)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ];
            // The identifier is shown only where it is not already the
            // label, because most providers use one string for both and a
            // row that prints it twice is half a list of duplicates.
            let listed = offers
                .iter()
                .any(|offer| offer.provider == provider && !offer.models.is_empty());
            let detail = match id {
                Some(id) if id != title => Some(id),
                Some(_) => None,
                None if listed => Some("Whichever it would choose itself"),
                None => Some("Its models are listed once it has run"),
            };
            if let Some(detail) = detail {
                label.push(
                    design::caption(detail)
                        .class(cosmic::theme::Text::Custom(design::muted_text))
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into(),
                );
            }
            rows.push(
                widget::button::custom(
                    widget::row::with_children(vec![
                        // The mark keeps its column whether or not this row
                        // carries it, so the labels stay in one line down
                        // the list instead of stepping in and out.
                        if selected {
                            widget::icon(crate::ui_controls::symbolic_icon("emblem-ok-symbolic"))
                                .size(design::COMPACT_ICON_SIZE)
                                .into()
                        } else {
                            iced::widget::Space::new()
                                .width(f32::from(design::COMPACT_ICON_SIZE))
                                .into()
                        },
                        widget::column::with_children(label)
                            .width(Length::Fill)
                            .into(),
                    ])
                    .spacing(step::GAP_S)
                    .align_y(Alignment::Center),
                )
                .name(name)
                .class(crate::ui_controls::control_style(selected, None))
                .selected(selected)
                .padding(iced::Padding::from([step::GAP_XS, step::GAP_S]))
                // The row spans the list, so the highlight under the model in
                // use is a band across it rather than a patch the width of its
                // name.
                .width(Length::Fill)
                .on_press(row.choose(target, draft))
                .into(),
            );
        }
    }
    widget::column::with_children(vec![
        widget::search_input(
            if headed {
                "Search models and providers"
            } else {
                "Search models"
            },
            query,
        )
        .id(card_control_id(target.card, "chat-model-search"))
        .size(design::TEXT_BODY)
        .width(Length::Fill)
        .on_input(move |query| action(Action::ModelQuery(target, query)))
        // Enter takes the top match, which is what makes three keystrokes
        // enough to change model.
        .on_submit_maybe(top.map(|choice| move |_: String| choice.clone()))
        // The popover owns dismissal. Unfocusing on mouse-down inside a
        // model row must not remove that row before its mouse-up selects it.
        .into(),
        widget::container(
            widget::scrollable(
                widget::column::with_children(rows)
                    .padding(design::with_vertical_rail(iced::Padding::ZERO)),
            )
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .height(Length::Shrink),
        )
        .max_height(design::MODEL_POPUP_CEILING)
        .into(),
    ])
    .spacing(step::GAP_XS)
    // The width is stated rather than left to the content: without it the
    // panel spanned the pane and the list became the wall of slugs this
    // replaces.
    .width(design::MODEL_POPUP_WIDTH)
    .into()
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

    /// A press of `named`, which is what routing reads; the physical key
    /// is not.
    fn key(named: keyboard::key::Named, modifiers: keyboard::Modifiers) -> text_editor::KeyPress {
        let key = keyboard::Key::Named(named);
        text_editor::KeyPress {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            modifiers,
            text: None,
            status: text_editor::Status::Focused { is_hovered: false },
        }
    }

    /// With the list open the keys a reader picks with go to it, and Enter
    /// goes to it only when choosing still changes something; otherwise it
    /// sends, or edits, exactly as it would with no list.
    #[test]
    fn an_open_list_takes_only_the_keys_it_answers_to() {
        use keyboard::key::Named;
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let list = |enter_chooses| OpenList {
            insertion: "$commits".into(),
            enter_chooses,
        };
        let none = keyboard::Modifiers::empty();
        let send = Some(PromptMode::Send);
        let route = |named, modifiers, open: &OpenList| {
            let press = key(named, modifiers);
            let send = submit_chord(&press, send, None, true);
            completion_action(target, &press, open, send)
        };

        assert!(matches!(
            route(Named::ArrowDown, none, &list(false)),
            Some(Action::CompletionMove(_, true))
        ));
        assert!(matches!(
            route(Named::ArrowUp, none, &list(false)),
            Some(Action::CompletionMove(_, false))
        ));
        assert!(matches!(
            route(Named::Escape, none, &list(false)),
            Some(Action::CompletionDismiss(_))
        ));
        // Tab always chooses, and does nothing else if the row is gone when
        // it lands.
        assert!(matches!(
            route(Named::Tab, none, &list(false)),
            Some(Action::Complete(_, insertion, None)) if insertion == "$commits"
        ));
        // Enter chooses only when the list says it would change the word, and
        // sends as it would have if the word moved on before it landed.
        assert!(matches!(
            route(Named::Enter, none, &list(true)),
            Some(Action::Complete(_, insertion, Some(otherwise)))
                if insertion == "$commits"
                    && matches!(*otherwise, Action::Submit(_, PromptMode::Send))
        ));
        assert!(route(Named::Enter, none, &list(false)).is_none());
        // Where Enter breaks the line, that is what it falls back to.
        let press = key(Named::Enter, none);
        assert!(matches!(
            completion_action(target, &press, &list(true), None),
            Some(Action::Complete(_, _, Some(otherwise)))
                if matches!(
                    *otherwise,
                    Action::Composer(_, text_editor::Action::Edit(text_editor::Edit::Enter))
                )
        ));
        // Chords reach past the list, to sending and to the editor.
        for (named, modifiers) in [
            (Named::Enter, keyboard::Modifiers::COMMAND),
            (Named::Enter, keyboard::Modifiers::SHIFT),
            (Named::Tab, keyboard::Modifiers::SHIFT),
            (Named::ArrowDown, keyboard::Modifiers::ALT),
        ] {
            assert!(
                route(named, modifiers, &list(true)).is_none(),
                "{named:?} with {modifiers:?}"
            );
        }
        // Keys the list does not answer to stay with the editor.
        assert!(route(Named::ArrowLeft, none, &list(true)).is_none());
    }

    /// The hint says which named skills reach the agent as words, and why.
    #[test]
    fn the_unloaded_hint_names_the_skills_and_the_rule() {
        assert_eq!(
            unloaded_hint(&["/beta"], chat::SkillReach::Leading).as_deref(),
            Some(
                "/beta goes as plain text: only a skill that starts the message is loaded with it"
            )
        );
        assert_eq!(
            unloaded_hint(&["/skill:b", "/skill:c"], chat::SkillReach::First).as_deref(),
            Some(
                "/skill:b, /skill:c go as plain text: only the first skill a message names is loaded with it"
            )
        );
        assert_eq!(unloaded_hint(&[], chat::SkillReach::Leading), None);
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

    fn scroll(from_top: f32, content: f32, window: f32) -> Scroll {
        Scroll {
            from_top,
            content,
            window,
        }
    }

    /// History arrives by gesture now, so the two ends of the loaded window
    /// are the pagination control: a reader who has reached one has to be
    /// recognised as having reached it, and one in the middle must not be.
    #[test]
    fn reaching_an_end_of_the_transcript_is_what_asks_for_more_of_it() {
        let middle = scroll(500.0, 2_000.0, 400.0);
        assert!(
            !middle.at_top(),
            "the middle of a transcript is not its top"
        );
        assert!(!middle.at_bottom());

        // A wheel notch settles short of an exact end, and that still counts.
        assert!(scroll(PAGE_REACH - 1.0, 2_000.0, 400.0).at_top());
        assert!(scroll(1_600.0 - PAGE_REACH + 1.0, 2_000.0, 400.0).at_bottom());

        // A transcript shorter than its pane cannot be scrolled at all, so
        // both ends are reached: with no gesture available, the page has to
        // arrive without one.
        let short = scroll(0.0, 200.0, 400.0);
        assert!(short.at_top() && short.at_bottom());
    }

    /// A question's own prose and the command or payload it is about read as
    /// two different things, so the surface has to be handed them separately
    /// - but only where the provider drew the line itself.
    #[test]
    fn a_question_is_split_only_where_the_provider_drew_a_boundary() {
        assert_eq!(
            question_and_context(
                "Claude wants to run a command.\n\n{\"cmd\":\"rm -rf .\"}\nIn: /tmp"
            ),
            (
                "Claude wants to run a command.",
                "{\"cmd\":\"rm -rf .\"}\nIn: /tmp"
            )
        );
        assert_eq!(
            question_and_context("Codex wants to apply a patch.\n```diff\n-old\n+new\n```"),
            ("Codex wants to apply a patch.", "-old\n+new")
        );
        // The case this rule exists for: a sentence that ran over two lines
        // is one sentence, and setting its second half as an argv would be a
        // lie about what the provider said.
        assert_eq!(
            question_and_context(
                "Claude wants to run a command in a directory\nit has not been given access to."
            ),
            (
                "Claude wants to run a command in a directory\nit has not been given access to.",
                ""
            )
        );
        assert_eq!(
            question_and_context("Codex is waiting for your answer."),
            ("Codex is waiting for your answer.", "")
        );
        // A description that is nothing but material leaves the request's own
        // title as the whole question.
        assert_eq!(
            question_and_context("```\ngit push --force\n```"),
            ("", "git push --force")
        );
    }

    fn choice(id: &str) -> chat::Choice {
        chat::Choice {
            id: id.into(),
            label: id.into(),
            description: String::new(),
        }
    }

    /// The order the answers are drawn in is the order the keyboard reaches
    /// them, so a permission request has to put refusing first and the widest
    /// grant last - and must not hand Enter to anything that grants.
    #[test]
    fn the_safe_answer_comes_first_and_enter_never_grants() {
        let permission = chat::Request {
            id: "r1".into(),
            title: "Command approval".into(),
            description: String::new(),
            fields: Vec::new(),
            actions: vec![
                choice("acceptForSession"),
                choice("accept"),
                choice("decline"),
            ],
        };
        let mut ordered = permission.actions.clone();
        ordered.sort_by_key(stake);
        assert_eq!(
            ordered
                .iter()
                .map(|choice| choice.id.as_str())
                .collect::<Vec<_>>(),
            vec!["decline", "accept", "acceptForSession"]
        );
        assert_eq!(
            submit_answer(&permission),
            None,
            "a question with something to refuse must not answer to Enter"
        );

        // A question that only asks has an affirmative and nothing to refuse,
        // which is exactly where a keyboard answer belongs.
        let questions = chat::Request {
            actions: vec![choice("submit")],
            ..permission
        };
        assert_eq!(
            submit_answer(&questions).map(|choice| choice.id.as_str()),
            Some("submit")
        );
    }

    fn model(id: &str, label: &str) -> chat::Model {
        chat::Model {
            id: id.into(),
            label: label.into(),
            reasoning: Vec::new(),
        }
    }

    fn listed<'a>(groups: &[(Provider, Vec<ModelRow<'a>>)]) -> Vec<(Provider, Vec<&'a str>)> {
        groups
            .iter()
            .map(|(provider, rows)| {
                (
                    *provider,
                    rows.iter()
                        .map(|row| row.model.map_or("default", |model| model.id.as_str()))
                        .collect(),
                )
            })
            .collect()
    }

    /// A draft can start on a provider's default model as well as a named
    /// one, so the default heads each provider's group; a started
    /// conversation can only switch to a model it names.
    #[test]
    fn a_draft_offers_each_providers_default_ahead_of_its_models() {
        let model = |id: &str| chat::Model {
            id: id.into(),
            label: id.into(),
            reasoning: Vec::new(),
        };
        let codex = [model("gpt-a"), model("gpt-b")];
        let offers = [
            Offer {
                provider: Provider::Codex,
                models: &codex,
            },
            Offer {
                provider: Provider::Claude,
                models: &[],
            },
        ];
        assert_eq!(
            listed(&model_rows(&offers, "", true)),
            [
                (Provider::Codex, vec!["default", "gpt-a", "gpt-b"]),
                (Provider::Claude, vec!["default"]),
            ]
        );
        assert_eq!(
            listed(&model_rows(&offers[..1], "", false)),
            [(Provider::Codex, vec!["gpt-a", "gpt-b"])]
        );
        assert_eq!(
            listed(&model_rows(&offers, "gpt-b", true)),
            [(Provider::Codex, vec!["gpt-b"])]
        );
    }

    /// A provider is named by the start of a word it is known by, not by
    /// any letters its heading happens to contain, and not by one letter.
    #[test]
    fn a_search_names_a_provider_by_the_start_of_its_names() {
        let omp = [chat::Model {
            id: "kimi".into(),
            label: "Kimi".into(),
            reasoning: Vec::new(),
        }];
        let offers = [Offer {
            provider: Provider::Omp,
            models: &omp,
        }];
        for query in ["omp", "Other", "mod"] {
            assert_eq!(
                listed(&model_rows(&offers, query, true)),
                [(Provider::Omp, vec!["default", "kimi"])],
                "{query}"
            );
        }
        for query in ["the", "o", "er"] {
            assert!(model_rows(&offers, query, true).is_empty(), "{query}");
        }
        assert!(names_provider(Provider::Codex, "codex"));
        assert!(names_provider(Provider::Codex, "chat"));
        assert!(!names_provider(Provider::Claude, "laude"));
    }

    #[test]
    fn clicking_a_model_keeps_the_popup_alive_until_selection() {
        use cosmic::iced::advanced::{
            Layout, Shell, clipboard, layout, mouse, renderer::Headless, widget::Tree,
        };
        use cosmic::iced::{Event, Point, Rectangle, Size};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(<cosmic::Renderer as Headless>::new(
                iced::Font::DEFAULT,
                iced::Pixels(13.),
                Some("tiny-skia"),
            ))
            .unwrap();
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let models = vec![model("gpt-selected", "ChatGPT selected")];
        let offers = [Offer {
            provider: Provider::Codex,
            models: &models,
        }];
        let mut element = model_list(target, &offers, false, (Provider::Codex, None), "");
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(300., 400.));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let field = Layout::new(&node)
            .children()
            .next()
            .unwrap()
            .bounds()
            .center();
        let row = Layout::new(&node)
            .children()
            .nth(1)
            .unwrap()
            .bounds()
            .center();
        let mut update = |pressed: bool, point: Point| {
            let mut messages = Vec::new();
            let event = Event::Mouse(if pressed {
                mouse::Event::ButtonPressed(mouse::Button::Left)
            } else {
                mouse::Event::ButtonReleased(mouse::Button::Left)
            });
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            messages
        };
        let _ = update(true, field);
        let _ = update(false, field);
        let pressed = update(true, row);
        assert!(
            !pressed
                .iter()
                .any(|message| matches!(message, Message::ChatUi(Action::ModelPicker(_, false))))
        );
        let released = update(false, row);
        assert!(released.iter().any(|message| matches!(message, Message::ChatUi(Action::Model(selected, model)) if *selected == target && model == "gpt-selected")));
    }

    /// A reader recalls a model by any part of its identifier, including the
    /// provider prefix, and never by its capitalisation.
    #[test]
    fn the_model_search_matches_any_part_of_an_identifier() {
        let models = vec![
            model("anthropic/claude-sonnet-4-5", "Claude Sonnet 4.5"),
            model("openai/gpt-5-codex", "GPT-5 Codex"),
        ];
        let ids = |query: &str| {
            model_hits(&models, query)
                .into_iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("SONNET"), vec!["anthropic/claude-sonnet-4-5"]);
        assert_eq!(ids("openai/"), vec!["openai/gpt-5-codex"]);
        assert_eq!(ids("codex"), vec!["openai/gpt-5-codex"]);
        // Typing what the row shows finds the row.
        assert_eq!(ids("Sonnet 4.5"), vec!["anthropic/claude-sonnet-4-5"]);
        assert_eq!(ids("  ").len(), 2, "an empty query lists every model");
        assert!(ids("haiku").is_empty());
    }
}
