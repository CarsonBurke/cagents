use super::coordination::Coordinated;
use super::*;

#[derive(Clone, Copy)]
enum ViewTransition {
    Idle,
    FirstStart,
    /// A new provider is starting on the conversation of one that stopped.
    Resuming,
    SessionChange,
}

impl From<Option<&Phase>> for ViewTransition {
    fn from(phase: Option<&Phase>) -> Self {
        match phase {
            None => Self::Idle,
            Some(Phase::SavingDraft { .. }) => Self::FirstStart,
            Some(Phase::Starting(attempt)) if attempt.intent == chat::Intent::Resume => {
                Self::Resuming
            }
            Some(_) => Self::SessionChange,
        }
    }
}

impl From<bool> for ViewTransition {
    fn from(transitioning: bool) -> Self {
        if transitioning {
            Self::SessionChange
        } else {
            Self::Idle
        }
    }
}

/// Bind a link to the surface that rendered it, before another pane, project,
/// or child conversation can become selected.
pub(super) fn capture_document_link(
    message: Message,
    target: Target,
    document: &Option<PathBuf>,
) -> Message {
    match message {
        Message::ChatUi(Action::OpenDocument(found, raw)) if found == target => {
            action(Action::OpenDocumentFrom(target, document.clone(), raw))
        }
        Message::ChatUi(Action::OpenSharedDocument(found, raw)) if found == target => action(
            Action::OpenDocumentFrom(target, document.clone(), raw.to_string()),
        ),
        message => message,
    }
}

pub(super) fn body<'a>(text: impl Into<Cow<'a, str>> + 'a) -> Element<'a, Message> {
    widget::selectable_text(text)
        .size(design::TEXT_BODY)
        .width(Length::Fill)
        .into()
}

fn transcript_body(text: impl Into<String>, target: Target) -> Element<'static, Message> {
    widget::container(selectable_prose::plain(
        text,
        design::TEXT_TRANSCRIPT,
        iced::Font::default(),
        target,
    ))
    .width(Length::Fill)
    .into()
}

/// The exchanges [`Runtime::transcript_preview`] draws, from the end.
const PREVIEW_TURNS: usize = 8;

/// The composer's own line height and inner padding.
///
/// Named because the composer's height is computed from the draft rather than
/// fixed, so the two constants have to agree with what the editor draws.
/// iced's default line height, 1.4 em, at the editor's prose size.
const COMPOSER_LINE_HEIGHT: f32 = design::TEXT_PROSE * 1.4;
/// Where the composer's glyphs and captions start: the `+` glyph, and the
/// suggestion hint and footer aligned beneath it.
const COMPOSER_PADDING: f32 =
    COMPOSER_TOOLBAR_PADDING + (design::PRIMARY_CONTROL_SIZE - design::ROW_ICON_SIZE as f32) / 2.0;
/// The controls' inset from the composer's sides, the same as from its foot,
/// so the hover square of `+` or send sits evenly in the composer's corner,
/// concentric with it.
const COMPOSER_TOOLBAR_PADDING: f32 = COMPOSER_CONTROL_INSET;
/// The editor's vertical inset in the composer row.
///
/// A single line of text is then as tall as a control with a small margin on
/// either side, so the row is no taller than its buttons need.
const COMPOSER_INSET_Y: f32 =
    (design::PRIMARY_CONTROL_SIZE + 2.0 * space::XS - COMPOSER_LINE_HEIGHT) / 2.0;
/// How far a scrolled draft runs on into the editor's vertical inset (#2824):
/// up to the composer's border, never over it.
const COMPOSER_TEXT_REACH: f32 = COMPOSER_INSET_Y - design::BORDER_WIDTH;
/// The composer editor's padding: its vertical inset, and its sides with the
/// rail for the vertical scrollbar.
fn composer_editor_padding() -> iced::Padding {
    design::with_vertical_rail(iced::Padding::from([COMPOSER_INSET_Y, space::XS]))
}
/// How far a composer control sits above the row's foot: centered on a single
/// line of text, and beside the last line of a longer one.
const COMPOSER_CONTROL_INSET: f32 =
    COMPOSER_INSET_Y + (COMPOSER_LINE_HEIGHT - design::PRIMARY_CONTROL_SIZE) / 2.0;

/// The widest a user's message is drawn.
///
/// Narrower than the measure prose is held to, because a prompt is an aside
/// in the conversation rather than the body of it: keeping it clearly short
/// of the full column is what makes the alternation of asking and answering
/// visible while scrolling.
const PROMPT_MEASURE: f32 = 560.0;

/// The preview height of messages inside coordinating tool cards.
pub(super) const DELIVERY_FOLDED_HEIGHT: f32 = 180.0;
/// The lines of a notice's event shown before it is opened.
pub(super) const NOTICE_LINES: usize = 3;
/// The characters of a notice's event shown before it is opened, so a single
/// line too long to read at a glance cannot wrap down the transcript either.
pub(super) const NOTICE_CHARS: usize = 480;

const MESSAGE_ACTION_HEIGHT: f32 = 24.0;
/// The most characters a footer name shows before it is cut.
const FOOTER_LABEL: usize = 32;
/// The same, in a pane too narrow for the conversation's place.
const NARROW_FOOTER_LABEL: usize = 20;
/// The same where the model is named only on hover, for the text drawn in
/// its place while the session switches.
const TIGHT_FOOTER_LABEL: usize = 14;
const MESSAGE_ACTION_FOOTER: f32 = MESSAGE_ACTION_HEIGHT + step::GAP_XS;
/// What one row of the timeline draws: its line box and the padding above
/// and below it. Rows are [`step::GAP_XS`] apart, so this plus that gap is
/// the timeline's pitch.
const TIMELINE_ROW: f32 = design::LINE_TRANSCRIPT + 2.0 * step::GAP_XS;

/// How tall a row is taken to be until it is first measured, so a long
/// transcript scrolls without the rows below a mounted one jumping: one
/// timeline row for a line of the timeline, and otherwise its prose, as
/// many lines as its length would fill at `width`, set as answers are.
/// Either way with the room its actions reserve beneath it.
fn row_estimate(item: &chat::Item, rhythm: Rhythm, footer: f32, width: f32) -> f32 {
    let drawn = match (rhythm, CachedItem::prose(&item.content)) {
        (Rhythm::Line, _) | (_, None) => TIMELINE_ROW,
        (_, Some(body)) => {
            // About half an em a character, on average, at the prose size.
            let per_line = (width / (design::TEXT_TRANSCRIPT / 2.0)).max(20.0);
            let line = iced::widget::text::LineHeight::default()
                .to_absolute(design::TEXT_TRANSCRIPT.into())
                .0;
            (body.len() as f32 / per_line).ceil().max(1.0) * line
        }
    };
    drawn + footer
}

/// What a transcript row is to the rows around it, which alone decides the
/// space above it.
///
/// The space is a property of the two rows it separates, never of the group
/// or exchange either belongs to, so a column of timeline rows keeps one
/// pitch whether or not a group boundary, or an item that draws nothing,
/// falls between two of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rhythm {
    /// One line of the timeline: a step, the strip that folds a group of
    /// them, a thought, an agent started or messaged.
    Line,
    /// Something read as a whole: an answer, narration, an image.
    Block,
    /// What sets itself apart: the message that opens an exchange, or a part
    /// of the transcript of its own, such as a failure or the closing line.
    Opening,
}

impl Rhythm {
    /// The space between a row of this kind and a row of `next` drawn
    /// beneath it.
    fn gap(self, next: Rhythm) -> f32 {
        match (self, next) {
            (_, Rhythm::Opening) => step::GAP_L,
            (Rhythm::Line, Rhythm::Line) => step::GAP_XS,
            _ => step::GAP_M,
        }
    }

    /// How a transcript item draws, by its role, slot and what it holds.
    fn of(item: &chat::Item, slot: turn::Slot, card: bool) -> Self {
        match (&item.content, slot) {
            _ if card => Self::Line,
            (ItemContent::Text { .. }, turn::Slot::Thinking) => Self::Line,
            (ItemContent::Text { .. } | ItemContent::Image { .. }, _) => Self::Block,
            // A step, or a notice the grouping placed among the prose, is
            // drawn as a step's row.
            _ => Self::Line,
        }
    }
}

/// The loaded transcript keeps row descriptions; only its visible rows create
/// widget trees, and a visible row is kept between frames by retained::kept.
///
/// Existing action footers provide separation. Count that space toward the
/// next semantic boundary instead of adding another gap after an invisible row.
struct VirtualRows<'a> {
    children: Vec<virtualized::Row<'a>>,
    /// The kind of the last row drawn, which the next row's gap is measured
    /// from. A row that draws nothing leaves it as it was.
    last: Option<Rhythm>,
    trailing_footer: f32,
}
impl<'a> VirtualRows<'a> {
    fn new() -> Self {
        Self {
            children: vec![],
            last: None,
            trailing_footer: 0.0,
        }
    }
    /// A part of the transcript of its own, set apart from what is above it.
    fn push(&mut self, row: Element<'a, Message>) -> usize {
        self.append(
            virtualized::Row::once(0, 0, 80.0, None, move || row),
            Some(Rhythm::Opening),
            0.0,
        )
    }
    fn lazy(
        &mut self,
        key: u64,
        revision: u64,
        estimate: f32,
        aliases: Vec<iced::advanced::widget::Id>,
        rhythm: Option<Rhythm>,
        footer: f32,
        build: impl Fn() -> Element<'a, Message> + 'a,
    ) -> usize {
        let mut row = virtualized::Row::new(key, revision, estimate, None, build);
        row.aliases = aliases;
        self.append(row, rhythm, footer)
    }
    /// Lets the newest row also stand for `id`, which draws nothing of its
    /// own.
    fn alias_last(&mut self, id: iced::advanced::widget::Id) {
        if let Some(row) = self.children.last_mut() {
            row.aliases.push(id);
        }
    }
    /// Appends `row`, after the gap its kind takes from the last row drawn.
    /// A row without a kind draws nothing: it takes no gap, and the next row
    /// is spaced from the one before it as if it were not there.
    fn append(
        &mut self,
        mut row: virtualized::Row<'a>,
        rhythm: Option<Rhythm>,
        footer: f32,
    ) -> usize {
        if let (Some(last), Some(next)) = (self.last, rhythm) {
            let gap = (last.gap(next) - self.trailing_footer).max(0.0);
            if gap > 0.0 {
                // A gap is keyed by the row it precedes, so history loaded
                // above it does not hand it another gap's measured height.
                let key = match row.key {
                    0 => self.children.len() as u64,
                    key => key.rotate_left(1) ^ GAP_KEY,
                };
                self.children
                    .push(virtualized::Row::space(key, u64::from(gap.to_bits()), gap));
            }
        }
        let index = self.children.len();
        if row.key == 0 {
            row.key = index as u64;
        }
        self.children.push(row);
        if rhythm.is_some() {
            self.last = rhythm;
            self.trailing_footer = footer;
        }
        index
    }
}

#[cfg(test)]
thread_local! {
    /// How many text messages this thread has built a view of.
    pub(super) static TEXT_VIEWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Sets a gap's key apart from the key of the row it precedes.
const GAP_KEY: u64 = 0x9e37_79b9_7f4a_7c15;

/// One row an exchange contributes to the transcript.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TurnPart {
    /// What was asked: the reader's prompt or a message delivered to the
    /// agent, at this index of the transcript's items.
    Prompt(usize),
    /// The strip that folds the exchange's steps.
    Steps,
    /// A message to, or launch of, another agent, at this position of the
    /// exchange's prose.
    Card(usize),
    /// An answer, a thought, an image or a step the grouping did not
    /// recognize, at this position of the exchange's prose.
    Prose(usize),
}

/// Where one item of an exchange is drawn.
#[derive(Clone, Copy)]
enum Placement<'a> {
    /// A row of the exchange, with the prepared body it draws from, which a
    /// prompt and the step strip may lack.
    Row(TurnPart, Option<&'a CachedItem>),
    /// An item that draws no row of its own: a delivery or message still
    /// being prepared, or a step shown as a failure rather than on the strip.
    Unplaced(&'a chat::Item),
}

/// The bubble a user's message is drawn in: the accent wash, or an outline
/// for an attachment, held to [`PROMPT_MEASURE`].
///
/// A message long enough to fold takes the whole measure, so showing more or
/// less of it changes only its height; any other sizes to what it holds.
/// Either way the bubble's width is never fluid. A fluid child, such as the
/// fold's full-width disclosure row, would otherwise make the bubble, and the
/// row it sits in, fill the transcript, and the bubble would lose its anchor
/// on the right.
fn user_bubble<'a>(
    body: Element<'a, Message>,
    attachment: bool,
    folds: bool,
) -> Element<'a, Message> {
    widget::container(body)
        .class(design::surface(
            (!attachment).then_some(design::accent_wash),
            [design::RADIUS_M; 4],
            attachment,
        ))
        .padding([step::GAP_S as u16, step::GAP_M as u16])
        .width(if folds {
            Length::Fixed(PROMPT_MEASURE)
        } else {
            Length::Shrink
        })
        .max_width(PROMPT_MEASURE)
        .into()
}

/// Reserve an external action row so hovering never shifts the transcript.
/// The hover also keeps its controls visible while they are keyboard-focused.
pub(super) fn message_actions<'a>(
    message: Element<'a, Message>,
    actions: Element<'a, Message>,
    alignment: Alignment,
) -> Element<'a, Message> {
    super::hover::over(
        widget::column::with_children(vec![
            message,
            widget::Space::new().height(MESSAGE_ACTION_HEIGHT).into(),
        ])
        .spacing(step::GAP_XS)
        // Should the message ever fill the row, it keeps its side.
        .align_x(alignment),
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
/// The `+` menu's width, which fits a row's name beside a few words of what
/// it does.
const COMPOSER_MENU_WIDTH: f32 = 340.0;
/// The `+` menu's list scrolls past ten rows, where a provider's commands
/// and skills would otherwise cover the conversation.
const COMPOSER_MENU_CEILING: f32 = 10.0 * COMPLETION_ROW;

/// The completion list's scrollable, named so the keyboard can scroll it.
const COMPLETION_LIST: &str = "chat-completion";

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
/// How many viewport heights before a loaded edge the next page is fetched.
const PREFETCH_SCREENS: f32 = 2.0;

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
        placeholder: design::field_placeholder_color(theme),
        value: design::field_text_color(theme),
        selection: cosmic.accent.base.into(),
    }
}

/// How much of the footer a chat pane's width affords.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterFit {
    /// The branch, the account by name and every figure.
    Wide,
    /// A split pane: the branch gives way and the account shows as its avatar.
    Narrow,
    /// Narrower still: the model shows as its provider's icon, and the
    /// context ring stands without its figure, which joins the cost in the
    /// ring's tooltip.
    Tight,
}

impl FooterFit {
    fn for_width(width: f32) -> Self {
        if width >= 600.0 {
            Self::Wide
        } else if width >= 440.0 {
            Self::Narrow
        } else {
            Self::Tight
        }
    }

    /// The most characters a footer name shows before it is cut.
    fn label_limit(self) -> usize {
        match self {
            Self::Wide => FOOTER_LABEL,
            Self::Narrow => NARROW_FOOTER_LABEL,
            Self::Tight => TIGHT_FOOTER_LABEL,
        }
    }
}

/// A control in the composer's trailing group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trailing {
    /// Sends the draft in `mode`; `slot` 0 is the primary chord's mode.
    Submit { mode: PromptMode, slot: usize },
    /// Cancels the active turn.
    Stop,
}

/// The composer's trailing controls, leading edge first.
///
/// The submit controls appear only while there is something to send, and
/// lead the group: whatever follows them - Stop while the agent works - keeps
/// its place at the trailing edge as a draft is typed or cleared, so a
/// pointer resting on Stop is never moved onto Send. A disabled Send beside
/// an empty draft said nothing a reader could act on, and while the agent
/// works it sat where Stop belonged.
fn composer_trailing(modes: &[PromptMode], busy: bool, has_draft: bool) -> Vec<Trailing> {
    let offered = if has_draft { modes } else { &[] };
    offered
        .iter()
        .enumerate()
        .map(|(slot, &mode)| Trailing::Submit { mode, slot })
        .chain(busy.then_some(Trailing::Stop))
        .collect()
}

/// What an empty composer says it is for, when Enter would send in `mode`
/// and the conversation is `busy` or not.
fn composer_placeholder(mode: Option<PromptMode>, busy: bool) -> &'static str {
    match mode {
        // Until the turn a message just sent starts shows, there is no turn
        // in view to steer: what is written next joins it.
        Some(PromptMode::Steer) if busy => "Steer this turn",
        Some(PromptMode::Queue) => "Queue for the next turn",
        Some(PromptMode::Send | PromptMode::Steer) | None => "Message",
    }
}

/// What the composer says when it's empty. Nothing can be typed while the
/// conversation changes hands, so it says nothing then. A refused
/// conversation's box, read-only or not, says why it sends nothing, rather
/// than inviting a message that has nowhere to go. Otherwise it names what
/// the field is for.
fn composer_hint(
    transitioning: bool,
    unsendable: Option<blocked_recovery::Unsendable>,
    main_agent: bool,
    mode: Option<PromptMode>,
    busy: bool,
) -> &'static str {
    if transitioning {
        ""
    } else if let Some(unsendable) = unsendable {
        unsendable.placeholder
    } else if main_agent {
        "Message main agent"
    } else {
        composer_placeholder(mode, busy)
    }
}

/// The icon and tooltip for a submit control.
///
/// `slot` is the control's place among the modes the provider currently
/// allows, and `enter_sends` decides which chord the first of them answers to,
/// so the tooltip names the key the composer is actually bound to rather than
/// a fixed one.
/// A submit control's icon and its name, which is also its tooltip. The name
/// ends with the chord that presses it, or with `unsendable`, why it can't be
/// pressed in a refused conversation, where no chord sends either.
fn submit_control(
    mode: PromptMode,
    slot: usize,
    enter_sends: bool,
    unsendable: Option<blocked_recovery::Unsendable>,
) -> (Icon, String) {
    let primary = if enter_sends { "Enter" } else { "Ctrl+Enter" };
    let (name, verb, chord) = match (mode, slot) {
        (PromptMode::Send, _) => (Icon::Send, "Send message", primary),
        (PromptMode::Steer, 0) => (Icon::Send, "Steer the active turn", primary),
        (PromptMode::Steer, _) => (Icon::Send, "Steer the active turn", "Ctrl+Shift+Enter"),
        (PromptMode::Queue, 0) => (Icon::Alarm, "Queue for the next turn", primary),
        (PromptMode::Queue, _) => (Icon::Alarm, "Queue for the next turn", "Ctrl+Shift+Enter"),
    };
    let tail = unsendable.map_or(chord, |unsendable| unsendable.reason);
    (name, format!("{verb} — {tail}"))
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
const QUEUE_PREVIEW_BYTES: usize = prompt::QUEUE_PREVIEW_BYTES;

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
    text: Cow<'a, str>,
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
    debug_assert!(text.len() <= QUEUE_PREVIEW_BYTES);
    let has_text = !text.is_empty();
    let preview: Cow<'a, str> = if has_text { text } else { names().into() };
    let mut details: Vec<Element<'a, Message>> = Vec::new();
    if !attachments.is_empty() && has_text {
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
#[cfg(test)]
fn visible_notice_body(body: &str, expanded: bool) -> String {
    if !expanded && (body.lines().count() > NOTICE_LINES || body.chars().count() > NOTICE_CHARS) {
        body.lines()
            .take(NOTICE_LINES)
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(NOTICE_CHARS)
            .collect()
    } else {
        body.to_owned()
    }
}

#[cfg(test)]
fn notice_identity(anchor: Option<&str>, message: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::hash::DefaultHasher::new();
    ("transcript-notice", anchor, message).hash(&mut hash);
    hash.finish()
}

#[cfg(test)]
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
                selectable_prose::Logical { identity, start: 0 },
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
    menu_row(
        None,
        row.label(),
        row.description,
        Some(completion_kind(row.kind)),
        highlighted,
    )
    .on_press_maybe(message)
    .into()
}

fn completion_kind(kind: completion::Kind) -> &'static str {
    match kind {
        completion::Kind::Command => "Command",
        completion::Kind::Skill => "Skill",
        completion::Kind::File => "File",
    }
}

/// A footer name, said whole on hover where it was cut. Names have no
/// bound, and a footer item does not wrap within itself.
fn bounded<'a>(
    content: Element<'a, Message>,
    name: &str,
    kind: &str,
    limit: usize,
) -> Element<'a, Message> {
    if name.chars().count() > limit {
        crate::interaction::tooltip_above(content, format!("{kind}: {name}"))
    } else {
        content
    }
}

/// How a turn the person stopped ended, in the words its stopped step and
/// the timeline's marker use.
pub(super) const TURN_STOPPED: &str = "Stopped";

/// How a turn that finished with `snapshot` ended, as its provider said.
pub(super) fn turn_outcome(snapshot: &chat::Snapshot) -> &'static str {
    match snapshot.detail.as_str() {
        chat::TURN_INTERRUPTED => TURN_STOPPED,
        "Turn failed" => "Failed",
        _ => "Finished",
    }
}

/// The row that closes a working transcript: a small muted spinner, or the
/// waiting glyph, beside what the agent is doing. Without a glyph its place
/// stays empty, so the words keep their position.
fn activity_row<'a>(
    glyph: Option<Icon>,
    label: String,
    failure: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let glyph = match glyph {
        Some(Icon::AgentWorking) => crate::ui_controls::activity_spinner(
            f32::from(design::COMPACT_ICON_SIZE),
            design::muted_icon_color,
        ),
        Some(glyph) => crate::ui_controls::passive_icon(glyph, design::COMPACT_ICON_SIZE),
        None => widget::Space::new()
            .width(design::COMPACT_ICON_SIZE)
            .height(design::COMPACT_ICON_SIZE)
            .into(),
    };
    let caption = |label: String| -> Element<'a, Message> {
        design::caption(label.replace(['\n', '\r'], " "))
            .class(cosmic::theme::Text::Custom(design::muted_text))
            .wrapping(iced::widget::text::Wrapping::None)
            .into()
    };
    let mut parts = Vec::new();
    if !label.is_empty() {
        parts.push(caption(label));
    }
    if let Some(failure) = failure {
        if !parts.is_empty() {
            parts.push(caption(" · ".into()));
        }
        parts.push(failure);
    }
    let status = widget::scrollable(widget::row::with_children(parts).align_y(Alignment::Center))
        .width(Length::Fill)
        .direction(iced::widget::scrollable::Direction::Horizontal(
            iced::widget::scrollable::Scrollbar::hidden(),
        ));
    widget::row::with_children(vec![glyph, crate::interaction::overflow_fade_x(status)])
        .spacing(step::GAP_XS)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// A one-line row of a composer list: an optional icon, a name, what it
