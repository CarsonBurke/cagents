//! Who is at work in a channel: its live agents and its open plain
//! terminals, drawn in place of the channel glyph as the mark of the one most
//! worth seeing, with how many there are.
//!
//! The row model keeps a [`Members`] tally rather than a list of sessions.
//! It is a few counters, so the navigation cache compares it as cheaply as the
//! row's other fields, and its display order follows from the counts alone:
//! working agents, then those waiting for input, stopped by a usage limit or
//! idle, then terminals. Within one state, providers keep their declaration
//! order, so a row does not reshuffle when an unrelated card moves.
//!
//! The row draws one mark, not one per member: at 1x a mark small enough to
//! share the glyph column with others is a smudge, and faces repeated after
//! the title read as duplicates and crowd it. A count badge on the lead's
//! mark says how many members there are. The tooltip and the accessible
//! name list them.
//!
//! An expanded row's mark is centred in its glyph column, in line with the
//! glyphs above and below, and its badge overhangs into the gutter before
//! the title. The collapsed rail's cell is wider than its glyph, so there
//! the lead moves aside and a second provider peeks out behind it when the
//! channel mixes providers; the row's column could show only a sliver of one.

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

/// The square the mark is laid out in: the row's glyph column. The badge
/// overhangs it to the right and below.
const BOX: f32 = super::ROW_ICON_WIDTH;
/// The lead's mark. Provider artwork fills its square, where a symbolic
/// glyph leaves a margin, so it is drawn a pixel larger than the glyphs
/// beside it to read at the same weight.
const LEAD: f32 = 15.0;
/// The terminal glyph is a symbolic glyph like the channel glyph it replaces.
const TERMINAL: u16 = super::CHILD_ROW_ICON_SIZE;
/// The lead's centre in the rail's box when a badge or a peeking mark goes
/// with it: left of the box's centre, which leaves them room. A
/// lone mark, and every expanded row's mark, keeps the column's centre, in
/// line with the glyphs above and below.
const LEAD_CENTRE: Point = Point::new(8.0, BOX / 2.0);
const LONE_CENTRE: Point = Point::new(BOX / 2.0, BOX / 2.0);
/// The peeking provider's mark, and where its centre sits from the lead's.
const PEEK: f32 = 11.0;
const PEEK_OFFSET: (f32, f32) = (6.0, -3.0);
/// The clear ring cut around the lead's mark where the peeking mark passes
/// behind it.
const PEEK_GAP: f32 = 2.0;
/// The count badge: its height, its width for one character and for two,
/// and the clear ring cut around it in the lead. A wider badge keeps its
/// right edge and grows left over the mark, so the gap before the title
/// never narrows. Where it sits is [`Ground::badge`].
const BADGE: f32 = 11.0;
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

const _: () = assert!(
    LONE_CENTRE.x + Ground::Row.badge().right <= BOX + super::ROW_GUTTER - 3.0,
    "an expanded row's badge stops short of the title"
);
const _: () = assert!(
    LONE_CENTRE.y + Ground::Row.badge().centre_y + BADGE / 2.0
        <= BOX + (super::WORKTREE_ROW_HEIGHT - BOX) / 2.0 - 2.0,
    "an expanded row's badge stays inside its row"
);
const _: () = assert!(
    RAIL_LEAD_X + Ground::Rail.badge().right
        <= super::rail::CELL_WIDTH - super::rail::DISC + super::rail::RING,
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

/// Where the count badge sits on the lead's mark, from the lead's centre.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BadgePlace {
    /// The badge's right edge.
    right: f32,
    /// The badge's vertical centre.
    centre_y: f32,
}

