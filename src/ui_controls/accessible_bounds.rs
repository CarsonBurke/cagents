//! Placing a wrapper's accessible descendants where they are painted.
//!
//! Native widgets disagree about the virtual offset: buttons and prose
//! subtract it from their bounds, plain text ignores it. A wrapper that hands
//! its descendants a zero offset receives every bound in layout coordinates,
//! and can then clip and translate them all once, at its own boundary.
use cosmic::iced::{Rectangle, Vector};
use iced_accessibility::{A11yNode, A11yTree, accesskit::Rect};

/// Moves every node of `accessible`, laid out in layout coordinates, to the
/// screen by `offset`. With a `clip` in the same layout coordinates, a node
/// outside it is hidden and one crossing it keeps only its shown part.
pub(crate) fn place(accessible: &mut A11yTree, offset: Vector, clip: Option<Rectangle>) {
    let place_node = |node: &mut A11yNode| {
        let node = node.node_mut();
        let Some(mut bounds) = node.bounds() else {
            return;
        };
        if let Some(clip) = clip {
            bounds.x0 = bounds.x0.max(f64::from(clip.x));
            bounds.y0 = bounds.y0.max(f64::from(clip.y));
            bounds.x1 = bounds.x1.min(f64::from(clip.x + clip.width));
            bounds.y1 = bounds.y1.min(f64::from(clip.y + clip.height));
            if bounds.x1 <= bounds.x0 || bounds.y1 <= bounds.y0 {
                node.set_hidden();
                return;
            }
        }
        let (dx, dy) = (f64::from(offset.x), f64::from(offset.y));
        node.set_bounds(Rect::new(
            bounds.x0 - dx,
            bounds.y0 - dy,
            bounds.x1 - dx,
            bounds.y1 - dy,
        ));
    };
    accessible.root_mut().iter_mut().for_each(place_node);
    accessible.children_mut().iter_mut().for_each(place_node);
}