/// does in muted text cut to the room left, and what kind of thing it is.
fn menu_row<'a>(
    icon: Option<Icon>,
    label: impl Into<Cow<'a, str>> + 'a,
    description: impl Into<Cow<'a, str>> + 'a,
    kind: Option<&'a str>,
    highlighted: bool,
) -> widget::button::Button<'a, Message> {
    let mut cells: Vec<Element<'a, Message>> = Vec::new();
    cells.extend(icon.map(|icon| {
        widget::icon(icon.handle())
            .size(design::COMPACT_ICON_SIZE)
            .into()
    }));
    cells.push(
        design::body(label)
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
    );
    cells.push(
        crate::ui_controls::FadeText::caption(description)
            .width(Length::Fill)
            .into(),
    );
    cells.extend(kind.map(|kind| {
        design::caption(kind)
            .class(cosmic::theme::Text::Custom(design::muted_text))
            .into()
    }));
    widget::button::custom(
        widget::row::with_children(cells)
            .spacing(step::GAP_S)
            .align_y(Alignment::Center),
    )
    .class(menu_row_style(highlighted))
    .padding(iced::Padding::from([0.0, step::GAP_S]))
    .height(Length::Fixed(COMPLETION_ROW))
    .width(Length::Fill)
}

/// A list row's surfaces, with the ring buttons draw for keyboard focus, so
/// the row Tab reached does not read as the one under the pointer.
fn menu_row_style(highlighted: bool) -> cosmic::theme::Button {
    let cosmic::theme::Button::Custom {
        active,
        disabled,
        hovered,
        pressed,
    } = crate::ui_controls::control_style(highlighted, None)
    else {
        unreachable!("control_style draws a custom button");
    };
    let ring = |mut style: widget::button::Style, focused: bool, theme: &cosmic::Theme| {
        if focused {
            style.outline_width = design::BORDER_WIDTH;
            style.outline_color = design::accent_color(theme);
        }
        style
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| ring(active(focused, theme), focused, theme)),
        disabled,
        hovered: Box::new(move |focused, theme| ring(hovered(focused, theme), focused, theme)),
        pressed: Box::new(move |focused, theme| ring(pressed(focused, theme), focused, theme)),
    }
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
pub enum Page {
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
    /// What of the window a page keeps, where it was last laid out. Unknown
    /// until the transcript has laid out its rows.
    kept: Option<Kept>,
}

/// Where the halves a page keeps end, in the transcript's laid-out
/// coordinates. A page slides the window around its middle item: an earlier
/// page keeps the items through it, a later page the items from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Kept {
    /// The top of the highest row holding an item after the middle one:
    /// everything above it survives an earlier page.
    pub older_end: f32,
    /// The bottom of the lowest row holding an item before the middle one:
    /// everything below it survives a later page.
    pub newer_start: f32,
}

impl Scroll {
    pub(super) fn new(from_top: f32, content: f32, window: f32) -> Self {
        Self {
            from_top,
            content,
            window,
            kept: None,
        }
    }
    pub(super) fn with_kept(self, kept: Option<Kept>) -> Self {
        Self { kept, ..self }
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
    pub(super) fn content(self) -> f32 {
        self.content
    }
    pub(super) fn offset_from_top(self) -> f32 {
        self.from_top
    }
    pub(super) fn window_height(self) -> f32 {
        self.window
    }

    pub(super) fn page_height(self) -> f32 {
        (self.window * 0.9).max(1.0)
    }

    /// Whether the next earlier page should be fetched now, ahead of the
    /// reader. Short of the very top this waits until the whole viewport is
    /// in the half the page keeps, so what is on screen never leaves the
    /// window. Items differ in height and are not drawn in item order within
    /// an exchange, so that half is where its rows were drawn. Until they
    /// have been, only the edge asks.
    pub(super) fn prefetch_earlier(self) -> bool {
        self.at_top()
            || self.kept.is_some_and(|kept| {
                self.from_top <= self.prefetch_reach()
                    && self.from_top + self.window <= kept.older_end
            })
    }

    /// Whether the next newer page should be fetched now, the mirror of
    /// [`Self::prefetch_earlier`].
    pub(super) fn prefetch_later(self) -> bool {
        self.at_bottom()
            || self.kept.is_some_and(|kept| {
                self.travel() - self.from_top <= self.prefetch_reach()
                    && self.from_top >= kept.newer_start
            })
    }

    /// How far from a loaded edge a page is fetched: far enough that wheel
    /// and Page Up reading does not reach the edge before the page arrives.
    fn prefetch_reach(self) -> f32 {
        self.window * PREFETCH_SCREENS
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
                || model_choice_label(model).to_lowercase().contains(&query)
        })
        .collect()
}

impl Cagents {
    /// The branch the conversation works on; its channel and project are
    /// already named in the tabs and sidebar.
    fn chat_place(&self, directory: &Path) -> Place {
        Place {
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
        let trailing_clearance = if self.pane_inspector_layout(card).is_none() {
            crate::app::pane_inspector::TOGGLE_CLEARANCE
        } else {
            0.0
        };
        if let Some(done) = self.unnamed_agent_view(card) {
            return done;
        }
        match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime))
                if runtime.awaiting_admission()
                    && self.shared_views.quarantined.contains(&card) =>
            {
                runtime.pending_admission_view(trailing_clearance, true)
            }
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
                let offers = if runtime.is_draft() {
                    std::iter::once(current)
                        .chain(MODEL_FAMILIES.into_iter().filter(|provider| {
                            *provider != current
                                && provider.has_native_chat()
                                && self.account_ui.offers(*provider)
                        }))
                        .map(|provider| {
                            let catalog = self.account_ui.catalog(provider);
                            Offer {
                                provider,
                                models: catalog
                                    .map_or(&[][..], |catalog| catalog.models.as_slice()),
                                catalog,
                            }
                        })
                        .collect()
                } else {
                    vec![Offer {
                        provider: current,
                        models: &runtime.snapshot.models,
                        catalog: self.account_ui.catalog(current),
                    }]
                };
                let view = runtime.view_with_resets(
                    self.chat_auth_recovery(runtime),
                    self.chat_ui.phases.get(&card),
                    self.settings.chat_enter_sends,
                    &self.settings.keyboard_shortcuts,
                    self.workspace
                        .presentation_geometry()
                        .card(card)
                        .map_or(self.workspace.viewport_width(), |card| card.width())
                        .min(super::layout::READING_COLUMN),
                    trailing_clearance,
                    place,
                    !self.workspace_inputs_blocked(),
                    accounts,
                    offers,
                    self.chat_reset_controls(
                        runtime.session.launch.provider,
                        &runtime.session.launch.recipe,
                    ),
                    self.chat_offers_fresh_conversation(card),
                );
                if !runtime.closed_elsewhere
                    && let Some(recovery) = self.account_recovery_controls(card)
                {
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
            .padding(iced::Padding::new(step::GAP_M).right(step::GAP_M.max(trailing_clearance)))
            .into(),
        }
    }
}

/// Prefer concrete activity from the newest assistant item when available.
fn activity_label(snapshot: &chat::Snapshot) -> &'static str {
    if snapshot.has_later {
        return "Working";
    }
    items_activity(&snapshot.items)
}

/// What the newest of `items` says an agent is doing.
fn items_activity(items: &[Arc<chat::Item>]) -> &'static str {
    match items
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

/// Each item belongs to this transcript's stream; advancing it ends the thought.
/// `index` is the thought's place in `items`, which every caller already
/// holds, so asking costs nothing however long the transcript is.
fn reasoning_finished(items: &[Arc<chat::Item>], busy: bool, index: usize) -> bool {
    !busy || index + 1 < items.len()
}

/// The thought's own timestamps describe its work. Older providers without an
/// end can use the next observation in this same stream, or its recorded end.
/// `index` is the thought's place in `items`.
fn reasoning_elapsed(
    items: &[Arc<chat::Item>],
    index: usize,
    finished: bool,
    finished_at: Option<u64>,
) -> Option<String> {
    if !finished {
        return None;
    }
    let item = items.get(index)?;
    let start = item.started_at?;
    let end = item
        .at
        .or_else(|| {
            items
                .get(index + 1)
                .and_then(|next| next.started_at.or(next.at))
        })
        .or_else(|| finished_at.filter(|_| index + 1 == items.len()))?;
    // Under a second reads as "0s", which says nothing.
    let millis = end.saturating_sub(start);
    (millis >= 1_000).then(|| disclosure::duration(millis))
}

/// The providers a draft offers, in the order its picker lists them after
/// its own.
/// A provider without native chat is never offered.
const MODEL_FAMILIES: [Provider; 5] = [
    Provider::Codex,
    Provider::Claude,
    Provider::Antigravity,
    Provider::Muse,
    Provider::Omp,
];

/// One provider's models, as a draft's picker offers them.
struct Offer<'a> {
    provider: Provider,
    /// What the provider listed last, under its default model. Empty where
    /// it never has, and then the picker offers the default alone.
    models: &'a [chat::Model],
    catalog: Option<&'a crate::account::cache::Catalog>,
}

/// What a provider is called where its models are listed.
fn family_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "ChatGPT",
        Provider::Claude => "Claude",
        Provider::Omp => "Other models",
        Provider::Antigravity => "Antigravity",
        Provider::Muse => "Muse",
    }
}

/// The worktree a conversation runs in.
struct Place {
    branch: Option<String>,
}

/// Whether a delivery draws its message actions: while its detail shows,
/// or always when it cannot be opened, so a message that is only its row
/// still offers its copy and its time on hover.
fn delivery_actions(has_body: bool, monitor: bool, long: bool, expanded: bool) -> bool {
    let shown = (expanded || monitor) && has_body;
    shown || !delivery_opens(has_body, monitor, long)
}

/// Whether a delivery's row opens its detail: it has one, and it is not a
/// monitor event short enough to be shown whole.
fn delivery_opens(has_body: bool, monitor: bool, long: bool) -> bool {
    has_body && (!monitor || long)
}

/// A monitor's event, whose body is shown without being opened.
fn delivery_monitor(kind: chat::DeliveryKind, summary: &str) -> bool {
    kind == chat::DeliveryKind::Notice && summary.starts_with("Monitor event: ")
}

/// Whether a delivery has more to show than its heading's first line: a
/// prepared notice's body, or a message's body, the rest of its heading or
/// its sender.
fn delivery_has_body(
    notice: Option<&literal_content::NoticeData>,
    body: &str,
    remainder: &str,
    sender: &str,
) -> bool {
    notice.map_or_else(
        || !body.is_empty() || !remainder.is_empty() || !sender.is_empty(),
        |notice| notice.body.full.len() != 0,
    )
}

/// The agent a report's title names, as `Agent "NAME"` or `Agent NAME`
/// say it.
fn delivery_agent_name(title: &str) -> Option<&str> {
    let name = title.strip_prefix("Agent ")?;
    let name = name
        .strip_prefix('"')
        .and_then(|name| name.strip_suffix('"'))
        .unwrap_or(name);
    (!name.trim().is_empty()).then_some(name)
}

fn delivery_command_outcome(heading: &str) -> Option<(&str, &str)> {
    let (title, outcome) = heading
        .strip_prefix("Background command ")
        .map(|outcome| ("Background command", outcome))
        .or_else(|| heading.rsplit_once(" · "))?;
    let known = matches!(outcome, "finished" | "stopped" | "failed")
        || outcome
            .strip_prefix("failed (exit ")
            .and_then(|code| code.strip_suffix(')'))
            .and_then(|code| code.parse::<i32>().ok())
            .is_some();
    known.then_some((title, outcome))
}

fn delivery_command_failed(heading: &str) -> bool {
    delivery_command_outcome(heading).is_some_and(|(_, outcome)| outcome.starts_with("failed"))
}

fn delivery_report_outcome<'a>(from: &str, heading: &'a str) -> Option<(&'a str, &'a str)> {
    if !from.is_empty()
        || !["Agent ", "Workflow "]
            .iter()
            .any(|prefix| heading.starts_with(prefix))
    {
        return None;
    }
    [" was stopped", " finished", " failed"]
        .into_iter()
        .find_map(|outcome| {
            heading
                .strip_suffix(outcome)
                .map(|title| (title, outcome.trim()))
        })
}

/// The inputs of one scope's selection catalog; see [`Runtime::selection_key`].
#[derive(PartialEq, Eq)]
pub(super) struct SelectionKey {
    scope: Option<String>,
    presentation: u64,
    turns: revised::Stamp,
    busy: bool,
    /// The selected scope's item disclosures; a child with none has no set.
    expanded: Option<revised::Stamp>,
    opened: revised::Stamp,
    tool_groups_expanded: bool,
    failures: (u64, revised::Stamp, revised::Stamp),
    queued: Option<(u64, u64)>,
}

impl Runtime {
    pub(super) fn document_link_origin(&self) -> Option<PathBuf> {
        self.selected_subagent
            .is_none()
            .then(|| self.session.launch.recipe.working_directory.join(".chat"))
    }

    // Provider descriptions can contain a whole prompt. Keep status controls
    // compact while the full transcript remains available on selection.
    pub(super) fn short_description(raw: &str, limit: usize) -> String {
        let mut chars = raw.chars();
        let text: String = chars.by_ref().take(limit).collect();
        if chars.next().is_some() {
            format!("{}…", text.trim_end())
        } else {
            text
        }
    }

    /// What the agent goes by: the name its provider reports for it, else
    /// the first line of the task it was given, which tells apart agents of
    /// one type better than the type does. Claude and OMP report a name, then
    /// a short description, then the type; Codex reports only a nickname.
    pub(super) fn native_child_name(agent: &chat::Subagent) -> &str {
        Some(agent.kind.trim())
            .filter(|name| !name.is_empty())
            .or_else(|| Self::native_child_task(agent))
            .unwrap_or("Subagent")
    }

    /// Whether `agent` goes by the task it was given, which its tab and
    /// heading then already say.
    pub(super) fn named_by_task(agent: &chat::Subagent) -> bool {
        Self::native_child_task(agent) == Some(Self::native_child_name(agent))
    }

