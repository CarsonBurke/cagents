use crate::core::svg;
use crate::core::{Color, Size};
use crate::image::atlas::{self, Atlas};

use resvg::tiny_skia;
use resvg::usvg;
use rustc_hash::{FxHashMap, FxHashSet};
use std::fs;
use std::panic;
use std::sync::Arc;

/// Entry in cache corresponding to an svg handle
pub enum Svg {
    /// Parsed svg
    Loaded(usvg::Tree),
    /// Svg not found or failed to parse
    NotFound,
}

impl Svg {
    /// Viewport width and height
    pub fn viewport_dimensions(&self) -> Size<u32> {
        match self {
            Svg::Loaded(tree) => {
                let size = tree.size();

                Size::new(size.width() as u32, size.height() as u32)
            }
            Svg::NotFound => Size::new(1, 1),
        }
    }
}

/// Caches svg vector and raster data
#[derive(Debug, Default)]
pub struct Cache {
    svgs: FxHashMap<u64, Svg>,
    rasterized: FxHashMap<(u64, u32, u32, ColorFilter), atlas::Entry>,
    svg_hits: FxHashSet<u64>,
    rasterized_hits: FxHashSet<(u64, u32, u32, ColorFilter)>,
    should_trim: bool,
    fontdb: Option<Arc<usvg::fontdb::Database>>,
}

type ColorFilter = Option<[u8; 4]>;

impl Cache {
    /// Load svg
    pub fn load(&mut self, handle: &svg::Handle) -> &Svg {
        if self.svgs.contains_key(&handle.id()) {
            return self.svgs.get(&handle.id()).unwrap();
        }

        // TODO: Reuse `cosmic-text` font database
        if self.fontdb.is_none() {
            let mut fontdb = usvg::fontdb::Database::new();
            fontdb.load_system_fonts();

            self.fontdb = Some(Arc::new(fontdb));
        }

        let options = usvg::Options {
            fontdb: self
                .fontdb
                .as_ref()
                .expect("fontdb must be initialized")
                .clone(),
            ..usvg::Options::default()
        };

        let svg = match handle.data() {
            svg::Data::Path(path) => fs::read_to_string(path)
                .ok()
                .and_then(|contents| {
                    usvg::Tree::from_str(&contents, &options).ok()
                })
                .map(Svg::Loaded)
                .unwrap_or(Svg::NotFound),
            svg::Data::Bytes(bytes) => {
                match usvg::Tree::from_data(bytes, &options) {
                    Ok(tree) => Svg::Loaded(tree),
                    Err(_) => Svg::NotFound,
                }
            }
        };

        self.should_trim = true;

        let _ = self.svgs.insert(handle.id(), svg);
        self.svgs.get(&handle.id()).unwrap()
    }

    /// Load svg and upload raster data
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        belt: &mut wgpu::util::StagingBelt,
        handle: &svg::Handle,
        color: Option<Color>,
        size: Size,
        scale: f32,
        atlas: &mut Atlas,
    ) -> Option<&atlas::Entry> {
        let id = handle.id();

        let (width, height) = (
            (scale * size.width).ceil() as u32,
            (scale * size.height).ceil() as u32,
        );

        let color = color.map(Color::into_rgba8);
        let key = (id, width, height, color);

        // TODO: Optimize!
        // We currently rerasterize the SVG when its size changes. This is slow
        // as heck. A GPU rasterizer like `pathfinder` may perform better.
        // It would be cool to be able to smooth resize the `svg` example.
        if self.rasterized.contains_key(&key) {
            let _ = self.svg_hits.insert(id);
            let _ = self.rasterized_hits.insert(key);

            return self.rasterized.get(&key);
        }

        match self.load(handle) {
            Svg::Loaded(tree) => {
                if width == 0 || height == 0 {
                    return None;
                }

                // TODO: Optimize!
                // We currently rerasterize the SVG when its size changes. This is slow
                // as heck. A GPU rasterizer like `pathfinder` may perform better.
                // It would be cool to be able to smooth resize the `svg` example.
                let mut img = tiny_skia::Pixmap::new(width, height)?;

                let transform = fit(tree.size(), width, height);

                // SVG rendering can panic on malformed or complex vectors.
                // We catch panics to prevent crashes and continue gracefully.
                let render =
                    panic::catch_unwind(panic::AssertUnwindSafe(|| {
                        resvg::render(tree, transform, &mut img.as_mut());
                    }));

                if let Err(error) = render {
                    log::warn!(
                        "SVG rendering for {handle:?} panicked: {error:?}"
                    );
                }

                let mut rgba = img.take();

                // A tinted vector is one color under varying coverage, so
                // every texel takes it, transparent ones included. Left black,
                // they would darken the edges a filtered (rotated) draw blends
                // in from its transparent neighbours.
                if let Some(color) = color {
                    rgba.chunks_exact_mut(4).for_each(|rgba| {
                        rgba[..3].copy_from_slice(&color[..3]);
                    });
                }

                let allocation = atlas
                    .upload(device, encoder, belt, width, height, &rgba)?;

                log::debug!("allocating {id} {width}x{height}");

                let _ = self.svg_hits.insert(id);
                let _ = self.rasterized_hits.insert(key);
                let _ = self.rasterized.insert(key, allocation);
                self.should_trim = true;

                self.rasterized.get(&key)
            }
            Svg::NotFound => None,
        }
    }

    /// Load svg and upload raster data
    pub fn trim(&mut self, atlas: &mut Atlas) {
        if !self.should_trim {
            return;
        }

        let svg_hits = &self.svg_hits;
        let rasterized_hits = &self.rasterized_hits;

        self.svgs.retain(|k, _| svg_hits.contains(k));
        self.rasterized.retain(|k, entry| {
            let retain = rasterized_hits.contains(k);

            if !retain {
                atlas.remove(entry);
            }

            retain
        });
        self.svg_hits.clear();
        self.rasterized_hits.clear();
        self.should_trim = false;
    }
}

