use super::coordination::Coordinated;
use super::*;

pub(super) fn body<'a>(text: impl Into<Cow<'a, str>> + 'a) -> Element<'a, Message> {
    widget::selectable_text(text)
        .size(design::TEXT_BODY)
        .width(Length::Fill)
        .into()
}

fn transcript_body(text: impl Into<String>, target: Target) -> Element<'static, Message> {
    widget::container(selectable_prose::plain(
        text,
        design::TEXT_BODY,
        iced::Font::default(),
        target,
    ))
    .width(Length::Fill)
    .into()
}
/// The composer's own line height and inner padding.
///
/// Named because the composer's height is computed from the draft rather than
/// fixed, so the two constants have to agree with what the editor draws.
const COMPOSER_LINE_HEIGHT: f32 = 18.0;
const COMPOSER_PADDING: f32 = crate::tab_strip::EDGE_INSET + space::S;
// Icon hit targets include their own centered inset; align the visible glyph.
const COMPOSER_TOOLBAR_PADDING: f32 =
    COMPOSER_PADDING - (design::PRIMARY_CONTROL_SIZE - design::ROW_ICON_SIZE as f32) / 2.0;

/// The widest a user's message is drawn.
///
/// Narrower than the measure prose is held to, because a prompt is an aside
/// in the conversation rather than the body of it: keeping it clearly short
/// of the full column is what makes the alternation of asking and answering
/// visible while scrolling.
const PROMPT_MEASURE: f32 = 560.0;

/// How much of a long report or message shows until the reader opens it:
/// its opening paragraph or so, enough to recognize it without reading it. A
/// short one is read in full where it lands.
pub(super) const DELIVERY_FOLDED_HEIGHT: f32 = 180.0;
/// The lines of a notice's event shown before it is opened.
const NOTICE_LINES: usize = 3;

const MESSAGE_ACTION_HEIGHT: f32 = 24.0;
const MESSAGE_ACTION_FOOTER: f32 = MESSAGE_ACTION_HEIGHT + step::GAP_XS;

/// Existing action footers provide separation. Count that space toward the
/// next semantic boundary instead of adding another gap after an invisible row.
struct TranscriptRows<'a> {
    children: Vec<Element<'a, Message>>,
    gap: f32,
    trailing_footer: f32,
}
impl<'a> TranscriptRows<'a> {
    fn new(gap: f32) -> Self {
        Self {
            children: Vec::new(),
            gap,
            trailing_footer: 0.0,
        }
    }
    fn push(&mut self, row: Element<'a, Message>) -> usize {
        self.push_with_gap(row, self.gap, 0.0)
    }
    fn message(&mut self, row: Element<'a, Message>, footer: f32) -> usize {
        self.push_with_gap(row, self.gap, footer)
    }
    fn push_with_gap(&mut self, row: Element<'a, Message>, gap: f32, footer: f32) -> usize {
        if !self.children.is_empty() {
            let gap = (gap - self.trailing_footer).max(0.0);
            if gap > 0.0 {
                self.children.push(widget::Space::new().height(gap).into());
            }
        }
        let index = self.children.len();
        self.children.push(row);
        self.trailing_footer = footer;
        index
    }
}

/// The loaded transcript keeps row descriptions; only its visible exchanges
/// create widget trees. Finished visible exchanges still use retained::kept.
struct VirtualRows<'a> {
    children: Vec<virtualized::Row<'a>>,
    gap: f32,
    trailing_footer: f32,
}
impl<'a> VirtualRows<'a> {
    fn new(gap: f32) -> Self {
        Self {
            children: vec![],
            gap,
            trailing_footer: 0.0,
        }
    }
    fn push(&mut self, row: Element<'a, Message>) -> usize {
        self.message(row, 0.0)
    }
    fn message(&mut self, row: Element<'a, Message>, footer: f32) -> usize {
        self.append(
            virtualized::Row::once(0, 0, 80.0, None, move || row),
            self.gap,
            footer,
        )
    }
    fn lazy(
        &mut self,
        key: u64,
        revision: u64,
        estimate: f32,
        aliases: Vec<iced::advanced::widget::Id>,
        gap: f32,
        footer: f32,
        build: impl Fn() -> Element<'a, Message> + 'a,
    ) -> usize {
        let mut row = virtualized::Row::new(key, revision, estimate, None, build);
        row.aliases = aliases;
        self.append(row, gap, footer)
    }
    fn append(&mut self, mut row: virtualized::Row<'a>, gap: f32, footer: f32) -> usize {
        if !self.children.is_empty() {
            let gap = (gap - self.trailing_footer).max(0.0);
            if gap > 0.0 {
                let key = self.children.len() as u64;
                self.children
                    .push(virtualized::Row::new(key, 0, gap, None, move || {
                        widget::Space::new().height(gap).into()
                    }));
            }
        }
        let index = self.children.len();
        if row.key == 0 {
            row.key = index as u64;
        }
        self.children.push(row);
        self.trailing_footer = footer;
        index
    }
}

/// Reserve an external action row so hovering never shifts the transcript.
/// Iced's hover widget also keeps controls visible while keyboard-focused.
pub(super) fn message_actions<'a>(
    message: Element<'a, Message>,
    actions: Element<'a, Message>,
    alignment: Alignment,
) -> Element<'a, Message> {
    iced::widget::hover(
        widget::column::with_children(vec![
            message,
            widget::Space::new().height(MESSAGE_ACTION_HEIGHT).into(),
        ])
        .spacing(step::GAP_XS),
        widget::container(actions)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(alignment)
            .align_y(Alignment::End),
    )
}

/// How tall the pinned plan is allowed to grow.
///
/// The plan sits above the transcript, so it stays a reference a reader
/// glances at rather than becoming the pane: past this height it scrolls, and
/// a twenty-step plan therefore costs the conversation no more room than a
/// three-step one.
const PLAN_CEILING: f32 = 120.0;

/// Waiting prompts stay within reach without taking over the conversation.
const QUEUE_CEILING: f32 = 180.0;

/// The line a waiting message's preview is set on.
const QUEUE_LINE_HEIGHT: f32 = 18.0;

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
/// whole number of rows before adding the inset that scrolls with them.
const COMPLETION_CEILING: f32 = 7.0 * COMPLETION_ROW;

/// The completion list's inset from its own edge.
const COMPLETION_INSET: f32 = space::XS;

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
fn submit_control(mode: PromptMode, slot: usize, enter_sends: bool) -> (Icon, String) {
    let primary = if enter_sends { "Enter" } else { "Ctrl+Enter" };
    let (name, verb, chord) = match (mode, slot) {
        (PromptMode::Send, _) => (Icon::Send, "Send message", primary),
        (PromptMode::Steer, 0) => (Icon::Send, "Steer the active turn", primary),
        (PromptMode::Steer, _) => (Icon::Send, "Steer the active turn", "Ctrl+Shift+Enter"),
        (PromptMode::Queue, 0) => (Icon::Alarm, "Queue for the next turn", primary),
        (PromptMode::Queue, _) => (Icon::Alarm, "Queue for the next turn", "Ctrl+Shift+Enter"),
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

/// The lines of a waiting message its row shows.
const QUEUE_PREVIEW_LINES: usize = 2;

/// How much of a waiting message is handed to its row to be laid out. Two
/// lines never show more, and a hundred entries of sixteen kibibytes each
/// would otherwise be shaped on every layout to draw two lines apiece.
const QUEUE_PREVIEW_BYTES: usize = 480;

/// When a waiting message goes to the agent, as its row marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueTiming {
    NextStep,
    NextTurn,
    /// On its way to the agent, or back to the composer.
    Leaving,
}

/// One waiting message: when it goes, what it says, and what can be done
/// with it.
fn queue_row<'a>(
    timing: QueueTiming,
    text: &'a str,
    attachments: &'a [Attachment],
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let marker = match timing {
        QueueTiming::NextStep => crate::interaction::tooltip(
            crate::ui_controls::passive_icon(Icon::Send, design::COMPACT_ICON_SIZE),
            "Delivered at the agent's next step",
        ),
        QueueTiming::NextTurn => crate::interaction::tooltip(
            crate::ui_controls::passive_icon(Icon::Alarm, design::COMPACT_ICON_SIZE),
            "Sent when this turn ends",
        ),
        QueueTiming::Leaving => crate::ui_controls::activity_spinner(
            f32::from(design::COMPACT_ICON_SIZE),
            design::accent_color,
        ),
    };
    let names = || {
        attachments
            .iter()
            .map(|attachment| {
                attachment
                    .path
                    .file_name()
                    .unwrap_or(attachment.path.as_os_str())
                    .to_string_lossy()
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    // A message of files alone is named by them rather than left blank.
    let preview: Cow<'a, str> = if text.trim().is_empty() {
        names().into()
    } else {
        let text = text.trim();
        text[..text.floor_char_boundary(QUEUE_PREVIEW_BYTES)].into()
    };
    let mut details: Vec<Element<'a, Message>> = Vec::new();
    if !attachments.is_empty() && !text.trim().is_empty() {
        details.push(crate::interaction::tooltip(
            widget::row::with_children(vec![
                crate::ui_controls::passive_icon(Icon::Attachment, design::COMPACT_ICON_SIZE),
                design::caption(attachments.len().to_string()).into(),
            ])
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center),
            names(),
        ));
    }
    let preview = widget::container(
        design::heading(preview)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .width(Length::Fill)
            .ellipsize(iced::widget::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(QUEUE_PREVIEW_LINES),
            )),
    )
    .width(Length::Fill)
    .max_height(QUEUE_LINE_HEIGHT * QUEUE_PREVIEW_LINES as f32)
    .clip(true);
    let mut row = vec![marker, preview.into()];
    if !details.is_empty() {
        row.push(widget::row::with_children(details).spacing(space::S).into());
    }
    row.push(
        widget::row::with_children(controls)
            .align_y(Alignment::Center)
            .into(),
    );
    widget::row::with_children(row)
        .spacing(step::GAP_S)
        .padding([space::XS, space::XS])
        .align_y(Alignment::Center)
        .into()
}

/// Whether a key press takes the newest waiting message back into the
/// composer.
///
/// Up in an empty composer is the key a reader already reaches for to get a
/// message back, so what is still waiting comes before what was sent. Once
/// there is a draft, bare Up is the caret's, and merging a waiting message
/// into it is not something to do by accident: Alt+Up asks for that.
fn queue_key(press: &text_editor::KeyPress, empty: bool) -> bool {
    matches!(press.status, text_editor::Status::Focused { .. })
        && matches!(
            press.key,
            keyboard::Key::Named(keyboard::key::Named::ArrowUp)
        )
        && !press.modifiers.shift()
        && !press.modifiers.control()
        && !press.modifiers.logo()
        && (press.modifiers.alt() || empty)
}

