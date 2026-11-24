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
pub const BADGE: f32 = 12.0;
/// The badge's width for one character and for two ("9+").
pub const BADGE_WIDTHS: [f32; 2] = [12.0, 15.0];
/// The clear ring cut around the badge in the glyph beneath it.
pub const BADGE_GAP: f32 = 1.0;
/// The count's size: a caption's weight in a pill small enough to sit on a
/// row glyph.
const BADGE_TEXT: f32 = 10.0;
/// How far a [`BadgeFill::Wash`] leans from its ground toward the ink:
/// clearly a pill on a resting, hovered or selected row, still quieter than
/// the title.
const BADGE_WASH: f32 = 0.3;

const _: () = assert!(BADGE_WIDTHS[0] == BADGE && BADGE_WIDTHS[1] > BADGE);

/// The count as a badge holds it: nothing for one or none, then one
/// character, then "9+".
pub fn badge_label(count: usize) -> Option<String> {
    match count {
        0 | 1 => None,
        2..=9 => Some(count.to_string()),
        _ => Some("9+".to_owned()),
    }
}

/// The badge's width for `label`.
pub fn badge_width(label: &str) -> f32 {
    BADGE_WIDTHS[usize::from(label.chars().count() > 1)]
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
    /// The lower right corner of a glyph `size` square: the badge overhangs
    /// its right edge a little and is centred on its bottom edge, covering
    /// only that corner of the glyph.
    pub const fn of(size: f32) -> Self {
        Self {
            right: size / 2.0 + 3.5,
            centre_y: size / 2.0 - 0.5,
        }
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

/// `icon` at `size`, in `tone`, with `count` on its lower right corner when
/// there is more than one; the glyph alone otherwise. The layout is the
/// glyph's square, so a glyph that gains a badge moves nothing beside it;
/// the badge overhangs that square to the right and below.
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
    let Some(label) = badge_label(count) else {
        return glyph(icon.handle());
    };
    let square = f32::from(size);
    let corner = Corner::of(square);
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
        static CUT: RefCell<HashMap<(Icon, u16, bool), Handle>> = RefCell::new(HashMap::new());
    }
    let wide = width > BADGE_WIDTHS[0];
    CUT.with_borrow_mut(|cut| {
        cut.entry((icon, size, wide))
            .or_insert_with(|| {
                let square = f32::from(size);
                std::str::from_utf8(icon.svg())
                    .ok()
                    .and_then(|svg| {
                        cut_svg(svg, square, badge_hole(square, Corner::of(square), width))
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

/// `svg` with `hole` masked out of everything it draws. The hole is truly
/// transparent rather than painted in a ground colour, which could match the
/// row only at rest. `None` when `svg` has no root element with a viewBox.
pub fn cut_svg(svg: &str, size: f32, hole: Hole) -> Option<String> {
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

    #[test]
    fn one_is_the_glyph_alone_and_more_than_nine_is_nine_plus() {
        assert_eq!(badge_label(0), None);
        assert_eq!(badge_label(1), None);
        assert_eq!(badge_label(2).as_deref(), Some("2"));
        assert_eq!(badge_label(9).as_deref(), Some("9"));
        assert_eq!(badge_label(10).as_deref(), Some("9+"));
        assert_eq!(badge_label(250).as_deref(), Some("9+"));
        assert_eq!(badge_width("2"), BADGE_WIDTHS[0]);
        assert_eq!(badge_width("9+"), BADGE_WIDTHS[1]);
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
                cut_svg(svg, size, badge_hole(size, Corner::of(size), BADGE)).is_some(),
                "{icon:?}"
            );
        }
    }

    /// The bounds of each layer `badged` draws, bottom first.
    fn layers(count: usize) -> (cosmic::iced::Rectangle, Vec<cosmic::iced::Rectangle>) {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
        let mut element: Element<'static, ()> = badged(
            Icon::Attention,
            design::ROW_ICON_SIZE,
            Tone::Themed(design::muted_icon_color),
            count,
            BadgeFill::ACCENT,
        );
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

    /// A badge takes no room of its own: the glyph's square is the layout
    /// whether or not it shows, and the badge sits on its lower right,
    /// overhanging it a little.
    #[test]
    fn a_badge_sits_on_the_glyphs_lower_right_without_growing_it() {
        let size = f32::from(design::ROW_ICON_SIZE);
        let (alone, _) = layers(1);
        assert_eq!((alone.width, alone.height), (size, size));
        let (badged, layers) = layers(3);
        assert_eq!((badged.width, badged.height), (size, size));
        let [glyph, badge] = layers[..] else {
            panic!("a glyph and its badge: {layers:?}");
        };
        assert_eq!((glyph.width, glyph.height), (size, size));
        assert_eq!((badge.width, badge.height), (BADGE, BADGE));
        assert!(badge.center_x() > glyph.center_x() && badge.center_y() > glyph.center_y());
        assert_eq!(badge.x + badge.width, size + 3.5);
        assert_eq!(badge.center_y(), size - 0.5);
    }

    #[test]
    fn a_cut_masks_everything_the_artwork_draws() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22" fill="black"><path d="M0 0h22v22H0z"/></svg>"#;
        let masked = cut_svg(
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
            cut_svg(
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
            cut_svg(svg, 11.0, hole)
                .unwrap()
                .contains(r#"<ellipse cx="2" cy="4" rx="6" ry="6" fill="black"/>"#)
        );
    }
}
