//! Reveal keyboard selection in a uniformly sized native result list.
use cosmic::iced::{
    self,
    advanced::widget::{Id, Operation, operation},
};

/// Keeps a selected row visible without moving rows already in view. The
/// mounted scrollable supplies its actual viewport, content and current offset.
pub fn reveal_list_selection<Message: Send + 'static>(
    id: Id,
    selected: usize,
    row_height: f32,
    spacing: f32,
) -> iced::Task<Message> {
    iced::advanced::widget::operate(RevealSelection {
        id,
        selected,
        row_height,
        spacing,
    })
}

struct RevealSelection {
    id: Id,
    selected: usize,
    row_height: f32,
    spacing: f32,
}
impl<Message> Operation<Message> for RevealSelection {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<Message>)) {
        visit(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: iced::Rectangle,
        content: iced::Rectangle,
        translation: iced::Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if id != Some(&self.id) {
            return;
        }
        let top = self.selected as f32 * (self.row_height + self.spacing);
        state.scroll_to(operation::scrollable::AbsoluteOffset {
            x: None,
            y: Some(selection_offset(
                top,
                self.row_height,
                bounds.height,
                content.height,
                translation.y,
            )),
        });
    }
}

/// The scroll offset that brings the row spanning `top..top + height` into
/// view, leaving rows already in view where they are.
///
/// A row is revealed a fade's depth clear of the edge, not flush against
/// it: while more rows lie beyond, that edge carries the overflow fade, and
/// the row the reader just moved to must not be the one dissolving into it.
/// At either end of the list there is nothing beyond and no fade, so the
/// clamp lets the row sit flush. A viewport too short for the margin on both
/// sides shares what room it has, so a reveal never oscillates.
pub fn selection_offset(top: f32, height: f32, viewport: f32, content: f32, current: f32) -> f32 {
    let margin = super::OVERFLOW_FADE.min(((viewport - height) / 2.0).max(0.0));
    let bottom = top + height;
    let target = if top - margin < current {
        top - margin
    } else if bottom + margin > current + viewport {
        bottom + margin - viewport
    } else {
        current
    };
    target.clamp(0.0, (content - viewport).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_short_viewport_shares_the_margin_and_settles() {
        // 40 px of room around a 30 px row: 5 px a side, and a second reveal
        // of the same row stays put.
        let first = selection_offset(200.0, 30.0, 40.0, 1000.0, 0.0);
        assert_eq!(first, 195.0);
        assert_eq!(selection_offset(200.0, 30.0, 40.0, 1000.0, first), first);
    }

    #[test]
    fn reveal_preserves_visible_rows_and_uses_actual_viewport() {
        // Rows 84..124 and 66..106 are clear of both fades already.
        assert_eq!(selection_offset(84.0, 40.0, 160.0, 1000.0, 0.0), 0.0);
        assert_eq!(selection_offset(66.0, 40.0, 160.0, 1000.0, 42.0), 42.0);
        // Below the viewport: it ends a fade's depth above the bottom edge.
        assert_eq!(selection_offset(168.0, 40.0, 160.0, 1000.0, 0.0), 72.0);
        // Above it: it starts a fade's depth below the top edge.
        assert_eq!(selection_offset(300.0, 40.0, 160.0, 1000.0, 400.0), 276.0);
        // The first row and the last sit flush: no fade lies beyond them.
        assert_eq!(selection_offset(0.0, 40.0, 160.0, 1000.0, 200.0), 0.0);
        assert_eq!(selection_offset(960.0, 40.0, 160.0, 1000.0, 0.0), 840.0);
        assert_eq!(selection_offset(84.0, 40.0, 160.0, 100.0, 200.0), 0.0);
    }
}
