//! Who is at work in a channel: the providers of its live agents, and its
//! open plain terminals, drawn as one fixed-size cluster in place of the
//! channel glyph.
//!
//! The row model keeps a [`Members`] tally rather than a list of sessions.
//! It is a few counters, so the navigation cache compares it as cheaply as the
//! row's other fields, and its display order follows from the counts alone:
//! working agents, then those waiting for input, stopped by a usage limit or
//! idle, then terminals. Within one state, providers keep their declaration
//! order, so a row does not reshuffle when an unrelated card moves.

use cosmic::{
    Element,
    iced::{Alignment, Background, Border, Color, Length, Padding},
    theme, widget,
};

use super::SidebarAction;
use crate::{agent::Provider, design, ui_controls::Icon};

/// What one agent in a channel is doing, in the order the cluster lists them.
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

    fn label(self) -> Option<&'static str> {
        match self {
            Self::Working => Some("working"),
            Self::Waiting => Some("waiting for input"),
            Self::Limited => Some("stopped by a usage limit"),
            Self::Idle => Some("idle"),
            Self::Running => None,
        }
    }
}

/// One mark in the cluster.
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
                Member::Agent(provider, state) => (provider.label(), state.label()),
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

    /// What the cluster draws: up to four marks, the fourth giving way to a
    /// count of the rest once there are more than four.
    pub fn cluster(&self) -> Cluster {
        let total = self.len();
        let mut slots = [None; MAX_SLOTS];
        let shown = if total > MAX_SLOTS {
            MAX_SLOTS - 1
        } else {
            total
        };
        for (slot, member) in slots.iter_mut().zip(self.iter().take(shown)) {
            *slot = Some(Slot::Member(member));
        }
        if total > MAX_SLOTS {
            slots[MAX_SLOTS - 1] = Some(Slot::More(total - shown));
        }
        Cluster { slots }
    }
}

const MAX_SLOTS: usize = 4;

/// One spot in the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Member(Member),
    /// How many members the cluster has no room to draw.
    More(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cluster {
    slots: [Option<Slot>; MAX_SLOTS],
}

/// How the cluster arranges its spots in its square box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    /// One circle, centred.
    Single,
    /// Two circles on the falling diagonal, the first top left.
    Diagonal,
    /// One circle on top, two below.
    Triangle,
    /// Two by two, read row by row.
    Square,
}

impl Arrangement {
    /// Each circle's diameter as a share of the box.
    fn diameter(self) -> f32 {
        match self {
            Self::Single => 0.78,
            Self::Diagonal => 0.62,
            Self::Triangle => 0.56,
            Self::Square => 0.52,
        }
    }

    /// Each circle's centre, as offsets from the box's centre in units of
    /// how far a circle can move before it leaves the box.
    fn offsets(self) -> &'static [(f32, f32)] {
        match self {
            Self::Single => &[(0.0, 0.0)],
            Self::Diagonal => &[(-1.0, -1.0), (1.0, 1.0)],
            Self::Triangle => &[(0.0, -1.0), (-1.0, 1.0), (1.0, 1.0)],
            Self::Square => &[(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)],
        }
    }
}

impl Cluster {
    pub fn slots(&self) -> impl Iterator<Item = Slot> + '_ {
        self.slots.iter().flatten().copied()
    }

    pub fn arrangement(&self) -> Option<Arrangement> {
        match self.slots().count() {
            0 => None,
            1 => Some(Arrangement::Single),
            2 => Some(Arrangement::Diagonal),
            3 => Some(Arrangement::Triangle),
            _ => Some(Arrangement::Square),
        }
    }
}

/// The square the cluster fills: the row's glyph column, and the rail's.
pub const BOX: f32 = 20.0;
/// The ring of the row's own background that cuts each circle out of the one
/// it overlaps, drawn outside the circle so the circle keeps its size.
const RING: f32 = 1.2;
/// A provider's mark as a share of its circle: as large as it can be while
/// the circle still shows round around it.
const MARK: f32 = 0.74;

/// Where the cluster is drawn, which decides the ground its ring matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ground {
    /// An expanded sidebar row.
    Row { selected: bool },
    /// A collapsed rail cell, on its project's tray.
    Rail { selected: bool },
}