/// A failure, as an entry of the transcript: what went wrong, set apart the
/// way a system notice is and read in sequence with what it interrupted.
fn visible_notice_body(body: &str, expanded: bool) -> String {
    if !expanded && body.lines().count() > NOTICE_LINES {
        body.lines()
            .take(NOTICE_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        body.to_owned()
    }
}

fn notice_identity(anchor: Option<&str>, message: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::hash::DefaultHasher::new();
    ("transcript-notice", anchor, message).hash(&mut hash);
    hash.finish()
}

fn notice_entry(message: &str, target: Target, identity: u64) -> Element<'static, Message> {
    widget::container(
        widget::row::with_children(vec![
            widget::icon(Icon::Warning.handle())
                .size(design::COMPACT_ICON_SIZE)
                .into(),
            widget::container(selectable_prose::logical_text(
                vec![
                    iced::widget::text::Span::new(message.to_owned())
                        .color(design::warning_color(&cosmic::theme::active())),
                ]
                .into(),
                design::TEXT_CAPTION.into(),
                iced::Font::default(),
                target,
                None,
                selectable_prose::Logical {
                    identity,
                    start: 0,
                    paint: None,
                },
            ))
            .width(Length::Fill)
            .into(),
        ])
        .spacing(step::GAP_S)
        .align_y(Alignment::Start),
    )
    .class(design::surface(
        Some(design::sunken),
        [design::RADIUS_M; 4],
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
/// A row shows a short skill name, says what it does, and says whether it is a
/// command or a skill: a command is the whole
/// message and a skill is part of one, and a reader deciding where to type
/// next needs to know which they just added.
fn completion_row<'a>(
    row: completion::Candidate<'a>,
    highlighted: bool,
    message: Option<Message>,
) -> Element<'a, Message> {
    widget::button::custom(
        widget::row::with_children(vec![
            design::body(row.label())
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
                completion::Kind::File => "File",
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
pub(super) fn completion_extent(count: usize) -> (f32, f32, f32) {
    (
        COMPLETION_ROW,
        (count as f32 * COMPLETION_ROW).min(COMPLETION_CEILING) + 2.0 * COMPLETION_INSET,
        COMPLETION_INSET,
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
    Latest,
}

/// Where the transcript's viewport sits in the conversation it is scrolling.
///
/// The transcript is anchored to its bottom, because a conversation grows at
/// the newest end. The anchoring wrapper measures the actual top offset and
/// reports it even when the loaded page fits entirely inside the viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Scroll {
    from_top: f32,
    content: f32,
    window: f32,
}

impl Scroll {
    pub(super) fn new(from_top: f32, content: f32, window: f32) -> Self {
        Self {
            from_top,
            content,
            window,
        }
    }
    /// How far the transcript can be scrolled.
    fn travel(self) -> f32 {
        (self.content - self.window).max(0.0)
    }

    /// Keep the edge regions separate even when only a few pixels overflow.
    fn page_reach(self) -> f32 {
        PAGE_REACH.min(self.travel() * 0.25)
    }

    /// Whether the reader has reached the oldest message that is loaded.
    ///
    /// A transcript shorter than the pane it is drawn in counts as reached:
    /// no gesture can take the reader any further up it. The explicit history
    /// control remains available without starting a paging loop.
    pub(super) fn at_top(self) -> bool {
        self.travel() <= 0.0 || self.from_top <= self.page_reach()
    }

    /// Whether the reader has reached the newest message that is loaded.
    ///
    /// The newest message a conversation has is not always the newest one
    /// loaded: paging back through history pins the window to an older page,
    /// and reaching the bottom asks for the next cached page toward its live end.
    pub(super) fn at_bottom(self) -> bool {
        self.travel() <= 0.0 || self.from_top >= self.travel() - self.page_reach()
    }

    /// The height of the loaded transcript, which is what tells the page the
    /// reader has already asked for from the one they are asking for now.
    pub(super) fn from_top(self) -> f32 {
        self.from_top
    }

    pub(super) fn content(self) -> f32 {
        self.content
    }
    pub(super) fn from_top(self) -> f32 {
        self.from_top
    }
    pub(super) fn window_height(self) -> f32 {
        self.window
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

/// An affirmative question control is visually primary even when skipping is
/// available. Approval and consent decisions remain explicit secondary controls.
fn primary_answer(request: &chat::Request) -> Option<&chat::Choice> {
    (request.kind == chat::RequestKind::Question)
        .then(|| {
            request
                .actions
                .iter()
                .find(|choice| stake(choice) == Stake::Once)
        })
        .flatten()
}

/// Enter submits a question form, never a permission, consent, or fieldless
/// yes/no confirmation. The latter has no edited answer to submit deliberately.
fn submit_answer(request: &chat::Request) -> Option<&chat::Choice> {
    if request.fields.is_empty() {
        return None;
    }
    primary_answer(request)
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
    fn chat_place(&self, directory: &Path, card: CardId) -> Place {
        Place {
            channel: self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card))
                .map(|channel| {
                    if channel.title.trim().is_empty() {
                        format!(
                            "Channel {}",
                            channel.id.to_string().chars().take(8).collect::<String>()
                        )
                    } else {
                        channel.title.clone()
                    }
                }),
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
                let place = self.chat_place(&runtime.session.launch.recipe.working_directory, card);
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
                let view = runtime.view(
                    self.chat_auth_recovery(runtime),
                    self.chat_start_pending(card),
                    self.settings.chat_enter_sends,
                    &self.settings.keyboard_shortcuts,
                    self.workspace
                        .presentation_geometry()
                        .card(card)
                        .map_or(self.workspace.viewport_width(), |card| card.width())
                        .min(design::READABLE_MEASURE + 2.0 * step::GAP_M),
                    place,
                    !self.workspace_inputs_blocked(),
                    accounts,
                    offers,
                );
                if let Some(recovery) = self.account_recovery_controls(card) {
                    widget::column::with_children(vec![recovery, view]).into()
                } else {
                    view
                }
            }
            _ => widget::row::with_children(vec![
                crate::ui_controls::activity_spinner(
                    f32::from(design::ROW_ICON_SIZE),
                    design::accent_color,
                ),
                design::body("Connecting to native chat…").into(),
            ])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .padding(step::GAP_M)
            .into(),
        }
    }
}

/// Prefer concrete activity from the newest assistant item when available.
fn activity_label(snapshot: &chat::Snapshot) -> &'static str {
    if snapshot.has_later {
        return "Working";
    }
    match snapshot
        .items
        .last()
        .filter(|item| item.role == chat::Role::Assistant)
        .map(|item| &item.content)
    {
        Some(ItemContent::Text { thinking: true, .. }) => "Thinking",
        Some(ItemContent::Text { .. }) => "Responding",
        Some(ItemContent::Command {
            status: chat::ToolStatus::Running,
            ..
        }) => "Running command",
        _ => "Working",
    }
}

/// Thinking stops at a later answer or prompt, not at another reasoning chunk.
fn reasoning_finished(items: &[Arc<chat::Item>], busy: bool, id: &str) -> bool {
    !busy
        || items
            .iter()
            .skip_while(|item| item.id != id)
            .skip(1)
            .any(|item| {
                item.role == chat::Role::User
                    || (item.role == chat::Role::Assistant
                        && matches!(
                            item.content,
                            ItemContent::Text {
                                thinking: false,
                                phase: chat::Phase::Final,
                                ..
                            }
                        ))
            })
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
    channel: Option<String>,
    name: String,
    branch: Option<String>,
}

impl Runtime {
    // Provider descriptions can contain a whole prompt. Keep status controls
    // compact while the full transcript remains available on selection.
    pub(super) fn short_description(raw: &str, limit: usize) -> String {
        let mut chars = raw.chars();
        let text: String = chars.by_ref().take(limit).collect();
        if chars.next().is_some() {
            format!("{text}…")
        } else {
            text
        }
    }
    pub(super) fn native_child_name(agent: &chat::Subagent) -> &str {
        agent
            .description
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .or_else(|| (!agent.kind.trim().is_empty()).then_some(agent.kind.trim()))
            .unwrap_or("Subagent")
    }

    fn native_child_status(agent: &chat::Subagent) -> &'static str {
        match agent.status {
            chat::ToolStatus::Running if agent.waiting => "Waiting for you",
            chat::ToolStatus::Running => "Working",
            chat::ToolStatus::Succeeded => "Finished",
            chat::ToolStatus::Failed => "Failed",
            chat::ToolStatus::Denied => "Denied",
            chat::ToolStatus::Stopped => "Stopped",
        }
    }

    fn native_child_identity(&self, agent: &chat::Subagent) -> String {
        let provider = match self.session.launch.provider {
            Provider::Codex => "Codex",
            Provider::Claude => "Claude",
            Provider::Omp => "OMP",
        };
        let model = agent
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or("Model not reported");
        let mut identity = format!(
            "{provider} native subagent · {} · {} · {}",
            Self::short_description(Self::native_child_name(agent), 80),
            Self::short_description(model, 80),
            Self::native_child_status(agent)
        );
        if !agent.detail.trim().is_empty() {
            identity.push_str(" · ");
            identity.push_str(&Self::short_description(agent.detail.trim(), 120));
        }
        identity
    }

    /// Completion requires provider evidence; an idle, attached conversation
    /// without that evidence is ready, while an unattached one is connecting.
    fn conversation_status(&self, transitioning: bool) -> (Option<Icon>, &str) {
        let snapshot = &self.snapshot;
        if self.queued.is_some() {
            (Some(Icon::AgentWorking), "Starting")
        } else if transitioning {
            (Some(Icon::AgentWorking), "Switching session")
        } else if self.attachment_chooser_pending {
            (Some(Icon::AgentWorking), "Choosing attachments")
        } else if self.pending.is_some() {
            (Some(Icon::AgentWorking), "Sending")
        } else if self.disconnected {
            (Some(Icon::Warning), "Disconnected")
        } else if snapshot.exited && self.session.owns_codex_listener() {
            (Some(Icon::Warning), "Unavailable")
        } else if snapshot.exited {
            (Some(Icon::Warning), "Exited")
        } else if snapshot.waiting || !snapshot.requests.is_empty() {
            (Some(Icon::AgentWaiting), "Waiting for you")
        } else if !snapshot.busy
            && snapshot
                .subagents
                .iter()
                .any(|agent| agent.waiting && agent.status == chat::ToolStatus::Running)
        {
            (Some(Icon::AgentWaiting), "Subagents waiting")
        } else if snapshot.is_working() {
            (
                Some(Icon::AgentWorking),
                if !snapshot.busy {
                    "Subagents working"
                } else if activity_label(snapshot) != "Working" || snapshot.detail.is_empty() {
                    activity_label(snapshot)
                } else {
                    snapshot.detail.as_str()
                },
            )
        } else if snapshot.finished_at.is_some() {
            match snapshot.detail.as_str() {
                "Turn interrupted" => (Some(Icon::Cancel), "Interrupted"),
                "Turn failed" => (Some(Icon::Warning), "Failed"),
                _ if self.error.is_some() || snapshot.error.is_some() => {
                    (Some(Icon::Warning), "Finished with error")
                }
                _ => (Some(Icon::Check), "Finished"),
            }
        } else if self.error.is_some() || snapshot.error.is_some() {
            (Some(Icon::Warning), "Error")
        } else if snapshot.ready {
            (None, "Ready")
        } else {
            (Some(Icon::AgentWorking), "Connecting")
        }
    }

    /// The logical copy/selection catalog is rebuilt only when exposed text
    /// or disclosure changes, never for mouse motion, copying, find or timers.
    fn selection_document(
        &self,
    ) -> (
        Arc<selection::Catalog>,
        std::rc::Rc<std::cell::RefCell<selection::Bookmark>>,
    ) {
        use std::hash::{Hash, Hasher};
        let mut fingerprint = std::hash::DefaultHasher::new();
        self.selected_subagent.hash(&mut fingerprint);
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id));
        let items = selected.map_or(self.snapshot.items.as_slice(), |agent| {
            agent.items.as_slice()
        });
        // Cache preparation advances this epoch only for presented text or
        // ordering edits. Metadata-only heartbeats and hidden-child streams
        // must not rebuild the entire logical catalog.
        self.presentation_revision.hash(&mut fingerprint);
        selected
            .map_or(self.snapshot.busy, |agent| {
                agent.status == chat::ToolStatus::Running
            })
            .hash(&mut fingerprint);
        let mut expanded: Vec<_> = self.expanded.iter().collect();
        expanded.sort_unstable();
        expanded.hash(&mut fingerprint);
        let mut opened: Vec<_> = self.opened.iter().collect();
        opened.sort_unstable();
        opened.hash(&mut fingerprint);
        self.tool_groups_expanded.hash(&mut fingerprint);
        self.failures.hash(&mut fingerprint);
        self.error
            .as_ref()
            .or(self.snapshot.error.as_ref())
            .hash(&mut fingerprint);
        self.queued
            .as_ref()
            .map(|queued| &queued.text)
            .hash(&mut fingerprint);
        let fingerprint = fingerprint.finish();
        let mut document = self.selection_document.borrow_mut();
        if document
            .as_ref()
            .is_none_or(|(revision, _)| *revision != fingerprint)
        {
            let mut builder = selection::Catalog::builder();
            if selected.is_some() {
                for item in items {
                    self.selection_item(&mut builder, item, turn::slot_of(item), true);
                }
            } else {
                for (anchor, error) in self.failures.iter().filter(|(anchor, error)| {
                    anchor.is_none() && !self.active_auth_notice(anchor, error)
                }) {
                    builder.logical(
                        notice_identity(anchor.as_deref(), error),
                        Arc::from(error.as_str()),
                    );
                }
                for turn in &self.turns {
                    for index in &turn.prompt {
                        if let Some(item) = items.get(*index) {
                            self.selection_item(&mut builder, item, turn::Slot::Prose, true);
                        }
                    }
                    if self
                        .opened
                        .get(&turn.key)
                        .copied()
                        .unwrap_or(self.tool_groups_expanded)
                    {
                        for placed in &turn.steps {
                            if let Some(item) = items.get(placed.index) {
                                self.selection_item(&mut builder, item, placed.slot, true);
                            }
                        }
                    }
                    for placed in &turn.prose {
                        if let Some(item) = items.get(placed.index) {
                            self.selection_item(&mut builder, item, placed.slot, true);
                        }
                    }
                    for (anchor, error) in self.failures.iter().filter(|(anchor, error)| {
                        anchor.as_ref() == Some(&turn.key)
                            && !self.active_auth_notice(anchor, error)
                    }) {
                        builder.logical(
                            notice_identity(anchor.as_deref(), error),
                            Arc::from(error.as_str()),
                        );
                    }
                }
                if let Some(queued) = &self.queued {
                    builder.plain("queued-prompt", &[2], Arc::from(queued.text.trim_end()));
                }
            }
            if let Some(error) = self.error.as_ref().or(self.snapshot.error.as_ref())
                && !chat::requires_sign_in(self.session.launch.provider, error)
                && !self.failures.iter().any(|(_, recorded)| recorded == error)
            {
                builder.logical(notice_identity(None, error), Arc::from(error.as_str()));
            }
            let catalog = builder.finish_reusing(document.as_ref().map(|(_, catalog)| catalog.as_ref()));
            *document = Some((fingerprint, catalog));
        }
        let catalog = document.as_ref().unwrap().1.clone();
        let bookmark = self
            .selection_bookmarks
            .borrow_mut()
            .entry(self.selected_subagent.clone())
            .or_insert_with(Default::default)
            .clone();
        (catalog, bookmark)
    }

    fn selection_item(
        &self,
        builder: &mut selection::DocumentBuilder,
        item: &chat::Item,
        slot: turn::Slot,
        detail: bool,
    ) {
        if item.role == chat::Role::User && !matches!(item.content, ItemContent::Delivery { .. }) {
            let (text, _, _) = self.prompt_text(item);
            builder.plain(&item.id, &[2], text.into());
            return;
        }
        let Some(cached) = self.cache.get(&item.id) else {
            return;
        };
        if slot == turn::Slot::Thinking && !self.expanded.contains(&item.id) {
            let selected = self
                .selected_subagent
                .as_ref()
                .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id));
            let (items, busy) = selected.map_or(
                (self.snapshot.items.as_slice(), self.snapshot.busy),
                |agent| {
                    (
                        agent.items.as_slice(),
                        agent.status == chat::ToolStatus::Running,
                    )
                },
            );
            if reasoning_finished(items, busy, &item.id) {
                return;
            }
        }
        if matches!(
            item.content,
            ItemContent::Text { .. } | ItemContent::Delivery { .. }
        ) {
            if let ItemContent::Delivery {
                kind: chat::DeliveryKind::Notice,
                body,
                ..
            } = &item.content
            {
                let body = visible_notice_body(body, self.expanded.contains(&item.id));
                if !body.is_empty() {
                    builder.plain(&item.id, &[2], body.into());
                }
            } else {
                builder.markdown(&item.id, &[0], cached.markdown.items());
            }
        }
        if let Some(coordinated) = &cached.coordination {
            for (index, content) in coordinated.contents().enumerate() {
                builder.markdown(&item.id, &[1, index as u32], content.items());
            }
        }
        let opened = detail && self.expanded.contains(&item.id);
        let plain = |builder: &mut selection::DocumentBuilder, path: &[u32], text: &str| {
            if !text.is_empty() {
                builder.plain(&item.id, path, Arc::from(text));
            }
        };
        match &item.content {
            ItemContent::Command {
                label,
                command,
                working_directory,
                ..
            } => {
                if cached.coordination.is_none() && !label.is_empty() && label != command {
                    plain(builder, &[3, 0], turn::step_label(&item.content));
                }
                if opened {
                    plain(builder, &[3, 1], working_directory);
                }
            }
            ItemContent::Tool { name, .. } if cached.coordination.is_none() => {
                plain(builder, &[3, 0], name)
            }
            ItemContent::WebSearch { query, results } if opened => {
                plain(builder, &[8, 0], query);
                for (index, result) in results.iter().enumerate() {
                    plain(builder, &[8, 1, index as u32, 0], &result.title);
                    plain(builder, &[8, 1, index as u32, 1], &result.url);
                    plain(builder, &[8, 1, index as u32, 2], &result.snippet);
                }
            }
            ItemContent::Image { description, .. } => plain(builder, &[9, 0], description),
            _ => {}
        }
        if opened {
            if let Some(input) = &cached.tool_input {
                builder.plain(&item.id, &[4], input.text.clone());
            }
            if let Some(output) = &cached.tool_output
                && !output.text.is_empty()
            {
                builder.plain(&item.id, &[5], output.text.clone());
            }
            if let ItemContent::Changes { changes, .. } = &item.content {
                for (index, change) in changes.iter().enumerate() {
                    plain(builder, &[7, index as u32, 0], &change.path);
                    if let chat::ChangeKind::Renamed { from } = &change.kind {
                        plain(builder, &[7, index as u32, 1], from);
                    }
                    if let Some(patch) = cached.patches.get(index)
                        && !patch.text.is_empty()
                    {
                        builder.plain(&item.id, &[6, index as u32], patch.text.clone());
                    }
                }
            }
        }
    }

    fn view<'a>(
        &'a self,
        recovery: Option<auth_recovery::Recovery>,
        transitioning: bool,
        enter_sends: bool,
        shortcuts: &crate::settings::KeyboardShortcuts,
        content_width: f32,
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
        let dark = cosmic::theme::is_dark();
        let markdown_settings =
            design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE);
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| snapshot.subagents.iter().find(|agent| &agent.id == id));
        let mut anchors = Vec::new();
        let mut transcript = VirtualRows::new(step::GAP_L);
        let mut selected_identity: Option<Element<'_, Message>> = None;
        if let Some(agent) = selected {
            let provider = match self.session.launch.provider {
                Provider::Codex => "Codex",
                Provider::Claude => "Claude",
                Provider::Omp => "OMP",
            };
            let model = agent
                .model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .unwrap_or("Model not reported");
            let heading: Element<'_, Message> = widget::column::with_children(vec![
                widget::row::with_children(vec![
                    chat_title(Self::short_description(Self::native_child_name(agent), 80))
                        .width(Length::Fill)
                        .into(),
                    design::heading(Self::native_child_status(agent))
                        .size(design::TEXT_CAPTION)
                        .into(),
                ])
                .spacing(step::GAP_S)
                .align_y(Alignment::Center)
                .into(),
                crate::interaction::tooltip(
                    design::caption(format!(
                        "Native subagent · {provider} · {}",
                        Self::short_description(model, 80)
                    )),
                    self.native_child_identity(agent),
                )
                .into(),
            ])
            .spacing(step::GAP_XS)
            .width(Length::Fill)
            .into();
            selected_identity = Some(
                if agent.status == chat::ToolStatus::Running && snapshot.capabilities.stop_subagent
                {
                    widget::row::with_children(vec![
                        heading.into(),
                        crate::ui_controls::button::quiet("Stop")
                            .icon(Icon::Stop)
                            .name(
                                "Stop this subagent; the conversation and other subagents carry on",
                            )
                            .on_press_maybe(available.then(|| {
                                action(Action::Command(
                                    target,
                                    Command::StopSubagent {
                                        id: agent.id.clone(),
                                    },
                                ))
                            }))
                            .into(),
                    ])
                    .spacing(step::GAP_XS)
                    .align_y(Alignment::Center)
                    .into()
                } else {
                    heading.into()
                },
            );
            if agent.items.is_empty() {
                transcript.push(transcript_body(
                    if agent.status == chat::ToolStatus::Running {
                        "Waiting for subagent activity…"
                    } else {
                        "No recent subagent activity is available here."
                    },
                    target,
                ));
            }
            for item in &agent.items {
                let Some(cached) = self.cache.get(&item.id) else {
                    continue;
                };
                let footer = if (item.role == chat::Role::User && !item.content.is_step())
                    || matches!(item.content, ItemContent::Text { .. })
                    || cached
                        .coordination
                        .as_deref()
                        .is_some_and(Coordinated::is_card)
                {
                    MESSAGE_ACTION_FOOTER
                } else {
                    0.0
                };
                use std::hash::{Hash, Hasher};
                let mut key = std::hash::DefaultHasher::new();
                ("selected-child-item", &item.id).hash(&mut key);
                let mut revision = std::hash::DefaultHasher::new();
                (
                    cached.revision,
                    self.expanded.contains(&item.id),
                    self.find
                        .as_ref()
                        .filter(|_| self.find_items.contains(item.id.as_str())),
                    self.find_paint
                        .as_ref()
                        .and_then(|paint| paint.current.as_ref())
                        .filter(|found| found.item.as_ref() == item.id.as_str())
                        .map(|found| (found.identity, found.range.start)),
                    dark,
                )
                    .hash(&mut revision);
                let estimate = CachedItem::prose(&item.content).map_or(80.0, |body| {
                    (body.len() as f32 / (content_width / 8.0).max(20.0))
                        .ceil()
                        .max(1.0)
                        * 22.0
                        + footer
                });
                let row_index = transcript.lazy(
                    key.finish(),
                    revision.finish(),
                    estimate,
                    vec![scroll::item_id(&item.id)],
                    step::GAP_L,
                    footer,
                    move || {
                        scroll::item(
                            &item.id,
                            widget::container(
                                if item.role == chat::Role::User && !item.content.is_step() {
                                    if matches!(item.content, ItemContent::Delivery { .. }) {
                                        self.delivery_view(item, cached, markdown_settings)
                                    } else {
                                        self.prompt_view(item)
                                    }
                                } else if let Some(coordinated) = cached
                                    .coordination
                                    .as_deref()
                                    .filter(|coordinated| coordinated.is_card())
                                {
                                    let raw = self
                                        .expanded
                                        .contains(&item.id)
                                        .then(|| self.step_detail(item, cached, markdown_settings));
                                    self.coordination_card(
                                        item,
                                        cached,
                                        coordinated,
                                        markdown_settings,
                                        raw,
                                    )
                                } else {
                                    match &item.content {
                                        ItemContent::Text { .. } => self.prose_view(
                                            item,
                                            cached,
                                            turn::slot_of(item),
                                            markdown_settings,
                                        ),
                                        ItemContent::Image { .. } => {
                                            self.image_preview(item, cached)
                                        }
                                        _ => self.step_view(item, cached, markdown_settings),
                                    }
                                },
                            )
                            .width(Length::Fill),
                        )
                    },
                );
                let _ = row_index; // Virtual placeholder bounds supply item anchors.
            }
        } else {
            if snapshot.has_earlier {
                transcript.push(paging_view(
                    target,
                    Page::Earlier,
                    self.paging() == Some(Page::Earlier),
                    self.disconnected || self.paging().is_some(),
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
                    .padding(space::XXL)
                    .into(),
                );
            }
            for (_, error) in self.failures.iter().filter(|(anchor, error)| {
                anchor.is_none() && !self.active_auth_notice(anchor, error)
            }) {
                transcript.push(notice_entry(error, target, notice_identity(None, error)));
            }
            for turn in &self.turns {
                use std::hash::{Hash, Hasher};
                let mut key = std::hash::DefaultHasher::new();
                turn.key.hash(&mut key);
                let estimate = turn
                    .prompt
                    .iter()
                    .chain(turn.prose.iter().map(|placed| &placed.index))
                    .filter_map(|index| snapshot.items.get(*index))
                    .map(|item| {
                        CachedItem::prose(&item.content).map_or(60.0, |body| {
                            ((body.len() as f32 / (content_width / 8.0).max(20.0))
                                .ceil()
                                .max(1.0)
                                * 22.0)
                                + 40.0
                        })
                    })
                    .sum::<f32>()
                    + if turn.steps.is_empty() { 0.0 } else { 48.0 };
                let aliases = turn
                    .prompt
                    .iter()
                    .chain(
                        turn.prose
                            .iter()
                            .chain(&turn.steps)
                            .map(|placed| &placed.index),
                    )
                    .filter_map(|index| snapshot.items.get(*index))
                    .map(|item| scroll::item_id(&item.id))
                    .collect();
                transcript.lazy(
                    key.finish(),
                    self.turn_fingerprint(turn, dark),
                    estimate,
                    aliases,
                    if turn.prompt.is_empty() {
                        step::GAP_M
                    } else {
                        step::GAP_L
                    },
                    self.turn_trailing_footer(turn),
                    move || self.turn_view(turn, markdown_settings, dark),
                );
                for (_, error) in self.failures.iter().filter(|(anchor, error)| {
                    anchor.as_ref() == Some(&turn.key) && !self.active_auth_notice(anchor, error)
                }) {
                    transcript.push(notice_entry(
                        error,
                        target,
                        notice_identity(Some(&turn.key), error),
                    ));
                }
            }
            if !snapshot.has_later
                && snapshot.activity() == crate::agent::ActivityState::Idle
                && snapshot.queued_messages.is_empty()
                && self.queued.is_none()
                && self.followups.is_empty()
                && self.pending.is_none()
                && let Some((_, label, full)) = &self.finished_stamp
            {
                transcript.push(
                    widget::tooltip(
                        design::caption(label.clone())
                            .class(cosmic::theme::Text::Custom(design::muted_text)),
                        design::caption(full.clone()),
                        widget::tooltip::Position::Top,
                    )
                    .into(),
                );
            }
            if snapshot.has_later {
                transcript.push(paging_view(
                    target,
                    Page::Newer,
                    self.paging() == Some(Page::Newer),
                    self.disconnected || self.paging().is_some(),
                ));
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
        if self.changes.open {
            content.push(self.changes_view(content_width));
        }
        // A plan is consulted rather than read, so it is pinned above the
        // transcript that scrolls under it. Inside the transcript it went out
        // of reach with the first thing the agent did after publishing it,
        // which is precisely when a reader starts asking what is left.
        // A failure is part of the conversation's record, so it is the last
        // entry in the transcript rather than a line pinned above the
        // composer: it scrolls away with what it was about instead of
        // staying on screen until the next command happens to clear it.
        if let Some(error) = self.error.as_ref().or(snapshot.error.as_ref())
            && !chat::requires_sign_in(self.session.launch.provider, error)
            && !self.failures.iter().any(|(_, recorded)| recorded == error)
        {
            transcript.push(notice_entry(error, target, notice_identity(None, error)));
        }
        content.extend(self.plan_view());
        let find_bar: Element<'_, Message> = if let Some(query) = &self.find {
                widget::row::with_children(vec![
                    crate::interaction::focused_escape(
                        crate::ui_controls::field::search(
                            "Find in conversation",
                            query,
                        )
                        .id(card_control_id(target.card, "chat-find"))
                        .on_input(move |query| action(Action::Find(target, query)))
                        .on_focus(action(Action::FindFocused(target, true)))
                        .on_unfocus(action(Action::FindFocused(target, false)))
                        .width(Length::Fill),
                    move || action(Action::CloseFind(target)),
                ),
                design::caption(if self.find_matches.is_empty() {
                    if self.find_waiting.is_some() {
                        "Searching…".to_owned()
                    } else {
                        "0 matches".to_owned()
                    }
                } else {
                    format!(
                        "{} of {} matches",
                        self.find_index + 1,
                        self.find_matches.len()
                    )
                })
                .into(),
                small_icon(
                    Icon::ChevronUp,
                    "Previous match — Shift+Enter in search",
                    Action::FindNext(target, false),
                ),
                small_icon(
                    Icon::ChevronDown,
                    "Next match — Enter in search",
                    Action::FindNext(target, true),
                ),
                small_icon(
                    Icon::Close,
                    "Close find — Escape",
                    Action::CloseFind(target),
                ),
            ])
            .align_y(Alignment::Center)
            .spacing(step::GAP_S)
            .into()
        } else {
            widget::Space::new().height(0).into()
        };
        let transcript =
            widget::container(virtualized::column(transcript.children, Default::default()))
                .padding(iced::Padding::new(step::GAP_M))
                .width(Length::Fill);
        let transcript_index = content.len();
        let selected_transcript = if self.visible.load(Ordering::Relaxed) {
            let (catalog, bookmark) = self.selection_document();
            crate::ui_memory::gate(selection::transcript_virtualized(
                scroll::transcript_observed(
                    transcript,
                    target,
                    self.selected_subagent.as_deref(),
                    anchors,
                    selected.is_some() || (!snapshot.has_later && self.paging().is_none()),
                    selected
                        .map_or(snapshot.items.as_slice(), |agent| agent.items.as_slice())
                        .iter()
                        .map(|item| (scroll::item_id(&item.id), item.id.as_str()))
                        .chain(self.turns.iter().flat_map(|turn| {
                            let key = turn
                                .steps
                                .first()
                                .and_then(|step| snapshot.items.get(step.index));
                            turn.steps.iter().filter_map(move |step| {
                                Some((
                                    scroll::steps_id(&key?.id),
                                    snapshot.items.get(step.index)?.id.as_str(),
                                ))
                            })
                        }))
                        .collect(),
                    self.observed_scroll.clone(),
                    self.scroll_memory.clone(),
                    self.find.is_some(),
                ),
                target,
                self.selected_subagent.as_deref(),
                catalog,
                bookmark,
            ))
        } else {
            crate::ui_memory::gate(
                widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
        };
        content.push(
            widget::column::with_children(vec![find_bar, selected_transcript])
                .spacing(if self.find.is_some() {
                    step::GAP_S
                } else {
                    0.0
                })
                .height(Length::Fill)
                .into(),
        );
        if selected.is_none() && self.unseen_activity && !self.active_scroll().at_bottom() {
            content.push(
                widget::button::custom(design::caption("↓ New activity"))
                    .class(crate::ui_controls::control_style(true, None))
                    .on_press_maybe((!self.disconnected).then(|| action(Action::Latest(target))))
                    .into(),
            );
        }
        // A draft starting for its first message says so on the message
        // itself.
        if transitioning && self.queued.is_none() {
            content.push(body(
                "Switching or closing this session… Editing and session controls are disabled. You can still read and copy the conversation.",
            ));
        }
        if (self.disconnected || snapshot.exited) && self.auth_error().is_none() {
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
                } else if snapshot.exited && self.session.owns_codex_listener() {
                    "Chat attachment ended. The retained provider may still be running; your draft is retained."
                } else if snapshot.exited {
                    snapshot.read_only_reason.as_deref().unwrap_or(
                        "Agent process exited. The transcript and draft are retained.",
                    )
                } else {
                    "Disconnected. The daemon may still be working; your draft is retained."
                })
                .width(Length::Fill)
                .into(),
                icon(
                    Icon::Refresh,
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
                    Icon::Plus,
                    "Start a new conversation beside it",
                    enabled,
                    Action::StartFresh(target.card),
                ));
            }
            content.push(
                widget::row::with_children(controls)
                    .spacing(space::S)
                    .align_y(Alignment::Center)
                    .into(),
            );
        }
        if let Some((url, instructions)) = &snapshot.open_url {
            content.push(
                widget::column::with_children(vec![
                    body(instructions.as_str()),
                    widget::button::custom(design::body("Open authorization page"))
                        .class(crate::ui_controls::control_style(false, None))
                        .on_press(action(Action::OpenUrl(url.clone())))
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .into(),
            );
        }
        if self.auth_error().is_some() {
            content.push(auth_recovery::view(self, recovery, !transitioning));
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
                                    request_view(
                                        self.target,
                                        request,
                                        &self.answers,
                                        available,
                                        RequestSurface::Transcript,
                                    )
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
            let mut tabs = vec![crate::ui_controls::view_tab(
                crate::ui_controls::ViewTab {
                    label: "Conversation".into(),
                    description: "Main conversation".into(),
                    count: None,
                    selected: selected.is_none(),
                    id: None,
                    on_press: Some(action(Action::SelectSubagent(target, None))),
                },
                true,
                None,
            )];
            for agent in snapshot.subagents.iter().rev() {
                let label = Self::native_child_name(agent);
                let marker = if agent.status == chat::ToolStatus::Running && agent.waiting {
                    crate::ui_controls::passive_icon(Icon::AgentWaiting, design::COMPACT_ICON_SIZE)
                } else if agent.status == chat::ToolStatus::Running {
                    crate::ui_controls::activity_spinner(
                        f32::from(design::COMPACT_ICON_SIZE),
                        design::accent_color,
                    )
                } else {
                    crate::ui_controls::passive_icon(
                        match agent.status {
                            chat::ToolStatus::Succeeded => Icon::Check,
                            chat::ToolStatus::Failed => Icon::Warning,
                            _ => Icon::Cancel,
                        },
                        design::COMPACT_ICON_SIZE,
                    )
                };
                tabs.push(crate::ui_controls::view_tab(
                    crate::ui_controls::ViewTab {
                        label: format!("Native · {}", Self::short_description(label, 48)).into(),
                        description: self.native_child_identity(agent).into(),
                        count: None,
                        selected: selected.is_some_and(|active| active.id == agent.id),
                        id: None,
                        on_press: Some(action(Action::SelectSubagent(
                            target,
                            Some(agent.id.clone()),
                        ))),
                    },
                    true,
                    Some(marker),
                ));
            }
            let height = crate::ui_controls::view_tab_height(true);
            content.push(crate::tab_strip::horizontal(
                widget::Id::new(format!("chat.subagents.{}", target.card)),
                tabs,
                height,
                height + design::BORDER_WIDTH,
            ));
        }
        if let Some(identity) = selected_identity {
            content.push(
                widget::container(identity)
                    .padding([space::XS, space::M])
                    .into(),
            );
        }
        // The draft is copied out only where its text is shown or searched:
        // `text()` allocates the whole draft, and this runs on every frame.
        if let Some(provided) = &snapshot.composer
            && !provided.trim().is_empty()
            && !self.composer.text().contains(provided.as_str())
        {
            content.push(
                widget::button::custom(design::body("Append provider's suggested draft"))
                    .class(crate::ui_controls::control_style(false, None))
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
            let (_, viewport, _) = completion_extent(rows.len());
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
                    widget::scrollable(widget::column::with_children(rows).padding(
                        design::with_vertical_rail(iced::Padding::new(COMPLETION_INSET)),
                    ))
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
        let unloaded = if reach == chat::SkillReach::Anywhere || snapshot.skills.is_empty() {
            Vec::new()
        } else {
            completion::unloaded(&self.composer.text(), &snapshot.skills, reach)
        };
        if let Some(hint) = unloaded_hint(&unloaded, reach) {
            content.push(
                design::caption(hint)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
        }
        let mut attachment_chips = Vec::new();
        for (index, attachment) in self.attachments.iter().enumerate() {
            let chip = widget::row::with_children(vec![
                if let Some(handle) = self.thumbnails.handle(&attachment.path) {
                    iced::widget::image(handle.clone())
                        .width(design::DENSE_ROW_HEIGHT)
                        .height(design::DENSE_ROW_HEIGHT)
                        .content_fit(iced::ContentFit::Cover)
                        .into()
                } else {
                    crate::ui_controls::passive_icon(Icon::Attachment, design::ROW_ICON_SIZE)
                },
                crate::design::caption(if clipboard_image::is_pasted(&attachment.path) {
                    "Pasted image".to_owned()
                } else {
                    attachment
                        .path
                        .file_name()
                        .unwrap_or(attachment.path.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                })
                .width(Length::Fill)
                .into(),
                icon(
                    Icon::Delete,
                    "Remove attachment",
                    !transitioning,
                    Action::RemoveAttachment(target, index),
                ),
            ])
            .align_y(Alignment::Center)
            .spacing(step::GAP_XS);
            attachment_chips.push(
                widget::tooltip(
                    chip,
                    design::caption(attachment.path.display().to_string()),
                    widget::tooltip::Position::Top,
                )
                .into(),
            );
        }
        if self.pastes_pending > 0 {
            content.push(
                design::caption("Pasting…")
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
        }
        // Keep the named composer in a fixed sibling slot as provider controls,
        // requests, and attachments arrive. The pinned Iced child reconciler
        // finds named siblings but does not move their trees to new indices.
        let mut content = vec![
            widget::column::with_children(content.into_iter().enumerate().map(|(index, child)| {
                if index == transcript_index {
                    child
                } else {
                    widget::container(child)
                        .padding(iced::Padding::from([0.0, step::GAP_M]).top(if index == 0 {
                            step::GAP_M
                        } else {
                            0.0
                        }))
                        .width(Length::Fill)
                        .into()
                }
            }))
            .spacing(space::S)
            .height(Length::Fill)
            .into(),
        ];
        let has_text = self
            .composer
            .lines()
            .any(|line| !line.text.trim().is_empty());
        let has_draft = has_text || !self.attachments.is_empty();
        // The submit modes the send controls expose, in the order the keyboard
        // reaches them: the first is Ctrl+Enter, the second Ctrl+Shift+Enter.
        let modes: &[PromptMode] = if snapshot.busy {
            match snapshot.capabilities.steer {
                // A message taken back from the queue goes back the way it
                // was queued unless the other chord says otherwise.
                true if self.resend == Some(PromptMode::Queue) => {
                    &[PromptMode::Queue, PromptMode::Steer]
                }
                true => &[PromptMode::Steer, PromptMode::Queue],
                false => &[PromptMode::Queue],
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
        let placeholder = if selected.is_some() {
            "Message main agent"
        } else {
            match modes.first() {
                Some(PromptMode::Send) => "Message",
                Some(PromptMode::Steer) => "Steer this turn",
                Some(PromptMode::Queue) => "Queue for the next turn",
                None => "Message",
            }
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
        let suggestion = if available && !transitioning {
            self.prompt_suggestion()
        } else {
            None
        };
        let key_suggestion = suggestion.cloned();
        let placeholder = suggestion.map_or(placeholder, |suggestion| suggestion.text.as_str());
        let suggestion_hint: Element<'_, Message> = if suggestion.is_some() {
            widget::text("Tab to use suggestion")
                .size(design::TEXT_CAPTION)
                .into()
        } else {
            iced::widget::Space::new().height(0).into()
        };
        let editor: Element<'_, Message> = if transitioning {
            // The reservation belongs inside the scroll viewport. An inset on
            // the outer container moves the viewport itself, and the thumb
            // floats at that viewport's right edge either way, so the draft
            // text would still run underneath it.
            widget::container(
                widget::scrollable(widget::container(body(self.composer.text())).padding(
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
            // Up on a draft's first line starts browsing sent messages.
            // While browsing, both arrows walk whole history entries so a
            // multiline recalled prompt cannot trap the original draft.
            let (recall_older, recall_newer) = self.history_navigation();
            // What is still waiting comes back before what was already sent,
            // but a walk through sent messages that has begun keeps the keys.
            // A press while one is already on its way back is still about
            // the queue, and is not left to fall through to that walk.
            let take_back =
                !recall_newer && (self.newest_take_back().is_some() || !self.recalling.is_empty());
            crate::ui_controls::field::editor("Message", &self.composer)
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
                .on_cursor(move |cursor| action(Action::ComposerCursor(target, cursor)))
                .key_binding(move |press| {
                    if press.modifiers.command()
                        && matches!(press.key, keyboard::Key::Named(keyboard::key::Named::End))
                    {
                        return Some(text_editor::Binding::Custom(action(Action::Latest(target))));
                    }
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
                    if (self.composer_actions || self.branch_picker)
                        && matches!(completion_key(&press), Some(CompletionKey::Dismiss))
                    {
                        return Some(text_editor::Binding::Custom(action(
                            Action::ComposerActions(target, false),
                        )));
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
                    if let Some(suggestion) = &key_suggestion
                        && matches!(completion_key(&press), Some(CompletionKey::Choose))
                    {
                        return Some(text_editor::Binding::Custom(action(
                            Action::AcceptSuggestion(target, suggestion.clone()),
                        )));
                    }
                    if snapshot.busy
                        && available
                        && self.model_picker.is_none()
                        && !self.reasoning_picker
                        && !self.account_picker
                        && !self.permission_picker
                        && matches!(completion_key(&press), Some(CompletionKey::Dismiss))
                    {
                        return Some(text_editor::Binding::Custom(action(Action::Interrupt(
                            target,
                        ))));
                    }
                    if let Some(mode) =
                        submit_chord(&press, key_submit.0, key_submit.1, enter_sends)
                    {
                        return Some(text_editor::Binding::Custom(action(Action::Submit(
                            target, mode,
                        ))));
                    }
                    if take_back && queue_key(&press, !has_text) {
                        return Some(text_editor::Binding::Custom(action(
                            Action::TakeBackQueued(target, None),
                        )));
                    }
                    if let Some(older) = history_key(&press, recall_older, recall_newer) {
                        return Some(text_editor::Binding::Custom(action(if older {
                            Action::HistoryOlder(target)
                        } else {
                            Action::HistoryNewer(target)
                        })));
                    }
                    let binding =
                        word_delete(&press).or_else(|| text_editor::Binding::from_key_press(press));
                    if matches!(binding, Some(text_editor::Binding::Paste)) {
                        Some(text_editor::Binding::Custom(action(Action::Paste(target))))
                    } else {
                        binding
                    }
                })
                .into()
        };
        let mut actions = Vec::new();
        if snapshot.capabilities.branch || snapshot.capabilities.compact {
            actions.push(self.composer_actions(available));
        }
        if !snapshot.capabilities.interrupt_modes.is_empty() {
            let mode = snapshot.interrupt_mode.as_deref().unwrap_or("immediate");
            let next = if mode == "wait" { "immediate" } else { "wait" };
            actions.push(
                crate::ui_controls::icon_button(
                    Icon::Jump,
                    if mode == "wait" {
                        "Steering: after turn"
                    } else {
                        "Steering: after tools"
                    },
                )
                .on_press_maybe(available.then(|| {
                    action(Action::Command(
                        target,
                        Command::SetInterruptMode { mode: next.into() },
                    ))
                }))
                .into(),
            );
        }
        if snapshot.capabilities.attachments {
            actions.push(icon(
                Icon::Attachment,
                "Attach files",
                available && !self.attachment_chooser_pending,
                Action::Attach(target),
            ));
        }
        // Commands are reached by typing `/`, which is how every agent surface
        // a user arrives from works and what the composer already answers to.
        // A dropdown listing them duplicated that path in a control that was
        // permanently present for something used occasionally.
        let mut submissions = Vec::new();
        if snapshot.busy {
            // A provider whose interrupt does not spare background subagents
            // takes them down with the turn. Saying so on the control is not
            // enough for Esc, which is pressed without reading it, so such a
            // cancel waits for a second press that names what it stops.
            let at_stake = self.interrupt_casualties();
            let subagents = if at_stake == 1 {
                "subagent"
            } else {
                "subagents"
            };
            submissions.push(if self.interrupt_armed.is_some() && at_stake > 0 {
                crate::ui_controls::button::destructive(format!(
                    "Stop turn and {at_stake} {subagents}"
                ))
                .icon(Icon::Stop)
                .name(format!(
                    "Confirm: cancel the active turn and stop {at_stake} background {subagents} (Esc)"
                ))
                .on_press_maybe(available.then(|| action(Action::Interrupt(target))))
                .into()
            } else {
                icon(
                    Icon::Stop,
                    if at_stake > 0 {
                        Cow::Owned(format!(
                            "Cancel active turn (Esc). This also stops {at_stake} background {subagents}"
                        ))
                    } else {
                        Cow::Borrowed("Cancel active turn (Esc)")
                    },
                    available,
                    Action::Interrupt(target),
                )
            });
        }
        // The primary action anchors the trailing edge. Tooltips and
        // accessible names carry the delivery mode and keyboard chord.
        for (slot, mode) in modes.iter().enumerate().rev() {
            let (name, tooltip) = submit_control(*mode, slot, enter_sends);
            submissions.push(
                crate::ui_controls::icon_button(name, tooltip)
                    .selected(slot == 0)
                    .tone(crate::design::Tone::Themed(if slot == 0 {
                        design::accent_color
                    } else {
                        design::muted_icon_color
                    }))
                    .on_press_maybe(
                        (available && has_draft).then(|| action(Action::Submit(target, *mode))),
                    )
                    .into(),
            );
        }
        // Very narrow panes keep every icon's hit target intact.
        let toolbar: Element<'_, Message> = if content_width < 360.0 {
            widget::column::with_children(vec![
                widget::flex_row(actions)
                    .spacing(step::GAP_XS as u16)
                    .align_items(Alignment::Center)
                    .into(),
                widget::container(
                    widget::row::with_children(submissions)
                        .spacing(step::GAP_XS)
                        .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .align_x(Alignment::End)
                .into(),
            ])
            .spacing(space::XS)
            .into()
        } else {
            actions.push(iced::widget::Space::new().width(Length::Fill).into());
            actions.extend(submissions);
            widget::row::with_children(actions)
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into()
        };
        // The editor and its controls are one object: a single raised surface
        // with the controls tucked inside its lower edge, so the composer
        // reads as one place to type rather than a field with a toolbar
        // bolted under it.
        content.push(
            widget::container(
                widget::column::with_children(vec![
                    widget::column::with_children(self.queue_view().into_iter().chain(
                        (!attachment_chips.is_empty()).then(|| {
                            widget::column::with_children(attachment_chips)
                                .spacing(step::GAP_XS)
                                .padding([0, step::GAP_M as u16])
                                .into()
                        }),
                    ))
                    .spacing(step::GAP_XS)
                    .into(),
                    editor,
                    suggestion_hint,
                    widget::container(toolbar)
                        .padding(
                            iced::Padding::from([0.0, COMPOSER_TOOLBAR_PADDING]).bottom(space::S),
                        )
                        .into(),
                ])
                .width(Length::Fill),
            )
            .class(design::surface(
                Some(if self.drop_hover {
                    design::accent_wash
                } else {
                    design::raised
                }),
                [design::RADIUS_M; 4],
                true,
            ))
            .width(Length::Fill)
            .into(),
        );
        let usage = &snapshot.usage;
        // Prefer a wider chat over hiding its status and settings behind a
        // scrollbar. The footer wraps only when the pane itself is too narrow.
        // The line always says what the conversation is doing, idle
        // included: a reader glancing at a card should never have to infer
        // its state from the absence of a word. The order is the order of
        // what a reader most needs to know first - that the card is between
        // sessions, that something is being sent, that the agent has stopped
        // or failed, that it is waiting on them, that it is working.
        let mut status: Vec<Element<'_, Message>> = Vec::new();
        let (glyph, state) = self.conversation_status(transitioning);
        if let Some(glyph) = glyph {
            status.push(if glyph == Icon::AgentWorking {
                crate::ui_controls::activity_spinner(
                    f32::from(design::ROW_ICON_SIZE),
                    design::accent_color,
                )
            } else {
                crate::ui_controls::passive_icon(glyph, design::COMPACT_ICON_SIZE)
            });
        }
        if !state.is_empty() {
            status.push(
                design::heading(if selected.is_some() {
                    format!("Main: {state}")
                } else {
                    state.to_owned()
                })
                .size(design::TEXT_CAPTION)
                .class(
                    if glyph == Some(Icon::Warning) && !transitioning && self.pending.is_none() {
                        cosmic::theme::Text::Custom(design::warning_text)
                    } else {
                        cosmic::theme::Text::default()
                    },
                )
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            );
        }
        if let Some(channel) = &place.channel {
            status.push(
                crate::interaction::tooltip(
                    design::caption(format!("# {}", Self::short_description(channel, 24)))
                        .wrapping(iced::widget::text::Wrapping::None),
                    format!("Channel: {channel}"),
                )
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
                    crate::ui_controls::passive_icon(crate::sidebar::worktree_icon(), 12),
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
        status.extend(self.session_controls(transitioning, accounts, offers, shortcuts));
        // Keep budget and cost together when the footer wraps.
        let mut spend: Vec<Element<'_, Message>> = Vec::new();
        if usage.context_limit > 0 {
            // A context budget is a quantity a reader acts on - compact now,
            // or lose the thread - so it is drawn as a ring with its figure
            // beside it. The linear capsule this replaces spent the width of a
            // whole caption saying what the percentage already said.
            let used = (usage.context_tokens as f32 / usage.context_limit as f32).clamp(0.0, 1.0);
            spend.push(crate::interaction::tooltip(
                widget::row::with_children(vec![
                    crate::ui_controls::budget_radial(used),
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
        } else if let Some(estimate) = usage.cost_estimate {
            let (label, tooltip) = cost_estimate_label(estimate);
            spend.push(crate::interaction::tooltip(design::caption(label), tooltip));
        }
        let mut footer = status;
        if let Some(notice) = permissions::notice(snapshot) {
            footer.push(
                widget::button::custom(
                    widget::row::with_children(vec![
                        crate::ui_controls::passive_icon(Icon::Warning, design::COMPACT_ICON_SIZE),
                        design::caption(notice)
                            .class(cosmic::theme::Text::Custom(design::warning_text))
                            .wrapping(iced::widget::text::Wrapping::None)
                            .into(),
                    ])
                    .spacing(step::GAP_XS)
                    .align_y(Alignment::Center),
                )
                .name("Resolve blocked permissions")
                .class(crate::ui_controls::control_style(
                    self.permission_picker,
                    None,
                ))
                .padding(CONTROL_PADDING)
                .on_press(action(Action::PermissionPicker(
                    target,
                    !self.permission_picker,
                )))
                .into(),
            );
        }
        footer.push(
            widget::row::with_children(spend)
                .spacing(step::GAP_S)
                .align_y(Alignment::Center)
                .into(),
        );
        let footer = crate::ui_controls::popover(
            widget::flex_row(footer)
                .spacing(step::GAP_S as u16)
                .align_items(Alignment::Center),
            self.permission_picker
                .then(|| self.permissions_view(available, content_width - 32.0)),
            widget::popover::Position::Top,
            action(Action::PermissionPicker(target, false)),
        );
        let pane = widget::container(super::layout::pane(
            widget::column::with_children(content.into_iter().enumerate().map(|(index, child)| {
                if index == 0 {
                    child
                } else {
                    widget::container(child)
                        .padding([0.0, step::GAP_M])
                        .width(Length::Fill)
                        .into()
                }
            }))
            .spacing(step::GAP_S)
            .height(Length::Fill)
            .width(Length::Fill)
            .into(),
            widget::container(footer)
                .padding(
                    iced::Padding::from([0.0, step::GAP_M + COMPOSER_PADDING]).bottom(step::GAP_M),
                )
                .into(),
        ))
        .width(Length::Fill)
        .height(Length::Fill);
        let pane = widget::container(pane)
            .center_x(Length::Fill)
            .height(Length::Fill);
        // The whole pane takes a drop, not just the composer: the transcript
        // is most of what a reader aims at, and a drop lands in the draft
        // wherever it was aimed.
        crate::file_drop::target_with_hover(
            pane,
            ("chat", target.card),
            accepting_drops && !transitioning,
            move |payload| action(Action::Dropped(target, payload)),
            move |hovering| action(Action::DropHover(target, hovering)),
        )
    }

    /// The page of history this conversation is waiting on, if any.
    fn paging(&self) -> Option<Page> {
        if self.disconnected {
            return None;
        }
        self.pending
            .as_ref()
            .and_then(|pending| pending.page)
            .or(self.page_request.map(|(page, _)| page))
            .or(self.snapshot.history_loading.then_some(Page::Earlier))
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
                chat::PlanState::Active => crate::ui_controls::activity_spinner(
                    f32::from(design::COMPACT_ICON_SIZE),
                    design::accent_color,
                ),
                chat::PlanState::Done => widget::icon(Icon::Check.handle())
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
                chat::PlanState::Pending => widget::icon(Icon::Minus.handle())
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
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

    fn composer_actions(&self, available: bool) -> Element<'_, Message> {
        let target = self.target;
        let snapshot = &self.snapshot;
        let open = self.composer_actions || self.branch_picker;
        let enabled =
            available && snapshot.ready && !snapshot.busy && snapshot.read_only_reason.is_none();
        let anchor = crate::ui_controls::icon_button(Icon::Plus, "Conversation actions")
            .id(card_control_id(target.card, "chat-actions"))
            .selected(open)
            .on_press(action(Action::ComposerActions(target, !open)));
        let popup = open.then(|| {
            let content: Element<'_, Message> = if self.branch_picker {
                let mut choices: Vec<Element<'_, Message>> = Vec::new();
                let codex = self.session.launch.provider == Provider::Codex;
                choices.push(design::heading("Fork chat").into());
                choices.push(
                    design::caption(if codex {
                        "Start a new chat after a completed turn, or copy the whole conversation."
                    } else {
                        "Start a new chat before a prompt, then edit or resend its draft."
                    })
                    .into(),
                );
                if codex {
                    choices.push(
                        crate::ui_controls::button::secondary("Fork entire conversation")
                            .width(Length::Fill)
                            .on_press_maybe(enabled.then(|| {
                                action(Action::Command(target, Command::Branch { id: None }))
                            }))
                            .into(),
                    );
                }
                for point in &snapshot.branch_points {
                    choices.push(crate::interaction::tooltip(
                        crate::ui_controls::button::secondary(Self::short_description(
                            &point.label,
                            36,
                        ))
                        .name(point.label.as_str())
                        .width(Length::Fill)
                        .on_press_maybe(enabled.then(|| {
                            action(Action::Command(
                                target,
                                Command::Branch {
                                    id: Some(point.id.clone()),
                                },
                            ))
                        })),
                        point.label.as_str(),
                    ));
                }
                choices.push(
                    crate::ui_controls::button::secondary("Back")
                        .on_press(action(Action::BranchPicker(target, false)))
                        .into(),
                );
                widget::column::with_children(choices)
                    .spacing(step::GAP_S)
                    .into()
            } else {
                let mut choices: Vec<Element<'_, Message>> = Vec::new();
                if snapshot.capabilities.branch {
                    choices.push(
                        crate::ui_controls::button::secondary("Fork conversation…")
                            .icon(Icon::Branch)
                            .id(card_control_id(target.card, "chat-actions-fork"))
                            .width(Length::Fill)
                            .on_press_maybe(
                                enabled.then(|| action(Action::BranchPicker(target, true))),
                            )
                            .into(),
                    );
                }
                if snapshot.capabilities.compact {
                    choices.push(
                        crate::ui_controls::button::secondary("Compact conversation")
                            .icon(Icon::Compress)
                            .id(card_control_id(target.card, "chat-actions-compact"))
                            .name(
                                "Compact the conversation: summarize earlier turns to free context",
                            )
                            .width(Length::Fill)
                            .on_press_maybe(
                                enabled.then(|| action(Action::Command(target, Command::Compact))),
                            )
                            .into(),
                    );
                }
                widget::column::with_children(choices)
                    .spacing(step::GAP_S)
                    .into()
            };
            widget::scrollable(content).height(Length::Shrink).into()
        });
        crate::ui_controls::popover(
            anchor,
            popup.map(|content: Element<'_, Message>| {
                widget::container(content)
                    .width(320)
                    .max_height(260.0)
                    .into()
            }),
            widget::popover::Position::Top,
            action(Action::ComposerActions(target, false)),
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
        shortcuts: &crate::settings::KeyboardShortcuts,
    ) -> Vec<Element<'a, Message>> {
        let target = self.target;
        let snapshot = &self.snapshot;
        let mut controls: Vec<Element<'a, Message>> = Vec::new();
        if let Some(accounts) = accounts {
            controls.push(self.account_picker(accounts, transitioning, shortcuts));
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
                controls.push(self.model_picker(label, offers, shortcuts));
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
                .position(|mode| Some(mode) == snapshot.permission_mode.as_ref());
            if transitioning || (self.session.launch.provider == Provider::Codex && snapshot.busy) {
                controls.push(
                    design::caption(
                        selected
                            .map_or("Permissions", |index| permission_mode_label(&modes[index])),
                    )
                    .into(),
                );
            } else {
                let open = self.permission_picker;
                let anchor =
                    widget::button::custom(
                        widget::row::with_children(vec![
                            design::caption(selected.map_or("Permissions", |index| {
                                permission_mode_label(&modes[index])
                            }))
                            .into(),
                            widget::icon(Icon::PanDown.handle())
                                .size(design::COMPACT_ICON_SIZE)
                                .into(),
                        ])
                        .spacing(step::GAP_XS)
                        .align_y(Alignment::Center),
                    )
                    .name("Choose permission mode")
                    .class(crate::ui_controls::control_style(open, None))
                    .padding(CONTROL_PADDING)
                    .on_press(action(Action::PermissionPicker(target, !open)));
                controls.push(crate::interaction::tooltip(
                    anchor,
                    crate::workspace_widget::shortcut_tooltip(
                        crate::workspace_widget::GlobalShortcut::ChatPermissions,
                        shortcuts,
                    ),
                ));
            }
        }
        controls
    }

    /// A draft can explicitly pin its account. Started conversations display
    /// their current account; automatic routing may hand off between turns.
    fn account_picker<'a>(
        &'a self,
        accounts: ChatAccounts,
        transitioning: bool,
        shortcuts: &crate::settings::KeyboardShortcuts,
    ) -> Element<'a, Message> {
        let target = self.target;
        let open = self.account_picker && !transitioning;
        let label = design::caption(accounts.current.clone())
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ));
        let icon = Icon::Avatar.handle();
        if transitioning || !self.choosing() {
            // Manual choices are made before the first message. Automatic
            // routing can hand off later at a safe idle boundary.
            return crate::interaction::tooltip(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(icon, design::COMPACT_ICON_SIZE),
                    label.into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center),
                format!(
                    "This conversation runs on {}. {}",
                    accounts.current,
                    if self
                        .session
                        .launch
                        .recipe
                        .environment
                        .get(account::routing::AUTOMATIC_MARKER)
                        .is_some_and(|value| value == "1")
                    {
                        "Automatic can switch to another signed-in account between turns when needed."
                    } else {
                        "This account is pinned. Start a new conversation to use another account."
                    }
                ),
            );
        }
        let anchor = widget::button::custom(
            widget::row::with_children(vec![
                widget::icon(icon).size(design::COMPACT_ICON_SIZE).into(),
                label.into(),
                widget::icon(Icon::PanDown.handle())
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
        let anchor = crate::interaction::tooltip(
            anchor,
            crate::workspace_widget::shortcut_tooltip(
                crate::workspace_widget::GlobalShortcut::ChatAccount,
                shortcuts,
            ),
        );
        let choices = open.then(|| {
            widget::column::with_children(
                accounts
                    .choices
                    .into_iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        let mut row: Vec<Element<'a, Message>> = Vec::new();
                        match choice.used {
                            Some(used) => row.push(crate::ui_controls::budget_radial(used)),
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
                        .id(card_control_id(
                            target.card,
                            &format!("chat-account-choice-{index}"),
                        ))
                        .selected(choice.selected)
                        .class(crate::ui_controls::control_style(choice.selected, None))
                        .padding([step::GAP_XS, step::GAP_S])
                        .width(Length::Fill)
                        .on_press(action(Action::ChooseAccount(target, choice.id)))
                        .into()
                    })
                    .chain(std::iter::once(
                        widget::button::custom(design::caption("Manage accounts…"))
                            .id(card_control_id(target.card, "chat-manage-accounts"))
                            .class(crate::ui_controls::control_style(false, None))
                            .padding([step::GAP_XS, step::GAP_S])
                            .width(Length::Fill)
                            .on_press(Message::AccountUi(crate::app::account_ui::Action::OpenPane))
                            .into(),
                    ))
                    .collect::<Vec<_>>(),
            )
            .spacing(space::XXS)
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
        shortcuts: &crate::settings::KeyboardShortcuts,
    ) -> Element<'a, Message> {
        let target = self.target;
        let open = self.model_picker.is_some();
        let label = if self.snapshot.fast_mode.as_ref().is_some_and(|mode| mode.enabled()) {
            Cow::Owned(format!("{label} · Fast"))
        } else {
            label
        };
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
                widget::icon(Icon::PanDown.handle())
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
        let anchor = crate::interaction::tooltip(
            anchor,
            crate::workspace_widget::shortcut_tooltip(
                crate::workspace_widget::GlobalShortcut::ChatModel,
                shortcuts,
            ),
        );
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
        let models = model_list(
            self.target,
            &offers,
            draft,
            (provider, self.snapshot.selected_model.as_deref()),
            query,
        );
        let Some(mode) = &self.snapshot.fast_mode else {
            return Some(models);
        };
        let available = !self.disconnected
            && !self.snapshot.exited
            && self.pending.is_none()
            && self.queued.is_none()
            && self.snapshot.ready
            && self.snapshot.capabilities.fast_mode
            && self.snapshot.read_only_reason.is_none()
            && mode.changeable();
        let label = match mode.state {
            crate::chat::FastModeState::Off => "Fast mode: off",
            crate::chat::FastModeState::On => "Fast mode: on",
            crate::chat::FastModeState::Cooldown => "Fast mode: cooldown",
        };
        let control = crate::interaction::tooltip(
            crate::ui_controls::button::quiet(label).on_press_maybe(available.then(|| {
                action(Action::Command(
                    self.target,
                    Command::SetFastMode { enabled: !mode.enabled() },
                ))
            })),
            fast_mode_tooltip(mode),
        );
        Some(widget::column::with_children(vec![models, control])
            .spacing(step::GAP_XS)
            .into())
    }

    /// One exchange: what was asked, what was done, and what came back.
    ///
    /// An exchange is kept between frames, and built and laid out again only
    /// when something it draws changes, which is what stops a streaming
    /// answer from paying for every exchange above it twenty times a second.
    /// Open details own their text and share the same retained path. A
    /// disclosure or changed item only invalidates its own exchange.
    fn turn_view<'a>(
        &'a self,
        turn: &'a turn::Turn,
        settings: markdown::Settings,
        dark: bool,
    ) -> Element<'a, Message> {
        retained::kept(self.turn_fingerprint(turn, dark), move || {
            self.turn_rows(
                turn,
                |item, cached| self.step_view(item, cached, settings),
                |item, cached, coordinated| {
                    let raw = self
                        .expanded
                        .contains(&item.id)
                        .then(|| self.step_detail(item, cached, settings));
                    self.coordination_card(item, cached, coordinated, settings, raw)
                },
                settings,
            )
        })
    }

    #[cfg(test)]
    pub(super) fn find_turn_view_for_test(&self) -> Element<'_, Message> {
        let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE);
        self.turn_view(&self.turns[0], settings, cosmic::theme::is_dark())
    }

    /// Everything a retained exchange draws, folded into the value that
    /// decides whether it is drawn again.
    ///
    /// It reads the items' change tokens rather than their bodies, so it
    /// costs the same for a one-line answer as for a long one.
    pub(super) fn find_turn_matches(&self, key: &str) -> bool {
        self.find_turns.contains(key)
    }

    fn turn_fingerprint(&self, turn: &turn::Turn, dark: bool) -> u64 {
        self.turn_fingerprint_at(turn, dark, self.source_now_ms())
    }

    fn turn_fingerprint_at(&self, turn: &turn::Turn, dark: bool, now: Option<u64>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        (self.target, dark, self.syntax_theme as u8).hash(&mut hasher);
        (
            &turn.key,
            &turn.summary,
            turn.glyph().map(|status| status as u8),
        )
            .hash(&mut hasher);
        self.opened.get(&turn.key).hash(&mut hasher);
        self.tool_groups_expanded.hash(&mut hasher);
        self.find
            .as_ref()
            .filter(|_| self.find_turn_matches(&turn.key))
            .hash(&mut hasher);
        self.find_paint
            .as_ref()
            .and_then(|paint| paint.current.as_ref())
            .filter(|found| found.turn.as_ref() == turn.key.as_str())
            .map(|found| (found.identity, found.range.start))
            .hash(&mut hasher);
        // A running group's duration counts up, so its strip changes with the
        // clock; a finished one's is fixed at how long the work took.
        if !turn.steps.is_empty() {
            now.and_then(|now| turn.working_for(now))
                .map(|millis| if turn.running { millis / 1_000 } else { millis })
                .hash(&mut hasher);
        }
        let placed = turn.prompt.iter().map(|index| (*index, None)).chain(
            turn.steps
                .iter()
                .chain(&turn.prose)
                .map(|placed| (placed.index, Some(placed.slot))),
        );
        let mut coordinating = false;
        for (index, slot) in placed {
            let Some(item) = self.snapshot.items.get(index) else {
                continue;
            };
            // The latest observation can change at provider heartbeat speed
            // even when the rendered content and elapsed second are unchanged.
            (&item.id, item.started_at.or(item.at), slot, self.expanded.contains(&item.id)).hash(&mut hasher);
            if slot == Some(turn::Slot::Thinking) {
                // A newer turn starting does not revive old reasoning that
                // already has a final answer or later prompt after it.
                reasoning_finished(&self.snapshot.items, self.snapshot.busy, &item.id).hash(&mut hasher);
            }
            // Only this item's badge changes on copy. Its dismissal timer
            // does not affect what the turn draws.
            self.copied
                .as_ref()
                .is_some_and(|(id, _)| *id == item.id)
                .hash(&mut hasher);
            self.cache
                .get(&item.id)
                .map(|cached| {
                    (
                        cached.token,
                        cached.revision,
                        cached.image.as_ref().map(iced::widget::image::Handle::id),
                        cached.image_source.is_some(),
                        self.preview_request.as_deref() == Some(item.id.as_str()),
                        cached.image_wait.is_some(),
                        cached.image_unavailable,
                    )
                })
                .hash(&mut hasher);
            // A coordinating step names agents by what this conversation
            // calls them, links their transcripts while it lists them, tells
            // how those it left in the background are doing, and
            // remembers which of its parts the reader unfolded.
            if self
                .cache
                .get(&item.id)
                .is_some_and(|cached| cached.coordination.is_some())
            {
                coordinating = true;
                let mut unfolded: Vec<&str> = self
                    .unfolded
                    .iter()
                    .filter(|key| {
                        key.strip_prefix(item.id.as_str())
                            .is_some_and(|part| part.starts_with('#'))
                    })
                    .map(String::as_str)
                    .collect();
                unfolded.sort_unstable();
                unfolded.hash(&mut hasher);
            }
        }
        if coordinating {
            for agent in &self.snapshot.subagents {
                (
                    &agent.id,
                    &agent.kind,
                    &agent.description,
                    agent.status,
                    &agent.model,
                    agent.waiting,
                )
                    .hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn prose_action_footer(item: &chat::Item, cached: &CachedItem, slot: turn::Slot) -> f32 {
        if matches!(item.content, ItemContent::Text { .. })
            || (slot == turn::Slot::Card && cached.coordination.is_some())
        {
            MESSAGE_ACTION_FOOTER
        } else {
            0.0
        }
    }

    fn turn_trailing_footer(&self, turn: &turn::Turn) -> f32 {
        // Match the last row actually rendered, including cache misses during
        // parsing, using the same footer classification as turn_rows.
        for placed in turn.prose.iter().rev() {
            if let Some(item) = self.snapshot.items.get(placed.index)
                && let Some(cached) = self.cache.get(&item.id)
            {
                return Self::prose_action_footer(item, cached, placed.slot);
            }
        }
        if !turn.steps.is_empty() {
            return 0.0;
        }
        if turn.prompt.iter().rev().any(|index| {
            self.snapshot.items.get(*index).is_some_and(|item| {
                !matches!(item.content, ItemContent::Delivery { .. })
                    || self.cache.contains_key(&item.id)
            })
        }) {
            MESSAGE_ACTION_FOOTER
        } else {
            0.0
        }
    }

    /// The rows of one exchange, with `step` drawing each of its steps and
    /// `card` each of its messages to and launches of other agents.
    fn turn_rows<'s, 'a>(
        &'s self,
        turn: &'s turn::Turn,
        step: impl Fn(&'s chat::Item, &'s CachedItem) -> Element<'a, Message>,
        card: impl Fn(&'s chat::Item, &'s CachedItem, &'s Coordinated) -> Element<'a, Message>,
        settings: markdown::Settings,
    ) -> Element<'a, Message> {
        let items = &self.snapshot.items;
        let placed = |index: usize| {
            let item = items.get(index)?;
            Some((item, self.cache.get(&item.id)?))
        };
        let mut rows = TranscriptRows::new(step::GAP_M);
        for index in &turn.prompt {
            let Some(item) = items.get(*index) else {
                continue;
            };
            if !matches!(item.content, ItemContent::Delivery { .. }) {
                rows.message(
                    scroll::item(&item.id, self.prompt_view(item)),
                    MESSAGE_ACTION_FOOTER,
                );
            } else if let Some(cached) = self.cache.get(&item.id) {
                // Drawn once its body is parsed, as an answer is; never in the
                // user's own bubble.
                rows.message(
                    scroll::item(&item.id, self.delivery_view(item, cached, settings)),
                    MESSAGE_ACTION_FOOTER,
                );
            }
        }
        if !turn.steps.is_empty() {
            let mut strip = self.step_strip(turn, || {
                turn.steps
                    .iter()
                    .filter_map(|placed_step| placed(placed_step.index))
                    .map(|(item, cached)| {
                        let key = selection::logical_identity(&item.id, &[9]);
                        let revision = cached.revision;
                        // Header elements are cheap owned metadata; only a
                        // mounted row creates their widget tree and geometry.
                        let row = step(item, cached);
                        let id = item.id.clone();
                        let mut row = virtualized::Row::once(
                            key,
                            revision,
                            56.0 + step::GAP_XS,
                            Some(scroll::item_id(&id)),
                            move || {
                                widget::container(scroll::item(&id, row))
                                    .padding(iced::Padding::ZERO.bottom(step::GAP_XS))
                                    .into()
                            },
                        );
                        row.anchor = true;
                        row
                    })
                    .collect()
            });
            // One measured strip supplies aliases for folded steps; expanded
            // rows below it supply their own, more precise item bounds.
            if let Some(item) = turn.steps.first().and_then(|step| items.get(step.index)) {
                strip = widget::container(strip)
                    .id(scroll::steps_id(&item.id))
                    .into();
            }
            rows.push(strip);
        }
        for prose in &turn.prose {
            let Some((item, cached)) = placed(prose.index) else {
                continue;
            };
            if let Some(coordinated) = cached.coordination.as_deref()
                && prose.slot == turn::Slot::Card
            {
                rows.message(
                    scroll::item(&item.id, card(item, cached, coordinated)),
                    Self::prose_action_footer(item, cached, prose.slot),
                );
                continue;
            }
            rows.message(
                scroll::item(
                    &item.id,
                    match &item.content {
                        ItemContent::Text { .. } => {
                            self.text_view(item, cached, prose.slot, settings)
                        }
                        ItemContent::Image { .. } => self.image_preview(item, cached),
                        // Anything else in the answer slot is a step the grouping did
                        // not recognize, drawn the way a step is.
                        _ => step(item, cached),
                    },
                ),
                Self::prose_action_footer(item, cached, prose.slot),
            );
        }
        widget::container(widget::column::with_children(rows.children).width(Length::Fill))
            .id(iced::advanced::widget::Id::new(format!(
                "chat-turn-{}-{}",
                self.target.card, turn.key
            )))
            .into()
    }

    /// What to call an attachment the reader just sent.
    ///
    /// A file has a name, and the name is all the reader needs: the directories
    /// above it are the ones they were just browsing. An inline attachment has
    /// no name at all, because its "path" is the payload itself, so the tail of
    /// it is base64 and the provider's description is the only thing that reads
    /// as a label.
    pub(super) fn attachment_label<'a>(path: &'a str, description: &'a str) -> &'a str {
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

    /// The messages waiting to be sent, docked on the composer.
    ///
    /// One list whoever holds a message - the provider's own queue or this
    /// chat, for a provider without one - because a reader asks the same of
    /// each: what is waiting, when it goes, and how to change their mind. A
    /// row is a glance rather than the message, so the list stays a list at
    /// a hundred entries; revising one takes it back into the composer, the
    /// one place that edits text and files.
    fn queue_view(&self) -> Option<Element<'_, Message>> {
        let target = self.target;
        let caps = &self.snapshot.capabilities;
        let sending = self.followups_sending();
        let available = self.pending.is_none() && !self.disconnected && !self.snapshot.exited;
        let control = |name, label: &'static str, enabled: bool, message: Action| {
            crate::ui_controls::icon_button(name, label)
                .compact()
                .on_press_maybe(enabled.then(|| action(message)))
                .tone(crate::design::Tone::Themed(crate::design::muted_icon_color))
                .into()
        };
        let copy = |text: &str| {
            (!text.trim().is_empty()).then(|| {
                small_icon(
                    Icon::Copy,
                    "Copy queued message",
                    Action::CopyLiteral(text.to_owned()),
                )
            })
        };
        let mut rows = Vec::new();
        for queued in &self.snapshot.queued_messages {
            if self.taken_back.contains(&queued.id) {
                continue;
            }
            let entry = QueueEntry::Provider(queued.id.clone());
            let recalling = self.is_recalling(&queued.id);
            let available = available && !recalling;
            let mut controls: Vec<Element<'_, Message>> = copy(&queued.text).into_iter().collect();
            if caps.queue_remove {
                // An entry the composer cannot hold whole keeps the control,
                // so its absence is not left to be wondered at.
                controls.push(if queued.editable {
                    control(
                        Icon::Edit,
                        "Take back to edit",
                        available,
                        Action::TakeBackQueued(target, Some(entry.clone())),
                    )
                } else {
                    control(
                        Icon::Edit,
                        "Too large or too rich to edit here; it can still be removed",
                        false,
                        Action::TakeBackQueued(target, Some(entry.clone())),
                    )
                });
            }
            if caps.queue_start {
                controls.push(control(
                    Icon::ChevronUp,
                    "Send next, ahead of the rest",
                    available,
                    Action::Command(
                        target,
                        Command::StartQueued {
                            id: queued.id.clone(),
                        },
                    ),
                ));
            }
            if caps.queue_remove {
                controls.push(control(
                    Icon::Close,
                    "Remove from the queue",
                    available,
                    Action::RemoveQueued(target, entry),
                ));
            }
            rows.push(queue_row(
                match queued.delivery {
                    _ if recalling => QueueTiming::Leaving,
                    chat::QueueDelivery::NextStep => QueueTiming::NextStep,
                    chat::QueueDelivery::NextTurn => QueueTiming::NextTurn,
                },
                &queued.text,
                &queued.attachments,
                controls,
            ));
        }
        for followup in &self.followups {
            let entry = QueueEntry::Local(followup.id);
            rows.push(queue_row(
                if sending {
                    QueueTiming::Leaving
                } else {
                    QueueTiming::NextTurn
                },
                &followup.text,
                &followup.attachments,
                copy(&followup.text)
                    .into_iter()
                    .chain([
                        control(
                            Icon::Edit,
                            "Take back to edit",
                            !sending,
                            Action::TakeBackQueued(target, Some(entry.clone())),
                        ),
                        control(
                            Icon::Close,
                            "Remove from the queue",
                            !sending,
                            Action::RemoveQueued(target, entry),
                        ),
                    ])
                    .collect(),
            ));
        }
        if rows.is_empty() {
            return None;
        }
        let count = rows.len();
        if self.snapshot.queued_messages.len() == chat::QueuedMessage::LIMIT {
            rows.push(design::caption("Showing the first 128 queued messages").into());
        }
        let mut header: Vec<Element<'_, Message>> = vec![
            design::caption(format!("Queued · {count}"))
                .width(Length::Fill)
                .into(),
        ];
        // The one thing about the queue a reader cannot discover by looking.
        if sending {
            header.push(design::caption("Sending…").into());
        } else if self.newest_take_back().is_some() {
            // Bare Up belongs to the caret once there is a draft.
            let empty = self
                .composer
                .lines()
                .all(|line| line.text.trim().is_empty());
            header.push(crate::interaction::tooltip(
                crate::ui_controls::passive_icon(Icon::Keyboard, design::COMPACT_ICON_SIZE),
                if empty {
                    "↑ to edit the latest queued message"
                } else {
                    "Alt+↑ to edit the latest queued message"
                },
            ));
        }
        Some(
            widget::container(
                widget::column::with_children(vec![
                    widget::row::with_children(header)
                        .spacing(step::GAP_S)
                        .padding([0.0, step::GAP_XS])
                        .into(),
                    widget::container(
                        widget::scrollable(
                            widget::column::with_children(rows)
                                .spacing(step::GAP_XS)
                                .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                        )
                        .id(card_control_id(self.target.card, "chat-queue"))
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .height(Length::Shrink),
                    )
                    .max_height(QUEUE_CEILING)
                    .into(),
                ])
                .spacing(step::GAP_XS),
            )
            .width(Length::Fill)
            .padding(step::GAP_S)
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_M, design::RADIUS_M, 0.0, 0.0],
                false,
            ))
            .into(),
        )
    }

    pub(super) fn message_copy(&self, item: &chat::Item) -> Element<'static, Message> {
        small_icon(
            if self.copied.as_ref().is_some_and(|(id, _)| *id == item.id) {
                Icon::Check
            } else {
                Icon::Copy
            },
            "Copy this message",
            Action::Copy(self.target, item.id.clone(), CopyPart::Text),
        )
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
            widget::container(selectable_prose::logical_text(
                vec![iced::widget::text::Span::new(
                    queued.text.trim_end().to_owned(),
                )]
                .into(),
                design::TEXT_PROSE.into(),
                iced::Font::default(),
                self.target,
                None,
                selectable_prose::Logical {
                    identity: selection::logical_identity("queued-prompt", &[2]),
                    start: 0,
                    paint: self.find_paint.clone(),
                },
            ))
            .class(design::surface(
                Some(design::accent_wash),
                [design::RADIUS_M; 4],
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
                crate::ui_controls::activity_spinner(
                    f32::from(design::COMPACT_ICON_SIZE),
                    design::accent_color,
                ),
                design::caption(waiting)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
                small_icon(
                    Icon::Copy,
                    "Copy queued message",
                    Action::CopyLiteral(queued.text.clone()),
                ),
                crate::ui_controls::icon_button(Icon::Undo, "Take the message back to edit it")
                    .compact()
                    .on_press_maybe((!sending).then(|| action(Action::CancelQueued(self.target))))
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
    fn prompt_text(&self, item: &chat::Item) -> (String, bool, bool) {
        let attachment = matches!(item.content, ItemContent::Image { .. });
        let text = match &item.content {
            ItemContent::Text { text, .. } => text.as_str(),
            ItemContent::Image { path, description } => Self::attachment_label(path, description),
            content => turn::step_label(content),
        }
        .to_owned();
        // An attachment sits under its prompt as an outlined chip rather than
        // another filled bubble, so a message with two images does not read as
        // three separate things the reader said.
        let long = !attachment && (text.lines().count() > 12 || text.chars().count() > 2000);
        let expanded = self.expanded.contains(&item.id);
        let text = if long && !expanded {
            text.lines()
                .take(12)
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(2000)
                .collect::<String>()
        } else {
            text
        };
        (text, attachment, long)
    }

    fn prompt_view(&self, item: &chat::Item) -> Element<'static, Message> {
        let (text, attachment, long) = self.prompt_text(item);
        let expanded = self.expanded.contains(&item.id);
        let logical = selectable_prose::Logical {
            identity: selection::logical_identity(&item.id, &[2]),
            start: 0,
            paint: self.find_paint.clone(),
        };
        let body: Element<'static, Message> = if attachment {
            widget::row::with_children(vec![
                widget::icon(Icon::Attachment.handle()).size(12).into(),
                selectable_prose::logical_text(
                    vec![iced::widget::text::Span::new(text)].into(),
                    design::TEXT_CAPTION.into(),
                    iced::Font::default(),
                    self.target,
                    None,
                    logical,
                ),
            ])
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center)
            .into()
        } else {
            let text = selectable_prose::logical_text(
                vec![iced::widget::text::Span::new(text)].into(),
                design::TEXT_PROSE.into(),
                iced::Font::default(),
                self.target,
                None,
                logical,
            );
            if long {
                widget::column::with_children(vec![
                    text,
                    widget::button::custom(design::caption(if expanded {
                        "Show less"
                    } else {
                        "Show more"
                    }))
                    .class(crate::ui_controls::control_style(false, None))
                    .on_press(action(Action::Toggle(self.target, item.id.clone())))
                    .into(),
                ])
                .spacing(step::GAP_S)
                .into()
            } else {
                text
            }
        };
        let message = widget::container(body)
            .class(design::surface(
                (!attachment).then_some(design::accent_wash),
                [design::RADIUS_M; 4],
                attachment,
            ))
            .padding([step::GAP_S as u16, step::GAP_M as u16])
            .max_width(PROMPT_MEASURE)
            .into();
        widget::container(message_actions(
            message,
            self.message_copy(item),
            Alignment::End,
        ))
        .align_x(Alignment::End)
        .width(Length::Fill)
        .into()
    }

    /// Something another agent, or the harness for it, delivered.
    ///
    /// It opens the work it prompts, as the reader's own message does, but it
    /// is not theirs: drawn as their bubble, a subagent's report read as
    /// though the reader had pasted it. So it sits on the agent's side of the
    /// column, on the surface used for quoted content, under a heading that
    /// names who sent it. A long report is folded to its opening, because the
    /// agent receiving it usually answers with the part that matters and a
    /// report several screens long would bury that answer.
    ///
    /// A notice is the harness saying background work changed state, so it
    /// takes one quiet line instead of a card.
    fn delivery_view(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        let ItemContent::Delivery {
            kind,
            from,
            sender,
            summary,
            body,
        } = &item.content
        else {
            return self.prompt_view(item);
        };
        let expanded = self.expanded.contains(&item.id);
        let toggle = |label: &'static str| -> Element<'static, Message> {
            widget::button::custom(design::caption(if expanded { "Show less" } else { label }))
                .class(crate::ui_controls::control_style(false, None))
                .on_press(action(Action::Toggle(self.target, item.id.clone())))
                .into()
        };
        let glyph = |name: Icon| -> Element<'static, Message> {
            widget::icon(name.handle())
                .size(14)
                .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
                    cosmic::iced::widget::svg::Style {
                        color: Some(design::muted_icon_color(theme)),
                    }
                })))
                .into()
        };
        if *kind == chat::DeliveryKind::Notice {
            let mut rows = vec![
                widget::row::with_children(vec![
                    glyph(Icon::Attention),
                    design::caption(summary.clone()).into(),
                ])
                .spacing(step::GAP_S)
                .align_y(Alignment::Center)
                .into(),
            ];
            if !body.is_empty() {
                let long = body.lines().count() > NOTICE_LINES;
                let text = visible_notice_body(body, expanded);
                rows.push(
                    widget::container(selectable_prose::logical_text(
                        vec![iced::widget::text::Span::new(text)].into(),
                        design::TEXT_CAPTION.into(),
                        iced::Font::MONOSPACE,
                        self.target,
                        None,
                        selectable_prose::Logical {
                            identity: selection::logical_identity(&item.id, &[2]),
                            start: 0,
                            paint: self.find_paint.clone(),
                        },
                    ))
                    .width(Length::Fill)
                    .into(),
                );
                if long {
                    rows.push(toggle("Show more"));
                }
            }
            return widget::container(message_actions(
                widget::column::with_children(rows)
                    .spacing(step::GAP_XS)
                    .into(),
                self.message_copy(item),
                Alignment::Start,
            ))
            .max_width(design::READABLE_MEASURE)
            .width(Length::Fill)
            .into();
        }
        // A named sender leads and its own summary follows; an unnamed one is
        // described by the harness's summary, as `chat::delivery_heading`.
        let mut heading = vec![glyph(Icon::Chat)];
        let mut subtitle = None;
        if from.is_empty() && !summary.is_empty() {
            heading.push(chat_title(summary.clone()).into());
        } else {
            heading.push(design::caption(kind.verb()).into());
            heading.push(chat_title(chat::delivery_sender(from, sender)).into());
            // A named sender's ID only tells two of the same name apart.
            if !from.is_empty() && !sender.is_empty() {
                heading.push(
                    design::caption(sender.chars().take(8).collect::<String>())
                        .font(iced::Font::MONOSPACE)
                        .into(),
                );
            }
            if !summary.is_empty() {
                subtitle = Some(design::caption(summary.clone()));
            }
        }
        let mut rows = vec![
            widget::row::with_children(heading)
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
        ];
        rows.extend(subtitle.map(Element::from));
        if !body.is_empty() {
            rows.push(fold::fold(
                prose::view(cached, settings, self.target, self.find.as_deref()),
                toggle(if *kind == chat::DeliveryKind::Report {
                    "Show full report"
                } else {
                    "Show full message"
                }),
                !expanded,
                DELIVERY_FOLDED_HEIGHT,
                step::GAP_S,
                design::sunken,
            ));
        }
        let card = widget::container(
            widget::column::with_children(rows)
                .spacing(step::GAP_S)
                .width(Length::Fill),
        )
        .class(design::surface(
            Some(design::sunken),
            [design::RADIUS_M; 4],
            true,
        ))
        .padding([step::GAP_S as u16, step::GAP_M as u16])
        .width(Length::Fill)
        .into();
        widget::container(message_actions(
            card,
            self.message_copy(item),
            Alignment::Start,
        ))
        .max_width(design::READABLE_MEASURE)
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
        let ItemContent::Text { .. } = &item.content else {
            // Anything else in the answer slot is a step the grouping did not
            // recognize; render it the way a step renders rather than
            // inventing a second treatment for it.
            return self.step_view(item, cached, settings);
        };
        self.text_view(item, cached, slot, settings)
    }

    /// A text item in its slot, as an element that owns what it draws.
    fn text_view(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
        slot: turn::Slot,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        let target = self.target;
        if slot == turn::Slot::Commentary {
            return widget::container(message_actions(
                prose::view(cached, settings, self.target, self.find.as_deref()),
                self.message_copy(item),
                Alignment::Start,
            ))
            .max_width(design::READABLE_MEASURE)
            .width(Length::Fill)
            .into();
        }
        if slot == turn::Slot::Thinking {
            let (items, busy) = self
                .selected_subagent
                .as_ref()
                .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id))
                .map_or(
                    (self.snapshot.items.as_slice(), self.snapshot.busy),
                    |agent| {
                        (
                            agent.items.as_slice(),
                            agent.status == chat::ToolStatus::Running,
                        )
                    },
                );
            let finished = reasoning_finished(items, busy, &item.id);
            let expanded = self.expanded.contains(&item.id);
            let elapsed = item.started_at.or(item.at).and_then(|start| {
                items
                    .iter()
                    .skip_while(|candidate| candidate.id != item.id)
                    .skip(1)
                    .find_map(|candidate| candidate.started_at.or(candidate.at))
                    .map(|end| format_duration(end.saturating_sub(start)))
            });
            let label = elapsed.filter(|_| finished).map_or_else(
                || {
                    if finished {
                        "Thought process"
                    } else {
                        "Thinking…"
                    }
                    .to_owned()
                },
                |elapsed| format!("Thought for {elapsed}"),
            );
            let disclosure = widget::button::custom(
                widget::row::with_children(vec![
                    widget::icon(
                        if expanded {
                            Icon::PanDown
                        } else {
                            Icon::PanEnd
                        }
                        .handle(),
                    )
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
                    design::caption(label.clone()).into(),
                ])
                .spacing(step::GAP_S)
                .align_y(Alignment::Center),
            )
            .name(label)
            .class(crate::ui_controls::control_style(false, None))
            .padding([step::GAP_XS as u16, step::GAP_S as u16])
            .on_press(action(Action::Toggle(target, item.id.clone())));
            // Preserve the disclosure's widget path across toggles so native
            // keyboard focus and pressed state survive retained-tree diffs.
            let mut rows = vec![disclosure.into()];
            if expanded {
                rows.push(prose::view(cached, settings, target, self.find.as_deref()));
            }
            let message =
                widget::container(widget::column::with_children(rows).spacing(step::GAP_S))
                    .class(cosmic::theme::Container::custom(move |theme| {
                        widget::container::Style {
                            background: expanded.then(|| design::sunken(theme).into()),
                            text_color: Some(design::muted_color(theme)),
                            border: iced::Border {
                                radius: if expanded {
                                    design::RADIUS_M.into()
                                } else {
                                    Default::default()
                                },
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    }))
                    .padding(if expanded {
                        iced::Padding::from([step::GAP_S, step::GAP_M])
                    } else {
                        iced::Padding::ZERO
                    })
                    .max_width(design::READABLE_MEASURE)
                    .into();
            return widget::container(message_actions(
                message,
                self.message_copy(item),
                Alignment::Start,
            ))
            .width(Length::Fill)
            .into();
        }
        // An answer's hover footer dates the message and offers to copy it, and
        // nothing else: a reader of an answer reaches for those two, and the
        // Markdown source of a rendered answer was a developer's control on a
        // reader's surface.
        // The controls lead and the time follows, muted like the time: a
        // reader reaches for copy far more often than they read the stamp,
        // and an icon in the text color beside a muted stamp was the one
        // thing on the row that looked like it wanted attention.
        let mut controls: Vec<Element<'static, Message>> = vec![small_icon(
            if self.copied.as_ref().is_some_and(|(id, _)| *id == item.id) {
                Icon::Check
            } else {
                Icon::Copy
            },
            "Copy this message",
            Action::Copy(target, item.id.clone(), CopyPart::Text),
        )];

        if let Some((_, stamp, full)) = &cached.stamp {
            controls.push(
                widget::tooltip(
                    design::caption(stamp.clone())
                        .class(cosmic::theme::Text::Custom(design::muted_text)),
                    design::caption(full.clone()),
                    widget::tooltip::Position::Top,
                )
                .into(),
            );
        }
        // The answer is bare prose on the pane's own surface. Wrapping it in
        // a panel would put a frame around the one thing the reader came for,
        // and would make it look like just another block.
        widget::container(message_actions(
            prose::view(cached, settings, self.target, self.find.as_deref()),
            widget::row::with_children(controls)
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
            Alignment::Start,
        ))
        .max_width(design::READABLE_MEASURE)
        .width(Length::Fill)
        .into()
    }
}

