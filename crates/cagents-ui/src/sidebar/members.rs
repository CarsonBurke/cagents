//! Who is at work in a channel: its live agents and its open plain
//! terminals, drawn as the mark of the one most worth seeing in place of the
//! channel glyph, with the rest of them beside the title.
//!
//! The row model keeps a [`Members`] tally rather than a list of sessions.
//! It is a few counters, so the navigation cache compares it as cheaply as the
//! row's other fields, and its display order follows from the counts alone:
//! working agents, then those waiting for input, stopped by a usage limit or
//! idle, then terminals. Within one state, providers keep their declaration
//! order, so a row does not reshuffle when an unrelated card moves.
//!
//! An expanded row leads with one mark, whole and centred in the glyph
//! column like every other row's glyph. The others follow the title as a
//! facepile: a face for each of them, up to a few, overlapping only by a few
//! pixels, then how many more there are. Faces show the kinds the lead does
//! not, first, then more of the kinds already shown. The glyph column is one
//! glyph wide; a second mark and a count squeezed into it covered each other
//! and pushed the lead out of line.
//!
//! The title keeps priority. A title that fits the row is never cut for the
//! pile: the pile shows as many faces as fit beside it, or nothing. A title
//! too long for the row is cut anyway, so it shares the row with one face
//! and the count, unless that would leave too little of it to read. A count
//! is never shown without a face, which would say how many but not who.
//!
//! The collapsed rail has no room beside its glyph, so there a second
//! provider peeks out behind the lead's mark and a count badge says how
//! many there are. The tooltip and the accessible name list every member
//! either way.

use std::sync::LazyLock;

use cosmic::{
    Element,
    iced::{Alignment, Background, Border, Color, Length, Point},
    theme,
    widget::{self, icon::Handle},
};

use super::RowInk;
use crate::{
    agent::Provider,
    design,
    ui_controls::{Icon, provider_artwork},
};

/// What one agent in a channel is doing, in the order the row lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemberState {
    Working,
    Waiting,
    Limited,
    Idle,
    /// The provider runs but has not said what it is doing.
    Running,
}

impl MemberState {
    const ALL: [Self; 5] = [
        Self::Working,
        Self::Waiting,
        Self::Limited,
        Self::Idle,
        Self::Running,
    ];

    /// The state a pane's reported activity and usage limit give its agent.
    pub fn of(activity: crate::agent::ActivityState, limited: bool) -> Self {
        use crate::agent::ActivityState;
        if limited {
            return Self::Limited;
        }
        match activity {
            ActivityState::Working => Self::Working,
            ActivityState::Waiting => Self::Waiting,
            ActivityState::Idle => Self::Idle,
            ActivityState::Unknown => Self::Running,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting for input",
            Self::Limited => "stopped by a usage limit",
            Self::Idle => "idle",
            Self::Running => "running",
        }
    }
}

/// One member of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Member {
    Agent(Provider, MemberState),
    /// An open terminal running no provider agent.
    Terminal,
}

/// A channel's live agents and plain terminals, counted by provider and
/// state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Members {
    agents: [[u16; MemberState::ALL.len()]; Provider::ALL.len()],
    terminals: u16,
}

impl Members {
    pub fn add_agent(&mut self, provider: Provider, state: MemberState) {
        let index = MemberState::ALL
            .iter()
            .position(|known| *known == state)
            .expect("every state is listed");
        let slot = &mut self.agents[provider as usize][index];
        *slot = slot.saturating_add(1);
    }

    pub fn add_terminal(&mut self) {
        self.terminals = self.terminals.saturating_add(1);
    }

    pub fn len(&self) -> usize {
        self.groups().map(|(_, count)| usize::from(count)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Each kind of member with how many there are, in display order.
    fn groups(&self) -> impl Iterator<Item = (Member, u16)> + '_ {
        MemberState::ALL
            .iter()
            .enumerate()
            .flat_map(move |(index, state)| {
                Provider::ALL.iter().map(move |provider| {
                    (
                        Member::Agent(*provider, *state),
                        self.agents[*provider as usize][index],
                    )
                })
            })
            .chain(std::iter::once((Member::Terminal, self.terminals)))
            .filter(|(_, count)| *count != 0)
    }

    /// Every member, one at a time, in display order.
    pub fn iter(&self) -> impl Iterator<Item = Member> + '_ {
        self.groups()
            .flat_map(|(member, count)| std::iter::repeat_n(member, usize::from(count)))
    }

    /// The members named for a tooltip or a screen reader: "Claude Code,
    /// working; Codex, idle; Terminal". Repeats are counted rather than listed, so a
    /// busy channel's name stays short enough to hear.
    pub fn description(&self) -> String {
        let mut parts = Vec::new();
        for (member, count) in self.groups() {
            let (name, state) = match member {
                Member::Agent(provider, state) => (provider.label(), Some(state.label())),
                Member::Terminal => ("Terminal", None),
            };
            let name = if count == 1 {
                name.to_owned()
            } else if member == Member::Terminal {
                format!("{count} Terminals")
            } else {
                format!("{count} {name}")
            };
            parts.push(match state {
                Some(state) => format!("{name}, {state}"),
                None => name,
            });
        }
        parts.join("; ")
    }

    /// The member the row's mark stands for: the first in display order.
    pub fn lead(&self) -> Option<Member> {
        self.iter().next()
    }

