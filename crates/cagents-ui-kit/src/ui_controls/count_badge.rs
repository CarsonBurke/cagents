//! A glyph at its normal size with a small round count on its lower right
//! corner: how many of the thing the glyph stands for there are.
//!
//! This is the one way the app pairs a glyph with a number. A count of one,
//! or none, draws the glyph alone, since the glyph already says "there is
//! one"; the tooltip and the accessible name carry the detail either way.
//! Stacking a caption-sized count under the glyph, as rows once did, gave a
//! number too small to read and a row twice the glyph's height.
//!
//! The badge is cut out of the glyph rather than ringed in a ground colour:
//! the hole is transparent, so it reads as set off from the glyph on a row's
//! resting, hovered and selected fill alike. A channel's member mark
//! ([`crate::sidebar`]'s `members`) draws its badge with the same pill, the
//! same widths and the same cut.

use std::{cell::RefCell, collections::HashMap};

use cosmic::{
    Element,
    iced::{Alignment, Background, Border, Color, Length, Point},
    theme,
    widget::{self, icon::Handle},
};

use super::Icon;
use crate::design::{self, Tone};

/// The badge's height.
pub const BADGE: f32 = 11.0;
/// The badge's width for one character, two ("9+", "12") and three ("99+").
pub const BADGE_WIDTHS: [f32; 3] = [11.0, 15.0, 19.0];
/// The clear ring cut around the badge in the glyph beneath it.
pub const BADGE_GAP: f32 = 1.0;
/// The count's size: a caption's weight in a pill small enough to sit on a
/// row glyph.
const BADGE_TEXT: f32 = 10.0;
/// How far a [`BadgeFill::Wash`] leans from its ground toward the ink:
/// clearly a pill on a resting, hovered or selected row, still quieter than
/// the title.
const BADGE_WASH: f32 = 0.3;

const _: () = assert!(
    BADGE_WIDTHS[0] == BADGE
        && BADGE_WIDTHS[1] > BADGE_WIDTHS[0]
        && BADGE_WIDTHS[2] > BADGE_WIDTHS[1]
);

/// The largest count a badge on a glyph spells out; more is "9+". A glyph's
/// corner has room for one character before the badge covers the glyph.
pub const GLYPH_MAX: usize = 9;
/// The largest count a bare pill spells out, as the rail's unread pill does.
pub const PILL_MAX: usize = 99;

/// The count as a badge holds it: nothing for one or none, the number up to
/// `max`, then `max` and a plus.
pub fn badge_label(count: usize, max: usize) -> Option<String> {
    match count {
        0 | 1 => None,
        count if count <= max => Some(count.to_string()),
        _ => Some(format!("{max}+")),
    }
}

/// The badge's width for `label`.
pub fn badge_width(label: &str) -> f32 {
    BADGE_WIDTHS[label.chars().count().clamp(1, BADGE_WIDTHS.len()) - 1]
}

/// Where a badge sits on its glyph, from the glyph's centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corner {
    /// The badge's right edge. A wider badge keeps it and grows left over
    /// the glyph, so whatever follows the glyph keeps its gap.
    pub right: f32,
    /// The badge's vertical centre.
    pub centre_y: f32,
}

impl Corner {
    /// The lower right corner of a glyph `size` square, on whole pixels. A
    /// one-character badge starts two pixels right of the glyph's centre
    /// line and one below its middle, so its cut clears the stem and dot a
    /// state glyph centres there ("!", "?") and covers only the corner.
    pub const fn of(size: u16) -> Self {
        let half = (size / 2) as f32;
        let left = half + 2.0;
        let top = half + 1.0;
        let centre = size as f32 / 2.0;
        Self {
            right: left + BADGE - centre,
            centre_y: top + BADGE / 2.0 - centre,
        }
    }

    /// How far a badge here reaches past the right edge of its glyph: the
    /// room to leave after a badged glyph so the badge clears what follows.
    pub const fn overhang(size: u16) -> f32 {
        Self::of(size).right - size as f32 / 2.0
    }
}

