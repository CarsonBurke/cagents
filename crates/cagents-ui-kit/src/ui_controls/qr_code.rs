//! A QR code, drawn as quads.
//!
//! It is always dark on light, whatever the theme: phone scanners expect
//! that contrast, and many fail on an inverted code. The light square
//! includes the four-module quiet zone the standard asks for.

use std::sync::Arc;

use cosmic::{
    Element,
    iced::{self, Length},
};

/// Modules of quiet zone around the code, on every side.
const QUIET_ZONE: usize = 4;

/// An encoded QR code: which modules are dark.
#[derive(Debug, PartialEq, Eq)]
pub struct QrMatrix {
    width: usize,
    dark: Vec<bool>,
}

impl QrMatrix {
    /// Encode `text` at medium error correction; `None` when it is too long
    /// for any QR code.
    pub fn encode(text: &str) -> Option<Arc<Self>> {
        let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
            .ok()?;
        Some(Arc::new(Self {
            width: code.width(),
            dark: code
                .to_colors()
                .into_iter()
                .map(|color| color == qrcode::Color::Dark)
                .collect(),
        }))
    }

    /// Modules on a side, without the quiet zone.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The dark runs of each row, as `(row, first column, length)`.
    fn runs(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        self.dark
            .chunks(self.width)
            .enumerate()
            .flat_map(|(row, modules)| {
                let mut runs = Vec::new();
                let mut start = None;
                for (column, dark) in modules.iter().chain([&false]).enumerate() {
                    match (start, *dark) {
                        (None, true) => start = Some(column),
                        (Some(first), false) => {
                            runs.push((row, first, column - first));
                            start = None;
                        }
                        _ => {}
                    }
                }
                runs
            })
    }
}

/// `matrix` drawn with whole-pixel modules, as large as fits in `size`
/// logical pixels, quiet zone included.
pub fn qr_code<'a, Message: 'a>(matrix: Arc<QrMatrix>, size: f32) -> Element<'a, Message> {
    let modules = matrix.width + 2 * QUIET_ZONE;
    let module = (size / modules as f32).floor().max(1.0);
    Element::new(QrCode {
        matrix,
        module,
        side: module * modules as f32,
    })
}

struct QrCode {
    matrix: Arc<QrMatrix>,
    module: f32,
    side: f32,
}

impl<Message> iced::advanced::Widget<Message, cosmic::Theme, cosmic::Renderer> for QrCode {
    fn size(&self) -> iced::Size<Length> {
        iced::Size::new(Length::Fixed(self.side), Length::Fixed(self.side))
    }

    fn layout(
        &mut self,
        _tree: &mut iced::advanced::widget::Tree,
        _renderer: &cosmic::Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> iced::advanced::layout::Node {
        iced::advanced::layout::atomic(limits, self.side, self.side)
    }

    fn draw(
        &self,
        _tree: &iced::advanced::widget::Tree,
        renderer: &mut cosmic::Renderer,
        _theme: &cosmic::Theme,
        _style: &iced::advanced::renderer::Style,
        layout: iced::advanced::Layout<'_>,
        _cursor: iced::mouse::Cursor,
        viewport: &iced::Rectangle,
    ) {
        use iced::advanced::Renderer as _;

        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let quad = |bounds| iced::advanced::renderer::Quad {
            bounds,
            border: iced::Border {
                radius: 0.0.into(),
                ..Default::default()
            },
            shadow: iced::Shadow::default(),
            snap: true,
        };
        renderer.fill_quad(
            iced::advanced::renderer::Quad {
                border: iced::Border {
                    radius: crate::design::RADIUS_XS.into(),
                    ..Default::default()
                },
                ..quad(bounds)
            },
            iced::Color::WHITE,
        );
        let origin = QUIET_ZONE as f32 * self.module;
        for (row, column, length) in self.matrix.runs() {
            renderer.fill_quad(
                quad(iced::Rectangle {
                    x: bounds.x + origin + column as f32 * self.module,
                    y: bounds.y + origin + row as f32 * self.module,
                    width: length as f32 * self.module,
                    height: self.module,
                }),
                iced::Color::BLACK,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_cover_exactly_the_dark_modules() {
        let matrix = QrMatrix::encode("cagents-mobile://pair?v=1").unwrap();
        let mut drawn = vec![false; matrix.dark.len()];
        for (row, column, length) in matrix.runs() {
            for module in drawn
                .iter_mut()
                .skip(row * matrix.width + column)
                .take(length)
            {
                assert!(!*module, "runs never overlap");
                *module = true;
            }
        }
        assert_eq!(drawn, matrix.dark);
    }

    #[test]
    fn a_pairing_code_fits_a_popover_with_whole_pixel_modules() {
        // A code with three addresses and a key, as a pairing shows.
        let code = format!(
            "cagents-mobile://pair?v=1&name=workstation&user=person&port=22&hostkey=SHA256%3A{}&host=workstation.tail1234.ts.net&host=100.101.102.103&host=192.168.100.200&key={}&expires=1800000000",
            "I7uYBbsuJDf9UOOdWpjHuU7LQcImmvDuSC3TnbMc5jo",
            "A".repeat(43)
        );
        let matrix = QrMatrix::encode(&code).unwrap();
        let modules = matrix.width() + 2 * QUIET_ZONE;
        assert!(
            (224.0 / modules as f32).floor() >= 3.0,
            "{modules} modules leave under 3 px each"
        );
    }
}