    /// The members after the lead, as the facepile shows them: a face for
    /// each of them, at most [`MAX_FACES`], and how many members there are
    /// after the lead.
    ///
    /// The faces say who else is there, so each kind among them gets a face
    /// before any kind gets a second: first the kinds the lead's mark does
    /// not show, in display order, then the lead's own kind, then the
    /// remaining members in display order. A channel of three Claude Code
    /// agents shows two Claude Code faces after its lead; a channel of
    /// terminals shows one terminal face and counts the rest. One terminal
    /// face is the most a pile draws: it is a themed glyph that cannot be
    /// cut for a neighbour, so two of them could not overlap.
    pub fn company(&self) -> Company {
        let Some(lead) = self.lead().map(Face::of) else {
            return Company {
                faces: Vec::new(),
                others: 0,
            };
        };
        let mut kinds: Vec<Face> = Vec::with_capacity(Provider::ALL.len() + 1);
        let mut others = 0;
        for member in self.iter().skip(1) {
            others += 1;
            let face = Face::of(member);
            if !kinds.contains(&face) {
                kinds.push(face);
            }
        }
        // A stable sort: the lead's kind moves to the end, the others keep
        // their display order.
        kinds.sort_by_key(|face| *face == lead);
        let mut faces = kinds.clone();
        faces.truncate(MAX_FACES);
        // Each kind's first member already has its face; the members after
        // those fill what room is left.
        let mut first = kinds;
        for face in self.iter().skip(1).map(Face::of) {
            if faces.len() == MAX_FACES {
                break;
            }
            if let Some(index) = first.iter().position(|kind| *kind == face) {
                first.swap_remove(index);
            } else if face != Face::Terminal {
                faces.push(face);
            }
        }
        Company { faces, others }
    }

    /// A provider other than the lead's, the first in display order, which
    /// peeks out behind the lead's mark in the rail. Terminals have no
    /// provider to show.
    pub fn peek(&self) -> Option<Provider> {
        let lead = match self.lead()? {
            Member::Agent(provider, _) => provider,
            Member::Terminal => return None,
        };
        self.groups().find_map(|(member, _)| match member {
            Member::Agent(provider, _) if provider != lead => Some(provider),
            _ => None,
        })
    }
}

/// One kind of member, as a face in the facepile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    Provider(Provider),
    Terminal,
}

impl Face {
    fn of(member: Member) -> Self {
        match member {
            Member::Agent(provider, _) => Self::Provider(provider),
            Member::Terminal => Self::Terminal,
        }
    }
}

/// The members after a row's lead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    /// The faces the pile may show, in the order a narrowing row keeps
    /// them; see [`Members::company`].
    pub faces: Vec<Face>,
    /// How many members there are after the lead.
    pub others: usize,
}

/// The most faces a pile shows; any more members are counted.
pub const MAX_FACES: usize = 3;
/// A face. Provider artwork fills its square, where the terminal's symbolic
/// glyph leaves a margin, so the two read at about the same weight here.
const FACE: f32 = design::LEADING_ICON_SIZE as f32;
/// How much of a face the next one covers: enough to read as one group,
/// little enough that each face's artwork stays whole enough to recognise.
const FACE_OVERLAP: f32 = 4.0;
const FACE_STEP: f32 = FACE - FACE_OVERLAP;
/// The clear ring cut around a face where its neighbour covers it.
const FACE_GAP: f32 = 1.5;
/// Between the last face and the count.
const COUNT_GAP: f32 = 2.0;

/// The square the mark is laid out in: the row's glyph column. In the rail
/// the badge overhangs it to the right and below.
const BOX: f32 = super::ROW_ICON_WIDTH;
/// The lead's mark. Provider artwork fills its square, where a symbolic
/// glyph leaves a margin, so it is drawn a pixel larger than the glyphs
/// beside it to read at the same weight.
const LEAD: f32 = 15.0;
/// The terminal glyph is a symbolic glyph like the channel glyph it replaces.
const TERMINAL: u16 = super::CHILD_ROW_ICON_SIZE;
/// The lead's centre in the rail's box when a badge or a peeking mark goes
/// with it: left of the box's centre, which leaves the badge room. A lone
/// mark, and every expanded row's mark, keeps the column's centre, in line
/// with the glyphs above and below.
const LEAD_CENTRE: Point = Point::new(8.0, BOX / 2.0);
const LONE_CENTRE: Point = Point::new(BOX / 2.0, BOX / 2.0);
/// The peeking provider's mark, and where its centre sits from the lead's.
const PEEK: f32 = 11.0;
const PEEK_OFFSET: (f32, f32) = (6.0, -3.0);
/// The clear ring cut around the lead's mark where the peeking mark passes
/// behind it.
const PEEK_GAP: f32 = 2.0;
/// The count badge: its height, its centre from the lead's at one character
/// wide, its width for one character and for two, and the clear ring cut
/// around it in the lead. A wider badge keeps its right edge and grows left
/// over the mark, so the gap before the title never narrows, and its text
/// stays inside the box, which bounds where text is drawn.
const BADGE: f32 = 11.0;
const BADGE_OFFSET: (f32, f32) = (7.5, 6.0);
const BADGE_WIDTHS: [f32; 2] = [11.0, 14.0];
const BADGE_GAP: f32 = 1.0;
const BADGE_TEXT: f32 = 9.0;
/// How far the badge's fill leans from the row's ground toward its ink:
/// clearly a pill on a resting, hovered or selected row, still quieter than
/// the title.
const BADGE_WASH: f32 = 0.3;
/// The lead's centre in a rail cell when a badge or a peeking mark goes with
/// it. The cell pins its news and activity badges to its right-hand corners;
/// the lead sits far enough left that its count badge stops at their ring
/// rather than under them. A lone mark is centred like any other glyph.
const RAIL_LEAD_X: f32 = 11.0;