    pub(super) fn native_child_status(agent: &chat::Subagent) -> &'static str {
        match agent.status {
            chat::ToolStatus::Running if agent.waiting => "Waiting",
            chat::ToolStatus::Waiting => "Waiting on background work",
            chat::ToolStatus::Running => "Working",
            chat::ToolStatus::Succeeded => "Finished",
            chat::ToolStatus::Failed => "Failed",
            chat::ToolStatus::Denied => "Denied",
            chat::ToolStatus::Stopped => "Stopped",
            chat::ToolStatus::Ended => "Ended",
        }
    }

    /// The provider error a running child reported last, while nothing newer
    /// from it has arrived. Its run may still recover, so this is the child's
    /// own state, said on its row, not a question for the person.
    pub(super) fn child_error<'a>(&self, agent: &'a chat::Subagent) -> Option<&'a str> {
        let detail = agent.detail.trim();
        (agent.status == chat::ToolStatus::Running
            && !detail.is_empty()
            && agent.items.last().is_some_and(|item| {
                chat::failure::provider_error_text(self.session.launch.provider, item).is_some()
            }))
        .then_some(detail)
    }

    pub(super) fn native_child_identity(&self, agent: &chat::Subagent) -> String {
        self.child_description(agent, &self.child_label(agent, 80))
    }

    /// What a child's tab and rows say beyond its name: its state, where it
    /// runs and the task it was given. `label` is its name as listed.
    pub(super) fn child_description(&self, agent: &chat::Subagent, label: &str) -> String {
        let provider = match self.session.launch.provider {
            Provider::Codex => "Codex",
            Provider::Claude => "Claude",
            Provider::Omp => "OMP",
            Provider::Antigravity => "Antigravity",
            Provider::Muse => "Muse",
        };
        let model = agent
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or("Model not reported");
        let mut identity = format!(
            "{} · {} · {provider} native subagent · {}",
            Self::short_description(label, 80),
            Self::native_child_status(agent),
            Self::short_description(model, 80),
        );
        // A blocked or failed child's provider error keeps its raw diagnostic
        // for copying; the tab says what it means. A finished child's detail
        // is its own report, whose prose may merely mention such words.
        let detail = agent.detail.trim();
        if !detail.is_empty() {
            identity.push_str(" · ");
            let failure = (agent.waiting
                || agent.status == chat::ToolStatus::Failed
                || self.child_error(agent).is_some())
            .then(|| chat::failure::Failure::parse(self.session.launch.provider, detail))
            .flatten();
            match failure {
                Some(failure) => identity.push_str(&failure.summary),
                None => identity.push_str(&Self::short_description(detail, 120)),
            }
        }
        if let Some(task) = Self::native_child_task(agent).filter(|_| !Self::named_by_task(agent)) {
            identity.push_str(" · Task: ");
            identity.push_str(&Self::short_description(task, 120));
        }
        identity
    }

    /// The first line of the task a child was given, which can be a whole
    /// prompt. It describes a child's tab; it never names one.
    pub(super) fn native_child_task(agent: &chat::Subagent) -> Option<&str> {
        agent
            .description
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
    }

    /// Completion requires provider evidence. An unstarted conversation is a
    /// draft; an idle attached one is ready, while an attachment is connecting.
    pub(super) fn conversation_status(&self, transitioning: bool) -> (Option<Icon>, String) {
        self.conversation_status_with_failures(transitioning, true)
    }

    fn conversation_status_with_failures(
        &self,
        transitioning: bool,
        include_failures: bool,
    ) -> (Option<Icon>, String) {
        self.conversation_status_of(transitioning, include_failures, true)
    }

    /// The conversation's own status, leaving its subagents to whoever
    /// shows them separately.
    pub(super) fn own_conversation_status(&self) -> (Option<Icon>, String) {
        self.conversation_status_of(false, false, false)
    }

    fn conversation_status_of(
        &self,
        transitioning: bool,
        include_failures: bool,
        include_subagents: bool,
    ) -> (Option<Icon>, String) {
        let (glyph, label) = self.conversation_status_base(transitioning);
        let snapshot = &self.snapshot;
        if self.closed_elsewhere
            || self.queued.is_some()
            || transitioning
            || self.attachment_chooser_pending.is_some()
            || self.sending()
            || self.disconnected
            || snapshot.exited
        {
            return (glyph, label.to_owned());
        }
        let activity = snapshot.subagent_activity();
        let (working, waiting, on_background_work, failed) = if include_subagents {
            (
                activity.working,
                activity.waiting,
                activity.on_background_work,
                activity.failed.len(),
            )
        } else {
            (0, 0, 0, 0)
        };
        let active = self.reported_errors().find_map(|message| {
            chat::failure::Failure::parse(self.session.launch.provider, message)
        });
        // The reset is when the conversation can carry on, so a limit that
        // still holds, as the daemon judges it, says it.
        let limit_reset = self
            .usage_limit
            .as_ref()
            .filter(|_| {
                active
                    .as_ref()
                    .is_some_and(|failure| failure.kind == chat::failure::FailureKind::AccountLimit)
            })
            .and_then(|limit| limit.reset.as_deref())
            .map(|reset| format!("Account limit reached · resets {reset}"));
        let failure_label = active.as_ref().and_then(|failure| match failure.kind {
            chat::failure::FailureKind::AccountLimit => Some("Account limit reached"),
            chat::failure::FailureKind::Authentication => Some("Sign-in required"),
            chat::failure::FailureKind::NoAllowance => Some("No usage allowance"),
            chat::failure::FailureKind::Capacity => Some("Provider at capacity"),
            chat::failure::FailureKind::Connection => Some("Connection interrupted"),
            chat::failure::FailureKind::Interrupted => Some("Interrupted"),
            chat::failure::FailureKind::OutOfMemory => Some("Stopped: out of memory"),
            // The agent carried on; its notice is in the transcript.
            chat::failure::FailureKind::CommandOutOfMemory => None,
            chat::failure::FailureKind::Exited => Some("Stopped unexpectedly"),
        });
        let mut parts = Vec::new();
        if let Some(label) = limit_reset.as_deref().or(failure_label) {
            parts.push(label.to_owned());
        } else if snapshot.busy
            || snapshot.waiting
            || !snapshot.requests.is_empty()
            || (working == 0 && waiting == 0 && on_background_work == 0 && failed == 0)
        {
            parts.push(label.to_owned());
        }
        let mut agent_parts = Vec::new();
        if working > 0 {
            agent_parts.push(format!(
                "{working} {} working",
                if working == 1 { "agent" } else { "agents" }
            ));
        }
        if waiting > 0 {
            agent_parts.push(if agent_parts.is_empty() {
                format!(
                    "{waiting} {} waiting",
                    if waiting == 1 { "agent" } else { "agents" }
                )
            } else {
                format!("{waiting} waiting")
            });
        }
        if on_background_work > 0 {
            agent_parts.push(if agent_parts.is_empty() {
                format!(
                    "{on_background_work} {} waiting on background work",
                    if on_background_work == 1 {
                        "agent"
                    } else {
                        "agents"
                    }
                )
            } else {
                format!("{on_background_work} waiting on background work")
            });
        }
        parts.extend(agent_parts);
        if include_failures && failed > 0 {
            parts.push(Self::agent_failure_label(&activity.failed));
        }
        if working + waiting + on_background_work + failed > 0 || failure_label.is_some() {
            return (
                if working > 0 || snapshot.busy {
                    Some(Icon::AgentWorking)
                } else if failure_label.is_some() || failed > 0 {
                    Some(Icon::Warning)
                } else if waiting > 0 {
                    Some(Icon::AgentWaiting)
                } else {
                    None
                },
                parts.join(" · "),
            );
        }
        (glyph, parts.join(" · "))
    }

    pub(super) fn agent_failure_task(agent: &chat::Subagent) -> &str {
        Self::native_child_task(agent).unwrap_or_else(|| Self::native_child_name(agent))
    }

    fn agent_failure_label(failed: &[&chat::Subagent]) -> String {
        match failed {
            [agent] => format!("1 failed: {}", Self::agent_failure_task(agent)),
            _ => format!("{} failed", failed.len()),
        }
    }

    fn agent_failure_control(&self) -> Option<Element<'_, Message>> {
        let failed = self.snapshot.subagent_activity().failed;
        if failed.is_empty() {
            return None;
        }
        let (name, on_press) = match failed.as_slice() {
            [agent] => (
                format!("Open failed agent: {}", Self::agent_failure_task(agent)),
                Action::SelectSubagent(self.target, Some(agent.id.clone())),
            ),
            _ => (
                format!("Choose from {} failed agents", failed.len()),
                Action::SubagentFailures(self.target),
            ),
        };
        let tooltip = failed
            .iter()
            .map(|agent| {
                let task = Self::agent_failure_task(agent);
                if agent.detail.trim().is_empty() {
                    task.to_owned()
                } else {
                    format!("{task}: {}", agent.detail.trim())
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        use std::hash::{Hash, Hasher};
        let mut identity = std::hash::DefaultHasher::new();
        for agent in &failed {
            agent.id.hash(&mut identity);
        }
        let button = widget::button::custom(
            design::caption(Self::agent_failure_label(&failed))
                .wrapping(iced::widget::text::Wrapping::None),
        )
        .name(name)
        .id(card_control_id(
            self.target.card,
            &format!("chat-agent-failures-{:x}", identity.finish()),
        ))
        .padding(0)
        .class(crate::ui_controls::control_style(false, None))
        .on_press(action(on_press));
        Some(crate::interaction::tooltip_above(button, tooltip))
    }

    /// What the agent is doing now, for the row that closes the transcript:
    /// the parent's work and its subagents', or the selected subagent's own.
    /// `None` once nothing runs, so the row settles into the finished stamp,
    /// and wherever the transcript already says what is happening: a first
    /// message starting shows it on the message, and switching, closing, a
    /// lost connection or a stopped agent each say so with their controls.
    pub(in crate::app) fn timeline_activity(
        &self,
        transitioning: bool,
        selected: Option<&chat::Subagent>,
    ) -> Option<(Icon, String)> {
        let snapshot = &self.snapshot;
        // A message drawn in the transcript as on its way says so itself.
        if self.closed_elsewhere
            || transitioning
            || self.queued.is_some()
            || self
                .commands
                .outgoing()
                .any(|outgoing| outgoing.in_transcript())
            || self.attachment_chooser_pending.is_some()
            || self.disconnected
            || snapshot.exited
        {
            return None;
        }
        if let Some(agent) = selected {
            if agent.status == chat::ToolStatus::Waiting {
                return Some((
                    Icon::AgentBackground,
                    Self::native_child_status(agent).to_owned(),
                ));
            }
            return (agent.status == chat::ToolStatus::Running).then(|| {
                if agent.waiting {
                    (Icon::AgentWaiting, "Waiting".to_owned())
                } else if let Some(error) = self.child_error(agent) {
                    (
                        Icon::AgentWorking,
                        chat::failure::summary(self.session.launch.provider, error).into_owned(),
                    )
                } else {
                    let activity = items_activity(&agent.items);
                    let label = if activity == "Working" && !agent.detail.trim().is_empty() {
                        agent.detail.trim().to_owned()
                    } else {
                        activity.to_owned()
                    };
                    (Icon::AgentWorking, label)
                }
            });
        }
        // Newer turns are not shown, so the end of this page is not now.
        if snapshot.has_later {
            return None;
        }
        let (glyph, label) = self.conversation_status(transitioning);
        match glyph {
            Some(glyph @ (Icon::AgentWorking | Icon::AgentWaiting)) => Some((glyph, label)),
            // A recent failed task stays actionable even after the lead finishes.
            Some(Icon::Warning) if !snapshot.subagent_activity().failed.is_empty() => {
                Some((Icon::Warning, label))
            }
            // Nothing is asked of the reader, but the finished stamp alone
            // would read as everything being done.
            _ if snapshot
                .subagents
                .iter()
                .any(|agent| agent.status == chat::ToolStatus::Waiting) =>
            {
                Some((Icon::AgentBackground, label))
            }
            _ => None,
        }
    }

    /// Whether the newest row of the shown transcript shows, with a spinner
    /// of its own, the work under way: a thought still streaming, or a step
    /// still running. The transcript shows one activity indicator, and it
    /// belongs there; otherwise the closing line carries it. A coordination
    /// card states its own runs in words, so the closing line spins for it.
    fn work_shown(&self, selected: Option<&chat::Subagent>) -> bool {
        match selected {
            Some(agent) => agent.items.last().is_some_and(|item| {
                Self::row_live(
                    &self.child_cache,
                    item,
                    agent.status == chat::ToolStatus::Running,
                )
            }),
            None => {
                let snapshot = &self.snapshot;
                snapshot.busy
                    && !snapshot.has_later
                    && !self.sending()
                    && snapshot
                        .items
                        .last()
                        .is_some_and(|item| Self::row_live(&self.cache, item, true))
            }
        }
    }

    /// Whether `item` shows, with a spinner of its own, work still under
    /// way: a running step, or a thought still streaming while its agent
    /// works. Callers pass the newest item, the only one whose thought can be
    /// live, and the cache its transcript is drawn from.
    fn row_live(cache: &HashMap<String, CachedItem>, item: &chat::Item, working: bool) -> bool {
        let card = cache
            .get(&item.id)
            .and_then(|cached| cached.coordination.as_deref())
            .is_some_and(Coordinated::is_card);
        !card
            && (item.content.status() == Some(chat::ToolStatus::Running)
                || working
                    && matches!(item.content, ItemContent::Text { thinking: true, .. })
                    && !reasoning::contentless(&item.content))
    }

    /// Whether the lead's closing line spins, as it does while subagents
    /// work after the lead has finished, or while a message is sent.
    fn closing_spins(&self) -> bool {
        self.timeline_activity(false, None)
            .is_some_and(|(glyph, _)| glyph == Icon::AgentWorking)
    }

    /// What a step strip draws for its status. While the lead works, a
    /// running strip carries the activity indicator only when it holds the
    /// newest row; otherwise a newer row or the closing line does. When the
    /// lead is idle, the group with the newest running step, such as a
    /// background command, carries it unless the closing line already
    /// spins. A strip that does not keeps its kind glyph, and its caption and
    /// counting time still say it is working.
    pub(super) fn strip_status(&self, turn: &turn::Turn) -> Option<chat::ToolStatus> {
        let status = turn.glyph();
        if status != Some(chat::ToolStatus::Running) {
            return status;
        }
        let snapshot = &self.snapshot;
        let carries = if snapshot.busy && !snapshot.has_later {
            self.work_shown(None)
                && turn
                    .steps
                    .last()
                    .is_some_and(|step| step.index + 1 == snapshot.items.len())
        } else {
            turn.newest_running && !self.closing_spins()
        };
        carries.then_some(chat::ToolStatus::Running)
    }

    /// The glyph and words of the line that closes a working transcript.
    /// Where the newest row already shows the work with a spinner, the line
    /// drops the words that repeat that row and its spinner. What it says
    /// beyond them, such as subagents working, a provider failure or the
    /// provider's own detail, stays; with nothing left, its place stays empty.
    fn closing_line(
        &self,
        glyph: Icon,
        label: String,
        failure: bool,
        selected: Option<&chat::Subagent>,
    ) -> (Option<Icon>, String) {
        if glyph != Icon::AgentWorking || !self.work_shown(selected) {
            return (Some(glyph), label);
        }
        let plain = match selected {
            Some(agent) => items_activity(&agent.items),
            None => activity_label(&self.snapshot),
        };
        let rest = if label == plain {
            ""
        } else {
            label
                .strip_prefix(plain)
                .and_then(|rest| rest.strip_prefix(" · "))
                .unwrap_or(&label)
        };
        // A blank caption keeps the row's height.
        let rest = if rest.is_empty() && !failure {
            " "
        } else {
            rest
        };
        (None, rest.to_owned())
    }

    /// The line that closes a working transcript, as [`Self::closing_line`]
    /// words it. Where it drops words, assistive technology still hears the
    /// whole status; a row holding a failure control keeps the control's own
    /// name instead.
    fn closing_row<'a>(
        &self,
        glyph: Icon,
        label: String,
        failure: Option<Element<'a, Message>>,
        selected: Option<&chat::Subagent>,
    ) -> Element<'a, Message> {
        let (shown, words) = self.closing_line(glyph, label.clone(), failure.is_some(), selected);
        let named = failure.is_none() && words != label;
        let row = activity_row(shown, words, failure);
        if named {
            crate::ui_controls::labelled(row, label)
        } else {
            row
        }
    }

    /// Whether a message is on its way to the provider. Other commands -
    /// choosing a model or permissions, paging history, answering, stopping -
    /// leave what the conversation is doing as it was until it says otherwise.
    pub(super) fn sending(&self) -> bool {
        self.commands.outgoing().next().is_some()
            || self
                .commands
                .iter()
                .any(|pending| pending.sent.is_some() || !pending.sent_attachments.is_empty())
    }

    /// How the last turn ended, for the stamp that closes a settled
    /// transcript. The turn's own outcome is kept from when it finished, so a
    /// later notice does not rewrite it; an error standing now is still said,
    /// since a reader may have nowhere else to learn of it.
    pub(super) fn finished_label(&self) -> &'static str {
        let ended = self
            .finished_stamp
            .as_ref()
            .filter(|stamp| Some(stamp.at) == self.snapshot.finished_at)
            .map_or_else(|| turn_outcome(&self.snapshot), |stamp| stamp.outcome);
        // The closing of a card moved elsewhere is not the agent's error;
        // the closed card says what happened.
        if ended == "Finished"
            && self
                .reported_errors()
                .any(|error| !self.expected_remote_close_error(error))
        {
            "Finished with error"
        } else {
            ended
        }
    }

    /// The errors this conversation reports as its state: its own and its
    /// provider's, leaving out notices of things the agent carried on past.
    pub(super) fn reported_errors(&self) -> impl Iterator<Item = &str> {
        self.error
            .as_deref()
            .into_iter()
            .chain(self.snapshot.error.as_deref())
            .filter(|message| !chat::failure::is_notice(message))
    }

    pub(super) fn conversation_status_base(&self, transitioning: bool) -> (Option<Icon>, &str) {
        let snapshot = &self.snapshot;
        if self.closed_elsewhere {
            (
                Some(Icon::Close),
                if !self.has_unsent_text() && self.attachments.is_empty() {
                    "Closed"
                } else {
                    "Closed · draft retained"
                },
            )
        } else if self.queued.is_some() {
            (Some(Icon::AgentWorking), "Starting")
        } else if transitioning {
            (Some(Icon::AgentWorking), "Switching session")
        } else if self.attachment_chooser_pending.is_some() {
            (Some(Icon::AgentWorking), "Choosing attachments")
        } else if self.sending() {
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
            && !snapshot
                .subagents
                .iter()
                .any(|agent| !agent.waiting && agent.status == chat::ToolStatus::Running)
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
            let label = self.finished_label();
            (
                Some(match label {
                    TURN_STOPPED => Icon::Stop,
                    "Finished" => Icon::Check,
                    _ => Icon::Warning,
                }),
                label,
            )
        } else if self.reported_errors().next().is_some() {
            (Some(Icon::Warning), "Error")
        } else if self.is_draft() {
            (None, "Draft")
        } else if snapshot.ready {
            (None, "Ready")
        } else {
            (Some(Icon::AgentWorking), "Connecting")
        }
    }

    /// What the logical copy/selection catalog is built from, as revisions
    /// compared by value. Building it costs a lookup of the selected child
    /// and never reads a transcript, a disclosure set or a diagnostic's text.
    ///
    /// Cache preparation advances `presentation_revision` for every presented
    /// text or ordering edit, including the item list of the selected child,
    /// and only for those: metadata-only heartbeats and hidden-child streams
    /// leave it alone. The parent's grouping into turns is stamped on its own,
    /// because a regroup with unchanged items does not advance that epoch.
    /// Which failure incidents exist and where they anchor is their content
    /// revision. Together with the selected scope these cover everything an
    /// incident's exposure reads.
    pub(super) fn selection_key(&self) -> SelectionKey {
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id));
        SelectionKey {
            scope: self.selected_subagent.clone(),
            presentation: self.presentation_revision,
            turns: self.turns.stamp(),
            busy: selected.map_or(self.snapshot.busy, |agent| {
                agent.status == chat::ToolStatus::Running
            }),
            expanded: match &self.selected_subagent {
                Some(child) => self.child_expanded.get(child).map(Revised::stamp),
                None => Some(self.expanded.stamp()),
            },
            opened: self.opened.stamp(),
            tool_groups_expanded: self.tool_groups_expanded,
            failures: self.failure_ui.exposure(),
            queued: self
                .local_prompt()
                .map(|local| (local.source.id, local.presentation.revision)),
        }
    }

    /// The logical copy/selection catalog is rebuilt only when exposed text
    /// or disclosure changes, never for mouse motion, copying, find or timers.
    pub(super) fn selection_document(
        &self,
    ) -> (
        Arc<selection::Catalog>,
        std::rc::Rc<std::cell::RefCell<selection::Bookmark>>,
    ) {
        let key = self.selection_key();
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id));
        let items = selected.map_or(self.snapshot.items.as_slice(), |agent| {
            agent.items.as_slice()
        });
        let mut document = self.selection_document.borrow_mut();
        if document.as_ref().is_none_or(|(built, _)| *built != key) {
            let mut builder = selection::Catalog::builder();
            if let Some(agent) = selected {
                for incident in self.failure_ui.incidents.iter().filter(|incident| {
                    self.incident_has_child_detail(incident, &agent.id)
                        && !agent.items.iter().any(|item| {
                            self.failure_ui
                                .child_item(&agent.id, &item.id)
                                .is_some_and(|(found, _)| found.key == incident.key)
                        })
                }) {
                    self.selection_failure(&mut builder, incident);
                }
                for item in items {
                    if let Some((incident, first)) = self.failure_ui.child_item(&agent.id, &item.id)
                    {
                        if first {
                            self.selection_failure(&mut builder, incident);
                        }
                        continue;
                    }
                    self.selection_item(&mut builder, item, turn::slot_of(item), true);
                }
            } else {
                for incident in self.failure_ui.incidents.iter().filter(|incident| {
                    !incident.active_recovery
                        && incident.child_scope.is_none()
                        && incident.anchor.is_none()
                }) {
                    self.selection_failure(&mut builder, incident);
                }
                for turn in &self.turns {
                    for index in &turn.prompt {
                        if let Some(item) = items.get(*index) {
                            self.selection_item(&mut builder, item, turn::Slot::Prose, true);
                        }
                    }
                    if self.steps_shown(turn) {
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
                    for incident in self.failure_ui.incidents.iter().filter(|incident| {
                        !incident.active_recovery
                            && incident.child_scope.is_none()
                            && incident.anchor.as_ref() == Some(&turn.key)
                    }) {
                        self.selection_failure(&mut builder, incident);
                    }
                }
                if let Some(local) = self.local_prompt()
                    && let Some(prepared) = &local.presentation.prepared
                {
                    builder.indexed(
                        "queued-prompt",
                        &[2],
                        prepared.full.plain(),
                        prepared.full.plain_prefixes(),
                    );
                }
            }
            let catalog =
                builder.finish_reusing(document.as_ref().map(|(_, catalog)| catalog.as_ref()));
            *document = Some((key, catalog));
            self.find_navigation.changed_document();
        }
        let catalog = document.as_ref().unwrap().1.clone();
        let bookmark = self
            .selection_bookmarks
            .borrow_mut()
            .entry(self.selected_subagent.clone())
            .or_default()
            .clone();
        (catalog, bookmark)
    }

    fn selection_failure_preview<'a>(
        &'a self,
        incident: &failure::Incident,
    ) -> Option<(u64, &'a str)> {
        self.failure_ui
            .expanded
            .contains(&incident.key)
            .then_some(())?;
        let (preview, _) = self.failure_preview(&incident.key)?;
        Some((failure::diagnostic_preview_identity(&incident.key), preview))
    }

    fn selection_failure(
        &self,
        builder: &mut selection::DocumentBuilder,
        incident: &failure::Incident,
    ) {
        if let Some((identity, preview)) = self.selection_failure_preview(incident) {
            builder.logical(identity, Arc::from(preview));
        }
    }

    fn selection_item(
        &self,
        builder: &mut selection::DocumentBuilder,
        item: &chat::Item,
        slot: turn::Slot,
        detail: bool,
    ) {
        if item.role == chat::Role::User && !matches!(item.content, ItemContent::Delivery { .. }) {
            if matches!(item.content, ItemContent::Text { .. }) {
                if let Some(prompt) = self.prepared_prompt(item) {
                    let text = prompt.shown(self.item_expanded(&item.id));
                    builder.indexed(&item.id, &[2], text.plain(), text.plain_prefixes());
                }
            } else if matches!(item.content, ItemContent::Image { .. }) {
                if let Some(text) = self
                    .prepared_literal(item)
                    .and_then(literal_content::Content::image_prompt)
                {
                    builder.indexed(&item.id, &[2], text.plain(), text.plain_prefixes());
                }
            } else {
                let (text, _, _) = self.prompt_text(item);
                builder.plain(&item.id, &[2], text.into());
            }
            return;
        }
        let Some(cached) = self.presentation_cache().get(&item.id) else {
            return;
        };
        if cached.placeholder {
            return;
        }
        if let Some(literal) = self.prepared_literal(item) {
            // Monitor previews are visible; other notices disclose their body.
            if matches!(
                item.content,
                ItemContent::Delivery {
                    kind: chat::DeliveryKind::Notice,
                    ..
                }
            ) && !self.item_expanded(&item.id)
                && !matches!(&item.content, ItemContent::Delivery { summary, .. } if summary.starts_with("Monitor event: "))
            {
                return;
            }
            literal.catalog(builder, item, self.item_expanded(&item.id));
            return;
        }
        if literal_content::Prepared::required(&item.content) {
            return;
        }
        if slot == turn::Slot::Thinking && !self.item_expanded(&item.id) {
            return;
        }
        if matches!(item.content, ItemContent::Delivery { .. }) && !self.item_expanded(&item.id) {
            return;
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
                let _ = body; // Literal Notice is indexed only from its exact accepted DTO above.
            } else {
                builder.markdown(&item.id, &[0], cached.markdown.items());
            }
        }
        // What a coordinating step said and collected is drawn only in its
        // open detail.
        if let Some(coordinated) = &cached.coordination
            && detail
            && self.item_expanded(&item.id)
        {
            for (index, content) in coordinated.contents().enumerate() {
                builder.markdown(&item.id, &[1, index as u32], content.items());
            }
        }
        if detail && self.item_expanded(&item.id) {
            if let Some(input) = &cached.tool_input {
                builder.indexed(
                    &item.id,
                    &[4],
                    input.text.clone(),
                    input.reader_source.prefixes(),
                );
            }
            if let Some(output) = &cached.tool_output
                && !output.text.is_empty()
            {
                builder.indexed(
                    &item.id,
                    &[5],
                    output.text.clone(),
                    output.reader_source.prefixes(),
                );
            }
            for (index, patch) in cached.patches.iter().enumerate() {
                if !patch.text.is_empty() {
                    builder.indexed(
                        &item.id,
                        &[6, index as u32],
                        patch.text.clone(),
                        patch.reader_source.prefixes(),
                    );
                }
            }
        }
    }

    fn child_item_revision(
        &self,
        agent: &chat::Subagent,
        index: usize,
        item: &chat::Item,
        cached: &CachedItem,
        dark: bool,
        now: Option<u64>,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut revision = std::hash::DefaultHasher::new();
        (
            cached.revision,
            self.item_expanded(&item.id),
            self.child_find_revision(),
            dark,
            cached.shows_figures().then(|| self.figures.version()),
        )
            .hash(&mut revision);
        if item.content.is_step() {
            disclosure::item_duration_ms(item, now)
                .map(|millis| millis / 1_000)
                .hash(&mut revision);
            agent
                .items
                .last()
                .is_some_and(|last| last.id == item.id)
                .hash(&mut revision);
        }
        if matches!(item.content, ItemContent::Text { thinking: true, .. }) {
            let finished = reasoning_finished(
                &agent.items,
                agent.status == chat::ToolStatus::Running,
                index,
            );
            (
                finished,
                reasoning_elapsed(&agent.items, index, finished, None),
            )
                .hash(&mut revision);
        }
        revision.finish()
    }

    fn child_find_revision(&self) -> Option<(u64, u64)> {
        self.find.as_ref().map(|_| {
            (
                self.find_navigation.search_epoch(),
                find_navigation::render_revision(self.target),
            )
        })
    }

    #[cfg(test)]
    fn view<'a>(
        &'a self,
        recovery: Option<auth_recovery::Recovery>,
        transition: impl Into<ViewTransition>,
        enter_sends: bool,
        shortcuts: &crate::settings::KeyboardShortcuts,
        content_width: f32,
        transcript_clearance: f32,
        place: Place,
        accepting_drops: bool,
        accounts: Option<ChatAccounts<'a>>,
        offers: Vec<Offer<'a>>,
        fresh_available: bool,
    ) -> Element<'a, Message> {
        self.view_with_resets(
            recovery,
            transition,
            enter_sends,
            shortcuts,
            content_width,
            transcript_clearance,
            place,
            accepting_drops,
            accounts,
            offers,
            None,
            fresh_available,
        )
    }

    fn view_with_resets<'a>(
        &'a self,
        recovery: Option<auth_recovery::Recovery>,
        transition: impl Into<ViewTransition>,
        enter_sends: bool,
        shortcuts: &crate::settings::KeyboardShortcuts,
        content_width: f32,
        transcript_clearance: f32,
        place: Place,
        accepting_drops: bool,
        accounts: Option<ChatAccounts<'a>>,
        offers: Vec<Offer<'a>>,
        reset_controls: Option<crate::app::account_ui::resets::Controls>,
        fresh_available: bool,
    ) -> Element<'a, Message> {
        let transition = transition.into();
        let transitioning = !matches!(transition, ViewTransition::Idle);
        if self.awaiting_admission() {
            return self.pending_admission_view(transcript_clearance, false);
        }
        let target = self.target;
        // Nothing is sent past a first message still waiting to go.
        let available = self.composer_available(transitioning);
        let snapshot = &self.snapshot;
        // One theme lookup and one settings value per frame, rather than one
        // per rendered message.
        // Answers are read at a size above the shell's own captions and
        // controls, and code inside them at the body size rather than the
        // three quarters of it iced defaults to, which put a code span at
        // ten pixels.
        let dark = cosmic::theme::is_dark();
        let markdown_settings =
            design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| snapshot.subagents.iter().find(|agent| &agent.id == id));
        let anchors = Vec::new();
        let mut transcript = VirtualRows::new();
        let mut selected_identity: Option<Element<'_, Message>> = None;
        if let Some(agent) = selected {
            let provider = match self.session.launch.provider {
                Provider::Codex => "Codex",
                Provider::Claude => "Claude",
                Provider::Omp => "OMP",
                Provider::Antigravity => "Antigravity",
                Provider::Muse => "Muse",
            };
            // A model the provider never reported is left out rather than
            // announced; the tooltip says so for whoever asks.
            let model = agent
                .model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty());
            let mut heading = vec![
                widget::row::with_children(vec![
                    design::transcript_name(self.child_label(agent, 80))
                        .width(Length::Fill)
                        .into(),
                    design::heading(Self::native_child_status(agent))
                        .size(design::TEXT_CAPTION)
                        .into(),
                ])
                .spacing(step::GAP_S)
                .align_y(Alignment::Center)
                .into(),
            ];
            // The name titles the transcript; the task it was given sits
            // beneath it, quieter, so the reader knows what it is for, unless
            // the agent already goes by that task.
            if let Some(task) =
                Self::native_child_task(agent).filter(|_| !Self::named_by_task(agent))
            {
                heading.push(
                    design::caption(Self::short_description(task, 80))
                        .class(cosmic::theme::Text::Custom(design::muted_text))
                        .into(),
                );
            }
            heading.push(crate::interaction::tooltip(
                design::caption(match model {
                    Some(model) => format!(
                        "{provider} subagent · {}",
                        Self::short_description(model, 80)
                    ),
                    None => format!("{provider} subagent"),
                }),
                self.native_child_identity(agent),
            ));
            let heading: Element<'_, Message> = widget::column::with_children(heading)
                .spacing(step::GAP_XS)
                .width(Length::Fill)
                .into();
            selected_identity = Some(
                // One waiting on its own background work is still going, and
                // stopping it is how that work ends.
                if agent.status.is_live() && snapshot.capabilities.stop_subagent {
                    widget::row::with_children(vec![
                        heading,
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
                    heading
                },
            );
            if agent.items.is_empty()
                && !(agent.status == chat::ToolStatus::Failed && !agent.detail.trim().is_empty())
            {
                transcript.push(transcript_body(
                    if agent.status == chat::ToolStatus::Running {
                        "Waiting for subagent activity…"
                    } else {
                        "No recent subagent activity is available here."
                    },
                    target,
                ));
            }
            for incident in self
                .failure_ui
                .incidents
                .iter()
                .filter(|incident| self.incident_has_child_detail(incident, &agent.id))
            {
                let has_item = agent.items.iter().any(|item| {
                    self.failure_ui
                        .child_item(&agent.id, &item.id)
                        .is_some_and(|(found, _)| found.key == incident.key)
                });
                if !has_item {
                    transcript
                        .push(self.failure_view_with_reset(incident, reset_controls.as_ref()));
                }
            }
            for (index, item) in agent.items.iter().enumerate() {
                if let Some((incident, first)) = self.failure_ui.child_item(&agent.id, &item.id) {
                    if first {
                        transcript.push(
                            widget::container(
                                self.failure_view_with_reset(incident, reset_controls.as_ref()),
                            )
                            .id(self.find_anchor(&item.id))
                            .into(),
                        );
                    }
                    continue;
                }
                let Some(cached) = self.child_cache.get(&item.id) else {
                    continue;
                };
                // As in the lead's transcript, a thought with nothing to
                // read is working time, not a row.
                if reasoning::contentless(&item.content) {
                    continue;
                }
                // Drawn by delivery_view, as in the main transcript.
                let delivery = item.role == chat::Role::User
                    && matches!(item.content, ItemContent::Delivery { .. });
                let footer = if delivery {
                    if self.delivery_carries_actions(item) {
                        MESSAGE_ACTION_FOOTER
                    } else {
                        0.0
                    }
                } else if (item.role == chat::Role::User && !item.content.is_step())
                    || matches!(item.content, ItemContent::Text { .. })
                {
                    MESSAGE_ACTION_FOOTER
                } else {
                    0.0
                };
                use std::hash::{Hash, Hasher};
                let mut key = std::hash::DefaultHasher::new();
                ("selected-child-item", &item.id).hash(&mut key);
                let revision = self.child_item_revision(
                    agent,
                    index,
                    item,
                    cached,
                    dark,
                    self.source_now_ms(),
                );
                // The subagent's own messages to it open its exchanges; the
                // rest of its work keeps the main transcript's rhythm.
                let rhythm = if delivery {
                    Rhythm::Line
                } else if item.role == chat::Role::User && !item.content.is_step() {
                    Rhythm::Opening
                } else {
                    let card = cached
                        .coordination
                        .as_deref()
                        .is_some_and(|coordinated| coordinated.is_card());
                    Rhythm::of(item, turn::slot_of(item), card)
                };
                let estimate = row_estimate(item, rhythm, footer, content_width);
                let row_index = transcript.lazy(
                    key.finish(),
                    revision,
                    estimate,
                    vec![scroll::item_id(&item.id), self.find_anchor(&item.id)],
                    Some(rhythm),
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
                                        .item_expanded(&item.id)
                                        .then(|| self.step_detail(item, cached, markdown_settings));
                                    self.coordination_card(item, coordinated, raw)
                                } else {
                                    match &item.content {
                                        ItemContent::Text { .. } => self.prose_view(
                                            index,
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
                            .id(self.find_anchor(&item.id))
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
            let unanchored = |incident: &&failure::Incident| {
                !incident.active_recovery
                    && incident.child_scope.is_none()
                    && incident.anchor.is_none()
            };
            // A pane that shows why its conversation couldn't open is not
            // waiting for a first message; inviting one would contradict it.
            if self.turns.is_empty()
                && self.local_prompt().is_none()
                && !self.closed_elsewhere
                && !self
                    .failure_ui
                    .incidents
                    .iter()
                    .any(|incident| unanchored(&incident))
            {
                // The empty state says what to do. It used to show whatever the
                // adapter's last status detail was, which meant an empty
                // conversation greeted the reader with a connection state instead
                // of an invitation; the status line is where a state belongs.
                // A conversation another window started is not asking for a
                // first message, so it does not invite one while it comes up.
                transcript.push(
                    widget::container(
                        design::transcript(if self.provider_coming_up() {
                            "This conversation is starting."
                        } else {
                            "Your conversation appears here. Send a message to begin."
                        })
                        .class(cosmic::theme::Text::Custom(design::muted_text)),
                    )
                    .center_x(Length::Fill)
                    .padding(space::XXL)
                    .into(),
                );
            }
            for incident in self.failure_ui.incidents.iter().filter(unanchored) {
                transcript.push(
                    widget::container(
                        self.failure_view_with_reset(incident, reset_controls.as_ref()),
                    )
                    .id(self.find_anchor(&format!("failure:{}", incident.key)))
                    .into(),
                );
            }
            // Each row an exchange draws, and the gap above it.
            transcript.children.reserve(
                self.turns
                    .iter()
                    .map(|turn| 2 * (turn.prompt.len() + turn.prose.len() + 1))
                    .sum(),
            );
            for turn in &self.turns {
                self.push_turn_rows(
                    &mut transcript,
                    turn,
                    content_width,
                    markdown_settings,
                    dark,
                );
                for incident in self.failure_ui.incidents.iter().filter(|incident| {
                    !incident.active_recovery
                        && incident.child_scope.is_none()
                        && incident.anchor.as_ref() == Some(&turn.key)
                }) {
                    transcript.push(
                        widget::container(
                            self.failure_view_with_reset(incident, reset_controls.as_ref()),
                        )
                        .id(self.find_anchor(&format!("failure:{}", incident.key)))
                        .into(),
                    );
                }
            }
            if !snapshot.has_later
                && snapshot.activity() == crate::agent::ActivityState::Idle
                && snapshot.queued_messages.is_empty()
                && self.queued.is_none()
                && self.followups.is_empty()
                && !self.sending()
                && let Some(stamp) = &self.finished_stamp
            {
                transcript.push(
                    widget::tooltip(
                        design::caption(format!("{} at {}", self.finished_label(), stamp.clock))
                            .class(cosmic::theme::Text::Custom(design::muted_text)),
                        design::caption(stamp.full.clone()),
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
            for outgoing in self
                .commands
                .outgoing()
                .filter(|outgoing| outgoing.in_transcript())
            {
                transcript.push(self.local_prompt_view(&outgoing.prompt, "Sending…".into(), None));
            }
        }
        // What the agent is doing now closes the transcript, so it scrolls
        // away with the turns it follows rather than holding a row above the
        // composer, and each change redraws this one row rather than adding
        // another.
        if let Some((glyph, label)) = self.timeline_activity(transitioning, selected) {
            let failure = selected
                .is_none()
                .then(|| self.agent_failure_control())
                .flatten();
            let label = if failure.is_some() {
                self.conversation_status_with_failures(transitioning, false)
                    .1
            } else {
                label
            };
            transcript.push(self.closing_row(glyph, label, failure, selected));
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
        content.extend(self.plan_view());
        if let Some(error) = self.preparation.error() {
            content.push(
                widget::row::with_children(vec![
                    design::caption(error.to_owned()).width(Length::Fill).into(),
                    crate::ui_controls::button::secondary("Retry loading")
                        .on_press(action(Action::RetryMarkdown(target)))
                        .into(),
                ])
                .spacing(step::GAP_S)
                .width(Length::Fill)
                .into(),
            );
        }
        if let Some(query) = &self.find {
            content.push(
                widget::row::with_children(vec![
                    // Enter and Escape are answered by the field that holds
                    // the keyboard, however it got there: a click, Ctrl+F or
                    // the Previous and Next buttons handing it back.
                    crate::interaction::focused_key(
                        crate::ui_controls::field::search("Find in conversation", query)
                            .id(card_control_id(target.card, "chat-find"))
                            .on_input(move |query| action(Action::Find(target, query)))
                            .width(Length::Fill),
                        move |event| {
                            let keyboard::Event::KeyPressed {
                                key: keyboard::Key::Named(key),
                                modifiers,
                                ..
                            } = event
                            else {
                                return None;
                            };
                            match key {
                                keyboard::key::Named::Escape => {
                                    Some(action(Action::CloseFind(target)))
                                }
                                keyboard::key::Named::Enter
                                    if !modifiers.control()
                                        && !modifiers.alt()
                                        && !modifiers.logo() =>
                                {
                                    Some(action(Action::FindNext(target, !modifiers.shift())))
                                }
                                _ => None,
                            }
                        },
                    ),
                    design::caption(if let Some(error) = self.find_error() {
                        error.to_owned()
                    } else if self.find_searching() {
                        "Searching…".to_owned()
                    } else if self.find_matches.is_empty() {
                        "0 matches".to_owned()
                    } else {
                        format!(
                            "{} of {} {}",
                            self.find_index + 1,
                            self.find_matches.len(),
                            if selected.is_some() { "items" } else { "turns" }
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
                .into(),
            );
            if self.find_error().is_some() {
                content.push(
                    crate::ui_controls::button::secondary("Retry search")
                        .on_press(action(Action::FindRetry(target)))
                        .into(),
                );
            }
        }
        // The scroll viewport spans the pane, so its scrollbar follows the
        // pane edge, while the rows keep the reading column the composer
        // shares and its leading inset. The trailing side keeps extra
        // clearance for the pane toggle, which also protects a row scrolled
        // beneath that fixed control. The viewport runs right up to whatever
        // is pinned above and below it, the composer included, so a line
        // scrolling past either edge meets that surface rather than a strip
        // of pane laid over it. The gap the pane keeps between its blocks
        // moves inside the viewport instead: it is the room the first and
        // last rows rest at, and scrolled content passes through it.
        let transcript_index = content.len();
        let transcript = widget::container(
            widget::container(virtualized::column(transcript.children, Default::default()))
                .padding(
                    iced::Padding::new(step::GAP_M)
                        .top(step::GAP_M + if transcript_index > 0 { space::S } else { 0.0 })
                        .bottom(step::GAP_M + step::GAP_S)
                        .right(step::GAP_M.max(transcript_clearance)),
                )
                .width(Length::Fill)
                .max_width(super::layout::READING_COLUMN),
        )
        .center_x(Length::Fill);
        let transcript_layer = if self.visible.load(Ordering::Relaxed) {
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
                    if let Some(edge) = self.scroll_memory.edge(self.selected_subagent.as_deref()) {
                        self.presentation_evicted
                            || (edge == scroll::Edge::Latest
                                && self.selected_subagent.is_none()
                                && (snapshot.has_later
                                    || snapshot.history_loading
                                    || self.paging() == Some(Page::Latest)
                                    || self.deferred_page == Some(HistoryNavigation::Latest)))
                            || {
                                let items = selected.map_or(snapshot.items.as_slice(), |agent| {
                                    agent.items.as_slice()
                                });
                                match edge {
                                    scroll::Edge::Start => items.first(),
                                    scroll::Edge::End | scroll::Edge::Latest => items.last(),
                                }
                                .is_some_and(|item| {
                                    self.presentation_cache()
                                        .get(&item.id)
                                        .is_none_or(|cached| cached.placeholder)
                                })
                            }
                    } else {
                        self.scroll_memory.restoration_pending(
                            self.selected_subagent.as_deref(),
                            self.presentation_evicted,
                            |id| {
                                self.active_item(id).is_some()
                                    && self
                                        .presentation_cache()
                                        .get(id)
                                        .is_none_or(|cached| cached.placeholder)
                            },
                        )
                    },
                    if self
                        .scroll_memory
                        .preserve_arrived(self.selected_subagent.as_deref())
                    {
                        selected
                            .map_or(snapshot.items.as_slice(), |agent| agent.items.as_slice())
                            .iter()
                            .filter(|item| {
                                self.presentation_cache()
                                    .get(&item.id)
                                    .is_none_or(|cached| cached.placeholder)
                            })
                            .map(|item| item.id.as_str())
                            .collect()
                    } else {
                        Default::default()
                    },
                    Some(self.find_navigation.guard()),
                    if selected.is_none() {
                        scroll::History {
                            pages: scroll::Pages {
                                earlier: snapshot.has_earlier,
                                later: snapshot.has_later,
                            },
                            // The store's `middle` cursor is the window's
                            // item at `len / 2` too (`Store::window_reusing`).
                            pivot: (!snapshot.items.is_empty()).then(|| scroll::Pivot {
                                index: snapshot.items.len() / 2,
                                items: snapshot.items.len(),
                            }),
                        }
                    } else {
                        scroll::History::default()
                    },
                ),
                target,
                self.selected_subagent.as_deref(),
                catalog,
                bookmark,
            ))
        } else {
            // Overscan cards can remain mounted after model dehydration.
            // Diff to a cheap child so no old retained paragraph survives.
            crate::ui_memory::gate(
                widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
        };
        // Keep the transcript mounted in its original layer slot while the
        // chooser or the image viewer covers it, preserving its native scroll
        // and selection state, and what arrives meanwhile, such as a message
        // sent while looking at an image. Either holds the slot's extent, so
        // the composer below doesn't move as it opens or closes. The chooser
        // is the transient one, opened over the viewer and closed back to it.
        content.push(crate::interaction::exclusive_layer(
            transcript_layer,
            self.child_picker_view()
                .map(|picker| {
                    widget::container(
                        widget::container(picker)
                            .width(Length::Fill)
                            .max_width(super::layout::READING_COLUMN),
                    )
                    .center_x(Length::Fill)
                    .into()
                })
                .or_else(|| {
                    self.image_viewer.as_ref().map(|gallery| {
                        gallery.pane_view(target, content_width, transcript_clearance)
                    })
                }),
        ));
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
        // A resume says so on its own control, beside what it is resuming.
        if transitioning && self.queued.is_none() && !matches!(transition, ViewTransition::Resuming)
        {
            content.push(body(match transition {
                ViewTransition::FirstStart => "Starting this conversation… Editing and session controls are disabled. You can still read and copy the conversation.",
                _ => "Switching or closing this session… Editing and session controls are disabled. You can still read and copy the conversation.",
            }));
        }
        if let Some(refusal) = self.close_refusal_view() {
            content.push(refusal);
        }
        if self.closed_elsewhere {
            content.push(self.closed_draft_controls(transitioning));
        } else if self.capacity_refused {
            let enabled = !transitioning
                && self.commands.is_idle()
                && self.attachment_chooser_pending.is_none()
                && (!self.is_draft()
                    || !self.composer.text().trim().is_empty()
                    || !self.attachments.is_empty());
            content.push(
                widget::column::with_children(vec![
                    design::caption(CAPACITY_GUIDANCE).into(),
                    crate::ui_controls::button::primary("Retry")
                        .on_press_maybe(enabled.then(|| {
                            action(if self.is_draft() {
                                Action::Submit(target, PromptMode::Send)
                            } else {
                                Action::Reconnect(target)
                            })
                        }))
                        .into(),
                ])
                .spacing(space::S)
                .into(),
            );
        } else if self.start_blocked.is_some()
            || ((self.disconnected || snapshot.exited) && self.auth_error().is_none())
        {
            content.push(self.stopped_controls(
                transitioning,
                matches!(transition, ViewTransition::Resuming),
                fresh_available,
            ));
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
        if !self.closed_elsewhere && self.auth_error().is_some() {
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
                widget::container(crate::interaction::overflow_fade_y(
                    widget::scrollable(
                        widget::column::with_children(
                            snapshot
                                .requests
                                .iter()
                                .map(|request| {
                                    // Requests belong to the parent even while
                                    // its child transcript is being inspected.
                                    let document = Some(
                                        self.session.launch.recipe.working_directory.join(".chat"),
                                    );
                                    request_view(
                                        self.target,
                                        request,
                                        &self.answers,
                                        available,
                                        RequestSurface::Transcript,
                                    )
                                    .map(move |message| {
                                        capture_document_link(message, target, &document)
                                    })
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
                    design::base,
                ))
                .max_height(REQUEST_CEILING)
                .into(),
            );
        }
        if !snapshot.subagents.is_empty() {
            content.push(self.child_navigation(content_width));
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
                widget::container(crate::interaction::overflow_fade_y(
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
                    design::raised,
                ))
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
                crate::design::caption(
                    if attachment.reference.is_none()
                        && clipboard_image::is_pasted(&attachment.path)
                    {
                        "Pasted image".into()
                    } else {
                        attachment.label()
                    },
                )
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
                    design::caption(attachment.reference.as_ref().map_or_else(
                        || attachment.path.display().to_string(),
                        |reference| reference.name.clone(),
                    )),
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
        // Only the transcript layer spans the pane; everything around it
        // keeps the reading column.
        let transcript_last = content.len() == transcript_index + 1;
        let mut content = vec![
            widget::column::with_children(content.into_iter().enumerate().map(|(index, child)| {
                if index == transcript_index {
                    child
                } else {
                    widget::container(child)
                        .padding({
                            let padding =
                                iced::Padding::from([0.0, step::GAP_M]).top(if index == 0 {
                                    step::GAP_M
                                } else if index == transcript_index + 1 {
                                    // Against the transcript's edge: its own
                                    // padding keeps the rows clear of this.
                                    0.0
                                } else {
                                    space::S
                                });
                            // The first block sits beside the pane toggle.
                            if index == 0 {
                                padding.right(step::GAP_M.max(transcript_clearance))
                            } else {
                                padding
                            }
                        })
                        .width(Length::Fill)
                        .max_width(super::layout::READING_COLUMN)
                        .into()
                }
            }))
            .align_x(Alignment::Center)
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
        // A message just sent is about to start a turn, so what is written
        // after it steers or waits as it would once the turn shows.
        let under_way = self.turn_under_way();
        let modes: &[PromptMode] = if under_way {
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
        let unsendable = self
            .start_blocked
            .map(|reopen| reopen.unsendable(fresh_available));
        let placeholder = composer_hint(
            transitioning,
            unsendable,
            selected.is_some(),
            modes.first().copied(),
            self.turn_running(),
        );
        // Key bindings must remain available while the editor changes its own
        // contents. The action handler checks the current draft before sending;
        // tying the binding to a view-time draft snapshot can leave Enter
        // inserting a newline until another pointer event redraws the pane.
        // A first message still on its way keeps the binding too: Enter then
        // sends nothing, rather than breaking the line of the next draft.
        let key_submit = if (available || self.queued.is_some()) && self.start_blocked.is_none() {
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
        let composer_ceiling = 12.0 * COMPOSER_LINE_HEIGHT + 2.0 * COMPOSER_INSET_Y;
        let suggestion = if available && !transitioning {
            self.prompt_suggestion()
        } else {
            None
        };
        let key_suggestion = suggestion.cloned();
        let placeholder = suggestion.map_or(placeholder, |suggestion| suggestion.text.as_str());
        let suggestion_hint: Element<'_, Message> = if suggestion.is_some() {
            widget::container(widget::text("Tab to use suggestion").size(design::TEXT_CAPTION))
                .padding(iced::Padding::from([0.0, COMPOSER_PADDING]).bottom(space::XS))
                .into()
        } else {
            iced::widget::Space::new().height(0).into()
        };
        // The editor grows with the draft up to the ceiling, then scrolls.
        // The vertical rail stays reserved so the text keeps its measure.
        // A scrolled draft runs on through the editor's vertical inset to
        // the composer's border, fading into the composer there rather than
        // being sliced at the inset (#2824).
        let composer_fill = if self.drop_hover {
            design::accent_wash
        } else {
            design::raised
        };
        // A saved conversation the update refused takes no message in this
        // pane until it reopens (#2905): the box would take typing with
        // nowhere to send it. What it holds stays readable and copyable. A
        // pane that can start a new conversation in its place, which takes
        // the message along, and a refused first message stay editable.
        let read_only = transitioning
            || (self
                .start_blocked
                .is_some_and(|reopen| !reopen.offers_fresh())
                && !self.is_draft());
        let editor = crate::ui_controls::field::editor("Message", &self.composer)
            .id(card_control_id(target.card, "chat-composer"))
            .placeholder(placeholder)
            .size(design::TEXT_PROSE)
            .height(Length::Shrink)
            .max_height(composer_ceiling)
            .padding(composer_editor_padding())
            .fade_into_padding(COMPOSER_TEXT_REACH, COMPOSER_TEXT_REACH, composer_fill)
            .class(cosmic::theme::iced::TextEditor::Custom(Box::new(
                composer_editor_style,
            )));
        let editor: Element<'_, Message> = if read_only {
            // While the conversation changes hands the draft stays in place
            // and can still be scrolled, selected and copied, but not edited.
            // It is the same editor rather than a text copy of the draft,
            // which would shape all of a long draft at once on the UI thread
            // (#2973).
            editor
                .read_only()
                .on_action(move |event| action(Action::ReadComposer(target, event)))
                .into()
        } else {
            // Up first asks the native editor to resolve its rendered rows.
            // While browsing, both arrows walk whole history entries so a
            // multiline recalled prompt cannot trap the original draft.
            let (recall_older, recall_newer) = self.history_navigation();
            // What is still waiting comes back before what was already sent,
            // but a walk through sent messages that has begun keeps the keys.
            // A press while one is already on its way back is still about
            // the queue, and is not left to fall through to that walk.
            let take_back =
                !recall_newer && (self.newest_take_back().is_some() || !self.recalling.is_empty());
            let viewing = self.image_viewer.is_some() && self.child_picker.is_none();
            editor
                .on_action(move |event| action(Action::Composer(target, event)))
                .on_cursor(move |cursor| action(Action::ComposerCursor(target, cursor)))
                .key_binding(move |press| {
                    // The transcript is under the viewer, so Ctrl+End stays
                    // in the draft rather than scroll what can't be seen.
                    if !viewing
                        && press.modifiers.command()
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
                    let picking = self.model_picker.is_some()
                        || self.account_picker
                        || self.permission_picker;
                    // Escape closes the topmost layer first: an open image
                    // above the draft goes before a running turn, which the
                    // next Escape interrupts.
                    if viewing
                        && !picking
                        && matches!(completion_key(&press), Some(CompletionKey::Dismiss))
                    {
                        return Some(text_editor::Binding::Custom(action(Action::CloseImage(
                            images::ViewerHost::Pane(target),
                        ))));
                    }
                    if snapshot.busy
                        && available
                        && !picking
                        && matches!(completion_key(&press), Some(CompletionKey::Dismiss))
                    {
                        return Some(text_editor::Binding::Custom(action(Action::Interrupt(
                            target,
                        ))));
                    }
                    if let Some(mode) =
                        submit_chord(&press, key_submit.0, key_submit.1, enter_sends)
                    {
                        let submit =
                            text_editor::Binding::Custom(action(Action::Submit(target, mode)));
                        // Once the question about the image is sent, the
                        // keyboard goes back to the image, so the next Escape
                        // closes it rather than interrupt the reply. Typing
                        // still reaches the composer.
                        return Some(if viewing {
                            text_editor::Binding::Sequence(vec![
                                submit,
                                text_editor::Binding::Custom(action(Action::FocusImage(target))),
                            ])
                        } else {
                            submit
                        });
                    }
                    if take_back && queue_key(&press, self.composer.is_empty()) {
                        return Some(text_editor::Binding::Custom(action(
                            Action::TakeBackQueued(target, None),
                        )));
                    }
                    if let Some(older) = history_key(&press, recall_older, recall_newer) {
                        return Some(text_editor::Binding::Custom(action(if older {
                            Action::ComposerUp(target)
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
        // Commands are reached by typing `/`, which is how every agent surface
        // a user arrives from works and what the composer already answers to.
        // A dropdown listing them duplicated that path in a control that was
        // permanently present for something used occasionally.
        // Stop is there whenever the conversation works, turn or not: it is
        // the way out of a turn that is slow to begin.
        let submissions: Vec<Element<'_, Message>> =
            composer_trailing(modes, snapshot.busy, has_draft)
                .into_iter()
                .map(|control| match control {
                    Trailing::Submit { mode, slot } => {
                        let shown = match mode {
                            PromptMode::Steer if !self.turn_running() => PromptMode::Send,
                            mode => mode,
                        };
                        let (name, tooltip) = submit_control(shown, slot, enter_sends, unsendable);
                        crate::ui_controls::icon_button(name, tooltip)
                            .id(card_control_id(target.card, &format!("chat-submit-{slot}")))
                            .selected(slot == 0)
                            .tone(crate::design::Tone::Themed(if slot == 0 {
                                design::accent_color
                            } else {
                                design::muted_icon_color
                            }))
                            .on_press_maybe(
                                (available
                                    && !self.start_blocked.is_some()
                                    && !self.provider_coming_up())
                                .then(|| action(Action::SubmitControl(target, mode))),
                            )
                            .into()
                    }
                    Trailing::Stop => self.stop_control(available),
                })
                .collect();
        // One row: `+`, the draft, then what sends or stops it. The controls
        // hold the row's foot, so a draft that grows keeps them beside its
        // last line, and the inset centers them on a single line of text.
        let control = |control: Element<'a, Message>| -> Element<'a, Message> {
            widget::container(control)
                .padding(iced::Padding::ZERO.bottom(COMPOSER_CONTROL_INSET))
                .into()
        };
        let (plus, menu) = match self.composer_actions(transitioning) {
            Some((plus, menu)) => (Some(plus), menu),
            None => (None, None),
        };
        let mut row: Vec<Element<'_, Message>> = Vec::new();
        row.extend(plus.map(control));
        row.push(editor);
        row.extend(submissions.into_iter().map(control));
        // The editor and its controls are one object: a single raised
        // surface, so the composer reads as one place to type rather than a
        // field with a toolbar bolted under it. The `+` menu opens above it,
        // over the timeline, leaving the composer where it is.
        content.push(crate::interaction::composer_popover(
            widget::container(
                widget::column::with_children(vec![
                    widget::column::with_children(
                        self.queue_view()
                            .into_iter()
                            .chain(self.restored_waiting_notice())
                            // Above the draft, so the row with Stop keeps
                            // its place for the second press.
                            .chain(self.stop_confirmation())
                            .chain((!attachment_chips.is_empty()).then(|| {
                                widget::column::with_children(attachment_chips)
                                    .spacing(step::GAP_XS)
                                    .padding([0, step::GAP_M as u16])
                                    .into()
                            })),
                    )
                    .spacing(step::GAP_XS)
                    .into(),
                    widget::row::with_children(row)
                        .spacing(step::GAP_XS)
                        .align_y(Alignment::End)
                        .padding([0.0, COMPOSER_TOOLBAR_PADDING])
                        .into(),
                    suggestion_hint,
                ])
                .width(Length::Fill),
            )
            .id(card_control_id(target.card, "chat-composer-surface"))
            .class(design::surface(
                Some(composer_fill),
                [design::RADIUS_M; 4],
                true,
            ))
            .width(Length::Fill),
            menu,
            action(Action::ComposerActions(target, false)),
        ));
        let usage = &snapshot.usage;
        // Keep the branch and account together; model and permissions share
        // the trailing group with usage at every footer width.
        let fit = FooterFit::for_width(content_width);
        let mut status: Vec<Element<'_, Message>> = Vec::new();
        if fit == FooterFit::Wide
            && let Some(branch) = &place.branch
        {
            status.push(bounded(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(crate::sidebar::worktree_icon(), 12),
                    crate::ui_controls::FadeText::caption(branch.clone())
                        .limit(FOOTER_LABEL)
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
                branch,
                "Branch",
                FOOTER_LABEL,
            ));
        }
        let mut engine = Vec::new();
        if !self.closed_elsewhere {
            let (context, controls) =
                self.session_controls(transitioning, accounts, offers, shortcuts, fit);
            status.extend(context);
            engine = controls;
        }
        let notice = permissions::notice(snapshot).map(|notice| {
            widget::button::custom(
                widget::row::with_children(
                    std::iter::once(crate::ui_controls::passive_icon(
                        Icon::Warning,
                        design::COMPACT_ICON_SIZE,
                    ))
                    .chain((fit == FooterFit::Wide).then(|| {
                        design::caption(notice)
                            .class(cosmic::theme::Text::Custom(design::warning_text))
                            .wrapping(iced::widget::text::Wrapping::None)
                            .into()
                    })),
                )
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
            .into()
        });
        // Money a reader is spending: two decimals, because four decimals of
        // a dollar is a number nobody reads.
        let cost = usage
            .cost_microusd
            .map(|cost| {
                let cost = format!("${:.2}", cost as f64 / 1_000_000.0);
                (cost.clone(), cost)
            })
            .or_else(|| usage.cost_estimate.map(cost_estimate_label));
        // Keep budget and cost together when the footer wraps.
        let mut spend: Vec<Element<'_, Message>> = Vec::new();
        let mut cost_shown = false;
        if usage.context_limit > 0 {
            // A context budget is a quantity a reader acts on - compact now,
            // or lose the thread - so it is drawn as a ring with its figure
            // beside it. The linear capsule this replaces spent the width of a
            // whole caption saying what the percentage already said.
            let used = (usage.context_tokens as f32 / usage.context_limit as f32).clamp(0.0, 1.0);
            // The raw counts are checked once, if ever, so they are held
            // here rather than spending a permanent run of status-line
            // width: the ring and its percentage are what a reader acts on.
            // The tightest pane says the cost here too, rather than drop it.
            let mut detail = format!(
                "{:.0}% of context · {} of {} tokens · {} in · {} out",
                used * 100.0,
                usage.context_tokens,
                usage.context_limit,
                usage.input_tokens,
                usage.output_tokens
            );
            if fit == FooterFit::Tight
                && let Some((label, _)) = &cost
            {
                detail.push_str(" · ");
                detail.push_str(label);
                cost_shown = true;
            }
            // The ring is a glyph; a screen reader hears what its tooltip
            // says, which in the tightest pane is also the only figure.
            let ring = crate::ui_controls::labelled(
                crate::ui_controls::budget_radial(used),
                detail.clone(),
            );
            spend.push(crate::interaction::tooltip_above(
                widget::row::with_children(std::iter::once(ring).chain(
                    (fit != FooterFit::Tight).then(|| {
                        design::caption(format!("{:.0}%", used * 100.0))
                            .wrapping(iced::widget::text::Wrapping::None)
                            .into()
                    }),
                ))
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center),
                detail,
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
        if !cost_shown && let Some((label, tooltip)) = cost {
            let caption =
                design::caption(label.clone()).wrapping(iced::widget::text::Wrapping::None);
            spend.push(if label == tooltip {
                caption.into()
            } else {
                crate::interaction::tooltip_above(caption, tooltip)
            });
        }
        let spend = widget::row::with_children(spend)
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .into();
        let mut leading = status;
        leading.extend(notice);
        engine.push(spend);
        let trailing: Element<'_, Message> = widget::row::with_children(engine)
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .into();
        let footer_row: Element<'_, Message> = if fit == FooterFit::Wide {
            // The leading group wraps when the place is long; the trailing
            // one holds the footer's far edge. The flex row stays shrink-wide
            // inside a filling container: a filling flex row hands its width
            // to every item as a minimum, setting each on a line of its own.
            widget::row::with_children(vec![
                widget::container(
                    widget::flex_row(leading)
                        .spacing(step::GAP_S as u16)
                        .align_items(Alignment::Center),
                )
                .width(Length::Fill)
                .into(),
                trailing,
            ])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into()
        } else {
            // The trailing controls keep their width while the account
            // group gives up the remaining space.
            // The filling container holds the trailing group at the far
            // edge, as in a wide pane, while the shrinkable inside it keeps
            // the leading group at its natural width.
            widget::row::with_children(vec![
                widget::container(crate::ui_controls::shrinkable(
                    widget::row::with_children(leading)
                        .spacing(step::GAP_S)
                        .align_y(Alignment::Center),
                ))
                .width(Length::Fill)
                .into(),
                trailing,
            ])
            .spacing(step::GAP_S)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into()
        };
        let footer = crate::ui_controls::scrolling_popover(
            footer_row,
            self.permission_picker
                .then(|| self.permissions_view(available, content_width - 32.0)),
            widget::popover::Position::Top,
            action(Action::PermissionPicker(target, false)),
        );
        let pane = widget::container(super::layout::pane(
            // The first block holds the transcript, which runs on down to
            // the next block when it ends that one: see `transcript` above.
            widget::column::with_children(content.into_iter().enumerate().map(|(index, child)| {
                if index == 0 {
                    child
                } else {
                    widget::container(child)
                        .padding(iced::Padding::from([0.0, step::GAP_M]).top(
                            if index == 1 && transcript_last {
                                0.0
                            } else {
                                step::GAP_S
                            },
                        ))
                        .width(Length::Fill)
                        .max_width(super::layout::READING_COLUMN)
                        .into()
                }
            }))
            .align_x(Alignment::Center)
            .height(Length::Fill)
            .width(Length::Fill)
            .into(),
            widget::container(footer)
                .id(card_control_id(target.card, "chat-footer"))
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
        let document = self.document_link_origin();
        crate::file_drop::target_with_hover(
            pane,
            ("chat", target.card),
            accepting_drops && !transitioning,
            move |payload| action(Action::Dropped(target, payload)),
            move |hovering| action(Action::DropHover(target, hovering)),
        )
        .map(move |message| capture_document_link(message, target, &document))
    }

    /// The page of history this conversation is waiting on, if any.
    fn paging(&self) -> Option<Page> {
        if self.disconnected {
            return None;
        }
        self.commands
            .iter()
            .find_map(|pending| pending.page)
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
            // The step in hand is pointed at, still: the transcript's one
            // activity indicator already says that the agent is working.
            let marker: Element<'_, Message> = match entry.status {
                chat::PlanState::Active => widget::icon(Icon::ChevronRight.handle())
                    .size(design::COMPACT_ICON_SIZE)
                    .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(|theme| {
                        cosmic::iced::widget::svg::Style {
                            color: Some(design::accent_color(theme)),
                        }
                    })))
                    .into(),
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

    /// The control that cancels the active turn.
    ///
    /// A provider whose interrupt does not spare background subagents takes
    /// them down with the turn. Saying so on the control is not enough for
    /// Esc, which is pressed without reading it, so such a cancel waits for a
    /// second press, and [`Self::stop_confirmation`] names what it stops.
    /// The control keeps its size and place while it waits, so the draft
    /// beside it doesn't rewrap and the second press lands where the first
    /// did.
    fn stop_control(&self, available: bool) -> Element<'_, Message> {
        let target = self.target;
        let at_stake = self.interrupt_casualties();
        let subagents = if at_stake == 1 {
            "subagent"
        } else {
            "subagents"
        };
        let armed = self.interrupt_armed.is_some() && at_stake > 0;
        let tooltip = if armed {
            Cow::Owned(format!(
                "Confirm: cancel the active turn and stop {at_stake} background {subagents} (Esc)"
            ))
        } else if at_stake > 0 {
            Cow::Owned(format!(
                "Cancel active turn (Esc). This also stops {at_stake} background {subagents}"
            ))
        } else if self.turn_running() {
            Cow::Borrowed("Cancel active turn (Esc)")
        } else {
            Cow::Borrowed(
                "Stop before the turn begins (Esc). Unsent messages return to the composer.",
            )
        };
        let mut control = crate::ui_controls::icon_button(Icon::Stop, tooltip)
            .id(card_control_id(target.card, "chat-stop"))
            .on_press_maybe(available.then(|| action(Action::Interrupt(target))));
        if armed {
            control = control
                .selected(true)
                .tone(crate::design::Tone::Themed(design::destructive_color));
        }
        control.into()
    }

    /// While a cancel that also stops background subagents waits for its
    /// second press, what that press stops.
    fn stop_confirmation(&self) -> Option<Element<'_, Message>> {
        let at_stake = self.interrupt_casualties();
        (self.interrupt_armed.is_some() && at_stake > 0).then(|| {
            let subagents = if at_stake == 1 {
                "subagent"
            } else {
                "subagents"
            };
            widget::container(
                widget::text(format!(
                    "Press Stop or Esc again to cancel the turn and stop {at_stake} background {subagents}"
                ))
                .size(design::TEXT_CAPTION)
                .class(cosmic::theme::Text::Custom(design::destructive_text)),
            )
            .padding(iced::Padding::from([space::XS, COMPOSER_PADDING]))
            .into()
        })
    }

    /// The `+` at the composer's leading edge, and the menu it opens while it
    /// is open.
    ///
    /// Attaching, steering timing and the conversation commands are each used
    /// now and then, so they share one entry point instead of each holding a
    /// toolbar slot beside the draft. `None` when the conversation offers none
    /// of them.
    fn composer_actions(
        &self,
        transitioning: bool,
    ) -> Option<(Element<'_, Message>, Option<Element<'_, Message>>)> {
        let target = self.target;
        let snapshot = &self.snapshot;
        let rows = self.composer_action_rows(transitioning);
        if rows.is_empty() && !self.composer_menu_searchable() {
            return None;
        }
        let open = self.composer_actions || self.branch_picker;
        let available = self.composer_available(transitioning);
        let enabled =
            available && snapshot.ready && !snapshot.busy && snapshot.read_only_reason.is_none();
        let label = if rows.iter().any(|(row, _)| *row == ComposerRow::Attach) {
            "Attach files and more actions"
        } else {
            "More actions"
        };
        let anchor = crate::ui_controls::icon_button(Icon::Plus, label)
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
                self.composer_menu_view(transitioning)
            };
            widget::container(content).width(COMPOSER_MENU_WIDTH).into()
        });
        Some((anchor.into(), popup))
    }

    /// The `+` menu's own list: a search when there are commands or skills
    /// to find, then the composer's actions, then the commands and skills
    /// the search matches.
    fn composer_menu_view(&self, transitioning: bool) -> Element<'_, Message> {
        let target = self.target;
        let snapshot = &self.snapshot;
        let entries = self.composer_menu(transitioning);
        // Enter in the search takes the first entry that can be pressed, so
        // that one is marked as the completion list marks its choice.
        let chosen = entries.iter().position(|(_, enabled)| *enabled);
        let rows: Vec<Element<'_, Message>> = entries
            .into_iter()
            .enumerate()
            .map(|(index, (entry, enabled))| match entry {
                MenuEntry::Row(row) => {
                    let pressed = enabled.then(|| action(self.composer_row_action(row)));
                    let (icon, label, description, name): (_, Cow<'_, str>, Cow<'_, str>, _) =
                        match row {
                            ComposerRow::Attach => (
                                Icon::Attachment,
                                "Attach files or images…".into(),
                                "Add to this draft".into(),
                                None,
                            ),
                            ComposerRow::Plan => {
                                let back = self.plan_toggle().unwrap_or(PLAN_MODE);
                                if back == PLAN_MODE {
                                    (
                                        Icon::List,
                                        "Plan mode".into(),
                                        "Explore and plan before making changes".into(),
                                        None,
                                    )
                                } else {
                                    (
                                        Icon::List,
                                        "Leave plan mode".into(),
                                        format!("Return to {}", permission_mode_label(back)).into(),
                                        None,
                                    )
                                }
                            }
                            ComposerRow::Steering => {
                                let wait = snapshot.interrupt_mode.as_deref() == Some("wait");
                                if wait {
                                    (
                                        Icon::Jump,
                                        "Steering: after turn".into(),
                                        "Switch to after tools".into(),
                                        Some(
                                            "Steering: after turn. Press to steer after tools instead",
                                        ),
                                    )
                                } else {
                                    (
                                        Icon::Jump,
                                        "Steering: after tools".into(),
                                        "Switch to after the turn".into(),
                                        Some(
                                            "Steering: after tools. Press to steer after the turn instead",
                                        ),
                                    )
                                }
                            }
                            ComposerRow::Fork => (
                                Icon::Branch,
                                "Fork conversation…".into(),
                                "Branch from an earlier turn".into(),
                                None,
                            ),
                            ComposerRow::Compact => (
                                Icon::Compress,
                                "Compact conversation".into(),
                                "Summarize earlier turns".into(),
                                Some(
                                    "Compact the conversation: summarize earlier turns to free context",
                                ),
                            ),
                        };
                    let highlighted = chosen == Some(index);
                    let mut button =
                        menu_row(Some(icon), label.clone(), description, None, highlighted)
                        .id(card_control_id(target.card, row.id()))
                        .on_press_maybe(pressed);
                    button = button.name(name.map_or(label, Cow::Borrowed));
                    button.into()
                }
                MenuEntry::Named(name) => {
                    let insertion = name.insertion.clone().into_owned();
                    let kind = completion_kind(name.kind);
                    let highlighted = chosen == Some(index);
                    menu_row(None, name.label(), name.description, Some(kind), highlighted)
                        .name(format!("{kind}: {}", name.label()))
                        .on_press_maybe(
                            enabled.then(|| action(Action::ComposerName(target, insertion))),
                        )
                        .into()
                }
            })
            .collect();
        let empty = rows.is_empty();
        // The rows scroll beneath the search field inside the popover.
        let list = crate::interaction::overflow_fade_y(
            widget::scrollable(
                widget::column::with_children(rows)
                    .padding(design::with_vertical_rail(iced::Padding::ZERO)),
            )
            .height(Length::Shrink),
            design::component_base,
        );
        let mut content: Vec<Element<'_, Message>> = Vec::new();
        if self.composer_menu_searchable() {
            content.push(
                crate::ui_controls::field::search(
                    "Search actions, commands and skills",
                    self.composer_actions_query.as_str(),
                )
                .id(card_control_id(target.card, COMPOSER_SEARCH))
                .width(Length::Fill)
                .on_input(move |query| action(Action::ComposerActionsQuery(target, query)))
                // Enter takes the first match that can be pressed.
                .on_submit(move |_| action(Action::ComposerActionsSubmit(target)))
                .into(),
            );
        }
        if empty {
            content.push(
                widget::container(
                    design::caption("Nothing matches")
                        .class(cosmic::theme::Text::Custom(design::muted_text)),
                )
                .padding([space::XS, step::GAP_S])
                .into(),
            );
        } else {
            content.push(
                widget::container(list)
                    .max_height(COMPOSER_MENU_CEILING)
                    .into(),
            );
        }
        widget::column::with_children(content)
            .spacing(space::XS)
            .into()
    }

    /// The account, model, reasoning effort, and permission mode, at the
    /// status line's own scale.
    ///
    /// They come back in two groups: the account beside the branch, and the
    /// model followed by its permission mode at the footer's trailing edge.
    ///
    /// These were three full-size dropdowns stacked above the composer, which
    /// gave settings a reader changes occasionally more room than the message
    /// they were about to send. While a session is switching they are drawn as
    /// plain text: the values still matter, and a row of disabled controls
    /// says nothing a reader can act on.
    fn session_controls<'a>(
        &'a self,
        transitioning: bool,
        accounts: Option<ChatAccounts<'a>>,
        offers: Vec<Offer<'a>>,
        shortcuts: &crate::settings::KeyboardShortcuts,
        fit: FooterFit,
    ) -> (Vec<Element<'a, Message>>, Vec<Element<'a, Message>>) {
        let target = self.target;
        let snapshot = &self.snapshot;
        let mut context: Vec<Element<'a, Message>> = Vec::new();
        let mut controls: Vec<Element<'a, Message>> = Vec::new();
        if let Some(accounts) = accounts {
            context.push(self.account_picker(accounts, transitioning, shortcuts, fit));
        }
        let draft = self.is_draft();
        if (snapshot.capabilities.model_selection && !snapshot.models.is_empty()) || draft {
            let default_effort = offers
                .iter()
                .find(|offer| offer.provider == self.session.launch.provider)
                .and_then(|offer| offer.catalog)
                .and_then(|catalog| catalog.default_effort(self.chosen_model().id));
            let label = footer_model_label(
                snapshot,
                self.chosen_model(),
                draft,
                self.session.launch.provider,
                fit,
                default_effort,
            );
            if transitioning {
                controls.push(bounded(
                    crate::ui_controls::FadeText::caption(label.clone())
                        .limit(fit.label_limit())
                        .into(),
                    &label,
                    "Model",
                    fit.label_limit(),
                ));
            } else {
                controls.push(self.model_picker(label, offers, shortcuts, fit));
            }
        }
        if !snapshot.capabilities.permission_modes.is_empty() {
            let modes = &snapshot.capabilities.permission_modes;
            let chosen = self.chosen_permission_mode();
            let selected = modes.iter().position(|mode| Some(mode.as_str()) == chosen);
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
                let anchor = widget::button::custom(
                    // Faded rather than wrapped where a narrow footer leaves
                    // the leading group little room; the chevron that says
                    // this is a picker keeps its place either way.
                    crate::ui_controls::trailing(
                        crate::ui_controls::FadeText::caption(
                            selected.map_or("Permissions", |index| {
                                permission_mode_label(&modes[index])
                            }),
                        ),
                        widget::icon(Icon::PanDown.handle()).size(design::COMPACT_ICON_SIZE),
                        step::GAP_XS,
                    ),
                )
                .name("Choose permission mode")
                .id(card_control_id(target.card, "chat-permission-picker"))
                .class(crate::ui_controls::control_style(open, None))
                .padding(CONTROL_PADDING)
                .on_press(action(Action::PermissionPicker(target, !open)));
                controls.push(crate::interaction::tooltip_above(
                    anchor,
                    crate::workspace_widget::shortcut_tooltip(
                        crate::workspace_widget::GlobalShortcut::ChatPermissions,
                        shortcuts,
                    ),
                ));
            }
        }
        (context, controls)
    }

    /// A draft can explicitly pin its account. Started conversations display
    /// their current account; automatic routing may hand off between turns.
    fn account_picker<'a>(
        &'a self,
        accounts: ChatAccounts<'a>,
        transitioning: bool,
        shortcuts: &crate::settings::KeyboardShortcuts,
        fit: FooterFit,
    ) -> Element<'a, Message> {
        let target = self.target;
        let open = self.account_picker && !transitioning;
        // A narrow footer shows the account as its avatar, named on hover
        // and to assistive technology, so the controls beside it keep room.
        let named = fit == FooterFit::Wide;
        let label = || -> Element<'a, Message> {
            crate::ui_controls::FadeText::caption(accounts.current).into()
        };
        let icon = Icon::Avatar.handle();
        if transitioning || !self.choosing() {
            // Manual choices are made before the first message. Automatic
            // routing can hand off later at a safe idle boundary.
            let avatar = crate::ui_controls::passive_icon(icon, design::COMPACT_ICON_SIZE);
            // Without its caption the avatar alone says nothing to a screen
            // reader, so it carries the account's name.
            let avatar = if named {
                avatar
            } else {
                crate::ui_controls::labelled(avatar, format!("Account: {}", accounts.current))
            };
            return crate::interaction::tooltip_above(
                widget::row::with_children(std::iter::once(avatar).chain(named.then(label)))
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
            widget::row::with_children(
                std::iter::once(widget::icon(icon).size(design::COMPACT_ICON_SIZE).into())
                    .chain(named.then(label))
                    .chain(std::iter::once(
                        widget::icon(Icon::PanDown.handle())
                            .size(design::COMPACT_ICON_SIZE)
                            .into(),
                    )),
            )
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center),
        )
        .name(if named {
            Cow::Borrowed("Choose an account")
        } else {
            Cow::Owned(format!("Choose an account, now {}", accounts.current))
        })
        .id(card_control_id(target.card, "chat-account-picker"))
        .class(crate::ui_controls::control_style(open, None))
        .padding(CONTROL_PADDING)
        .on_press(action(Action::AccountPicker(target, !open)));
        let shortcut = crate::workspace_widget::shortcut_tooltip(
            crate::workspace_widget::GlobalShortcut::ChatAccount,
            shortcuts,
        );
        let anchor = crate::interaction::tooltip_above(
            anchor,
            if named {
                shortcut
            } else {
                format!("Account: {} · {shortcut}", accounts.current)
            },
        );
        let choices = open.then(|| {
            widget::column::with_children(
                accounts
                    .choices
                    .into_iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        let mut row: Vec<Element<'a, Message>> = Vec::new();
                        match choice.usage_fraction {
                            Some(fraction) => row.push(crate::ui_controls::budget_radial_toned(
                                fraction,
                                choice.tone.color().map(design::Tone::Themed),
                            )),
                            None => row.push(
                                iced::widget::Space::new()
                                    .width(design::RADIAL_GAUGE_SIZE)
                                    .into(),
                            ),
                        }
                        let mut text: Vec<Element<'a, Message>> =
                            vec![crate::ui_controls::fade_text(choice.label).into()];
                        if !choice.detail.is_empty() {
                            text.push(
                                design::caption(choice.detail)
                                    .class(choice.tone.text_class())
                                    .into(),
                            );
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
                        .on_press(action(Action::ChooseAccount(target, choice.id.clone())))
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
        fit: FooterFit,
    ) -> Element<'a, Message> {
        let target = self.target;
        let open = self.model_picker.is_some();
        let label = if self
            .snapshot
            .fast_mode
            .as_ref()
            .is_some_and(|mode| mode.enabled())
        {
            Cow::Owned(format!("{label} · Fast"))
        } else {
            label
        };
        // An unlisted model is named by its id, which has no bound. Cut, the
        // trailing group stays narrow enough that the footer's leading group
        // keeps a column, and the tooltip says the whole name.
        let shortcut = crate::workspace_widget::shortcut_tooltip(
            crate::workspace_widget::GlobalShortcut::ChatModel,
            shortcuts,
        );
        // The tightest footer shows the provider's icon alone, and says the
        // model on hover and to assistive technology.
        let named = fit != FooterFit::Tight;
        let limit = fit.label_limit();
        let (mut tooltip, name) = if !named || label.chars().count() > limit {
            (
                format!("Model: {label} · {shortcut}"),
                Cow::Owned(format!("Choose a model, now {label}")),
            )
        } else {
            (shortcut, Cow::Borrowed("Choose a model"))
        };
        if !self.is_draft()
            && let Some(choice) = staged_model_choice(&self.snapshot, self.chosen_model())
        {
            tooltip = format!("{choice} applies from the next turn");
        }
        let anchor = widget::button::custom(
            widget::row::with_children(
                std::iter::once(
                    widget::icon(crate::ui_controls::provider_icon(
                        self.session.launch.provider,
                    ))
                    .size(design::COMPACT_ICON_SIZE)
                    .into(),
                )
                .chain(named.then(|| {
                    crate::ui_controls::FadeText::caption(label)
                        .limit(limit)
                        .into()
                }))
                .chain(std::iter::once(
                    widget::icon(Icon::PanDown.handle())
                        .size(design::COMPACT_ICON_SIZE)
                        .into(),
                )),
            )
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center),
        )
        .name(name)
        .id(card_control_id(target.card, "chat-model-picker"))
        .class(crate::ui_controls::control_style(open, None))
        .padding(CONTROL_PADDING)
        .on_press(action(Action::ModelPicker(target, !open)));
        let anchor = crate::interaction::tooltip_above(anchor, tooltip);
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
        let catalog = offers
            .iter()
            .find(|offer| offer.provider == provider)
            .and_then(|offer| offer.catalog);
        let offers = if draft {
            offers
        } else {
            vec![Offer {
                provider,
                models: &self.snapshot.models,
                catalog,
            }]
        };
        let models = model_list(
            self.target,
            &offers,
            draft,
            (
                provider,
                self.chosen_model()
                    .model(&self.snapshot)
                    .map(|model| model.id.as_str())
                    .or(self.chosen_model().id),
            ),
            query,
        );
        let mut sections = vec![models];
        if self.snapshot.capabilities.reasoning_selection
            && let Some(model) = self.chosen_model().model(&self.snapshot)
            && !model.reasoning.is_empty()
        {
            let default_effort = reasoning_default(&self.snapshot, model, catalog);
            let mut choices = vec![picker_section_header(
                widget::row::with_children(vec![
                    iced::widget::Space::new()
                        .width(f32::from(design::COMPACT_ICON_SIZE))
                        .into(),
                    design::caption("Reasoning effort")
                        .class(cosmic::theme::Text::Custom(design::muted_text))
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
                step::GAP_XS,
            )];
            choices.extend(
                reasoning_rows(model, default_effort)
                    .into_iter()
                    .enumerate()
                    .map(|(index, (effort, title))| {
                        picker_choice_row(
                            title.clone(),
                            None,
                            title,
                            card_control_id(
                                self.target.card,
                                &match effort {
                                    None => "chat-reasoning-choice-default".into(),
                                    Some(_) => format!("chat-reasoning-choice-{}", index - 1),
                                },
                            ),
                            effort == self.chosen_model().reasoning,
                            action(Action::Reasoning(
                                self.target,
                                model.id.clone(),
                                effort.map(str::to_owned),
                            )),
                        )
                    }),
            );
            sections.push(
                widget::column::with_children(choices)
                    .padding(design::with_vertical_rail(iced::Padding::ZERO))
                    .into(),
            );
        }
        let Some(mode) = &self.snapshot.fast_mode else {
            return Some(
                widget::column::with_children(sections)
                    .width(design::MODEL_POPUP_WIDTH)
                    .spacing(step::GAP_S)
                    .into(),
            );
        };
        let available = !self.disconnected
            && !self.snapshot.exited
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
                    Command::SetFastMode {
                        enabled: !mode.enabled(),
                    },
                ))
            })),
            fast_mode_tooltip(mode),
        );
        sections.push(control);
        Some(
            widget::column::with_children(sections)
                .width(design::MODEL_POPUP_WIDTH)
                .spacing(step::GAP_XS)
                .into(),
        )
    }

    /// One exchange's rows in a virtual transcript: each prompt, the step
    /// strip and each message is a row of its own, with its own key,
    /// fingerprint, height estimate and the item anchors find and scrolling
    /// address.
    ///
    /// A row is kept between frames, and built and laid out again only when
    /// something it draws changes, which is what stops a streaming answer
    /// from paying for every exchange above it twenty times a second. Rows
    /// are kept per message rather than per exchange, so an exchange that
    /// gains a message, or whose answer streams, pays for that message alone
    /// rather than for every message the exchange already holds.
    fn push_turn_rows<'a>(
        &'a self,
        transcript: &mut VirtualRows<'a>,
        turn: &'a turn::Turn,
        content_width: f32,
        settings: markdown::Settings,
        dark: bool,
    ) {
        use std::hash::{Hash, Hasher};
        let items = &self.snapshot.items;
        let now = self.source_now_ms();
        let common = self.turn_common_hasher(turn, dark);
        // Find reaches an exchange through its first row, whether or not
        // that row is mounted, and so does anything drawn before it.
        // Room for the row's own item too, which is usually all it holds.
        let mut leading = Vec::with_capacity(2);
        leading.push(self.find_anchor(&turn.key));
        let mut rows = 0;
        self.for_each_turn_part(turn, |placement| {
            let (part, cached) = match placement {
                // An item that draws nothing of its own, such as one still
                // being prepared, is addressed by the row drawn before it.
                Placement::Unplaced(item) => {
                    let id = scroll::item_id(&item.id);
                    match rows {
                        0 => leading.push(id),
                        _ => transcript.alias_last(id),
                    }
                    return;
                }
                Placement::Row(part, cached) => (part, cached),
            };
            let key = self.turn_row_key(turn, part);
            let mut aliases = match rows {
                0 => std::mem::take(&mut leading),
                _ => Vec::with_capacity(1),
            };
            let (item, footer) = match part {
                TurnPart::Steps => {
                    aliases.reserve(turn.steps.len());
                    aliases.extend(
                        turn.steps
                            .iter()
                            .filter_map(|placed| items.get(placed.index))
                            .map(|item| scroll::item_id(&item.id)),
                    );
                    (None, 0.0)
                }
                TurnPart::Prompt(index) => {
                    let item = &items[index];
                    aliases.push(scroll::item_id(&item.id));
                    let footer = match item.content {
                        ItemContent::Delivery { .. } if !self.delivery_carries_actions(item) => 0.0,
                        _ => MESSAGE_ACTION_FOOTER,
                    };
                    (Some(item), footer)
                }
                TurnPart::Card(prose) | TurnPart::Prose(prose) => {
                    let placed = &turn.prose[prose];
                    let item = &items[placed.index];
                    aliases.push(scroll::item_id(&item.id));
                    let footer = match (part, cached) {
                        (TurnPart::Prose(_), Some(cached)) => {
                            self.prose_action_footer(item, cached, placed.slot)
                        }
                        _ => 0.0,
                    };
                    (Some(item), footer)
                }
            };
            // An exchange the reader or another agent opened is set apart
            // from the one before it. Every other row is spaced by what it
            // and the row above it are, so the timeline reads as one list
            // across the groups it is folded into.
            let rhythm = match part {
                // What another agent or a background task sent is a line
                // of the timeline, like the steps that started it.
                TurnPart::Prompt(index)
                    if matches!(items[index].content, ItemContent::Delivery { .. }) =>
                {
                    Rhythm::Line
                }
                _ if rows == 0 && !turn.prompt.is_empty() => Rhythm::Opening,
                TurnPart::Prompt(_) => Rhythm::Block,
                TurnPart::Steps | TurnPart::Card(_) => Rhythm::Line,
                TurnPart::Prose(prose) => {
                    let placed = &turn.prose[prose];
                    Rhythm::of(&items[placed.index], placed.slot, false)
                }
            };
            let estimate = item.map_or(TIMELINE_ROW, |item| {
                row_estimate(item, rhythm, footer, content_width)
            });
            rows += 1;
            let fingerprint = self.turn_part_fingerprint(turn, part, key, cached, &common, now);
            transcript.lazy(
                key,
                fingerprint,
                estimate,
                aliases,
                Some(rhythm),
                footer,
                move || {
                    retained::kept(fingerprint, move || {
                        self.turn_part_view(turn, part, settings)
                    })
                },
            );
        });
        if rows == 0 {
            // An exchange that draws nothing yet is still reached through a
            // row of its own, which takes no room: the rows around it are
            // spaced as if it were not there.
            let mut key = std::hash::DefaultHasher::new();
            (0u8, &turn.key).hash(&mut key);
            transcript.lazy(
                key.finish(),
                common.finish(),
                0.0,
                leading,
                None,
                0.0,
                || widget::Space::new().into(),
            );
        }
    }

    /// The newest exchanges, read-only, for a glance from outside the pane:
    /// the same rows as the transcript, bottom-aligned in whatever height the
    /// caller gives them, with what the agent is doing now beneath. It holds
    /// nothing while the chat's transcript is not loaded.
    pub(in crate::app) fn transcript_preview(&self) -> Option<Element<'_, Message>> {
        if self.turns.is_empty() {
            return None;
        }
        let dark = cosmic::theme::is_dark();
        let markdown_settings =
            design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
        let mut transcript = VirtualRows::new();
        // The preview shows the end of the conversation, which the last few
        // exchanges fill however long it has run.
        let tail = self.turns.len().saturating_sub(PREVIEW_TURNS);
        for turn in &self.turns[tail..] {
            self.push_turn_rows(
                &mut transcript,
                turn,
                super::layout::READING_COLUMN,
                markdown_settings,
                dark,
            );
        }
        // The preview is a picture of the chat, so it carries no failure
        // control to press.
        if let Some((glyph, label)) = self.timeline_activity(false, None) {
            transcript.push(self.closing_row(glyph, label, None, None));
        }
        Some(virtualized::column(transcript.children, Default::default()))
    }

    /// One exchange alone, every row of it mounted, as the transcript draws
    /// it.
    #[cfg(test)]
    fn turn_view<'a>(
        &'a self,
        turn: &'a turn::Turn,
        settings: markdown::Settings,
        dark: bool,
    ) -> Element<'a, Message> {
        self.turn_view_in(
            turn,
            settings,
            dark,
            virtualized::Viewport {
                top: 0.0,
                height: 1.0e9,
                bottom: false,
            },
        )
    }

    /// One exchange alone, with the rows within `viewport` mounted.
    #[cfg(test)]
    fn turn_view_in<'a>(
        &'a self,
        turn: &'a turn::Turn,
        settings: markdown::Settings,
        dark: bool,
        viewport: virtualized::Viewport,
    ) -> Element<'a, Message> {
        let mut rows = VirtualRows::new();
        self.push_turn_rows(
            &mut rows,
            turn,
            super::layout::READING_COLUMN,
            settings,
            dark,
        );
        let window = std::rc::Rc::new(std::cell::Cell::new(Some(viewport)));
        virtualized::column(rows.children, window)
    }

    #[cfg(test)]
    pub(super) fn find_turn_view_for_test(&self) -> Element<'_, Message> {
        let settings = design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
        self.turn_view(&self.turns[0], settings, cosmic::theme::is_dark())
    }

    pub(super) fn find_turn_matches(&self, key: &str) -> bool {
        self.find_matches.contains(key)
    }

    /// Everything an exchange draws, as one value: it changes when any of
    /// the exchange's rows would be drawn again.
    #[cfg(test)]
    fn turn_fingerprint(&self, turn: &turn::Turn, dark: bool) -> u64 {
        self.turn_fingerprint_at(turn, dark, self.source_now_ms())
    }

    #[cfg(test)]
    fn turn_fingerprint_at(&self, turn: &turn::Turn, dark: bool, now: Option<u64>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = self.turn_common_hasher(turn, dark);
        self.turn_row_fingerprints_at(turn, dark, now)
            .hash(&mut hasher);
        hasher.finish()
    }

    /// Each row an exchange draws, with the value that decides whether it
    /// is drawn again.
    #[cfg(test)]
    fn turn_row_fingerprints_at(
        &self,
        turn: &turn::Turn,
        dark: bool,
        now: Option<u64>,
    ) -> Vec<(TurnPart, u64)> {
        let common = self.turn_common_hasher(turn, dark);
        let mut rows = Vec::new();
        self.for_each_turn_part(turn, |placement| {
            if let Placement::Row(part, cached) = placement {
                let key = self.turn_row_key(turn, part);
                rows.push((
                    part,
                    self.turn_part_fingerprint(turn, part, key, cached, &common, now),
                ));
            }
        });
        rows
    }

    /// Visits, in order, where each item of an exchange is drawn: the rows
    /// it draws, with the prepared body each draws from, and the items that
    /// draw none of their own.
    fn for_each_turn_part<'a>(&'a self, turn: &turn::Turn, mut visit: impl FnMut(Placement<'a>)) {
        let items = &self.snapshot.items;
        for &index in &turn.prompt {
            let Some(item) = items.get(index) else {
                continue;
            };
            let cached = self.cache.get(&item.id);
            // A delivery is drawn once its body is parsed, as an answer is.
            if matches!(item.content, ItemContent::Delivery { .. }) && cached.is_none() {
                visit(Placement::Unplaced(item));
            } else {
                visit(Placement::Row(TurnPart::Prompt(index), cached));
            }
        }
        let steps = || {
            turn.steps
                .iter()
                .filter_map(|placed| items.get(placed.index))
        };
        // A step shown as a failure is drawn as one rather than on the
        // strip.
        if steps().any(|item| self.failure_ui.item(&item.id).is_none()) {
            visit(Placement::Row(TurnPart::Steps, None));
        } else {
            steps().for_each(|item| visit(Placement::Unplaced(item)));
        }
        for (position, placed) in turn.prose.iter().enumerate() {
            let Some(item) = items.get(placed.index) else {
                continue;
            };
            let cached = Some(item)
                .filter(|item| self.failure_ui.item(&item.id).is_none())
                .and_then(|item| self.cache.get(&item.id));
            visit(match cached {
                Some(prepared)
                    if placed.slot == turn::Slot::Card && prepared.coordination.is_some() =>
                {
                    Placement::Row(TurnPart::Card(position), cached)
                }
                Some(_) => Placement::Row(TurnPart::Prose(position), cached),
                None => Placement::Unplaced(item),
            });
        }
    }

    /// What names a row of an exchange from one frame to the next: the item
    /// it draws, or the exchange whose steps its strip folds.
    fn turn_row_key(&self, turn: &turn::Turn, part: TurnPart) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut key = std::hash::DefaultHasher::new();
        match part {
            TurnPart::Steps => (1u8, &turn.key).hash(&mut key),
            TurnPart::Prompt(index) => (2u8, &self.snapshot.items[index].id).hash(&mut key),
            TurnPart::Card(prose) | TurnPart::Prose(prose) => {
                (3u8, &self.snapshot.items[turn.prose[prose].index].id).hash(&mut key)
            }
        }
        key.finish()
    }

    /// What every row of an exchange draws alike, ready for each row to add
    /// its own.
    fn turn_common_hasher(&self, turn: &turn::Turn, dark: bool) -> std::hash::DefaultHasher {
        use std::hash::Hash;
        let mut hasher = std::hash::DefaultHasher::new();
        (
            self.target,
            dark,
            self.syntax_theme as u8,
            self.failure_ui.revision,
        )
            .hash(&mut hasher);
        for incident in &self.failure_ui.incidents {
            self.failure_ui
                .expanded
                .contains(&incident.key)
                .hash(&mut hasher);
        }
        turn.key.hash(&mut hasher);
        self.find
            .as_ref()
            .filter(|_| self.find_turn_matches(&turn.key))
            .map(|_| {
                (
                    self.find_navigation.search_epoch(),
                    find_navigation::render_revision(self.target),
                )
            })
            .hash(&mut hasher);
        hasher
    }

    /// Everything one row of an exchange draws, folded into the value that
    /// decides whether it is drawn again.
    ///
    /// It reads the items' change tokens rather than their bodies, so it
    /// costs the same for a one-line answer as for a long one.
    fn turn_part_fingerprint(
        &self,
        turn: &turn::Turn,
        part: TurnPart,
        key: u64,
        cached: Option<&CachedItem>,
        common: &std::hash::DefaultHasher,
        now: Option<u64>,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let items = &self.snapshot.items;
        let mut hasher = common.clone();
        // The key names the row's own item, or its exchange's strip.
        key.hash(&mut hasher);
        let coordinating = match part {
            TurnPart::Prompt(index) => {
                0u8.hash(&mut hasher);
                self.hash_turn_item(index, cached, None, now, &mut hasher)
            }
            TurnPart::Steps => {
                1u8.hash(&mut hasher);
                (
                    &turn.summary,
                    self.strip_status(turn).map(|status| status as u8),
                )
                    .hash(&mut hasher);
                self.opened.get(&turn.key).hash(&mut hasher);
                self.tool_groups_expanded.hash(&mut hasher);
                // A running group's duration counts up, so its strip changes
                // with the clock; a finished one's is fixed at how long the
                // work took.
                now.and_then(|now| turn.working_for(now))
                    .map(|millis| if turn.running { millis / 1_000 } else { millis })
                    .hash(&mut hasher);
                let mut coordinating = false;
                for placed in &turn.steps {
                    let Some(item) = items.get(placed.index) else {
                        continue;
                    };
                    item.id.hash(&mut hasher);
                    coordinating |= self.hash_turn_item(
                        placed.index,
                        self.cache.get(&item.id),
                        Some(placed.slot),
                        now,
                        &mut hasher,
                    );
                }
                coordinating
            }
            TurnPart::Card(prose) | TurnPart::Prose(prose) => {
                2u8.hash(&mut hasher);
                let placed = &turn.prose[prose];
                self.hash_turn_item(placed.index, cached, Some(placed.slot), now, &mut hasher)
            }
        };
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
            // The client replaces its map whenever it learns an address, and a
            // new handle brings its own.
            std::sync::Arc::as_ptr(&self.agent_addresses).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// What one item of an exchange, drawn from `cached`, contributes to its
    /// row's fingerprint beside the item's identity, which the caller adds,
    /// and whether it coordinates other agents, so that the row also tracks
    /// them.
    fn hash_turn_item(
        &self,
        index: usize,
        cached: Option<&CachedItem>,
        slot: Option<turn::Slot>,
        now: Option<u64>,
        hasher: &mut std::hash::DefaultHasher,
    ) -> bool {
        use std::hash::Hash;
        let item = &self.snapshot.items[index];
        // The latest observation can change at provider heartbeat speed even
        // when the rendered content and elapsed second are unchanged.
        (
            item.started_at.or(item.at),
            slot,
            self.item_expanded(&item.id),
        )
            .hash(hasher);
        if item.content.is_step() {
            disclosure::item_duration_ms(item, now)
                .map(|millis| millis / 1_000)
                .hash(hasher);
        }
        if slot == Some(turn::Slot::Thinking) {
            // A newer turn starting does not revive old reasoning that
            // already has another item after it.
            let finished = reasoning_finished(
                &self.snapshot.items,
                self.snapshot.busy && !self.snapshot.has_later,
                index,
            );
            (
                finished,
                reasoning_elapsed(
                    &self.snapshot.items,
                    index,
                    finished,
                    self.snapshot
                        .finished_at
                        .filter(|_| !self.snapshot.has_later),
                ),
            )
                .hash(hasher);
        }
        // Only this item's badge changes on copy. Its dismissal timer does not
        // affect what the row draws.
        self.copied
            .as_ref()
            .is_some_and(|(id, _)| *id == item.id)
            .hash(hasher);
        cached
            .map(|cached| {
                (
                    cached.token,
                    cached.revision,
                    // Fixed footer strings change only when the displayed
                    // minute/date changes, not on every provider heartbeat.
                    cached.stamp.as_ref().map(|(_, label, full)| (label, full)),
                    cached.image.as_ref().map(iced::widget::image::Handle::id),
                    cached.image_source.is_some(),
                    self.preview_request.as_deref() == Some(item.id.as_str()),
                    cached.image_wait.is_some(),
                    cached.image_unavailable,
                )
            })
            .hash(hasher);
        // What a Markdown image draws lives with the conversation, so a
        // preview that finished decoding redraws the row showing it.
        if cached.is_some_and(CachedItem::shows_figures) {
            self.figures.version().hash(hasher);
        }
        // A coordinating step names agents by what this conversation calls
        // them, links their transcripts while it lists them, tells how those
        // it left in the background are doing, and remembers which of its
        // parts the reader unfolded.
        if !cached.is_some_and(|cached| cached.coordination.is_some()) {
            return false;
        }
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
        unfolded.hash(hasher);
        true
    }

    /// The room a prose row leaves under it for its hover actions. A
    /// coordination row carries its actions in its context menu instead.
    fn prose_action_footer(&self, item: &chat::Item, cached: &CachedItem, slot: turn::Slot) -> f32 {
        if slot == turn::Slot::Thinking
            && !(self.item_expanded(&item.id)
                && cached
                    .reasoning
                    .as_ref()
                    .is_some_and(|preview| !preview.first.is_empty()))
        {
            return 0.0;
        }
        if matches!(item.content, ItemContent::Text { .. }) {
            MESSAGE_ACTION_FOOTER
        } else {
            0.0
        }
    }

    /// One row of an exchange, drawing `part` of it.
    fn turn_part_view(
        &self,
        turn: &turn::Turn,
        part: TurnPart,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        let _stage = chat_stage::scope(Stage::TurnBuild);
        let items = &self.snapshot.items;
        let placed = |index: usize| {
            let item = items.get(index)?;
            Some((item, self.cache.get(&item.id)?))
        };
        match part {
            TurnPart::Prompt(index) => {
                let Some(item) = items.get(index) else {
                    return widget::Space::new().into();
                };
                let body = match (&item.content, self.cache.get(&item.id)) {
                    // Never in the user's own bubble.
                    (ItemContent::Delivery { .. }, Some(cached)) => {
                        self.delivery_view(item, cached, settings)
                    }
                    _ => self.prompt_view(item),
                };
                scroll::item(&item.id, body)
            }
            TurnPart::Steps => {
                // A group of one step is that step. `for_each_turn_part`
                // places the strip only when a step is not drawn as a
                // failure, so a lone one here is drawn as a row.
                if let [only] = turn.steps.as_slice()
                    && let Some((item, cached)) = placed(only.index)
                {
                    return widget::container(scroll::item(
                        &item.id,
                        self.lone_step_view(turn, item, cached, settings),
                    ))
                    .id(scroll::steps_id(&item.id))
                    .into();
                }
                let strip = self.step_strip(turn, || {
                    turn.steps
                        .iter()
                        .filter_map(|placed_step| placed(placed_step.index))
                        .filter(|(item, _)| self.failure_ui.item(&item.id).is_none())
                        .map(|(item, cached)| {
                            let key = selection::logical_identity(&item.id, &[9]);
                            let revision = cached.revision;
                            // Header elements are cheap owned metadata; only a
                            // mounted row creates their widget tree and geometry.
                            let row = self.step_view(item, cached, settings);
                            let id = item.id.clone();
                            let mut row = virtualized::Row::once(
                                key,
                                revision,
                                step::GAP_XS + TIMELINE_ROW,
                                Some(scroll::item_id(&id)),
                                // The air above each row, so the strip's
                                // rows keep the pitch of the rows around
                                // it at both of its ends.
                                move || {
                                    widget::container(scroll::item(&id, row))
                                        .padding(iced::Padding::ZERO.top(step::GAP_XS))
                                        .into()
                                },
                            );
                            row.anchor = true;
                            row
                        })
                        .collect()
                });
                // One measured strip supplies aliases for folded steps;
                // expanded rows below it supply their own, more precise item
                // bounds.
                match turn.steps.first().and_then(|step| items.get(step.index)) {
                    Some(item) => widget::container(strip)
                        .id(scroll::steps_id(&item.id))
                        .into(),
                    None => strip,
                }
            }
            TurnPart::Card(prose) | TurnPart::Prose(prose) => {
                let slot = turn.prose[prose].slot;
                let Some((item, cached)) = placed(turn.prose[prose].index) else {
                    return widget::Space::new().into();
                };
                let body = match (&cached.coordination, &item.content) {
                    (Some(coordinated), _) if part == TurnPart::Card(prose) => {
                        let raw = self
                            .item_expanded(&item.id)
                            .then(|| self.step_detail(item, cached, settings));
                        self.coordination_card(item, coordinated, raw)
                    }
                    (_, ItemContent::Text { .. }) => {
                        self.text_view(turn.prose[prose].index, item, cached, slot, settings)
                    }
                    (_, ItemContent::Image { .. }) => self.image_preview(item, cached),
                    // Anything else in the answer slot is a step the grouping
                    // did not recognize, drawn the way a step is.
                    _ => self.step_view(item, cached, settings),
                };
                scroll::item(&item.id, body)
            }
        }
    }

    /// What to call an attachment the reader just sent.
    ///
    /// A file has a name, and the name is all the reader needs: the directories
    /// above it are the ones they were just browsing. An inline attachment has
    /// no name at all, because its "path" is the payload itself, so the tail of
    /// it is base64 and the provider's description is the only thing that reads
    /// as a label.
    fn attachment_label<'a>(path: &'a str, description: &'a str) -> &'a str {
        if !path.starts_with("data:")
            && let Some(name) = path.rsplit('/').next().filter(|name| !name.is_empty())
        {
            return name;
        }
        if description.is_empty() {
            "Attachment"
        } else {
            description
        }
    }

    /// Says, where the queue was, that what a provider which did not come
    /// back was holding is now the reader's draft, and was not sent again.
    fn restored_waiting_notice(&self) -> Option<Element<'_, Message>> {
        let restored = self.restored_waiting?;
        let text = restored.caption();
        Some(
            widget::container(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(Icon::History, design::COMPACT_ICON_SIZE),
                    design::caption(text).width(Length::Fill).into(),
                    small_icon(
                        Icon::Close,
                        "Dismiss",
                        Action::DismissRestoredWaiting(self.target),
                    ),
                ])
                .spacing(step::GAP_S)
                .align_y(Alignment::Center),
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
        let available = !self.disconnected && !self.snapshot.exited;
        let control = |name, label: &'static str, enabled: bool, message: Action| {
            crate::ui_controls::icon_button(name, label)
                .compact()
                .on_press_maybe(enabled.then(|| action(message)))
                .tone(crate::design::Tone::Themed(crate::design::muted_icon_color))
                .into()
        };
        let copy = |text: &str, source: QueueCopy| {
            (!text.is_empty()).then(|| {
                small_icon(
                    Icon::Copy,
                    "Copy queued message",
                    Action::CopyQueued(target, source),
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
            let preview = self
                .provider_queue
                .borrow_mut()
                .get(&self.snapshot, &queued.id);
            let mut controls: Vec<Element<'_, Message>> = preview
                .as_ref()
                .and_then(|preview| {
                    copy(
                        &queued.text,
                        QueueCopy::Provider(Arc::downgrade(preview), queued.id.clone()),
                    )
                })
                .into_iter()
                .collect();
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
                preview.as_ref().map_or_else(
                    || Cow::Borrowed("Message…"),
                    |preview| Cow::Owned(preview.preview.to_string()),
                ),
                &queued.attachments,
                controls,
            ));
        }
        // A message on its way to the provider's queue, or to steer the
        // turn, joins the list at once; the provider's own listing replaces
        // it once it has the message.
        for outgoing in self
            .commands
            .outgoing()
            .filter(|outgoing| !outgoing.in_transcript())
        {
            rows.push(queue_row(
                QueueTiming::Leaving,
                outgoing.preview.as_str().into(),
                &outgoing.prompt.attachments,
                copy(
                    outgoing.prompt.text(),
                    QueueCopy::Initial(Arc::downgrade(&outgoing.prompt.source)),
                )
                .into_iter()
                .collect(),
            ));
        }
        for followup in &self.followups {
            let entry = QueueEntry::Local(followup.id);
            let sending = self.followup_sending(followup.id);
            rows.push(queue_row(
                if sending {
                    QueueTiming::Leaving
                } else {
                    QueueTiming::NextTurn
                },
                followup.preview.as_str().into(),
                &followup.attachments,
                copy(&followup.text, QueueCopy::Local(followup.id))
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
        if sending
            || self
                .commands
                .outgoing()
                .any(|outgoing| !outgoing.in_transcript())
        {
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
                    // The list runs to the panel's foot, its inset scrolling
                    // with it, so a row passing the foot fades into the
                    // panel rather than stopping short of it.
                    widget::container(crate::interaction::overflow_fade_y(
                        widget::scrollable(
                            widget::column::with_children(rows)
                                .spacing(step::GAP_XS)
                                .padding(design::with_vertical_rail(
                                    iced::Padding::ZERO.bottom(step::GAP_S),
                                )),
                        )
                        .id(card_control_id(self.target.card, "chat-queue"))
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .height(Length::Shrink),
                        design::sunken,
                    ))
                    .max_height(QUEUE_CEILING + step::GAP_S)
                    .into(),
                ])
                .spacing(step::GAP_XS),
            )
            .width(Length::Fill)
            .padding(iced::Padding::new(step::GAP_S).bottom(0.0))
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_M, design::RADIUS_M, 0.0, 0.0],
                false,
            ))
            .into(),
        )
    }

    pub(super) fn message_copy(&self, item: &chat::Item) -> Element<'static, Message> {
        let mut controls = vec![small_icon(
            if self.copied.as_ref().is_some_and(|(id, _)| *id == item.id) {
                Icon::Check
            } else {
                Icon::Copy
            },
            "Copy this message",
            if item.logical_copy_body().is_some()
                && !self
                    .presentation_cache()
                    .get(&item.id)
                    .and_then(|cached| cached.coordination.as_deref())
                    .is_some_and(|card| card.is_card())
            {
                Action::CopyMessage(self.target, item.id.clone(), self.message_copy_source(item))
            } else if let Some(source) = self.excerpted_output_source(item) {
                Action::CopyStep(self.target, item.id.clone(), CopyPart::Text, source)
            } else {
                Action::Copy(self.target, item.id.clone(), CopyPart::Text)
            },
        )];
        if let Some((at, stamp, full)) = self
            .presentation_cache()
            .get(&item.id)
            .and_then(|cached| cached.stamp.as_ref())
            && item.at == Some(*at)
        {
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
        widget::row::with_children(controls)
            .spacing(step::GAP_XS)
            .align_y(Alignment::Center)
            .into()
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
            .commands
            .iter()
            .any(|pending| pending.queued == Some(super::QueuedStep::Prompt));
        self.local_prompt_view(&queued.prompt, waiting, Some(!sending))
    }

    /// A message of the reader's this window holds until its conversation
    /// has it: a first message while its conversation starts, or one sent
    /// and not yet shown.
    ///
    /// It is drawn where it will be once sent, as the reader's own message,
    /// with what it is waiting on under it: sending it cleared the composer,
    /// and a message that vanished until the provider answered would read
    /// as lost. `take_back` offers to return it to the composer, enabled or
    /// not, where that can be done.
    fn local_prompt_view<'a>(
        &'a self,
        local: &'a super::prompt::LocalPrompt,
        waiting: String,
        take_back: Option<bool>,
    ) -> Element<'a, Message> {
        // One message is prepared at a time; another, rarely, reads plain
        // until its turn.
        let prepared = self
            .local_prompt()
            .filter(|shown| Arc::ptr_eq(&shown.source, &local.source))
            .and(local.presentation.prepared.as_ref());
        let logical = selectable_prose::Logical {
            identity: selection::logical_identity("queued-prompt", &[2]),
            start: 0,
        };
        let body: Element<'a, Message> = if let Some(prepared) = prepared {
            let mut settings =
                design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
            settings.style.font = iced::Font::default();
            if prepared.full.len() > 2048 {
                let flow = local.presentation.flow.markdown_entry(
                    logical.identity,
                    prepared.full.clone(),
                    settings.style,
                );
                selectable_prose::flow_source(
                    settings.text_size,
                    settings.style.font,
                    self.target,
                    None,
                    logical,
                    flow,
                )
            } else {
                selectable_prose::logical_text(
                    prepared.full.spans(settings.style),
                    settings.text_size,
                    settings.style.font,
                    self.target,
                    None,
                    logical,
                )
            }
        } else {
            widget::text(prompt::preview(local.text()).0)
                .size(design::TEXT_TRANSCRIPT)
                .into()
        };
        let mut rows: Vec<Element<'a, Message>> = Vec::new();
        if !local.text().trim().is_empty() {
            rows.push(user_bubble(body, false, false));
        }
        for attachment in &local.attachments {
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
        let mut status: Vec<Element<'a, Message>> = vec![
            crate::ui_controls::activity_spinner(
                f32::from(design::COMPACT_ICON_SIZE),
                design::accent_color,
            ),
            design::caption(waiting)
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
            small_icon(
                Icon::Copy,
                // A message still to send is in the queue; one sent is the
                // reader's message like any other.
                if take_back.is_some() {
                    "Copy queued message"
                } else {
                    "Copy this message"
                },
                Action::CopyQueued(
                    self.target,
                    QueueCopy::Initial(Arc::downgrade(&local.source)),
                ),
            ),
        ];
        if let Some(enabled) = take_back {
            status.push(
                crate::ui_controls::icon_button(Icon::Undo, "Take the message back to edit it")
                    .compact()
                    .on_press_maybe(enabled.then(|| action(Action::CancelQueued(self.target))))
                    .into(),
            );
        }
        rows.push(
            widget::row::with_children(status)
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

    /// A user message's shown text, before its prepared form is ready: the
    /// text, whether it is an attachment, and whether it folds.
    fn prompt_text(&self, item: &chat::Item) -> (String, bool, bool) {
        if let ItemContent::Image {
            reference: Some(reference),
            ..
        } = &item.content
        {
            return (reference.label(), true, false);
        }
        let attachment = matches!(item.content, ItemContent::Image { .. });
        let text = match &item.content {
            ItemContent::Text { text, .. } => Cow::Borrowed(text.as_str()),
            ItemContent::Image {
                path, description, ..
            } => Cow::Borrowed(Self::attachment_label(path, description)),
            content => turn::step_label(content),
        };
        if attachment {
            (text.into_owned(), true, false)
        } else {
            let (preview, long) = prompt::preview(&text);
            (preview, false, long)
        }
    }

    /// The user's own message.
    ///
    /// Set on the accent wash and tucked to the right. That placement is what
    /// identifies the author without printing "You" above every message, and
    /// the narrower measure keeps a turn's opening findable when scrolling
    /// back.
    fn prompt_view(&self, item: &chat::Item) -> Element<'static, Message> {
        let prepared = self.prepared_prompt(item);
        let attachment = matches!(item.content, ItemContent::Image { .. });
        let image_text = self
            .prepared_literal(item)
            .and_then(literal_content::Content::image_prompt);
        // Pending cold/replaced sources show only their own bounded prefix.
        // Full literal text and catalog authority wait for the exact worker DTO.
        let (fallback, attachment, pending_long) = if attachment {
            (String::new(), true, false)
        } else if prepared.is_some() {
            (String::new(), false, false)
        } else {
            self.prompt_text(item)
        };
        let long = prepared.map_or(pending_long, |prompt| prompt.long);
        let expanded = self.item_expanded(&item.id);
        let logical = selectable_prose::Logical {
            identity: selection::logical_identity(&item.id, &[2]),
            start: 0,
        };
        let body: Element<'static, Message> = if attachment {
            // Empty unowned paths also represent documents and refused image
            // receipts. Keep their descriptor instead of a generic read error.
            if let Some(cached) = self.presentation_cache().get(&item.id)
                && matches!(&item.content, ItemContent::Image { path, reference, .. }
                    if reference.is_some() || !path.is_empty())
            {
                self.image_preview(item, cached)
            } else {
                widget::row::with_children(vec![
                    widget::icon(Icon::Attachment.handle())
                        .size(design::COMPACT_ICON_SIZE)
                        .into(),
                    if let Some(text) = image_text {
                        prose::paragraph(
                            design::markdown_settings(
                                &cosmic::theme::active(),
                                design::TEXT_CAPTION,
                            ),
                            text,
                            self.target,
                            None,
                            Some(logical),
                            self.presentation_cache()
                                .get(&item.id)
                                .map(|cached| &cached.flow_cache),
                        )
                    } else if let ItemContent::Image {
                        reference: Some(reference),
                        ..
                    } = &item.content
                    {
                        design::caption(reference.label()).into()
                    } else {
                        design::caption("Loading attachment…").into()
                    },
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into()
            }
        } else {
            let text = if let Some(prompt) = prepared {
                let text = prompt.shown(expanded);
                let mut settings =
                    design::markdown_settings(&cosmic::theme::active(), design::TEXT_TRANSCRIPT);
                settings.style.font = iced::Font::default();
                if text.len() > 2048 {
                    let flow = self.presentation_cache()[&item.id]
                        .flow_cache
                        .markdown_entry(logical.identity, text.clone(), settings.style);
                    selectable_prose::flow_source(
                        settings.text_size,
                        settings.style.font,
                        self.target,
                        None,
                        logical,
                        flow,
                    )
                } else {
                    selectable_prose::logical_text(
                        text.spans(settings.style),
                        settings.text_size,
                        settings.style.font,
                        self.target,
                        None,
                        logical,
                    )
                }
            } else {
                // This bounded nonselectable preview cannot certify a full
                // source/catalog before preparation accepts the current Item.
                widget::text(fallback).size(design::TEXT_TRANSCRIPT).into()
            };
            if long {
                widget::column::with_children(vec![
                    text,
                    disclosure::row(
                        if expanded { "Show less" } else { "Show more" }.to_owned(),
                        if expanded { "Show less" } else { "Show more" }.into(),
                        Icon::Chat,
                        None,
                        None,
                        Some(expanded),
                        Some(Action::Toggle(self.target, item.id.clone())),
                    ),
                ])
                .spacing(step::GAP_S)
                .into()
            } else {
                text
            }
        };
        let message = user_bubble(body, attachment, long);
        widget::container(message_actions(
            message,
            self.message_copy(item),
            Alignment::End,
        ))
        .align_x(Alignment::End)
        .width(Length::Fill)
        .into()
    }

    /// A delivery stays at the point where it prompted the next reply. The
    /// one-line disclosure names its source or outcome; literal notice output
    /// and full reports are available on demand.
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
        let expanded = self.item_expanded(&item.id);
        let notice = if *kind == chat::DeliveryKind::Notice {
            let Some(notice) = self
                .prepared_literal(item)
                .and_then(literal_content::Content::notice)
            else {
                return design::caption("Loading notice…").into();
            };
            Some(notice)
        } else {
            None
        };
        let heading = notice.map_or_else(
            || {
                chat::delivery_heading(
                    *kind,
                    from,
                    sender,
                    if !body.is_empty() && body.trim() == summary.trim() {
                        ""
                    } else {
                        summary
                    },
                )
            },
            |notice| notice.summary.plain().to_string(),
        );
        let (first, remainder) = heading
            .split_once(['\r', '\n'])
            .map_or((heading.as_str(), ""), |(first, rest)| {
                (first, rest.trim_start_matches(['\r', '\n']))
            });
        let monitor = delivery_monitor(*kind, summary);
        let has_body = delivery_has_body(notice, body, remainder, sender);
        // Only explicit command/report outcomes are errors. A peer's prose
        // mentioning a failure is ordinary delivered content. An agent that
        // ended with nothing to report, or whose result is shown elsewhere,
        // arrives as a notice under the same heading, and reads as a report.
        let report_outcome = match kind {
            chat::DeliveryKind::Report => true,
            chat::DeliveryKind::Notice => !monitor && delivery_command_outcome(first).is_none(),
            _ => false,
        }
        .then(|| delivery_report_outcome(from, first))
        .flatten();
        let failed =
            (*kind == chat::DeliveryKind::Notice && !monitor && delivery_command_failed(&heading))
                || report_outcome.is_some_and(|(_, outcome)| outcome == "failed");
        let glyph = if failed {
            Icon::Error
        } else if chat::is_stopped_notice(&item.content) {
            Icon::Stop
        } else if *kind == chat::DeliveryKind::Notice
            && !monitor
            && delivery_command_outcome(&heading).is_some()
        {
            Icon::Terminal
        } else if *kind == chat::DeliveryKind::Notice {
            Icon::Attention
        } else {
            Icon::Chat
        };
        let (title, outcome) = if *kind == chat::DeliveryKind::Notice && !monitor {
            delivery_command_outcome(first).or(report_outcome)
        } else {
            report_outcome
        }
        .map_or((first, None), |(title, outcome)| (title, Some(outcome)));
        // Bound synchronous text shaping. The original remains in the tooltip,
        // copy action and expanded detail; fades never introduce an ellipsis.
        let short: String = title.chars().take(1024).collect();
        // An agent's report is told by its glyph and outcome, so its row
        // names the agent as every row of the timeline does, by weight,
        // without the "Agent" and quotes the provider's heading wraps it in.
        // Its tooltip and accessible name keep the heading whole.
        let headline = match report_outcome.and_then(|_| delivery_agent_name(&short)) {
            Some(name) => disclosure::Headline::Name(name.to_owned()),
            None => disclosure::Headline::Text(short),
        };
        let disclosure = delivery_opens(
            has_body,
            monitor,
            notice.is_some_and(|notice| notice.body.long),
        );
        let heading_row = disclosure::row_named(
            headline,
            if disclosure {
                format!(
                    "{} delivery details: {heading}",
                    if expanded { "Hide" } else { "Show" }
                )
            } else {
                heading.clone()
            },
            glyph,
            failed.then_some(chat::ToolStatus::Failed),
            outcome.map(|outcome| disclosure::Trailing::Outcome(outcome.to_owned())),
            disclosure.then_some(expanded),
            disclosure.then(|| Action::Toggle(self.target, item.id.clone())),
        );
        let heading_row = crate::interaction::tooltip(heading_row, heading.clone());
        let mut rows = Vec::new();
        if (expanded || monitor) && has_body {
            if expanded
                && *kind != chat::DeliveryKind::Notice
                && !sender.is_empty()
                && !heading.contains(sender)
            {
                rows.push(
                    widget::container(selectable_prose::text(
                        vec![
                            iced::widget::text::Span::new(format!("Sender: {sender}"))
                                .color(design::muted_color(&cosmic::theme::active())),
                        ]
                        .into(),
                        design::TEXT_CAPTION.into(),
                        iced::Font::default(),
                        self.target,
                        self.find.as_deref(),
                    ))
                    .padding([0, step::GAP_M as u16])
                    .into(),
                );
            }
            if expanded && !remainder.is_empty() {
                rows.push(selectable_prose::plain(
                    remainder.to_owned(),
                    design::TEXT_CAPTION,
                    iced::Font::default(),
                    self.target,
                ));
            }
            if expanded && first.chars().nth(1024).is_some() {
                rows.push(selectable_prose::plain(
                    first.to_owned(),
                    design::TEXT_CAPTION,
                    iced::Font::default(),
                    self.target,
                ));
            }
            let detail = if notice.is_some() {
                literal_content::notice_body(self, item, cached, settings, None)
            } else {
                prose::view(cached, settings, self.target, self.find.as_deref())
            };
            rows.push(
                widget::container(detail)
                    .padding([0, step::GAP_M as u16])
                    .into(),
            );
            if expanded {
                let label = if *kind == chat::DeliveryKind::Report {
                    "Copy report"
                } else {
                    "Copy message"
                };
                rows.push(
                    widget::button::custom(design::caption(label))
                        .name(label)
                        .class(crate::ui_controls::control_style(false, None))
                        .on_press(action(Action::Copy(
                            self.target,
                            item.id.clone(),
                            CopyPart::Text,
                        )))
                        .into(),
                );
            }
        }
        // Like a step or a thought, a delivery that opens is a line of the
        // timeline until its detail is open. Open, or when it has nothing to
        // open, the whole message, its row included, carries the message's
        // actions beneath it, so hovering the row shows them.
        debug_assert_eq!(
            !rows.is_empty(),
            (expanded || monitor) && has_body,
            "delivery_actions mirrors the detail drawn here"
        );
        let body: Element<'static, Message> = if !delivery_actions(
            has_body,
            monitor,
            notice.is_some_and(|notice| notice.body.long),
            expanded,
        ) {
            heading_row
        } else {
            rows.insert(0, heading_row);
            message_actions(
                widget::column::with_children(rows)
                    .spacing(step::GAP_XS)
                    .into(),
                self.message_copy(item),
                Alignment::Start,
            )
        };
        widget::container(body)
            .max_width(design::READABLE_MEASURE)
            .width(Length::Fill)
            .into()
    }

    /// Whether a delivered message draws its actions, and so reserves the
    /// room for them beneath it: while its detail is shown, or always when
    /// it has no detail to open, as a message does.
    fn delivery_carries_actions(&self, item: &chat::Item) -> bool {
        let ItemContent::Delivery {
            kind,
            from,
            sender,
            summary,
            body,
        } = &item.content
        else {
            return false;
        };
        let monitor = delivery_monitor(*kind, summary);
        let (has_body, long) = if *kind == chat::DeliveryKind::Notice {
            // Drawn as loading, without actions, until it is prepared.
            let Some(notice) = self
                .prepared_literal(item)
                .and_then(literal_content::Content::notice)
            else {
                return false;
            };
            (
                delivery_has_body(Some(notice), body, "", sender),
                notice.body.long,
            )
        } else {
            let heading = chat::delivery_heading(
                *kind,
                from,
                sender,
                if !body.is_empty() && body.trim() == summary.trim() {
                    ""
                } else {
                    summary
                },
            );
            let remainder = heading
                .split_once(['\r', '\n'])
                .map_or("", |(_, rest)| rest.trim_start_matches(['\r', '\n']));
            (delivery_has_body(None, body, remainder, sender), false)
        };
        delivery_actions(has_body, monitor, long, self.item_expanded(&item.id))
    }

    /// An assistant answer, its reasoning, its narration, or a system notice.
    ///
    /// Every prose phase shares the message body size and Markdown renderer,
    /// including partial streaming paragraphs.
    fn prose_view<'a>(
        &'a self,
        index: usize,
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
        self.text_view(index, item, cached, slot, settings)
    }

    /// A text item in its slot, as an element that owns what it draws.
    /// `index` is the item's place in the transcript it is drawn from.
    fn text_view(
        &self,
        index: usize,
        item: &chat::Item,
        cached: &CachedItem,
        slot: turn::Slot,
        settings: markdown::Settings,
    ) -> Element<'static, Message> {
        #[cfg(test)]
        TEXT_VIEWS.with(|built| built.set(built.get() + 1));
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
            let (items, busy, finished_at) = self
                .selected_subagent
                .as_ref()
                .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id))
                .map_or(
                    (
                        self.snapshot.items.as_slice(),
                        self.snapshot.busy && !self.snapshot.has_later,
                        self.snapshot
                            .finished_at
                            .filter(|_| !self.snapshot.has_later),
                    ),
                    |agent| {
                        (
                            agent.items.as_slice(),
                            agent.status == chat::ToolStatus::Running,
                            None,
                        )
                    },
                );
            debug_assert!(
                items.get(index).is_none_or(|at| at.id == item.id),
                "a thought is drawn at its own position in its stream"
            );
            let finished = reasoning_finished(items, busy, index);
            let preview = cached.reasoning.as_ref();
            let has_text = preview.is_some_and(|preview| !preview.first.is_empty());
            let expanded = has_text && self.item_expanded(&item.id);
            let mut elapsed = reasoning_elapsed(items, index, finished, finished_at);
            let label = if has_text {
                let preview = preview.expect("reasoning text has a prepared preview");
                if finished {
                    &preview.first
                } else {
                    &preview.latest
                }
                .clone()
            } else if finished {
                // With no text to name it, the duration completes the
                // sentence rather than standing apart from it.
                match elapsed.take() {
                    Some(elapsed) => format!("Thought for {elapsed}"),
                    None => "Thought process".to_owned(),
                }
            } else {
                "Thinking".to_owned()
            };
            let name = format!("Reasoning: {label}");
            let header = disclosure::row(
                label,
                name,
                Icon::Agent,
                (!finished).then_some(chat::ToolStatus::Running),
                elapsed,
                has_text.then_some(expanded),
                has_text.then(|| Action::Toggle(target, item.id.clone())),
            );
            let mut rows = vec![header];
            if expanded {
                rows.push(
                    widget::container(message_actions(
                        prose::view(cached, settings, target, self.find.as_deref()),
                        self.message_copy(item),
                        Alignment::Start,
                    ))
                    .padding([step::GAP_S, step::GAP_M])
                    .max_width(design::READABLE_MEASURE)
                    .into(),
                );
            }
            let message =
                widget::container(widget::column::with_children(rows).spacing(step::GAP_S))
                    .width(Length::Fill)
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
                    .into();
            return message;
        }
        // The answer is bare prose on the pane's own surface. Wrapping it in
        // a panel would put a frame around the one thing the reader came for,
        // and would make it look like just another block.
        widget::container(message_actions(
            prose::view(cached, settings, self.target, self.find.as_deref()),
            self.message_copy(item),
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
    for (label, uri) in artifacts::links(request) {
        rows.push(crate::interaction::tooltip(
            widget::button::link(format!("Open artifact: {label}"))
                .on_press(action(Action::OpenDocument(target, uri.clone()))),
            uri,
        ));
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

/// Keep the provider's alias as a choice while naming the model it resolves to.
fn model_choice_label(model: &chat::Model) -> Cow<'_, str> {
    if model.id == "default" && !model.label.to_lowercase().starts_with("default") {
        format!("Default ({})", model.label).into()
    } else {
        model.label.as_str().into()
    }
}

/// Provider identifiers stay in the protocol; people see these names everywhere.
fn effort_label(effort: &str) -> Cow<'_, str> {
    match effort {
        "low" => "Low".into(),
        "medium" => "Medium".into(),
        "high" => "High".into(),
        "xhigh" => "Extra high".into(),
        "max" => "Max".into(),
        _ => effort.into(),
    }
}

fn reasoning_rows<'a>(
    model: &'a chat::Model,
    default: Option<&str>,
) -> Vec<(Option<&'a str>, Cow<'a, str>)> {
    let title = default.map_or_else(
        || Cow::Borrowed("Default"),
        |effort| Cow::Owned(format!("Default ({})", effort_label(effort))),
    );
    std::iter::once((None, title))
        .chain(
            model
                .reasoning
                .iter()
                .map(|effort| (Some(effort.as_str()), effort_label(effort))),
        )
        .collect()
}