impl Ground {
    /// The row's resting fill, opaque, as the row's button paints it over the
    /// pane. A ring has to hide what it overlaps, so a translucent pane's own
    /// alpha is not kept.
    fn color(self, theme: &cosmic::Theme) -> Color {
        let on: Color = theme.current_container().on.into();
        match self {
            Self::Row { selected } => {
                let pane: Color = theme.cosmic().primary(false).base.into();
                design::mix(
                    pane,
                    on,
                    if selected {
                        super::SELECTED_ROW_ALPHA
                    } else {
                        0.0
                    },
                )
            }
            Self::Rail { selected } => design::mix(
                design::rail_tray(theme),
                on,
                if selected {
                    super::rail::SELECTED_ALPHA
                } else {
                    0.0
                },
            ),
        }
    }
}

/// The cluster `members` draw, or `None` when there is nobody to draw and
/// the row keeps its channel glyph. A `muted` row's marks recede with its
/// label.
pub fn view(
    members: &Members,
    ground: Ground,
    muted: bool,
) -> Option<Element<'static, SidebarAction>> {
    let cluster = members.cluster();
    let arrangement = cluster.arrangement()?;
    let diameter = BOX * arrangement.diameter();
    let reach = (BOX - diameter) / 2.0;
    let outer = diameter + 2.0 * RING;
    let side = BOX + 2.0 * RING;
    let mut layers: Vec<Element<'static, SidebarAction>> = cluster
        .slots()
        .zip(arrangement.offsets())
        .map(|(slot, (x, y))| {
            // The box's centre, moved by the offset, less the circle's own
            // radius: the circle's top left inside the ring-wide margin.
            let left = side / 2.0 + x * reach - outer / 2.0;
            let top = side / 2.0 + y * reach - outer / 2.0;
            widget::container(circle(slot, diameter, ground, muted))
                .padding(Padding {
                    top,
                    left,
                    right: 0.0,
                    bottom: 0.0,
                })
                .width(Length::Fixed(side))
                .height(Length::Fixed(side))
                .into()
        })
        .collect();
    // The first member, the one most worth seeing, sits on top.
    layers.reverse();
    let cluster = cosmic::iced::widget::Stack::with_children(layers)
        .width(Length::Fixed(side))
        .height(Length::Fixed(side));
    Some(match ground {
        Ground::Row { .. } => cluster.into(),
        // The rail cell pins its badges to its right-hand corners, over
        // whatever is centred beneath them. The cluster is as wide as the
        // badges leave free, so it sits against them, clear of both, rather
        // than on the cell's centre line with its right-hand circles covered.
        Ground::Rail { .. } => widget::container(cluster)
            .padding(Padding {
                left: RAIL_INSET - RING,
                ..Padding::ZERO
            })
            .width(Length::Fixed(super::rail::CELL_WIDTH))
            .into(),
    })
}

/// The cluster's left edge in a rail cell: as far right as it can sit before
/// it reaches the corner badges.
const RAIL_INSET: f32 = super::rail::CELL_WIDTH - super::rail::DISC - BOX;
const _: () = assert!(RAIL_INSET >= RING);

fn circle(
    slot: Slot,
    diameter: f32,
    ground: Ground,
    muted: bool,
) -> Element<'static, SidebarAction> {
    let opacity = if muted { 0.55 } else { 1.0 };
    let mark = diameter * MARK;
    let content: Element<'static, SidebarAction> = match slot {
        Slot::Member(member) => widget::icon(match member {
            Member::Agent(provider, _) => crate::ui_controls::provider_icon(provider),
            Member::Terminal => Icon::Terminal.handle(),
        })
        .width(Length::Fixed(mark))
        .height(Length::Fixed(mark))
        .opacity(opacity)
        .into(),
        Slot::More(count) => {
            let label = more_label(count);
            // As large as the circle holds: at 1x a smaller "+" blurred into
            // a dot.
            widget::text(label.clone())
                .size(if label.len() > 2 {
                    diameter * 0.6
                } else {
                    diameter * 0.82
                })
                .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                    diameter.into(),
                ))
                .font(design::heading_font())
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .into()
        }
    };
    let outer = diameter + 2.0 * RING;
    widget::container(content)
        .center_x(Length::Fixed(outer))
        .center_y(Length::Fixed(outer))
        .align_x(Alignment::Center)
        .class(theme::Container::custom(move |theme| {
            let ring = ground.color(theme);
            let mut ink: Color = theme.current_container().on.into();
            ink.a *= opacity;
            widget::container::Style {
                background: Some(Background::Color(design::mix(
                    ring,
                    theme.current_container().on.into(),
                    CHIP_WASH,
                ))),
                icon_color: Some(ink),
                text_color: Some(ink),
                border: Border {
                    color: ring,
                    width: RING,
                    radius: (outer / 2.0).into(),
                },
                ..Default::default()
            }
        }))
        .into()
}