/// The badge's right edge, from the lead's centre.
const BADGE_RIGHT: f32 = BADGE_OFFSET.0 + BADGE_WIDTHS[0] / 2.0;

const _: () = assert!(
    RAIL_LEAD_X + BADGE_RIGHT <= super::rail::CELL_WIDTH - super::rail::DISC + super::rail::RING,
    "the badge stops at the rail corner badges' ring"
);
const _: () = assert!(
    RAIL_LEAD_X + PEEK_OFFSET.0 + PEEK / 2.0
        <= super::rail::CELL_WIDTH - super::rail::DISC + super::rail::RING,
    "the peeking mark stops at the rail news badge's ring"
);

/// Where the mark is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ground {
    /// An expanded sidebar row.
    Row,
    /// A collapsed rail cell, on its project's tray.
    Rail,
}

impl Ground {
    /// The resting colour beneath the mark, which the badge's fill leans
    /// away from.
    fn color(self, theme: &cosmic::Theme) -> Color {
        match self {
            Self::Row => theme.cosmic().primary(false).base.into(),
            Self::Rail => design::rail_tray(theme),
        }
    }
}

/// The mark `members` draw, or `None` when there is nobody to draw and the
/// row keeps its channel glyph.
pub fn view<Message: 'static>(
    members: &Members,
    ground: Ground,
    ink: RowInk,
) -> Option<Element<'static, Message>> {
    let lead = members.lead()?;
    let count = members.len();
    // An expanded row's others follow its title; see [`company`].
    let badge = (count > 1 && ground == Ground::Rail).then(|| badge_label(count));
    let width = badge
        .as_deref()
        .map(|label| BADGE_WIDTHS[usize::from(label.len() > 1)]);
    let opacity = if ink == RowInk::Muted { 0.55 } else { 1.0 };
    let peek = members.peek().filter(|_| ground == Ground::Rail);
    let lone = badge.is_none() && peek.is_none();
    let centre = if lone { LONE_CENTRE } else { LEAD_CENTRE };
    let mut layers: Vec<Element<'static, Message>> = Vec::with_capacity(3);
    if let Some(provider) = peek {
        layers.push(at(
            mark(peek_handle(provider), PEEK, ink, opacity),
            centre.x + PEEK_OFFSET.0 - PEEK / 2.0,
            centre.y + PEEK_OFFSET.1 - PEEK / 2.0,
        ));
    }
    let (handle, size) = match lead {
        Member::Agent(provider, _) => (lead_handle(provider, width), LEAD),
        // A themed glyph's drawing is not ours to cut, so the rail's badge
        // sits on it; it is drawn only for a channel of terminals alone.
        Member::Terminal => (Icon::Terminal.handle(), f32::from(TERMINAL)),
    };
    layers.push(at(
        mark(handle, size, ink, opacity),
        centre.x - size / 2.0,
        centre.y - size / 2.0,
    ));
    if let (Some(label), Some(width)) = (badge, width) {
        layers.push(at(
            count_badge(label, width, ground, opacity),
            centre.x + BADGE_RIGHT - width,
            centre.y + BADGE_OFFSET.1 - BADGE / 2.0,
        ));
    }
    let stack = cosmic::iced::widget::Stack::with_children(layers)
        .width(Length::Fixed(BOX))
        .height(Length::Fixed(BOX));
    Some(match ground {
        Ground::Row => stack.into(),
        Ground::Rail if lone => stack.into(),
        Ground::Rail => widget::container(stack)
            .padding([0.0, 0.0, 0.0, RAIL_LEAD_X - LEAD_CENTRE.x])
            .width(Length::Fixed(super::rail::CELL_WIDTH))
            .into(),
    })
}

/// What a row's facepile needs: the faces it may show and the widths of
/// its count labels, so a row can choose how much of it fits beside its
/// title without measuring text again on every layout.
pub struct Pile {
    company: Company,
    /// `count_widths[shown - 1]` is the width of the count drawn after
    /// `shown` faces, or zero when those faces stand for every member.
    count_widths: Vec<f32>,
}

/// How a row's title and its facepile share the room beside the glyph.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    /// How many faces the pile shows, with their count; none hides it.
    pub faces: usize,
    /// The title's width: all of it, or what is left after the pile.
    pub title: f32,
}

/// The least of a title a row keeps beside a face; any less and the pile
/// goes, since a few letters say less than the tooltip's list.
pub const MIN_TITLE: f32 = 48.0;

impl Pile {
    /// `None` when the lead is alone. `measure` gives a caption's width.
    pub fn new(members: &Members, measure: impl Fn(&str) -> f32) -> Option<Self> {
        let company = members.company();
        if company.faces.is_empty() {
            return None;
        }
        let count_widths = (1..=company.faces.len())
            .map(|shown| count_label(&company, shown).map_or(0.0, |label| measure(&label).ceil()))
            .collect();
        Some(Self {
            company,
            count_widths,
        })
    }