/// What the badge is filled with.
#[derive(Clone, Copy)]
pub enum BadgeFill {
    /// A neutral pill: the ground beneath it leaned toward the ink, with the
    /// count in the ink. For a count that is not news, such as how many
    /// members a channel has.
    Wash(fn(&cosmic::Theme) -> Color),
    /// A semantic colour with its own ink, for a count of news: unread
    /// updates, agents waiting, failures.
    Tone {
        fill: fn(&cosmic::Theme) -> Color,
        ink: fn(&cosmic::Theme) -> Color,
    },
}

impl BadgeFill {
    /// Unread news, in the accent the rail's unread pill uses.
    pub const ACCENT: Self = Self::Tone {
        fill: design::accent_color,
        ink: accent_ink,
    };
    /// Something that needs the user before it can go on.
    pub const WARNING: Self = Self::Tone {
        fill: design::warning_color,
        ink: warning_ink,
    };
    /// A failure.
    pub const DANGER: Self = Self::Tone {
        fill: design::destructive_color,
        ink: destructive_ink,
    };
    /// A neutral count on the surface the current container draws.
    pub const NEUTRAL: Self = Self::Wash(design::base);

    fn colors(self, theme: &cosmic::Theme) -> (Color, Color) {
        match self {
            Self::Wash(ground) => {
                let on: Color = theme.current_container().on.into();
                (design::mix(ground(theme), on, BADGE_WASH), on)
            }
            Self::Tone { fill, ink } => (fill(theme), ink(theme)),
        }
    }
}

fn accent_ink(theme: &cosmic::Theme) -> Color {
    theme.cosmic().accent.on.into()
}

fn warning_ink(theme: &cosmic::Theme) -> Color {
    theme.cosmic().warning.on.into()
}

fn destructive_ink(theme: &cosmic::Theme) -> Color {
    theme.cosmic().destructive.on.into()
}

/// The badge itself, `label` in a pill as wide as [`badge_width`] gives,
/// drawn at `opacity`.
pub fn count_pill<Message: 'static>(
    label: String,
    fill: BadgeFill,
    opacity: f32,
) -> Element<'static, Message> {
    let width = badge_width(&label);
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
        let (mut fill, mut ink) = fill.colors(theme);
        fill.a *= opacity;
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

/// `label` in a pill centred on a glyph `size` square, standing in for the
/// glyph: a count that is itself the mark, as unread news is. The layout is
/// the square, so a dot that grows into a count moves nothing beside it; a
/// pill wider than the square overhangs it evenly, on whole pixels.
pub fn pill_in<Message: 'static>(
    label: String,
    fill: BadgeFill,
    size: u16,
) -> Element<'static, Message> {
    let square = f32::from(size);
    let width = badge_width(&label);
    cosmic::iced::widget::Stack::with_children(vec![at(
        count_pill(label, fill, 1.0),
        ((square - width) / 2.0).floor(),
        ((square - BADGE) / 2.0).floor(),
    )])
    .width(Length::Fixed(square))
    .height(Length::Fixed(square))
    .into()
}

/// The diameter of the dot that says there is news on a glyph `size`
/// square: a third of it, as the unread dot that stands for a row is.
const fn news_dot(size: u16) -> f32 {
    ((size + 1) / 3) as f32
}

/// The news dot's top left on a glyph `size` square: centred half a pixel
/// past its upper right corner, outside a spinner's ring.
const fn news_dot_at(size: u16) -> (f32, f32) {
    let dot = news_dot(size);
    let centre = size as f32 + 0.5;
    (centre - dot / 2.0, 0.5 - dot / 2.0)
}

fn news_dot_layer<Message: 'static>(size: u16) -> Element<'static, Message> {
    let dot = news_dot(size);
    let (x, y) = news_dot_at(size);
    at(
        widget::container(cosmic::iced::widget::Space::new())
            .width(Length::Fixed(dot))
            .height(Length::Fixed(dot))
            .class(theme::Container::custom(move |theme| {
                widget::container::Style {
                    background: Some(Background::Color(design::accent_color(theme))),
                    border: Border {
                        radius: (dot / 2.0).into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }))
            .into(),
        x,
        y,
    )
}

/// `mark`, a spinner `size` square, with the news dot on its upper right
/// corner, clear of the ring: unread updates on something still at work,
/// which the rail shows in the same corner. Not for a glyph, whose corner
/// the dot would cover without a cut.
pub fn with_news<Message: 'static>(
    mark: Element<'static, Message>,
    size: u16,
) -> Element<'static, Message> {
    let square = f32::from(size);
    cosmic::iced::widget::Stack::with_children(vec![mark, news_dot_layer(size)])
        .width(Length::Fixed(square))
        .height(Length::Fixed(square))
        .into()
}