fn model_matches_active(model: &chat::Model, active: &chat::ActiveModel) -> bool {
    model.id == active.id || active.aliases.contains(&model.id)
}

fn reasoning_default<'a>(
    snapshot: &'a chat::Snapshot,
    model: &chat::Model,
    catalog: Option<&'a crate::account::cache::Catalog>,
) -> Option<&'a str> {
    snapshot
        .active_model
        .as_ref()
        .filter(|active| model_matches_active(model, active))
        .and_then(|active| active.default_reasoning.as_deref())
        .or_else(|| catalog.and_then(|catalog| catalog.default_effort(Some(&model.id))))
}

fn staged_model_choice(snapshot: &chat::Snapshot, chosen: ChosenModel<'_>) -> Option<String> {
    let active = snapshot.active_model.as_ref()?;
    let model = chosen.model(snapshot);
    let selected = model.map(|model| model.id.as_str()).or(chosen.id);
    let model_changed = selected.is_some_and(|id| id != active.id)
        && !model.is_some_and(|model| model_matches_active(model, active));
    let default = active.default_reasoning.as_deref();
    let effort_changed = chosen.reasoning.or(default) != active.reasoning.as_deref().or(default);
    let mut choices = Vec::new();
    if model_changed {
        choices.push(model.map_or_else(
            || selected.unwrap_or("Default").to_owned(),
            |model| model_choice_label(model).into_owned(),
        ));
    }
    if effort_changed {
        let label = chosen.reasoning.map_or_else(
            || "Default".to_owned(),
            |effort| effort_label(effort).into_owned(),
        );
        choices.push(format!("{label} effort"));
    }
    (!choices.is_empty()).then(|| choices.join(" · "))
}

