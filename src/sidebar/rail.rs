//! The compact navigation rail's cell: one glyph, centred, with its status as
//! badges pinned to the glyph's corners.
//!
//! The rail used to reuse the expanded row's grid: a glyph slot followed by a
//! status slot, left-aligned in whatever width the rail left over. An unread
//! count stacked itself under its icon, a spinner sat beside the glyph and a
//! count wrapped onto a second line, so no two rows had the same silhouette.
//! Every rail row is now one fixed cell, and its status can only appear at one
//! of two fixed corners: news at the top right, activity at the bottom right.
//! Badges are drawn over the cell rather than laid out in it, so they never
//! move the glyph and never wrap.

use cosmic::{
    Element,
    iced::{Alignment, Background, Border, Color, Length},
    theme,
    widget::{self, button},
};

use super::{AttentionSummary, COMPACT_WIDTH, SidebarAction};
use crate::design;

/// One rail cell: the rail less an equal gutter each side. The right gutter is
/// the scroll thumb's own rail; the matching left one puts the glyph column on
/// the rail's centre line rather than off to one side of it.
pub(super) const CELL_WIDTH: f32 = COMPACT_WIDTH - 2.0 * design::SCROLLBAR_GUTTER;
/// Every rail row is the same height, projects included, so the rail scrolls
/// in a steady rhythm.
pub(super) const CELL_HEIGHT: f32 = design::ROW_HEIGHT;
/// The square each glyph is centred in and each badge is pinned to. It is the
/// same for a project's chip and a channel's glyph, so badges form one column
/// down the rail.
const FRAME: f32 = 30.0;
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
/// A news badge with no count to show, ring included.
const DOT: f32 = 10.0;
const SPINNER: f32 = 10.0;
/// The spinner's backing disc, ring included.
const SPINNER_DISC: f32 = SPINNER + RING;

const _: () = assert!(FRAME <= CELL_WIDTH && FRAME <= CELL_HEIGHT);

/// What a badge's colour says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    /// Unread news.
    Accent,
    /// An agent waiting for input, or working.
    Warning,
    /// A failed update.
    Danger,
    /// The application's own work: a deletion or a worktree being created.
    Muted,
}

impl Tone {
    fn fill(self, theme: &cosmic::Theme) -> Color {
        match self {
            Self::Accent => design::accent_color(theme),
            Self::Warning => design::warning_color(theme),
            Self::Danger => design::destructive_color(theme),
            Self::Muted => design::muted_icon_color(theme),
        }
    }

    fn ink(self, theme: &cosmic::Theme) -> Color {
        let cosmic = theme.cosmic();
        match self {
            Self::Accent => cosmic.accent.on.into(),
            Self::Warning => cosmic.warning.on.into(),
            Self::Danger => cosmic.destructive.on.into(),
            Self::Muted => cosmic.primary(theme.transparent).base.into(),
        }
    }

    fn spinner(self) -> fn(&cosmic::Theme) -> Color {
        match self {
            Self::Accent => design::accent_color,
            Self::Warning => design::warning_color,
            Self::Danger => design::destructive_color,
            Self::Muted => design::muted_icon_color,
        }
    }
}

/// The top-right badge: news, with its unread count when it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct News {
    pub tone: Tone,
    pub count: Option<usize>,
}

/// Everything a rail cell may pin to its glyph. Both corners are independent,
/// so an agent that is working in a channel with unread news shows both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Badges {
    pub news: Option<News>,
    pub activity: Option<Tone>,
}

impl Badges {
    /// The rail's reading of an attention summary. Waiting outranks a failure,
    /// which outranks plain unread news, for the badge's colour; the count is
    /// always the unread count, so it means the same thing on every row.
    pub fn attention(summary: AttentionSummary) -> Self {
        let tone = if summary.waiting != 0 {
            Some(Tone::Warning)
        } else if summary.failed != 0 {
            Some(Tone::Danger)
        } else if summary.unread != 0 {
            Some(Tone::Accent)
        } else {
            None
        };
        Self {
            news: tone.map(|tone| News {
                tone,
                count: (summary.unread != 0).then_some(summary.unread),
            }),
            activity: (summary.working != 0).then_some(Tone::Warning),
        }
    }