/// How far a circle's fill leans from the row's ground toward its ink: enough
/// to read as a disc on either theme, little enough that a provider's own
/// colours stay the strongest thing in it.
const CHIP_WASH: f32 = 0.16;

/// The rest of the members, as a circle can hold them.
fn more_label(count: usize) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        format!("+{count}")
    }
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
    }

    #[test]
    fn the_cluster_grows_from_one_circle_to_a_square_then_counts_the_rest() {
        let agents = [
            (Claude, MemberState::Working),
            (Codex, MemberState::Working),
            (Omp, MemberState::Idle),
            (Claude, MemberState::Idle),
            (Codex, MemberState::Idle),
            (Claude, MemberState::Running),
        ];
        assert_eq!(Members::default().cluster().arrangement(), None);
        let expected = [
            Arrangement::Single,
            Arrangement::Diagonal,
            Arrangement::Triangle,
            Arrangement::Square,
            Arrangement::Square,
            Arrangement::Square,
        ];
        for (count, arrangement) in (1..=agents.len()).zip(expected) {
            let cluster = members(&agents[..count], 0).cluster();
            assert_eq!(cluster.arrangement(), Some(arrangement), "{count} agents");
            assert_eq!(
                cluster.slots().count(),
                count.min(MAX_SLOTS),
                "{count} agents"
            );
            assert_eq!(arrangement.offsets().len(), count.min(MAX_SLOTS));
        }
        let six = members(&agents, 0).cluster();
        assert_eq!(
            six.slots().collect::<Vec<_>>(),
            vec![
                Slot::Member(Member::Agent(Claude, MemberState::Working)),
                Slot::Member(Member::Agent(Codex, MemberState::Working)),
                Slot::Member(Member::Agent(Omp, MemberState::Idle)),
                Slot::More(3),
            ]
        );
        // Exactly four still fits: no count takes a member's place.
        let four = members(&agents[..4], 0).cluster();
        assert!(four.slots().all(|slot| matches!(slot, Slot::Member(_))));
    }

    #[test]
    fn a_terminal_takes_a_spot_after_every_agent() {
        let cluster = members(&[(Claude, MemberState::Idle)], 1).cluster();
        assert_eq!(cluster.arrangement(), Some(Arrangement::Diagonal));
        assert_eq!(cluster.slots().last(), Some(Slot::Member(Member::Terminal)));
        let terminals = members(&[], 2);
        assert_eq!(terminals.description(), "2 Terminals");
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
            "2 Claude Code, working; Oh My Pi, stopped by a usage limit; Codex"
        );
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
    fn every_circle_stays_inside_the_box_and_its_ring_margin() {
        for arrangement in [
            Arrangement::Single,
            Arrangement::Diagonal,
            Arrangement::Triangle,
            Arrangement::Square,
        ] {
            let diameter = BOX * arrangement.diameter();
            let reach = (BOX - diameter) / 2.0;
            for (x, y) in arrangement.offsets() {
                for offset in [x, y] {
                    let near = BOX / 2.0 + offset * reach - diameter / 2.0;
                    assert!(near >= -f32::EPSILON, "{arrangement:?}");
                    assert!(near + diameter <= BOX + 1e-4, "{arrangement:?}");
                }
            }
        }
    }

    #[test]
    fn the_count_fits_its_circle() {
        assert_eq!(more_label(2), "+2");
        assert_eq!(more_label(99), "+99");
        assert_eq!(more_label(100), "99+");
    }
}
