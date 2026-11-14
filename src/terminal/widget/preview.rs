//! A terminal's screen drawn from a snapshot, at a size of its own.
//!
//! [`TerminalWidget`](super::TerminalWidget) sizes the PTY to its bounds, so
//! a second one mounted to show a terminal somewhere else would resize the
//! real terminal under its pane. This widget never touches the terminal: it
//! draws the rows of a [`RenderSnapshot`] it is handed, cropped to its own
//! bounds, with the same row painting and glyph cache as the pane.
//!
//! Like the pane, it acknowledges each frame it draws: the daemon sends the
//! next frame only once the last one was drawn, so a preview that drew
//! without acknowledging would freeze the terminal's output at that frame.

use std::{cell::RefCell, sync::Arc};

use cosmic::{
    Element, Renderer, Theme,
    iced::{
        Border, Length, Rectangle, Shadow, Size,
        advanced::{
            Layout, Widget, layout, mouse, renderer, text,
            widget::{Tree, tree},
        },
    },
};

use super::{
    LINE_HEIGHT_RATIO, RenderSnapshot, ShapeCache, TerminalGeometry, TerminalSurface, color,
    draw_row, graphics_text,
};

/// Draws the latest rows of a terminal screen, bottom-aligned to the last row
/// that holds anything, at `font_size`.
pub struct SnapshotPreview {
    surface: TerminalSurface,
    snapshot: Arc<RenderSnapshot>,
    font_size: f32,
    family: &'static str,
    scale_factor: f32,
}

impl SnapshotPreview {
    pub fn new(surface: &TerminalSurface) -> Self {
        Self {
            snapshot: surface.frame(),
            surface: surface.clone(),
            font_size: 10.0,
            family: super::intern_family(""),
            scale_factor: 1.0,
        }
    }

    pub fn font_size(mut self, size: f32) -> Self {
        self.font_size = size;
        self
    }

    pub fn font_family(mut self, family: &str) -> Self {
        self.family = super::intern_family(family);
        self
    }

    pub fn scale_factor(mut self, scale_factor: f32) -> Self {
        self.scale_factor = scale_factor;
        self
    }
}

/// The rows of `snapshot` to show in `capacity` lines: those ending at the
/// last row that holds anything, so a short screen shows its prompt rather
/// than the blank rows beneath it, and a full one its newest output.
fn shown_rows(snapshot: &RenderSnapshot, capacity: usize) -> std::ops::Range<usize> {
    let lines = &snapshot.lines;
    let cursor = snapshot
        .cursor
        .filter(|cursor| cursor.visible)
        .map(|cursor| usize::from(cursor.row) + 1);
    let content = lines
        .iter()
        .rposition(|row| row.cells.iter().any(|cell| !cell.text.is_empty()))
        .map(|row| row + 1);
    let end = content.max(cursor).unwrap_or(0).min(lines.len());
    end.saturating_sub(capacity)..end
}

#[derive(Default)]
struct State {
    shaped: RefCell<ShapeCache>,
}

impl<Message> Widget<Message, Theme, Renderer> for SnapshotPreview {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use cosmic::iced::advanced::Renderer as _;
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            self.surface.acknowledge_redraw(self.snapshot.generation);
            return;
        };
        self.surface.acknowledge_redraw(self.snapshot.generation);
        renderer.with_layer(clip, |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds,
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                color(self.snapshot.background),
            );
            if self.snapshot.detached || self.snapshot.rows == 0 {
                return;
            }
            let geometry = TerminalGeometry::new(
                bounds,
                self.font_size,
                self.scale_factor,
                self.family,
                LINE_HEIGHT_RATIO,
                0.0,
            );
            let Some(content_clip) = geometry.content_bounds.intersection(&clip) else {
                return;
            };
            let shown = shown_rows(&self.snapshot, usize::from(geometry.rows));
            let rows = &self.snapshot.lines[shown.clone()];
            let column_limit = usize::from(self.snapshot.columns.min(geometry.columns));
            renderer.with_layer(content_clip, |renderer| {
                for (index, row) in rows.iter().enumerate() {
                    let cells = &row.cells[..row.cells.len().min(column_limit)];
                    draw_row(
                        renderer,
                        &geometry,
                        index as u16,
                        cells,
                        self.snapshot.background,
                    );
                }
                let state = tree.state.downcast_ref::<State>();
                let mut shaped = state.shaped.borrow_mut();
                // The cache follows absolute rows, so a screen that scrolled by
                // a line reshapes only the line that appeared.
                shaped.scroll_to(self.snapshot.scroll.offset + shown.start as u64);
                shaped.refresh(&geometry, self.font_size, rows, column_limit);
                let foreground = color(self.snapshot.foreground);
                for (index, cached) in shaped.rows.iter().enumerate() {
                    let Ok(row) = u16::try_from(index) else {
                        break;
                    };
                    for segment in &cached.segments {
                        let Some(cell) = geometry.cell_bounds(segment.column, row) else {
                            continue;
                        };
                        text::Renderer::fill_raw(
                            renderer,
                            graphics_text::Raw {
                                buffer: Arc::downgrade(&segment.buffer),
                                position: geometry.text_origin(cell),
                                color: foreground,
                                clip_bounds: content_clip,
                            },
                        );
                    }
                }
            });
        });
    }
}

impl<'a, Message: 'a> From<SnapshotPreview> for Element<'a, Message> {
    fn from(preview: SnapshotPreview) -> Self {
        Element::new(preview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{CellSnapshot, CellText, CursorSnapshot, Rgb, RowSnapshot};

    fn row(text: &str) -> Arc<RowSnapshot> {
        Arc::new(RowSnapshot {
            cells: text
                .chars()
                .map(|ch| CellSnapshot {
                    text: CellText::from(ch.to_string().as_str()),
                    ..CellSnapshot::default()
                })
                .collect(),
            ..RowSnapshot::default()
        })
    }

    fn snapshot(lines: &[&str], cursor_row: Option<u16>) -> Arc<RenderSnapshot> {
        Arc::new(RenderSnapshot {
            rows: lines.len() as u16,
            columns: 10,
            cursor: cursor_row.map(|row| CursorSnapshot {
                column: 0,
                row,
                shape: crate::terminal::CursorShape::Block,
                color: Rgb::default(),
                visible: true,
                blinking: false,
            }),
            lines: lines.iter().map(|line| row(line)).collect(),
            ..RenderSnapshot::empty()
        })
    }

    #[test]
    fn a_short_screen_shows_its_top_and_a_full_one_its_newest_rows() {
        let short = snapshot(&["$ ls", "a b", "$", "", "", ""], Some(2));
        assert_eq!(shown_rows(&short, 4), 0..3);
        let full = snapshot(&["1", "2", "3", "4", "5", "6"], Some(5));
        assert_eq!(shown_rows(&full, 4), 2..6);
        // A cursor below the last output keeps the empty prompt line in view.
        let prompt = snapshot(&["out", "", "", ""], Some(2));
        assert_eq!(shown_rows(&prompt, 2), 1..3);
        let blank = snapshot(&["", ""], None);
        assert_eq!(shown_rows(&blank, 2), 0..0);
    }
}