fn footer_model_label<'a>(
    snapshot: &'a chat::Snapshot,
    chosen: ChosenModel<'a>,
    draft: bool,
    provider: Provider,
    fit: FooterFit,
    draft_default_effort: Option<&str>,
) -> Cow<'a, str> {
    let selected = chosen.model(snapshot);
    let active = (!draft).then_some(snapshot.active_model.as_ref()).flatten();
    let label = if let Some(active) = active {
        active.label.as_str().into()
    } else {
        match selected {
            Some(model) if provider == Provider::Omp && !draft => model.label.as_str().into(),
            Some(model) => model_choice_label(model),
            None => match chosen.id {
                Some(id) => id.into(),
                None if fit != FooterFit::Wide => "Default model".into(),
                None => format!("{} · Default model", family_label(provider)).into(),
            },
        }
    };
    let chosen = (active.is_none() || provider == Provider::Omp)
        .then_some(chosen.reasoning)
        .flatten()
        .filter(|effort| Some(*effort) != draft_default_effort);
    let effort = active.map_or(chosen, |active| {
        active
            .reasoning
            .as_deref()
            .filter(|effort| Some(*effort) != active.default_reasoning.as_deref())
    });
    match effort.filter(|effort| !effort.is_empty()) {
        Some(effort) => format!("{label} · {}", effort_label(effort)).into(),
        None => label,
    }
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
            let default = ((draft && !offer.models.iter().any(|model| model.id == "default"))
                || offer.models.is_empty())
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
        Provider::Antigravity => &["antigravity", "agy", "gemini", "google"],
        Provider::Muse => &["muse", "meta"],
    };
    names.iter().any(|name| {
        name.starts_with(needle) || name.split(' ').any(|word| word.starts_with(needle))
    })
}