pub(super) fn permission_mode_label(mode: &str) -> &str {
    match mode {
        "default" | "manual" => "Ask before edits",
        "readOnly" => "Read only",
        "workspaceWrite" => "Workspace write",
        "acceptEdits" => "Auto-accept edits",
        "auto" => "Auto",
        "plan" => "Plan",
        "dontAsk" => "Don't ask",
        "bypassPermissions" | "yolo" => "YOLO",
        _ => mode,
    }
}

/// What sits at the end of the loaded window while the conversation
/// carries on past it.
///
/// Gestures toward this edge request a page. Its control also gives fitting
/// transcripts and failed requests an explicit way to navigate. The loading
/// state stays at the edge where the requested rows will appear.
fn paging_view(
    target: Target,
    page: Page,
    loading: bool,
    blocked: bool,
) -> Element<'static, Message> {
    let row = vec![
        widget::container(if loading {
            crate::ui_controls::activity_spinner(
                f32::from(design::COMPACT_ICON_SIZE),
                design::muted_color,
            )
        } else {
            widget::Space::new().into()
        })
        .center(Length::Fixed(f32::from(design::COMPACT_ICON_SIZE)))
        .into(),
        design::caption(match page {
            Page::Earlier => "Load earlier history",
            Page::Newer => "Load newer history",
            Page::Latest => "Loading recent history…",
        })
        .class(cosmic::theme::Text::Custom(design::muted_text))
        .into(),
    ];
    widget::container(
        widget::button::custom(
            widget::row::with_children(row)
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center),
        )
        .class(crate::ui_controls::control_style(false, None))
        .on_press_maybe((!blocked).then(|| {
            action(match page {
                Page::Earlier => Action::Command(target, Command::LoadEarlier),
                Page::Newer => Action::Command(target, Command::LoadLater),
                Page::Latest => Action::Latest(target),
            })
        })),
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
#[derive(Clone, Copy)]
pub(super) enum RequestSurface {
    Transcript,
    Permissions,
}