impl std::fmt::Debug for Svg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Svg::Loaded(_) => write!(f, "Svg::Loaded"),
            Svg::NotFound => write!(f, "Svg::NotFound"),
        }
    }
}

/// Draws a tree of `size` into a `width` x `height` raster at the largest
/// uniform scale that keeps all of it inside, centred on whole pixels.
///
/// Upstream scaled by the raster's longer side, or its height when square, so
/// a tree wider than tall, such as a 17x16 theme icon in a square icon box,
/// overflowed the raster and lost its right edge, and truncating the tree to
/// whole units first could stretch it unevenly.
fn fit(size: tiny_skia::Size, width: u32, height: u32) -> tiny_skia::Transform {
    let (width, height) = (width as f32, height as f32);
    let scale = (width / size.width()).min(height / size.height());
    // Whole-pixel offsets keep edges the drawing places on its grid crisp; a
    // centre half a pixel off would blur every one of them.
    let dx = ((width - size.width() * scale) / 2.0).round();
    let dy = ((height - size.height() * scale) / 2.0).round();

    tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, dx, dy)
}

#[cfg(test)]
mod tests {
    use super::fit;
    use resvg::tiny_skia;

    #[test]
    fn a_tree_stays_inside_its_raster_at_one_scale() {
        let sizes = [(16.0, 16.0), (17.0, 16.0), (16.0, 16.03), (16.0, 17.0)];
        for (tree_width, tree_height) in sizes {
            let size =
                tiny_skia::Size::from_wh(tree_width, tree_height).unwrap();
            for (width, height) in [
                (12, 12),
                (16, 16),
                (20, 20),
                (25, 25),
                (35, 35),
                (20, 18),
                (18, 20),
            ] {
                let transform = fit(size, width, height);
                assert_eq!(
                    transform.sx, transform.sy,
                    "{size:?} {width}x{height}"
                );
                let right = transform.tx + tree_width * transform.sx;
                let bottom = transform.ty + tree_height * transform.sy;
                assert!(
                    transform.tx >= 0.0 && transform.ty >= 0.0,
                    "{size:?} {width}x{height}"
                );
                assert!(
                    right <= width as f32 + 1e-3,
                    "{size:?} {width}x{height}: {right}"
                );
                assert!(
                    bottom <= height as f32 + 1e-3,
                    "{size:?} {width}x{height}: {bottom}"
                );
                // It fills the raster along one axis.
                assert!(
                    (tree_width * transform.sx - width as f32).abs() < 1e-3
                        || (tree_height * transform.sy - height as f32).abs()
                            < 1e-3,
                    "{size:?} {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn a_square_tree_fills_a_square_raster_exactly() {
        let size = tiny_skia::Size::from_wh(16.0, 16.0).unwrap();
        for side in [12, 14, 16, 18, 20, 25, 28, 35] {
            let transform = fit(size, side, side);
            assert_eq!((transform.tx, transform.ty), (0.0, 0.0));
            assert_eq!(transform.sx, side as f32 / 16.0);
        }
    }
}