/// `icon` at `size`, in `tone`, with `count` on its lower right corner when
/// there is more than one; the glyph alone otherwise. The layout is the
/// glyph's square, so a glyph that gains a badge moves nothing beside it;
/// the badge overhangs that square to the right and below by
/// [`Corner::overhang`]. A count past [`GLYPH_MAX`] widens the badge over
/// the glyph's middle.
pub fn badged<Message: 'static>(
    icon: Icon,
    size: u16,
    tone: Tone,
    count: usize,
    fill: BadgeFill,
) -> Element<'static, Message> {
    let glyph = |handle: Handle| -> Element<'static, Message> {
        widget::container(widget::icon(handle).size(size))
            .class(theme::Container::custom(move |theme| {
                widget::container::Style {
                    icon_color: Some(tone.resolve(theme)),
                    ..Default::default()
                }
            }))
            .into()
    };
    let Some(label) = badge_label(count, GLYPH_MAX) else {
        return glyph(icon.handle());
    };
    let square = f32::from(size);
    let corner = Corner::of(size);
    let width = badge_width(&label);
    let centre = square / 2.0;
    cosmic::iced::widget::Stack::with_children(vec![
        glyph(cut_icon(icon, size, width)),
        at(
            count_pill(label, fill, 1.0),
            centre + corner.right - width,
            centre + corner.centre_y - BADGE / 2.0,
        ),
    ])
    .width(Length::Fixed(square))
    .height(Length::Fixed(square))
    .into()
}

/// `content` with its top left at `(x, y)` in a glyph's box. It keeps its
/// own size where it reaches past the box: a badge overhangs it.
pub fn at<Message: 'static>(
    content: Element<'static, Message>,
    x: f32,
    y: f32,
) -> Element<'static, Message> {
    cosmic::iced::widget::pin(content)
        .position(Point::new(x, y))
        .overflow(true)
        .into()
}

/// The pill a badge `width` wide at `corner` clears from a glyph `size`
/// square, in that glyph's own coordinates.
pub fn badge_hole(size: f32, corner: Corner, width: f32) -> Hole {
    Hole::Pill {
        left: size / 2.0 + corner.right - width - BADGE_GAP,
        top: size / 2.0 + corner.centre_y - BADGE / 2.0 - BADGE_GAP,
        width: width + 2.0 * BADGE_GAP,
        height: BADGE + 2.0 * BADGE_GAP,
    }
}

/// `icon` with the hole a badge `width` wide at its lower right corner
/// cut out. One handle per glyph, size and width for the life of the
/// thread, so its rasterization stays in the renderer's cache.
fn cut_icon(icon: Icon, size: u16, width: f32) -> Handle {
    thread_local! {
        static CUT: RefCell<HashMap<(Icon, u16, u32), Handle>> = RefCell::new(HashMap::new());
    }
    CUT.with_borrow_mut(|cut| {
        cut.entry((icon, size, width.to_bits()))
            .or_insert_with(|| {
                let square = f32::from(size);
                std::str::from_utf8(icon.svg())
                    .ok()
                    .and_then(|svg| {
                        cut_svg(svg, square, &[badge_hole(square, Corner::of(size), width)])
                    })
                    .map_or_else(
                        // A drawing the cut cannot read keeps its whole
                        // glyph; the badge still sits on it.
                        || icon.handle(),
                        |svg| widget::icon::from_svg_bytes(svg.into_bytes()).symbolic(true),
                    )
            })
            .clone()
    })
}