pub(super) fn request_view<'a>(
    target: Target,
    request: &'a chat::Request,
    answers: &'a Answers,
    available: bool,
    surface: RequestSurface,
) -> Element<'a, Message> {
    let (question, context) = question_and_context(&request.description);
    let mut rows: Vec<Element<'a, Message>> = vec![
        widget::row::with_children(vec![
            // The same glyph the status line shows while the agent waits
            // on the reader, so the pane and the panel agree about who is
            // holding the conversation up.
            crate::ui_controls::passive_icon(Icon::AgentWaiting, design::ROW_ICON_SIZE),
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
            target,
        ));
    }
    for field in &request.fields {
        rows.push(field_view(target, request, field, answers, available));
    }
    rows.push(answers_view(target, request, available, surface));
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
                        widget::icon(Icon::Check.handle())
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
        crate::ui_controls::field::text(
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
        .name(field.label.as_str())
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
        // Enter submits an available question form; permissions and consent
        // require an explicit choice.
        .on_submit_maybe(submit_answer(request).filter(|_| available).map(|choice| {
            let answer = action(Action::Answer(
                target,
                request.id.clone(),
                choice.id.clone(),
            ));
            move |_: String| answer.clone()
        }))
        .into(),
    );
    widget::column::with_children(rows)
        .spacing(step::GAP_S)
        .width(Length::Fill)
        .into()
}