impl Ground {
    /// Where the badge sits on the lead. An expanded row's lead keeps its
    /// column's centre, so its badge sits further in and lower than the
    /// rail's: it stops three pixels short of the title and clear of the
    /// title's letters, below their middle, covering only the mark's
    /// lower right corner.
    const fn badge(self) -> BadgePlace {
        match self {
            Self::Row => BadgePlace {
                right: 11.0,
                centre_y: 7.0,
            },
            Self::Rail => BadgePlace {
                right: 7.5 + BADGE_WIDTHS[0] / 2.0,
                centre_y: 6.0,
            },
        }
    }

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
    let badge = (count > 1).then(|| badge_label(count));
    let width = badge
        .as_deref()
        .map(|label| BADGE_WIDTHS[usize::from(label.len() > 1)]);
    let opacity = if ink == RowInk::Muted { 0.55 } else { 1.0 };
    // Only the rail has room beside its lead for a second provider.
    let peek = members.peek().filter(|_| ground == Ground::Rail);
    let lone = badge.is_none() && peek.is_none();
    let centre = if lone || ground == Ground::Row {
        LONE_CENTRE
    } else {
        LEAD_CENTRE
    };
    let mut layers: Vec<Element<'static, Message>> = Vec::with_capacity(3);
    if let Some(provider) = peek {
        layers.push(at(
            mark(peek_handle(provider), PEEK, ink, opacity),
            centre.x + PEEK_OFFSET.0 - PEEK / 2.0,
            centre.y + PEEK_OFFSET.1 - PEEK / 2.0,
        ));
    }
    let (handle, size) = match lead {
        Member::Agent(provider, _) => (lead_handle(provider, ground, width), LEAD),
        // A themed glyph's drawing is not ours to cut, so the badge sits on
        // it; it is drawn only for a channel of terminals alone.
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
            centre.x + ground.badge().right - width,
            centre.y + ground.badge().centre_y - BADGE / 2.0,
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
/// sits on it on `ground`, so the badge is set off from the mark by
/// whatever is behind the row: its resting, hovered or selected fill alike.
fn lead_handle(provider: Provider, ground: Ground, badge_width: Option<f32>) -> Handle {
    type Cuts = [[[Handle; BADGE_WIDTHS.len()]; 2]; Provider::ALL.len()];
    static CUT: LazyLock<Cuts> = LazyLock::new(|| {
        Provider::ALL.map(|provider| {
            [Ground::Row, Ground::Rail].map(|ground| {
                let place = ground.badge();
                BADGE_WIDTHS.map(|width| {
                    let hole = Hole::Pill {
                        left: LEAD / 2.0 + place.right - width - BADGE_GAP,
                        top: LEAD / 2.0 + place.centre_y - BADGE / 2.0 - BADGE_GAP,
                        width: width + 2.0 * BADGE_GAP,
                        height: BADGE + 2.0 * BADGE_GAP,
                    };
                    cut_handle(provider, LEAD, hole)
                })
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
            CUT[provider as usize][usize::from(ground == Ground::Rail)][index].clone()
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

    /// The bounds of each layer `members` draw on `ground`, bottom first,
    /// in the glyph column's box.
    fn layers(members: &Members, ground: Ground) -> Vec<cosmic::iced::Rectangle> {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
        let mut element: Element<'static, super::super::SidebarAction> =
            view(members, ground, RowInk::Full).expect("someone to draw");
        let renderer = crate::sidebar::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(
                cosmic::iced::Size::ZERO,
                cosmic::iced::Size::new(super::super::rail::CELL_WIDTH, BOX),
            ),
        );
        let mut stack = Layout::new(&node);
        // The rail pads its stack aside within its cell.
        if ground == Ground::Rail && stack.children().count() == 1 {
            stack = stack.children().next().unwrap();
        }
        stack
            .children()
            .map(|pin| pin.children().next().map_or(pin.bounds(), |at| at.bounds()))
            .collect()
    }

    /// An expanded row draws its lead whole in the glyph column's centre,
    /// where a lone member's is, with a count badge on it when others are
    /// there and nothing else: no second provider, which the column could
    /// show only a sliver of. The badge sits on the lead's lower right and
    /// stops short of the title.
    #[test]
    fn a_rows_lead_keeps_the_glyph_columns_centre_under_its_badge() {
        let gutter = super::super::ROW_GUTTER;
        let claude = members(&[(Claude, MemberState::Working)], 0);
        let crowd = members(
            &[
                (Claude, MemberState::Working),
                (Codex, MemberState::Idle),
                (Omp, MemberState::Idle),
            ],
            4,
        );
        let alone = layers(&claude, Ground::Row);
        assert_eq!(alone.len(), 1, "a lone member has no badge: {alone:?}");
        for (members, label) in [
            (crowd, "a mixed crowd"),
            (
                members(
                    &[(Claude, MemberState::Working), (Claude, MemberState::Idle)],
                    0,
                ),
                "two of one provider",
            ),
            (members(&[], 10), "ten terminals"),
        ] {
            let drawn = layers(&members, Ground::Row);
            let [lead, badge] = drawn[..] else {
                panic!("{label}: the lead and its badge alone: {drawn:?}");
            };
            assert_eq!(lead.center(), alone[0].center(), "{label}");
            assert_eq!(lead.center_x(), BOX / 2.0, "{label}");
            assert!(
                lead.x >= 0.0 && lead.x + lead.width <= BOX,
                "{label}: whole"
            );
            assert!(
                badge.center_x() > lead.center_x() && badge.center_y() > lead.center_y(),
                "{label}: on the lead's lower right: {badge:?} {lead:?}"
            );
            assert!(
                badge.x + badge.width <= BOX + gutter - 3.0,
                "{label}: short of the title: {badge:?}"
            );
        }
        assert_eq!(
            layers(&members(&[], 1), Ground::Row)[0].center_x(),
            BOX / 2.0
        );
        // The rail is drawn as it was before the expanded row's badge moved:
        // the lead left of the box's centre, the peeking provider above its
        // right, and the badge's right edge 13 px and its centre 6 px from
        // the lead's centre.
        let rail = layers(&crowd, Ground::Rail);
        let [peek, lead, badge] = rail[..] else {
            panic!("the rail keeps its peeking provider beside the lead and badge: {rail:?}");
        };
        let centre = lead.center();
        assert_eq!(centre.x, RAIL_LEAD_X);
        assert_eq!(centre.y, BOX / 2.0);
        assert_eq!(
            (peek.center_x() - centre.x, peek.center_y() - centre.y),
            (6.0, -3.0)
        );
        assert_eq!(badge.x + badge.width - centre.x, 13.0);
        assert_eq!(badge.center_y() - centre.y, 6.0);
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