    /// Work the application is doing to the row itself.
    pub fn busy(tone: Tone) -> Self {
        Self {
            news: None,
            activity: Some(tone),
        }
    }

    /// A problem with the row that needs no count.
    pub fn alert(tone: Tone) -> Self {
        Self {
            news: Some(News { tone, count: None }),
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
            .center_x(Length::Fixed(FRAME))
            .center_y(Length::Fixed(FRAME))
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
    if let Some(tone) = badges.activity {
        layers.push(
            widget::container(activity_badge(tone))
                .class(design::transparent())
                .align_x(Alignment::End)
                .align_y(Alignment::End)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
    }
    let frame: Element<'a, SidebarAction> = if layers.len() == 1 {
        layers.pop().expect("the glyph layer")
    } else {
        cosmic::iced::widget::Stack::with_children(layers)
            .width(Length::Fixed(FRAME))
            .height(Length::Fixed(FRAME))
            .into()
    };
    widget::container(frame)
        .class(design::transparent())
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

fn ring(theme: &cosmic::Theme) -> Color {
    design::rail_tray(theme)
}

fn news_badge<'a>(news: News) -> Element<'a, SidebarAction> {
    let tone = news.tone;
    let style = move |theme: &cosmic::Theme| widget::container::Style {
        background: Some(Background::Color(tone.fill(theme))),
        text_color: Some(tone.ink(theme)),
        border: Border {
            color: ring(theme),
            width: RING,
            radius: (PILL_HEIGHT / 2.0).into(),
        },
        ..Default::default()
    };
    match news.count {
        Some(count) => {
            let label = count_label(count);
            // A single digit is a circle; longer counts grow the pill
            // leftwards over the glyph, never past the cell.
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
            .class(theme::Container::custom(style))
            .into()
        }
        None => widget::container(cosmic::iced::widget::Space::new())
            .width(Length::Fixed(DOT))
            .height(Length::Fixed(DOT))
            .class(theme::Container::custom(move |theme| {
                widget::container::Style {
                    border: Border {
                        radius: (DOT / 2.0).into(),
                        ..style(theme).border
                    },
                    ..style(theme)
                }
            }))
            .into(),
    }
}

fn activity_badge<'a>(tone: Tone) -> Element<'a, SidebarAction> {
    widget::container(crate::ui_controls::activity_spinner(
        SPINNER,
        tone.spinner(),
    ))
    .center_x(Length::Fixed(SPINNER_DISC))
    .center_y(Length::Fixed(SPINNER_DISC))
    .class(theme::Container::custom(|theme| widget::container::Style {
        background: Some(Background::Color(ring(theme))),
        border: Border {
            radius: (SPINNER_DISC / 2.0).into(),
            ..Default::default()
        },
        ..Default::default()
    }))
    .into()
}

/// A project's identity chip: its avatar, or its initials on its own colour.
pub(super) fn project_chip<'a>(
    avatar: Option<&cosmic::iced::advanced::image::Handle>,
    initials: String,
    color: Color,
    muted: bool,
) -> Element<'a, SidebarAction> {
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
            border_radius: design::RADIUS_S.into(),
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
        });
        assert_eq!(
            badges.news,
            Some(News {
                tone: Tone::Accent,
                count: Some(3)
            })
        );
        assert_eq!(badges.activity, Some(Tone::Warning));
    }

    #[test]
    fn waiting_colours_the_news_badge_without_inventing_a_count() {
        let badges = Badges::attention(AttentionSummary {
            waiting: 1,
            ..AttentionSummary::default()
        });
        assert_eq!(
            badges.news,
            Some(News {
                tone: Tone::Warning,
                count: None
            })
        );
        assert_eq!(badges.activity, None);
        let failed = Badges::attention(AttentionSummary {
            unread: 2,
            failed: 1,
            ..AttentionSummary::default()
        });
        assert_eq!(failed.news.map(|news| news.tone), Some(Tone::Danger));
        assert_eq!(failed.news.and_then(|news| news.count), Some(2));
        assert_eq!(
            Badges::attention(AttentionSummary::default()),
            Badges::default()
        );
    }
}