    /// The width of the pile with `shown` faces, and their count; zero for
    /// none.
    pub fn width(&self, shown: usize) -> f32 {
        if shown == 0 {
            return 0.0;
        }
        let count = self.count_widths[shown - 1];
        let faces = FACE + (shown - 1) as f32 * FACE_STEP;
        faces + if count > 0.0 { COUNT_GAP + count } else { 0.0 }
    }

    /// How a title `natural` pixels wide shares `room` with this pile, a
    /// gutter between them. A title that fits keeps all of itself beside
    /// as many faces as also fit, which may be none. A title that does not
    /// fit is cut anyway, so it keeps all but the room one face and the
    /// count take, unless that leaves it less than [`MIN_TITLE`].
    pub fn fit(&self, room: f32, natural: f32) -> Fit {
        let gutter = super::ROW_GUTTER;
        if natural <= room {
            let faces = (1..=self.company.faces.len())
                .rev()
                .find(|&shown| natural + gutter + self.width(shown) <= room)
                .unwrap_or(0);
            return Fit {
                faces,
                title: natural,
            };
        }
        let title = room - gutter - self.width(1);
        if title >= MIN_TITLE {
            Fit { faces: 1, title }
        } else {
            Fit {
                faces: 0,
                title: room.max(0.0),
            }
        }
    }

    /// The pile with `shown` faces, in the row's `ink`. `face_id` names
    /// each face's place in the pile.
    pub fn view<Message: 'static>(
        &self,
        shown: usize,
        ink: RowInk,
        face_id: impl Fn(usize) -> widget::Id,
    ) -> Element<'static, Message> {
        let opacity = if ink == RowInk::Muted { 0.55 } else { 1.0 };
        let faces = &self.company.faces[..shown.min(self.company.faces.len())];
        if faces.is_empty() {
            return cosmic::iced::widget::Space::new().into();
        }
        // Neighbouring faces overlap, and one of each pair passes behind
        // the other with a clear ring cut where it does. A terminal is a
        // themed glyph whose drawing is not ours to cut, so it is on top of
        // both its neighbours; without one, the last face is. Faces before
        // the top one pass behind the face after them, faces after it
        // behind the face before them.
        let top = faces
            .iter()
            .position(|face| *face == Face::Terminal)
            .unwrap_or(faces.len() - 1);
        let layer = |index: usize| {
            let handle = match faces[index] {
                Face::Provider(provider) if index < top => face_handle(provider, Side::Right),
                Face::Provider(provider) if index > top => face_handle(provider, Side::Left),
                Face::Provider(provider) => crate::ui_controls::provider_icon(provider),
                Face::Terminal => Icon::Terminal.handle(),
            };
            at(
                widget::container(mark(handle, FACE, ink, opacity))
                    .id(face_id(index))
                    .into(),
                index as f32 * FACE_STEP,
                0.0,
            )
        };
        // Drawn from both ends toward the top face, so each face is drawn
        // after the one it covers.
        let layers: Vec<Element<'static, Message>> = (0..top)
            .chain((top + 1..faces.len()).rev())
            .chain(std::iter::once(top))
            .map(layer)
            .collect();
        let mut cells: Vec<Element<'static, Message>> = Vec::with_capacity(2);
        cells.push(
            cosmic::iced::widget::Stack::with_children(layers)
                .width(Length::Fixed(FACE + (faces.len() - 1) as f32 * FACE_STEP))
                .height(Length::Fixed(FACE))
                .into(),
        );
        if let Some(label) = count_label(&self.company, faces.len()) {
            cells.push(
                widget::container(
                    widget::text(label)
                        .size(design::TEXT_CAPTION)
                        .wrapping(cosmic::iced::widget::text::Wrapping::None),
                )
                .class(theme::Container::custom(move |theme| {
                    let mut color = design::muted_text(theme)
                        .color
                        .unwrap_or_else(|| theme.current_container().on.into());
                    color.a *= opacity;
                    widget::container::Style {
                        text_color: Some(color),
                        ..Default::default()
                    }
                }))
                .into(),
            );
        }
        widget::row::with_children(cells)
            .spacing(COUNT_GAP)
            .align_y(Alignment::Center)
            .into()
    }
}

impl Pile {
    /// A title followed by as much of this pile as [`Pile::fit`] lets
    /// share its slot. `natural` gives the title's whole width; it is
    /// measured at the first layout and kept, not on every one. `title`
    /// draws the title at the width it is given.
    ///
    /// The pile follows the title rather than the slot's end, and the two
    /// are as tall as the title's text, as a title on its own is, so they
    /// share its pixel row; a height of their own would centre them
    /// between two and blur them.
    pub fn beside<'a, Message: 'static>(
        self,
        ink: RowInk,
        natural: impl Fn() -> f32 + 'a,
        title: impl Fn(f32) -> Element<'a, Message> + 'a,
        face_id: impl Fn(usize) -> widget::Id + 'a,
    ) -> Element<'a, Message> {
        let measured = std::cell::OnceCell::new();
        crate::ui_controls::responsive(move |slot| {
            let natural = *measured.get_or_init(|| natural().ceil());
            let fit = self.fit(slot.width, natural);
            let title = title(fit.title);
            if fit.faces == 0 {
                return title;
            }
            widget::row::with_children(vec![title, self.view(fit.faces, ink, &face_id)])
                .spacing(super::ROW_GUTTER)
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .into()
        })
        .height(Length::Shrink)
        .into()
    }
}

