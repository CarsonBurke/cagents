//! The compact navigation rail's cell: one glyph, centred, with its status as
//! badges pinned to the cell's corners.
//!
//! The rail used to reuse the expanded row's grid: a glyph slot followed by a
//! status slot, left-aligned in whatever width the rail left over. An unread
//! count stacked itself under its icon, a spinner sat beside the glyph and a
//! count wrapped onto a second line, so no two rows had the same silhouette.
//! Every rail row is now one fixed cell, and its status can only appear at one
//! of two fixed corners: news at the top right, activity at the bottom right.
//! Badges are drawn over the cell rather than laid out in it, so they never
//! move the glyph and never wrap. Each kind of news has its own shape as well
//! as its own colour, the shapes the expanded sidebar uses for the same
//! states, so waiting and failed stay apart where the theme's warning and
//! destructive colours are close, and for a reader who cannot tell them
//! apart by colour.

use cosmic::{
    Element,
    iced::{Alignment, Background, Border, Color, Length},
    theme,
    widget::{self, button},
};

use super::{AttentionSummary, COMPACT_WIDTH, Icon, SidebarAction};
use crate::design;

/// One rail cell: the rail less an equal gutter each side. The right gutter is
/// the scroll thumb's own rail; the matching left one puts the glyph column on
/// the rail's centre line rather than off to one side of it.
pub(super) const CELL_WIDTH: f32 = COMPACT_WIDTH - 2.0 * design::SCROLLBAR_GUTTER;
/// Every rail row is the same height, projects included, so the rail scrolls
/// in a steady rhythm.
pub(super) const CELL_HEIGHT: f32 = design::ROW_HEIGHT;
/// A project's identity: its avatar or initials.
pub(super) const PROJECT_CHIP: f32 = 22.0;
/// A channel, worktree or operation inside a project.
pub(super) const CHILD_GLYPH: u16 = design::ROW_ICON_SIZE;
/// A count's pill, ring included.
const PILL_HEIGHT: f32 = 14.0;
/// A badge is cut out of what it overlaps by a ring of the project tray it
/// sits on, so it reads as attached to the glyph rather than drawn on top of
/// it.
const RING: f32 = 2.0;
/// Unread news with no count to show, ring included.
const DOT: f32 = 10.0;
/// A status glyph's backing disc, ring included: the warning triangle, the
/// error mark and the spinner.
const DISC: f32 = 14.0;
const STATUS_GLYPH: u16 = 12;
const SPINNER: f32 = 10.0;

const _: () = assert!(PILL_HEIGHT <= DISC && DOT <= DISC);
const _: () = assert!(DISC <= CELL_HEIGHT / 2.0);

/// The top-right badge: the one piece of news most worth acting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum News {
    /// Unread updates: an accent pill with their count, or a dot when there
    /// is none to give.
    Unread(Option<usize>),
    /// Something needs the user before it can go on: an agent waiting for
    /// input, a deletion that failed, a project that cannot be reached. The
    /// expanded sidebar's warning triangle.
    Warning,
    /// An agent's update failed. The expanded sidebar's error mark.
    Failed,
    /// A provider usage limit stops an agent until it resets. The expanded
    /// sidebar's alarm.
    Limited,
}

/// Who is doing the work a bottom-right spinner shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activity {
    /// An agent, in the expanded sidebar's working colour.
    Agent,
    /// The application itself: a deletion, or a worktree being created.
    App,
}

/// Everything a rail cell may pin to its glyph. Both corners are independent,
/// so an agent that is working in a channel with unread news shows both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Badges {
    pub news: Option<News>,
    pub activity: Option<Activity>,
}

impl Badges {
    /// The rail's reading of an attention summary. One badge has room for one
    /// thing, so it shows what most needs the user: an agent waiting on them,
    /// then a failed update, then the unread count. The tooltip and the
    /// accessible name still give the whole summary.
    pub fn attention(summary: AttentionSummary) -> Self {
        let news = if summary.waiting != 0 {
            Some(News::Warning)
        } else if summary.failed != 0 {
            Some(News::Failed)
        } else if summary.limited != 0 {
            Some(News::Limited)
        } else if summary.unread != 0 {
            Some(News::Unread(Some(summary.unread)))
        } else {
            None
        };
        Self {
            news,
            activity: (summary.working != 0).then_some(Activity::Agent),
        }
    }

    /// Work the application is doing to the row itself.
    pub fn busy() -> Self {
        Self {
            news: None,
            activity: Some(Activity::App),
        }
    }

    /// A problem with the row that needs the user.
    pub fn warning() -> Self {
        Self {
            news: Some(News::Warning),
            activity: None,
        }
    }
}

/// The count as a pill can hold it: three characters at most.
pub(super) fn count_label(count: usize) -> String {
    if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    }
}