/// A region cleared from a glyph, in logical pixels of the glyph drawn
/// `size` square.
#[derive(Debug, Clone, Copy)]
pub enum Hole {
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

/// `svg` with `holes` masked out of everything it draws. The hole is truly
/// transparent rather than painted in a ground colour, which could match the
/// row only at rest. `None` when `svg` has no root element with a viewBox.
pub fn cut_svg(svg: &str, size: f32, holes: &[Hole]) -> Option<String> {
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
    let shape: String = holes
        .iter()
        .map(|hole| match *hole {
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
        })
        .collect();
    Some(format!(
        r#"{head}<defs><mask id="cagents-cut" maskUnits="userSpaceOnUse" x="{min_x}" y="{min_y}" width="{width}" height="{height}"><rect x="{min_x}" y="{min_y}" width="{width}" height="{height}" fill="white"/>{shape}</mask></defs><g mask="url(#cagents-cut)">{body}</g></svg>"#,
        head = &svg[..open_end],
        body = &svg[open_end..close],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_the_glyph_alone_and_a_count_past_its_cap_gains_a_plus() {
        assert_eq!(badge_label(0, GLYPH_MAX), None);
        assert_eq!(badge_label(1, GLYPH_MAX), None);
        assert_eq!(badge_label(2, GLYPH_MAX).as_deref(), Some("2"));
        assert_eq!(badge_label(9, GLYPH_MAX).as_deref(), Some("9"));
        assert_eq!(badge_label(10, GLYPH_MAX).as_deref(), Some("9+"));
        assert_eq!(badge_label(12, PILL_MAX).as_deref(), Some("12"));
        assert_eq!(badge_label(250, PILL_MAX).as_deref(), Some("99+"));
        assert_eq!(badge_width("2"), BADGE_WIDTHS[0]);
        assert_eq!(badge_width("9+"), BADGE_WIDTHS[1]);
        assert_eq!(badge_width("99+"), BADGE_WIDTHS[2]);
    }

    /// A one-character badge's cut stays clear of the column a state
    /// glyph's "!" or "?" stands in, so the badge never eats its dot.
    #[test]
    fn a_badge_clears_the_middle_of_its_glyph() {
        for size in [design::LEADING_ICON_SIZE, design::ROW_ICON_SIZE] {
            let square = f32::from(size);
            let Hole::Pill { left, .. } = badge_hole(square, Corner::of(size), BADGE) else {
                unreachable!()
            };
            // The dot of a 16 unit glyph is 1.4 units wide on its centre.
            assert!(left >= square / 2.0 + 0.7 * square / 16.0, "{size}: {left}");
        }
    }

    /// Every glyph a count can sit on has a drawing the cut can read, so no
    /// badge falls back to sitting on an uncut glyph.
    #[test]
    fn every_state_glyph_can_be_cut() {
        let states = crate::sidebar::AttentionState::ALL
            .into_iter()
            .filter(|state| *state != crate::sidebar::AttentionState::Working)
            .filter_map(crate::sidebar::AttentionState::glyph);
        for icon in states.chain([Icon::PullRequest, Icon::Attachment]) {
            let svg = std::str::from_utf8(icon.svg()).unwrap();
            let size = f32::from(design::ROW_ICON_SIZE);
            assert!(
                cut_svg(
                    svg,
                    size,
                    &[badge_hole(size, Corner::of(design::ROW_ICON_SIZE), BADGE)]
                )
                .is_some(),
                "{icon:?}"
            );
        }
    }

    /// The bounds of each layer `element` draws, bottom first.
    fn layers(
        mut element: Element<'static, ()>,
    ) -> (cosmic::iced::Rectangle, Vec<cosmic::iced::Rectangle>) {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
        let renderer = crate::sidebar::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(
                cosmic::iced::Size::ZERO,
                cosmic::iced::Size::new(100.0, 100.0),
            ),
        );
        let root = Layout::new(&node);
        let children = root
            .children()
            .map(|child| {
                child
                    .children()
                    .next()
                    .map_or(child.bounds(), |at| at.bounds())
            })
            .collect();
        (root.bounds(), children)
    }

    fn glyph(size: u16, count: usize) -> Element<'static, ()> {
        badged(
            Icon::Attention,
            size,
            Tone::Themed(design::muted_icon_color),
            count,
            BadgeFill::ACCENT,
        )
    }

    /// A badge takes no room of its own: the glyph's square is the layout
    /// whether or not it shows, and the badge sits on its lower right on
    /// whole pixels, overhanging it by `Corner::overhang`.
    #[test]
    fn a_badge_sits_on_the_glyphs_lower_right_without_growing_it() {
        for size in [design::LEADING_ICON_SIZE, design::ROW_ICON_SIZE] {
            let square = f32::from(size);
            let (alone, _) = layers(glyph(size, 1));
            assert_eq!((alone.width, alone.height), (square, square));
            let (badged, layers) = layers(glyph(size, 3));
            assert_eq!((badged.width, badged.height), (square, square));
            let [glyph, badge] = layers[..] else {
                panic!("a glyph and its badge: {layers:?}");
            };
            assert_eq!((glyph.width, glyph.height), (square, square));
            assert_eq!((badge.width, badge.height), (BADGE, BADGE));
            assert!(badge.center_x() > glyph.center_x() && badge.center_y() > glyph.center_y());
            assert_eq!(badge.x + badge.width, square + Corner::overhang(size));
            assert_eq!((badge.x.fract(), badge.y.fract()), (0.0, 0.0), "{badge:?}");
        }
    }

    /// News on a spinner is a dot on its upper right corner, clear of the
    /// ring, without growing its square.
    #[test]
    fn news_is_a_dot_clear_of_the_spinner() {
        let size = design::LEADING_ICON_SIZE;
        let square = f32::from(size);
        let (bounds, layers) = layers(with_news(
            super::super::activity_spinner(square, design::activity_color),
            size,
        ));
        assert_eq!((bounds.width, bounds.height), (square, square));
        let [_, dot] = layers[..] else {
            panic!("a spinner and its news: {layers:?}");
        };
        assert!(dot.center_y() < square / 2.0 && dot.center_x() > square / 2.0);
        assert_eq!(dot.width, dot.height);
        assert_eq!((dot.x.fract(), dot.y.fract()), (0.0, 0.0), "{dot:?}");
        let centre = square / 2.0;
        let reach = ((dot.center_x() - centre).powi(2) + (dot.center_y() - centre).powi(2)).sqrt();
        assert!(reach - dot.width / 2.0 >= centre, "{reach}");
    }

    /// A pill standing for a glyph keeps the glyph's square and centres on
    /// it, overhanging evenly when wider, on whole pixels.
    #[test]
    fn a_pill_in_a_glyphs_place_keeps_its_square() {
        let size = design::LEADING_ICON_SIZE;
        let square = f32::from(size);
        for label in ["2", "12", "99+"] {
            let (bounds, layers) = layers(pill_in(label.to_owned(), BadgeFill::ACCENT, size));
            assert_eq!((bounds.width, bounds.height), (square, square));
            let [pill] = layers[..] else {
                panic!("{layers:?}");
            };
            assert_eq!(pill.width, badge_width(label));
            assert!(
                (pill.center_x() - square / 2.0).abs() <= 0.5,
                "{label}: {pill:?}"
            );
            assert_eq!((pill.x.fract(), pill.y.fract()), (0.0, 0.0), "{pill:?}");
        }
    }

    #[test]
    fn a_cut_masks_everything_the_artwork_draws() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" fill="black"><path d="M0 0h22v22H0z"/></svg>"#;
        let masked = cut_svg(
            svg,
            11.0,
            &[Hole::Circle {
                x: 1.0,
                y: 2.0,
                radius: 3.0,
            }],
        )
        .unwrap();
        assert_eq!(
            masked,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" fill="black"><defs><mask id="cagents-cut" maskUnits="userSpaceOnUse" x="0" y="0" width="22" height="22"><rect x="0" y="0" width="22" height="22" fill="white"/><ellipse cx="2" cy="4" rx="6" ry="6" fill="black"/></mask></defs><g mask="url(#cagents-cut)"><path d="M0 0h22v22H0z"/></g></svg>"#
        );
        assert!(
            cut_svg(
                "<path/>",
                11.0,
                &[Hole::Circle {
                    x: 0.0,
                    y: 0.0,
                    radius: 1.0
                }]
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
            cut_svg(svg, 11.0, &[hole])
                .unwrap()
                .contains(r#"<ellipse cx="2" cy="4" rx="6" ry="6" fill="black"/>"#)
        );
    }
}
