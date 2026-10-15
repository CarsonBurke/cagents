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
    reveal_list_region(id, selected as f32 * (row_height + spacing), row_height)
}

/// Reveal a selected region in a list that also contains section headings.
/// The actual mounted viewport and offset decide whether any scroll is needed.
pub fn reveal_list_region<Message: Send + 'static>(id: Id, top: f32, height: f32) -> iced::Task<Message> {
    iced::advanced::widget::operate(RevealSelection { id, top, height })
}

struct RevealSelection {
    id: Id,
    top: f32,
    height: f32,
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
        state.scroll_to(operation::scrollable::AbsoluteOffset {
            x: None,
            y: Some(selection_offset(
                self.top,
                self.height,
                bounds.height,
                content.height,
                translation.y,
            )),
        });
    }
}

fn selection_offset(top: f32, height: f32, viewport: f32, content: f32, current: f32) -> f32 {
    let bottom = top + height;
    let target = if top < current {
        top
    } else if bottom > current + viewport {
        bottom - viewport
    } else {
        current
    };
    target.clamp(0.0, (content - viewport).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reveal_preserves_visible_rows_and_uses_actual_viewport() {
        assert_eq!(selection_offset(84.0, 40.0, 160.0, 1000.0, 0.0), 0.0);
        assert_eq!(selection_offset(168.0, 40.0, 160.0, 1000.0, 0.0), 48.0);
        assert_eq!(selection_offset(84.0, 40.0, 160.0, 1000.0, 42.0), 42.0);
        assert_eq!(selection_offset(0.0, 40.0, 160.0, 1000.0, 200.0), 0.0);
        assert_eq!(selection_offset(84.0, 40.0, 160.0, 100.0, 200.0), 0.0);
    }
}