/// A rail cell's content: the glyph centred in its frame, with its badges.
pub(super) fn cell<'a>(
    glyph: impl Into<Element<'a, SidebarAction>>,
    badges: Badges,
) -> Element<'a, SidebarAction> {
    let mut layers: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(3);
    layers.push(
        widget::container(glyph)
            .class(design::transparent())
            .center_x(Length::Fixed(CELL_WIDTH))
            .center_y(Length::Fixed(CELL_HEIGHT))
            .into(),
    );
    if let Some(news) = badges.news {
        layers.push(
            widget::container(news_badge(news))
                .class(design::transparent())
                .align_x(Alignment::End)
                .align_y(Alignment::Start)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
    }
    if let Some(activity) = badges.activity {
        layers.push(
            widget::container(activity_badge(activity))
                .class(design::transparent())
                .align_x(Alignment::End)
                .align_y(Alignment::End)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
    }
    if layers.len() == 1 {
        layers.pop().expect("the glyph layer")
    } else {
        cosmic::iced::widget::Stack::with_children(layers)
            .width(Length::Fixed(CELL_WIDTH))
            .height(Length::Fixed(CELL_HEIGHT))
            .into()
    }
}

fn ring(theme: &cosmic::Theme) -> Color {
    design::rail_tray(theme)
}

fn news_badge<'a>(news: News) -> Element<'a, SidebarAction> {
    match news {
        News::Unread(Some(count)) => unread_pill(count),
        News::Unread(None) => widget::container(cosmic::iced::widget::Space::new())
            .width(Length::Fixed(DOT))
            .height(Length::Fixed(DOT))
            .class(theme::Container::custom(|theme| widget::container::Style {
                background: Some(Background::Color(design::accent_color(theme))),
                border: Border {
                    color: ring(theme),
                    width: RING,
                    radius: (DOT / 2.0).into(),
                },
                ..Default::default()
            }))
            .into(),
        News::Warning => status_disc(
            widget::icon(Icon::Warning.handle())
                .size(STATUS_GLYPH)
                .into(),
            design::warning_color,
        ),
        News::Failed => status_disc(
            widget::icon(Icon::Error.handle()).size(STATUS_GLYPH).into(),
            design::destructive_color,
        ),
        News::Limited => status_disc(
            widget::icon(Icon::Alarm.handle()).size(STATUS_GLYPH).into(),
            design::warning_color,
        ),
    }
}

fn unread_pill<'a>(count: usize) -> Element<'a, SidebarAction> {
    let label = count_label(count);
    // A single digit is a circle; longer counts grow the pill leftwards over
    // the glyph, never past the cell.
    let width = if label.len() == 1 {
        Length::Fixed(PILL_HEIGHT)
    } else {
        Length::Shrink
    };
    widget::container(
        widget::text(label)
            .size(9.5)
            .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                (PILL_HEIGHT - 2.0 * RING).into(),
            ))
            .font(design::heading_font())
            .wrapping(cosmic::iced::widget::text::Wrapping::None),
    )
    .padding([0.0, RING + 1.5])
    .center_y(Length::Fixed(PILL_HEIGHT))
    .align_x(Alignment::Center)
    .width(width)
    .class(theme::Container::custom(|theme| widget::container::Style {
        background: Some(Background::Color(design::accent_color(theme))),
        text_color: Some(theme.cosmic().accent.on.into()),
        border: Border {
            color: ring(theme),
            width: RING,
            radius: (PILL_HEIGHT / 2.0).into(),
        },
        ..Default::default()
    }))
    .into()
}