fn picker_section_header<'a>(content: Element<'a, Message>, top: f32) -> Element<'a, Message> {
    widget::container(content)
        .padding(iced::Padding::from([step::GAP_XS, step::GAP_S]).top(top))
        .into()
}

fn picker_choice_row<'a>(
    title: Cow<'a, str>,
    detail: Option<&'a str>,
    name: Cow<'a, str>,
    id: widget::Id,
    selected: bool,
    message: Message,
) -> Element<'a, Message> {
    let height = if detail.is_some() {
        design::list::TWO_LINE_HEIGHT
    } else {
        design::DENSE_ROW_HEIGHT
    };
    let mut label = vec![
        design::body(title)
            .wrapping(iced::widget::text::Wrapping::None)
            .into(),
    ];
    if let Some(detail) = detail {
        label.push(
            design::caption(detail)
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
    }
    let control = widget::button::custom(
        widget::row::with_children(vec![
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
        .align_y(Alignment::Center)
        .height(Length::Fill),
    )
    .name(name)
    .id(id)
    .class(crate::ui_controls::control_style(selected, None))
    .selected(selected)
    .padding([0.0, step::GAP_S])
    .height(Length::Fixed(height))
    .width(Length::Fill)
    .on_press(message);
    if selected {
        control.description("Selected").into()
    } else {
        control.into()
    }
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
            rows.push(picker_section_header(
                widget::row::with_children(vec![
                    widget::icon(crate::ui_controls::provider_icon(provider))
                        .size(design::COMPACT_ICON_SIZE)
                        .into(),
                    design::caption(family_label(provider))
                        .class(cosmic::theme::Text::Custom(design::muted_text))
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .align_y(Alignment::Center)
                .into(),
                if index == 0 {
                    step::GAP_XS
                } else {
                    step::GAP_M
                },
            ));
        }
        for row in models {
            let selected =
                provider == current.0 && row.model.map(|model| model.id.as_str()) == current.1;
            let (title, id, name) = match row.model {
                Some(model) => (
                    model_choice_label(model),
                    Some(model.id.as_str()),
                    model_choice_label(model),
                ),
                None => (
                    Cow::Borrowed("Default model"),
                    None,
                    Cow::Owned(format!("{} default model", family_label(provider))),
                ),
            };
            // The identifier is shown only where it is not already the
            // label, because most providers use one string for both and a
            // row that prints it twice is half a list of duplicates.
            let listed = offers
                .iter()
                .any(|offer| offer.provider == provider && !offer.models.is_empty());
            let detail = match id {
                Some(id) if id != title.as_ref() => Some(id),
                Some(_) => None,
                None if listed => Some("Whichever it would choose itself"),
                None => Some("Its models are listed once it has run"),
            };
            rows.push(picker_choice_row(
                title,
                detail,
                name,
                card_control_id(
                    target.card,
                    &match id {
                        Some(id) => format!("chat-model-choice-{provider:?}-named-{id}"),
                        None => format!("chat-model-choice-{provider:?}-default"),
                    },
                ),
                selected,
                row.choose(target, draft),
            ));
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
        // The popover's own frame inset stays; a model passing it fades into
        // the popover rather than being cut at the inset.
        widget::container(crate::interaction::overflow_fade_y(
            widget::scrollable(
                widget::column::with_children(rows)
                    .padding(design::with_vertical_rail(iced::Padding::ZERO)),
            )
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .height(Length::Shrink),
            design::component_base,
        ))
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
mod issue1506_tests;

#[cfg(test)]
mod user_image_preview_tests;

#[cfg(test)]
mod image_viewer_tests;

#[cfg(test)]
mod figure_tests;

#[cfg(test)]
mod user_bubble_tests;

#[cfg(test)]
mod provider_context_tests;

#[cfg(test)]
pub(super) fn prepared_runtime_for_test(items: &[chat::Item]) -> (Runtime, Arc<ClientHandle>) {
    frame_cost::live_runtime(items)
}

/// A prepared live chat whose `turns` answers each link to three pull
/// requests, from #1000 up in `acme/widgets`.
#[cfg(test)]
pub(super) fn linked_runtime_for_test(turns: usize) -> Runtime {
    frame_cost::live_runtime(&frame_cost::linked_transcript(turns)).0
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod prepared_disclosure_tests {
    use super::*;

    #[test]
    fn prose_disclosure_keeps_prepared_source_and_palette_owners() {
        let item = chat::Item {
            id: "prepared-thought".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: format!(
                    "Long **thought** {}\n\n```rust\nlet answer = 42;\n```",
                    "ordinary words ".repeat(10_000)
                ),
                thinking: true,
                phase: chat::Phase::Commentary,
            },
            at: None,
            started_at: None,
        };
        let (mut live, _handle) = frame_cost::live_runtime(&[item]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        let cached = &live.cache["prepared-thought"];
        let revision = cached.revision;
        let item = cached.item.clone().expect("prepared item owner");
        let palettes = cached
            .palette_views
            .clone()
            .expect("worker prepared syntax palettes");
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        for expanded in [true, false, true] {
            drop(app.handle_chat_ui(Action::Toggle(target, "prepared-thought".into())));
            let CardRuntime::Chat(runtime) = &app.cards[&target.card] else {
                panic!("mounted chat")
            };
            assert_eq!(runtime.expanded.contains("prepared-thought"), expanded);
            let cached = &runtime.cache["prepared-thought"];
            assert_eq!(
                cached.revision, revision,
                "disclosure must not rebuild prepared prose"
            );
            assert!(Arc::ptr_eq(cached.item.as_ref().unwrap(), &item));
            let current = cached.palette_views.as_ref().unwrap();
            assert!(Arc::ptr_eq(&current[0], &palettes[0]));
            assert!(Arc::ptr_eq(&current[1], &palettes[1]));
        }
    }
}