/// Which neighbour covers a face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// How many members `shown` faces leave uncounted, as the pile says it:
/// "+3". `None` when the faces stand for every member after the lead.
fn count_label(company: &Company, shown: usize) -> Option<String> {
    let rest = company.others - shown.min(company.others);
    (rest > 0).then(|| format!("+{rest}"))
}

/// A provider's face with a clear ring cut where its neighbour on `side`
/// covers it.
fn face_handle(provider: Provider, side: Side) -> Handle {
    static CUT: LazyLock<[[Handle; 2]; Provider::ALL.len()]> = LazyLock::new(|| {
        Provider::ALL.map(|provider| {
            [-FACE_STEP, FACE_STEP].map(|step| {
                // The neighbour's centre, in this face's own square.
                let hole = Hole::Circle {
                    x: FACE / 2.0 + step,
                    y: FACE / 2.0,
                    radius: FACE / 2.0 + FACE_GAP,
                };
                cut_handle(provider, FACE, hole)
            })
        })
    });
    CUT[provider as usize][usize::from(side == Side::Right)].clone()
}

/// `content` with its top left at `(x, y)` in the mark's box. It keeps its
/// own size where it reaches past the box: the badge overhangs it.
fn at<Message: 'static>(
    content: Element<'static, Message>,
    x: f32,
    y: f32,
) -> Element<'static, Message> {
    cosmic::iced::widget::pin(content)
        .position(Point::new(x, y))
        .overflow(true)
        .into()
}

fn mark<Message: 'static>(
    handle: Handle,
    size: f32,
    ink: RowInk,
    opacity: f32,
) -> Element<'static, Message> {
    widget::container(
        widget::icon(handle)
            .width(Length::Fixed(size))
            .height(Length::Fixed(size))
            .opacity(opacity),
    )
    .class(theme::Container::custom(move |theme| {
        widget::container::Style {
            icon_color: Some(ink.icon_color(theme)),
            ..Default::default()
        }
    }))
    .into()
}