/// A status glyph on a disc of the tray's colour, which cuts it out of the
/// glyph it overlaps.
fn status_disc<'a>(
    content: Element<'a, SidebarAction>,
    ink: fn(&cosmic::Theme) -> Color,
) -> Element<'a, SidebarAction> {
    widget::container(content)
        .center_x(Length::Fixed(DISC))
        .center_y(Length::Fixed(DISC))
        .class(theme::Container::custom(move |theme| {
            widget::container::Style {
                background: Some(Background::Color(ring(theme))),
                icon_color: Some(ink(theme)),
                border: Border {
                    radius: (DISC / 2.0).into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .into()
}

fn activity_badge<'a>(activity: Activity) -> Element<'a, SidebarAction> {
    let ink: fn(&cosmic::Theme) -> Color = match activity {
        Activity::Agent => design::warning_color,
        Activity::App => design::muted_icon_color,
    };
    status_disc(crate::ui_controls::activity_spinner(SPINNER, ink), ink)
}

/// A project's identity chip: its avatar, or its initials on its own colour.
pub(super) fn project_chip<'a, Message: 'a>(
    avatar: Option<&cosmic::iced::advanced::image::Handle>,
    initials: String,
    color: Color,
    muted: bool,
) -> Element<'a, Message> {
    let opacity = if muted { 0.5 } else { 1.0 };
    match avatar {
        Some(handle) => cosmic::iced::widget::image(handle.clone())
            .width(Length::Fixed(PROJECT_CHIP))
            .height(Length::Fixed(PROJECT_CHIP))
            .border_radius(design::RADIUS_S)
            .opacity(opacity)
            .into(),
        None => widget::container(
            widget::text(initials)
                .size(design::TEXT_CAPTION)
                .font(design::heading_font())
                .wrapping(cosmic::iced::widget::text::Wrapping::None),
        )
        .center_x(Length::Fixed(PROJECT_CHIP))
        .center_y(Length::Fixed(PROJECT_CHIP))
        .class(theme::Container::custom(move |_| {
            widget::container::Style {
                background: Some(Background::Color(color.scale_alpha(opacity))),
                // Identity colours are generated dark and saturated for white
                // initials in either theme; see `project_identity`.
                text_color: Some(Color::WHITE.scale_alpha(if muted { 0.7 } else { 1.0 })),
                border: Border {
                    radius: design::RADIUS_S.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .into(),
    }
}

/// A project's rows in the rail, kept together on a shared tray when the
/// project shows any rows beneath its chip, so where one project ends and the
/// next begins is visible without labels.
pub(super) fn group<'a>(
    content: Element<'a, SidebarAction>,
    has_children: bool,
) -> Element<'a, SidebarAction> {
    let container = widget::container(content).width(Length::Fixed(CELL_WIDTH));
    if has_children {
        container
            .class(theme::Container::custom(|theme| widget::container::Style {
                background: Some(Background::Color(design::rail_tray(theme))),
                border: Border {
                    radius: design::RADIUS_M.into(),
                    ..Default::default()
                },
                ..Default::default()
            }))
            .into()
    } else {
        container.class(design::transparent()).into()
    }
}

/// A rail row's button: a rounded tile over the whole cell, so the pointer
/// target is the full cell, with a ring outside it for keyboard focus.
pub(super) fn control<'a>(
    content: Element<'a, SidebarAction>,
    selected: bool,
) -> button::Button<'a, SidebarAction> {
    button::custom(content)
        .class(if selected {
            tile_style::<true>()
        } else {
            tile_style::<false>()
        })
        .selected(selected)
        .padding(0)
        .width(Length::Fixed(CELL_WIDTH))
        .height(Length::Fixed(CELL_HEIGHT))
}

fn tile_style<const SELECTED: bool>() -> theme::Button {
    fn appearance(
        theme: &cosmic::Theme,
        focused: bool,
        selected: bool,
        lift: f32,
        disabled: bool,
    ) -> button::Style {
        let ink: Color = theme.current_container().on.into();
        let wash = if selected { 0.12 } else { 0.0 } + lift;
        button::Style {
            background: (wash > 0.0).then(|| Background::Color(ink.scale_alpha(wash))),
            border_radius: theme.cosmic().corner_radii.radius_s.into(),
            outline_width: if focused { design::BORDER_WIDTH } else { 0.0 },
            outline_color: if focused {
                design::accent_color(theme)
            } else {
                Color::TRANSPARENT
            },
            icon_color: Some(if disabled { ink.scale_alpha(0.5) } else { ink }),
            text_color: Some(if disabled { ink.scale_alpha(0.5) } else { ink }),
            ..design::button_style()
        }
    }
    theme::Button::Custom {
        active: Box::new(|focused, theme| appearance(theme, focused, SELECTED, 0.0, false)),
        disabled: Box::new(|theme| appearance(theme, false, SELECTED, 0.0, true)),
        hovered: Box::new(|focused, theme| appearance(theme, focused, SELECTED, 0.07, false)),
        pressed: Box::new(|focused, theme| appearance(theme, focused, SELECTED, 0.1, false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_fit_a_three_character_pill() {
        assert_eq!(count_label(1), "1");
        assert_eq!(count_label(99), "99");
        assert_eq!(count_label(100), "99+");
    }

    #[test]
    fn badges_keep_news_and_activity_on_separate_corners() {
        let badges = Badges::attention(AttentionSummary {
            unread: 3,
            working: 1,
            failed: 0,
            waiting: 0,
            limited: 0,
        });
        assert_eq!(badges.news, Some(News::Unread(Some(3))));
        assert_eq!(badges.activity, Some(Activity::Agent));
    }

    /// Waiting and failed differ in shape, not only in a colour some themes
    /// and some readers barely tell apart; a waiting agent outranks a failed
    /// update, which outranks a usage limit, which outranks unread news: the
    /// order of the expanded sidebar's `AttentionSummary::state`.
    #[test]
    fn the_news_badge_shows_what_most_needs_the_user() {
        let waiting = Badges::attention(AttentionSummary {
            waiting: 1,
            failed: 1,
            unread: 4,
            ..AttentionSummary::default()
        });
        assert_eq!(waiting.news, Some(News::Warning));
        assert_eq!(waiting.activity, None);
        let failed = Badges::attention(AttentionSummary {
            unread: 2,
            failed: 1,
            ..AttentionSummary::default()
        });
        assert_eq!(failed.news, Some(News::Failed));
        let limited = AttentionSummary {
            unread: 2,
            working: 1,
            ..AttentionSummary::default()
        }
        .limited(true);
        let limited = Badges::attention(limited);
        assert_eq!(limited.news, Some(News::Limited));
        // A limited agent is not working, whatever it last reported.
        assert_eq!(limited.activity, None);
        assert_eq!(
            Badges::attention(AttentionSummary::default()),
            Badges::default()
        );
    }
}