fn fast_mode_tooltip(mode: &crate::chat::FastMode) -> String {
    match mode.disabled_reason.as_deref() {
        Some("preference" | "sdk_opt_in_required") => "Enable faster responses for subsequent turns. This can use more allowance or cost more.".into(),
        Some("model" | "unsupported_model") => "Fast mode is unavailable for the selected model.".into(),
        Some("organization" | "org_disabled") => "Fast mode is disabled by the organization.".into(),
        Some("subscription" | "subscription_required") => "Fast mode is unavailable on this account's plan.".into(),
        Some(reason) if reason.contains(' ') => reason.to_owned(),
        Some(_) => "The provider has disabled fast mode for this chat.".into(),
        None if mode.state == crate::chat::FastModeState::Cooldown => "Fast mode is temporarily cooling down. The provider will resume it when available; click to turn it off.".into(),
        None => "Use the provider's faster mode for subsequent turns. It can use more allowance or cost more.".into(),
    }
}

fn cost_estimate_label(estimate: crate::chat::pricing::CostEstimate) -> (String, String) {
    let amount = if estimate.microusd > 0 && estimate.microusd < 10_000 {
        "<$0.01".to_owned()
    } else {
        format!("${:.2}", estimate.microusd as f64 / 1_000_000.0)
    };
    let label = format!(
        "{amount} est.{}",
        if estimate.partial { " · observed" } else { "" }
    );
    let mut tooltip = "API-equivalent text token estimate using the model, cached input, and service tier reported for observed requests. Rates checked 2026-10-12. ChatGPT plan usage is not billed per token; tool fees and regional premiums are excluded.".to_owned();
    if estimate.partial {
        tooltip.push_str(" Some usage could not be fully priced or reconciled, including unobserved earlier requests. This is a partial estimate, not a complete conversation total.");
    }
    (label, tooltip)
}

