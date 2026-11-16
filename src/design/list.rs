//! The horizontal grid that lists, the fields above them and their section
//! headers share.
//!
//! A navigation list is read as columns: the edge every highlight and field
//! shares, the column of icons, and the column of labels. When each surface
//! chose its own inset, a selected row ended a few pixels short of the search
//! field above it and a placeholder started a few pixels past the labels
//! below it, and the list read as loosely assembled. Every list row, list
//! field and list header measures from these four values instead:
//!
//! ```text
//! pane edge
//! |<GUTTER>|<INSET>|<ICON>|<GAP>| label ...            |<INSET>|<GUTTER>|
//!          ^ highlight / field edge                            ^ edge
//! ```
//!
//! A vertical scrollbar's rail lives inside the trailing gutter rather than
//! being added to it, so the rows inside a scrollable span the same width as
//! a field mounted outside it.

use super::{ROW_ICON_SIZE, SCROLLBAR_GUTTER, space};

/// From the pane edge to the edge of a row's highlight or a field's border,
/// on both sides.
pub const GUTTER: f32 = space::S;
/// From a highlight or field edge to the icon column, and from the last
/// trailing control to the opposite edge.
pub const INSET: f32 = space::S;
/// The width of the icon column. Row icons are drawn at this size.
pub const ICON: f32 = ROW_ICON_SIZE as f32;
/// Between the icon column and the label column. COSMIC's text input sets the
/// same 8px between its leading icon and its text, which is what lets a
/// search field's placeholder start on the label column.
pub const GAP: f32 = space::S;

/// The line box of a two-line row's second line, in the caption role.
pub const CAPTION_LINE: f32 = 14.0;
/// A row with a caption under its label: both line boxes with 6px above and
/// below the pair, one less than a one-line row leaves around its label, so
/// the row stays on the 4px grid at 44px.
pub const TWO_LINE_HEIGHT: f32 = super::LINE_BODY + CAPTION_LINE + 2.0 * (space::XS + space::XXS);

/// Where the icon column starts, from the pane edge.
pub const fn icon_x() -> f32 {
    GUTTER + INSET
}

/// Where the label column starts, from the pane edge.
pub const fn label_x() -> f32 {
    GUTTER + INSET + ICON + GAP
}

/// Where the label column starts, from a highlight or field edge.
pub const fn label_offset() -> f32 {
    INSET + ICON + GAP
}

/// The padding of a scrollable list's content: the gutter on both sides, with
/// the scrollbar rail inside the trailing one.
///
/// The rail is [`SCROLLBAR_GUTTER`] wide and the gutter is at least as wide,
/// so a thumb shown over a long list floats in the gutter and never covers a
/// highlight.
pub fn scroll_padding(vertical: f32) -> cosmic::iced::Padding {
    const _: () = assert!(GUTTER >= SCROLLBAR_GUTTER);
    cosmic::iced::Padding {
        top: vertical,
        right: GUTTER,
        bottom: vertical,
        left: GUTTER,
    }
}

/// The padding around a field or header that sits above a list.
pub fn edge_padding(top: f32, bottom: f32) -> cosmic::iced::Padding {
    cosmic::iced::Padding {
        top,
        right: GUTTER,
        bottom,
        left: GUTTER,
    }
}