fn count_badge<Message: 'static>(
    label: String,
    width: f32,
    ground: Ground,
    opacity: f32,
) -> Element<'static, Message> {
    widget::container(
        widget::text(label)
            .size(BADGE_TEXT)
            .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                BADGE.into(),
            ))
            .font(cosmic::font::semibold())
            .wrapping(cosmic::iced::widget::text::Wrapping::None),
    )
    .center_x(Length::Fixed(width))
    .center_y(Length::Fixed(BADGE))
    .align_x(Alignment::Center)
    .class(theme::Container::custom(move |theme| {
        let on: Color = theme.current_container().on.into();
        let mut fill = design::mix(ground.color(theme), on, BADGE_WASH);
        fill.a *= opacity;
        let mut ink = on;
        ink.a *= opacity;
        widget::container::Style {
            background: Some(Background::Color(fill)),
            text_color: Some(ink),
            border: Border {
                radius: (BADGE / 2.0).into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }))
    .into()
}

/// How many members there are, as the badge holds them.
fn badge_label(count: usize) -> String {
    if count > 9 {
        "9+".to_owned()
    } else {
        count.to_string()
    }
}

/// The lead's mark, with a clear pill cut where the badge of `badge_width`
/// sits on it, so the badge is set off from the mark by whatever is behind
/// the row: its resting, hovered or selected fill alike.
fn lead_handle(provider: Provider, badge_width: Option<f32>) -> Handle {
    static CUT: LazyLock<[[Handle; BADGE_WIDTHS.len()]; Provider::ALL.len()]> =
        LazyLock::new(|| {
            Provider::ALL.map(|provider| {
                BADGE_WIDTHS.map(|width| {
                    let hole = Hole::Pill {
                        left: LEAD / 2.0 + BADGE_RIGHT - width - BADGE_GAP,
                        top: LEAD / 2.0 + BADGE_OFFSET.1 - BADGE / 2.0 - BADGE_GAP,
                        width: width + 2.0 * BADGE_GAP,
                        height: BADGE + 2.0 * BADGE_GAP,
                    };
                    cut_handle(provider, LEAD, hole)
                })
            })
        });
    match badge_width {
        None => crate::ui_controls::provider_icon(provider),
        Some(width) => {
            let index = BADGE_WIDTHS
                .iter()
                .position(|known| *known == width)
                .expect("a badge has one of the known widths");
            CUT[provider as usize][index].clone()
        }
    }
}

/// The peeking provider's mark, with a clear ring cut around the lead's.
fn peek_handle(provider: Provider) -> Handle {
    static CUT: LazyLock<[Handle; Provider::ALL.len()]> = LazyLock::new(|| {
        Provider::ALL.map(|provider| {
            // The lead's centre, in the peeking mark's own square.
            let hole = Hole::Circle {
                x: PEEK / 2.0 - PEEK_OFFSET.0,
                y: PEEK / 2.0 - PEEK_OFFSET.1,
                radius: LEAD / 2.0 + PEEK_GAP,
            };
            cut_handle(provider, PEEK, hole)
        })
    });
    CUT[provider as usize].clone()
}

/// A region cleared from a mark, in logical pixels of a mark drawn `size`
/// square.
#[derive(Debug, Clone, Copy)]
enum Hole {
    Circle {
        x: f32,
        y: f32,
        radius: f32,
    },
    Pill {
        left: f32,
        top: f32,
        width: f32,
        height: f32,
    },
}

fn cut_handle(provider: Provider, size: f32, hole: Hole) -> Handle {
    let (svg, symbolic) = provider_artwork(provider);
    let svg = std::str::from_utf8(svg).expect("provider artwork is UTF-8");
    let cut = cut(svg, size, hole).expect("provider artwork has a root svg with a viewBox");
    cosmic::widget::icon::from_svg_bytes(cut.into_bytes()).symbolic(symbolic)
}

/// `svg` with `hole` masked out of everything it draws. The hole is truly
/// transparent rather than painted in a ground colour, which could match the
/// row only at rest.
fn cut(svg: &str, size: f32, hole: Hole) -> Option<String> {
    let root = svg.find("<svg")?;
    let open_end = root + svg[root..].find('>')? + 1;
    let close = svg.rfind("</svg>")?;
    let view_box = &svg[root..open_end];
    let view_box = &view_box[view_box.find("viewBox=\"")? + "viewBox=\"".len()..];
    let view_box = &view_box[..view_box.find('"')?];
    let [min_x, min_y, width, height]: [f32; 4] = view_box
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::parse)
        .collect::<Result<Vec<f32>, _>>()
        .ok()?
        .try_into()
        .ok()?;
    let (scale_x, scale_y) = (width / size, height / size);
    let shape = match hole {
        Hole::Circle { x, y, radius } => format!(
            r#"<ellipse cx="{}" cy="{}" rx="{}" ry="{}" fill="black"/>"#,
            min_x + x * scale_x,
            min_y + y * scale_y,
            radius * scale_x,
            radius * scale_y,
        ),
        Hole::Pill {
            left,
            top,
            width: hole_width,
            height: hole_height,
        } => format!(
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{}" ry="{}" fill="black"/>"#,
            min_x + left * scale_x,
            min_y + top * scale_y,
            hole_width * scale_x,
            hole_height * scale_y,
            hole_height / 2.0 * scale_x,
            hole_height / 2.0 * scale_y,
        ),
    };
    Some(format!(
        r#"{head}<defs><mask id="cagents-cut" maskUnits="userSpaceOnUse" x="{min_x}" y="{min_y}" width="{width}" height="{height}"><rect x="{min_x}" y="{min_y}" width="{width}" height="{height}" fill="white"/>{shape}</mask></defs><g mask="url(#cagents-cut)">{body}</g></svg>"#,
        head = &svg[..open_end],
        body = &svg[open_end..close],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Provider::{Claude, Codex, Omp};

    fn members(agents: &[(Provider, MemberState)], terminals: usize) -> Members {
        let mut members = Members::default();
        for (provider, state) in agents {
            members.add_agent(*provider, *state);
        }
        for _ in 0..terminals {
            members.add_terminal();
        }
        members
    }

    #[test]
    fn working_agents_lead_then_idle_agents_then_terminals() {
        let members = members(
            &[
                (Codex, MemberState::Idle),
                (Claude, MemberState::Idle),
                (Codex, MemberState::Working),
                (Omp, MemberState::Waiting),
            ],
            1,
        );
        assert_eq!(
            members.iter().collect::<Vec<_>>(),
            vec![
                Member::Agent(Codex, MemberState::Working),
                Member::Agent(Omp, MemberState::Waiting),
                Member::Agent(Claude, MemberState::Idle),
                Member::Agent(Codex, MemberState::Idle),
                Member::Terminal,
            ]
        );
        assert_eq!(members.len(), 5);
        assert_eq!(
            members.lead(),
            Some(Member::Agent(Codex, MemberState::Working))
        );
    }

    #[test]
    fn the_first_other_provider_peeks_behind_the_lead() {
        let mixed = members(
            &[
                (Claude, MemberState::Working),
                (Claude, MemberState::Idle),
                (Codex, MemberState::Idle),
                (Omp, MemberState::Idle),
            ],
            0,
        );
        assert_eq!(mixed.peek(), Some(Omp), "Oh My Pi is declared first");
        let same = members(
            &[(Claude, MemberState::Working), (Claude, MemberState::Idle)],
            2,
        );
        assert_eq!(same.peek(), None, "terminals have no provider to show");
        let terminals = members(&[], 2);
        assert_eq!(terminals.lead(), Some(Member::Terminal));
        assert_eq!(terminals.peek(), None);
        assert_eq!(Members::default().lead(), None);
    }

    #[test]
    fn the_company_shows_every_other_member_the_kinds_the_lead_does_not_first() {
        let mixed = members(
            &[
                (Claude, MemberState::Working),
                (Claude, MemberState::Idle),
                (Codex, MemberState::Idle),
                (Codex, MemberState::Idle),
                (Omp, MemberState::Idle),
            ],
            2,
        );
        let company = mixed.company();
        assert_eq!(company.others, 6, "everyone after the lead");
        assert_eq!(
            company.faces,
            vec![Face::Provider(Omp), Face::Provider(Codex), Face::Terminal],
            "one face per kind; the lead's own Claude Code is last, so a fourth"
        );
        let paired = members(
            &[(Claude, MemberState::Working), (Claude, MemberState::Idle)],
            1,
        )
        .company();
        assert_eq!(
            paired.faces,
            vec![Face::Terminal, Face::Provider(Claude)],
            "the lead's kind follows the others"
        );
        let shells = members(&[], 10).company();
        assert_eq!(
            (shells.faces, shells.others),
            (vec![Face::Terminal], 9),
            "a channel of terminals shows one of the others and counts the rest"
        );
        let claudes = members(
            &[
                (Claude, MemberState::Working),
                (Claude, MemberState::Idle),
                (Claude, MemberState::Idle),
            ],
            0,
        )
        .company();
        assert_eq!(
            (claudes.faces, claudes.others),
            (vec![Face::Provider(Claude); 2], 2),
            "the others are shown whatever their kind"
        );
        let twin = members(
            &[(Codex, MemberState::Working), (Codex, MemberState::Idle)],
            0,
        )
        .company();
        assert_eq!((twin.faces, twin.others), (vec![Face::Provider(Codex)], 1));
        let repeats = members(
            &[
                (Claude, MemberState::Working),
                (Codex, MemberState::Idle),
                (Codex, MemberState::Idle),
                (Codex, MemberState::Idle),
                (Claude, MemberState::Idle),
            ],
            2,
        )
        .company();
        assert_eq!(
            (repeats.faces, repeats.others),
            (
                vec![
                    Face::Provider(Codex),
                    Face::Terminal,
                    Face::Provider(Claude)
                ],
                6
            ),
            "every kind gets a face before any kind gets a second"
        );
        let shells_and_agent = members(&[(Codex, MemberState::Working)], 3).company();
        assert_eq!(
            shells_and_agent.faces,
            vec![Face::Terminal],
            "one terminal face at most: neither of two could be cut for the other"
        );
        let alone = members(&[(Claude, MemberState::Working)], 0).company();
        assert_eq!((alone.faces.len(), alone.others), (0, 0));
        assert!(Pile::new(&members(&[], 1), |_| 10.0).is_none());
        assert!(Pile::new(&members(&[(Claude, MemberState::Working)], 0), |_| 10.0).is_none());
        assert!(Pile::new(&members(&[], 10), |_| 10.0).is_some());
    }

    /// Claude Code leading Codex, Oh My Pi and three terminals: three faces
    /// and "+2" at most, with every caption ten pixels wide.
    fn crowd() -> Pile {
        Pile::new(
            &members(
                &[
                    (Claude, MemberState::Working),
                    (Codex, MemberState::Idle),
                    (Omp, MemberState::Idle),
                ],
                3,
            ),
            |_| 10.0,
        )
        .unwrap()
    }

    #[test]
    fn a_pile_shows_faces_with_their_count_and_never_a_count_alone() {
        let pile = crowd();
        assert_eq!(pile.width(0), 0.0, "no faces, no pile");
        assert_eq!(pile.width(1), FACE + COUNT_GAP + 10.0);
        assert_eq!(pile.width(3), FACE + 2.0 * FACE_STEP + COUNT_GAP + 10.0);
        assert_eq!(count_label(&pile.company, 1).as_deref(), Some("+4"));
        assert_eq!(count_label(&pile.company, 3).as_deref(), Some("+2"));
    }

    #[test]
    fn one_other_member_is_its_face_alone() {
        let pair = Pile::new(
            &members(
                &[(Claude, MemberState::Working), (Codex, MemberState::Idle)],
                0,
            ),
            |_| 10.0,
        )
        .unwrap();
        assert_eq!(pair.width(1), FACE, "no \"+1\"");
        assert_eq!(count_label(&pair.company, 1), None);
        assert_eq!(pair.fit(200.0, 100.0).faces, 1);
        assert_eq!(pair.fit(200.0, 400.0).faces, 1);
    }

    #[test]
    fn a_title_that_fits_stays_whole_beside_as_many_faces_as_fit() {
        let pile = crowd();
        let gutter = super::super::ROW_GUTTER;
        let title = 100.0;
        let beside = |shown| title + gutter + pile.width(shown);
        let whole = |faces| Fit { faces, title };
        assert_eq!(pile.fit(f32::INFINITY, title), whole(3));
        assert_eq!(pile.fit(beside(3), title), whole(3));
        assert_eq!(pile.fit(beside(3) - 0.5, title), whole(2));
        assert_eq!(pile.fit(beside(2), title), whole(2));
        assert_eq!(pile.fit(beside(2) - 0.5, title), whole(1));
        assert_eq!(pile.fit(beside(1), title), whole(1));
        assert_eq!(
            pile.fit(beside(1) - 0.5, title),
            whole(0),
            "the pile goes before the title is cut"
        );
        assert_eq!(pile.fit(title, title), whole(0));
    }

    #[test]
    fn a_title_cut_anyway_keeps_one_face_while_enough_of_it_shows() {
        let pile = crowd();
        let one = super::super::ROW_GUTTER + pile.width(1);
        assert_eq!(
            pile.fit(150.0, 400.0),
            Fit {
                faces: 1,
                title: 150.0 - one
            }
        );
        assert_eq!(
            pile.fit(MIN_TITLE + one, 400.0),
            Fit {
                faces: 1,
                title: MIN_TITLE
            }
        );
        assert_eq!(
            pile.fit(MIN_TITLE + one - 0.5, 400.0),
            Fit {
                faces: 0,
                title: MIN_TITLE + one - 0.5
            },
            "too little title to read: the pile goes"
        );
        assert_eq!(
            pile.fit(0.0, 400.0),
            Fit {
                faces: 0,
                title: 0.0
            }
        );
    }

    /// Every bound in a laid-out element, depth first.
    fn bounds(
        element: &mut Element<'static, super::super::SidebarAction>,
    ) -> Vec<cosmic::iced::Rectangle> {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
        fn walk(layout: Layout<'_>, out: &mut Vec<cosmic::iced::Rectangle>) {
            out.push(layout.bounds());
            for child in layout.children() {
                walk(child, out);
            }
        }
        let renderer = crate::sidebar::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(cosmic::iced::Size::ZERO, cosmic::iced::Size::new(BOX, BOX)),
        );
        let mut out = Vec::new();
        walk(Layout::new(&node), &mut out);
        out
    }

    /// An expanded row's lead sits whole where a lone member's does, in
    /// the glyph column's centre, however many others follow it; only the
    /// rail moves it aside for a badge.
    #[test]
    fn a_rows_lead_keeps_the_glyph_columns_centre() {
        let lone = |lead| {
            let mut element = view(&lead, Ground::Row, RowInk::Full).unwrap();
            bounds(&mut element)
        };
        let claude = members(&[(Claude, MemberState::Working)], 0);
        let crowd = members(
            &[
                (Claude, MemberState::Working),
                (Codex, MemberState::Idle),
                (Omp, MemberState::Idle),
            ],
            4,
        );
        assert_eq!(lone(claude), lone(crowd));
        let shells = lone(members(&[], 10));
        assert_eq!(shells, lone(members(&[], 1)));
        let glyph = shells.last().unwrap();
        assert_eq!(glyph.center_x(), BOX / 2.0, "{shells:?}");
        assert!(
            glyph.x >= 0.0 && glyph.x + glyph.width <= BOX,
            "whole: {glyph:?}"
        );
        let mut rail = view(&crowd, Ground::Rail, RowInk::Full).unwrap();
        assert_ne!(bounds(&mut rail), lone(crowd), "the rail keeps its badge");
    }

    #[test]
    fn the_description_names_each_agent_and_what_it_is_doing() {
        assert_eq!(
            members(
                &[(Codex, MemberState::Idle), (Claude, MemberState::Working)],
                1
            )
            .description(),
            "Claude Code, working; Codex, idle; Terminal"
        );
        assert_eq!(
            members(
                &[
                    (Claude, MemberState::Working),
                    (Claude, MemberState::Working),
                    (Omp, MemberState::Limited),
                    (Codex, MemberState::Running),
                ],
                0
            )
            .description(),
            "2 Claude Code, working; Oh My Pi, stopped by a usage limit; Codex, running"
        );
        assert_eq!(members(&[], 2).description(), "2 Terminals");
        assert_eq!(Members::default().description(), "");
    }

    #[test]
    fn a_usage_limit_outranks_whatever_the_agent_last_reported() {
        use crate::agent::ActivityState;
        assert_eq!(
            MemberState::of(ActivityState::Working, true),
            MemberState::Limited
        );
        assert_eq!(
            MemberState::of(ActivityState::Unknown, false),
            MemberState::Running
        );
    }

    #[test]
    fn the_badge_holds_one_character_or_nine_plus() {
        assert_eq!(badge_label(2), "2");
        assert_eq!(badge_label(9), "9");
        assert_eq!(badge_label(10), "9+");
        assert_eq!(badge_label(250), "9+");
    }

    #[test]
    fn a_cut_masks_everything_the_artwork_draws() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" fill="black"><path d="M0 0h22v22H0z"/></svg>"#;
        let masked = cut(
            svg,
            11.0,
            Hole::Circle {
                x: 1.0,
                y: 2.0,
                radius: 3.0,
            },
        )
        .unwrap();
        assert_eq!(
            masked,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" fill="black"><defs><mask id="cagents-cut" maskUnits="userSpaceOnUse" x="0" y="0" width="22" height="22"><rect x="0" y="0" width="22" height="22" fill="white"/><ellipse cx="2" cy="4" rx="6" ry="6" fill="black"/></mask></defs><g mask="url(#cagents-cut)"><path d="M0 0h22v22H0z"/></g></svg>"#
        );
        assert!(
            cut(
                "<path/>",
                11.0,
                Hole::Circle {
                    x: 0.0,
                    y: 0.0,
                    radius: 1.0
                }
            )
            .is_none()
        );
    }

    #[test]
    fn a_view_box_may_separate_its_numbers_with_commas() {
        let svg = r#"<svg viewBox="0,0, 22,22"><path/></svg>"#;
        let hole = Hole::Circle {
            x: 1.0,
            y: 2.0,
            radius: 3.0,
        };
        assert!(
            cut(svg, 11.0, hole)
                .unwrap()
                .contains(r#"<ellipse cx="2" cy="4" rx="6" ry="6" fill="black"/>"#)
        );
    }

    #[test]
    fn every_provider_mark_can_be_cut() {
        for provider in Provider::ALL {
            let (svg, _) = provider_artwork(provider);
            let svg = std::str::from_utf8(svg).unwrap();
            for hole in [
                Hole::Circle {
                    x: 0.0,
                    y: 8.0,
                    radius: 9.5,
                },
                Hole::Pill {
                    left: 8.0,
                    top: 7.0,
                    width: 16.0,
                    height: 13.0,
                },
            ] {
                let masked = cut(svg, LEAD, hole).unwrap_or_else(|| panic!("{provider:?}"));
                assert!(masked.contains(r#"<g mask="url(#cagents-cut)">"#));
                assert!(masked.ends_with("</g></svg>"), "{provider:?}");
            }
        }
    }
}