/// The decisions the provider offered, in the order a reader should meet
/// them.
///
/// Questions put submitting answers first, even when skipping is offered.
/// Approval and consent requests put refusal first and require an explicit
/// choice; a broader session approval follows the one-time approval.
fn answers_view<'a>(
    target: Target,
    request: &'a chat::Request,
    available: bool,
    surface: RequestSurface,
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
    let answers = ordered_answers(request);
    let primary = primary_answer(request).map(|choice| &choice.id);
    widget::row::with_children(
        answers
            .into_iter()
            .map(|choice| {
                let message = available.then_some(action(Action::Answer(
                    target,
                    request.id.clone(),
                    choice.id.clone(),
                )));
                let control = if primary == Some(&choice.id) {
                    crate::ui_controls::button::primary(choice.label.as_str())
                } else {
                    crate::ui_controls::button::secondary(choice.label.as_str())
                }
                .on_press_maybe(message);
                // Custom IDs retain state globally in iced. The transcript
                // and popup render the same answers simultaneously, so only
                // the popup copy receives its keyboard-navigation ID.
                let control: Element<'a, Message> = match surface {
                    RequestSurface::Permissions => control
                        .id(answer_control_id(target, request, choice))
                        .into(),
                    RequestSurface::Transcript => control.into(),
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

pub(super) fn ordered_answers(request: &chat::Request) -> Vec<&chat::Choice> {
    let mut answers: Vec<_> = request.actions.iter().collect();
    // A stable sort keeps equal-stake answers in the provider's order.
    if request.kind != chat::RequestKind::Question {
        answers.sort_by_key(|choice| stake(choice));
    }
    answers
}

pub(super) fn answer_control_id(
    target: Target,
    request: &chat::Request,
    choice: &chat::Choice,
) -> widget::Id {
    card_control_id(
        target.card,
        &format!("chat-answer-{}-{}", request.id, choice.id),
    )
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
                            widget::icon(Icon::Check.handle())
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
        crate::ui_controls::field::search(
            if headed {
                "Search models and providers"
            } else {
                "Search models"
            },
            query,
        )
        .id(card_control_id(target.card, "chat-model-search"))
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
mod frame_cost;

#[cfg(test)]
mod tests {
    use super::*;

    fn native_child_fixture() -> chat::Subagent {
        chat::Subagent {
            id: "native-child".into(),
            kind: "Scout".into(),
            description: "Inspect repository".into(),
            status: chat::ToolStatus::Running,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        }
    }

    #[test]
    fn native_cost_estimates_are_labeled_and_partial_coverage_is_explicit() {
        let (label, tooltip) = cost_estimate_label(crate::chat::pricing::CostEstimate {
            microusd: 25_000,
            partial: false,
        });
        assert_eq!(label, "$0.03 est.");
        assert!(tooltip.contains("ChatGPT plan usage is not billed per token"));
        let (label, tooltip) = cost_estimate_label(crate::chat::pricing::CostEstimate {
            microusd: 1_000,
            partial: true,
        });
        assert_eq!(label, "<$0.01 est. · observed");
        assert!(tooltip.contains("partial estimate"));
    }

    #[test]
    fn fast_mode_explains_preference_and_cooldown_without_protocol_terms() {
        let preference = crate::chat::FastMode {
            state: crate::chat::FastModeState::Off,
            disabled_reason: Some("sdk_opt_in_required".into()),
        };
        assert!(preference.changeable());
        assert!(fast_mode_tooltip(&preference).starts_with("Enable faster responses"));
        assert!(!fast_mode_tooltip(&preference).contains("sdk"));
        let cooldown = crate::chat::FastMode {
            state: crate::chat::FastModeState::Cooldown,
            disabled_reason: None,
        };
        assert!(cooldown.enabled());
        assert!(fast_mode_tooltip(&cooldown).contains("temporarily cooling down"));
    }

    #[test]
    fn assistant_activity_uses_action_footer_as_separation_without_hover_reflow() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        struct Probe {
            summary: String,
            bounds: HashMap<String, iced::Rectangle>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                text: &str,
            ) {
                if text == self.summary {
                    self.bounds.insert("activity".into(), bounds);
                }
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
                    for key in [
                        "Before activity.",
                        "After activity.",
                        "New user exchange.\nA second line.",
                    ] {
                        if state.plain.as_ref() == key {
                            self.bounds.insert(key.into(), bounds);
                        }
                    }
                }
            }
        }
        let text = |id: &str, role, text: &str| chat::Item {
            id: id.into(),
            role,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        };
        let items = vec![
            text("before", chat::Role::Assistant, "Before activity."),
            chat::Item {
                id: "tool".into(),
                role: chat::Role::Assistant,
                content: ItemContent::Command {
                    label: "Check fixture".into(),
                    command: "true".into(),
                    working_directory: "/project".into(),
                    action: chat::CommandAction::Run,
                    status: chat::ToolStatus::Succeeded,
                    exit_code: Some(0),
                    duration_ms: None,
                    output: "visible when expanded".into(),
                },
                at: None,
                started_at: None,
            },
            text("after", chat::Role::Assistant, "After activity."),
            text(
                "new-prompt",
                chat::Role::User,
                "New user exchange.\nA second line.",
            ),
        ];
        let renderer = crate::app::tests::headless_renderer();
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        for width in [360.0, 780.0] {
            let (runtime, _handle) = frame_cost::live_runtime(&items);
            let summary = runtime
                .turns
                .iter()
                .find(|turn| !turn.steps.is_empty())
                .unwrap()
                .summary
                .clone();
            let mut element = runtime.view(
                None,
                false,
                true,
                &shortcuts,
                width,
                Place {
                    channel: None,
                    name: "Spacing fixture".into(),
                    branch: None,
                },
                true,
                None,
                Vec::new(),
            );
            let mut tree = Tree::new(&element);
            let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 1000.0));
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut probe = Probe {
                summary: summary.clone(),
                bounds: Default::default(),
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
            let before = probe.bounds["Before activity."];
            let activity = probe.bounds["activity"];
            let after = probe.bounds["After activity."];
            let next_prompt = probe.bounds["New user exchange.\nA second line."];
            let before_gap = activity.y - (before.y + before.height);
            let after_gap = after.y - (activity.y + activity.height);
            let exchange_gap = next_prompt.y - (after.y + after.height);
            assert!(
                (0.0..=40.0).contains(&before_gap),
                "idle activity gap {before_gap} at width {width}"
            );
            assert!(
                (8.0..=24.0).contains(&after_gap),
                "activity-to-answer gap {after_gap} at width {width}"
            );
            assert!(
                exchange_gap >= before_gap && exchange_gap <= 44.0,
                "new exchange gap {exchange_gap} at width {width}"
            );
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &iced::Event::Mouse(iced::mouse::Event::CursorMoved {
                    position: before.center(),
                }),
                Layout::new(&node),
                iced::mouse::Cursor::Available(before.center()),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut iced::advanced::Shell::new(&mut messages),
                &iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(width, 1000.0)),
            );
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut hovered = Probe {
                summary,
                bounds: Default::default(),
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut hovered);
            assert_eq!(
                hovered.bounds, probe.bounds,
                "hover controls must not move neighboring messages"
            );
        }
    }

    #[test]
    fn conversation_footer_requires_completion_evidence_and_tracks_delegated_waiting() {
        let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
        assert_eq!(runtime.conversation_status(false).1, "Ready");
        Arc::make_mut(&mut runtime.snapshot).ready = false;
        assert_eq!(runtime.conversation_status(false).1, "Connecting");
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.ready = true;
        snapshot.finished_at = Some(42);
        assert_eq!(runtime.conversation_status(false).1, "Finished");
        Arc::make_mut(&mut runtime.snapshot).detail = "Turn interrupted".into();
        assert_eq!(runtime.conversation_status(false).1, "Interrupted");
        Arc::make_mut(&mut runtime.snapshot).detail = "Turn failed".into();
        assert_eq!(runtime.conversation_status(false).1, "Failed");
        Arc::make_mut(&mut runtime.snapshot).detail.clear();
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        assert_ne!(
            runtime.conversation_status(false).1,
            "Finished",
            "old completion cannot mask a new turn"
        );
        Arc::make_mut(&mut runtime.snapshot).error = Some("Earlier history unavailable".into());
        assert_eq!(
            runtime.conversation_status(false).0,
            Some(Icon::AgentWorking)
        );
        Arc::make_mut(&mut runtime.snapshot).waiting = true;
        assert_eq!(runtime.conversation_status(false).1, "Waiting for you");
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.waiting = false;
        snapshot.busy = false;
        assert_eq!(runtime.conversation_status(false).1, "Finished with error");
        Arc::make_mut(&mut runtime.snapshot).error = None;
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.subagents.push(native_child_fixture());
        assert_eq!(runtime.conversation_status(false).1, "Subagents working");
        Arc::make_mut(&mut runtime.snapshot).subagents[0].waiting = true;
        assert_eq!(runtime.conversation_status(false).1, "Subagents waiting");
        Arc::make_mut(&mut runtime.snapshot).subagents[0].status = chat::ToolStatus::Succeeded;
        assert_eq!(
            runtime.conversation_status(false).1,
            "Finished",
            "terminal children cannot retain waiting state"
        );
        runtime.disconnected = true;
        assert_eq!(runtime.conversation_status(false).1, "Disconnected");
    }

    #[test]
    fn native_child_identity_uses_its_reported_model_and_waiting_state() {
        let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
        Arc::make_mut(&mut runtime.snapshot).selected_model = Some("parent-model".into());
        let mut child = native_child_fixture();
        let unknown = runtime.native_child_identity(&child);
        assert!(unknown.contains("Claude native subagent"));
        assert!(unknown.contains("Model not reported"));
        assert!(!unknown.contains("parent-model"));
        child.model = Some("child-model".into());
        child.waiting = true;
        child.detail = "Waiting for your approval".into();
        let known = runtime.native_child_identity(&child);
        assert!(known.contains("child-model"));
        assert!(known.contains("Waiting for you"));
        assert!(known.contains("Waiting for your approval"));
        child.status = chat::ToolStatus::Succeeded;
        assert_eq!(Runtime::native_child_status(&child), "Finished");
    }

    #[test]
    fn selected_native_child_renders_identity_channel_and_distinct_tab() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
        let mut child = native_child_fixture();
        child.model = Some("child-model".into());
        child.waiting = true;
        child.detail = "Waiting for your approval".into();
        runtime.selected_subagent = Some(child.id.clone());
        Arc::make_mut(&mut runtime.snapshot).subagents.push(child);
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            600.0,
            Place {
                name: "project".into(),
                branch: None,
                channel: Some("Review".into()),
            },
            true,
            None,
            vec![],
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 800.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let labels: Vec<_> = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect();
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Claude native subagent")
                    && label.contains("child-model")
                    && label.contains("Waiting for you")),
            "{labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Native · Inspect repository")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("# Review")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("Conversation")),
            "{labels:?}"
        );
    }

    #[test]
    fn native_child_tabs_select_both_transcripts_through_real_runtime_rebuild_with_approval() {
        use iced::advanced::clipboard;
        use iced::runtime::user_interface::{Cache, UserInterface};
        fn runtime(app: &Cagents, target: Target) -> &Runtime {
            match app.cards.get(&target.card) {
                Some(CardRuntime::Chat(runtime)) => runtime,
                _ => panic!("the fixture chat remains mounted"),
            }
        }
        fn view(app: &Cagents, target: Target) -> Element<'_, Message> {
            runtime(app, target).view(
                None,
                false,
                true,
                &app.settings.keyboard_shortcuts,
                620.0,
                Place {
                    name: "native-fixture".into(),
                    branch: None,
                    channel: Some("Review".into()),
                },
                true,
                None,
                Vec::new(),
            )
        }
        let thought = chat::Item {
            id: "live-thought".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: "Inspecting compact tools and native agent tabs.".into(),
                thinking: true,
                phase: chat::Phase::Commentary,
            },
            at: None,
            started_at: None,
        };
        let (mut live, _handle) = frame_cost::live_runtime(&[thought]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        live.session.launch.provider = Provider::Codex;
        live.expanded.insert("live-thought".into());
        let mut child = native_child_fixture();
        child.waiting = true;
        child.model = Some("gpt-fixture-mini".into());
        child.detail = "Waiting for your approval".into();
        let child_name = live.native_child_identity(&child);
        let snapshot = Arc::make_mut(&mut live.snapshot);
        snapshot.busy = true;
        snapshot.subagents.push(child);
        snapshot.requests.push(chat::Request {
            kind: crate::chat::RequestKind::Approval,
            id: "child-approval".into(),
            title: "Child agent · Command approval".into(),
            description: "printf native-fixture".into(),
            fields: Vec::new(),
            actions: vec![
                chat::Choice {
                    id: "decline".into(),
                    label: "Deny".into(),
                    description: String::new(),
                },
                chat::Choice {
                    id: "accept".into(),
                    label: "Allow once".into(),
                    description: String::new(),
                },
            ],
        });
        app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        let mut renderer = crate::app::tests::headless_renderer();
        let size = iced::Size::new(620.0, 720.0);
        let mut cache = Cache::new();
        for (name, selected) in [
            (child_name.as_str(), Some("native-child")),
            ("Main conversation", None),
        ] {
            let mut ui = UserInterface::build(view(&app, target), size, cache, &mut renderer);
            let accessible = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
            let bounds = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .find(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some(name)
                })
                .and_then(|node| node.node().bounds())
                .expect("the transcript tab is mounted");
            let pointer = iced::Point::new(
                (bounds.x0 + bounds.x1) as f32 / 2.0,
                (bounds.y0 + bounds.y1) as f32 / 2.0,
            );
            let mut messages = Vec::new();
            ui.update(
                &[
                    iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: pointer }),
                    iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    )),
                ],
                iced::mouse::Cursor::Available(pointer),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                !messages
                    .iter()
                    .any(|message| matches!(message, Message::ChatUi(Action::SelectSubagent(..))))
            );
            cache = ui.into_cache();
            // This is the actual UserInterface cache extraction/rebuild path
            // used after the press requests a native redraw.
            let mut ui = UserInterface::build(view(&app, target), size, cache, &mut renderer);
            ui.update(
                &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                ))],
                iced::mouse::Cursor::Available(pointer),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            cache = ui.into_cache();
            let selection = messages
                .into_iter()
                .find_map(|message| match message {
                    Message::ChatUi(action @ Action::SelectSubagent(..)) => Some(action),
                    _ => None,
                })
                .expect(
                    "native transcript tab publishes selection through the full chat hierarchy",
                );
            drop(app.handle_chat_ui(selection));
            assert_eq!(runtime(&app, target).selected_subagent.as_deref(), selected);
        }
    }

    #[test]
    fn live_reasoning_disclosure_toggles_through_actions_and_retains_streaming_choice() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree},
        };
        #[derive(Default)]
        struct ReadText(Vec<(String, iced::Rectangle)>);
        impl Operation for ReadText {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                    self.0.push((prose.plain.to_string(), bounds));
                }
            }
        }
        fn runtime(app: &Cagents, target: Target) -> &Runtime {
            match app.cards.get(&target.card) {
                Some(CardRuntime::Chat(runtime)) => runtime,
                _ => panic!("the fixture chat remains mounted"),
            }
        }
        let item = chat::Item {
            id: "live-thought".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: "Secret reasoning body".into(),
                thinking: true,
                phase: chat::Phase::Commentary,
            },
            at: None,
            started_at: None,
        };
        let (mut live, handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
        Arc::make_mut(&mut live.snapshot).busy = true;
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        let renderer = crate::app::tests::headless_renderer();
        let view = |app: &Cagents| {
            let runtime = runtime(app, target);
            let item = &runtime.snapshot.items[0];
            runtime.text_view(
                item,
                &runtime.cache[&item.id],
                turn::Slot::Thinking,
                design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE),
            )
        };
        let mut tree = Tree::new(view(&app).as_widget());
        for (step_index, expanded) in [false, true, true, false].into_iter().enumerate() {
            assert_eq!(
                runtime(&app, target).expanded.contains("live-thought"),
                expanded,
                "the actual Toggle action controls the disclosure state"
            );
            let mut element = view(&app);
            tree.diff(element.as_widget_mut());
            let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 800.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, viewport.size()),
            );
            let mut rendered = ReadText::default();
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut rendered,
            );
            let values: Vec<_> = rendered
                .0
                .into_iter()
                .filter(|(_, bounds)| {
                    bounds.width > 0.0
                        && bounds.height > 0.0
                        && bounds.y < viewport.height
                        && bounds.y + bounds.height > 0.0
                })
                .map(|(text, _)| text)
                .collect();
            assert_eq!(
                values
                    .iter()
                    .any(|value| value.contains("Secret reasoning body")),
                expanded,
                "step={step_index} expanded={expanded} markdown_items={} rendered={values:?}",
                runtime(&app, target).cache["live-thought"]
                    .markdown
                    .items()
                    .len()
            );
            if expanded && !values.iter().any(|value| value.contains("streamed detail")) {
                // Publish a live delta while open, then keep the same widget tree.
                let runtime = app.chat_runtime_mut(target).unwrap();
                let mut snapshot = runtime.snapshot.as_ref().clone();
                let mut item = snapshot.items[0].as_ref().clone();
                if let ItemContent::Text { text, .. } = &mut item.content {
                    text.push_str(" streamed detail");
                }
                snapshot.items[0] = Arc::new(item);
                handle.publish_snapshot_for_test(snapshot);
                runtime.refresh();
                assert!(runtime.expanded.contains("live-thought"));
                continue;
            }
            if step_index == 3 {
                break;
            }
            let pointer = iced::Point::new(36.0, 16.0);
            let mut messages = Vec::new();
            for event in [
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
            ] {
                element.as_widget_mut().update(
                    &mut tree,
                    &event,
                    Layout::new(&node),
                    iced::mouse::Cursor::Available(pointer),
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            let toggle = messages
                .into_iter()
                .find_map(|message| match message {
                    Message::ChatUi(Action::Toggle(actual, id)) if actual == target => Some(id),
                    _ => None,
                })
                .expect("the native reasoning button emits a disclosure action");
            drop(app.handle_chat_ui(Action::Toggle(target, toggle)));
            assert_eq!(
                runtime(&app, target).expanded.contains("live-thought"),
                !expanded,
                "published Toggle must update the mounted runtime"
            );
        }
    }

    #[test]
    fn reasoning_disclosure_preserves_keyboard_focus_across_retained_toggles() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation},
        };
        #[derive(Default)]
        struct FocusDisclosure(bool);
        impl Operation for FocusDisclosure {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                _: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if !self.0 {
                    state.focus();
                    self.0 = true;
                } else {
                    state.unfocus();
                }
            }
        }
        #[derive(Default)]
        struct Probe {
            focused: Option<bool>,
            prose: Vec<String>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                _: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                self.focused.get_or_insert_with(|| state.is_focused());
            }
            fn custom(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if bounds.width > 0.0
                    && bounds.height > 0.0
                    && let Some(prose) = state.downcast_ref::<selectable_prose::State>()
                {
                    self.prose.push(prose.plain.to_string());
                }
            }
        }
        fn runtime(app: &Cagents, target: Target) -> &Runtime {
            match app.cards.get(&target.card) {
                Some(CardRuntime::Chat(runtime)) => runtime,
                _ => panic!("the fixture chat remains mounted"),
            }
        }
        let item = chat::Item {
            id: "keyboard-thought".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: "Keyboard reasoning body".into(),
                thinking: true,
                phase: chat::Phase::Commentary,
            },
            at: None,
            started_at: None,
        };
        let (mut live, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
        Arc::make_mut(&mut live.snapshot).busy = true;
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        let renderer = crate::app::tests::headless_renderer();
        let view = |app: &Cagents| {
            let runtime = runtime(app, target);
            let item = &runtime.snapshot.items[0];
            runtime.text_view(
                item,
                &runtime.cache[&item.id],
                turn::Slot::Thinking,
                design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE),
            )
        };
        let mut tree = Tree::new(view(&app).as_widget());
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 800.0));
        for (index, expanded) in [false, true, false].into_iter().enumerate() {
            let mut element = view(&app);
            tree.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, viewport.size()),
            );
            if index == 0 {
                // Native keyboard focus is assigned once. The second Enter
                // must work through the same retained tree without refocusing.
                let mut focus = FocusDisclosure::default();
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut focus,
                );
                assert!(focus.0, "the mounted disclosure is focusable");
            }
            let mut probe = Probe::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
            assert_eq!(
                probe.focused,
                Some(true),
                "disclosure focus survives diff at stage {index}"
            );
            assert_eq!(
                probe
                    .prose
                    .iter()
                    .any(|text| text.contains("Keyboard reasoning body")),
                expanded
            );
            assert_eq!(
                runtime(&app, target).expanded.contains("keyboard-thought"),
                expanded
            );
            if index == 2 {
                break;
            }
            let enter = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                modified_key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
                physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Enter),
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
            let mut toggles = messages.into_iter().filter_map(|message| match message {
                Message::ChatUi(Action::Toggle(actual, id)) if actual == target => Some(id),
                _ => None,
            });
            let id = toggles
                .next()
                .expect("Enter toggles the focused disclosure");
            assert_eq!(id, "keyboard-thought");
            assert!(toggles.next().is_none(), "one key emits one toggle");
            drop(app.handle_chat_ui(Action::Toggle(target, id)));
            assert_eq!(
                runtime(&app, target).expanded.contains("keyboard-thought"),
                !expanded
            );
        }
    }

    #[test]
    fn rendered_exact_tool_targets_keep_verb_prefixed_names() {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced_accessibility::accesskit::Role;
        for (operation, target, name) in [
            (
                chat::CommandAction::Read,
                "Read λ notes.md",
                "Read Read λ notes.md",
            ),
            (
                chat::CommandAction::Search,
                "Search TODO",
                "Searched Search TODO",
            ),
            (chat::CommandAction::List, "List *.rs", "Listed List *.rs"),
        ] {
            let item = chat::Item {
                id: "exact".into(),
                role: chat::Role::Assistant,
                content: ItemContent::Command {
                    label: target.into(),
                    command: target.into(),
                    working_directory: "/project".into(),
                    action: operation,
                    status: chat::ToolStatus::Succeeded,
                    exit_code: None,
                    duration_ms: None,
                    output: "result".into(),
                },
                started_at: None,
                at: None,
            };
            let (runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
            assert_eq!(runtime.cache[&item.id].preview.text, target);
            assert_eq!(runtime.turns[0].summary, name);
            for open in [false, true] {
                let mut element = runtime.step_header(&item, &runtime.cache[&item.id], open);
                let renderer = crate::app::tests::headless_renderer();
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(520.0, 1000.0)),
                );
                let accessible = element.as_widget().a11y_nodes(
                    Layout::new(&node),
                    &tree,
                    iced::mouse::Cursor::Unavailable,
                );
                let nodes: Vec<_> = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .collect();
                assert!(
                    nodes.iter().any(|node| node.node().role() == Role::Button
                        && node.node().label() == Some(name)),
                    "row action must name its exact target"
                );
                // RichText has no separate accessible leaf in this pinned
                // widget graph; the mounted row's exact name carries its
                // target, while the preview cache preserves the drawn bytes.
                let row = nodes
                    .iter()
                    .find(|node| node.node().label() == Some(name))
                    .unwrap()
                    .id()
                    .clone();
                let copy = nodes
                    .iter()
                    .find(|node| node.node().label() == Some("Copy this message"))
                    .unwrap()
                    .id()
                    .clone();
                use iced_accessibility::{
                    A11yId,
                    accesskit::{Action as A11yAction, ActionRequest, TreeId},
                };
                let event = |id: A11yId, action| {
                    let A11yId::Widget(widget) = id.clone() else {
                        panic!("widget identity required")
                    };
                    iced::Event::A11y(
                        widget,
                        ActionRequest {
                            action,
                            target_tree: TreeId::ROOT,
                            target_node: id.into(),
                            data: None,
                        },
                    )
                };
                let mut dispatch = |event: iced::Event| {
                    let mut messages = Vec::new();
                    element.as_widget_mut().update(
                        &mut tree,
                        &event,
                        Layout::new(&node),
                        iced::mouse::Cursor::Unavailable,
                        &renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut iced::advanced::Shell::new(&mut messages),
                        &iced::Rectangle::with_size(iced::Size::new(520.0, 1000.0)),
                    );
                    messages
                };
                let unknown = A11yId::from(iced::advanced::widget::Id::unique());
                assert!(dispatch(event(unknown.clone(), A11yAction::Click)).is_empty());
                assert!(dispatch(event(unknown, A11yAction::Focus)).is_empty());
                let mut mismatched = event(row.clone(), A11yAction::Click);
                if let iced::Event::A11y(_, request) = &mut mismatched {
                    request.target_node = copy.clone().into();
                }
                assert!(dispatch(mismatched).is_empty());
                let mut wrong_tree = event(row.clone(), A11yAction::Click);
                if let iced::Event::A11y(_, request) = &mut wrong_tree {
                    request.target_tree = TreeId(uuid::Uuid::new_v4());
                }
                assert!(dispatch(wrong_tree).is_empty());
                let toggled = dispatch(event(row.clone(), A11yAction::Click));
                assert_eq!(toggled.len(), 1);
                assert!(
                    matches!(&toggled[0], Message::ChatUi(Action::Toggle(actual, id))
                    if *actual == runtime.target && id == &item.id)
                );
                let copied = dispatch(event(copy.clone(), A11yAction::Click));
                assert_eq!(copied.len(), 1);
                assert!(
                    matches!(&copied[0], Message::ChatUi(Action::Copy(actual, id, CopyPart::Text))
                    if *actual == runtime.target && id == &item.id)
                );
                assert!(dispatch(event(copy.clone(), A11yAction::Focus)).is_empty());
                drop(dispatch);
                // Keep the actual widget state mounted through diff. AT focus
                // makes the hover control keyboard-visible without a pointer.
                tree.diff(element.as_widget_mut());
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
                let mut dispatch = |event: iced::Event| {
                    let mut messages = Vec::new();
                    element.as_widget_mut().update(
                        &mut tree,
                        &event,
                        Layout::new(&node),
                        iced::mouse::Cursor::Unavailable,
                        &renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut iced::advanced::Shell::new(&mut messages),
                        &iced::Rectangle::with_size(iced::Size::new(520.0, 1000.0)),
                    );
                    messages
                };
                assert!(dispatch(event(row, A11yAction::Blur)).is_empty());
                let copied = dispatch(enter.clone());
                assert_eq!(copied.len(), 1);
                assert!(
                    matches!(&copied[0], Message::ChatUi(Action::Copy(actual, id, CopyPart::Text))
                    if *actual == runtime.target && id == &item.id)
                );
                assert!(dispatch(event(copy, A11yAction::Blur)).is_empty());
                assert!(
                    dispatch(enter).is_empty(),
                    "cleared hidden Copy focus must not consume Enter"
                );
            }
        }
    }

    #[test]
    fn transcript_viewport_reaches_pane_edges_with_inset_inside_content() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        struct Probe {
            id: widget::Id,
            viewport: Option<iced::Rectangle>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: iced::Rectangle,
                _: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&self.id) {
                    self.viewport = Some(bounds);
                }
            }
        }
        let items = frame_cost::transcript(1);
        let (runtime, _handle) = frame_cost::live_runtime(&items);
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        let mut element = runtime.view(
            None,
            false,
            true,
            &shortcuts,
            600.0,
            Place {
                channel: None,
                name: "Fixture".into(),
                branch: None,
            },
            true,
            None,
            vec![],
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 700.0)),
        );
        let mut probe = Probe {
            id: card_control_id(runtime.target.card, "chat-transcript"),
            viewport: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        let bounds = probe.viewport.expect("transcript viewport");
        assert_eq!(bounds.x, 0.0);
        assert_eq!(bounds.y, 0.0);
        assert_eq!(bounds.width, 600.0);
    }

    #[test]
    fn composer_stays_inset_with_and_without_a_conversation_title() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        struct Probe {
            id: widget::Id,
            bounds: Option<iced::Rectangle>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if id == Some(&self.id) {
                    self.bounds = Some(bounds);
                }
            }
        }
        let items = frame_cost::transcript(1);
        let renderer = crate::app::tests::headless_renderer();
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        for title in [None, Some("Conversation".to_owned())] {
            for width in [280.0, 600.0] {
                let (mut runtime, _handle) = frame_cost::live_runtime(&items);
                Arc::make_mut(&mut runtime.snapshot).title = title.clone();
                let mut element = runtime.view(
                    None,
                    false,
                    true,
                    &shortcuts,
                    width,
                    Place {
                        channel: None,
                        name: "Fixture".into(),
                        branch: None,
                    },
                    true,
                    None,
                    vec![],
                );
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 700.0)),
                );
                let mut probe = Probe {
                    id: card_control_id(runtime.target.card, "chat-composer"),
                    bounds: None,
                };
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut probe,
                );
                let bounds = probe.bounds.expect("composer editor");
                assert_eq!(bounds.x, step::GAP_M);
                assert_eq!(bounds.width, width - 2.0 * step::GAP_M);
            }
        }
    }

    #[test]
    fn prompt_notice_and_raw_output_share_transcript_selection() {
        use iced::advanced::{
            layout,
            widget::{Operation, Tree},
        };
        let items = frame_cost::transcript(1);
        let (runtime, _handle) = frame_cost::live_runtime(&items);
        let prompt = items
            .iter()
            .find(|item| item.role == chat::Role::User)
            .expect("user prompt");
        let expected = match &prompt.content {
            ItemContent::Text { text, .. } => text.clone(),
            _ => panic!("fixture prompt must be text"),
        };
        let output = tool_text::ToolText::update(None, "tool output\n", tool_text::Language::Plain);
        let content = widget::column::with_children(vec![
            runtime.prompt_view(prompt),
            transcript_body("Queued message", runtime.target),
            notice_entry(
                "Provider warning",
                runtime.target,
                notice_identity(None, "Provider warning"),
            ),
            output.view(
                crate::settings::EditorSyntaxTheme::Auto,
                None,
                runtime.target,
                None,
            ),
        ]);
        let mut element = selection::transcript(content, runtime.target, None);
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1200.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        #[derive(Default)]
        struct ReadText(Vec<(String, iced::Rectangle)>);
        impl Operation for ReadText {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                    self.0.push((prose.plain.to_string(), bounds));
                }
            }
        }
        let mut text = ReadText::default();
        element.as_widget_mut().operate(
            &mut tree,
            iced::advanced::Layout::new(&node),
            &renderer,
            &mut text,
        );
        assert_eq!(
            text.0
                .iter()
                .map(|(text, _)| text.clone())
                .collect::<Vec<_>>(),
            [
                expected.clone(),
                "Queued message".into(),
                "Provider warning".into(),
                "tool output\n".into()
            ]
        );
        #[derive(Default)]
        struct ClipboardText(String);
        impl iced::advanced::Clipboard for ClipboardText {
            fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
                Some(self.0.clone())
            }
            fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
                self.0 = contents;
            }
        }
        let first = text.0.first().unwrap().1;
        let last = text.0.last().unwrap().1;
        let start = iced::Point::new(first.x, first.y + 3.0);
        let end = iced::Point::new(last.x + last.width + 5.0, last.y + last.height + 1.0);
        let mut clipboard = ClipboardText::default();
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 1200.0));
        for (event, pointer) in [
            (
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                start,
            ),
            (
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
                end,
            ),
            (
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
                end,
            ),
            (
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("c".into()),
                    modified_key: keyboard::Key::Character("c".into()),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::CTRL,
                    text: None,
                    repeat: false,
                }),
                end,
            ),
        ] {
            let mut messages = Vec::new();
            let mut shell = iced::advanced::Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &event,
                iced::advanced::Layout::new(&node),
                iced::mouse::Cursor::Available(pointer),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert!(shell.is_event_captured());
            if shell.is_layout_invalid() {
                node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
            }
        }
        assert_eq!(
            clipboard.0,
            format!("{expected}\n\nQueued message\n\nProvider warning\n\ntool output\n")
        );
    }

    #[test]
    fn forgetting_a_conversation_releases_catalog_and_selection_contexts() {
        let (mut runtime, _handle) = frame_cost::live_runtime(&frame_cost::transcript(1));
        let (catalog, _) = runtime.selection_document();
        let backing = Arc::downgrade(&catalog);
        assert!(catalog.retained_plain_bytes() > 0);
        drop(catalog);
        runtime.forget_transcript();
        assert!(runtime.selection_document.borrow().is_none());
        assert!(runtime.selection_bookmarks.borrow().is_empty());
        assert!(backing.upgrade().is_none(), "no orphan logical plaintext owner survives reset");
    }

    #[test]
    fn notice_projection_keeps_exact_visible_newlines() {
        for body in ["short", "short\n", "short\n\n"] {
            assert_eq!(visible_notice_body(body, false), body);
            assert_eq!(visible_notice_body(body, true), body);
        }
        let long = "line\n".repeat(NOTICE_LINES + 1);
        assert_eq!(
            visible_notice_body(&long, false),
            vec!["line"; NOTICE_LINES].join("\n")
        );
        assert_eq!(visible_notice_body(&long, true), long);
    }

    #[test]
    fn failure_catalog_keeps_visible_turn_order_and_duplicate_messages_distinct() {
        let items = frame_cost::transcript(2);
        let (mut runtime, _handle) = frame_cost::live_runtime(&items);
        assert_eq!(runtime.turns.len(), 2);
        let first = runtime.turns[0].key.clone();
        let second = runtime.turns[1].key.clone();
        runtime.failures = vec![
            (Some(first.clone()), "Repeated failure".into()),
            (Some(second.clone()), "Repeated failure".into()),
        ];
        runtime.error = Some("Current failure".into());
        let (catalog, _) = runtime.selection_document();
        let blocks = catalog.search_blocks();
        let identities: Vec<_> = blocks.iter().map(|(identity, _)| *identity).collect();
        let position = |identity| {
            identities
                .iter()
                .position(|candidate| *candidate == identity)
                .unwrap()
        };
        let first_notice = position(notice_identity(Some(&first), "Repeated failure"));
        let second_notice = position(notice_identity(Some(&second), "Repeated failure"));
        let second_prompt = &items[runtime.turns[1].prompt[0]];
        assert!(first_notice < position(selection::logical_identity(&second_prompt.id, &[2])));
        assert!(second_notice > first_notice);
        assert_eq!(blocks.last().unwrap().1.as_ref(), "Current failure");
    }

    #[test]
    fn queued_and_notice_widgets_copy_through_logical_catalog() {
        use iced::advanced::{
            layout,
            widget::{Operation, Tree},
        };
        let items = frame_cost::transcript(1);
        let (runtime, _handle) = frame_cost::live_runtime(&items);
        let queued = Queued {
            text: "Queued message".into(),
            resolved_text: "Queued message".into(),
            attachments: vec![],
            model: None,
            draft: runtime.session.clone(),
            since: None,
            rechecking: false,
        };
        let mut delivery = items[0].clone();
        delivery.id = "delivery-notice".into();
        delivery.content = ItemContent::Delivery {
            kind: chat::DeliveryKind::Notice,
            from: String::new(),
            sender: String::new(),
            summary: "Provider report".into(),
            body: "Delivered warning".into(),
        };
        let cached = CachedItem::new(&delivery.content, crate::settings::EditorSyntaxTheme::Auto);
        let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
        let first = notice_identity(Some("turn-a"), "Repeated failure");
        let second = notice_identity(Some("turn-b"), "Repeated failure");
        assert_ne!(first, second);
        let mut builder = selection::Catalog::builder();
        builder.logical(first, Arc::from("Repeated failure"));
        builder.plain("queued-prompt", &[2], Arc::from("Queued message"));
        builder.plain(&delivery.id, &[2], Arc::from("Delivered warning"));
        builder.logical(second, Arc::from("Repeated failure"));
        let content = widget::column::with_children(vec![
            notice_entry("Repeated failure", runtime.target, first),
            runtime.queued_view(&queued),
            runtime.delivery_view(&delivery, &cached, settings),
            notice_entry("Repeated failure", runtime.target, second),
        ]);
        let mut element = selection::transcript_virtualized(
            content,
            runtime.target,
            None,
            builder.finish(),
            Default::default(),
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 1200.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        #[derive(Default)]
        struct ReadText(Vec<(String, iced::Rectangle)>);
        impl Operation for ReadText {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                    self.0.push((prose.plain.to_string(), bounds));
                }
            }
        }
        let mut text = ReadText::default();
        element.as_widget_mut().operate(
            &mut tree,
            iced::advanced::Layout::new(&node),
            &renderer,
            &mut text,
        );
        assert_eq!(
            text.0
                .iter()
                .map(|(text, _)| text.clone())
                .collect::<Vec<_>>(),
            [
                "Repeated failure",
                "Queued message",
                "Delivered warning",
                "Repeated failure"
            ]
        );
        #[derive(Default)]
        struct ClipboardText(String);
        impl iced::advanced::Clipboard for ClipboardText {
            fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
                Some(self.0.clone())
            }
            fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
                self.0 = contents;
            }
        }
        let first = text.0.first().unwrap().1;
        let last = text.0.last().unwrap().1;
        let start = iced::Point::new(first.x, first.y + 3.0);
        let end = iced::Point::new(last.x + last.width + 5.0, last.y + last.height + 1.0);
        let mut clipboard = ClipboardText::default();
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 1200.0));
        for (event, pointer) in [
            (
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                start,
            ),
            (
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
                end,
            ),
            (
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
                end,
            ),
            (
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("c".into()),
                    modified_key: keyboard::Key::Character("c".into()),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::CTRL,
                    text: None,
                    repeat: false,
                }),
                end,
            ),
        ] {
            let mut messages = Vec::new();
            let mut shell = iced::advanced::Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &event,
                iced::advanced::Layout::new(&node),
                iced::mouse::Cursor::Available(pointer),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert!(shell.is_event_captured());
            if shell.is_layout_invalid() {
                node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
            }
        }
        assert_eq!(
            clipboard.0,
            "Repeated failure\n\nQueued message\n\nDelivered warning\n\nRepeated failure"
        );
    }

    #[test]
    fn pointer_copy_across_visual_paragraph_and_code_chunks_adds_no_separators() {
        use iced::advanced::{
            layout,
            widget::{Operation, Tree},
        };
        let items = frame_cost::transcript(1);
        let (runtime, _handle) = frame_cost::live_runtime(&items);
        for source in ["bounded αβ words ".repeat(300), format!("```\n{}```", "αβ item line\n".repeat(100))] {
        let cached = CachedItem::new(&ItemContent::Text { text: source, thinking: false,
            phase: chat::Phase::Final }, crate::settings::EditorSyntaxTheme::Auto);
        let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
        let mut builder = selection::Catalog::builder();
        builder.markdown(&format!("unbound-{}", cached.revision), &[0], cached.markdown.items());
        let catalog = builder.finish();
        let expected = catalog.search_blocks().into_iter().map(|(_, text)| text.to_string()).collect::<String>();
        let content = scroll::transcript(prose::view(&cached, settings, runtime.target, None),
            runtime.target, None, vec![], true, vec![]);
        let mut element = selection::transcript_virtualized(
            content, runtime.target, None, catalog, Default::default(),
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 3000.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        #[derive(Default)]
        struct ReadText(Vec<(String, iced::Rectangle)>);
        impl Operation for ReadText {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_mut::<selectable_prose::State>() {
                    self.0.push((prose.plain.to_string(), bounds));
                }
            }
        }
        let mut text = ReadText::default();
        element.as_widget_mut().operate(
            &mut tree,
            iced::advanced::Layout::new(&node),
            &renderer,
            &mut text,
        );
        assert!(text.0.len() >= 2, "fixture crosses a real visual chunk seam");
        assert_eq!(text.0.iter().map(|(text, _)| text.as_str()).collect::<String>(), expected);
        #[derive(Default)]
        struct ClipboardText(String);
        impl iced::advanced::Clipboard for ClipboardText {
            fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
                Some(self.0.clone())
            }
            fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
                self.0 = contents;
            }
        }
        let first = text.0.first().unwrap().1;
        let last = text.0.last().unwrap().1;
        let start = iced::Point::new(first.x, first.y + 3.0);
        let end = iced::Point::new(last.x + last.width + 5.0, last.y + last.height + 1.0);
        let mut clipboard = ClipboardText::default();
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 3000.0));
        for (event, pointer) in [
            (
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                start,
            ),
            (
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
                end,
            ),
            (
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
                end,
            ),
            (
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("c".into()),
                    modified_key: keyboard::Key::Character("c".into()),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::CTRL,
                    text: None,
                    repeat: false,
                }),
                end,
            ),
        ] {
            let mut messages = Vec::new();
            let mut shell = iced::advanced::Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &event,
                iced::advanced::Layout::new(&node),
                iced::mouse::Cursor::Available(pointer),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert!(shell.is_event_captured());
            if shell.is_layout_invalid() {
                node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
            }
        }
        assert_eq!(clipboard.0, expected, "visual chunk seams are absent from copied logical source");
        }
    }

    #[test]
    fn waiting_queue_is_bounded_and_independent_of_transcript_scroll() {
        let mut runtime = Runtime::draft(
            Target {
                card: CardId::new(),
                generation: 0,
            },
            chat::Session {
                id: crate::model::TerminalId::new(),
                launch: chat::Launch {
                    provider: Provider::Claude,
                    recipe: crate::model::LaunchRecipe::shell(PathBuf::from("/unused")).unwrap(),
                    session_id: None,
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: chat::NativeContinuity::ReconnectOnly,
            },
            Snapshot::default(),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        assert!(runtime.queue_view().is_none());
        runtime.followups.push(super::super::Followup {
            id: 0,
            text: "Local follow-up".into(),
            attachments: Vec::new(),
        });
        assert!(runtime.queue_view().is_some());
        runtime.selected_subagent = Some("child".into());
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.capabilities.queue_remove = true;
        snapshot.capabilities.queue_start = true;
        snapshot.queued_messages = (0..chat::QueuedMessage::LIMIT)
            .map(|id| chat::QueuedMessage::new(id.to_string(), "Long queued prompt\n".repeat(100)))
            .collect();
        let renderer = crate::app::tests::headless_renderer();
        for width in [320.0, 400.0, 960.0] {
            let mut element = runtime.queue_view().unwrap();
            let mut tree = iced::advanced::widget::Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &iced::advanced::layout::Limits::new(
                    iced::Size::ZERO,
                    iced::Size::new(width, 1000.0),
                ),
            );
            assert_eq!(node.size().width, width);
            assert!(
                node.size().height < 240.0,
                "queue must leave room for transcript and composer: {:?}",
                node.size()
            );
        }
    }

    #[test]
    fn reasoning_uses_its_own_transcript_and_does_not_finish_on_another_thought() {
        let thought = |id: &str| {
            Arc::new(chat::Item {
                id: id.into(),
                role: chat::Role::Assistant,
                content: ItemContent::Text {
                    text: "Thinking".into(),
                    thinking: true,
                    phase: chat::Phase::Final,
                },
                at: None,
                started_at: None,
            })
        };
        let mut items = vec![thought("one"), thought("two")];
        assert!(!reasoning_finished(&items, true, "one"));
        assert!(reasoning_finished(&items, false, "one"));
        let mut answer = (*thought("answer")).clone();
        answer.content = ItemContent::Text {
            text: "Answer".into(),
            thinking: false,
            phase: chat::Phase::Final,
        };
        items.push(Arc::new(answer));
        assert!(reasoning_finished(&items, true, "one"));
    }

    #[test]
    fn activity_does_not_describe_prompts_completed_commands_or_older_pages_as_live_work() {
        let mut snapshot = Snapshot {
            busy: true,
            ..Snapshot::default()
        };
        snapshot.items.push(Arc::new(chat::Item {
            id: "user".into(),
            role: chat::Role::User,
            content: ItemContent::Text {
                text: "Prompt".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            started_at: None,
            at: None,
        }));
        assert_eq!(activity_label(&snapshot), "Working");
        Arc::make_mut(&mut snapshot.items[0]).role = chat::Role::Assistant;
        assert_eq!(activity_label(&snapshot), "Responding");
        Arc::make_mut(&mut snapshot.items[0]).content = ItemContent::Command {
            label: "Echo".into(),
            command: "echo hi".into(),
            working_directory: String::new(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            output: "hi".into(),
            exit_code: Some(0),
            duration_ms: None,
        };
        assert_eq!(activity_label(&snapshot), "Working");
        snapshot.has_later = true;
        assert_eq!(activity_label(&snapshot), "Working");
    }

    #[test]
    fn concrete_activity_is_inferred_from_streamed_items() {
        let mut snapshot = Snapshot {
            busy: true,
            detail: "Working".into(),
            ..Snapshot::default()
        };
        snapshot.items.push(
            chat::Item {
                id: "thought".into(),
                role: chat::Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Text {
                    text: "Planning".into(),
                    thinking: true,
                    phase: chat::Phase::Commentary,
                },
            }
            .into(),
        );
        assert_eq!(activity_label(&snapshot), "Thinking");
        if let ItemContent::Text { thinking, .. } =
            &mut Arc::make_mut(&mut snapshot.items[0]).content
        {
            *thinking = false;
        }
        assert_eq!(activity_label(&snapshot), "Responding");
    }

    /// The fingerprint of every exchange on screen, oldest first.
    fn fingerprints(runtime: &Runtime, dark: bool) -> Vec<u64> {
        runtime
            .turns
            .iter()
            .map(|turn| runtime.turn_fingerprint(turn, dark))
            .collect()
    }

    #[test]
    fn new_turn_activity_keeps_completed_historical_reasoning_retained() {
        let items = frame_cost::transcript(3);
        let (mut runtime, _handle) = frame_cost::live_runtime(&items);
        let idle = fingerprints(&runtime, true);
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        assert_eq!(idle, fingerprints(&runtime, true),
            "all fixture reasoning already has a later final answer");
    }

    #[test]
    fn busy_changes_invalidate_only_turns_containing_reasoning() {
        let mut items = frame_cost::transcript(2);
        items.retain(|item| !matches!(item.content, ItemContent::Text { thinking: true, .. }));
        for (id, role, thinking) in [
            ("reasoning-prompt", chat::Role::User, false),
            ("reasoning", chat::Role::Assistant, true),
        ] {
            items.push(chat::Item {
                id: id.into(),
                role,
                content: ItemContent::Text {
                    text: id.into(),
                    thinking,
                    phase: chat::Phase::Final,
                },
                started_at: None,
                at: None,
            });
        }
        let (mut runtime, _handle) = frame_cost::live_runtime(&items);
        let reasoning = runtime
            .turns
            .iter()
            .map(|turn| {
                turn.prose
                    .iter()
                    .any(|placed| placed.slot == turn::Slot::Thinking)
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning.iter().filter(|&&thinking| thinking).count(), 1);
        assert!(
            reasoning.len() >= 3,
            "historical work and the latest reasoning are grouped"
        );
        assert!(runtime.turns.iter().any(|turn| !turn.steps.is_empty()));
        let idle = fingerprints(&runtime, true);
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        let busy = fingerprints(&runtime, true);
        assert_eq!(
            idle.iter()
                .zip(&busy)
                .map(|(idle, busy)| idle != busy)
                .collect::<Vec<_>>(),
            reasoning,
        );
        assert!(!reasoning_finished(
            &runtime.snapshot.items,
            true,
            "reasoning"
        ));
        Arc::make_mut(&mut runtime.snapshot).busy = false;
        assert_eq!(idle, fingerprints(&runtime, true));
        assert!(reasoning_finished(
            &runtime.snapshot.items,
            false,
            "reasoning"
        ));

        // Even reasoning already followed by an answer keeps its busy
        // dependency; this change does not localize cross-item completion.
        let (mut history, _handle) = frame_cost::live_runtime(&frame_cost::transcript(1));
        let idle = fingerprints(&history, true);
        Arc::make_mut(&mut history.snapshot).busy = true;
        for (turn, (idle, busy)) in history
            .turns
            .iter()
            .zip(idle.iter().zip(fingerprints(&history, true)))
        {
            assert_eq!(
                *idle != busy,
                turn.prose
                    .iter()
                    .any(|placed| placed.slot == turn::Slot::Thinking)
            );
        }
    }

    #[test]
    fn copy_feedback_invalidates_only_the_messages_own_turns() {
        let items = (0..3)
            .flat_map(|index| {
                [
                    (chat::Role::User, format!("prompt-{index}")),
                    (chat::Role::Assistant, format!("answer-{index}")),
                ]
            })
            .map(|(role, id)| chat::Item {
                content: ItemContent::Text {
                    text: id.clone(),
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                id,
                role,
                started_at: None,
                at: None,
            })
            .collect::<Vec<_>>();
        let (mut runtime, _handle) = frame_cost::live_runtime(&items);
        assert_eq!(runtime.turns.len(), 3);
        let changed = |before: &[u64], after: &[u64]| {
            before
                .iter()
                .zip(after)
                .map(|(before, after)| before != after)
                .collect::<Vec<_>>()
        };
        let original = fingerprints(&runtime, true);
        let copied_at = Instant::now();
        runtime.copied = Some(("answer-0".into(), copied_at));
        let first = fingerprints(&runtime, true);
        assert_eq!(changed(&original, &first), [true, false, false]);

        // Repeating Copy refreshes its expiry but draws the same checkmark.
        runtime.copied = Some(("answer-0".into(), copied_at + Duration::from_millis(500)));
        assert_eq!(first, fingerprints(&runtime, true));

        // Moving feedback removes one checkmark and adds the other.
        runtime.copied = Some(("answer-1".into(), copied_at));
        let second = fingerprints(&runtime, true);
        assert_eq!(changed(&first, &second), [true, true, false]);
        runtime.copied = None;
        let cleared = fingerprints(&runtime, true);
        assert_eq!(changed(&second, &cleared), [false, true, false]);
        assert_eq!(original, cleared);

        // An item outside this transcript cannot change any visible badge.
        runtime.copied = Some(("retired-item".into(), copied_at));
        assert_eq!(original, fingerprints(&runtime, true));
    }

    /// Publishes `items` to `runtime` as a provider frame would.
    fn publish(
        runtime: &mut Runtime,
        handle: &ClientHandle,
        items: &[chat::Item],
        generation: u64,
    ) {
        handle.publish_snapshot_for_test(Snapshot {
            generation,
            ready: true,
            items: items.iter().cloned().map(Arc::new).collect(),
            ..Snapshot::default()
        });
        runtime.refresh();
    }

    #[test]
    fn prose_only_refresh_keeps_prompt_strings_and_turn_group_allocations() {
        let mut items = frame_cost::transcript(1);
        let (mut runtime, handle) = frame_cost::live_runtime(&items);
        let prompt = runtime.history[0].as_ptr();
        let turns = runtime.turns.as_ptr();
        let prose = runtime.turns.last().unwrap().prose.as_ptr();
        let ItemContent::Text { text, .. } = &mut items.last_mut().unwrap().content else {
            unreachable!()
        };
        text.push_str(" More streamed text.");
        publish(&mut runtime, &handle, &items, 20);
        assert_eq!(
            runtime.history[0].as_ptr(),
            prompt,
            "unchanged history strings are not cloned"
        );
        assert_eq!(
            runtime.turns.as_ptr(),
            turns,
            "unchanged placement is not regrouped"
        );
        assert_eq!(runtime.turns.last().unwrap().prose.as_ptr(), prose);
    }

    #[test]
    fn refresh_reparses_tail_colliding_rewrites_and_invalidates_retained_turns() {
        let body =
            |prefix: &str| format!("```text\n{}{}\n```\n", prefix.repeat(80), "z".repeat(80));
        let mut items = vec![chat::Item {
            id: "answer".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: body("a"),
                thinking: false,
                phase: chat::Phase::Final,
            },
            started_at: None,
            at: None,
        }];
        let (mut runtime, handle) = frame_cost::live_runtime(&items);
        let before = fingerprints(&runtime, true);
        let token = runtime.cache["answer"].token;
        let revision = runtime.cache["answer"].revision;
        // A new immutable Arc with identical content keeps its prepared presentation.
        publish(&mut runtime, &handle, &items, 1);
        assert_eq!(revision, runtime.cache["answer"].revision);
        assert_eq!(before, fingerprints(&runtime, true));
        items[0].content = ItemContent::Text {
            text: body("b"),
            thinking: false,
            phase: chat::Phase::Final,
        };
        assert_eq!(token, super::super::ChangeToken::of(&items[0].content));
        publish(&mut runtime, &handle, &items, 2);
        assert_ne!(before, fingerprints(&runtime, true));
        let codes = |runtime: &Runtime| {
            let mut codes = Vec::new();
            super::super::for_each_fence(
                runtime.cache["answer"].markdown.items(),
                &mut |_, code| {
                    codes.push(code.to_owned());
                },
            );
            codes
        };
        assert_eq!(
            codes(&runtime),
            vec![format!("{}{}\n", "b".repeat(80), "z".repeat(80))]
        );
        // Rewriting an earlier prefix and appending is not an incremental append either.
        items[0].content = ItemContent::Text {
            text: format!("{}\nMore", body("c")),
            thinking: false,
            phase: chat::Phase::Final,
        };
        publish(&mut runtime, &handle, &items, 3);
        assert_eq!(
            codes(&runtime),
            vec![format!("{}{}\n", "c".repeat(80), "z".repeat(80))]
        );
    }

    #[test]
    fn refresh_replaces_expanded_tool_text_after_a_tail_colliding_rewrite() {
        let mut items = vec![chat::Item {
            id: "tool".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "shell".into(),
                input: "ls".into(),
                output: format!("{}{}", "a".repeat(80), "z".repeat(80)),
                status: chat::ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        }];
        let (mut runtime, handle) = frame_cost::live_runtime(&items);
        runtime.expanded.insert("tool".into());
        // First update creates the expanded presentation.
        if let ItemContent::Tool { input, .. } = &mut items[0].content {
            *input = "pwd".into();
        }
        publish(&mut runtime, &handle, &items, 1);
        let token = runtime.cache["tool"].token;
        let before = fingerprints(&runtime, true);
        let expected = format!("{}{}", "b".repeat(80), "z".repeat(80));
        if let ItemContent::Tool { output, .. } = &mut items[0].content {
            *output = expected.clone();
        }
        assert_eq!(token, super::super::ChangeToken::of(&items[0].content));
        publish(&mut runtime, &handle, &items, 2);
        assert_eq!(
            runtime.cache["tool"]
                .tool_output
                .as_ref()
                .unwrap()
                .text
                .as_ref(),
            expected
        );
        assert_ne!(before, fingerprints(&runtime, true));
    }

    #[test]
    fn image_handle_changes_invalidate_retained_turns_even_without_a_loading_frame() {
        let items = vec![chat::Item {
            id: "image".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Image {
                path: "/image.png".into(),
                description: "Image".into(),
            },
            started_at: None,
            at: None,
        }];
        let (mut runtime, _) = frame_cost::live_runtime(&items);
        let cached = runtime.cache.get_mut("image").unwrap();
        cached.image_source = None;
        cached.image = Some(iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4]));
        let before = fingerprints(&runtime, true);
        runtime.cache.get_mut("image").unwrap().image =
            Some(iced::widget::image::Handle::from_rgba(1, 1, vec![255; 4]));
        assert_ne!(before, fingerprints(&runtime, true));
    }

    #[test]
    fn tool_heartbeats_do_not_rebuild_the_turn_within_an_elapsed_second() {
        let mut items = vec![chat::Item {
            id: "tool".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "Bash".into(),
                input: "sleep 5".into(),
                output: String::new(),
                status: chat::ToolStatus::Running,
            },
            started_at: Some(1000),
            at: Some(1500),
        }];
        let (mut runtime, handle) = frame_cost::live_runtime(&items);
        let fingerprint = runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(2500));
        items[0].at = Some(1800);
        publish(&mut runtime, &handle, &items, 1);
        assert_eq!(
            fingerprint,
            runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(2900))
        );
        assert_ne!(
            fingerprint,
            runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(3000))
        );
        if let ItemContent::Tool { status, .. } = &mut items[0].content {
            *status = chat::ToolStatus::Succeeded;
        }
        items[0].at = Some(3500);
        publish(&mut runtime, &handle, &items, 2);
        assert_ne!(
            fingerprint,
            runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(4000)),
            "a tool's settled status and duration still invalidate its turn",
        );
        assert_eq!(
            runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(4000)),
            runtime.turn_fingerprint_at(&runtime.turns[0], true, Some(60000)),
            "completed durations stay fixed",
        );
    }

    /// A streaming answer is the only exchange that changes, so it is the
    /// only one drawn again; the ones above it keep what they drew.
    #[test]
    fn only_the_streaming_exchange_is_drawn_again() {
        let mut items = frame_cost::transcript(3);
        let (mut runtime, handle) = frame_cost::live_runtime(&items);
        let before = fingerprints(&runtime, true);
        assert!(before.len() >= 3);
        let last = items.len() - 1;
        let ItemContent::Text { text, .. } = &mut items[last].content else {
            unreachable!()
        };
        text.push_str(" and one more word");
        publish(&mut runtime, &handle, &items, 1);
        let after = fingerprints(&runtime, true);
        assert_eq!(before[..before.len() - 1], after[..after.len() - 1]);
        assert_ne!(before.last(), after.last());
        // A republished snapshot with nothing new changes nothing.
        publish(&mut runtime, &handle, &items, 2);
        assert_eq!(fingerprints(&runtime, true), after);
    }

    /// Anything a retained exchange draws besides its items is part of what
    /// decides whether it is drawn again.
    #[test]
    fn an_exchange_is_drawn_again_when_its_presentation_changes() {
        let items = frame_cost::transcript(1);
        let (mut runtime, _handle) = frame_cost::live_runtime(&items);
        let strip = runtime
            .turns
            .iter()
            .position(|turn| !turn.steps.is_empty())
            .expect("a group with steps");
        let fingerprint = |runtime: &Runtime, dark| fingerprints(runtime, dark)[strip];
        let light = fingerprint(&runtime, false);
        assert_ne!(light, fingerprint(&runtime, true), "the appearance");
        let key = runtime.turns[strip].key.clone();
        let open = runtime
            .opened
            .get(&key)
            .copied()
            .unwrap_or(runtime.tool_groups_expanded);
        runtime.opened.insert(key, !open);
        let toggled = fingerprint(&runtime, false);
        assert_ne!(light, toggled, "the strip opening or closing");
        runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::SolarizedDark);
        assert_ne!(toggled, fingerprint(&runtime, false), "the syntax theme");
    }

    /// Open details own their content, and remain reusable until a rendered
    /// input changes. Disclosure itself must change that key.
    #[test]
    fn an_open_steps_retained_key_changes_once_for_its_disclosure() {
        let items = frame_cost::transcript(1);
        let (mut runtime, _handle) = frame_cost::live_runtime(&items);
        let strip = runtime
            .turns
            .iter()
            .position(|turn| !turn.steps.is_empty())
            .expect("a group with steps");
        let closed = runtime.turn_fingerprint(&runtime.turns[strip], true);
        let step = runtime.turns[strip].steps[0].index;
        runtime
            .expanded
            .insert(runtime.snapshot.items[step].id.clone());
        let opened = runtime.turn_fingerprint(&runtime.turns[strip], true);
        assert_ne!(closed, opened);
        assert_eq!(
            opened,
            runtime.turn_fingerprint(&runtime.turns[strip], true)
        );
    }

    /// Every kind of Markdown block an answer can hold is drawn by the
    /// transcript's own renderer, including those inside lists and quotes.
    #[test]
    fn every_markdown_block_is_drawn() {
        let cached = CachedItem::new(
            &ItemContent::Text {
                text: "# Title\n\nA [link](https://example.com) and `code`.\n\n\
                       - one\n- [x] done\n\n3. three\n4. four\n\n> quoted\n>\n> ```sh\n> ls\n> ```\n\n\
                       ---\n\n| a | b |\n|:-:|--:|\n| 1 | 2 |\n\n![alt](image.png)\n\n\
                       ```rust\nfn main() {}\n```\n"
                    .into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            crate::settings::EditorSyntaxTheme::Auto,
        );
        assert_eq!(
            cached.blocks.len(),
            2,
            "the quoted fence and the top-level one"
        );
        let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_PROSE);
        let renderer = crate::app::tests::headless_renderer();
        let mut element = prose::view(
            &cached,
            settings,
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
        );
        let mut tree = iced::advanced::widget::Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &iced::advanced::layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 4_000.0)),
        );
        // Heading, paragraph, both lists, the quote, the rule, the table, the
        // image and the fence, each laid out with room of its own.
        assert_eq!(node.children().len(), 9);
        assert!(
            node.children()
                .iter()
                .all(|block| block.size().width > 0.0 && block.size().height > 0.0)
        );
    }

    #[test]
    fn up_reaches_the_queue_from_the_first_line_and_alt_up_from_anywhere() {
        use keyboard::Modifiers;
        use keyboard::key::Named;
        for (named, modifiers, first_line, expected) in [
            (Named::ArrowUp, Modifiers::empty(), true, true),
            (Named::ArrowUp, Modifiers::empty(), false, false),
            (Named::ArrowUp, Modifiers::ALT, false, true),
            (Named::ArrowUp, Modifiers::ALT, true, true),
            // Selecting upward and jumping the caret stay the editor's.
            (Named::ArrowUp, Modifiers::SHIFT, true, false),
            (Named::ArrowUp, Modifiers::CTRL, true, false),
            (
                Named::ArrowUp,
                Modifiers::ALT | Modifiers::SHIFT,
                true,
                false,
            ),
            (Named::ArrowDown, Modifiers::ALT, true, false),
        ] {
            assert_eq!(
                queue_key(&key(named, modifiers), first_line),
                expected,
                "{named:?} {modifiers:?} first_line={first_line}"
            );
        }
        let mut unfocused = key(Named::ArrowUp, Modifiers::empty());
        unfocused.status = text_editor::Status::Active;
        assert!(!queue_key(&unfocused, true));
    }

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

    #[test]
    fn scrollable_short_pages_have_separate_pagination_edges() {
        for travel in [0.01, 1.0, 24.0, 48.0, 96.0] {
            let content = 400.0 + travel;
            let top = scroll(0.0, content, 400.0);
            assert!(top.at_top() && !top.at_bottom());
            let bottom = scroll(content - 400.0, content, 400.0);
            assert!(bottom.at_bottom() && !bottom.at_top());
            let middle = scroll((content - 400.0) * 0.5, content, 400.0);
            assert!(!middle.at_top() && !middle.at_bottom());
        }
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
            kind: crate::chat::RequestKind::Approval,
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

        // Ordinary question forms can submit on Enter even when cancellation
        // is available. Their explicit semantic kind prevents granting permission.
        let questions = chat::Request {
            kind: chat::RequestKind::Question,
            fields: vec![chat::Field {
                id: "answer".into(),
                label: "Question".into(),
                description: String::new(),
                choices: vec![],
                multiple: false,
                secret: false,
                initial: String::new(),
            }],
            actions: vec![choice("submit"), choice("cancel")],
            ..permission
        };
        assert_eq!(
            submit_answer(&questions).map(|choice| choice.id.as_str()),
            Some("submit")
        );
        assert_eq!(
            primary_answer(&questions).map(|choice| choice.id.as_str()),
            Some("submit")
        );
        assert_eq!(
            ordered_answers(&questions)
                .iter()
                .map(|choice| choice.id.as_str())
                .collect::<Vec<_>>(),
            vec!["submit", "cancel"]
        );
        let yes_no = chat::Request {
            fields: vec![],
            ..questions.clone()
        };
        assert!(
            submit_answer(&yes_no).is_none(),
            "fieldless confirmation requires an explicit click"
        );
        assert_eq!(
            primary_answer(&yes_no).map(|choice| choice.id.as_str()),
            Some("submit")
        );
        for kind in [chat::RequestKind::Approval, chat::RequestKind::Consent] {
            let explicit = chat::Request {
                kind,
                ..questions.clone()
            };
            assert!(submit_answer(&explicit).is_none());
            assert!(primary_answer(&explicit).is_none());
            assert_eq!(ordered_answers(&explicit)[0].id, "cancel");
        }
        // Claude's transport retains allow/deny while the product is a question.
        let claude = chat::Request {
            actions: vec![choice("allow"), choice("deny")],
            ..questions
        };
        assert_eq!(
            primary_answer(&claude).map(|choice| choice.id.as_str()),
            Some("allow")
        );
        assert_eq!(
            submit_answer(&claude).map(|choice| choice.id.as_str()),
            Some("allow")
        );
        assert_eq!(ordered_answers(&claude)[0].id, "allow");
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
    #[test]
    fn file_change_review_fits_narrow_and_wide_panes_and_names_delegated_proposals() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
        let key = chat::FileChangeKey {
            source: chat::FileChangeSource::Subagent { id: "child".into() },
            path: "src/renamed.rs".into(),
        };
        let mut snapshot = (*runtime.snapshot).clone();
        snapshot.file_changes = Arc::new(chat::FileChangesSummary {
            total_files: 1,
            coverage: chat::FileChangesCoverage {
                has_unloaded_history: true,
                has_imported_history: true,
                has_incomplete_delegated_history: true,
                ..chat::FileChangesCoverage::default()
            },
            files: vec![chat::FileChangeSummary {
                key: key.clone(),
                resolved_path: None,
                ownership_unverified: true,
                revision: 1,
                kind: chat::ChangeKind::Renamed {
                    from: "src/old.rs".into(),
                },
                succeeded: 0,
                pending: 1,
                failed: 0,
                additions: None,
                deletions: None,
                imported: 1,
            }],
            ..chat::FileChangesSummary::default()
        });
        runtime.snapshot = Arc::new(snapshot);
        runtime.changes.open = true;
        let renderer = crate::app::tests::headless_renderer();
        for width in [360.0, 670.0, 960.0] {
            runtime.changes.back();
            let mut element = runtime.changes_view(width);
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
            );
            assert!(
                node.size().width <= width,
                "review must fit a narrow split pane"
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            let labels: Vec<_> = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter_map(|node| node.node().label().map(str::to_owned))
                .collect();
            for expected in [
                "Files changed",
                "Proposed edit",
                "Subagent: child",
                "pending",
                "from src/old.rs",
                "Close",
            ] {
                assert!(
                    labels.iter().any(|label| label.contains(expected)),
                    "{width}: missing {expected}: {labels:?}"
                );
            }
            assert!(
                !labels.iter().any(|label| label == "Open file"),
                "unresolved child-relative paths must not route to the parent"
            );
            drop(element);
            runtime.changes.select(key.clone());
            let mut selected = runtime.changes_view(width);
            let mut selected_tree = Tree::new(selected.as_widget());
            let selected_node = selected.as_widget_mut().layout(
                &mut selected_tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
            );
            let selected_accessible = selected.as_widget().a11y_nodes(
                Layout::new(&selected_node),
                &selected_tree,
                iced::mouse::Cursor::Unavailable,
            );
            let nodes: Vec<_> = selected_accessible
                .root()
                .iter()
                .chain(selected_accessible.children())
                .collect();
            if width < 560.0 {
                assert!(
                    nodes
                        .iter()
                        .any(|node| node.node().label() == Some("Back to files")),
                    "narrow detail view needs a visible return to the file list"
                );
                assert!(
                    !nodes.iter().any(|node| node
                        .node()
                        .label()
                        .is_some_and(|label| label.starts_with("View reported edits for"))),
                    "narrow panes switch to the detail instead of stacking both scrollers"
                );
            } else {
                let file = nodes
                    .iter()
                    .find(|node| {
                        node.node()
                            .label()
                            .is_some_and(|label| label.starts_with("View reported edits for"))
                    })
                    .and_then(|node| node.node().bounds())
                    .expect("file list stays mounted beside the detail");
                let detail = nodes
                    .iter()
                    .find(|node| node.node().label() == Some("The file location was not reported."))
                    .and_then(|node| node.node().bounds())
                    .expect("selected file detail is mounted");
                assert!(
                    file.x1 <= detail.x0,
                    "{width}: default-width files and edits must sit side by side"
                );
            }
            drop(selected);
            let shortcuts = crate::settings::KeyboardShortcuts::default();
            let mut full = runtime.view(
                None,
                false,
                true,
                &shortcuts,
                width,
                Place {
                    name: "fixture".into(),
                    branch: None,
                    channel: None,
                },
                true,
                None,
                Vec::new(),
            );
            let mut full_tree = Tree::new(full.as_widget());
            let full_node = full.as_widget_mut().layout(
                &mut full_tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
            );
            let accessible = full.as_widget().a11y_nodes(
                Layout::new(&full_node),
                &full_tree,
                iced::mouse::Cursor::Unavailable,
            );
            assert!(
                !accessible.root().iter().chain(accessible.children()).any(|node|
                    node.node().label().is_some_and(|name| name.starts_with("Reported edits 1"))),
                "file review must not add a permanent footer action"
            );
            for label in ["Send message", "Close"] {
                let bounds = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .find(|node| {
                        node.node()
                            .label()
                            .is_some_and(|name| name.starts_with(label))
                    })
                    .and_then(|node| node.node().bounds())
                    .unwrap_or_else(|| panic!("{width}: {label} must remain mounted"));
                assert!(
                    bounds.y0 >= 0.0 && bounds.y1 <= 600.0 && bounds.y1 > bounds.y0,
                    "{width}: {label} must remain visible in a 600px window: {bounds:?}"
                );
            }
        }
    }
    #[test]
    fn narrow_file_review_restores_actual_scroll_on_back_after_the_old_ui_operation_misses() {
        use iced::advanced::{
            Layout, layout,
            widget::{Id, Operation, Tree},
        };
        struct Offset {
            id: Id,
            y: Option<f32>,
        }
        impl Operation for Offset {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&Id>,
                _: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn iced::advanced::widget::operation::scrollable::Scrollable,
            ) {
                if id == Some(&self.id) {
                    self.y = Some(translation.y);
                }
            }
        }
        let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
        let key = chat::FileChangeKey {
            source: chat::FileChangeSource::Conversation,
            path: "src/file-036.rs".into(),
        };
        let rows = (0..128)
            .map(|index| chat::FileChangeSummary {
                key: chat::FileChangeKey {
                    source: chat::FileChangeSource::Conversation,
                    path: format!("src/file-{index:03}.rs"),
                },
                resolved_path: Some(format!("/project/src/file-{index:03}.rs")),
                revision: 1,
                kind: chat::ChangeKind::Updated,
                succeeded: 1,
                pending: 0,
                failed: 0,
                additions: Some(1),
                deletions: Some(1),
                imported: 0,
                ownership_unverified: false,
            })
            .collect();
        let summary = chat::FileChangesSummary {
            request_id: 42,
            revision: 1,
            files: rows,
            total_files: 128,
            applied_files: 128,
            ..Default::default()
        };
        Arc::make_mut(&mut runtime.snapshot).file_changes = Arc::new(summary.clone());
        runtime.changes.list = Some(summary);
        runtime.changes.show();
        runtime.changes.list_scroll = 1000.0;
        let original_epoch = runtime.changes.list_epoch;
        let id = card_control_id(runtime.target.card, "chat-changes-files");
        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(360.0, 600.0));
        let mut files = runtime.changes_view(360.0);
        let mut tree = Tree::new(files.as_widget());
        let node = files.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut offset = Offset {
            id: id.clone(),
            y: None,
        };
        files
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut offset);
        assert_eq!(offset.y, Some(1000.0));
        drop(files);
        runtime.changes.select(key);
        runtime
            .changes
            .record_scroll(42, 42, original_epoch, true, 0.0);
        assert_eq!(
            runtime.changes.list_scroll, 1000.0,
            "a stale retiring list callback cannot overwrite the saved offset"
        );
        let mut detail = runtime.changes_view(360.0);
        tree.diff(detail.as_widget_mut());
        let node = detail.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut early = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
            id.clone(),
            iced::widget::scrollable::AbsoluteOffset {
                x: None,
                y: Some(1000.0),
            },
        );
        detail
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut early);
        let mut absent = Offset {
            id: id.clone(),
            y: None,
        };
        detail
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut absent);
        assert_eq!(
            absent.y, None,
            "an operation before list remount cannot restore it"
        );
        drop(detail);
        runtime.changes.back();
        runtime
            .changes
            .record_scroll(42, 42, original_epoch, true, 0.0);
        assert_eq!(
            runtime.changes.list_scroll, 1000.0,
            "late callbacks remain stale after Back makes the list visible again"
        );
        let mut files = runtime.changes_view(360.0);
        tree.diff(files.as_widget_mut());
        let node = files.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut restored = Offset {
            id: id.clone(),
            y: None,
        };
        files
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut restored);
        assert_eq!(
            restored.y,
            Some(1000.0),
            "first remount layout restores the real widget offset without any follow-up task"
        );
        let mut moved = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
            id.clone(),
            iced::widget::scrollable::AbsoluteOffset {
                x: None,
                y: Some(1100.0),
            },
        );
        files
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut moved);
        let node = files.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut unchanged = Offset { id, y: None };
        files
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut unchanged);
        assert_eq!(
            unchanged.y,
            Some(1100.0),
            "later layouts cannot reset newer widget scroll before its callback arrives"
        );
    }
    #[test]
    fn busy_composer_escape_dismisses_actions_before_interrupting() {
        use iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};

        let renderer = crate::app::tests::headless_renderer();
        let viewport = iced::Rectangle::with_size(iced::Size::new(620.0, 720.0));
        for (actions_open, fork_open) in [(false, false), (true, false), (true, true)] {
            let (mut runtime, _handle) = frame_cost::live_runtime(&[]);
            let snapshot = Arc::make_mut(&mut runtime.snapshot);
            snapshot.ready = true;
            snapshot.busy = true;
            snapshot.capabilities.branch = true;
            snapshot.capabilities.compact = true;
            runtime.composer_actions = actions_open;
            // An already-open fork picker can remain visible as a turn starts.
            runtime.branch_picker = fork_open;
            let target = runtime.target;
            let shortcuts = crate::settings::KeyboardShortcuts::default();
            let mut element = runtime.view(
                None,
                false,
                true,
                &shortcuts,
                viewport.width,
                Place {
                    name: "fixture".into(),
                    branch: None,
                    channel: None,
                },
                true,
                None,
                Vec::new(),
            );
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, viewport.size()),
            );
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                    target.card,
                    "chat-composer",
                )),
            );
            let escape = iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &escape,
                Layout::new(&node),
                iced::mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            if actions_open {
                assert!(messages.iter().any(|message| matches!(
                    message,
                    Message::ChatUi(Action::ComposerActions(actual, false)) if *actual == target
                )));
                assert!(!messages.iter().any(|message| matches!(
                    message,
                    Message::ChatUi(Action::Interrupt(..))
                )));
            } else {
                assert!(
                    messages.iter().any(|message| matches!(
                        message,
                        Message::ChatUi(Action::Interrupt(actual)) if *actual == target
                    )),
                    "the focused busy composer otherwise handles Escape as interruption"
                );
            }
        }
    }

    #[test]
    fn switching_review_and_fork_popover_keeps_the_composer_visible_at_600px() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let (mut live, _handle) = frame_cost::live_runtime(&[]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let card = app.workspace.focused().unwrap();
        live.target.card = card;
        let target = live.target;
        let snapshot = Arc::make_mut(&mut live.snapshot);
        snapshot.ready = true;
        snapshot.capabilities.branch = true;
        Arc::make_mut(&mut snapshot.file_changes).total_files = 1;
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(live.session.clone());
        app.cards.insert(card, CardRuntime::Chat(Box::new(live)));
        let renderer = crate::app::tests::headless_renderer();
        for mode in [
            Action::ChangesPicker(target, true),
            Action::BranchPicker(target, true),
            Action::ChangesPicker(target, true),
        ] {
            drop(app.handle_chat_ui(mode));
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("fixture chat");
            };
            assert_ne!(
                runtime.changes.open, runtime.branch_picker,
                "only the selected inline panel is open"
            );
            for width in [360.0, 670.0] {
                let mut element = runtime.view(
                    None,
                    false,
                    true,
                    &app.settings.keyboard_shortcuts,
                    width,
                    Place {
                        name: "fixture".into(),
                        branch: None,
                        channel: None,
                    },
                    true,
                    None,
                    Vec::new(),
                );
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
                );
                let accessible = element.as_widget().a11y_nodes(
                    Layout::new(&node),
                    &tree,
                    iced::mouse::Cursor::Unavailable,
                );
                let bounds = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .find(|node| {
                        node.node()
                            .label()
                            .is_some_and(|label| label.starts_with("Send message"))
                    })
                    .and_then(|node| node.node().bounds())
                    .expect("composer send control remains mounted");
                assert!(
                    bounds.y0 >= 0.0 && bounds.y1 <= 600.0 && bounds.y1 > bounds.y0,
                    "{width}: inline panels must not push the composer out of the window: {bounds:?}"
                );
            }
        }
    }
}
